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
    // Added 2026-10-30 with the combined-fault chaos tests (PR #2157): two
    // tests that expose real defects in the evening-drain state file and
    // fail until those are fixed (the owner decides the fix; root cause and
    // proposed fix are in each test's ignore reason and doc comment). They
    // are kept, not deleted, so the defects stay visible and the fix PR only
    // has to remove these two rows. Run them with:
    //
    //   cargo test -p tickvault-app --test chaos_combined_evening_drain -- --ignored
    (
        "crates/app/tests/chaos_combined_evening_drain.rs",
        "a_single_flipped_bit_never_lets_more_restarts_through",
    ),
    (
        "crates/app/tests/chaos_combined_evening_drain.rs",
        "a_torn_file_from_last_evening_does_not_block_tonight",
    ),
    // Added 2026-10-30 with plan ITEM DT-SOCK (owner: every in-memory step
    // under 1 us at p99): wall-clock harnesses for the socket read path's
    // in-memory work per frame (production spacing, bursts, a per-step
    // breakdown, a live drain, the drain's recv over a backlog). They print
    // percentiles and gate nothing, like the harness above: a wall-clock
    // number must not become a flaky CI gate. The gate is the Criterion
    // budget `ws_reader_frame_handle` (bench-gate); the behaviour is pinned
    // by the ordinary tests in `frame_ring.rs` and `ws_frame_spill/receipt_anchor.rs`.
    //
    //   cargo test --release -p tickvault-core --test socket_read_in_memory_harness -- --ignored --nocapture --test-threads=1
    (
        "crates/core/tests/socket_read_in_memory_harness.rs",
        "socket_read_in_memory_cost_per_frame",
    ),
    (
        "crates/core/tests/socket_read_in_memory_harness.rs",
        "socket_read_in_memory_cost_breakdown",
    ),
    (
        "crates/core/tests/socket_read_in_memory_harness.rs",
        "socket_read_cost_with_a_live_drain_in_a_burst",
    ),
    (
        "crates/core/tests/socket_read_in_memory_harness.rs",
        "ring_recv_cost_over_a_queued_backlog",
    ),
    // Added 2026-10-30 with the order-path work (owner: every in-memory
    // order-path step under 1 us at p99): a wall-clock harness that prints
    // the per-call p50 / p99 / p99.9 / max of each step, with and without a
    // JSON log subscriber. It gates nothing; the gates are the per-call p99
    // gate inside `crates/app/benches/order_path.rs`, the Criterion budget
    // `order_path` (bench-gate) and `dhat_order_path_recorder`. Run it with:
    //
    //   cargo test --profile bench -p tickvault-app --test order_path_p99_harness -- --ignored --nocapture --test-threads=1
    (
        "crates/app/tests/order_path_p99_harness.rs",
        "order_path_per_call_latency_distribution",
    ),
    // Added 2026-10-30 with plan item DT-S1: a wall-clock harness for the
    // hot-thread placement switches (consumer pick-up delay with a busy
    // neighbour on the hot core, three placements). It prints percentiles
    // and gates nothing; it needs two otherwise idle cores and (for its
    // third row) SCHED_FIFO, which a shared CI runner cannot promise. The
    // behaviour is pinned by the ordinary tests in
    // `crates/app/src/drain_thread.rs`, `dhan_feed_stack/drain_thread_tests.rs`
    // and `crates/core/src/websocket/hot_thread_placement.rs`. Run it with:
    //
    //   taskset -c 0,1 cargo test --release -p tickvault-app --test drain_placement_harness -- --ignored --nocapture
    (
        "crates/app/tests/drain_placement_harness.rs",
        "drain_placement_wait_for_cpu_three_ways",
    ),
    // Added 2026-10-26 with audit M4: a wall-clock timing harness for the
    // socket hand-off stage (frames 400 us apart so the WAL writer and the
    // drain are parked). It prints percentiles and gates nothing, the same
    // as the other `#[ignore]`d cost harnesses (a wall-clock number must not
    // become a flaky CI gate); the behaviour is pinned by the ordinary tests
    // in `crates/storage/src/wal_writer_wait.rs`. Re-run it when that wait
    // changes:
    //
    //   cargo test --release -p tickvault-core --test socket_to_wal_tail_harness -- --ignored --nocapture
    (
        "crates/core/tests/socket_to_wal_tail_harness.rs",
        "socket_to_wal_stage_cost_with_parked_consumers",
    ),
    // Added 2026-10-11 with the real-SIGKILL chaos test (audit TEST-1): these
    // two are the CHILD halves of `chaos_wal_real_sigkill.rs`. Each parent
    // test re-runs its own test binary with `--ignored --exact <child>`,
    // SIGKILLs it mid-write, and then checks what survived on disk. The
    // parents are ordinary tests and run in every suite, so the children
    // always run through them; run on their own they would just write until
    // killed, which proves nothing.
    (
        "crates/storage/tests/chaos_wal_real_sigkill.rs",
        "chaos_kill_child_ws_frame_wal_writer",
    ),
    (
        "crates/storage/tests/chaos_wal_real_sigkill.rs",
        "chaos_kill_child_seal_spill_writer",
    ),
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
    // Added 2026-10-30 with the tick row fast price path (plan ITEM DT): the
    // same shape as the entry above. It checks `tick_price_f64` against
    // `round_to_2dp(f32_to_f64_clean(v))` for every f32 below 2^17, both
    // signs (about 2.2 billion inputs). Measured: 0 differences, 265 s in
    // release on the dev container; a debug CI build would take hours. It is
    // not the gate: the proptests beside it (any bit pattern, price-like
    // values, edge values, rows and ILP bytes against the legacy writer) run
    // in every suite. Re-run it whenever `tick_price_f64` changes:
    //
    //   cargo test -p tickvault-storage --release --lib -- --ignored every_fast_path_f32
    (
        "crates/storage/src/tick_row_identity_tests.rs",
        "tick_price_f64_matches_legacy_for_every_fast_path_f32",
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
    // (a volume_leaderboard wall-clock harness, removed 2026-10-22 with its
    // module, plan item 62)
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
    // Added 2026-10-30 with plan item 3b: times the subscribe reconcile's
    // share of the drain's 30 s silence scan at 25,000 expected instruments
    // and prints the number. Same shape as the harnesses above: a wall-clock
    // figure on a shared CI runner is a flake, so it is run in release by
    // hand and never gates a merge. The behaviour is gated by the ordinary
    // tests in the same file.
    (
        "crates/app/src/subscribe_reconcile.rs",
        "sweep_cost_at_the_authorized_ceiling",
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
    // (a volume_leaderboard wall-clock harness, removed 2026-10-22 with its
    // module, plan item 62)
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
