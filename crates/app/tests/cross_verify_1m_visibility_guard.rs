//! RETIRED (PR-C3, 2026-07-14 — Dhan instrument-chain deletion, operator
//! retirement directive 2026-07-13 per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B; Q1/Q2: the Dhan live WS is retired, so the
//! 15:31 IST Dhan live-vs-historical 1m cross-verify has no live side to
//! compare — cross-verify-1m-error-codes.md retirement banner).
//!
//! History: this suite (2026-06-10) pinned the cross-verify VISIBILITY
//! chain — the main.rs Telegram emit sites (retired in PR-C2 with
//! `spawn_post_market_tasks`) and the module-internal per-day summary-JSON
//! write the `GET /api/debug/cross-verify/latest` endpoint reads. PR-C3
//! deleted `cross_verify_1m_boot.rs` itself (its parser was relocated to
//! `dhan_intraday_parse.rs` in C1; the day CSVs + `cross_verify_1m_audit`
//! table are retained as forensic records, and the debug endpoint still
//! serves the retained artifacts read-only).

#[test]
fn cross_verify_1m_visibility_suite_retired_with_dhan_live_ws() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired. Re-introducing a Dhan live-vs-historical candle
    // cross-verify requires a fresh dated operator quote in
    // websocket-connection-scope-lock.md FIRST (§D of the amendment).
}
