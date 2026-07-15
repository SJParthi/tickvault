//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This suite pinned `emit_websocket_connected_alerts` — the boot-path
//! Telegram parity helper for the Dhan main-feed WebSocket pool
//! (`WebSocketPoolOnline` aggregate + the 2026-05-09 no-per-connection-
//! duplicate ratchet). The helper, the pool, and BOTH boot arms it enforced
//! parity across (fast crash-recovery + slow) were DELETED with the lane:
//! main.rs has a single boot path and spawns no Dhan main-feed WS.
//!
//! The original regression class (2026-04-17: only 3 of 5 "WebSocket #N
//! connected" alerts on slow boot; zero on fast boot) is structurally
//! impossible now — there is no Dhan pool to announce. Groww's connect
//! visibility is the `ws_event_audit` Connected row + the
//! `FeedConnectedAwaitingTicks` Telegram (ws-event-audit-error-codes.md);
//! the order-update WS lifecycle is pinned by
//! `crates/core/tests/ws_event_audit_wiring_guard.rs`.

#[test]
fn boot_path_notify_parity_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired. A future re-introduction of a Dhan market-data WS
    // requires a fresh dated operator quote in
    // websocket-connection-scope-lock.md FIRST (§D of the amendment).
}
