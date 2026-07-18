//! Ratchet guard for `scripts/coverage-gate.sh` (P0 of the coverage-gaps plan).
//!
//! GAP #0 (`.claude/plans/research/coverage-gaps.md` §2): the gate used a
//! start-anchored `re.match(r'crates/([^/]+)/', ...)` while cargo-llvm-cov
//! emits ABSOLUTE file paths, so the per-crate aggregation was always empty
//! and the gate passed vacuously — the "100% minimum" policy was never
//! actually enforced on any CI run.
//!
//! These tests EXECUTE the real gate script against synthetic coverage JSON
//! fixtures and pin both halves of the fix:
//!   1. absolute paths are aggregated (per-crate rows are printed), and
//!   2. a report that matches zero crates FAILS (fail-closed, audit-findings
//!      Rule 11: no false-OK signals),
//!
//! plus a source-scan that the anchored-match bug cannot be reintroduced.

use std::path::PathBuf;
use std::process::Command;

/// Repo root = crates/storage/../../
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// Runs the gate against the given coverage-JSON content; returns (exit_code, stdout+stderr).
fn run_gate(fixture_name: &str, json: &str) -> (i32, String) {
    let fixture = std::env::temp_dir().join(fixture_name);
    std::fs::write(&fixture, json).unwrap();
    let out = Command::new("bash")
        .arg("scripts/coverage-gate.sh")
        .arg(&fixture)
        .current_dir(repo_root())
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&fixture);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), combined)
}

/// One llvm-cov-shaped file entry with an ABSOLUTE path (the CI runner shape).
fn file_entry(abs_path: &str, count: u64, covered: u64) -> String {
    format!(
        r#"{{"filename":"{abs_path}","summary":{{"lines":{{"count":{count},"covered":{covered}}}}}}}"#
    )
}

fn coverage_json(entries: &[String]) -> String {
    format!(r#"{{"data":[{{"files":[{}]}}]}}"#, entries.join(","))
}

/// The crates listed in `quality/crate-coverage-thresholds.toml` `[crates]`
/// (their listing is separately pinned by
/// `crates/common/tests/coverage_threshold_lockdown.rs::coverage_lockdown_required_crates_are_listed`).
const THRESHOLD_CRATES: [&str; 7] = [
    "common",
    "core",
    "trading",
    "storage",
    "api",
    "app",
    "tickvault-logs-mcp",
];

/// Fully-covered file entries for every threshold-listed crate EXCEPT the
/// ones in `except` (the test then appends its own crafted entry for those).
/// Needed since 2026-07-10: the gate's fail-closed CRATE-PRESENCE ASSERT
/// requires every thresholds-listed crate to appear in the report, so a
/// single-crate fixture would (correctly) hard-fail on the other five.
fn full_presence_entries(except: &[&str]) -> Vec<String> {
    THRESHOLD_CRATES
        .iter()
        .filter(|c| !except.contains(c))
        .map(|c| {
            file_entry(
                &format!("/home/runner/work/tickvault/tickvault/crates/{c}/src/lib.rs"),
                100,
                100,
            )
        })
        .collect()
}

/// Fix half 1: absolute paths MUST be aggregated — at least one per-crate row
/// is printed and a fully-covered fixture passes.
#[test]
fn coverage_gate_prints_rows_for_absolute_paths() {
    let mut entries = full_presence_entries(&["common", "storage"]);
    entries.push(file_entry(
        "/home/runner/work/tickvault/tickvault/crates/common/src/config.rs",
        100,
        100,
    ));
    entries.push(file_entry(
        "/home/runner/work/tickvault/tickvault/crates/storage/src/lib.rs",
        100,
        100,
    ));
    let json = coverage_json(&entries);
    let (code, out) = run_gate("tv_gate_abs_pass.json", &json);
    assert_eq!(
        code, 0,
        "fully-covered absolute-path fixture must pass:\n{out}"
    );
    assert!(
        out.contains("common") && out.contains("storage"),
        "gate must print per-crate rows for absolute paths (GAP #0 regression):\n{out}"
    );
    assert!(
        out.contains("PASS"),
        "gate must print PASS rows, not pass silently:\n{out}"
    );
}

/// Fix half 2: a report whose paths match NO crate must FAIL, never pass
/// vacuously (this was the exact production bug shape).
#[test]
fn coverage_gate_fails_closed_on_zero_matched_files() {
    let json = coverage_json(&[file_entry(
        "/home/runner/work/elsewhere/src/lib.rs",
        100,
        50,
    )]);
    let (code, out) = run_gate("tv_gate_zero_match.json", &json);
    assert_ne!(
        code, 0,
        "zero matched files MUST be a failure (fail-closed), got exit 0:\n{out}"
    );
    assert!(
        out.contains("refusing vacuous pass"),
        "fail-closed refusal must be explicit in output:\n{out}"
    );
}

/// A crate below its threshold must FAIL with a per-crate FAIL row — with
/// absolute paths (the CI shape, which the old gate silently ignored).
#[test]
fn coverage_gate_fails_below_threshold_crate() {
    let mut entries = full_presence_entries(&["storage"]);
    entries.push(file_entry(
        "/home/runner/work/tickvault/tickvault/crates/storage/src/lib.rs",
        100,
        50,
    ));
    let json = coverage_json(&entries);
    let (code, out) = run_gate("tv_gate_below_floor.json", &json);
    assert_ne!(code, 0, "50%% storage coverage must fail its floor:\n{out}");
    assert!(
        out.contains("FAIL") && out.contains("storage"),
        "below-threshold crate must produce an explicit FAIL row:\n{out}"
    );
}

/// Source-scan ratchet: the start-anchored match-on-`crates/` pattern must
/// never return to the gate script.
///
/// 2026-07-18 amendment (PR #1642 review round 1 / rust-only Phase 2a-2):
/// the gate's inline python was replaced by awk, so the python-era literal
/// `re.search(r'crates/` no longer exists and this pin went stale (the exact
/// CI failure on head e5586994). The INTENT is unchanged and re-pinned on
/// the awk shape: aggregation must use awk's `match()` (LEFTMOST match —
/// re.search semantics) on an UNANCHORED `crates/<name>/` regex, and a
/// start-anchored `/^crates\//` shape (GAP #0: vacuous pass on absolute
/// llvm-cov paths) stays banned in BOTH eras' spellings. The behavioral
/// tests above (absolute-path aggregation + fail-closed) still execute the
/// real script, so the intent is enforced by execution, not just by literal.
#[test]
fn coverage_gate_source_has_no_anchored_match() {
    let src = std::fs::read_to_string(repo_root().join("scripts/coverage-gate.sh")).unwrap();
    assert!(
        !src.contains("re.match(r'crates/"),
        "scripts/coverage-gate.sh reintroduced start-anchored re.match on crates/ \
         — this is GAP #0 (vacuous pass on absolute llvm-cov paths); use a \
         leftmost (unanchored) match"
    );
    assert!(
        !src.contains(r"match(fn, /^crates"),
        "scripts/coverage-gate.sh reintroduced a start-anchored awk match on \
         crates/ — this is GAP #0 (vacuous pass on absolute llvm-cov paths); \
         the aggregation regex must stay unanchored"
    );
    assert!(
        src.contains(r"match(fn, /crates\/[^\/]+\//)"),
        "scripts/coverage-gate.sh must aggregate crates via awk's leftmost \
         match() on the unanchored crates/<name>/ regex (the re.search \
         semantics of the pre-2026-07-18 python implementation)"
    );
    assert!(
        src.contains("refusing vacuous pass"),
        "scripts/coverage-gate.sh must keep the fail-closed empty-aggregation guard"
    );
    // 2026-07-10 pins: the crate-presence assert + the zero-thresholds
    // fail-closed check must never be stripped from the gate.
    assert!(
        src.contains("crate-presence assert FAILED"),
        "scripts/coverage-gate.sh must keep the fail-closed CRATE-PRESENCE \
         assert (a thresholds-listed crate absent from the report = hard fail)"
    );
    assert!(
        src.contains("parsed 0 entries"),
        "scripts/coverage-gate.sh must keep the zero-thresholds fail-closed \
         check (empty [crates] section = hard fail, never a vacuous pass)"
    );
}

/// 2026-07-02 fix: a crate EXACTLY AT its threshold must PASS — a ratchet
/// floor means "at least the threshold". The old gate computed the percentage
/// in binary floating point (`covered/count*100.0`), where a true ratio equal
/// to the threshold can evaluate a hair BELOW it (e.g. `0.995` is not exactly
/// representable), and printed it rounded to 1 decimal — producing the
/// contradictory `FAIL: common 99.5% (threshold: 99.5%)` seen on main. The
/// fixed gate compares exact rationals (`Fraction(covered*100, count)` vs the
/// decimal threshold string), so exact-equal ALWAYS passes and just-below
/// ALWAYS fails, at full precision.
#[test]
fn coverage_gate_exact_threshold_value_passes() {
    // storage floor is 90.1 (91.2 -> 90.1 on 2026-07-17, the stage-2
    // dead-WS-sweep truthful re-baseline — see the dated record in
    // quality/crate-coverage-thresholds.toml) → 901 covered of 1000 lines
    // is EXACTLY at it.
    let mut entries = full_presence_entries(&["storage"]);
    entries.push(file_entry(
        "/home/runner/work/tickvault/tickvault/crates/storage/src/lib.rs",
        1000,
        901,
    ));
    let json = coverage_json(&entries);
    let (code, out) = run_gate("tv_gate_exact_at_floor.json", &json);
    assert_eq!(
        code, 0,
        "coverage exactly AT the threshold must PASS (ratchet = at least):\n{out}"
    );
    assert!(
        out.contains("PASS") && out.contains("storage"),
        "exact-threshold crate must produce an explicit PASS row:\n{out}"
    );
}

/// Companion boundary: one line BELOW the exact threshold must still FAIL —
/// the exact-rational comparison must not introduce any pass-side slack, and
/// the printed percentage now carries 2 decimals so a just-below value can no
/// longer be DISPLAYED as visually equal to its threshold.
#[test]
fn coverage_gate_one_line_below_threshold_fails_with_honest_display() {
    // 9009/10000 = 90.09% — strictly below the 90.1 storage floor
    // (91.2 -> 90.1 on 2026-07-17, the stage-2 dead-WS-sweep truthful
    // re-baseline — dated record in quality/crate-coverage-thresholds.toml).
    let mut entries = full_presence_entries(&["storage"]);
    entries.push(file_entry(
        "/home/runner/work/tickvault/tickvault/crates/storage/src/lib.rs",
        10000,
        9009,
    ));
    let json = coverage_json(&entries);
    let (code, out) = run_gate("tv_gate_just_below_floor.json", &json);
    assert_ne!(
        code, 0,
        "coverage strictly below the threshold must FAIL:\n{out}"
    );
    assert!(
        out.contains("90.09"),
        "the FAIL row must display the true 2-decimal value (90.09), never a \
         rounded 90.1 that looks equal to the threshold:\n{out}"
    );
}

/// 2026-07-10 CRATE-PRESENCE ASSERT ratchet (audit follow-up row 9, Rule 11
/// false-OK class): a thresholds-listed crate that is ABSENT from the
/// coverage report must HARD-FAIL the gate, NAMING the crate. Before this
/// assert, a vanished crate (renamed directory, llvm-cov filter change,
/// partial run) was simply never iterated and its floor passed vacuously.
#[test]
fn coverage_gate_fails_when_threshold_crate_missing_from_report() {
    // Every threshold crate EXCEPT `app` — `app` silently vanished.
    let json = coverage_json(&full_presence_entries(&["app"]));
    let (code, out) = run_gate("tv_gate_missing_crate.json", &json);
    assert_ne!(
        code, 0,
        "a thresholds-listed crate missing from the report MUST fail the gate \
         (vacuous-pass regression):\n{out}"
    );
    assert!(
        out.contains("MISSING: app"),
        "the missing crate must be NAMED in the failure output:\n{out}"
    );
    assert!(
        out.contains("crate-presence assert FAILED"),
        "the failure must be explicit about the presence assert:\n{out}"
    );
}

/// 2026-07-10 inverse direction: a crate present in the report but with NO
/// explicit floor in the thresholds TOML still PASSES (default floor is
/// enforced) but must emit a loud WARN naming it, so new crates get an
/// explicit ratcheted floor added.
#[test]
fn coverage_gate_warns_on_report_crate_without_explicit_floor() {
    let mut entries = full_presence_entries(&[]);
    entries.push(file_entry(
        "/home/runner/work/tickvault/tickvault/crates/newcrate/src/lib.rs",
        100,
        100,
    ));
    let json = coverage_json(&entries);
    let (code, out) = run_gate("tv_gate_unlisted_crate.json", &json);
    assert_eq!(
        code, 0,
        "an unlisted-but-fully-covered crate must not fail the gate:\n{out}"
    );
    assert!(
        out.contains("WARN: newcrate"),
        "an unlisted crate must be NAMED in a WARN so it gets a floor:\n{out}"
    );
}
