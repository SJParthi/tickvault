//! BENCH TAIL-GATE DEFER GUARD (2026-10-30, PR #2143 review).
//!
//! The p99 tail gates in `crates/storage/benches/seal_path.rs` and
//! `crates/app/benches/feed_drain.rs` used to `exit(1)` on a miss. In
//! `bench.yml` that failed the `cargo bench` step, so the median gate
//! (`scripts/bench-gate.sh`) and the Criterion baseline save were skipped:
//! one slow tail on the nightly runner left the regression arm with no
//! baseline. Now, with `BENCH_TAIL_GATE_DEFER=1`, a miss prints
//! `BENCH-TAIL-GATE-FAILED` and exits 0, and a step AFTER the baseline save
//! fails the run on that line.
//!
//! This guard keeps the three pieces together:
//! 1. both benches print the marker on a miss and exit 1 unless the defer
//!    variable is `1` (a local `cargo bench` still fails);
//! 2. `bench.yml` sets the defer variable on the bench step;
//! 3. `bench.yml` has a step that fails on the marker, placed after the
//!    baseline save and running whenever the bench step succeeded.
//!
//! HONEST LIMIT: a text check of the sources, not a run of the workflow.

use std::path::{Path, PathBuf};

const MARKER: &str = "BENCH-TAIL-GATE-FAILED";
const DEFER_ENV: &str = "BENCH_TAIL_GATE_DEFER";
const BENCHES: [&str; 2] = [
    "crates/storage/benches/seal_path.rs",
    "crates/app/benches/feed_drain.rs",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("bench_tail_gate_defer_guard: cannot canonicalize repo root")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The text of `fn main` up to the end of the file.
fn main_body(src: &str) -> &str {
    let at = src.find("fn main()").expect("bench has a fn main");
    &src[at..]
}

#[test]
fn both_benches_print_the_marker_and_exit_unless_deferred() {
    for rel in BENCHES {
        let src = read(rel);
        assert!(
            src.contains(&format!(
                "const TAIL_GATE_FAILED_MARKER: &str = \"{MARKER}\";"
            )),
            "{rel}: the marker constant is missing or changed"
        );
        assert!(
            src.contains(&format!(
                "const TAIL_GATE_DEFER_ENV: &str = \"{DEFER_ENV}\";"
            )),
            "{rel}: the defer variable constant is missing or changed"
        );
        let main = main_body(&src);
        let print = main
            .find("eprintln!(\"{TAIL_GATE_FAILED_MARKER}")
            .unwrap_or_else(|| panic!("{rel}: main no longer prints the marker on a miss"));
        let gate = main
            .find("std::env::var(TAIL_GATE_DEFER_ENV).as_deref() != Ok(\"1\")")
            .unwrap_or_else(|| panic!("{rel}: main no longer checks the defer variable"));
        let exit = main
            .find("std::process::exit(1)")
            .unwrap_or_else(|| panic!("{rel}: main no longer exits 1 when not deferred"));
        assert!(
            print < gate && gate < exit,
            "{rel}: main must print the marker, then exit 1 unless deferred"
        );
    }
}

#[test]
fn bench_yml_defers_and_fails_after_the_baseline_save() {
    let wf = read(".github/workflows/bench.yml");
    let bench_step = wf
        .find("- name: Run benchmarks (workspace)")
        .expect("bench.yml: bench step present");
    let bench_run = wf[bench_step..]
        .find("run:")
        .map(|i| bench_step + i)
        .expect("bench step has a run");
    let defer_line = format!("{DEFER_ENV}: '1'");
    let defer = wf[bench_step..bench_run]
        .find(&defer_line)
        .unwrap_or_else(|| panic!("bench.yml: the bench step no longer sets {defer_line}"));
    assert!(defer > 0);
    let save = wf
        .find("- name: Save Criterion baseline")
        .expect("bench.yml: baseline save step present");
    let check = wf
        .find("- name: Fail on a p99 tail-gate miss")
        .expect("bench.yml: the tail-miss step is gone");
    assert!(
        save < check,
        "bench.yml: the tail-miss step must come after the baseline save"
    );
    let check_step = &wf[check..];
    let end = check_step[2..]
        .find("\n      - name:")
        .map_or(check_step.len(), |i| i + 2);
    let check_step = &check_step[..end];
    assert!(
        check_step.contains("if: always() && steps.bench.outcome == 'success'"),
        "bench.yml: the tail-miss step must run whenever the benches completed"
    );
    assert!(
        check_step.contains(&format!("! grep -n '{MARKER}' bench-output.log")),
        "bench.yml: the tail-miss step must fail on the marker in bench-output.log"
    );
    assert!(
        wf.contains("2>&1 | tee bench-output.log"),
        "bench.yml: the marker is printed on stderr, so the log must capture it"
    );
}
