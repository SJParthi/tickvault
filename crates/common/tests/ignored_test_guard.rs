//! The test-count ratchet is blind to the cheapest way to lose a test.
//!
//! `test-count-guard.sh` counts `#[test]` and `#[tokio::test]` **attributes in
//! source** and requires the total to never fall. That stops a test being
//! DELETED. It does nothing about a test being IGNORED: adding `#[ignore]`
//! above one leaves the attribute in place, so the count is unchanged, the
//! ratchet prints "Test count stable", and the test stops running.
//!
//! The house rule is *never delete, skip or disable a test to get a gate
//! green*. Deleting was enforced. Skipping was not — and skipping is the
//! easier move when a test is inconvenient at 2am.
//!
//! This is the **second** blind spot of this exact shape in that guard. Its own
//! header records the first: it counted only `#[test]`, and the string
//! `#[tokio::test]` does not contain `#[test]`, so **1,166 async tests —
//! roughly an eighth of the suite — were invisible to the ratchet** and could
//! have been deleted wholesale without it noticing. Same failure both times:
//! the count measured something *adjacent* to the thing it claimed to protect.
//!
//! Two rules here, and they do different jobs:
//!
//! 1. **Every `#[ignore]` must carry a reason.** A bare `#[ignore]` is how a
//!    test gets quietly parked — no diff comment, nothing for a reviewer to
//!    argue with. Requiring a string forces the author to say it out loud.
//! 2. **The ignored set may only shrink.** A reason string alone would let
//!    someone park a real gate behind a plausible-sounding excuse. New entries
//!    have to be added here deliberately, in the same change, where they are
//!    visible.
//!
//! The seven ignored today are all genuinely not gates: four measurement
//! harnesses (wall-clock timings, which would flake on a shared CI runner and a
//! flaky gate is worse than none), a reporting aid, a re-bless helper that
//! enforces nothing, and a chaos test needing a live Docker stack.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> crates -> repo root")
        .to_path_buf()
}

/// Every test allowed to be ignored, as (file, fn name). Keyed on both so a
/// reason string cannot be reused to park a different test.
///
/// This list may SHRINK freely. Growing it is a deliberate edit, in the same
/// change, with the reason visible to a reviewer.
const ALLOWED_IGNORED: &[(&str, &str)] = &[
    // Added 2026-10-10 with the contract name repair (plan item 56): an
    // end-to-end run against a real QuestDB (it creates and drops its own
    // probe table), which CI does not have. It gates nothing; the SQL the
    // repair sends is pinned by the ordinary tests in
    // `crates/storage/src/contract_name_repair.rs`. Re-run it whenever that
    // SQL changes:
    //
    //   TV_QUESTDB_EXEC_URL=http://127.0.0.1:9000/exec \
    //     cargo test -p tickvault-storage --test contract_name_repair_live -- --ignored
    (
        "crates/storage/tests/contract_name_repair_live.rs",
        "repair_names_blank_rows_in_place_and_leaves_everything_else",
    ),
    // Added 2026-10-09 with the arithmetic price widening (audit PR10): the
    // exhaustive proof that the new `f32_to_f64_clean` returns the same f64 as
    // the text path for all 2^32 f32 inputs. Measured: 0 differences, 382 s on
    // 4 cores in release; in a debug CI build it would take hours. It is not
    // the gate: the ordinary tests beside it (every paise price up to
    // 2,00,000 rupees, a 1-in-4,099 stride over every bit pattern, and the
    // boundary values) run in every suite. Re-run it whenever the arithmetic
    // path changes:
    //
    //   cargo test -p tickvault-common --release --lib -- --ignored every_f32
    (
        "crates/common/src/price_precision.rs",
        "test_f32_to_f64_clean_matches_text_path_for_every_f32",
    ),
    // Added 2026-10-03 with sweep S1 (the drain flush no longer hands over
    // its worker): the same wall-clock shape as the harnesses below -- it
    // times `block_in_place` against a bare call and prints both, so a shared
    // CI runner would make it a flake. It gates nothing; that the drain's
    // writers step off the worker only at their blocking steps is pinned by
    // the ordinary test `every_blocking_writer_step_runs_off_the_worker`.
    //
    //   cargo test -p tickvault-storage --lib off_worker -- --ignored --nocapture
    (
        "crates/storage/src/off_worker.rs",
        "block_in_place_cost_against_a_bare_call",
    ),
    // Added 2026-09-19 with the radix A/B it belongs to: the same wall-clock
    // shape as the other wall-clock harnesses here -- it times the leaderboard's
    // comparator against a candidate radix sort at four sizes and prints both
    // numbers, so a shared CI runner would make it a flake, and a flaky gate
    // teaches people to ignore gates.
    //
    // It is NOT the merge condition for anything. The radix was MEASURED and
    // REJECTED (release: comparator 1.0/7.0/42.0/692.7 us against radix
    // 4.1/18.7/77.3/1074.3 us at n=100/500/2,000/20,220), so no production
    // path calls it; the ordering it would have replaced is pinned by the
    // ordinary tests in the same file.
    //
    // Two reasons it is worth keeping rather than deleting: re-running it is
    // how the rejection stays honest if the row shape or the sizes change,
    // and it asserts -- each round checks it ordered the `n` rows it claims
    // and that both paths agree, so it cannot report a figure from an empty
    // vector the way the withdrawn 900 us sweep number did.
    (
        "crates/app/src/volume_leaderboard.rs",
        "radix_vs_comparator_at_every_measured_shape",
    ),
    // Added 2026-09-26 (audit PR4) with the sliced sweep it times: the same
    // wall-clock shape as the rank harness above. It reports the longest
    // single step of each phase at the 20,220-contract ceiling, which is the
    // bound on how long a frame waits behind the sweep. It asserts the board
    // it timed was full, so it cannot report a figure from an empty board;
    // the sliced and unsliced rankings are proven equal by the proptest
    // `begin_sweep_and_sweep_step_rank_identically_to_rank`, which is a gate. Measured 2026-09-26 (release, x86 dev
    // container): longest step collect 433 us, sort 197 us, gainer walk
    // 60 us; mean step 11.6 us over 8,316 steps.
    (
        "crates/app/src/volume_leaderboard.rs",
        "sliced_sweep_step_cost_at_the_authorized_ceiling",
    ),
    (
        "crates/trading/src/candles/multi_tf_aggregator.rs",
        "catch_up_seal_all_sweep_cost_at_the_authorized_ceiling",
    ),
    // Added 2026-08-22 with the fold-CPU measurement it belongs to: same
    // shape as the sweep harness above -- a wall-clock number on a shared CI
    // runner is a flake, and a flaky gate teaches people to ignore gates. It
    // is run deliberately in release, never as a merge condition.
    (
        "crates/trading/src/candles/multi_tf_aggregator.rs",
        "fold_cost_at_the_authorized_ceiling",
    ),
    (
        "crates/core/src/pipeline/tick_gap_detector.rs",
        "scan_silence_sweep_cost_at_the_authorized_ceiling",
    ),
    // Added 2026-08-26 with the defect-10 cost measurement it belongs to: the
    // same shape as the two harnesses above -- it times a full
    // select_depth_universe pass at the authorized 24,600-instrument ceiling
    // and prints the number. A wall-clock figure on a shared CI runner is a
    // flake, and a flaky gate teaches people to ignore gates, so it is run
    // deliberately in release and is never a merge condition. The behaviour it
    // measures is gated by the ordinary tests in the same file.
    (
        "crates/app/src/dhan_depth_universe.rs",
        "select_depth_universe_cost_at_the_authorized_ceiling",
    ),
    (
        "crates/storage/tests/critical_errcode_alarm_coverage_guard.rs",
        "report_critical_paging_coverage",
    ),
    (
        "crates/storage/tests/operator_boundary_indicator_strategy_guard.rs",
        "bless_indicator_strategy_boundary_manifest",
    ),
    (
        "crates/core/tests/chaos_cascade_triple_failure.rs",
        "cascade_01_triple_failure_live_docker_zero_loss",
    ),
    // Added 2026-09-03 with the streaming spill-replay repair it proves.
    //
    // DIFFERENT SHAPE from the wall-clock harnesses above, so the reason is
    // its own: this one is not timing-flaky, it is DISK-flaky. It writes a
    // multi-gigabyte fixture (default 2 GiB, `TV_SPILL_MEMORY_TEST_BYTES`
    // overrides) and a CI agent that runs out of disk would fail it while
    // saying nothing whatever about the code -- the textbook flaky gate.
    //
    // What it measures is the central claim of that repair: that replaying a
    // spill file far larger than RAM grows the process by the streaming
    // window rather than by the file. Measured 2026-09-03 on a 3.22 GB
    // fixture: peak RSS growth 42.3 MB against a 268.4 MB bound, whole file
    // delivered. The old whole-file read would have grown by 3.22 GB, which
    // is what restart-looped production that morning.
    //
    // It is NOT the merge condition for that behaviour -- a source guard
    // (`the_replay_path_never_reads_a_whole_spill_file_into_memory`) fails the
    // build if the whole-file read returns, and it runs on every PR. This is
    // the empirical companion, run deliberately:
    //   cargo test -p tickvault-storage --test spill_replay_memory_bound \
    //     -- --ignored --nocapture
    (
        "crates/storage/tests/spill_replay_memory_bound.rs",
        "a_multi_gigabyte_spill_file_does_not_grow_the_process",
    ),
    // Added 2026-09-06 with the volume-leaderboard ranking sweep it measures.
    //
    // SAME SHAPE as the three wall-clock harnesses above, and added for the
    // same reason: it times a full `rank` pass over 20,220 stock-option
    // contracts -- the measured live count -- and prints the number. A
    // wall-clock figure on a shared CI runner is a flake, and a flaky gate
    // teaches people to ignore gates.
    //
    // It exists because the number it produces was WRONG in the design notes
    // before it was written. The cost had been asserted as "~60 us, 0.0012%
    // duty" by borrowing the 2.8 ns/instrument constant from the
    // `scan_silence` harness above -- a LINEAR scan -- and applying it to a
    // `sort_unstable_by`. That is an invalid transfer between two different
    // complexity classes, and it was the number used to argue a min-heap
    // design away.
    //
    // ⚠ CORRECTED 2026-09-12 -- the figures this paragraph carried were
    // THEMSELVES wrong, and wrong in the reassuring direction. It read
    // "Measured here instead: 900.406 us for `rank(top 250)` and 899.543 us
    // for `rank_distinct_underlying(5)`, a 0.018% duty cycle at the 5-second
    // cadence -- so the conclusion survived and the evidence for it was
    // overstated 15x." Both numbers are WITHDRAWN. The harness that produced
    // them seeded every tracked contract with the SAME volume as its
    // baseline, so every row had a window delta of zero, every row was
    // skipped before the comparator, and the sort it timed was a sort of an
    // EMPTY slice. It also called `rank_distinct_underlying`, which was
    // DELETED on 2026-09-08 -- so the second figure names a function that no
    // longer exists.
    //
    // Re-measured 2026-09-12 with the harness repaired (contracts actually
    // trade before the sweep, and the lot lookup is a real hash probe rather
    // than a constant stub), at the 20,220-contract ceiling:
    //
    //   every contract traded  -> 2.95 ms  (0.29% duty at the 1 s cadence)
    //              2,000 traded -> 123 us  (0.012% duty)
    //                500 traded ->  29 us
    //
    // An independent re-run of the ceiling case reproduced ~3.24 ms, about
    // 10% higher, so read the ceiling as a ~2.9-3.3 ms BAND rather than a
    // point -- 2.95 sits at the optimistic edge of what this container
    // produces.
    //
    // So the true ceiling is ~3.3x WORSE than the withdrawn figure, not
    // better. The design conclusion is unchanged and is now actually
    // supported: 2.95 ms once per second is a 0.29% duty cycle, and the
    // every-contract-traded case is not a market that exists.
    //
    // It is NOT the merge condition for any behaviour. The ranking, the
    // family split, the monotonicity gate, the capacity cap and the
    // non-finite refusal are each pinned by ordinary tests in the same file
    // that run on every PR. Run this one deliberately:
    //   cargo test -p tickvault-app --lib -- --ignored --nocapture \
    //     volume_leaderboard::tests::rank_sweep_cost_at_the_authorized_ceiling
    (
        "crates/app/src/volume_leaderboard.rs",
        "rank_sweep_cost_at_the_authorized_ceiling",
    ),
    // Added 2026-09-08 with the per-underlying gainer memo it measures.
    //
    // SAME SHAPE as the rank harness directly above, and it exists for the
    // same reason that one does: the walk it times is the honest worst case
    // of `gainer_eligible` -- a day on which every stock is falling, so no
    // gainer is ever collected and the walk runs to the END of the traded
    // population (up to 20,220 contracts) once per 5-second sweep. Until
    // 2026-09-08 that walk made TWO store probes per ROW; the memo caps the
    // probes at the underlying count (~210). The harness prints the cost at
    // the authorized ceiling so the CLAUDE.md O(1) row carries a MEASURED
    // figure instead of a projection. A wall-clock number on a shared CI
    // runner is a flake, so it is never a merge condition; the memo behaviour
    // (bounded capacity, one verdict per underlying, the walk stopping at the
    // exit-rank limit) is pinned by the ordinary tests in the same file that
    // run on every PR. Run deliberately:
    //   cargo test -p tickvault-app --lib -- --ignored --nocapture \
    //     volume_leaderboard::tests::gainer_eligible_sweep_cost_at_the_authorized_ceiling
    (
        "crates/app/src/volume_leaderboard.rs",
        "gainer_eligible_sweep_cost_at_the_authorized_ceiling",
    ),
    // Added 2026-10-02 with the p99 latency target they measure (owner: "O(1)
    // or single milliseconds or microseconds ... even with p99 latency").
    // Same wall-clock shape as the harnesses above: each times every call of
    // a hot-path stage on its own and prints p50 / p99 / p99.9 / max next to
    // the timer floor, so a shared CI runner would make them a flake. They are
    // NOT the merge condition for any behaviour: decode is pinned by the
    // parser tests and DHAT gates, the fold by its own suites, the WAL
    // hand-off by `ws_frame_spill`'s tests. The fold harness asserts it sealed
    // bars, so it cannot report a figure from a fold that did nothing.
    // Measured 2026-10-02 (release, x86 dev container): p99 53 ns decode,
    // 280 ns frame classify, 630 ns WAL hand-off, 898 ns fold, 1.1 us
    // decode + fold. Run deliberately:
    //   cargo test --release -p tickvault-trading --test hot_path_latency -- --ignored --nocapture
    //   cargo test --release -p tickvault-core --test hot_path_latency -- --ignored --nocapture
    (
        "crates/core/tests/hot_path_latency.rs",
        "read_task_latency_distribution",
    ),
    (
        "crates/trading/tests/hot_path_latency.rs",
        "tick_hot_path_latency_distribution",
    ),
    // Added 2026-10-02, same shape and same reason: the database half of the
    // write path. The append harness times one ILP row append (no database);
    // the live harness times a synchronous flush against a running QuestDB and
    // returns early unless TV_QDB_HTTP is set, so it can never run in CI.
    // Measured 2026-10-02 against a local QuestDB 9.3.5 (dev container,
    // commit mode nosync): row append p99 1.2 us (tick) / 0.9 us (depth);
    // flush of 1,000 ticks p50 3.3 ms, p99 23 ms; 10,000 depth rows p50
    // 14 ms, p99 32 ms. With cairo.commit.mode=sync the flush was 6-8x slower.
    (
        "crates/storage/tests/ilp_write_latency.rs",
        "ilp_append_cost_per_row",
    ),
    (
        "crates/storage/tests/ilp_write_latency.rs",
        "ilp_flush_and_visibility_latency_live_questdb",
    ),
];

struct Ignored {
    file: String,
    func: String,
    has_reason: bool,
}

/// Find every `#[ignore]` ATTRIBUTE and the test it sits on.
///
/// Comment lines are skipped. That is not a nicety: this workspace has nine
/// prose mentions of `#[ignore]` in doc comments explaining why things are NOT
/// ignored, and a naive scan reports them as bare ignores — which is exactly
/// what the first pass of this audit did.
fn scan_ignored() -> Vec<Ignored> {
    let root = repo_root();
    let mut out = Vec::new();
    let mut stack = vec![root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if n != "target" {
                    stack.push(p);
                }
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&p) else {
                continue;
            };
            let rel = p
                .strip_prefix(&root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let lines: Vec<&str> = body.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let t = line.trim();
                if t.starts_with("//") || !t.starts_with("#[ignore") {
                    continue;
                }
                let has_reason = t.starts_with("#[ignore =") || t.starts_with("#[ignore(");
                // The test fn is the next `fn` line, skipping further attributes.
                let func = lines[i + 1..]
                    .iter()
                    .take(6)
                    .find_map(|l| {
                        let l = l.trim();
                        let rest = l.strip_prefix("fn ").or_else(|| {
                            l.strip_prefix("async fn ")
                                .or_else(|| l.strip_prefix("pub fn "))
                        })?;
                        Some(rest.split('(').next()?.trim().to_string())
                    })
                    .unwrap_or_else(|| format!("<unparsed at {rel}:{}>", i + 1));
                out.push(Ignored {
                    file: rel.clone(),
                    func,
                    has_reason,
                });
            }
        }
    }
    out
}

#[test]
fn every_ignored_test_states_why() {
    let bare: Vec<String> = scan_ignored()
        .into_iter()
        .filter(|i| !i.has_reason)
        .map(|i| format!("{} :: {}", i.file, i.func))
        .collect();
    assert!(
        bare.is_empty(),
        "these tests are ignored with no stated reason:\n  {}\n\n\
         A bare #[ignore] parks a test with nothing for a reviewer to argue \
         with, and the test-count ratchet cannot see it — the attribute stays, \
         so the count does not move. Write #[ignore = \"why\"].",
        bare.join("\n  ")
    );
}

#[test]
fn the_ignored_set_only_shrinks() {
    let found = scan_ignored();
    assert!(
        !found.is_empty(),
        "the scanner found no #[ignore] attributes at all. There are known to \
         be several, so the scanner is broken — and a broken scanner passes \
         this test vacuously forever."
    );

    let allowed: BTreeSet<(&str, &str)> = ALLOWED_IGNORED.iter().copied().collect();
    let newly: Vec<String> = found
        .iter()
        .filter(|i| !allowed.contains(&(i.file.as_str(), i.func.as_str())))
        .map(|i| format!("{} :: {}", i.file, i.func))
        .collect();

    assert!(
        newly.is_empty(),
        "these tests are newly ignored:\n  {}\n\n\
         The house rule is never delete, skip or disable a test to get a gate \
         green. The count ratchet enforces the DELETE half only: #[ignore] \
         leaves the #[test] attribute in place, so the count never moves and \
         the guard reports stable while the test does not run.\n\
         If this test genuinely is not a gate, add it to ALLOWED_IGNORED here \
         with its reason, in the same change, where a reviewer sees it.",
        newly.join("\n  ")
    );
}

#[test]
fn the_allowlist_does_not_outlive_its_entries() {
    let found: BTreeSet<(String, String)> = scan_ignored()
        .into_iter()
        .map(|i| (i.file, i.func))
        .collect();
    let stale: Vec<String> = ALLOWED_IGNORED
        .iter()
        .filter(|(f, n)| !found.contains(&((*f).to_string(), (*n).to_string())))
        .map(|(f, n)| format!("{f} :: {n}"))
        .collect();
    assert!(
        stale.is_empty(),
        "these allowlist entries no longer match an ignored test:\n  {}\n\n\
         Good news — a test was un-ignored or removed. Drop the entry in the \
         same change, so the allowlist cannot quietly accumulate permission for \
         tests that no longer exist.",
        stale.join("\n  ")
    );
}

#[test]
fn guard_self_test() {
    let found = scan_ignored();
    assert_eq!(
        found.len(),
        ALLOWED_IGNORED.len(),
        "scanner found {} ignored tests against an allowlist of {} — the two \
         other tests here would still pass if the scanner over- or \
         under-counted in a compensating way, so this pins the total",
        found.len(),
        ALLOWED_IGNORED.len()
    );
    assert!(
        found.iter().all(|i| i.has_reason),
        "every ignored test in this workspace carries a reason today; if that \
         changed, every_ignored_test_states_why should have caught it first"
    );
    // The comment-stripping is load-bearing: this workspace has prose mentions
    // of the attribute in doc comments, and counting those would report bare
    // ignores that do not exist.
    assert!(
        found.iter().all(|i| !i.func.starts_with("<unparsed")),
        "an #[ignore] was found with no test function under it — either the \
         scanner is matching prose, or an attribute is orphaned: {:?}",
        found.iter().map(|i| &i.func).collect::<Vec<_>>()
    );
}
