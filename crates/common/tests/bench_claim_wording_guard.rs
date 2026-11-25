//! BENCH CLAIM WORDING GUARD (2026-10-12, deep audit TRU-2).
//!
//! `scripts/bench-gate.sh` compares Criterion's MEDIAN
//! (`.median.point_estimate`) against each budget, and `bench.yml` runs on
//! push to `main` and nightly, never on a pull request. No percentile is
//! measured anywhere in that gate. The rule and architecture documents said
//! "Criterion p99" in 26 places, so a change that makes one tick in a
//! hundred slow while keeping the median would pass every gate while the
//! documents claimed a p99 check.
//!
//! This guard keeps the wording and the gate together:
//!
//! 1. `docs_never_claim_a_criterion_p99` — in `.claude/rules/project/*.md`,
//!    `docs/architecture/*.md` and `docs/claude-rules-full/**/*.md` (archive
//!    folders skipped), a line that mentions `p99` together with Criterion or
//!    a bench must also say `median` or carry a `CORRECTED` note.
//! 2. `bench_gate_still_reads_the_median` — the gate script still reads the
//!    median, so a future real percentile gate must update this guard (and
//!    may then drop the notes) in the same PR.
//!
//! HONEST LIMIT: a line-level text check. A claim split across two lines, or
//! phrased without the word `p99`, is not seen.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("bench_claim_wording_guard: cannot canonicalize repo root")
}

/// True when this line claims a Criterion/bench percentile without saying
/// it is really a median (or carrying a correction note).
fn line_claims_unqualified_bench_p99(line: &str) -> bool {
    let low = line.to_ascii_lowercase();
    low.contains("p99")
        && (low.contains("criterion") || low.contains("bench"))
        && !low.contains("median")
        && !line.contains("CORRECTED")
}

fn collect_md(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if recursive && !name.contains("archive") {
                collect_md(&path, recursive, out);
            }
        } else if name.ends_with(".md") {
            out.push(path);
        }
    }
}

#[test]
fn docs_never_claim_a_criterion_p99() {
    let root = repo_root();
    let mut files = Vec::new();
    collect_md(&root.join(".claude/rules/project"), false, &mut files);
    collect_md(&root.join("docs/architecture"), false, &mut files);
    collect_md(&root.join("docs/claude-rules-full"), true, &mut files);
    assert!(
        files.len() > 30,
        "only {} markdown files scanned; the walk is broken",
        files.len()
    );
    let mut hits = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", f.display()));
        for (i, line) in text.lines().enumerate() {
            if line_claims_unqualified_bench_p99(line) {
                let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
                hits.push(format!("{rel}:{}", i + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "these lines claim a Criterion/bench p99, but `scripts/bench-gate.sh` gates the \
         MEDIAN, after merge and nightly only: {hits:?}. Say `median`, or add a dated \
         CORRECTED note, or build a real percentile gate and update this guard."
    );
}

#[test]
fn bench_gate_still_reads_the_median() {
    let gate = std::fs::read_to_string(repo_root().join("scripts/bench-gate.sh"))
        .expect("scripts/bench-gate.sh present");
    assert!(
        gate.contains(".median.point_estimate"),
        "bench-gate.sh no longer reads `.median.point_estimate`. If it now gates a \
         percentile, update bench_claim_wording_guard.rs and the CORRECTED notes."
    );
    let wf = std::fs::read_to_string(repo_root().join(".github/workflows/bench.yml"))
        .expect("bench.yml present");
    assert!(
        !wf.contains("pull_request"),
        "bench.yml now runs on pull requests; update the CORRECTED notes that say the \
         bench gate runs after merge and nightly only."
    );
}

#[test]
fn bench_claim_detector_bites() {
    assert!(line_claims_unqualified_bench_p99(
        "| perf | DHAT + Criterion p99 budgets |"
    ));
    assert!(line_claims_unqualified_bench_p99(
        "bench-gated <=100ns p99 per update"
    ));
    assert!(!line_claims_unqualified_bench_p99(
        "Criterion p99 (CORRECTED 2026-10-12: median)"
    ));
    assert!(!line_claims_unqualified_bench_p99(
        "| Stage | Latency (typical) | Latency (p99) |"
    ));
    assert!(!line_claims_unqualified_bench_p99(
        "Criterion median against the budget"
    ));
}
