//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This WS-GAP-08 ratchet pinned that BOTH main.rs boot arms called
//! `wait_out_persisted_ws_rate_limit_cooldown().await` BEFORE their
//! `create_websocket_pool(` sites — breaking the 429 → 300s → exit →
//! restart → instant-reconnect-into-the-429-window infinite loop. The
//! cooldown module (`crates/core/src/websocket/rate_limit_cooldown.rs`),
//! the pool, and both boot arms were ALL DELETED with the Dhan live-WS
//! lane, so the loop is structurally impossible: no boot path opens a
//! Dhan market-data WS at all. The order-update WS (functional-dormant in
//! dhan_rest_stack) keeps its own reconnect ladder + WS-GAP-10 paging,
//! pinned by the core order_update_connection tests.

#[test]
fn ws_rate_limit_cooldown_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired. Re-introducing a Dhan market-data WS requires a
    // fresh dated operator quote in websocket-connection-scope-lock.md
    // FIRST (§D of the amendment).
}
