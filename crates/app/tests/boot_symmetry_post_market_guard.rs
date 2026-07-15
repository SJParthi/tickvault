//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This BOOT-SYMMETRY ratchet (2026-06-09) pinned `spawn_post_market_tasks`
//! being called from BOTH boot paths (fast crash-recovery + slow) and the
//! 15:31:30 IST end-of-day digest + 15:31:00 IST 1-minute cross-verify
//! living solely inside that helper. All three legs retired together:
//!
//! - `spawn_post_market_tasks` was DELETED with the Dhan lane (the Dhan
//!   REST surface now boots via `dhan_rest_stack::spawn_dhan_rest_stack`).
//! - The 15:31 Dhan live-vs-historical 1m cross-verify is RETIRED — with no
//!   Dhan live candles there is no live side to compare
//!   (cross-verify-1m-error-codes.md retirement banner).
//! - The `EndOfDayDigest` NotificationEvent variant is DORMANT pending its
//!   Phase C cleanup/re-home (tracked follow-up).
//! - The "both boot paths" symmetry class itself is structurally gone:
//!   main.rs has a SINGLE boot path since the fast crash-recovery arm was
//!   deleted with the lane.
//!
//! The surviving post-close schedulers (spot-1m sweep, option-chain probe,
//! Groww legs, scoreboard, TF-verifier) each carry their own wiring guards
//! (`spot_1m_rest_wiring_guard.rs`, `option_chain_1m_wiring_guard.rs`,
//! `groww_spot_1m_wiring_guard.rs`, …).

#[test]
fn boot_symmetry_post_market_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired.
}
