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

//! ## RE-BLESSED 2026-08-11 — the §D precondition is now SATISFIED
//!
//! The operator authorized the Dhan 16-connection live main-feed revival on
//! 2026-08-09 (websocket-connection-scope-lock.md, "DHAN LIVE MAIN-FEED WS
//! REVIVAL AUTHORIZED" + the same-day second quote), approved 2026-08-11 —
//! i.e. the "fresh dated operator quote FIRST" this tombstone demanded has
//! arrived.
//!
//! **This suite is left as a tombstone rather than re-armed, deliberately.**
//! It carries ZERO assertions (verified 2026-08-11), so it cannot block the
//! revival and there is nothing here to defuse. Re-arming it would mean
//! resurrecting pins for `start_dhan_lane` / `DhanLaneContext` /
//! `DhanLaneRunHandles` / `StartLaneError` — symbols the revival is NOT
//! bringing back. The lane returns under a different shape
//! (`crates/app/src/dhan_feed_stack.rs::spawn_dhan_feed_stack`, gated by
//! `feed_stack_gate`), so pinning the old D2a symbol names would assert a
//! structure that no longer exists and fail for the wrong reason.
//!
//! **Where the equivalent coverage now lives:** the boot-shape protection
//! this suite used to provide is carried, for the revived lane, by
//! `crates/app/tests/dhan_feed_wiring_guard.rs`
//! (`dhan_feed_stack_gate_still_defaults_off`), which pins
//! `spawn_dhan_feed_stack`'s DOUBLE gate — config `dhan_enabled` AND the
//! `DHAN_LIVE_FEED_ENV_ON` env opt-in — so a single-gate regression cannot
//! let a config edit alone put 16 sockets live. Verified present in tree
//! 2026-08-11; that guard, not this tombstone, is the file to extend when
//! the revival's boot shape changes.

#[test]
fn d2a_start_dhan_lane_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired, and why the 2026-08-09 revival does NOT re-arm it
    // (the lane returns under a different shape — see the re-blessing note).
}
