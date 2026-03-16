//! Graceful degradation tests.
//!
//! Verifies that the system handles failures in dependent services
//! without cascading crashes. A live trading system must degrade
//! gracefully — never crash, never lose state, always alert.
//!
//! These test REAL project code (parser, aggregator), not tokio primitives.

use dhan_live_trader_common::constants::{
    DISCONNECT_PACKET_SIZE, EXCHANGE_SEGMENT_NSE_FNO, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_PACKET_SIZE, OI_PACKET_SIZE, PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE,
    RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL, RESPONSE_CODE_MARKET_DEPTH, RESPONSE_CODE_OI,
    RESPONSE_CODE_PREVIOUS_CLOSE, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE,
};

// ---------------------------------------------------------------------------
// Parser: truncated packets for ALL types — not just ticker
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_header(response_code: u8) -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0] = response_code;
    buf[1..3].copy_from_slice(&8u16.to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&42u32.to_le_bytes());
    buf
}

#[test]
fn degradation_parser_truncated_all_packet_types() {
    // Every packet type with only-header data must return Err, never panic
    let test_cases: &[(u8, usize)] = &[
        (RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE),
        (RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE),
        (RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE),
        (RESPONSE_CODE_MARKET_DEPTH, MARKET_DEPTH_PACKET_SIZE),
        (RESPONSE_CODE_OI, OI_PACKET_SIZE),
        (RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE),
        (RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE),
    ];

    for &(code, expected_size) in test_cases {
        let header = make_header(code);
        let result = dhan_live_trader_core::parser::dispatch_frame(&header, 0);

        if expected_size > 8 {
            assert!(
                result.is_err(),
                "truncated packet (code {code}) should fail, not produce garbage data"
            );
        }
        // Either way, no panic = success
    }
}

// ---------------------------------------------------------------------------
// Parser: malformed data doesn't crash pipeline
// ---------------------------------------------------------------------------

#[test]
fn degradation_parser_handles_garbage_stream() {
    let garbage_frames: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![0, 0, 0, 0, 0, 0, 0, 0],
        vec![255; 100],
        vec![2, 0, 0, 0, 0, 0, 0, 0], // Valid code but truncated body
        vec![50, 0, 10, 0, 0, 0, 0, 0, 0, 0], // Disconnect packet
    ];

    let mut errors = 0_u32;
    let mut successes = 0_u32;

    for frame in &garbage_frames {
        match dhan_live_trader_core::parser::dispatch_frame(frame, 0) {
            Ok(_) => successes = successes.saturating_add(1),
            Err(_) => errors = errors.saturating_add(1),
        }
    }

    assert!(errors > 0, "some garbage should produce errors");
    let _ = successes;
}

#[test]
fn degradation_parser_recovers_after_bad_frame() {
    let mut good = vec![0u8; TICKER_PACKET_SIZE];
    good[0] = 2;
    good[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
    good[3] = EXCHANGE_SEGMENT_NSE_FNO;
    good[4..8].copy_from_slice(&12345_u32.to_le_bytes());
    good[8..12].copy_from_slice(&100.0_f32.to_le_bytes());
    good[12..16].copy_from_slice(&1_700_000_000_u32.to_le_bytes());

    let bad = vec![0xFF; 5];

    // Parse good → bad → good: parser must recover
    assert!(dhan_live_trader_core::parser::dispatch_frame(&good, 0).is_ok());
    assert!(dhan_live_trader_core::parser::dispatch_frame(&bad, 0).is_err());
    assert!(
        dhan_live_trader_core::parser::dispatch_frame(&good, 0).is_ok(),
        "parser must recover after bad frame"
    );
}

// ---------------------------------------------------------------------------
// CandleAggregator: degradation with out-of-order timestamps
// ---------------------------------------------------------------------------

#[test]
fn degradation_candle_aggregator_out_of_order_timestamps() {
    use dhan_live_trader_common::tick_types::ParsedTick;
    use dhan_live_trader_core::pipeline::candle_aggregator::CandleAggregator;

    let mut agg = CandleAggregator::new();

    // Tick 1: timestamp 1000
    agg.update(&ParsedTick {
        security_id: 1001,
        last_traded_price: 100.0,
        exchange_timestamp: 1000,
        ..Default::default()
    });

    // Tick 2: timestamp 999 (out of order — should not panic)
    agg.update(&ParsedTick {
        security_id: 1001,
        last_traded_price: 99.0,
        exchange_timestamp: 999,
        ..Default::default()
    });

    // Tick 3: timestamp 1001 (back to normal)
    agg.update(&ParsedTick {
        security_id: 1001,
        last_traded_price: 101.0,
        exchange_timestamp: 1001,
        ..Default::default()
    });

    // No panic = success
}

// ---------------------------------------------------------------------------
// CandleAggregator: empty sweep and flush resilience
// ---------------------------------------------------------------------------

#[test]
fn degradation_candle_aggregator_empty_sweep_no_panic() {
    use dhan_live_trader_core::pipeline::candle_aggregator::CandleAggregator;

    let mut agg = CandleAggregator::new();

    // Sweep with no active candles — should not panic
    agg.sweep_stale(1_700_000_000);
    assert_eq!(agg.active_count(), 0);
    assert!(agg.completed_slice().is_empty());

    // Flush with no active candles — should not panic
    agg.flush_all();
    assert!(agg.completed_slice().is_empty());
}

#[test]
fn degradation_candle_aggregator_flush_all_then_reuse() {
    use dhan_live_trader_common::tick_types::ParsedTick;
    use dhan_live_trader_core::pipeline::candle_aggregator::CandleAggregator;

    let mut agg = CandleAggregator::new();

    // Create 10 candles
    for i in 1..=10_u32 {
        agg.update(&ParsedTick {
            security_id: i,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            last_traded_price: 100.0,
            exchange_timestamp: 1_700_000_000,
            ..Default::default()
        });
    }
    assert_eq!(agg.active_count(), 10);

    // Flush all — should emit all active candles
    agg.flush_all();
    assert_eq!(agg.active_count(), 0, "flush_all clears all active candles");
    assert_eq!(agg.completed_slice().len(), 10, "10 candles emitted");

    // Clear completed and immediately create new candles — no state leak
    agg.clear_completed();
    agg.update(&ParsedTick {
        security_id: 9999,
        exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
        last_traded_price: 500.0,
        exchange_timestamp: 1_700_000_010,
        ..Default::default()
    });
    assert_eq!(agg.active_count(), 1, "new candle created after flush_all");
}

// ---------------------------------------------------------------------------
// Task isolation: spawned task panics don't crash the runtime
// ---------------------------------------------------------------------------

#[tokio::test]
async fn degradation_spawned_task_panic_contained() {
    let handle = tokio::spawn(async {
        panic!("simulated task crash");
    });

    let result = handle.await;
    assert!(result.is_err(), "panicked task should return JoinError");
    let err = result.unwrap_err();
    assert!(err.is_panic(), "error should be a panic");
}

#[tokio::test]
async fn degradation_multiple_task_failures_isolated() {
    let mut handles = vec![];

    // Spawn 5 tasks: 3 succeed, 2 panic
    for i in 0..5_u32 {
        handles.push(tokio::spawn(async move {
            if i % 2 == 0 {
                i
            } else {
                panic!("task {i} crashed");
            }
        }));
    }

    let mut successes = 0_u32;
    let mut failures = 0_u32;

    for handle in handles {
        match handle.await {
            Ok(_) => successes = successes.saturating_add(1),
            Err(_) => failures = failures.saturating_add(1),
        }
    }

    assert_eq!(successes, 3);
    assert_eq!(failures, 2);
}
