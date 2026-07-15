//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This I12 ratchet (2026-04-21) pinned the lane boot's pre-market HALT
//! branch embedding auto-fetched `/v2/profile` + `/v2/ip/getIP`
//! diagnostics (via `build_pre_market_diagnostics`) in the CRITICAL
//! Telegram. The HALT branch — and the boot-blocking pre-market gate it
//! diagnosed — DIED with the lane: there is no market-data boot to halt,
//! so the "operator under 09:15 deadline pressure" incident class this
//! closed is structurally gone. What survives: the token-manager
//! `pre_market_check` itself (invoked every 900s by the mid-session
//! profile watchdog, which `dhan_rest_stack` spawns — pinned by the
//! re-pointed token_health guard in
//! `crates/core/src/auth/secret_manager.rs`), and the REST canary's
//! secret-redacted body capture (dhan-rest-canary-error-codes.md) as the
//! "WHY did REST die" diagnostic surface.

#[test]
fn premarket_halt_auto_diagnostic_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired.
}
