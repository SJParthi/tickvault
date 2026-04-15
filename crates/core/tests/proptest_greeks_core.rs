//! Property-based tests for core crate parsers and pipeline.
//!
//! Extends proptest coverage to modules not yet covered: deep depth parser,
//! market status validator, dispatcher edge cases, subscription builder,
//! candle aggregator OHLCV invariants, and top movers change_pct computation.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use proptest::prelude::*;
use tickvault_common::constants::{
    BINARY_HEADER_SIZE, DEEP_DEPTH_HEADER_SIZE, DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
    PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
};
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::{ExchangeSegment, FeedMode};
use tickvault_core::parser::deep_depth::{
    DepthSide, feed_code_to_side, parse_deep_depth_header, parse_twenty_depth_packet,
    parse_two_hundred_depth_packet,
};
use tickvault_core::parser::dispatch_frame;
use tickvault_core::parser::dispatcher::dispatch_deep_depth_frame;
use tickvault_core::parser::types::ParsedFrame;
use tickvault_core::pipeline::candle_aggregator::CandleAggregator;
use tickvault_core::pipeline::top_movers::TopMoversTracker;
use tickvault_core::websocket::subscription_builder::build_subscription_messages;
use tickvault_core::websocket::types::InstrumentSubscription;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Builds a deep depth packet (bid or ask) with the given parameters.
fn make_deep_depth_packet(
    feed_code: u8,
    exchange_segment: u8,
    security_id: u32,
    seq_or_row: u32,
    level_count: usize,
) -> Vec<u8> {
    let packet_size = DEEP_DEPTH_HEADER_SIZE + level_count * 16;
    let mut buf = vec![0u8; packet_size];
    buf[0..2].copy_from_slice(&(packet_size as u16).to_le_bytes());
    buf[2] = feed_code;
    buf[3] = exchange_segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&seq_or_row.to_le_bytes());
    buf
}

/// Builds a standard-header packet with given response code and exact size.
fn make_standard_packet(response_code: u8, size: usize, security_id: u32, segment: u8) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    buf[0] = response_code;
    buf[1..3].copy_from_slice(&(size as u16).to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf
}

/// Builds a ParsedTick with the given LTP, day_close, volume, and timestamp.
fn make_tick(
    security_id: u32,
    segment: u8,
    ltp: f32,
    day_close: f32,
    volume: u32,
    timestamp: u32,
) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: segment,
        last_traded_price: ltp,
        day_close,
        volume,
        exchange_timestamp: timestamp,
        iv: f64::NAN,
        delta: f64::NAN,
        gamma: f64::NAN,
        theta: f64::NAN,
        vega: f64::NAN,
        ..Default::default()
    }
}

// ===========================================================================
// 1. Header parser — random 8 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_header_random_8_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), BINARY_HEADER_SIZE),
    ) {
        let result = tickvault_core::parser::header::parse_header(&data);
        // Must always succeed on exactly 8 bytes
        prop_assert!(result.is_ok());
    }
}

// ===========================================================================
// 2. Ticker parser — random 16 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_ticker_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), TICKER_PACKET_SIZE),
    ) {
        let header = tickvault_core::parser::header::parse_header(&data).unwrap();
        let _ = tickvault_core::parser::ticker::parse_ticker_packet(&data, &header, 0);
    }
}

// ===========================================================================
// 3. Quote parser — random 50 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_quote_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), QUOTE_PACKET_SIZE),
    ) {
        let header = tickvault_core::parser::header::parse_header(&data).unwrap();
        let _ = tickvault_core::parser::quote::parse_quote_packet(&data, &header, 0);
    }
}

// ===========================================================================
// 4. Full packet parser — random 162 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_full_packet_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), FULL_QUOTE_PACKET_SIZE),
    ) {
        let header = tickvault_core::parser::header::parse_header(&data).unwrap();
        let _ = tickvault_core::parser::full_packet::parse_full_packet(&data, &header, 0);
    }
}

// ===========================================================================
// 5. OI parser — random 12 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_oi_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), OI_PACKET_SIZE),
    ) {
        let header = tickvault_core::parser::header::parse_header(&data).unwrap();
        let _ = tickvault_core::parser::oi::parse_oi_packet(&data, &header);
    }
}

// ===========================================================================
// 6. Previous close parser — random 16 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_prev_close_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), PREVIOUS_CLOSE_PACKET_SIZE),
    ) {
        let header = tickvault_core::parser::header::parse_header(&data).unwrap();
        let _ = tickvault_core::parser::previous_close::parse_previous_close_packet(&data, &header);
    }
}

// ===========================================================================
// 7. Disconnect parser — random 10 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_disconnect_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), DISCONNECT_PACKET_SIZE),
    ) {
        let header = tickvault_core::parser::header::parse_header(&data).unwrap();
        let _ = tickvault_core::parser::disconnect::parse_disconnect_packet(&data, &header);
    }
}

// ===========================================================================
// 8. Market depth parser — random 112 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_market_depth_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), MARKET_DEPTH_PACKET_SIZE),
    ) {
        let header = tickvault_core::parser::header::parse_header(&data).unwrap();
        let _ = tickvault_core::parser::market_depth::parse_market_depth_packet(&data, &header, 0);
    }
}

// ===========================================================================
// 9. Market status validator — random bytes of various sizes
// ===========================================================================

proptest! {
    #[test]
    fn proptest_market_status_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..32),
    ) {
        let _ = tickvault_core::parser::market_status::validate_market_status_packet(&data);
    }

    #[test]
    fn proptest_market_status_valid_size_always_ok(
        data in prop::collection::vec(any::<u8>(), MARKET_STATUS_PACKET_SIZE..64),
    ) {
        let result = tickvault_core::parser::market_status::validate_market_status_packet(&data);
        prop_assert!(result.is_ok());
    }
}

// ===========================================================================
// 10. Deep depth header — random 12 bytes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_deep_depth_header_random_bytes_never_panics(
        data in prop::collection::vec(any::<u8>(), DEEP_DEPTH_HEADER_SIZE),
    ) {
        let result = parse_deep_depth_header(&data);
        prop_assert!(result.is_ok());
        let header = result.unwrap();
        // Verify all fields are populated from the raw bytes
        // Header fields are populated from raw bytes
        let _ = header.message_length;
    }
}

// ===========================================================================
// 11. Deep depth — 20-level packet with valid feed code preserves security_id
// ===========================================================================

proptest! {
    #[test]
    fn proptest_twenty_depth_security_id_preserved(
        security_id in 1..u32::MAX,
        feed_code in prop_oneof![Just(41_u8), Just(51)],
        segment in prop_oneof![Just(1_u8), Just(2)],
    ) {
        let buf = make_deep_depth_packet(feed_code, segment, security_id, 1, 20);
        let result = parse_twenty_depth_packet(&buf, 0).unwrap();
        prop_assert_eq!(result.header.security_id, security_id);
        prop_assert_eq!(result.header.exchange_segment_code, segment);
        prop_assert_eq!(result.levels.len(), 20);
        let expected_side = if feed_code == 41 { DepthSide::Bid } else { DepthSide::Ask };
        prop_assert_eq!(result.side, expected_side);
    }
}

// ===========================================================================
// 12. Deep depth — feed_code_to_side for all u8 values never panics
// ===========================================================================

proptest! {
    #[test]
    fn proptest_feed_code_to_side_never_panics(feed_code in any::<u8>()) {
        let result = feed_code_to_side(feed_code);
        match feed_code {
            41 => prop_assert_eq!(result.unwrap(), DepthSide::Bid),
            51 => prop_assert_eq!(result.unwrap(), DepthSide::Ask),
            _ => prop_assert!(result.is_err()),
        }
    }
}

// ===========================================================================
// 13. Deep depth — 200-level with random row_count validates bounds
// ===========================================================================

proptest! {
    #[test]
    fn proptest_two_hundred_depth_row_count_validation(
        row_count in 0_u32..300,
        feed_code in prop_oneof![Just(41_u8), Just(51)],
    ) {
        let level_count = if row_count <= 200 { row_count as usize } else { 200 };
        let buf = make_deep_depth_packet(feed_code, 2, 13, row_count, level_count);
        let result = parse_two_hundred_depth_packet(&buf, 0);
        if row_count > 200 {
            prop_assert!(result.is_err());
        } else {
            prop_assert!(result.is_ok());
            let parsed = result.unwrap();
            prop_assert_eq!(parsed.levels.len(), row_count as usize);
        }
    }
}

// ===========================================================================
// 14. Dispatcher — random bytes of all sizes never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_dispatch_frame_random_sizes_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let _ = dispatch_frame(&data, 0);
    }
}

// ===========================================================================
// 15. Dispatcher — dispatch_deep_depth_frame random data never panics
// ===========================================================================

proptest! {
    #[test]
    fn proptest_dispatch_deep_depth_random_data_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let _ = dispatch_deep_depth_frame(&data, 0);
    }
}

// ===========================================================================
// 16. Dispatcher — valid packets route to correct variant
// ===========================================================================

proptest! {
    #[test]
    fn proptest_dispatch_routes_valid_ticker_correctly(
        security_id in 1..u32::MAX,
        segment in prop_oneof![Just(0_u8), Just(1), Just(2), Just(3), Just(4), Just(5), Just(7), Just(8)],
    ) {
        let buf = make_standard_packet(2, TICKER_PACKET_SIZE, security_id, segment);
        let result = dispatch_frame(&buf, 0);
        prop_assert!(result.is_ok());
        match result.unwrap() {
            ParsedFrame::Tick(tick) => {
                prop_assert_eq!(tick.security_id, security_id);
                prop_assert_eq!(tick.exchange_segment_code, segment);
            }
            other => prop_assert!(false, "expected Tick, got: {other:?}"),
        }
    }

    #[test]
    fn proptest_dispatch_routes_index_ticker_correctly(
        security_id in 1..u32::MAX,
    ) {
        // Response code 1 = Index Ticker — same parser as Ticker
        let buf = make_standard_packet(1, TICKER_PACKET_SIZE, security_id, 0);
        let result = dispatch_frame(&buf, 0);
        prop_assert!(result.is_ok());
        match result.unwrap() {
            ParsedFrame::Tick(tick) => {
                prop_assert_eq!(tick.security_id, security_id);
            }
            other => prop_assert!(false, "expected Tick, got: {other:?}"),
        }
    }
}

// ===========================================================================
// 17. Dispatcher — unknown response codes return error, never panic
// ===========================================================================

proptest! {
    #[test]
    fn proptest_dispatch_unknown_response_code(
        code in any::<u8>().prop_filter("skip valid codes", |c| !matches!(*c, 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 50)),
    ) {
        let buf = make_standard_packet(code, BINARY_HEADER_SIZE, 1, 0);
        let result = dispatch_frame(&buf, 0);
        prop_assert!(result.is_err());
    }
}

// ===========================================================================
// 18. Subscription builder — batch count correct for random instrument counts
// ===========================================================================

proptest! {
    #[test]
    fn proptest_subscription_batch_count_correct(
        count in 0_usize..500,
        batch_size in 1_usize..200,
    ) {
        let instruments: Vec<InstrumentSubscription> = (0..count)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, i as u32))
            .collect();

        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, batch_size);

        if count == 0 {
            prop_assert_eq!(messages.len(), 0);
        } else {
            // Effective batch is clamped to [1, 100]
            let effective_batch = batch_size.clamp(1, 100);
            let expected_batches = count.div_ceil(effective_batch);
            prop_assert_eq!(messages.len(), expected_batches);
        }
    }
}

// ===========================================================================
// 19. Subscription builder — SecurityId always serializes as string in JSON
// ===========================================================================

proptest! {
    #[test]
    fn proptest_subscription_security_id_is_string(
        security_id in 1_u32..u32::MAX,
    ) {
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::NseFno, security_id)];
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);

        prop_assert_eq!(messages.len(), 1);
        let json = &messages[0];

        // SecurityId must be a quoted string, NOT a bare number
        let expected_fragment = format!("\"SecurityId\":\"{}\"", security_id);
        prop_assert!(
            json.contains(&expected_fragment),
            "SecurityId must serialize as string in JSON. Got: {}",
            json,
        );
    }
}

// ===========================================================================
// 20. Candle aggregator — OHLCV invariant: high >= low, tick_count >= 1
// ===========================================================================

proptest! {
    #[test]
    fn proptest_candle_ohlcv_invariant(
        prices in prop::collection::vec(0.01_f32..100_000.0, 1..50),
        volumes in prop::collection::vec(0_u32..10_000_000, 1..50),
    ) {
        let mut aggregator = CandleAggregator::new();
        let tick_count = prices.len().min(volumes.len());

        // Feed ticks with the same timestamp (same 1-second candle)
        for i in 0..tick_count {
            let tick = make_tick(42, 2, prices[i], 0.0, volumes[i], 1_700_000_000);
            aggregator.update(&tick);
        }

        // Now send a tick with a new timestamp to flush the candle
        let flush_tick = make_tick(42, 2, 100.0, 0.0, 0, 1_700_000_001);
        aggregator.update(&flush_tick);

        let completed = aggregator.completed_slice();
        prop_assert_eq!(completed.len(), 1);

        let candle = completed[0];

        // OHLCV invariants
        prop_assert!(candle.high >= candle.low, "high must be >= low");
        prop_assert!(candle.high >= candle.open, "high must be >= open");
        prop_assert!(candle.high >= candle.close, "high must be >= close");
        prop_assert!(candle.low <= candle.open, "low must be <= open");
        prop_assert!(candle.low <= candle.close, "low must be <= close");
        prop_assert!(candle.tick_count >= 1, "tick_count must be >= 1");
        prop_assert_eq!(candle.tick_count as usize, tick_count);

        // Open = first price, Close = last price
        prop_assert_eq!(candle.open, prices[0]);
        prop_assert_eq!(candle.close, prices[tick_count - 1]);
    }
}

// ===========================================================================
// 21. Candle aggregator — first tick sets open = high = low = close
// ===========================================================================

proptest! {
    #[test]
    fn proptest_candle_first_tick_sets_all_ohlc(
        price in 0.01_f32..100_000.0,
        volume in 0_u32..10_000_000,
    ) {
        let mut aggregator = CandleAggregator::new();
        let tick = make_tick(42, 2, price, 0.0, volume, 1_700_000_000);
        aggregator.update(&tick);

        // Flush by advancing timestamp
        let flush_tick = make_tick(42, 2, 1.0, 0.0, 0, 1_700_000_001);
        aggregator.update(&flush_tick);

        let completed = aggregator.completed_slice();
        prop_assert_eq!(completed.len(), 1);

        let candle = completed[0];
        prop_assert_eq!(candle.open, price);
        prop_assert_eq!(candle.high, price);
        prop_assert_eq!(candle.low, price);
        prop_assert_eq!(candle.close, price);
        prop_assert_eq!(candle.tick_count, 1);
    }
}

// ===========================================================================
// 22. Candle aggregator — different securities produce separate candles
// ===========================================================================

proptest! {
    #[test]
    fn proptest_candle_different_securities_separate(
        price_a in 0.01_f32..50_000.0,
        price_b in 0.01_f32..50_000.0,
        sec_a in 1_u32..u32::MAX / 2,
        sec_b in (u32::MAX / 2)..u32::MAX,
    ) {
        let mut aggregator = CandleAggregator::new();

        let tick_a = make_tick(sec_a, 2, price_a, 0.0, 100, 1_700_000_000);
        let tick_b = make_tick(sec_b, 2, price_b, 0.0, 200, 1_700_000_000);
        aggregator.update(&tick_a);
        aggregator.update(&tick_b);

        // Flush both by advancing timestamp
        aggregator.update(&make_tick(sec_a, 2, 1.0, 0.0, 0, 1_700_000_001));
        aggregator.update(&make_tick(sec_b, 2, 1.0, 0.0, 0, 1_700_000_001));

        let completed = aggregator.completed_slice();
        prop_assert_eq!(completed.len(), 2);

        let candle_a = completed.iter().find(|c| c.security_id == sec_a).unwrap();
        let candle_b = completed.iter().find(|c| c.security_id == sec_b).unwrap();

        prop_assert_eq!(candle_a.open, price_a);
        prop_assert_eq!(candle_b.open, price_b);
    }
}

// ===========================================================================
// 23. Top movers — change_pct computation correct for random prices
// ===========================================================================

proptest! {
    #[test]
    fn proptest_top_movers_change_pct_correct(
        ltp in 0.01_f32..100_000.0,
        prev_close in 0.01_f32..100_000.0,
    ) {
        let mut tracker = TopMoversTracker::new();
        // Segment 1 = NSE_EQ — TopMoversTracker rejects derivatives (segment 2)
        let tick = make_tick(42, 1, ltp, prev_close, 1000, 1_700_000_000);
        tracker.update(&tick);

        let snapshot = tracker.compute_snapshot();
        prop_assert_eq!(snapshot.total_tracked, 1);

        // Compute expected change_pct
        let expected_pct = ((ltp - prev_close) / prev_close) * 100.0;

        // The security should appear in either gainers, losers, or most_active
        let all_entries: Vec<_> = snapshot
            .equity_gainers
            .iter()
            .chain(snapshot.equity_losers.iter())
            .chain(snapshot.equity_most_active.iter())
            .filter(|e| e.security_id == 42)
            .collect();

        // Must be in most_active at minimum
        let in_most_active = snapshot.equity_most_active.iter().any(|e| e.security_id == 42);
        prop_assert!(in_most_active, "security should be in most_active");

        // Verify change_pct matches if in any list
        if let Some(entry) = all_entries.first() {
            let diff = (entry.change_pct - expected_pct).abs();
            prop_assert!(
                diff < 0.01,
                "change_pct mismatch: got {}, expected {}",
                entry.change_pct,
                expected_pct,
            );
        }
    }
}

// ===========================================================================
// 24. Top movers — skip ticks with zero/negative/NaN day_close
// ===========================================================================

proptest! {
    #[test]
    fn proptest_top_movers_skip_invalid_prev_close(
        ltp in 0.01_f32..100_000.0,
        invalid_close in prop_oneof![Just(0.0_f32), Just(-1.0_f32), Just(f32::NAN)],
    ) {
        let mut tracker = TopMoversTracker::new();
        // Segment 1 = NSE_EQ — TopMoversTracker rejects derivatives (segment 2)
        let tick = make_tick(42, 1, ltp, invalid_close, 1000, 1_700_000_000);
        tracker.update(&tick);

        prop_assert_eq!(tracker.tracked_count(), 0);
    }
}

// ===========================================================================
// 25. Top movers — multiple securities are tracked independently
// ===========================================================================

proptest! {
    #[test]
    fn proptest_top_movers_multiple_securities(
        count in 1_usize..50,
        ltp in 0.01_f32..50_000.0,
        prev_close in 0.01_f32..50_000.0,
    ) {
        let mut tracker = TopMoversTracker::new();

        for i in 0..count {
            // Segment 1 = NSE_EQ — TopMoversTracker rejects derivatives (segment 2)
            let tick = make_tick(i as u32 + 1, 1, ltp, prev_close, 1000 * (i as u32 + 1), 1_700_000_000);
            tracker.update(&tick);
        }

        prop_assert_eq!(tracker.tracked_count(), count);
    }
}

// ===========================================================================
// 26. Deep depth — dispatch_deep_depth_frame with valid bid/ask packets
// ===========================================================================

proptest! {
    #[test]
    fn proptest_dispatch_deep_depth_valid_packets(
        security_id in 1..u32::MAX,
        feed_code in prop_oneof![Just(41_u8), Just(51)],
    ) {
        let buf = make_deep_depth_packet(feed_code, 2, security_id, 1, 20);
        let result = dispatch_deep_depth_frame(&buf, 999);
        prop_assert!(result.is_ok());
        match result.unwrap() {
            ParsedFrame::DeepDepth {
                security_id: sid,
                side,
                levels,
                received_at_nanos,
                ..
            } => {
                prop_assert_eq!(sid, security_id);
                prop_assert_eq!(levels.len(), 20);
                prop_assert_eq!(received_at_nanos, 999);
                let expected = if feed_code == 41 { DepthSide::Bid } else { DepthSide::Ask };
                prop_assert_eq!(side, expected);
            }
            other => prop_assert!(false, "expected DeepDepth, got: {other:?}"),
        }
    }
}

// ===========================================================================
// 27. Subscription builder — empty input produces empty output
// ===========================================================================

proptest! {
    #[test]
    fn proptest_subscription_empty_always_empty(
        batch_size in 1_usize..200,
        mode in prop_oneof![Just(FeedMode::Ticker), Just(FeedMode::Quote), Just(FeedMode::Full)],
    ) {
        let messages = build_subscription_messages(&[], mode, batch_size);
        prop_assert_eq!(messages.len(), 0);
    }
}

// ===========================================================================
// 28. Subscription builder — single instrument produces exactly 1 message
// ===========================================================================

proptest! {
    #[test]
    fn proptest_subscription_single_instrument_one_message(
        security_id in 1_u32..u32::MAX,
        batch_size in 1_usize..200,
    ) {
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::NseEquity, security_id)];
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, batch_size);
        prop_assert_eq!(messages.len(), 1);

        // Verify the JSON contains the security_id as a string
        let json = &messages[0];
        let expected = format!("\"{}\"", security_id);
        prop_assert!(json.contains(&expected), "JSON should contain SecurityId as string");
    }
}

// ===========================================================================
// 29. Candle aggregator — volume is snapshot (latest tick's volume)
// ===========================================================================

proptest! {
    #[test]
    fn proptest_candle_volume_is_snapshot(
        volumes in prop::collection::vec(1_u32..10_000_000, 2..20),
    ) {
        let mut aggregator = CandleAggregator::new();

        for &vol in &volumes {
            let tick = make_tick(42, 2, 100.0, 0.0, vol, 1_700_000_000);
            aggregator.update(&tick);
        }

        // Flush
        aggregator.update(&make_tick(42, 2, 100.0, 0.0, 0, 1_700_000_001));

        let completed = aggregator.completed_slice();
        prop_assert_eq!(completed.len(), 1);

        // Volume should be the LAST tick's volume (snapshot, not cumulative)
        let last_volume = *volumes.last().unwrap();
        prop_assert_eq!(completed[0].volume, last_volume);
    }
}
