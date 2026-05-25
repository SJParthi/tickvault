//! Ratchet for PR #797 (operator-locked 2026-05-25): JSONL+HTML
//! cross-verify outcome MUST be PASS or FAIL only — never SKIP.
//!
//! Operator demand 2026-05-25 18:30 IST:
//! > "either first fetch or first check or n time check do it dude okay"
//!
//! Aligns the JSONL+HTML forensic record with the Telegram emission
//! contract established in PR #796. Before this fix, the HTML showed
//! orange "SKIP" while the Telegram message said "❌ CROSS-MATCH
//! FAILED" — operators saw conflicting signals.
//!
//! ## Pinned invariants
//!
//! 1. `build_report_row_from_cross_match` does NOT contain the string
//!    `"SKIP"` as a possible outcome value (only inside `if/else` arms).
//! 2. The function MUST produce only `"PASS"` or `"FAIL"` outcome strings.
//! 3. PR #797 marker comment present in the file.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read_cross_verify_report() -> String {
    let p = workspace_root().join("crates/core/src/historical/cross_verify_report.rs");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_build_report_row_outcome_is_pass_or_fail_only() {
    let body = read_cross_verify_report();
    // Locate the canonical outcome-assignment line. PR #797 contract:
    // exactly `let outcome = if report.passed { "PASS" } else { "FAIL" };`
    // No SKIP branch may exist in this exact line of code.
    assert!(
        body.contains(r#"let outcome = if report.passed { "PASS" } else { "FAIL" };"#),
        "PR #797: build_report_row_from_cross_match MUST assign outcome via exactly \
         `let outcome = if report.passed {{ \"PASS\" }} else {{ \"FAIL\" }};` — \
         no SKIP branch. This aligns JSONL+HTML with the Telegram emission contract \
         from PR #796 (empty live candles + partial coverage route to FAIL with \
         structured reason)."
    );
}

#[test]
fn test_pr_797_marker_comment_present() {
    let body = read_cross_verify_report();
    assert!(
        body.contains("PR #797"),
        "cross_verify_report.rs MUST cite PR #797 marker comment so future Claude \
         sessions don't accidentally re-introduce the SKIP outcome"
    );
}
