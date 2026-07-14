//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B; operator Q3: no Dhan instrument
//! download/parsing — hardcoded SIDs feed the retained REST pulls).
//!
//! This guard pinned the `InstrumentBuildFailed` boot-failure Telegram at
//! BOTH `load_instruments(...)` sites in main.rs (fast-boot +
//! standard-boot), the fail-closed `return Err(err)` halt, and the
//! `capture_rest_error_body` redaction on the reason field. Both sites —
//! and the instrument load itself — were DELETED with the lane: main.rs
//! performs no Dhan instrument fetch, so the silent-`?`-on-load-failure
//! regression class this guard closed is structurally gone. The
//! `InstrumentBuildFailed` / `InstrumentBuildSuccess` NotificationEvent
//! variants are DORMANT pending the Phase C cleanup/re-home (tracked
//! follow-up alongside EndOfDayDigest / MarketOpenReadinessConfirmation /
//! WebSocketPoolOnline).

#[test]
fn instrument_build_failed_wiring_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired.
}
