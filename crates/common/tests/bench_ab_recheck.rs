//! End-to-end proof for the same-runner A/B re-measure of the benchmark gate
//! (`crates/common/src/bin/bench_ab_recheck.rs`, 2026-11-07).
//!
//! Each case runs the REAL `scripts/bench-gate.sh` and the REAL
//! `bench_ab_recheck` binary. Only the Criterion bench binaries are
//! stand-ins: one small std-only program compiled here with `rustc` and
//! copied to cargo's bench-binary names, which writes Criterion's `new/`,
//! `<baseline>/` and `change/` records for the ids it is asked to run. Its
//! mean comes from the environment, per folder (`deps` = the head build,
//! `tv_ab_bins` = the baseline build the previous run saved) and per target.
//!
//! The fixture is the failing nightly of 2026-11-07 (run 42315415324):
//! `pipeline/batch_100_mixed` +54.4% (CI lower bound +43.5%),
//! `pipeline/burst_100_ticker` +66.1% (+53.4%), `seal_path/take_submit`
//! +13.2% (+10.9%), every absolute budget inside its limit.
//!
//! Cases:
//! - same commit, the swing was the runner: re-measured flat, gate passes;
//! - a real slowdown between two commits: still slower on the same runner,
//!   gate still fails, and only on the bench that really slowed;
//! - an absolute breach on a flagged bench survives the re-measure;
//! - nothing to compare against (no saved binary, another commit): refused;
//! - the workflow wires it that way.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

const HEAD_SHA: &str = "4189e98a8c0ffee00000000000000000000000aa";
const OTHER_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const PIPELINE_BIN: &str = "pipeline-6d4df94fc326a801";
const SEAL_BIN: &str = "seal_path-339cdda5954cbbc8";

const FAKE_SRC: &str = r#"
use std::{env, fs, path::Path, process::exit};
fn field(text: &str, key: &str) -> Option<String> {
    let at = text.find(key)? + key.len();
    let rest = &text[at..];
    let end = rest.find(|c: char| c == ',' || c == '}' || c == '"').unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}
fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.join("new").join("benchmark.json").is_file() { out.push(p.clone()); }
                walk(&p, out);
            }
        }
    }
}
fn main() {
    // <bench binary> --bench --noplot <mode> <name> <filter>
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 5 || args[0] != "--bench" || args[1] != "--noplot" { exit(5); }
    if env::var("BENCH_TAIL_GATE_DEFER").as_deref() != Ok("1") { exit(11); }
    let home = env::var("CRITERION_HOME").unwrap_or_else(|_| exit(4));
    let (mode, name, filter) = (&args[2], &args[3], &args[4]);
    let me = env::current_exe().unwrap();
    let file = me.file_name().unwrap().to_string_lossy().to_string();
    let target = file.rsplit_once('-').unwrap().0.to_string();
    let folder = me.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
    let mean: f64 = env::var(format!("FAKE_MEAN_NS_{folder}_{target}")).ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| exit(7));
    let inner = filter.strip_prefix("^(?:").and_then(|f| f.strip_suffix(")$")).unwrap_or_else(|| exit(6));
    let ids: Vec<String> = inner.split('|').map(|s| s.replace('\\', "")).collect();
    let mut dirs = Vec::new();
    walk(Path::new(&home), &mut dirs);
    for d in dirs {
        let bj = fs::read_to_string(d.join("new").join("benchmark.json")).unwrap();
        let id = field(&bj, "\"full_id\":\"").unwrap();
        if !ids.contains(&id) { continue; }
        let est = format!("{{\"mean\":{{\"point_estimate\":{mean},\"confidence_interval\":{{\"lower_bound\":{},\"upper_bound\":{}}}}},\"median\":{{\"point_estimate\":{mean}}}}}", mean * 0.99, mean * 1.01);
        fs::write(d.join("new").join("estimates.json"), &est).unwrap();
        let named = d.join(name);
        if mode == "--save-baseline" {
            fs::create_dir_all(&named).unwrap();
            fs::write(named.join("estimates.json"), &est).unwrap();
        } else if mode == "--baseline" {
            let base = fs::read_to_string(named.join("estimates.json")).unwrap_or_else(|_| exit(8));
            let b: f64 = field(&base, "\"point_estimate\":").unwrap().parse().unwrap();
            let p = mean / b - 1.0;
            fs::create_dir_all(d.join("change")).unwrap();
            fs::write(d.join("change").join("estimates.json"), format!("{{\"mean\":{{\"point_estimate\":{p},\"confidence_interval\":{{\"lower_bound\":{},\"upper_bound\":{}}}}}}}", p - 0.01, p + 0.01)).unwrap();
        } else { exit(9); }
    }
}
"#;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A fresh temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "tv-bench-ab-test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir"); // APPROVED: test
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The stand-in bench program, compiled once.
fn fake_bench() -> &'static Path {
    static FAKE: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FAKE
        .get_or_init(|| {
            let dir = TempDir::new("fake");
            let src = dir.0.join("fake.rs");
            std::fs::write(&src, FAKE_SRC).expect("write fake source"); // APPROVED: test
            let exe = dir.0.join("fake");
            let status = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
                .args(["--edition", "2021", "-o"])
                .arg(&exe)
                .arg(&src)
                .status()
                .expect("rustc runs"); // APPROVED: test
            assert!(status.success(), "the fake bench program must compile");
            (dir, exe)
        })
        .1
}

fn place_fake(at: &Path) {
    std::fs::create_dir_all(at.parent().expect("parent")).expect("dir"); // APPROVED: test
    std::fs::copy(fake_bench(), at).expect("copy fake"); // APPROVED: test
}

/// (Criterion id, new median ns, change point, change lower bound)
const FLAGGED: &[(&str, f64, f64, f64)] = &[
    ("pipeline/batch_100_mixed", 1200.0, 0.544, 0.435),
    ("pipeline/burst_100_ticker", 900.0, 0.661, 0.534),
    ("seal_path/take_submit", 400.0, 0.132, 0.109),
];

/// Benches that did not move.
const FLAT: &[(&str, f64)] = &[
    ("registry/get_hit", 14.0),
    ("oms/state_transition", 38.0),
    ("calendar/is_trading_day", 9.0),
    ("dispatch_frame/ticker", 16.0),
    ("token_handle_load", 6.0),
];

fn write_bench(criterion: &Path, id: &str, median: f64, point: f64, lower: f64) {
    let dir = criterion.join(id);
    std::fs::create_dir_all(dir.join("new")).expect("new"); // APPROVED: test
    std::fs::create_dir_all(dir.join("change")).expect("change"); // APPROVED: test
    std::fs::write(
        dir.join("new/estimates.json"),
        format!("{{\"median\":{{\"point_estimate\":{median}}}}}"),
    )
    .expect("write"); // APPROVED: test
    std::fs::write(
        dir.join("new/benchmark.json"),
        format!("{{\"full_id\":\"{id}\",\"title\":\"{id}\"}}"),
    )
    .expect("write"); // APPROVED: test
    std::fs::write(
        dir.join("change/estimates.json"),
        format!(
            "{{\"mean\":{{\"point_estimate\":{point},\"confidence_interval\":{{\"lower_bound\":{lower},\"upper_bound\":{point}}}}}}}"
        ),
    )
    .expect("write"); // APPROVED: test
}

/// The workspace the tool runs in: fixture Criterion dir, logs, head bench
/// binaries and (optionally) the baseline run's saved binaries.
struct Fixture {
    ws: TempDir,
}

impl Fixture {
    fn criterion(&self) -> PathBuf {
        self.ws.0.join("target/criterion")
    }

    fn new(seal_median: f64, saved_baseline_bins: bool) -> Self {
        let ws = TempDir::new("ws");
        let criterion = ws.0.join("target/criterion");
        for (id, median, point, lower) in FLAGGED {
            let median = if id.starts_with("seal_path") {
                seal_median
            } else {
                *median
            };
            write_bench(&criterion, id, median, *point, *lower);
        }
        for (id, median) in FLAT {
            write_bench(&criterion, id, *median, 0.004, -0.012);
        }
        for bin in [PIPELINE_BIN, SEAL_BIN] {
            place_fake(&ws.0.join("target/release/deps").join(bin));
            if saved_baseline_bins {
                place_fake(&criterion.join("tv_ab_bins").join(bin));
            }
        }
        let log = format!(
            "\u{1b}[1m\u{1b}[92m     Running\u{1b}[0m benches/pipeline.rs (target/release/deps/{PIPELINE_BIN})\n\
             Benchmarking pipeline/batch_100_mixed\nBenchmarking pipeline/batch_100_mixed: Warming up for 3.0000 s\n\
             Benchmarking pipeline/burst_100_ticker: Analyzing\n\
             \u{1b}[1m\u{1b}[92m     Running\u{1b}[0m benches/seal_path.rs (target/release/deps/{SEAL_BIN})\n\
             Benchmarking seal_path/take_submit: Warming up for 3.0000 s\n"
        );
        std::fs::write(ws.0.join("bench-output.log"), log).expect("write"); // APPROVED: test
        Fixture { ws }
    }

    /// The real gate on the fixture: (exit code, output).
    fn gate(&self) -> (i32, String) {
        let out = Command::new("bash")
            .arg("scripts/bench-gate.sh")
            .arg(self.criterion())
            .current_dir(repo_root())
            .env("BENCH_BUDGET_MULTIPLIER", "2.5")
            .output()
            .expect("bench-gate.sh runs"); // APPROVED: test
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(-1), text)
    }

    /// First gate run, as the workflow does it, into gate-output.log.
    fn first_gate(&self) -> (i32, String) {
        let (rc, text) = self.gate();
        std::fs::write(self.ws.0.join("gate-output.log"), &text).expect("write"); // APPROVED: test
        (rc, text)
    }

    /// The tool, with the restored cache key's commit and per-build means
    /// (`deps_<target>` = head, `tv_ab_bins_<target>` = saved baseline).
    fn recheck(&self, key_sha: &str, means: &[(&str, f64)]) -> (i32, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bench_ab_recheck"));
        cmd.current_dir(&self.ws.0)
            .args(["target/criterion", "bench-output.log", "gate-output.log"])
            .env(
                "BENCH_AB_BASELINE_KEY",
                format!("criterion-baseline-5cc632cfbd58-42311492058-{key_sha}"),
            )
            .env("GITHUB_SHA", HEAD_SHA);
        for (build, mean) in means {
            cmd.env(format!("FAKE_MEAN_NS_{build}"), mean.to_string());
        }
        let out = cmd.output().expect("bench_ab_recheck runs"); // APPROVED: test
        (
            out.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.criterion().join(rel)).expect("read") // APPROVED: test
    }
}

#[test]
fn the_2026_11_07_fixture_fails_the_gate_before_any_re_measure() {
    let fx = Fixture::new(400.0, false);
    let (rc, out) = fx.first_gate();
    assert_eq!(rc, 2, "{out}");
    assert!(
        out.contains("FAIL: pipeline_batch_100_mixed: +54.4% regression"),
        "{out}"
    );
    assert!(
        !out.contains("HARDWARE DRIFT"),
        "three of eight is not drift: {out}"
    );
}

#[test]
fn same_commit_runner_swing_passes_after_a_same_runner_re_measure() {
    let fx = Fixture::new(400.0, false);
    assert_eq!(fx.first_gate().0, 2);
    // Same commit, no saved binary: the head binary plays both parts. Same
    // code, same runner, same speed.
    let (rc, out) = fx.recheck(
        HEAD_SHA,
        &[("deps_pipeline", 1200.0), ("deps_seal_path", 400.0)],
    );
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("3 flagged benchmark(s) re-measured"), "{out}");
    assert!(out.contains("the head binary (same commit)"), "{out}");
    let (rc, gate) = fx.gate();
    assert_eq!(rc, 0, "the runner swing must not fail the gate: {gate}");
    assert!(gate.contains("All benchmarks within budget."), "{gate}");
    // The original measurement is back and the A/B leaves nothing behind.
    assert_eq!(
        fx.read("pipeline/batch_100_mixed/new/estimates.json"),
        "{\"median\":{\"point_estimate\":1200}}"
    );
    for id in ["pipeline/batch_100_mixed", "seal_path/take_submit"] {
        assert!(!fx.criterion().join(id).join("tv_ab").exists());
        assert!(!fx.criterion().join(id).join("tv_ab_orig_new").exists());
    }
}

#[test]
fn a_runner_swing_between_commits_passes_against_the_saved_baseline_binary() {
    let fx = Fixture::new(400.0, true);
    assert_eq!(fx.first_gate().0, 2);
    // Another commit, but the code is as fast as before on this runner.
    let (rc, out) = fx.recheck(
        OTHER_SHA,
        &[
            ("tv_ab_bins_pipeline", 1200.0),
            ("deps_pipeline", 1201.0),
            ("tv_ab_bins_seal_path", 400.0),
            ("deps_seal_path", 401.0),
        ],
    );
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("the baseline run's saved binary"), "{out}");
    assert_eq!(fx.gate().0, 0);
}

#[test]
fn a_real_slowdown_between_commits_still_fails_on_the_same_runner() {
    let fx = Fixture::new(400.0, true);
    assert_eq!(fx.first_gate().0, 2);
    // Same runner: the pipeline code really is 50% slower; seal_path is not.
    let (rc, out) = fx.recheck(
        OTHER_SHA,
        &[
            ("tv_ab_bins_pipeline", 800.0),
            ("deps_pipeline", 1200.0),
            ("tv_ab_bins_seal_path", 400.0),
            ("deps_seal_path", 400.0),
        ],
    );
    assert_eq!(rc, 0, "{out}");
    let (rc, gate) = fx.gate();
    assert_eq!(rc, 2, "a real regression must still fail: {gate}");
    assert!(
        gate.contains("FAIL: pipeline_batch_100_mixed: +50.0% regression"),
        "{gate}"
    );
    assert!(
        gate.contains("FAIL: pipeline_burst_100_ticker: +50.0% regression"),
        "{gate}"
    );
    assert!(!gate.contains("FAIL: seal_path_take_submit"), "{gate}");
}

#[test]
fn an_absolute_breach_on_a_flagged_bench_survives_the_re_measure() {
    // seal_path at 3,000 ns is over its 1,000 ns x 2.5 budget.
    let fx = Fixture::new(3000.0, false);
    let (rc, out) = fx.first_gate();
    assert_eq!(rc, 2, "{out}");
    // The re-measure sees a fast seal_path (400 ns) and no regression...
    let (rc, out) = fx.recheck(
        HEAD_SHA,
        &[("deps_pipeline", 1200.0), ("deps_seal_path", 400.0)],
    );
    assert_eq!(rc, 0, "{out}");
    // ...but the gate still judges the first measurement against the budget.
    let (rc, gate) = fx.gate();
    assert_eq!(rc, 1, "{gate}");
    assert!(
        gate.contains("FAIL: seal_path_take_submit: 3000ns"),
        "{gate}"
    );
}

#[test]
fn no_saved_binary_and_another_commit_is_not_re_measured() {
    let fx = Fixture::new(400.0, false);
    assert_eq!(fx.first_gate().0, 2);
    let before = fx.read("pipeline/batch_100_mixed/change/estimates.json");
    let (rc, out) = fx.recheck(
        OTHER_SHA,
        &[("deps_pipeline", 1200.0), ("deps_seal_path", 400.0)],
    );
    assert_eq!(rc, 1, "{out}");
    assert!(
        out.contains("no saved baseline binary for pipeline"),
        "{out}"
    );
    assert_eq!(
        before,
        fx.read("pipeline/batch_100_mixed/change/estimates.json"),
        "nothing may change when the tool refuses"
    );
    assert_eq!(fx.gate().0, 2);
    // A cache key from before 2026-11-07 carries no commit at all.
    let (rc, _) = fx.recheck("", &[]);
    assert_eq!(rc, 1);
}

#[test]
fn a_failing_bench_binary_keeps_the_verdict_and_restores_new() {
    let fx = Fixture::new(400.0, false);
    assert_eq!(fx.first_gate().0, 2);
    // No mean for seal_path: the stand-in exits non-zero, as a crashed bench would.
    let (rc, out) = fx.recheck(HEAD_SHA, &[("deps_pipeline", 1200.0)]);
    assert_eq!(rc, 1, "{out}");
    assert_eq!(
        fx.read("seal_path/take_submit/new/estimates.json"),
        "{\"median\":{\"point_estimate\":400}}"
    );
    assert!(
        !fx.criterion()
            .join("seal_path/take_submit/tv_ab_orig_new")
            .exists()
    );
}

#[test]
fn a_binary_outside_the_two_folders_is_never_started() {
    let fx = Fixture::new(400.0, false);
    assert_eq!(fx.first_gate().0, 2);
    // The bench log names a binary somewhere else: refused, never run.
    let log = fx.ws.0.join("bench-output.log");
    let text = std::fs::read_to_string(&log)
        .expect("read") // APPROVED: test
        .replace("target/release/deps/", "elsewhere/");
    std::fs::write(&log, text).expect("write"); // APPROVED: test
    place_fake(&fx.ws.0.join("elsewhere").join(PIPELINE_BIN));
    let (rc, out) = fx.recheck(HEAD_SHA, &[("elsewhere_pipeline", 1200.0)]);
    assert_eq!(rc, 1, "{out}");
    assert!(
        out.contains("not a bench binary this tool may run"),
        "{out}"
    );
}

#[test]
fn too_many_flagged_benches_are_not_re_measured() {
    let fx = Fixture::new(400.0, false);
    let lines: String = (0..17)
        .map(|i| format!("  FAIL: b{i}: +9.0% regression (CI lower bound +8.0% > 5.0%)\n"))
        .collect();
    std::fs::write(fx.ws.0.join("gate-output.log"), lines).expect("write"); // APPROVED: test
    let (rc, out) = fx.recheck(HEAD_SHA, &[]);
    assert_eq!(rc, 1, "{out}");
    assert!(out.contains("17 benchmarks flagged"), "{out}");
}

#[test]
fn missing_inputs_refuse() {
    let fx = Fixture::new(400.0, false);
    // No gate-output.log yet.
    let (rc, out) = fx.recheck(HEAD_SHA, &[]);
    assert_eq!(rc, 1, "{out}");
    // A gate output with no regression line.
    std::fs::write(fx.ws.0.join("gate-output.log"), "  PASS: a: 1ns\n").expect("write"); // APPROVED: test
    let (rc, out) = fx.recheck(HEAD_SHA, &[]);
    assert_eq!(rc, 1, "{out}");
    assert!(out.contains("no confident regression"), "{out}");
    // A flagged key with no Criterion directory.
    std::fs::write(
        fx.ws.0.join("gate-output.log"),
        "  FAIL: nowhere: +9.0% regression (CI lower bound +8.0% > 5.0%)\n",
    )
    .expect("write"); // APPROVED: test
    let (rc, out) = fx.recheck(HEAD_SHA, &[]);
    assert_eq!(rc, 1, "{out}");
    assert!(out.contains("nowhere: no Criterion directory"), "{out}");
    // Bad usage.
    let out = Command::new(env!("CARGO_BIN_EXE_bench_ab_recheck"))
        .output()
        .expect("runs"); // APPROVED: test
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn the_workflow_runs_the_re_measure_only_on_a_regression_and_regates() {
    let wf = std::fs::read_to_string(repo_root().join(".github/workflows/bench.yml"))
        .expect("bench.yml"); // APPROVED: test
    let key = "key: criterion-baseline-${{ steps.hw.outputs.fingerprint }}-${{ github.run_id }}-${{ github.sha }}";
    assert_eq!(
        wf.matches(key).count(),
        2,
        "the restore and the save key must both end in the commit"
    );
    assert!(wf.contains("id: restore"), "the restore step needs an id");
    assert!(wf.contains("BENCH_AB_BASELINE_KEY: ${{ steps.restore.outputs.cache-matched-key }}"));
    let at = wf.find("- name: Run bench-gate").expect("gate step"); // APPROVED: test
    let gate = &wf[at..];
    let gate = &gate[..gate.find("\n      - name:").unwrap_or(gate.len())];
    let find = |needle: &str| {
        gate.find(needle)
            .unwrap_or_else(|| panic!("gate step lost: {needle}"))
    };
    let first = find("bash scripts/bench-gate.sh | tee gate-output.log && rc=0 || rc=$?");
    let tool = find(
        "if [ \"$rc\" = 2 ] && cargo run -q --profile bench -p tickvault-common --bin bench_ab_recheck",
    );
    let regate =
        find("then rc=0; bash scripts/bench-gate.sh | tee -a gate-output.log || rc=$?; fi");
    // The head binaries are kept for the NEXT run only after this run's A/B
    // has used the restored ones.
    let keep = find("grep -o 'target/release/deps/[^)]*' bench-output.log");
    let out = find("echo \"gate_rc=$rc\"");
    assert!(first < tool && tool < regate && regate < keep && keep < out);
    // The saved binaries ride in the Criterion dir, which the save step saves.
    assert!(gate.contains("target/criterion/tv_ab_bins"));
}
