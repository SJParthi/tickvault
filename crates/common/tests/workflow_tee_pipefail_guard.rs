//! Guard test — no GitHub Actions step may pipe a command into `tee` without
//! pipefail.
//!
//! A step that has no explicit `shell:` runs under `bash -e {0}`, which has no
//! pipefail. In `cargo fuzz build x | tee log || { ...; exit 1; }` the step
//! then reads the exit code of `tee`, which is 0, so a failed build passes and
//! the failure branch never runs. That is how every fuzz target "ran" for 0 s
//! and reported green on 2026-09-28 (run 36397058798), and the cargo-careful
//! test step in safety.yml had the same hole (found 2026-10-04 audit, C1/H4).
//!
//! The rule: every `run:` block that pipes a command (not an `echo` or
//! `printf`) into `tee` must either set pipefail before that line
//! (`set -o pipefail`, `set -eo pipefail`, `set -euo pipefail`) or belong to a
//! step with `shell: bash`, which GitHub runs as `bash -eo pipefail {0}`.
//!
//! The fuzz lane is also pinned to the glibc target triple, since cargo-fuzz
//! otherwise builds for the platform it was compiled on (a static musl build
//! from install-action), which AddressSanitizer refuses.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// A line that pipes into `tee` (`| tee` with a space, so `||` never matches).
fn pipes_into_tee(line: &str) -> bool {
    !is_comment(line) && line.contains("| tee")
}

/// The first line of the logical shell command that ends on `idx`, following
/// backslash continuations upward.
fn logical_start(lines: &[&str], idx: usize) -> usize {
    let mut start = idx;
    while start > 0 && lines[start - 1].trim_end().ends_with('\\') {
        start -= 1;
    }
    start
}

/// Text of the command on `line`, with any `run:` prefix stripped.
fn command_text(line: &str) -> &str {
    let t = line.trim_start();
    let t = t.strip_prefix("- ").unwrap_or(t);
    match t.strip_prefix("run:") {
        Some(rest) => rest.trim_start(),
        None => t,
    }
}

fn is_message_pipe(lines: &[&str], idx: usize) -> bool {
    let mut first = command_text(lines[logical_start(lines, idx)]);
    // A `case` arm (`set)  echo ... | tee ...`): look past the pattern.
    if let Some((pattern, rest)) = first.split_once(char::is_whitespace)
        && pattern.ends_with(')')
        && !pattern.contains('(')
    {
        first = rest.trim_start();
    }
    first.starts_with("echo ") || first.starts_with("printf ")
}

fn sets_pipefail(line: &str) -> bool {
    !is_comment(line) && line.contains("set -") && line.contains("pipefail")
}

/// Every `| tee` pipeline that runs without pipefail, as `line_number: text`.
fn unprotected_tee_pipes(workflow: &str) -> (usize, Vec<String>) {
    let lines: Vec<&str> = workflow.lines().collect();
    let mut checked = 0usize;
    let mut bad = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if !pipes_into_tee(line) || is_message_pipe(&lines, idx) {
            continue;
        }
        checked += 1;
        // The enclosing `run:` line: the nearest line above (or this one)
        // whose trimmed text starts with `run:` or `- run:`.
        let run_idx = (0..=idx).rev().find(|&i| {
            let t = lines[i].trim_start();
            t.starts_with("run:") || t.starts_with("- run:")
        });
        let Some(run_idx) = run_idx else {
            bad.push(format!("{}: {} (no enclosing run:)", idx + 1, line.trim()));
            continue;
        };
        if (run_idx..idx).any(|i| sets_pipefail(lines[i])) {
            continue;
        }
        // The step header: walk up from the run line to the `- ` list item
        // that opens this step, checking for `shell: bash` on the way.
        let run_indent = indent_of(lines[run_idx]);
        let mut explicit_bash = false;
        let mut i = run_idx;
        loop {
            let t = lines[i].trim_start();
            if t.starts_with("shell:") && t.trim_end() == "shell: bash" {
                explicit_bash = true;
            }
            let opens_step = t.starts_with("- ") && indent_of(lines[i]) < run_indent;
            if opens_step || lines[i].trim_start().starts_with("- run:") || i == 0 {
                break;
            }
            i -= 1;
        }
        if !explicit_bash {
            bad.push(format!("{}: {}", idx + 1, line.trim()));
        }
    }
    (checked, bad)
}

fn workflow_files() -> Vec<PathBuf> {
    let dir = repo_root().join(".github/workflows");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("yml") | Some("yaml")
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_tee_pipeline_in_a_workflow_runs_with_pipefail() {
    let mut total_checked = 0usize;
    let mut failures = Vec::new();
    for path in workflow_files() {
        let text = fs::read_to_string(&path).unwrap();
        let (checked, bad) = unprotected_tee_pipes(&text);
        total_checked += checked;
        for b in bad {
            failures.push(format!("{}:{b}", path.display()));
        }
    }
    // Anti-vacuity: the scan must find the known command-into-tee pipelines
    // (fuzz build/run, careful, DHAT x4, loom x2, chaos x3, mutation, the
    // nightly test run, bench). Finding none means the scanner broke.
    assert!(
        total_checked >= 10,
        "scanned only {total_checked} command-into-tee pipelines; the scanner is broken or the workflows moved"
    );
    assert!(
        failures.is_empty(),
        "these steps pipe a command into `tee` without pipefail, so the step reports the exit code of `tee` (0) and a failure passes green. Add `set -o pipefail` at the top of the run block, or `shell: bash` on the step:\n{}",
        failures.join("\n")
    );
}

#[test]
fn the_scanner_flags_a_tee_pipe_without_pipefail() {
    let bad = "      - name: Build\n        run: |\n          cargo build 2>&1 | tee /tmp/b.log || {\n            exit 1\n          }\n";
    let (checked, flagged) = unprotected_tee_pipes(bad);
    assert_eq!(checked, 1);
    assert_eq!(
        flagged.len(),
        1,
        "a bare `bash -e` tee pipe must be flagged"
    );

    let multi = "      - name: Run\n        run: |\n          timeout 9 cargo fuzz run t \\\n            -- -max_total_time=9 2>&1 | tee /tmp/r.log || true\n";
    assert_eq!(unprotected_tee_pipes(multi).1.len(), 1);
}

#[test]
fn the_scanner_accepts_pipefail_and_explicit_bash_and_message_pipes() {
    let set = "      - name: Build\n        run: |\n          set -o pipefail\n          cargo build 2>&1 | tee /tmp/b.log\n";
    assert!(unprotected_tee_pipes(set).1.is_empty());

    let euo = "      - name: Build\n        run: |\n          set -euo pipefail\n          cargo build 2>&1 | tee /tmp/b.log\n";
    assert!(unprotected_tee_pipes(euo).1.is_empty());

    let shell = "      - name: Build\n        shell: bash\n        run: |\n          cargo build 2>&1 | tee /tmp/b.log\n";
    assert!(unprotected_tee_pipes(shell).1.is_empty());

    let echo = "      - name: Note\n        run: |\n          echo \"done\" | tee -a \"$GITHUB_STEP_SUMMARY\"\n";
    let (checked, flagged) = unprotected_tee_pipes(echo);
    assert_eq!(
        checked, 0,
        "an echo into tee is a message, not a gated command"
    );
    assert!(flagged.is_empty());

    let case_arm = "      - name: Note\n        run: |\n          case \"$x\" in\n            set)  echo \"ok\" | tee -a \"$GITHUB_STEP_SUMMARY\" ;;\n          esac\n";
    assert_eq!(
        unprotected_tee_pipes(case_arm).0,
        0,
        "an echo in a case arm is a message"
    );

    // pipefail set in an EARLIER step does not protect a later step.
    let other_step = "      - name: A\n        run: |\n          set -o pipefail\n          true\n      - name: B\n        run: |\n          cargo build 2>&1 | tee /tmp/b.log\n";
    assert_eq!(unprotected_tee_pipes(other_step).1.len(), 1);
}

#[test]
fn the_fuzz_lane_builds_and_runs_for_the_glibc_target_and_proves_it_ran() {
    let fuzz = fs::read_to_string(repo_root().join(".github/workflows/fuzz.yml")).unwrap();
    assert!(
        fuzz.contains("FUZZ_TARGET_TRIPLE: x86_64-unknown-linux-gnu"),
        "fuzz.yml must pin FUZZ_TARGET_TRIPLE to the glibc triple; cargo-fuzz otherwise targets musl and ASan refuses it"
    );
    assert!(
        fuzz.contains("fuzz build --target \"${FUZZ_TARGET_TRIPLE}\""),
        "the fuzz build must pass --target \"${{FUZZ_TARGET_TRIPLE}}\""
    );
    assert!(
        fuzz.contains("fuzz run --target \"${FUZZ_TARGET_TRIPLE}\""),
        "the fuzz run must pass --target \"${{FUZZ_TARGET_TRIPLE}}\""
    );
    assert!(
        fuzz.contains("Prove the fuzzer actually ran"),
        "the fuzz lane must keep its anti-vacuity step (libFuzzer INITED + a non-zero 'Done N runs' line)"
    );
}
