//! Ratchet for PR #795 (operator-locked 2026-05-25): cross-verify JSONL+HTML
//! files MUST be written for EVERY run outcome (PASS / FAIL / SKIP).
//!
//! Operator demand 2026-05-25 16:50 IST:
//! > "where is the html dude meanwhile how will you ensure that it didnt
//! >  even miss a single data also without any error dude"
//!
//! Root cause discovered: the JSONL+HTML write block was nested inside
//! the PASS/FAIL `else` branch in main.rs cross-match handler. SKIP
//! outcomes (no live data, partial coverage) left ZERO trace on disk —
//! operator had no way to confirm the scheduler actually ran.
//!
//! Fix: move the write block to the TOP of the cross-match handler,
//! immediately after `cross_match_historical_vs_live(...)` returns.
//! Every outcome now writes a JSONL row + regenerates the HTML.
//!
//! This source-scan ratchet pins the structural invariant:
//! - JSONL `append_jsonl_row` is called exactly ONCE per cross-match run
//! - HTML `write_html_report` is called exactly ONCE per cross-match run
//! - Both calls live OUTSIDE the SKIP/PASS/FAIL conditional ladder
//!
//! Failure mode this prevents: a future PR re-introducing the inner
//! write would either duplicate (2 rows per PASS/FAIL) or silently
//! skip (0 rows per SKIP) — both regressions.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read_main() -> String {
    let p = workspace_root().join("crates/app/src/main.rs");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_append_jsonl_row_called_exactly_once_in_main() {
    let body = read_main();
    let count = body
        .matches("cross_verify_report::append_jsonl_row")
        .count();
    assert_eq!(
        count, 1,
        "main.rs MUST call cross_verify_report::append_jsonl_row EXACTLY ONCE \
         per cross-match handler. Two calls = duplicate JSONL rows. Zero calls = \
         no forensic record. Found {count} call(s)."
    );
}

#[test]
fn test_write_html_report_called_exactly_once_in_main() {
    let body = read_main();
    let count = body
        .matches("cross_verify_report::write_html_report")
        .count();
    assert_eq!(
        count, 1,
        "main.rs MUST call cross_verify_report::write_html_report EXACTLY ONCE \
         per cross-match handler. Two calls = duplicate HTML overwrites. Zero \
         calls = no operator-readable report. Found {count} call(s)."
    );
}

#[test]
fn test_jsonl_html_write_lives_outside_pass_fail_branch() {
    let body = read_main();
    // The write block must appear BEFORE the `if !cross_match.live_candles_present`
    // SKIP gate so every outcome (including SKIP) writes the file.
    let write_pos = body
        .find("cross_verify_report::append_jsonl_row")
        .expect("append_jsonl_row call must exist");
    let skip_gate_pos = body
        .find("!cross_match.live_candles_present")
        .expect("SKIP-gate condition must exist");
    assert!(
        write_pos < skip_gate_pos,
        "main.rs append_jsonl_row call (byte {write_pos}) MUST appear BEFORE the \
         SKIP gate (byte {skip_gate_pos}). Otherwise SKIP outcomes leave zero \
         JSONL trace — operator cannot confirm the scheduler ran. PR #795 \
         invariant."
    );
}

#[test]
fn test_pr_795_marker_comment_present() {
    let body = read_main();
    assert!(
        body.contains("PR #795") && body.contains("ALWAYS write a forensic"),
        "main.rs MUST cite PR #795 + the 'ALWAYS write a forensic' rationale \
         comment so future Claude sessions don't accidentally re-nest the \
         write block inside the PASS/FAIL branch"
    );
}
