//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This D2a extraction guard pinned `start_dhan_lane` / `DhanLaneContext` /
//! `DhanLaneRunHandles` / `StartLaneError` and the
//! `if config.feeds.dhan_enabled` lane gate in main.rs — ALL deleted with
//! the Dhan live-WS lane (auth → daily-universe build → main-feed WS pool →
//! tick processor → order-update spawn → token-renewal). The retained Dhan
//! surface is `dhan_rest_stack::spawn_dhan_rest_stack` (REST-only stack +
//! the functional-dormant order-update WS), whose boot shape is pinned by
//! `per_feed_boot_isolation_guard.rs` and the re-pointed instance-lock /
//! token-health guards in `crates/core/src/auth/secret_manager.rs`.
//! The 2-WS-lock leg this guard carried is superseded by
//! `crates/core/tests/dhan_live_ws_retired_guard.rs` (no `api-feed.dhan.co`
//! connect path may exist in core production code).

#[test]
fn d2a_start_dhan_lane_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired. A future re-introduction of a Dhan market-data WS
    // requires a fresh dated operator quote in
    // websocket-connection-scope-lock.md FIRST (§D of the amendment).
}
