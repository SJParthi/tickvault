//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This AUTH-P12 / GAP-NET-01 ratchet pinned the runtime IP monitor
//! (`spawn_ip_monitor` + `IpMonitorConfig::for_runtime` + the dry_run halt
//! gate) being wired into the main.rs boot. The wiring was LANE-OWNED
//! (spawned from the deleted `start_dhan_lane` after the boot-time IP
//! verification) and died with the lane. The static-IP mandate it policed
//! protects the Dhan ORDER APIs (authentication.md rule 7) — with the
//! live-WS lane retired, no order placement live (`dry_run = true`,
//! OMS untouched), and the retained surface REST-pull-only, the monitor
//! is DORMANT: `crates/core/src/network/ip_monitor.rs` is retained
//! fully-unit-tested but un-spawned, pending the live-trading re-wire
//! into `dhan_rest_stack` (which owns the order-update WS + the future
//! order path) — tracked follow-up alongside the other retained-dormant
//! modules (fast_boot_validation.rs, wal_reinject.rs,
//! daily_universe_boot.rs). Re-wiring it MUST restore the
//! `for_runtime(verified_ip, dry_run)` halt gate this guard pinned.

#[test]
fn ip_monitor_wiring_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired and what the re-wire must restore.
}
