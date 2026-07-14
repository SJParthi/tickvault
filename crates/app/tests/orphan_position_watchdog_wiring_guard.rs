//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the Phase 0
//! Item 20 orphan-position 15:25 IST watchdog is actually wired into boot.
//!
//! Background: the pure evaluator + clock helpers existed in
//! `tickvault_trading::orphan_position_watchdog` for months, and
//! `phase-0-item-20-error-codes.md` CLAIMED a guard pinned the spawn — but
//! no such guard (and no spawn) existed. The watchdog was dead code: the
//! daily open-position safety gate never fired. This guard closes that gap.
//!
//! PR-C2 re-home (2026-07-13, operator retirement directive per
//! websocket-connection-scope-lock.md "2026-07-13 Amendment"): the spawn
//! moved from the DELETED main.rs `spawn_post_market_tasks` seam (and its
//! both-boot-paths symmetry, which died with the fast arm) into
//! `dhan_rest_stack.rs` — the sole surviving Dhan surface, which owns the
//! token + REST base URL the watchdog's `GET /v2/positions` poll needs
//! (the trading-adjacent KEEP class per
//! no-rest-except-live-feed-2026-06-27.md §3). The single-boot-path stack
//! spawn covers every boot, so the old boot-symmetry pin is subsumed.

use std::fs;
use std::path::PathBuf;

fn stack_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_rest_stack.rs");
    let full = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // PRODUCTION region only — cut at the module's in-file test module so a
    // test literal can never satisfy the pins.
    match full.find("#[cfg(test)]") {
        Some(cut) => full[..cut].to_string(),
        None => full,
    }
}

#[test]
fn test_orphan_position_watchdog_is_wired_into_rest_stack() {
    let src = stack_src();
    assert!(
        src.contains("spawn_supervised_orphan_position_watchdog("),
        "dhan_rest_stack.rs must spawn the orphan-position watchdog via \
         spawn_supervised_orphan_position_watchdog(...) — otherwise the daily \
         15:25 IST open-position safety gate never fires (the dead-code gap \
         this guard exists to close; re-homed here in PR-C2, 2026-07-13)."
    );
    // The dry_run wording flag must come from config, not a hardcoded bool.
    assert!(
        src.contains("config.strategy.dry_run"),
        "the watchdog spawn must pass config.strategy.dry_run (the Telegram \
         wording gate) — never a hardcoded literal."
    );
}

#[test]
fn test_orphan_position_watchdog_spawn_follows_token_phase() {
    // The watchdog polls Dhan REST with the stack's token — the spawn must
    // sit AFTER the token handle is derived (source order).
    let src = stack_src();
    let token_idx = src
        .find("let token_handle = token_manager.token_handle();")
        .expect("the stack's token-handle derivation must exist");
    let spawn_idx = src
        .find("spawn_supervised_orphan_position_watchdog(")
        .expect("watchdog spawn must exist in dhan_rest_stack.rs");
    assert!(
        spawn_idx > token_idx,
        "the watchdog spawn must follow the token phase — it cannot poll \
         /v2/positions without the stack's token handle."
    );
}
