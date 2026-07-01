//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that BOTH the Dhan
//! and Groww daily tick-conservation audits are actually wired into boot at
//! 15:40:00 IST.
//!
//! Background: the Dhan conservation audit reconciles the WAL frame count vs
//! the processor outcome counters vs the persisted `feed='dhan'` ticks. The
//! Groww lane reconciles the sidecar NDJSON delivered-count vs the persisted
//! `feed='groww'` ticks — closing the audit-coverage gap where the Groww feed
//! wrote ticks but no daily conservation row. If either spawn is silently
//! removed from `main.rs`, the daily zero-tick-loss audit stops firing for that
//! feed with no other signal — this guard fails the build first.
//!
//! Mirrors the codebase's `*_is_wired` guard pattern
//! (`orphan_position_watchdog_wiring_guard.rs`,
//! `daily_universe_boot_wiring_guard.rs`). Reads `main.rs` SOURCE text, so it
//! runs on the default build independent of any feature flag.

use std::fs;
use std::path::PathBuf;

fn main_rs_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_dhan_tick_conservation_audit_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("run_tick_conservation_audit("),
        "main.rs must call run_tick_conservation_audit(...) at 15:40 IST — \
         otherwise the daily Dhan WAL-vs-DB zero-tick-loss audit never fires."
    );
}

#[test]
fn test_groww_tick_conservation_audit_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("run_groww_tick_conservation_audit("),
        "main.rs must call run_groww_tick_conservation_audit(...) at 15:40 IST — \
         otherwise the Groww feed writes ticks but no daily conservation row \
         (the audit-coverage gap this slice closes)."
    );
}

#[test]
fn test_groww_conservation_is_runtime_gated_and_shares_the_dhan_window() {
    // The Groww run must live AFTER the Dhan run (same 15:40 IST spawned task,
    // same target_ist_day/window) and be gated on the truthful runtime "is
    // Groww enabled" Arc so a Dhan-only session writes no misleading zero row.
    let src = main_rs_source();
    let dhan_idx = src
        .find("run_tick_conservation_audit(")
        .expect("Dhan conservation run must exist in main.rs");
    let groww_idx = src
        .find("run_groww_tick_conservation_audit(")
        .expect("Groww conservation run must exist in main.rs");
    assert!(
        groww_idx > dhan_idx,
        "the Groww conservation run must be sequenced AFTER the Dhan run inside \
         the same 15:40 IST task so both reconcile the same IST day."
    );
    assert!(
        src.contains("is_enabled(tickvault_common::feed::Feed::Groww)"),
        "the Groww conservation run must be gated on \
         feed_runtime.is_enabled(Feed::Groww) — a Dhan-only session must not \
         write a misleading zero-balanced Groww conservation row."
    );
}
