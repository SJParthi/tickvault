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
//! Existing optional measurements and operator fixtures stay on their shrinking
//! list. Isolated SQL cases are different: ordinary tests skip them because
//! they require a disposable database, but the required release workflow must
//! execute each exact case and reject zero tests or any failure.

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
        "cascade_01_wal_roundtrip_while_questdb_paused_preserves_payloads",
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
];

/// These cases are excluded only from the default test process. They are
/// mandatory CI release gates, not an optional ignore budget. The structural
/// wiring and synthetic execution checks below keep both exact cases attached
/// to All Green. Successful live database output remains separate evidence.
const REQUIRED_ISOLATED_SQL: &[(&str, &str)] = &[
    (
        "crates/storage/src/candle_top_volume_views.rs",
        "candle_top_volume_questdb_duplicate_labels_preserve_one_row_per_candle",
    ),
    (
        "crates/storage/src/seal_recovery_guard.rs",
        "isolated_questdb_recovery_inserts_skips_and_refuses_conflicts",
    ),
];

fn allowed_ignored_cases() -> impl Iterator<Item = (&'static str, &'static str)> {
    ALLOWED_IGNORED
        .iter()
        .copied()
        .chain(REQUIRED_ISOLATED_SQL.iter().copied())
}

fn workflow_job(source: &str, name: &str) -> Option<String> {
    let marker = format!("  {name}:");
    let lines: Vec<_> = source.lines().collect();
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| **line == marker)
        .collect();
    if starts.len() != 1 {
        return None;
    }
    let start = starts[0].0 + 1;
    Some(
        lines[start..]
            .iter()
            .take_while(|line| {
                line.trim().is_empty()
                    || line.trim_start().starts_with('#')
                    || line.starts_with("    ")
            })
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

const SQL_FIXTURE_NAMES: [&str; 2] = [
    "console_views::candle_top_volume_views::tests::candle_top_volume_questdb_duplicate_labels_preserve_one_row_per_candle",
    "seal_recovery_guard::tests::isolated_questdb_recovery_inserts_skips_and_refuses_conflicts",
];

/// Extract the actual literal run block; no handwritten copy of the SQL loop.
fn isolated_sql_script(release: &str) -> Option<String> {
    let questdb = workflow_job(release, "questdb")?;
    let marker = "      - name: Execute each ignored SQL fixture and require one passing test each";
    if questdb.matches(marker).count() != 1 {
        return None;
    }
    let step = questdb.split_once(marker)?.1.split("\n      - ").next()?;
    let run = step.split_once("\n        run: |\n")?.1;
    let mut script = String::new();
    for line in run.lines() {
        let body = if line.trim().is_empty() {
            ""
        } else {
            line.strip_prefix("          ")?
        };
        script.push_str(body);
        script.push('\n');
    }
    (!script.is_empty()).then_some(script)
}

/// Cargo and git are synthetic functions. No compiler, registry, database or
/// repository operation is reached. Actual tee/grep and shell error handling
/// remain exercised, including the real pipeline's pipefail behavior.
const SQL_EXECUTION_STUBS: &str = r#"
cargo() {
  if [ "$#" -ne 11 ]; then return 91; fi
  case "$6" in
    "$VIEW_SQL_FIXTURE"|"$RECOVERY_SQL_FIXTURE") ;;
    *) return 92 ;;
  esac
  if [ "$*" != "test --locked -p tickvault-storage --lib $6 -- --ignored --exact --nocapture --test-threads=1" ]; then
    return 93
  fi
  printf '%s\n' "$6" >> "$TV_SQL_CALL_LOG"
  if [ "$6" = "$TV_SQL_TARGET" ]; then
    case "$TV_SQL_MODE" in
      failure) printf 'synthetic fixture failure\n'; return 37 ;;
      missing_running) printf 'test result: ok. 1 passed; 0 failed; 0 ignored;\n'; return 0 ;;
      missing_summary) printf 'running 1 test\n'; return 0 ;;
      silent_success) return 0 ;;
      zero_tests) printf 'running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored;\n'; return 0 ;;
      ignored_test) printf 'running 1 test\ntest result: ok. 0 passed; 0 failed; 1 ignored;\n'; return 0 ;;
      success_output_failure) printf 'running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n'; return 37 ;;
      success) ;;
      *) return 94 ;;
    esac
  fi
  printf 'running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n'
}
git() {
  if [ "$#" = 2 ] && [ "$1" = diff ] && [ "$2" = --exit-code ]; then
    return 0
  fi
  return 95
}
"#;

struct SqlScriptFixture(PathBuf);

impl Drop for SqlScriptFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn observe_sql_script(script: &str, mode: &str, target: &str) -> (bool, Vec<String>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let fixture = SqlScriptFixture(std::env::temp_dir().join(format!(
        "tickvault-ignored-sql-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir(&fixture.0).expect("create unique synthetic SQL fixture");
    std::fs::create_dir(fixture.0.join("release-questdb")).expect("create fixture logs");
    let output = std::process::Command::new("/bin/bash")
        .args(["--noprofile", "--norc", "-c"])
        .arg(format!("{SQL_EXECUTION_STUBS}\n{script}"))
        .current_dir(&fixture.0)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("RUNNER_TEMP", &fixture.0)
        .env("GITHUB_STEP_SUMMARY", fixture.0.join("summary"))
        .env("TV_SQL_CALL_LOG", fixture.0.join("calls"))
        .env("VIEW_SQL_FIXTURE", SQL_FIXTURE_NAMES[0])
        .env("RECOVERY_SQL_FIXTURE", SQL_FIXTURE_NAMES[1])
        .env("TV_SQL_MODE", mode)
        .env("TV_SQL_TARGET", target)
        .env("SOURCE_SHA", "0123456789abcdef0123456789abcdef01234567")
        .env(
            "TICKVAULT_ISOLATED_QUESTDB_EXEC_URL",
            "http://127.0.0.1:1/exec",
        )
        .output()
        .expect("execute actual SQL gate with synthetic Cargo");
    let calls = match std::fs::read_to_string(fixture.0.join("calls")) {
        Ok(calls) => calls.lines().map(str::to_owned).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read synthetic Cargo calls: {error}"),
    };
    (output.status.success(), calls)
}

fn both_sql_fixtures_execute(release: &str) -> bool {
    let Some(script) = isolated_sql_script(release) else {
        return false;
    };
    let (success, calls) = observe_sql_script(&script, "success", "");
    success && calls.iter().map(String::as_str).eq(SQL_FIXTURE_NAMES)
}

fn required_sql_execution_is_wired(release: &str, ci: &str) -> bool {
    let Some(questdb) = workflow_job(release, "questdb") else {
        return false;
    };
    let Some(fanin) = workflow_job(release, "release-validation") else {
        return false;
    };
    let Some(call) = workflow_job(ci, "release-validation") else {
        return false;
    };
    let Some(all_green) = workflow_job(ci, "all-green") else {
        return false;
    };
    if [&questdb, &call]
        .iter()
        .any(|job| job.lines().any(|line| line.starts_with("    if:")))
        || [&questdb, &fanin, &call, &all_green].iter().any(|job| {
            job.lines()
                .any(|line| line.trim_start().starts_with("continue-on-error:"))
        })
    {
        return false;
    }
    let step_marker =
        "      - name: Execute each ignored SQL fixture and require one passing test each";
    let starts: Vec<_> = questdb.match_indices(step_marker).collect();
    if starts.len() != 1 {
        return false;
    }
    let execution = questdb[starts[0].0 + step_marker.len()..]
        .split("\n      - name:")
        .next()
        .unwrap_or("");
    if execution
        .lines()
        .any(|line| line.trim_start().starts_with("if:"))
    {
        return false;
    }
    let has_line = |text: &str, expected: &str| text.lines().any(|line| line.trim() == expected);
    [
        r#"set -euo pipefail"#,
        r#"for fixture in "$VIEW_SQL_FIXTURE" "$RECOVERY_SQL_FIXTURE"; do"#,
        r#"cargo test --locked -p tickvault-storage --lib "$fixture" \"#,
        r#"-- --ignored --exact --nocapture --test-threads=1 \"#,
        r#"grep -Fxq 'running 1 test' "$fixture_log""#,
        r#"grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$fixture_log""#,
        r#"test "$fixture_index" = 2"#,
    ]
    .iter()
    .all(|line| has_line(execution, line))
        && has_line(
            &questdb,
            "VIEW_SQL_FIXTURE: console_views::candle_top_volume_views::tests::candle_top_volume_questdb_duplicate_labels_preserve_one_row_per_candle",
        )
        && has_line(
            &questdb,
            "RECOVERY_SQL_FIXTURE: seal_recovery_guard::tests::isolated_questdb_recovery_inserts_skips_and_refuses_conflicts",
        )
        && has_line(&fanin, "needs: [arm64, questdb]")
        && has_line(&fanin, "QDB_RESULT: ${{ needs.questdb.result }}")
        && has_line(&fanin, r#"test "$QDB_RESULT" = success"#)
        && has_line(&call, "uses: ./.github/workflows/release-validation.yml")
        && has_line(&all_green, "- release-validation")
        && both_sql_fixtures_execute(release)
}

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

    let release =
        std::fs::read_to_string(repo_root().join(".github/workflows/release-validation.yml"))
            .expect("release workflow");
    let ci =
        std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).expect("CI workflow");
    assert!(
        required_sql_execution_is_wired(&release, &ci),
        "isolated SQL exclusions require both exact tests in the mandatory release workflow"
    );
    let allowed: BTreeSet<(&str, &str)> = allowed_ignored_cases().collect();
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
    let stale: Vec<String> = allowed_ignored_cases()
        .filter(|(f, n)| !found.contains(&(f.to_string(), n.to_string())))
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
        allowed_ignored_cases().count(),
        "scanner found {} ignored tests against an allowlist of {} — the two \
         other tests here would still pass if the scanner over- or \
         under-counted in a compensating way, so this pins the total",
        found.len(),
        allowed_ignored_cases().count()
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

#[test]
fn required_sql_wiring_rejects_omissions_skips_and_vacuous_success() {
    let release =
        std::fs::read_to_string(repo_root().join(".github/workflows/release-validation.yml"))
            .expect("release workflow");
    let ci =
        std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).expect("CI workflow");
    assert!(required_sql_execution_is_wired(&release, &ci));
    for needle in [
        r#""$RECOVERY_SQL_FIXTURE""#,
        "--ignored",
        "--exact",
        "grep -Fxq 'running 1 test'",
        "grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;'",
        r#"test "$QDB_RESULT" = success"#,
        "needs: [arm64, questdb]",
    ] {
        assert!(
            release.contains(needle),
            "mutation must change the real workflow: {needle}"
        );
        assert!(
            !required_sql_execution_is_wired(&release.replace(needle, "REMOVED"), &ci),
            "{needle}"
        );
    }
    for (old, new) in [
        ("  questdb:\n", "  questdb:\n    if: false\n"),
        ("  questdb:\n", "  questdb:\n    continue-on-error: true\n"),
        (
            "      - name: Execute each ignored SQL fixture and require one passing test each\n",
            "      - name: Execute each ignored SQL fixture and require one passing test each\n        if: false\n",
        ),
    ] {
        assert!(release.contains(old));
        assert!(!required_sql_execution_is_wired(
            &release.replace(old, new),
            &ci
        ));
    }
    assert!(!required_sql_execution_is_wired(
        &release,
        &ci.replace("      - release-validation\n", "")
    ));
    assert!(!required_sql_execution_is_wired(
        &release,
        &ci.replace(
            "  release-validation:\n",
            "  release-validation:\n    if: false\n"
        )
    ));
}

#[test]
fn mandatory_sql_execution_propagates_failure_and_rejects_missing_test_output() {
    let release =
        std::fs::read_to_string(repo_root().join(".github/workflows/release-validation.yml"))
            .expect("release workflow");
    let script = isolated_sql_script(&release).expect("actual SQL execution script");
    let (success, calls) = observe_sql_script(&script, "success", "");
    assert!(
        success,
        "the normal synthetic pair must pass the actual workflow step"
    );
    assert!(calls.iter().map(String::as_str).eq(SQL_FIXTURE_NAMES));
    for (index, fixture) in SQL_FIXTURE_NAMES.iter().enumerate() {
        for mode in [
            "failure",
            "missing_running",
            "missing_summary",
            "silent_success",
            "zero_tests",
            "ignored_test",
            "success_output_failure",
        ] {
            let (success, calls) = observe_sql_script(&script, mode, fixture);
            assert!(
                !success,
                "the actual SQL step accepted {mode} for {fixture}"
            );
            assert!(
                calls
                    .iter()
                    .map(String::as_str)
                    .eq(SQL_FIXTURE_NAMES[..=index].iter().copied()),
                "{mode} must reach the failing exact fixture and stop there: {calls:?}"
            );
        }
    }
}

#[test]
fn mandatory_sql_execution_cannot_be_replaced_by_an_early_successful_exit() {
    let release =
        std::fs::read_to_string(repo_root().join(".github/workflows/release-validation.yml"))
            .expect("release workflow");
    let ci =
        std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).expect("CI workflow");
    let marker = "      - name: Execute each ignored SQL fixture and require one passing test each\n        run: |\n";
    assert_eq!(release.matches(marker).count(), 1);
    let changed = release.replacen(marker, &format!("{marker}          exit 0\n"), 1);
    let script = isolated_sql_script(&changed).expect("extract early-success mutant");
    let (success, calls) = observe_sql_script(&script, "success", "");
    assert!(
        success,
        "mutation specifically reproduces a successful shell exit"
    );
    assert!(
        calls.is_empty(),
        "the mutant must skip both Cargo invocations"
    );
    assert!(!required_sql_execution_is_wired(&changed, &ci));
}
