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
            security_id: 50000 + u64::from(i),
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
                    let security_id = 50000 + u64::from((i + thread_idx * 12345) % 5000);
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
// Stress: subscription builder max load — RETIRED (PR-C2, 2026-07-13).
// `build_subscription_messages` was deleted with the Dhan live main-feed WS
// lane (operator retirement directive — websocket-connection-scope-lock.md
// "2026-07-13 Amendment"); no surviving code path builds subscribe JSON.
// ---------------------------------------------------------------------------
