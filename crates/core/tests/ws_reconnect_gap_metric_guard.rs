//! Source-scan ratchet for the reconnect-gap metric (zero-tick-loss PR-3, G1).
//!
//! Dhan's binary packets carry NO sequence number, so the ticks dropped during
//! a sub-30s WebSocket reconnect are invisible to the 30s tick-gap detector.
//! PR-3 adds a Prometheus counter quantifying cumulative reconnect-gap seconds
//! so a CloudWatch rate-alarm can flag sustained/excessive reconnect churn.
//!
//! These tests fail the build if a future refactor drops the metric emission
//! from the reconnect site in `connection.rs`.

const CONNECTION_SRC: &str = include_str!("../src/websocket/connection.rs");

#[test]
fn reconnect_site_emits_gap_seconds_counter() {
    assert!(
        CONNECTION_SRC.contains("tv_ws_reconnect_gap_seconds_total"),
        "connection.rs must emit the `tv_ws_reconnect_gap_seconds_total` counter \
         at the reconnect site (G1 — reconnect-gap visibility). If you removed it, \
         the operator loses the only quantified signal for sub-30s reconnect churn."
    );
}

#[test]
fn reconnect_site_emits_reconnect_count_counter() {
    assert!(
        CONNECTION_SRC.contains("tv_ws_reconnect_total"),
        "connection.rs must emit the `tv_ws_reconnect_total` counter at the \
         reconnect site so the gap-seconds rate can be normalised per reconnect."
    );
}

#[test]
fn gap_counter_is_incremented_by_down_secs() {
    // The gap counter MUST be incremented by the measured `down_secs`, not a
    // constant — otherwise it cannot reflect actual reconnect-gap duration.
    let idx = CONNECTION_SRC
        .find("tv_ws_reconnect_gap_seconds_total")
        .expect("gap-seconds counter must be present");
    let window = &CONNECTION_SRC[idx..(idx + 160).min(CONNECTION_SRC.len())];
    assert!(
        window.contains(".increment(down_secs)"),
        "the `tv_ws_reconnect_gap_seconds_total` counter must be \
         `.increment(down_secs)` (the measured gap), not a constant. Found:\n{window}"
    );
}
