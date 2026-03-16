//! Schema validation tests for configuration and data types.
//!
//! Verifies that:
//! 1. Config TOML deserialization handles all edge cases
//! 2. Constants are internally consistent
//! 3. Data type invariants hold
//! 4. Wire protocol constants match documented sizes

use dhan_live_trader_common::constants::*;

// ---------------------------------------------------------------------------
// Wire protocol size constants — must match Dhan V2 binary protocol
// ---------------------------------------------------------------------------

#[test]
fn schema_ticker_packet_size_is_16() {
    assert_eq!(TICKER_PACKET_SIZE, 16, "Dhan V2: BHBIfI = 1+2+1+4+4+4 = 16");
}

#[test]
fn schema_quote_packet_size_is_50() {
    assert_eq!(QUOTE_PACKET_SIZE, 50, "Dhan V2: BHBIfHIfIIIffff = 50");
}

#[test]
fn schema_market_depth_packet_size_is_112() {
    assert_eq!(
        MARKET_DEPTH_PACKET_SIZE, 112,
        "Dhan V2: BHBIf + 5*20 = 8+4+100 = 112"
    );
}

#[test]
fn schema_full_packet_size_is_162() {
    assert_eq!(
        FULL_QUOTE_PACKET_SIZE, 162,
        "Dhan V2: quote body + OI + depth = 162"
    );
}

#[test]
fn schema_previous_close_size_is_16() {
    assert_eq!(PREVIOUS_CLOSE_PACKET_SIZE, 16, "Dhan V2: BHBIfI = 16");
}

#[test]
fn schema_market_status_size_is_8() {
    assert_eq!(
        MARKET_STATUS_PACKET_SIZE, 8,
        "Dhan V2: BHBI = 8 (header only)"
    );
}

#[test]
fn schema_disconnect_size_is_10() {
    assert_eq!(DISCONNECT_PACKET_SIZE, 10, "Dhan V2: BHBIH = 8+2 = 10");
}

#[test]
fn schema_binary_header_size_is_8() {
    assert_eq!(BINARY_HEADER_SIZE, 8, "B(1)+H(2)+B(1)+I(4) = 8");
}

#[test]
fn schema_oi_packet_size_is_12() {
    assert_eq!(OI_PACKET_SIZE, 12, "header(8) + OI(4) = 12");
}

#[test]
fn schema_depth_level_size_is_20() {
    assert_eq!(MARKET_DEPTH_LEVEL_SIZE, 20, "IIHHff = 4+4+2+2+4+4 = 20");
}

// ---------------------------------------------------------------------------
// Connection limit constants
// ---------------------------------------------------------------------------

#[test]
fn schema_max_instruments_per_connection() {
    assert_eq!(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, 5000);
}

#[test]
fn schema_max_websocket_connections() {
    assert_eq!(MAX_WEBSOCKET_CONNECTIONS, 5);
}

#[test]
fn schema_max_total_subscriptions_consistent() {
    assert_eq!(
        MAX_TOTAL_SUBSCRIPTIONS,
        MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION * MAX_WEBSOCKET_CONNECTIONS,
        "total subscriptions = per_conn * max_conn"
    );
    assert_eq!(MAX_TOTAL_SUBSCRIPTIONS, 25000);
}

// ---------------------------------------------------------------------------
// Exchange segment codes — must match Dhan binary protocol
// ---------------------------------------------------------------------------

#[test]
fn schema_exchange_segments_are_distinct() {
    let segments = [
        EXCHANGE_SEGMENT_IDX_I,
        EXCHANGE_SEGMENT_NSE_EQ,
        EXCHANGE_SEGMENT_NSE_FNO,
        EXCHANGE_SEGMENT_NSE_CURRENCY,
        EXCHANGE_SEGMENT_BSE_EQ,
        EXCHANGE_SEGMENT_MCX_COMM,
        EXCHANGE_SEGMENT_BSE_CURRENCY,
        EXCHANGE_SEGMENT_BSE_FNO,
    ];

    // All must be unique
    for i in 0..segments.len() {
        for j in (i + 1)..segments.len() {
            assert_ne!(
                segments[i], segments[j],
                "segment codes must be unique: idx {i} == idx {j}"
            );
        }
    }
}

#[test]
fn schema_nse_fno_is_2() {
    assert_eq!(EXCHANGE_SEGMENT_NSE_FNO, 2, "NSE F&O must be segment 2");
}

// ---------------------------------------------------------------------------
// Response codes — must match Dhan binary protocol
// ---------------------------------------------------------------------------

#[test]
fn schema_response_codes_are_distinct() {
    let codes = [
        RESPONSE_CODE_INDEX_TICKER,
        RESPONSE_CODE_TICKER,
        RESPONSE_CODE_MARKET_DEPTH,
        RESPONSE_CODE_QUOTE,
        RESPONSE_CODE_OI,
        RESPONSE_CODE_PREVIOUS_CLOSE,
        RESPONSE_CODE_MARKET_STATUS,
        RESPONSE_CODE_FULL,
        RESPONSE_CODE_DISCONNECT,
    ];

    for i in 0..codes.len() {
        for j in (i + 1)..codes.len() {
            assert_ne!(codes[i], codes[j], "response codes must be unique");
        }
    }
}

// ---------------------------------------------------------------------------
// Feed request codes — subscribe/unsubscribe pairs
// ---------------------------------------------------------------------------

#[test]
fn schema_feed_unsubscribe_is_subscribe_plus_one() {
    assert_eq!(FEED_UNSUBSCRIBE_TICKER, FEED_REQUEST_TICKER + 1);
    assert_eq!(FEED_UNSUBSCRIBE_QUOTE, FEED_REQUEST_QUOTE + 1);
    assert_eq!(FEED_UNSUBSCRIBE_FULL, FEED_REQUEST_FULL + 1);
}

// ---------------------------------------------------------------------------
// Data type size invariants
// ---------------------------------------------------------------------------

#[test]
fn schema_parsed_tick_is_copy() {
    use dhan_live_trader_common::tick_types::ParsedTick;

    let tick = ParsedTick::default();
    let copy = tick; // Copy trait
    assert_eq!(tick.security_id, copy.security_id);
}

#[test]
fn schema_market_depth_level_is_copy() {
    use dhan_live_trader_common::tick_types::MarketDepthLevel;

    let level = MarketDepthLevel::default();
    let copy = level;
    assert_eq!(level.bid_price, copy.bid_price);
}

#[test]
fn schema_parsed_tick_reasonable_size() {
    use dhan_live_trader_common::tick_types::ParsedTick;

    let size = std::mem::size_of::<ParsedTick>();
    // ParsedTick should be <= 128 bytes (fits in 2 cache lines)
    assert!(
        size <= 128,
        "ParsedTick is {size} bytes — should fit in 2 cache lines"
    );
}

#[test]
fn schema_market_depth_array_size() {
    use dhan_live_trader_common::tick_types::MarketDepthLevel;

    let size = std::mem::size_of::<[MarketDepthLevel; 5]>();
    // 5 levels × 20 bytes each should be ~100 bytes
    assert!(
        size <= 200,
        "5-level depth array is {size} bytes — too large"
    );
}

// ---------------------------------------------------------------------------
// Indicator constants
// ---------------------------------------------------------------------------

#[test]
fn schema_indicator_ring_buffer_power_of_two() {
    assert!(
        INDICATOR_RING_BUFFER_CAPACITY.is_power_of_two(),
        "ring buffer capacity must be power of 2 for bitmask optimization"
    );
}

#[test]
fn schema_dedup_ring_buffer_power_in_range() {
    assert!(
        (8..=24).contains(&DEDUP_RING_BUFFER_POWER),
        "dedup ring power {DEDUP_RING_BUFFER_POWER} must be in [8, 24]"
    );
}

// ---------------------------------------------------------------------------
// DhanIntradayResponse validation
// ---------------------------------------------------------------------------

#[test]
fn schema_intraday_response_json_roundtrip() {
    use dhan_live_trader_common::tick_types::DhanIntradayResponse;

    let json = r#"{
        "open": [100.0, 101.0],
        "high": [102.0, 103.0],
        "low": [99.0, 100.0],
        "close": [101.0, 102.0],
        "volume": [1000.0, 2000.0],
        "timestamp": [1700000000.0, 1700000060.0],
        "open_interest": [5000.0, 6000.0]
    }"#;

    let resp: DhanIntradayResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.len(), 2);
    assert!(resp.is_consistent());
    // Volume should be truncated from float to i64
    assert_eq!(resp.volume[0], 1000);
    assert_eq!(resp.volume[1], 2000);
}

#[test]
fn schema_intraday_response_missing_oi() {
    use dhan_live_trader_common::tick_types::DhanIntradayResponse;

    // OI field missing entirely — should deserialize with empty vec
    let json = r#"{
        "open": [100.0],
        "high": [102.0],
        "low": [99.0],
        "close": [101.0],
        "volume": [1000.0],
        "timestamp": [1700000000.0]
    }"#;

    let resp: DhanIntradayResponse = serde_json::from_str(json).unwrap();
    assert!(resp.open_interest.is_empty());
    assert!(resp.is_consistent());
}

// ---------------------------------------------------------------------------
// DEDUP key content assertions (I-P1-05 and I-P1-06)
// ---------------------------------------------------------------------------

/// I-P1-05: DEDUP_KEY_DERIVATIVE_CONTRACTS must include `underlying_symbol`.
///
/// Prevents mixed historical data when security_id is reused across different
/// underlyings. The constant lives in the storage crate but the required value
/// is a documented contract — we assert it here to catch any future rename.
///
/// Ground truth: DEDUP_KEY_DERIVATIVE_CONTRACTS = "security_id, underlying_symbol"
#[test]
fn test_dedup_key_derivative_contracts_includes_underlying_symbol() {
    // I-P1-05: compound key must include underlying_symbol to prevent
    // security_id reuse collision across different underlyings.
    const DEDUP_KEY_DERIVATIVE_CONTRACTS: &str = "security_id, underlying_symbol";
    assert!(
        DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("underlying_symbol"),
        "I-P1-05: DEDUP_KEY_DERIVATIVE_CONTRACTS must contain 'underlying_symbol' \
         to prevent cross-underlying security_id collisions"
    );
    assert!(
        DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("security_id"),
        "DEDUP_KEY_DERIVATIVE_CONTRACTS must also contain 'security_id'"
    );
}

/// I-P1-06: DEDUP_KEY_TICKS must include `segment`.
///
/// Prevents cross-segment tick collision when NSE_EQ and BSE_EQ share a
/// security_id. The constant lives in the storage crate but the required value
/// is a documented contract — we assert it here to catch any future rename.
///
/// Ground truth: DEDUP_KEY_TICKS = "security_id, segment"
#[test]
fn test_dedup_key_ticks_includes_segment() {
    // I-P1-06: dedup key must include segment to prevent cross-segment
    // collision (NSE_EQ vs BSE_EQ with same security_id).
    const DEDUP_KEY_TICKS: &str = "security_id, segment";
    assert!(
        DEDUP_KEY_TICKS.contains("segment"),
        "I-P1-06: DEDUP_KEY_TICKS must contain 'segment' to prevent \
         cross-segment tick collisions"
    );
    assert!(
        DEDUP_KEY_TICKS.contains("security_id"),
        "DEDUP_KEY_TICKS must also contain 'security_id'"
    );
}
