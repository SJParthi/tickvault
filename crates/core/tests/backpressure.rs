//! Backpressure tests for the tick processing pipeline.
//!
//! Tests that actual project components handle burst traffic correctly:
//! - Parser: 1000+ frames without panic or allocation issues
//! - CandleAggregator: burst from hundreds of securities with stale sweep
//! - Channel + Parser: real binary frames through bounded mpsc channels
//!
//! These test REAL project code, not tokio channel primitives.

use dhan_live_trader_common::constants::{
    EXCHANGE_SEGMENT_NSE_FNO, QUOTE_PACKET_SIZE, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER,
    TICKER_PACKET_SIZE,
};
use dhan_live_trader_common::tick_types::ParsedTick;
use dhan_live_trader_core::parser::dispatch_frame;
use dhan_live_trader_core::pipeline::candle_aggregator::CandleAggregator;

// ---------------------------------------------------------------------------
// Helpers — deterministic packet construction
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_ticker(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let mut buf = vec![0u8; TICKER_PACKET_SIZE];
    buf[0] = RESPONSE_CODE_TICKER;
    buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[12..16].copy_from_slice(&ltt.to_le_bytes());
    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_quote(security_id: u32, ltp: f32, volume: u32) -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = RESPONSE_CODE_QUOTE;
    buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[22..26].copy_from_slice(&volume.to_le_bytes());
    buf[34..38].copy_from_slice(&100.0_f32.to_le_bytes()); // day_open
    buf[42..46].copy_from_slice(&95.0_f32.to_le_bytes()); // day_high (placeholder)
    buf[46..50].copy_from_slice(&ltp.to_le_bytes()); // day_low
    buf
}

// ---------------------------------------------------------------------------
// Parser burst: 1000 ticker frames in rapid succession
// ---------------------------------------------------------------------------

#[test]
fn backpressure_parser_burst_1000_ticker_frames() {
    let base_time = 1_700_000_000_u32;
    let mut parsed_count = 0_u32;
    let mut error_count = 0_u32;

    for i in 0..1000_u32 {
        let sid = 50000_u32.wrapping_add(i % 200);
        let ltp = 245.50 + (i as f32 * 0.05);
        let packet = make_ticker(sid, ltp, base_time.wrapping_add(i));

        match dispatch_frame(&packet, i64::from(base_time).saturating_mul(1_000_000_000)) {
            Ok(_) => parsed_count = parsed_count.saturating_add(1),
            Err(_) => error_count = error_count.saturating_add(1),
        }
    }

    assert_eq!(
        parsed_count, 1000,
        "all 1000 ticker frames must parse successfully"
    );
    assert_eq!(error_count, 0, "zero errors on valid frames");
}

// ---------------------------------------------------------------------------
// Mixed packet burst: ticker + quote interleaved
// ---------------------------------------------------------------------------

#[test]
fn backpressure_parser_mixed_packet_types_burst() {
    let mut ticker_count = 0_u32;
    let mut quote_count = 0_u32;

    for i in 0..500_u32 {
        if i % 2 == 0 {
            let packet = make_ticker(
                10000_u32.wrapping_add(i),
                100.0,
                1_700_000_000_u32.wrapping_add(i),
            );
            if dispatch_frame(&packet, 0).is_ok() {
                ticker_count = ticker_count.saturating_add(1);
            }
        } else {
            let packet = make_quote(10000_u32.wrapping_add(i), 200.0, i.saturating_mul(1000));
            if dispatch_frame(&packet, 0).is_ok() {
                quote_count = quote_count.saturating_add(1);
            }
        }
    }

    assert_eq!(ticker_count, 250, "all ticker frames parsed");
    assert_eq!(quote_count, 250, "all quote frames parsed");
}

// ---------------------------------------------------------------------------
// CandleAggregator burst: 500 different securities ticking simultaneously
// ---------------------------------------------------------------------------

#[test]
fn backpressure_candle_aggregator_burst_500_securities() {
    let mut agg = CandleAggregator::new();

    // First tick for 500 securities — creates 500 live candles
    for sec_id in 1..=500_u32 {
        agg.update(&ParsedTick {
            security_id: sec_id,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            last_traded_price: 100.0 + (sec_id as f32 * 0.5),
            exchange_timestamp: 1_700_000_000,
            volume: sec_id.saturating_mul(100),
            ..Default::default()
        });
    }

    assert_eq!(agg.active_count(), 500, "all 500 securities tracked");

    // Second tick for all — updates existing candles (no new creates)
    for sec_id in 1..=500_u32 {
        agg.update(&ParsedTick {
            security_id: sec_id,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            last_traded_price: 101.0 + (sec_id as f32 * 0.5),
            exchange_timestamp: 1_700_000_000,
            volume: sec_id.saturating_mul(200),
            ..Default::default()
        });
    }

    assert_eq!(agg.active_count(), 500, "still 500 after updates");
}

// ---------------------------------------------------------------------------
// CandleAggregator sweep under load: stale candles flushed correctly
// ---------------------------------------------------------------------------

#[test]
fn backpressure_candle_sweep_stale_under_load() {
    let mut agg = CandleAggregator::new();
    let t = 1_700_000_000_u32;

    // Create candles at timestamp T for 100 securities
    for sec_id in 1..=100_u32 {
        agg.update(&ParsedTick {
            security_id: sec_id,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            last_traded_price: 100.0,
            exchange_timestamp: t,
            ..Default::default()
        });
    }
    assert_eq!(agg.active_count(), 100);

    // Sweep at T+10 — all candles are stale (>5s threshold)
    agg.sweep_stale(t.wrapping_add(10));
    assert_eq!(agg.active_count(), 0, "all candles swept as stale");
    assert_eq!(
        agg.completed_slice().len(),
        100,
        "100 completed candles emitted"
    );

    // Clear and verify buffer is reusable
    agg.clear_completed();
    assert!(agg.completed_slice().is_empty());

    // Immediately start new candles — verify no state leak
    agg.update(&ParsedTick {
        security_id: 9999,
        exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
        last_traded_price: 500.0,
        exchange_timestamp: t.wrapping_add(11),
        ..Default::default()
    });
    assert_eq!(agg.active_count(), 1, "new candle created after sweep");
}

// ---------------------------------------------------------------------------
// Channel + Parser: real binary frames through bounded mpsc
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backpressure_channel_with_real_binary_frames() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(256);

    // Producer: push 200 real ticker packets as Bytes
    for i in 0..200_u32 {
        let packet = make_ticker(
            50000_u32.wrapping_add(i),
            245.50,
            1_700_000_000_u32.wrapping_add(i),
        );
        tx.send(bytes::Bytes::from(packet)).await.unwrap();
    }
    drop(tx);

    // Consumer: drain and parse all frames (simulating tick_processor loop)
    let mut parsed = 0_u32;
    while let Some(frame) = rx.recv().await {
        if dispatch_frame(&frame, 0).is_ok() {
            parsed = parsed.saturating_add(1);
        }
    }

    assert_eq!(parsed, 200, "all frames parsed from channel");
}
