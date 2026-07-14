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

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md "2026-07-13
// Amendment" §B): `emit_call_site_for_summary_is_pinned` +
// `emit_call_site_for_aborted_is_pinned` pinned the main.rs spawn/emit sites
// of the 15:31 IST Dhan live-vs-historical 1m cross-verify, which is RETIRED
// with the Dhan live WS (no live side to compare —
// cross-verify-1m-error-codes.md retirement banner). The spawn died with the
// deleted `spawn_post_market_tasks` helper. The `cross_verify_1m_boot.rs`
// MODULE is retained un-consumed pending the Phase C parser relocation
// (`parse_intraday_1m_candles` → spot_1m_rest_boot; scope-lock amendment §B),
// so the module-internal summary-JSON pin below survives.

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
