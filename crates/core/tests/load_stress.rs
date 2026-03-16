//! Load and stress tests.
//!
//! Verifies that the system handles sustained throughput and burst traffic
//! without degradation, memory leaks, or panics. These tests simulate
//! real market conditions: ~25,000 instruments × 1-5 ticks/sec.
//!
//! NOTE: Timing assertions are skipped under instrumented builds
//! (cargo-careful, sanitizers) where execution is 10-100x slower.
//! Set `DLT_SKIP_PERF_ASSERTIONS=1` to skip timing checks.

use std::time::{Duration, Instant};

/// Returns true when running under instrumented builds (cargo-careful, sanitizers).
/// Timing assertions are unreliable under instrumentation overhead.
fn skip_perf_assertions() -> bool {
    std::env::var("DLT_SKIP_PERF_ASSERTIONS").is_ok()
}

use dhan_live_trader_common::constants::{
    EXCHANGE_SEGMENT_NSE_FNO, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
};

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
// Sustained throughput: parse 100,000 packets
// ---------------------------------------------------------------------------

#[test]
fn stress_parse_100k_tickers() {
    let start = Instant::now();
    let mut success_count = 0_u64;

    for i in 0..100_000_u32 {
        let packet = make_ticker(
            50000 + (i % 25000),
            100.0 + (i as f32 * 0.01),
            1_700_000_000 + i,
        );
        if dhan_live_trader_core::parser::dispatch_frame(&packet, 0).is_ok() {
            success_count += 1;
        }
    }

    let elapsed = start.elapsed();
    assert_eq!(success_count, 100_000);

    // Sanity check: 100K parses should complete in under 1 second
    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_secs(1),
            "100K ticker parses took {elapsed:?} — too slow"
        );
    }
}

#[test]
fn stress_parse_100k_quotes() {
    let start = Instant::now();
    let mut success_count = 0_u64;

    for i in 0..100_000_u32 {
        let packet = make_quote(50000 + (i % 25000), 200.0 + (i as f32 * 0.01));
        if dhan_live_trader_core::parser::dispatch_frame(&packet, 0).is_ok() {
            success_count += 1;
        }
    }

    let elapsed = start.elapsed();
    assert_eq!(success_count, 100_000);
    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_secs(2),
            "100K quote parses took {elapsed:?} — too slow"
        );
    }
}

// ---------------------------------------------------------------------------
// Burst: 10,000 packets in tight loop (simulates market open spike)
// ---------------------------------------------------------------------------

#[test]
fn stress_burst_10k_mixed_packets() {
    let start = Instant::now();

    for i in 0..10_000_u32 {
        let packet = if i % 3 == 0 {
            make_quote(50000 + (i % 5000), 100.0 + (i as f32 * 0.1))
        } else {
            make_ticker(
                50000 + (i % 5000),
                100.0 + (i as f32 * 0.1),
                1_700_000_000 + i,
            )
        };
        let result = dhan_live_trader_core::parser::dispatch_frame(&packet, i as i64);
        assert!(result.is_ok(), "parse failed at iteration {i}");
    }

    let elapsed = start.elapsed();
    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_millis(500),
            "10K mixed burst took {elapsed:?} — too slow"
        );
    }
}

// ---------------------------------------------------------------------------
// Sustained: Candle aggregation under load
// ---------------------------------------------------------------------------

#[test]
fn stress_candle_aggregator_50k_ticks() {
    use dhan_live_trader_common::tick_types::ParsedTick;
    use dhan_live_trader_core::pipeline::candle_aggregator::CandleAggregator;

    let mut agg = CandleAggregator::new();
    let start = Instant::now();

    for i in 0..50_000_u32 {
        let tick = ParsedTick {
            security_id: 50000 + (i % 100),
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            last_traded_price: 100.0 + (i as f32 * 0.01),
            exchange_timestamp: 1_700_000_000 + (i / 100), // New second every 100 ticks
            volume: i,
            ..Default::default()
        };
        agg.update(&tick);
    }

    let elapsed = start.elapsed();
    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_secs(1),
            "50K candle updates took {elapsed:?} — too slow"
        );
    }
    // Should have produced some completed candles (new seconds)
    let total_completed = agg.completed_slice().len();
    assert!(total_completed > 0, "should produce completed candles");
}
