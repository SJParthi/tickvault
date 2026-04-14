//! Load and stress tests.
//!
//! Verifies that the system handles sustained throughput and burst traffic
//! without degradation, memory leaks, or panics. These tests simulate
//! real market conditions: ~25,000 instruments × 1-5 ticks/sec.
//!
//! NOTE: Timing assertions are skipped under instrumented builds
//! (cargo-careful, sanitizers) where execution is 10-100x slower.
//! Set `TV_SKIP_PERF_ASSERTIONS=1` to skip timing checks.

use std::time::{Duration, Instant};

/// Returns true when running under instrumented builds (cargo-careful, sanitizers).
/// Timing assertions are unreliable under instrumentation overhead.
fn skip_perf_assertions() -> bool {
    std::env::var("TV_SKIP_PERF_ASSERTIONS").is_ok()
}

use tickvault_common::constants::{
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
        if tickvault_core::parser::dispatch_frame(&packet, 0).is_ok() {
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
        if tickvault_core::parser::dispatch_frame(&packet, 0).is_ok() {
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
        let result = tickvault_core::parser::dispatch_frame(&packet, i as i64);
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
    use tickvault_common::tick_types::ParsedTick;
    use tickvault_core::pipeline::candle_aggregator::CandleAggregator;

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

// ---------------------------------------------------------------------------
// Stress: parse 1 million ticker packets
// ---------------------------------------------------------------------------

#[test]
fn test_stress_parser_throughput_1m_packets() {
    let start = Instant::now();
    let mut success_count = 0_u64;

    for i in 0..1_000_000_u32 {
        let packet = make_ticker(
            50000 + (i % 25000),
            100.0 + (i as f32 * 0.001),
            1_700_000_000 + (i / 100),
        );
        if tickvault_core::parser::dispatch_frame(&packet, 0).is_ok() {
            success_count += 1;
        }
    }

    let elapsed = start.elapsed();
    assert_eq!(
        success_count, 1_000_000,
        "all 1M packets must parse successfully"
    );

    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_secs(10),
            "1M ticker parses took {elapsed:?} — too slow (budget: 10s)"
        );
    }
}

// ---------------------------------------------------------------------------
// Stress: concurrent registry access — 8 threads × 100K lookups
// ---------------------------------------------------------------------------

#[test]
fn test_stress_concurrent_registry_access() {
    use std::sync::Arc;
    use tickvault_common::instrument_registry::{
        InstrumentRegistry, SubscribedInstrument, SubscriptionCategory,
    };
    use tickvault_common::types::{ExchangeSegment, FeedMode};

    // Build a registry with 5000 instruments (max per connection).
    let instruments: Vec<SubscribedInstrument> = (0..5000_u32)
        .map(|i| SubscribedInstrument {
            security_id: 50000 + i,
            exchange_segment: ExchangeSegment::NseFno,
            category: SubscriptionCategory::StockDerivative,
            display_label: format!("INST_{i}"),
            underlying_symbol: "TEST".to_string(),
            instrument_kind: None,
            expiry_date: None,
            strike_price: None,
            option_type: None,
            feed_mode: FeedMode::Ticker,
        })
        .collect();

    let registry = Arc::new(InstrumentRegistry::from_instruments(instruments));
    let start = Instant::now();

    let handles: Vec<_> = (0..8)
        .map(|thread_idx| {
            let reg = Arc::clone(&registry);
            std::thread::spawn(move || {
                let mut found = 0_u64;
                for i in 0..100_000_u32 {
                    let security_id = 50000 + ((i + thread_idx * 12345) % 5000);
                    if reg.get(security_id).is_some() {
                        found += 1;
                    }
                }
                found
            })
        })
        .collect();

    let total_found: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let elapsed = start.elapsed();

    // All 800K lookups should succeed (all IDs are in range).
    assert_eq!(total_found, 800_000, "all lookups must succeed");

    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_secs(5),
            "800K concurrent registry lookups took {elapsed:?} — too slow (budget: 5s)"
        );
    }
}

// ---------------------------------------------------------------------------
// Stress: candle aggregator burst — 10K ticks in rapid succession
// ---------------------------------------------------------------------------

#[test]
fn test_stress_candle_aggregator_burst() {
    use tickvault_common::tick_types::ParsedTick;
    use tickvault_core::pipeline::candle_aggregator::CandleAggregator;

    let mut agg = CandleAggregator::new();
    let start = Instant::now();

    // Feed 10K ticks across 50 securities × 200 ticks each, spanning 20 seconds.
    for i in 0..10_000_u32 {
        let security_id = 50000 + (i % 50);
        let timestamp = 1_700_000_000 + (i / 500); // New second every 500 ticks
        let tick = ParsedTick {
            security_id,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            last_traded_price: 100.0 + (i as f32 * 0.01),
            exchange_timestamp: timestamp,
            volume: i * 10,
            ..Default::default()
        };
        agg.update(&tick);
    }

    let elapsed = start.elapsed();

    // Verify candle correctness: 10K ticks / 500 per second = 20 seconds.
    // 50 securities × (20-1) completed transitions = 950 completed candles.
    let completed_count = agg.total_completed();
    assert!(
        completed_count > 0,
        "should produce completed candles from burst traffic"
    );
    // Each security has 20 seconds, so 19 completed candles per security × 50 = 950.
    assert_eq!(
        completed_count, 950,
        "expected 950 completed candles (50 securities × 19 second transitions)"
    );
    // 50 securities still have active candles for the last second.
    assert_eq!(
        agg.active_count(),
        50,
        "50 securities should have active candles"
    );

    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_millis(500),
            "10K candle burst took {elapsed:?} — too slow (budget: 500ms)"
        );
    }
}

// ---------------------------------------------------------------------------
// Stress: subscription builder max load — 5000 instruments
// ---------------------------------------------------------------------------

#[test]
fn test_stress_subscription_builder_max_load() {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::build_subscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    // Build 5000 instruments (max per connection).
    let instruments: Vec<InstrumentSubscription> = (0..5000_u32)
        .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, 50000 + i))
        .collect();

    let start = Instant::now();
    let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
    let elapsed = start.elapsed();

    // 5000 / 100 = 50 messages.
    assert_eq!(
        messages.len(),
        50,
        "5000 instruments at batch 100 = 50 messages"
    );

    // Verify all messages are valid JSON.
    for (idx, msg) in messages.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(msg)
            .unwrap_or_else(|e| panic!("message {idx} is not valid JSON: {e}"));
        let instrument_count = parsed["InstrumentCount"]
            .as_u64()
            .unwrap_or_else(|| panic!("message {idx} missing InstrumentCount"));
        assert!(
            instrument_count <= 100,
            "message {idx} has {instrument_count} instruments (max 100)"
        );
        assert_eq!(
            parsed["RequestCode"].as_u64().unwrap(),
            21,
            "message {idx} must use Full mode RequestCode 21"
        );
    }

    // Total instruments across all messages should be 5000.
    let total_instruments: u64 = messages
        .iter()
        .map(|msg| {
            let parsed: serde_json::Value = serde_json::from_str(msg).unwrap();
            parsed["InstrumentCount"].as_u64().unwrap()
        })
        .sum();
    assert_eq!(
        total_instruments, 5000,
        "total instruments across all messages must be 5000"
    );

    if !skip_perf_assertions() {
        assert!(
            elapsed < Duration::from_secs(1),
            "building 50 subscription messages took {elapsed:?} — too slow (budget: 1s)"
        );
    }
}
