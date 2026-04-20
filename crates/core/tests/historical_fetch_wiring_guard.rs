//! Source-guard tests for historical-fetch idempotency wiring.
//!
//! These tests scan the source tree to ensure the decision logic
//! (`decide_fetch` + `write_marker`) is wired into `candle_fetcher.rs`
//! and that `main.rs` calls the public entry point. Regressions
//! (deletion of the wiring) fail the build, not just runtime.

use std::fs;
use std::path::Path;

const CANDLE_FETCHER: &str = "src/historical/candle_fetcher.rs";
const MAIN_RS: &str = "../app/src/main.rs";

fn read(path: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {}", p.display(), e))
}

#[test]
fn candle_fetcher_calls_decide_fetch() {
    let src = read(CANDLE_FETCHER);
    assert!(
        src.contains("decide_fetch("),
        "candle_fetcher.rs must call decide_fetch() to enforce idempotency"
    );
}

#[test]
fn candle_fetcher_calls_write_marker() {
    let src = read(CANDLE_FETCHER);
    assert!(
        src.contains("write_marker("),
        "candle_fetcher.rs must call write_marker() to record successful fetches"
    );
}

#[test]
fn candle_fetcher_calls_read_marker() {
    let src = read(CANDLE_FETCHER);
    assert!(
        src.contains("read_marker("),
        "candle_fetcher.rs must call read_marker() to consult the idempotency marker"
    );
}

#[test]
fn candle_fetcher_emits_decision_metric() {
    let src = read(CANDLE_FETCHER);
    assert!(
        src.contains("tv_historical_fetch_decisions_total"),
        "candle_fetcher.rs must emit tv_historical_fetch_decisions_total counter"
    );
}

#[test]
fn candle_fetcher_emits_marker_io_error_metrics() {
    let src = read(CANDLE_FETCHER);
    assert!(
        src.contains("tv_historical_fetch_marker_read_errors_total"),
        "candle_fetcher.rs must emit marker read-errors counter"
    );
    assert!(
        src.contains("tv_historical_fetch_marker_write_errors_total"),
        "candle_fetcher.rs must emit marker write-errors counter"
    );
}

#[test]
fn candle_fetcher_emits_last_success_age_gauge() {
    let src = read(CANDLE_FETCHER);
    assert!(
        src.contains("tv_historical_fetch_last_success_age_seconds"),
        "candle_fetcher.rs must emit tv_historical_fetch_last_success_age_seconds gauge"
    );
}

#[test]
fn main_rs_calls_fetch_historical_candles() {
    let src = read(MAIN_RS);
    assert!(
        src.contains("fetch_historical_candles("),
        "main.rs must call fetch_historical_candles entry point"
    );
}
