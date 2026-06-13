//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the Phase 0
//! Item 20 orphan-position 15:25 IST watchdog is actually wired into boot.
//!
//! Background: the pure evaluator + clock helpers existed in
//! `tickvault_trading::orphan_position_watchdog` for months, and
//! `phase-0-item-20-error-codes.md` CLAIMED a guard
//! `test_orphan_position_watchdog_is_wired_into_main` pinned the spawn — but
//! no such guard (and no spawn) existed. The watchdog was dead code: the
//! daily open-position safety gate never fired. This guard closes that gap:
//! it fails the build if the spawn is removed from `main.rs`.
//!
//! Mirrors the codebase's `*_is_wired` guard pattern
//! (`instrument_build_failed_wiring_guard.rs`, `daily_universe_boot_wiring_guard.rs`).
//! Reads `main.rs` SOURCE text, so it runs on the default build independent of
//! any feature flag.

use std::fs;
use std::path::PathBuf;

fn main_rs_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_orphan_position_watchdog_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("spawn_supervised_orphan_position_watchdog("),
        "main.rs must spawn the orphan-position watchdog via \
         spawn_supervised_orphan_position_watchdog(...) — otherwise the daily \
         15:25 IST open-position safety gate never fires (the dead-code gap \
         this slice fixes)."
    );
}

#[test]
fn test_orphan_position_watchdog_spawned_from_post_market_tasks() {
    // It must be spawned inside spawn_post_market_tasks, which is called from
    // BOTH boot paths (boot-symmetry) so a mid-session restart re-arms it.
    let src = main_rs_source();
    let fn_idx = src
        .find("fn spawn_post_market_tasks(")
        .expect("spawn_post_market_tasks must exist in main.rs");
    let spawn_idx = src
        .find("spawn_supervised_orphan_position_watchdog(")
        .expect("watchdog spawn must exist in main.rs");
    assert!(
        spawn_idx > fn_idx,
        "the watchdog spawn must live inside spawn_post_market_tasks (called \
         from both boot paths) so a fast-boot mid-session restart re-arms the \
         15:25 IST gate."
    );
}

#[test]
fn test_post_market_tasks_called_from_both_boot_paths() {
    // Boot-symmetry: spawn_post_market_tasks must be invoked at >= 2 call
    // sites (fast-boot + standard-boot). Definition + calls = >= 3 mentions.
    let src = main_rs_source();
    let mentions = src.matches("spawn_post_market_tasks(").count();
    assert!(
        mentions >= 3,
        "spawn_post_market_tasks must be defined once and called from both \
         boot paths (>= 3 textual mentions); found {mentions}."
    );
}
