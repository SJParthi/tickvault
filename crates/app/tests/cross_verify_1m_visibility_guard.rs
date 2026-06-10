//! Source-scan ratchet — 15:31 cross-verify VISIBILITY (2026-06-10).
//!
//! The post-market 1-minute cross-verify ran perfectly on 2026-06-10
//! (CSV written 15:33, zero mismatches) and told NO ONE: the
//! `CrossVerify1mSummary` returned by `run_cross_verify_1m` was dropped
//! by the caller. These guards pin the visibility chain so a refactor
//! can never silently restore that state:
//!
//! 1. `main.rs` emits `NotificationEvent::CrossVerify1mSummary` from the
//!    outer supervisor task (the daily Telegram deliverable).
//! 2. `main.rs` emits `NotificationEvent::CrossVerify1mAborted` on a
//!    panicked task — gated on `is_panic()` so graceful shutdown
//!    cancellation never pages (pre-impl finding H2).
//! 3. `cross_verify_1m_boot.rs` writes the per-day summary JSON the
//!    `GET /api/debug/cross-verify/latest` endpoint + portal card read.

use std::path::PathBuf;

fn read_app_source(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("guard could not read {}: {e}", path.display()))
}

#[test]
fn emit_call_site_for_summary_is_pinned() {
    let main_rs = read_app_source("src/main.rs");
    assert!(
        main_rs.contains("NotificationEvent::CrossVerify1mSummary {"),
        "main.rs must emit the typed CrossVerify1mSummary Telegram event \
         after run_cross_verify_1m completes — the summary must never be \
         silently dropped again (visibility directive 2026-06-10)"
    );
    assert!(
        main_rs.contains("missing: summary.stats.missing_ours"),
        "the summary event must carry missing_ours — missing minutes are \
         missed live candles, the very signal the audit exists for"
    );
    assert!(
        main_rs.contains("degraded: summary.degraded"),
        "the summary event must carry the degraded flag (false-OK guard)"
    );
}

#[test]
fn emit_call_site_for_aborted_is_pinned() {
    let main_rs = read_app_source("src/main.rs");
    assert!(
        main_rs.contains("NotificationEvent::CrossVerify1mAborted {"),
        "main.rs must emit CrossVerify1mAborted when the cross-verify task \
         dies before producing the daily summary — absence of the summary \
         must be impossible to miss"
    );
    assert!(
        main_rs.contains("join_err.is_panic()"),
        "the Aborted emit must be gated on is_panic() — cancellation during \
         the 16:30 IST auto-stop is normal teardown and must NOT page \
         (pre-impl finding H2)"
    );
    assert!(
        main_rs.contains("cross_verify_1m: task cancelled during shutdown"),
        "graceful cancellation must keep its quiet log-only arm"
    );
}

#[test]
fn summary_json_write_is_pinned() {
    let boot_rs = read_app_source("src/cross_verify_1m_boot.rs");
    assert!(
        boot_rs.contains("write_summary_file(csv_dir, trading_date, &summary)"),
        "run_cross_verify_1m must write the per-day summary JSON — the \
         portal Cross-verify card + GET /api/debug/cross-verify/latest \
         read it"
    );
    assert!(
        boot_rs.contains(".summary.json"),
        "the summary artefact filename contract (.summary.json) is pinned — \
         the API endpoint's strict .csv suffix filter depends on it"
    );
}
