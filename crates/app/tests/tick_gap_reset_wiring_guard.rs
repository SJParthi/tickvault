//! Wave-2-D Fix 2 — `TickGapDetector::reset_daily()` source-scan guard.
//!
//! `crates/core/src/pipeline/tick_gap_detector.rs::reset_daily()` is
//! defined and unit-tested but had no production call site in main.rs
//! — a literal `audit-findings-2026-04-17.md` Rule 13 violation
//! ("if a method exists + is tested but is never called, it IS a
//! bug"). Wave-2-D wires a 15:35 IST scheduled task that calls
//! `reset_daily()` once per day. This guard fails the build if a
//! future change deletes the wiring — preventing silent regression
//! to the pre-Wave-2-D leaky behaviour.

use std::path::PathBuf;

const APP_MAIN_RS: &str = "src/main.rs";

fn read_main() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(APP_MAIN_RS);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

#[test]
fn tick_gap_detector_reset_daily_is_wired_in_main_rs() {
    let src = read_main();
    assert!(
        src.contains("reset_daily()"),
        "Wave-2-D Fix 2 regression: `reset_daily()` call MUST be \
         present in `crates/app/src/main.rs`. Without this 15:35 IST \
         task the papaya `last_seen` map accumulates entries \
         indefinitely across trading days — overnight silence will \
         register as a tick gap on next-day market open. Rule 13."
    );
}

#[test]
fn tick_gap_reset_uses_pinned_constant_not_literal() {
    let src = read_main();
    assert!(
        src.contains("TICK_GAP_RESET_TIME_IST"),
        "Wave-2-D Fix 2: the 15:35 IST reset time MUST come from \
         `tickvault_common::constants::TICK_GAP_RESET_TIME_IST`, not a \
         hard-coded literal. Hardcoded times silently break when \
         IST/DST rules change. See .claude/rules/project/rust-code.md \
         (No Hardcoded Values)."
    );
}

#[test]
fn tick_gap_reset_emits_prometheus_counter() {
    let src = read_main();
    assert!(
        src.contains("tv_tick_gap_daily_resets_total"),
        "Wave-2-D Fix 2: the daily reset task MUST emit \
         `tv_tick_gap_daily_resets_total` so the operator-health \
         dashboard can verify the task is firing once per trading \
         day. Without the counter, a silently-broken scheduler is \
         invisible. See .claude/rules/project/observability-architecture.md."
    );
}

#[test]
fn tick_gap_reset_log_mentions_15_35_ist_for_grepability() {
    let src = read_main();
    assert!(
        src.contains("15:35 IST")
            || src.contains("15:35:00")
            || src.contains("TICK_GAP_RESET_TIME_IST"),
        "Wave-2-D Fix 2: the reset task's `info!` log must mention \
         15:35 IST so operators can grep app logs for the daily reset \
         signal. The constant reference at the call site satisfies \
         this requirement."
    );
}
