//! Deterministic replay tests.
//!
//! Verifies that the same sequence of binary packets always produces
//! the same parsed output and the same dedup/aggregation results.
//! Essential for debugging production issues with recorded tick data.

use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_FNO, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
};
use tickvault_core::parser::dispatch_frame;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_ticker(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let mut buf = vec![0u8; TICKER_PACKET_SIZE];
    buf[0] = 2;
    buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[12..16].copy_from_slice(&ltt.to_le_bytes());
    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_quote(security_id: u32, ltp: f32) -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = 4;
    buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf
}

// ---------------------------------------------------------------------------
// Replay: Same sequence, same results
// ---------------------------------------------------------------------------

#[test]
fn replay_ticker_sequence_deterministic() {
    // Simulate a recorded market session: 10 ticks across 3 instruments
    let packets: Vec<Vec<u8>> = vec![
        make_ticker(52432, 245.50, 1_700_000_000),
        make_ticker(52433, 120.25, 1_700_000_000),
        make_ticker(52434, 89.75, 1_700_000_000),
        make_ticker(52432, 245.75, 1_700_000_001),
        make_ticker(52433, 120.50, 1_700_000_001),
        make_ticker(52432, 246.00, 1_700_000_001),
        make_ticker(52434, 90.00, 1_700_000_002),
        make_ticker(52432, 246.25, 1_700_000_002),
        make_ticker(52433, 120.75, 1_700_000_002),
        make_ticker(52432, 246.50, 1_700_000_003),
    ];

    // Run 1
    let results_1: Vec<String> = packets
        .iter()
        .map(|p| format!("{:?}", dispatch_frame(p, 0).unwrap()))
        .collect();

    // Run 2 — exact same input
    let results_2: Vec<String> = packets
        .iter()
        .map(|p| format!("{:?}", dispatch_frame(p, 0).unwrap()))
        .collect();

    // Must be identical
    assert_eq!(results_1, results_2, "replay must be deterministic");
}

#[test]
fn replay_mixed_packet_types_deterministic() {
    let packets: Vec<Vec<u8>> = vec![
        make_ticker(52432, 245.50, 1_700_000_000),
        make_quote(52432, 245.75),
        make_ticker(52433, 120.25, 1_700_000_001),
        make_quote(52433, 120.50),
    ];

    let run_1: Vec<String> = packets
        .iter()
        .map(|p| format!("{:?}", dispatch_frame(p, 42).unwrap()))
        .collect();

    let run_2: Vec<String> = packets
        .iter()
        .map(|p| format!("{:?}", dispatch_frame(p, 42).unwrap()))
        .collect();

    assert_eq!(run_1, run_2);
}

// ---------------------------------------------------------------------------
// Replay: Order of inputs matters
// ---------------------------------------------------------------------------

#[test]
fn replay_different_order_different_results() {
    let a = make_ticker(52432, 245.50, 1_700_000_000);
    let b = make_ticker(52433, 120.25, 1_700_000_001);

    let forward = vec![
        format!("{:?}", dispatch_frame(&a, 0).unwrap()),
        format!("{:?}", dispatch_frame(&b, 0).unwrap()),
    ];

    let reverse = vec![
        format!("{:?}", dispatch_frame(&b, 0).unwrap()),
        format!("{:?}", dispatch_frame(&a, 0).unwrap()),
    ];

    // Different input order = different output sequence
    assert_ne!(forward, reverse);
}

// ---------------------------------------------------------------------------
// Replay: Error sequences are deterministic too
// ---------------------------------------------------------------------------

#[test]
fn replay_error_sequence_deterministic() {
    let packets: Vec<Vec<u8>> = vec![
        vec![],                                    // too short
        vec![255, 0, 16, 2, 0, 0, 0, 1],           // unknown code
        make_ticker(52432, 245.50, 1_700_000_000), // valid
        vec![2, 0, 8],                             // truncated ticker
    ];

    let run_1: Vec<String> = packets
        .iter()
        .map(|p| format!("{:?}", dispatch_frame(p, 0)))
        .collect();

    let run_2: Vec<String> = packets
        .iter()
        .map(|p| format!("{:?}", dispatch_frame(p, 0)))
        .collect();

    assert_eq!(run_1, run_2, "error replay must also be deterministic");
}

// ---------------------------------------------------------------------------
// Replay: Candle aggregation deterministic
// ---------------------------------------------------------------------------

#[test]
fn replay_candle_aggregation_deterministic() {
    use tickvault_common::tick_types::ParsedTick;
    use tickvault_core::pipeline::candle_aggregator::CandleAggregator;

    let ticks = vec![
        ParsedTick {
            security_id: 52432,
            exchange_segment_code: 2,
            last_traded_price: 245.50,
            exchange_timestamp: 1_700_000_000,
            volume: 1000,
            ..Default::default()
        },
        ParsedTick {
            security_id: 52432,
            exchange_segment_code: 2,
            last_traded_price: 246.00,
            exchange_timestamp: 1_700_000_000,
            volume: 1500,
            ..Default::default()
        },
        ParsedTick {
            security_id: 52432,
            exchange_segment_code: 2,
            last_traded_price: 245.25,
            exchange_timestamp: 1_700_000_001, // New second → completes previous candle
            volume: 2000,
            ..Default::default()
        },
    ];

    // Run 1
    let mut agg1 = CandleAggregator::new();
    for tick in &ticks {
        agg1.update(tick);
    }
    let completed_1 = format!("{:?}", agg1.completed_slice());

    // Run 2
    let mut agg2 = CandleAggregator::new();
    for tick in &ticks {
        agg2.update(tick);
    }
    let completed_2 = format!("{:?}", agg2.completed_slice());

    // Both runs should produce identical results
    assert_eq!(
        completed_1, completed_2,
        "candle aggregation must be deterministic"
    );
    // Should have produced at least one completed candle
    assert!(
        !agg1.completed_slice().is_empty(),
        "should produce completed candles when second boundary crossed"
    );
}
