//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This 2026-04-24 ratchet pinned the market-hours gates on the Dhan pool
//! watchdog's Degraded/Halt arms (incl. the `std::process::exit(2)` gate,
//! the WS-GAP-09 reconnect-in-place machinery, the runtime-lane no-exit
//! contract, and the outside-hours watchdog reset) plus the lane boot's
//! `BOOT TIMEOUT EXCEEDED` / `BootDeadlineMissed` deadline alert. ALL of
//! those sites died with the lane: the pool, `spawn_pool_watchdog_task`,
//! `WatchdogVerdict`, the lane boot-deadline instrumentation, and both
//! boot arms are DELETED — main.rs contains no pool watchdog and no
//! pool-halt process exit at all, so the 2026-04-24 post-market
//! false-page/exit cascade class is structurally impossible.
//!
//! Surviving neighbours keep their own guards: the order-update WS outage
//! paging is pinned by `crates/core/tests/ws_telegram_visibility_guard.rs`
//! + the WS-GAP-10 tests; the Groww feed's stall detection is the
//! feed-stall watchdog (feed-stall-watchdog-error-codes.md).

#[test]
fn post_market_pool_halt_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired.
}
