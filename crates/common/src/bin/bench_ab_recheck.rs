//! Same-runner A/B re-measure for the nightly benchmark gate (2026-11-07).
//!
//! Why: the nightly `Benchmarks & Perf Gate` run 42315415324 failed on five
//! "confident regressions" (`pipeline_burst_100_ticker` +66.1%, CI lower
//! bound +53.4%) against a baseline recorded by run 42311492058 on the SAME
//! commit and the SAME CPU model. Same code, so the swing was the runner.
//! Criterion's confidence interval only measures variance inside one run, so
//! it calls a runner-to-runner swing "confident". A red that does not mean
//! "the code got slower" teaches people to ignore the red that does.
//!
//! What it does, only after `scripts/bench-gate.sh` exits 2:
//!
//! 1. Reads the flagged benchmarks from the gate's `FAIL: ... regression`
//!    lines, and each one's bench binary from the bench log.
//! 2. Picks the BASELINE build of that binary: the copy the baseline run
//!    saved with its Criterion records (`<criterion>/tv_ab_bins/`, restored
//!    from the same cache entry, so built from the baseline commit); or, when
//!    the restored cache key ends in the head commit, the head binary itself.
//!    Neither available: it refuses.
//! 3. On THIS runner, back to back: runs the baseline binary
//!    (`--save-baseline tv_ab`), then the head binary (`--baseline tv_ab`),
//!    filtered to exactly the flagged ids, so Criterion writes each flagged
//!    benchmark's `change/estimates.json` from a same-runner comparison.
//! 4. Puts each flagged benchmark's ORIGINAL `new/` back, so the absolute
//!    budget arm of the second gate run judges exactly the numbers the first
//!    run judged (the re-measure can never rescue an absolute breach).
//!
//! The workflow then runs `scripts/bench-gate.sh` a second time, unchanged:
//! same absolute budgets, same 5% threshold, same CI-lower-bound rule. A real
//! regression is slower on the same runner too, so it still fails.
//!
//! Exit codes: 0 = re-measured, run the gate again; 1 = could not re-measure
//! (the workflow keeps the gate's exit 2, fail closed); 2 = bad usage.
//!
//! Usage: `bench_ab_recheck <criterion-dir> <bench-output.log> <gate-output.log>`,
//! run from the workspace root. Env: `BENCH_AB_BASELINE_KEY` (the restored
//! cache key), `GITHUB_SHA` (the head commit).
//!
//! It spawns nothing but Criterion bench binaries, and only after
//! `checked_bench_exe` has confirmed the path is a cargo-built bench binary
//! in one of the two places named above.
//!
//! HONEST LIMITS: a cache saved before 2026-11-07 has neither saved binaries
//! nor a commit in its key, so a run that restores one refuses, until a run
//! saves a new entry. The baseline runs first and the head second, so a
//! runner that slows down during the job biases toward a regression (never
//! toward a pass). Two runs a few seconds apart on one machine can still
//! differ; this narrows the noise to what one runner does inside one job, it
//! does not remove it.

#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Criterion baseline name the A/B pair is saved under.
const AB_BASELINE: &str = "tv_ab";
/// Where each flagged benchmark's original `new/` waits during the A/B.
const ORIG_NEW: &str = "tv_ab_orig_new";
/// Where bench.yml keeps each run's bench binaries, inside the Criterion dir
/// so they are saved and restored with that run's records.
const SAVED_BINS: &str = "tv_ab_bins";
/// More flagged benchmarks than this is not runner noise worth an A/B (and
/// would not fit the job's time limit): the red stands.
const MAX_AB_BENCHES: usize = 16;

/// One flagged benchmark, resolved to its Criterion directory and binary.
#[derive(Debug, Clone, PartialEq)]
struct Flagged {
    dir: PathBuf,
    full_id: String,
    /// The head bench binary as the bench log names it.
    exe: String,
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ ... final byte in '@'..='~'.
            for d in chars.by_ref() {
                if ('@'..='~').contains(&d) && d != '[' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Bench keys of the gate's regression failures (not its absolute ones).
fn regression_keys(gate_output: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for line in gate_output.lines() {
        let Some(rest) = line.trim_start().strip_prefix("FAIL: ") else {
            continue;
        };
        let Some((key, verdict)) = rest.split_once(": ") else {
            continue;
        };
        if verdict.contains("% regression (") && !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
        }
    }
    keys
}

/// The commit at the end of a cache key, when it is a full 40-hex SHA.
fn sha_from_cache_key(key: &str) -> Option<String> {
    let last = key.trim().rsplit('-').next()?;
    (last.len() == 40 && last.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| last.to_ascii_lowercase())
}

/// The gate's key for a `<root>/change` record, computed the way its awk
/// does: drop the FIRST `/change`, turn `/` into `_`, lowercase.
fn gate_key(root: &str) -> String {
    let rel = format!("{root}/change");
    let rel = rel.replacen("/change", "", 1);
    rel.replace('/', "_").to_lowercase()
}

/// Every benchmark root (relative to the Criterion dir) with a change record.
fn change_roots(criterion: &Path) -> Vec<String> {
    let mut roots = Vec::new();
    let mut stack = vec![criterion.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|n| n == "change")
                && path.join("estimates.json").is_file()
                && let Ok(rel) = dir.strip_prefix(criterion)
            {
                roots.push(rel.to_string_lossy().replace('\\', "/"));
            }
            stack.push(path);
        }
    }
    roots.sort();
    roots
}

/// `full_id` and `title` from `<dir>/new/benchmark.json`.
fn bench_ids(dir: &Path) -> Result<(String, String), String> {
    let path = dir.join("new").join("benchmark.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let field = |name: &str| v.get(name).and_then(|x| x.as_str()).map(str::to_string);
    match (field("full_id"), field("title")) {
        (Some(full_id), Some(title)) => Ok((full_id, title)),
        _ => Err(format!("{}: no full_id/title", path.display())),
    }
}

/// The bench binary that printed `Benchmarking <title>`, from cargo's
/// `Running <src> (<path>)` line above it.
fn exe_for_title(bench_log: &str, title: &str) -> Option<String> {
    let mut current: Option<String> = None;
    for raw in bench_log.lines() {
        let line = strip_ansi(raw);
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Running ") {
            if let (Some(open), true) = (rest.rfind('('), rest.ends_with(')')) {
                current = Some(rest[open + 1..rest.len() - 1].to_string());
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Benchmarking ")
            && (rest == title || rest.starts_with(&format!("{title}: ")))
        {
            return current;
        }
    }
    None
}

/// Escapes a benchmark id for Criterion's regex filter.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A Criterion filter that matches exactly these ids.
fn exact_filter(ids: &[String]) -> String {
    let alts: Vec<String> = ids.iter().map(|i| regex_escape(i)).collect();
    format!("^(?:{})$", alts.join("|"))
}

/// The bench target of a cargo-built bench binary name
/// (`pipeline-6d4df94fc326a801` -> `pipeline`), or `None` when the name is
/// not `<identifier>-<16 hex>`.
fn bench_target(file_name: &str) -> Option<&str> {
    let (target, hash) = file_name.rsplit_once('-')?;
    let target_ok = !target.is_empty()
        && target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let hash_ok = hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit());
    (target_ok && hash_ok).then_some(target)
}

/// The ONLY gate in front of a spawn: `path` must be a regular file named
/// like a cargo bench binary, sitting directly in a `release/deps` folder or
/// in the saved-binaries folder. Anything else is refused, so this tool can
/// only ever start a bench binary cargo built for this workspace.
fn checked_bench_exe(path: &Path, saved_bins: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(Path::new(""));
    let in_deps = parent.ends_with("release/deps");
    let in_saved = parent == saved_bins;
    if bench_target(&name).is_none() || !(in_deps || in_saved) || !path.is_file() {
        return Err(format!(
            "{}: not a bench binary this tool may run",
            path.display()
        ));
    }
    Ok(path.to_path_buf())
}

/// The saved baseline binary for a bench target, if exactly one is there.
fn saved_bin_for(saved_bins: &Path, target: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(saved_bins).ok()?;
    let found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(bench_target)
                == Some(target)
        })
        .collect();
    match found.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

fn copy_dir_files(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            std::fs::copy(&path, to.join(entry.file_name()))
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// Removes a directory if it exists.
fn remove_dir(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

/// Everything the re-measure reads from outside.
struct Inputs {
    root: PathBuf,
    criterion: PathBuf,
    bench_log: String,
    gate_output: String,
    baseline_key: String,
    head_sha: String,
}

/// Resolves every flagged benchmark, or refuses.
fn resolve(inp: &Inputs) -> Result<Vec<Flagged>, String> {
    let keys = regression_keys(&inp.gate_output);
    if keys.is_empty() {
        return Err("no confident regression in the gate output".into());
    }
    if keys.len() > MAX_AB_BENCHES {
        return Err(format!(
            "{} benchmarks flagged, more than {MAX_AB_BENCHES}: not re-measured",
            keys.len()
        ));
    }
    let roots = change_roots(&inp.criterion);
    let mut flagged = Vec::new();
    for key in &keys {
        let matching: Vec<&String> = roots.iter().filter(|r| gate_key(r) == *key).collect();
        if matching.is_empty() {
            return Err(format!("{key}: no Criterion directory"));
        }
        for root in matching {
            let dir = inp.criterion.join(root);
            let (full_id, title) = bench_ids(&dir)?;
            let exe = exe_for_title(&inp.bench_log, &title)
                .ok_or_else(|| format!("{title}: not found in the bench log"))?;
            flagged.push(Flagged { dir, full_id, exe });
        }
    }
    Ok(flagged)
}

/// Runs one bench binary on the flagged ids.
fn run_bench(inp: &Inputs, exe: &Path, mode: &str, filter: &str) -> Result<(), String> {
    let program = checked_bench_exe(exe, &inp.criterion.join(SAVED_BINS))?;
    let status = Command::new(&program)
        .current_dir(&inp.root)
        .args(["--bench", "--noplot", mode, AB_BASELINE, filter])
        .env("CRITERION_HOME", &inp.criterion)
        // The p99 tail gates were judged on the first run's log; a re-measure
        // must not exit on them before writing its records.
        .env("BENCH_TAIL_GATE_DEFER", "1")
        .status()
        .map_err(|e| format!("{}: could not start: {e}", program.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} {mode}: exited {status}", program.display()))
    }
}

/// One A/B pair for one bench binary; always puts the original `new/` back.
fn ab_pair(inp: &Inputs, base: &Path, head: &Path, members: &[&Flagged]) -> Result<(), String> {
    let ids: Vec<String> = members.iter().map(|f| f.full_id.clone()).collect();
    let filter = exact_filter(&ids);
    for f in members {
        remove_dir(&f.dir.join(ORIG_NEW))?;
        copy_dir_files(&f.dir.join("new"), &f.dir.join(ORIG_NEW))?;
        remove_dir(&f.dir.join(AB_BASELINE))?;
        remove_dir(&f.dir.join("change"))?;
    }
    let outcome = run_bench(inp, base, "--save-baseline", &filter)
        .and_then(|()| run_bench(inp, head, "--baseline", &filter));
    for f in members {
        remove_dir(&f.dir.join("new"))?;
        std::fs::rename(f.dir.join(ORIG_NEW), f.dir.join("new"))
            .map_err(|e| format!("{}: {e}", f.dir.display()))?;
        remove_dir(&f.dir.join(AB_BASELINE))?;
    }
    outcome?;
    for f in members {
        if !f.dir.join("change").join("estimates.json").is_file() {
            return Err(format!("{}: the A/B wrote no change record", f.full_id));
        }
    }
    Ok(())
}

/// The A/B re-measure. Returns a one-line summary per benchmark.
fn recheck(inp: &Inputs) -> Result<Vec<String>, String> {
    let flagged = resolve(inp)?;
    let same_commit = sha_from_cache_key(&inp.baseline_key)
        .is_some_and(|sha| sha == inp.head_sha.trim().to_ascii_lowercase());
    let saved_bins = inp.criterion.join(SAVED_BINS);

    let mut groups: BTreeMap<&str, Vec<&Flagged>> = BTreeMap::new();
    for f in &flagged {
        groups.entry(f.exe.as_str()).or_default().push(f);
    }
    let mut lines = Vec::new();
    for (exe, members) in groups {
        let head = inp.root.join(exe);
        let name = head
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let target = bench_target(&name).ok_or_else(|| format!("{exe}: not a bench binary"))?;
        let (base, which) = match saved_bin_for(&saved_bins, target) {
            Some(saved) => (saved, "the baseline run's saved binary"),
            None if same_commit => (head.clone(), "the head binary (same commit)"),
            None => {
                return Err(format!(
                    "no saved baseline binary for {target}, and the baseline commit is not \
                     the head commit (cache key '{}')",
                    inp.baseline_key
                ));
            }
        };
        ab_pair(inp, &base, &head, &members)?;
        for f in members {
            lines.push(format!(
                "  A/B: {} re-measured on this runner, baseline from {which}, then head",
                f.full_id
            ));
        }
    }
    Ok(lines)
}

fn outcome(args: &[String]) -> (String, i32) {
    let [_, criterion, bench_log, gate_out] = args else {
        return (
            "usage: bench_ab_recheck <criterion-dir> <bench-output.log> <gate-output.log>\n"
                .to_string(),
            2,
        );
    };
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let inputs = (|| -> Result<Inputs, String> {
        let root = std::env::current_dir().map_err(|e| e.to_string())?;
        Ok(Inputs {
            criterion: root.join(criterion),
            bench_log: read(bench_log)?,
            gate_output: read(gate_out)?,
            baseline_key: std::env::var("BENCH_AB_BASELINE_KEY").unwrap_or_default(),
            head_sha: std::env::var("GITHUB_SHA").unwrap_or_default(),
            root,
        })
    })();
    match inputs.and_then(|inp| recheck(&inp)) {
        Ok(lines) => (
            format!(
                "\n  SAME-RUNNER A/B: {} flagged benchmark(s) re-measured; the gate runs again on the result.\n{}\n",
                lines.len(),
                lines.join("\n")
            ),
            0,
        ),
        Err(e) => (
            format!("\n  SAME-RUNNER A/B NOT RUN: {e}. The regression verdict stands.\n"),
            1,
        ),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (text, code) = outcome(&args);
    eprint!("{text}");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regression_keys_take_only_regression_failures() {
        let out = "  PASS: a: 3ns (budget: 250ns)\n  FAIL: big: 900ns (budget: 250ns)\n  \
                   FAIL: pipeline_batch_100_mixed: +54.4% regression (CI lower bound +43.5% > 5.0%)\n  \
                   FAIL: x: +9.0% regression (point estimate, no CI available; max: 5.0%)\n  \
                   FAIL: x: +9.0% regression (point estimate, no CI available; max: 5.0%)\n  \
                   DRIFT: y: +20.0% (uniform shift)\n  FAIL: nocolon\n";
        assert_eq!(regression_keys(out), vec!["pipeline_batch_100_mixed", "x"]);
        assert!(regression_keys("").is_empty());
    }

    #[test]
    fn sha_is_read_only_from_a_full_hex_tail() {
        let sha = "4189e98a8c0ffee0000000000000000000000abc";
        assert_eq!(sha.len(), 40);
        let key = format!("criterion-baseline-5cc632cfbd58-42311492058-{sha}");
        assert_eq!(sha_from_cache_key(&key).as_deref(), Some(sha));
        assert_eq!(
            sha_from_cache_key(&key.to_uppercase()).as_deref(),
            Some(sha)
        );
        assert_eq!(
            sha_from_cache_key("criterion-baseline-5cc632cfbd58-42311492058"),
            None
        );
        assert_eq!(sha_from_cache_key(""), None);
        assert_eq!(sha_from_cache_key(&format!("k-{}", "g".repeat(40))), None);
    }

    #[test]
    fn gate_key_matches_the_gate_awk() {
        assert_eq!(
            gate_key("pipeline/batch_100_mixed"),
            "pipeline_batch_100_mixed"
        );
        assert_eq!(gate_key("Seal_Path/Take_Submit"), "seal_path_take_submit");
        // The awk drops the FIRST "/change", wherever it is.
        assert_eq!(gate_key("x/changes"), "xs_change");
    }

    #[test]
    fn exe_is_found_from_the_running_line_above_the_benchmark() {
        let log = "\u{1b}[1m\u{1b}[92m     Running\u{1b}[0m benches/a.rs (target/release/deps/a-0123abcd)\n\
                   Benchmarking a/one\nBenchmarking a/one: Warming up for 3.0000 s\n\
                        Running benches/pipeline.rs (target/release/deps/pipeline-6d4df94fc326a801)\n\
                   Benchmarking pipeline/batch_100_mixed: Warming up for 3.0000 s\n";
        assert_eq!(
            exe_for_title(log, "pipeline/batch_100_mixed").as_deref(),
            Some("target/release/deps/pipeline-6d4df94fc326a801")
        );
        assert_eq!(
            exe_for_title(log, "a/one").as_deref(),
            Some("target/release/deps/a-0123abcd")
        );
        // A prefix of another id is not that id.
        assert_eq!(exe_for_title(log, "a/on"), None);
        assert_eq!(exe_for_title(log, "missing"), None);
        // Before any Running line there is no binary.
        assert_eq!(exe_for_title("Benchmarking z\n", "z"), None);
    }

    #[test]
    fn strip_ansi_removes_colour_codes() {
        assert_eq!(strip_ansi("\u{1b}[1m\u{1b}[92mRun\u{1b}[0mning"), "Running");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn filter_matches_ids_exactly() {
        assert_eq!(
            exact_filter(&["a/b-1".into(), "c.d".into()]),
            r"^(?:a/b\-1|c\.d)$"
        );
    }

    #[test]
    fn bench_target_accepts_only_cargo_bench_names() {
        assert_eq!(bench_target("pipeline-6d4df94fc326a801"), Some("pipeline"));
        assert_eq!(
            bench_target("candle_fold_cold-a0257bdc61a29ac3"),
            Some("candle_fold_cold")
        );
        assert_eq!(bench_target("plain"), None);
        assert_eq!(bench_target("a-zz"), None);
        assert_eq!(bench_target("-6d4df94fc326a801"), None);
        assert_eq!(bench_target("x.sh-6d4df94fc326a801"), None);
        assert_eq!(bench_target("pipeline-6d4df94fc326a801.d"), None);
    }

    #[test]
    fn only_bench_binaries_in_the_two_places_may_run() {
        let saved = Path::new("/nonexistent/criterion/tv_ab_bins");
        // Right shape but missing: refused.
        assert!(checked_bench_exe(&saved.join("pipeline-6d4df94fc326a801"), saved).is_err());
        assert!(
            checked_bench_exe(
                Path::new("target/release/deps/pipeline-6d4df94fc326a801"),
                saved
            )
            .is_err()
        );
        // Wrong place or wrong name: refused before the file is even looked at.
        assert!(checked_bench_exe(Path::new("/usr/bin/env"), saved).is_err());
        assert!(checked_bench_exe(Path::new("/tmp/pipeline-6d4df94fc326a801"), saved).is_err());
    }

    #[test]
    fn usage_without_three_arguments() {
        let (text, code) = outcome(&["bench_ab_recheck".to_string()]);
        assert_eq!(code, 2);
        assert!(text.contains("usage:"));
    }

    #[test]
    fn remove_dir_is_quiet_on_a_missing_dir() {
        assert!(remove_dir(Path::new("/nonexistent/tv-bench-ab-test")).is_ok());
    }
}
