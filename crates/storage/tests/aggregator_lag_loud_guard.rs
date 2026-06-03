//! Ratchet (zero-tick-loss PR-8b, H2-lite): the candle aggregator's
//! tick-broadcast `Lagged` arm MUST be LOUD, not a silent counter.
//!
//! Before PR-8b the aggregator subscriber's `RecvError::Lagged(skipped)` arm
//! in `spawn_seal_writer_loop` only did
//! `metrics::counter!("tv_aggregator_tick_lag_total")` — a candle-data-loss
//! event (the derived candles for the lagged window under-count) with ZERO
//! operator signal. PR-8b upgraded it to `error!(code = AGGREGATOR-LAG-01)`
//! so it routes to Telegram + the `errors.jsonl` forensic sink (audit Rule 5).
//!
//! This guard fails the build if a future edit removes the loud emission and
//! lets the aggregator lag go silent again. (Tick loss/order are unaffected
//! either way — the `ticks` table is a separate lossless+ordered consumer;
//! this is purely about making the derived-candle under-count observable.)

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/storage") // APPROVED: test
}

fn read_main_rs() -> String {
    let p = workspace_root().join("crates/app/src/main.rs");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_aggregator_lag_arm_emits_typed_error_code() {
    let main_rs = read_main_rs();
    assert!(
        main_rs.contains("AggregatorLag01TickLagDropped"),
        "H2-lite ratchet: the candle aggregator's broadcast `Lagged` arm must \
         emit `error!(code = ErrorCode::AggregatorLag01TickLagDropped.code_str())` \
         so a >52s-stall lag (candle under-count) is LOUD, not a silent counter. \
         The dropped ticks remain safe + ordered in the `ticks` table; this guard \
         only pins that the candle under-count stays observable."
    );
}

#[test]
fn test_aggregator_lag_counter_still_present() {
    let main_rs = read_main_rs();
    // The metric must remain alongside the new error! (both, not either/or).
    assert!(
        main_rs.contains("tv_aggregator_tick_lag_total"),
        "H2-lite ratchet: the `tv_aggregator_tick_lag_total` counter must remain \
         in the aggregator `Lagged` arm (rate signal) alongside the AGGREGATOR-LAG-01 \
         error! log."
    );
}
