//! End-to-end WebSocket + Binary Protocol + QuestDB + Auth integration tests.
//!
//! Cross-references every byte offset, field mapping, exchange segment code,
//! feed request/response code, disconnect code, packet size, and subscription
//! format against the Dhan API V2 reference (dhanhq.co/docs/v2).
//!
//! CRITICAL: These tests guarantee that live market feed data is NEVER
//! manipulated, misaligned, or incorrectly mapped.

#![allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction

use tickvault_common::constants::{
    // Packet sizes
    BINARY_HEADER_SIZE,
    // Deep depth
    DEEP_DEPTH_FEED_CODE_ASK,
    DEEP_DEPTH_FEED_CODE_BID,
    DEEP_DEPTH_HEADER_SIZE,
    DEEP_DEPTH_LEVEL_OFFSET_ORDERS,
    DEEP_DEPTH_LEVEL_OFFSET_PRICE,
    DEEP_DEPTH_LEVEL_OFFSET_QUANTITY,
    DEEP_DEPTH_LEVEL_SIZE,
    // Depth level offsets
    DEPTH_LEVEL_OFFSET_ASK_ORDERS,
    DEPTH_LEVEL_OFFSET_ASK_PRICE,
    DEPTH_LEVEL_OFFSET_ASK_QTY,
    DEPTH_LEVEL_OFFSET_BID_ORDERS,
    DEPTH_LEVEL_OFFSET_BID_PRICE,
    DEPTH_LEVEL_OFFSET_BID_QTY,
    // Auth constants
    DHAN_GENERATE_TOKEN_PATH,
    DHAN_RENEW_TOKEN_PATH,
    // Disconnect codes
    DISCONNECT_ACCESS_TOKEN_EXPIRED,
    DISCONNECT_ACCESS_TOKEN_INVALID,
    DISCONNECT_AUTHENTICATION_FAILED,
    DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED,
    DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS,
    // Disconnect offset
    DISCONNECT_OFFSET_CODE,
    DISCONNECT_PACKET_SIZE,
    // Exchange segment codes
    EXCHANGE_SEGMENT_BSE_CURRENCY,
    EXCHANGE_SEGMENT_BSE_EQ,
    EXCHANGE_SEGMENT_BSE_FNO,
    EXCHANGE_SEGMENT_IDX_I,
    EXCHANGE_SEGMENT_MCX_COMM,
    EXCHANGE_SEGMENT_NSE_CURRENCY,
    EXCHANGE_SEGMENT_NSE_EQ,
    EXCHANGE_SEGMENT_NSE_FNO,
    // Feed request codes
    FEED_REQUEST_DISCONNECT,
    FEED_REQUEST_FULL,
    FEED_REQUEST_QUOTE,
    FEED_REQUEST_TICKER,
    FEED_UNSUBSCRIBE_FULL,
    FEED_UNSUBSCRIBE_QUOTE,
    FEED_UNSUBSCRIBE_TICKER,
    // Full packet offsets
    FULL_OFFSET_CLOSE,
    FULL_OFFSET_DEPTH_START,
    FULL_OFFSET_HIGH,
    FULL_OFFSET_LOW,
    FULL_OFFSET_OI,
    FULL_OFFSET_OI_DAY_HIGH,
    FULL_OFFSET_OI_DAY_LOW,
    FULL_OFFSET_OPEN,
    FULL_QUOTE_PACKET_SIZE,
    // Header offsets
    HEADER_OFFSET_EXCHANGE_SEGMENT,
    HEADER_OFFSET_MESSAGE_LENGTH,
    HEADER_OFFSET_RESPONSE_CODE,
    HEADER_OFFSET_SECURITY_ID,
    // IST offset
    IST_UTC_OFFSET_SECONDS_I64,
    MARKET_DEPTH_LEVEL_SIZE,
    MARKET_DEPTH_LEVELS,
    MARKET_DEPTH_PACKET_SIZE,
    // Market status
    MARKET_STATUS_CLOSED,
    MARKET_STATUS_OPEN,
    MARKET_STATUS_PACKET_SIZE,
    MARKET_STATUS_POST_CLOSE,
    MARKET_STATUS_PRE_OPEN,
    // Connection limits
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION,
    MAX_TOTAL_SUBSCRIPTIONS,
    MAX_WEBSOCKET_CONNECTIONS,
    // OI offset
    OI_OFFSET_VALUE,
    OI_PACKET_SIZE,
    // Previous close offsets
    PREV_CLOSE_OFFSET_OI,
    PREV_CLOSE_OFFSET_PRICE,
    PREVIOUS_CLOSE_PACKET_SIZE,
    // QuestDB tables
    QUESTDB_TABLE_CANDLES_1S,
    QUESTDB_TABLE_HISTORICAL_CANDLES,
    QUESTDB_TABLE_MARKET_DEPTH,
    QUESTDB_TABLE_NSE_HOLIDAYS,
    QUESTDB_TABLE_PREVIOUS_CLOSE,
    QUESTDB_TABLE_TICKS,
    // Quote offsets
    QUOTE_OFFSET_ATP,
    QUOTE_OFFSET_CLOSE,
    QUOTE_OFFSET_HIGH,
    QUOTE_OFFSET_LOW,
    QUOTE_OFFSET_LTP,
    QUOTE_OFFSET_LTQ,
    QUOTE_OFFSET_LTT,
    QUOTE_OFFSET_OPEN,
    QUOTE_OFFSET_TOTAL_BUY_QTY,
    QUOTE_OFFSET_TOTAL_SELL_QTY,
    QUOTE_OFFSET_VOLUME,
    QUOTE_PACKET_SIZE,
    // Response codes
    RESPONSE_CODE_DISCONNECT,
    RESPONSE_CODE_FULL,
    RESPONSE_CODE_INDEX_TICKER,
    RESPONSE_CODE_MARKET_DEPTH,
    RESPONSE_CODE_MARKET_STATUS,
    RESPONSE_CODE_OI,
    RESPONSE_CODE_PREVIOUS_CLOSE,
    RESPONSE_CODE_QUOTE,
    RESPONSE_CODE_TICKER,
    SERVER_PING_INTERVAL_SECS,
    SERVER_PING_TIMEOUT_SECS,
    SUBSCRIPTION_BATCH_SIZE,
    // Ticker offsets
    TICKER_OFFSET_LTP,
    TICKER_OFFSET_LTT,
    TICKER_PACKET_SIZE,
    TOTP_DIGITS,
    TOTP_PERIOD_SECS,
    TWENTY_DEPTH_LEVELS,
    TWO_HUNDRED_DEPTH_LEVELS,
    // WebSocket auth
    WEBSOCKET_AUTH_TYPE,
    WEBSOCKET_PROTOCOL_VERSION,
};
use tickvault_common::tick_types::{MarketDepthLevel, ParsedTick};

use tickvault_core::parser::dispatch_frame;
use tickvault_core::websocket::types::DisconnectCode;

// =========================================================================
// SECTION 1: BINARY PROTOCOL — PACKET SIZE VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#binary-response
// These sizes MUST match the Dhan Python SDK struct.calcsize() exactly.
// =========================================================================

#[test]
fn test_protocol_header_size_matches_dhan_spec() {
    // Spec: response_code(u8=1) + msg_length(u16=2) + segment(u8=1) + security_id(u32=4) = 8
    assert_eq!(
        BINARY_HEADER_SIZE, 8,
        "header must be exactly 8 bytes per Dhan spec"
    );
}

#[test]
fn test_protocol_ticker_packet_size_matches_dhan_spec() {
    // Spec: <BHBIfI> = 1+2+1+4+4+4 = 16 bytes
    assert_eq!(
        TICKER_PACKET_SIZE, 16,
        "ticker must be 16 bytes per Dhan SDK struct.calcsize('<BHBIfI')"
    );
}

#[test]
fn test_protocol_quote_packet_size_matches_dhan_spec() {
    // Spec: <BHBIfHIfIIIffff> = 1+2+1+4+4+2+4+4+4+4+4+4+4+4+4 = 50 bytes
    assert_eq!(QUOTE_PACKET_SIZE, 50, "quote must be 50 bytes per Dhan SDK");
}

#[test]
fn test_protocol_full_packet_size_matches_dhan_spec() {
    // Spec: <BHBIfHIfIIIIIIffff100s> = quote(42) + OI(12) + OHLC(16) + depth(100) - header overlap
    // Actual: 8(hdr) + 42(quote body) + 12(OI) + 16(OHLC) + 100(depth) - 16(shared) = 162
    assert_eq!(
        FULL_QUOTE_PACKET_SIZE, 162,
        "full packet must be 162 bytes per Dhan SDK"
    );
}

#[test]
fn test_protocol_oi_packet_size_matches_dhan_spec() {
    // Spec: <BHBII> = 1+2+1+4+4 = 12 bytes
    assert_eq!(
        OI_PACKET_SIZE, 12,
        "OI packet must be 12 bytes per Dhan SDK"
    );
}

#[test]
fn test_protocol_previous_close_size_matches_dhan_spec() {
    // Spec: <BHBIfI> = 1+2+1+4+4+4 = 16 bytes
    assert_eq!(
        PREVIOUS_CLOSE_PACKET_SIZE, 16,
        "prev close must be 16 bytes per Dhan SDK"
    );
}

#[test]
fn test_protocol_market_status_size_matches_dhan_spec() {
    // Spec: header only, 8 bytes
    assert_eq!(
        MARKET_STATUS_PACKET_SIZE, 8,
        "market status must be 8 bytes (header only)"
    );
}

#[test]
fn test_protocol_disconnect_size_matches_dhan_spec() {
    // Spec: <BHBIH> = 8 (header) + 2 (code) = 10 bytes
    assert_eq!(
        DISCONNECT_PACKET_SIZE, 10,
        "disconnect must be 10 bytes per Dhan SDK"
    );
}

#[test]
fn test_protocol_market_depth_level_size_matches_dhan_spec() {
    // Spec: <IIHHff> = 4+4+2+2+4+4 = 20 bytes per level
    assert_eq!(
        MARKET_DEPTH_LEVEL_SIZE, 20,
        "depth level must be 20 bytes per <IIHHff>"
    );
}

#[test]
fn test_protocol_market_depth_standalone_packet_size() {
    // header(8) + LTP(4) + depth(5*20=100) = 112
    assert_eq!(
        MARKET_DEPTH_PACKET_SIZE, 112,
        "standalone depth must be 112 bytes"
    );
}

#[test]
fn test_protocol_full_packet_depth_region_is_100_bytes() {
    // 5 levels × 20 bytes/level = 100 bytes
    assert_eq!(
        MARKET_DEPTH_LEVELS * MARKET_DEPTH_LEVEL_SIZE,
        100,
        "depth region must be exactly 100 bytes"
    );
}

// =========================================================================
// SECTION 2: HEADER OFFSET VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#response-header
// =========================================================================

#[test]
fn test_header_offsets_match_dhan_spec() {
    assert_eq!(HEADER_OFFSET_RESPONSE_CODE, 0, "response_code at byte 0");
    assert_eq!(HEADER_OFFSET_MESSAGE_LENGTH, 1, "msg_length at byte 1");
    assert_eq!(
        HEADER_OFFSET_EXCHANGE_SEGMENT, 3,
        "exchange_segment at byte 3"
    );
    assert_eq!(HEADER_OFFSET_SECURITY_ID, 4, "security_id at byte 4");
}

#[test]
fn test_header_field_sizes_match_dhan_spec() {
    // response_code: u8 (1 byte) at offset 0
    // msg_length: u16 LE (2 bytes) at offset 1-2
    // exchange_segment: u8 (1 byte) at offset 3
    // security_id: u32 LE (4 bytes) at offset 4-7
    // Total: 1 + 2 + 1 + 4 = 8 bytes
    assert_eq!(
        HEADER_OFFSET_RESPONSE_CODE + 1,
        HEADER_OFFSET_MESSAGE_LENGTH
    );
    assert_eq!(
        HEADER_OFFSET_MESSAGE_LENGTH + 2,
        HEADER_OFFSET_EXCHANGE_SEGMENT
    );
    assert_eq!(
        HEADER_OFFSET_EXCHANGE_SEGMENT + 1,
        HEADER_OFFSET_SECURITY_ID
    );
    assert_eq!(HEADER_OFFSET_SECURITY_ID + 4, BINARY_HEADER_SIZE);
}

// =========================================================================
// SECTION 3: TICKER OFFSETS
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#ticker-packet
// =========================================================================

#[test]
fn test_ticker_offsets_match_dhan_spec() {
    assert_eq!(TICKER_OFFSET_LTP, 8, "LTP at byte 8 (after header)");
    assert_eq!(TICKER_OFFSET_LTT, 12, "LTT at byte 12");
    assert_eq!(
        TICKER_OFFSET_LTT + 4,
        TICKER_PACKET_SIZE,
        "ticker ends at 16"
    );
}

// =========================================================================
// SECTION 4: QUOTE OFFSETS
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#quote-packet
// =========================================================================

#[test]
fn test_quote_offsets_match_dhan_spec() {
    assert_eq!(QUOTE_OFFSET_LTP, 8, "LTP at 8");
    assert_eq!(QUOTE_OFFSET_LTQ, 12, "LTQ at 12 (u16)");
    assert_eq!(QUOTE_OFFSET_LTT, 14, "LTT at 14 (u32)");
    assert_eq!(QUOTE_OFFSET_ATP, 18, "ATP at 18");
    assert_eq!(QUOTE_OFFSET_VOLUME, 22, "Volume at 22");
    assert_eq!(QUOTE_OFFSET_TOTAL_SELL_QTY, 26, "Total Sell Qty at 26");
    assert_eq!(QUOTE_OFFSET_TOTAL_BUY_QTY, 30, "Total Buy Qty at 30");
    assert_eq!(QUOTE_OFFSET_OPEN, 34, "Open at 34");
    assert_eq!(QUOTE_OFFSET_CLOSE, 38, "Close at 38");
    assert_eq!(QUOTE_OFFSET_HIGH, 42, "High at 42");
    assert_eq!(QUOTE_OFFSET_LOW, 46, "Low at 46");
    assert_eq!(QUOTE_OFFSET_LOW + 4, QUOTE_PACKET_SIZE, "quote ends at 50");
}

// =========================================================================
// SECTION 5: FULL PACKET OFFSETS — CRITICAL DIVERGENCE FROM QUOTE
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#full-packet
// In Full, offsets 34-45 are OI fields, NOT OHLC (which shifts to 46-61).
// =========================================================================

#[test]
fn test_full_packet_oi_offsets_diverge_from_quote() {
    // Quote has OHLC at 34-49
    // Full has OI at 34-45, then OHLC at 46-61
    assert_eq!(FULL_OFFSET_OI, 34, "OI at 34 (where Quote has Open)");
    assert_eq!(
        FULL_OFFSET_OI_DAY_HIGH, 38,
        "OI day high at 38 (where Quote has Close)"
    );
    assert_eq!(
        FULL_OFFSET_OI_DAY_LOW, 42,
        "OI day low at 42 (where Quote has High)"
    );
}

#[test]
fn test_full_packet_ohlc_shifted_by_12_bytes() {
    // Full OHLC is 12 bytes later than Quote OHLC
    assert_eq!(
        FULL_OFFSET_OPEN, 46,
        "Full Open at 46 (Quote at 34, delta=12)"
    );
    assert_eq!(
        FULL_OFFSET_CLOSE, 50,
        "Full Close at 50 (Quote at 38, delta=12)"
    );
    assert_eq!(
        FULL_OFFSET_HIGH, 54,
        "Full High at 54 (Quote at 42, delta=12)"
    );
    assert_eq!(
        FULL_OFFSET_LOW, 58,
        "Full Low at 58 (Quote at 46, delta=12)"
    );
}

#[test]
fn test_full_packet_depth_starts_at_62() {
    assert_eq!(FULL_OFFSET_DEPTH_START, 62, "depth starts at byte 62");
    assert_eq!(
        FULL_OFFSET_DEPTH_START + MARKET_DEPTH_LEVELS * MARKET_DEPTH_LEVEL_SIZE,
        162,
        "depth ends at 162 (packet end)"
    );
}

// =========================================================================
// SECTION 6: RESPONSE CODE VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/annexure/#feed-response-code
// =========================================================================

#[test]
fn test_response_codes_match_dhan_spec() {
    assert_eq!(RESPONSE_CODE_INDEX_TICKER, 1);
    assert_eq!(RESPONSE_CODE_TICKER, 2);
    assert_eq!(RESPONSE_CODE_MARKET_DEPTH, 3);
    assert_eq!(RESPONSE_CODE_QUOTE, 4);
    assert_eq!(RESPONSE_CODE_OI, 5);
    assert_eq!(RESPONSE_CODE_PREVIOUS_CLOSE, 6);
    assert_eq!(RESPONSE_CODE_MARKET_STATUS, 7);
    assert_eq!(RESPONSE_CODE_FULL, 8);
    assert_eq!(RESPONSE_CODE_DISCONNECT, 50);
}

#[test]
fn test_no_response_code_collision() {
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
    for (i, &a) in codes.iter().enumerate() {
        for (j, &b) in codes.iter().enumerate() {
            if i != j {
                assert_ne!(
                    a, b,
                    "response code collision: code[{i}]={a} == code[{j}]={b}"
                );
            }
        }
    }
}

// =========================================================================
// SECTION 7: EXCHANGE SEGMENT CODE VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/annexure/#exchange-segment
// =========================================================================

#[test]
fn test_exchange_segment_codes_match_dhan_spec() {
    assert_eq!(EXCHANGE_SEGMENT_IDX_I, 0, "IDX_I must be 0");
    assert_eq!(EXCHANGE_SEGMENT_NSE_EQ, 1, "NSE_EQ must be 1");
    assert_eq!(EXCHANGE_SEGMENT_NSE_FNO, 2, "NSE_FNO must be 2");
    assert_eq!(EXCHANGE_SEGMENT_NSE_CURRENCY, 3, "NSE_CURRENCY must be 3");
    assert_eq!(EXCHANGE_SEGMENT_BSE_EQ, 4, "BSE_EQ must be 4");
    assert_eq!(EXCHANGE_SEGMENT_MCX_COMM, 5, "MCX_COMM must be 5");
    // Code 6 is SKIPPED in Dhan protocol
    assert_eq!(EXCHANGE_SEGMENT_BSE_CURRENCY, 7, "BSE_CURRENCY must be 7");
    assert_eq!(EXCHANGE_SEGMENT_BSE_FNO, 8, "BSE_FNO must be 8");
}

#[test]
fn test_exchange_segment_code_6_is_unused() {
    // Dhan protocol skips code 6. Verify no constant uses it.
    let all_segments = [
        EXCHANGE_SEGMENT_IDX_I,
        EXCHANGE_SEGMENT_NSE_EQ,
        EXCHANGE_SEGMENT_NSE_FNO,
        EXCHANGE_SEGMENT_NSE_CURRENCY,
        EXCHANGE_SEGMENT_BSE_EQ,
        EXCHANGE_SEGMENT_MCX_COMM,
        EXCHANGE_SEGMENT_BSE_CURRENCY,
        EXCHANGE_SEGMENT_BSE_FNO,
    ];
    for seg in &all_segments {
        assert_ne!(
            *seg, 6u8,
            "code 6 must not be assigned — it's unused in Dhan protocol"
        );
    }
}

#[test]
fn test_no_exchange_segment_collision() {
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
    for (i, &a) in segments.iter().enumerate() {
        for (j, &b) in segments.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "segment collision: [{i}]={a} == [{j}]={b}");
            }
        }
    }
}

// =========================================================================
// SECTION 8: FEED REQUEST/UNSUBSCRIBE CODE VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/annexure/#feed-request-code
// =========================================================================

#[test]
fn test_feed_request_codes_match_dhan_spec() {
    assert_eq!(FEED_REQUEST_TICKER, 15, "Ticker subscribe must be 15");
    assert_eq!(FEED_REQUEST_QUOTE, 17, "Quote subscribe must be 17");
    assert_eq!(FEED_REQUEST_FULL, 21, "Full subscribe must be 21");
    assert_eq!(FEED_REQUEST_DISCONNECT, 12, "Disconnect must be 12");
}

#[test]
fn test_unsubscribe_codes_are_subscribe_plus_one() {
    assert_eq!(
        FEED_UNSUBSCRIBE_TICKER,
        FEED_REQUEST_TICKER + 1,
        "unsub ticker = 15+1=16"
    );
    assert_eq!(
        FEED_UNSUBSCRIBE_QUOTE,
        FEED_REQUEST_QUOTE + 1,
        "unsub quote = 17+1=18"
    );
    assert_eq!(
        FEED_UNSUBSCRIBE_FULL,
        FEED_REQUEST_FULL + 1,
        "unsub full = 21+1=22"
    );
}

#[test]
fn test_unsubscribe_codes_explicit_values() {
    assert_eq!(FEED_UNSUBSCRIBE_TICKER, 16);
    assert_eq!(FEED_UNSUBSCRIBE_QUOTE, 18);
    assert_eq!(FEED_UNSUBSCRIBE_FULL, 22);
}

// =========================================================================
// SECTION 9: DISCONNECT CODE VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#feed-disconnect
// =========================================================================

#[test]
fn test_disconnect_codes_match_dhan_spec() {
    assert_eq!(DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS, 805);
    assert_eq!(DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED, 806);
    assert_eq!(DISCONNECT_ACCESS_TOKEN_EXPIRED, 807);
    assert_eq!(DISCONNECT_AUTHENTICATION_FAILED, 808);
    assert_eq!(DISCONNECT_ACCESS_TOKEN_INVALID, 809);
}

#[test]
fn test_disconnect_code_from_u16_roundtrip() {
    let pairs: [(u16, DisconnectCode); 5] = [
        (805, DisconnectCode::ExceededActiveConnections),
        (806, DisconnectCode::DataApiSubscriptionRequired),
        (807, DisconnectCode::AccessTokenExpired),
        (808, DisconnectCode::AuthenticationFailed),
        (809, DisconnectCode::AccessTokenInvalid),
    ];
    for (raw, expected) in &pairs {
        let code = DisconnectCode::from_u16(*raw);
        assert_eq!(code, *expected, "from_u16({raw}) must map correctly");
        assert_eq!(code.as_u16(), *raw, "as_u16 must roundtrip to {raw}");
    }
}

#[test]
fn test_only_807_triggers_token_refresh() {
    let codes = [
        DisconnectCode::ExceededActiveConnections,
        DisconnectCode::DataApiSubscriptionRequired,
        DisconnectCode::AccessTokenExpired,
        DisconnectCode::AuthenticationFailed,
        DisconnectCode::AccessTokenInvalid,
        DisconnectCode::ClientIdInvalid,
        DisconnectCode::Unknown(999),
    ];
    for code in &codes {
        if matches!(code, DisconnectCode::AccessTokenExpired) {
            assert!(
                code.requires_token_refresh(),
                "{code:?} must require refresh"
            );
        } else {
            assert!(
                !code.requires_token_refresh(),
                "{code:?} must NOT require refresh"
            );
        }
    }
}

#[test]
fn test_reconnectable_codes_are_807_and_unknown_only() {
    assert!(!DisconnectCode::ExceededActiveConnections.is_reconnectable());
    assert!(!DisconnectCode::DataApiSubscriptionRequired.is_reconnectable());
    assert!(DisconnectCode::AccessTokenExpired.is_reconnectable());
    assert!(!DisconnectCode::AuthenticationFailed.is_reconnectable());
    assert!(!DisconnectCode::AccessTokenInvalid.is_reconnectable());
    assert!(!DisconnectCode::ClientIdInvalid.is_reconnectable());
    assert!(DisconnectCode::Unknown(999).is_reconnectable());
}

// =========================================================================
// SECTION 10: CONNECTION LIMIT VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#establishing-connection
// =========================================================================

#[test]
fn test_connection_limits_match_dhan_spec() {
    assert_eq!(
        MAX_WEBSOCKET_CONNECTIONS, 5,
        "max 5 connections per Dhan account"
    );
    assert_eq!(
        MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, 5000,
        "max 5000 instruments/conn"
    );
    assert_eq!(MAX_TOTAL_SUBSCRIPTIONS, 25000, "total = 5 × 5000 = 25000");
    assert_eq!(
        SUBSCRIPTION_BATCH_SIZE, 100,
        "max 100 instruments per subscribe message"
    );
}

#[test]
fn test_total_subscriptions_is_product_of_limits() {
    assert_eq!(
        MAX_TOTAL_SUBSCRIPTIONS,
        MAX_WEBSOCKET_CONNECTIONS * MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION
    );
}

// =========================================================================
// SECTION 11: KEEPALIVE VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#keeping-connection-alive
// =========================================================================

#[test]
fn test_keepalive_constants_match_dhan_spec() {
    assert_eq!(SERVER_PING_INTERVAL_SECS, 10, "server pings every 10s");
    assert_eq!(
        SERVER_PING_TIMEOUT_SECS, 40,
        "server times out after 40s no pong"
    );
}

// =========================================================================
// SECTION 12: WEBSOCKET AUTH VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#establishing-connection
// =========================================================================

#[test]
fn test_websocket_auth_constants() {
    assert_eq!(WEBSOCKET_AUTH_TYPE, 2, "authType must be 2");
    assert_eq!(WEBSOCKET_PROTOCOL_VERSION, "2", "version must be '2'");
}

// =========================================================================
// SECTION 13: MARKET STATUS CODES
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/ (market status section)
// =========================================================================

#[test]
fn test_market_status_codes_match_dhan_spec() {
    assert_eq!(MARKET_STATUS_CLOSED, 0);
    assert_eq!(MARKET_STATUS_PRE_OPEN, 1);
    assert_eq!(MARKET_STATUS_OPEN, 2);
    assert_eq!(MARKET_STATUS_POST_CLOSE, 3);
}

// =========================================================================
// SECTION 14: DEEP DEPTH PROTOCOL VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/ (20-level and 200-level depth)
// =========================================================================

#[test]
fn test_deep_depth_constants_match_dhan_spec() {
    assert_eq!(DEEP_DEPTH_HEADER_SIZE, 12, "deep depth header is 12 bytes");
    assert_eq!(
        DEEP_DEPTH_LEVEL_SIZE, 16,
        "deep depth level is 16 bytes (f64+u32+u32)"
    );
    assert_eq!(TWENTY_DEPTH_LEVELS, 20);
    assert_eq!(TWO_HUNDRED_DEPTH_LEVELS, 200);
    assert_eq!(DEEP_DEPTH_FEED_CODE_BID, 41);
    assert_eq!(DEEP_DEPTH_FEED_CODE_ASK, 51);
}

#[test]
fn test_deep_depth_level_offsets() {
    assert_eq!(DEEP_DEPTH_LEVEL_OFFSET_PRICE, 0, "price at offset 0 (f64)");
    assert_eq!(
        DEEP_DEPTH_LEVEL_OFFSET_QUANTITY, 8,
        "quantity at offset 8 (u32)"
    );
    assert_eq!(
        DEEP_DEPTH_LEVEL_OFFSET_ORDERS, 12,
        "orders at offset 12 (u32)"
    );
}

#[test]
fn test_deep_depth_20_level_packet_size() {
    // header(12) + 20 levels × 16 bytes = 12 + 320 = 332
    let expected = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
    assert_eq!(expected, 332, "20-level per-side packet = 332 bytes");
}

#[test]
fn test_deep_depth_200_level_packet_size() {
    // header(12) + 200 levels × 16 bytes = 12 + 3200 = 3212
    let expected = DEEP_DEPTH_HEADER_SIZE + TWO_HUNDRED_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
    assert_eq!(expected, 3212, "200-level per-side packet = 3212 bytes");
}

// =========================================================================
// SECTION 15: DEPTH LEVEL FIELD OFFSETS
// Cross-reference: Full packet depth format <IIHHff>
// =========================================================================

#[test]
fn test_depth_level_offsets_match_dhan_format() {
    // <IIHHff> = bid_qty(u32,4) + ask_qty(u32,4) + bid_orders(u16,2) + ask_orders(u16,2) + bid_price(f32,4) + ask_price(f32,4)
    assert_eq!(DEPTH_LEVEL_OFFSET_BID_QTY, 0);
    assert_eq!(DEPTH_LEVEL_OFFSET_ASK_QTY, 4);
    assert_eq!(DEPTH_LEVEL_OFFSET_BID_ORDERS, 8);
    assert_eq!(DEPTH_LEVEL_OFFSET_ASK_ORDERS, 10);
    assert_eq!(DEPTH_LEVEL_OFFSET_BID_PRICE, 12);
    assert_eq!(DEPTH_LEVEL_OFFSET_ASK_PRICE, 16);
    // Total per level: 4+4+2+2+4+4 = 20
    assert_eq!(DEPTH_LEVEL_OFFSET_ASK_PRICE + 4, MARKET_DEPTH_LEVEL_SIZE);
}

// =========================================================================
// SECTION 16: AUTH TOKEN CONSTANTS
// Cross-reference: dhanhq.co/docs/v2/ (authentication section)
// =========================================================================

#[test]
fn test_auth_constants_match_dhan_spec() {
    assert_eq!(TOTP_DIGITS, 6, "TOTP must be 6 digits");
    assert_eq!(TOTP_PERIOD_SECS, 30, "TOTP period must be 30s");
    assert_eq!(DHAN_GENERATE_TOKEN_PATH, "/app/generateAccessToken");
    assert_eq!(DHAN_RENEW_TOKEN_PATH, "/RenewToken");
}

#[test]
fn test_ist_offset_is_5h30m() {
    assert_eq!(IST_UTC_OFFSET_SECONDS_I64, 19800, "IST = UTC+5:30 = 19800s");
}

// =========================================================================
// SECTION 17: QUESTDB TABLE NAMES — MUST NEVER CHANGE
// =========================================================================

#[test]
fn test_questdb_table_names_are_stable() {
    assert_eq!(QUESTDB_TABLE_TICKS, "ticks");
    assert_eq!(QUESTDB_TABLE_HISTORICAL_CANDLES, "historical_candles");
    assert_eq!(QUESTDB_TABLE_CANDLES_1S, "candles_1s");
    assert_eq!(QUESTDB_TABLE_MARKET_DEPTH, "market_depth");
    assert_eq!(QUESTDB_TABLE_PREVIOUS_CLOSE, "previous_close");
    assert_eq!(QUESTDB_TABLE_NSE_HOLIDAYS, "nse_holidays");
}

// =========================================================================
// SECTION 18: BINARY FRAME E2E PARSING — REALISTIC NIFTY TICK
// Construct exact bytes as Dhan would send them, parse, verify every field.
// =========================================================================

/// Builds a raw binary frame matching Dhan's wire format exactly.
fn build_raw_frame(response_code: u8, segment: u8, security_id: u32, body: &[u8]) -> Vec<u8> {
    let total_size = BINARY_HEADER_SIZE + body.len();
    let mut buf = vec![0u8; total_size];
    buf[0] = response_code;
    buf[1..3].copy_from_slice(&(total_size as u16).to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[BINARY_HEADER_SIZE..].copy_from_slice(body);
    buf
}

#[test]
fn test_e2e_nifty_ticker_packet() {
    // Simulate: NIFTY 50 (security_id=13, IDX_I segment) LTP=24567.85, LTT=1741608945
    let ltp: f32 = 24567.85;
    let ltt: u32 = 1741608945;

    let mut body = [0u8; 8]; // ticker body = LTP(4) + LTT(4)
    body[0..4].copy_from_slice(&ltp.to_le_bytes());
    body[4..8].copy_from_slice(&ltt.to_le_bytes());

    let frame = build_raw_frame(RESPONSE_CODE_TICKER, EXCHANGE_SEGMENT_IDX_I, 13, &body);

    let received_nanos = 1_741_608_945_123_456_789_i64;
    let result = dispatch_frame(&frame, received_nanos).unwrap();

    match result {
        tickvault_core::parser::types::ParsedFrame::Tick(tick) => {
            assert_eq!(tick.security_id, 13, "NIFTY security_id");
            assert_eq!(
                tick.exchange_segment_code, EXCHANGE_SEGMENT_IDX_I,
                "IDX_I segment"
            );
            assert_eq!(
                tick.last_traded_price, ltp,
                "LTP must be exact f32 roundtrip"
            );
            assert_eq!(tick.exchange_timestamp, ltt, "LTT must match");
            assert_eq!(
                tick.received_at_nanos, received_nanos,
                "receive timestamp propagated"
            );
            // Ticker doesn't populate these fields
            assert_eq!(tick.volume, 0);
            assert_eq!(tick.open_interest, 0);
            assert_eq!(tick.last_trade_quantity, 0);
        }
        other => panic!("expected Tick, got {other:?}"),
    }
}

#[test]
fn test_e2e_nifty_quote_packet() {
    let ltp: f32 = 24567.85;
    let ltq: u16 = 150;
    let ltt: u32 = 1741608945;
    let atp: f32 = 24520.3;
    let volume: u32 = 8_500_000;
    let total_sell: u32 = 2_100_000;
    let total_buy: u32 = 3_200_000;
    let open: f32 = 24450.0;
    let close: f32 = 24380.5;
    let high: f32 = 24590.0;
    let low: f32 = 24410.0;

    let mut body = [0u8; 42]; // quote body = 50 - 8 header
    body[0..4].copy_from_slice(&ltp.to_le_bytes());
    body[4..6].copy_from_slice(&ltq.to_le_bytes());
    body[6..10].copy_from_slice(&ltt.to_le_bytes());
    body[10..14].copy_from_slice(&atp.to_le_bytes());
    body[14..18].copy_from_slice(&volume.to_le_bytes());
    body[18..22].copy_from_slice(&total_sell.to_le_bytes());
    body[22..26].copy_from_slice(&total_buy.to_le_bytes());
    body[26..30].copy_from_slice(&open.to_le_bytes());
    body[30..34].copy_from_slice(&close.to_le_bytes());
    body[34..38].copy_from_slice(&high.to_le_bytes());
    body[38..42].copy_from_slice(&low.to_le_bytes());

    let frame = build_raw_frame(RESPONSE_CODE_QUOTE, EXCHANGE_SEGMENT_NSE_FNO, 52432, &body);

    let result = dispatch_frame(&frame, 0).unwrap();
    match result {
        tickvault_core::parser::types::ParsedFrame::Tick(tick) => {
            assert_eq!(tick.security_id, 52432);
            assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(tick.last_traded_price, ltp);
            assert_eq!(tick.last_trade_quantity, ltq);
            assert_eq!(tick.exchange_timestamp, ltt);
            assert_eq!(tick.average_traded_price, atp);
            assert_eq!(tick.volume, volume);
            assert_eq!(tick.total_sell_quantity, total_sell);
            assert_eq!(tick.total_buy_quantity, total_buy);
            assert_eq!(tick.day_open, open);
            assert_eq!(tick.day_close, close);
            assert_eq!(tick.day_high, high);
            assert_eq!(tick.day_low, low);
            // Quote doesn't have OI
            assert_eq!(tick.open_interest, 0);
        }
        other => panic!("expected Tick, got {other:?}"),
    }
}

#[test]
fn test_e2e_full_packet_with_depth() {
    let ltp: f32 = 24567.85;
    let ltq: u16 = 150;
    let ltt: u32 = 1741608945;
    let atp: f32 = 24520.3;
    let volume: u32 = 8_500_000;
    let total_sell: u32 = 2_100_000;
    let total_buy: u32 = 3_200_000;
    let oi: u32 = 150_000;
    let oi_high: u32 = 160_000;
    let oi_low: u32 = 140_000;
    let open: f32 = 24450.0;
    let close: f32 = 24380.5;
    let high: f32 = 24590.0;
    let low: f32 = 24410.0;

    let mut body = [0u8; 154]; // full body = 162 - 8 header
    // Shared with quote (offsets 0-25 in body, 8-33 in packet)
    body[0..4].copy_from_slice(&ltp.to_le_bytes());
    body[4..6].copy_from_slice(&ltq.to_le_bytes());
    body[6..10].copy_from_slice(&ltt.to_le_bytes());
    body[10..14].copy_from_slice(&atp.to_le_bytes());
    body[14..18].copy_from_slice(&volume.to_le_bytes());
    body[18..22].copy_from_slice(&total_sell.to_le_bytes());
    body[22..26].copy_from_slice(&total_buy.to_le_bytes());
    // Full-specific: OI at body offsets 26-37 (packet 34-45)
    body[26..30].copy_from_slice(&oi.to_le_bytes());
    body[30..34].copy_from_slice(&oi_high.to_le_bytes());
    body[34..38].copy_from_slice(&oi_low.to_le_bytes());
    // Full-specific: OHLC at body offsets 38-53 (packet 46-61)
    body[38..42].copy_from_slice(&open.to_le_bytes());
    body[42..46].copy_from_slice(&close.to_le_bytes());
    body[46..50].copy_from_slice(&high.to_le_bytes());
    body[50..54].copy_from_slice(&low.to_le_bytes());
    // Depth at body offsets 54-153 (packet 62-161): 5 levels × 20 bytes
    let depth_data: [(u32, u32, u16, u16, f32, f32); 5] = [
        (1000, 800, 42, 35, 24565.0, 24570.0),
        (750, 650, 30, 28, 24560.0, 24575.0),
        (500, 400, 20, 18, 24555.0, 24580.0),
        (300, 250, 12, 10, 24550.0, 24585.0),
        (100, 80, 5, 4, 24545.0, 24590.0),
    ];
    for (i, (bq, aq, bo, ao, bp, ap)) in depth_data.iter().enumerate() {
        let base = 54 + i * 20;
        body[base..base + 4].copy_from_slice(&bq.to_le_bytes());
        body[base + 4..base + 8].copy_from_slice(&aq.to_le_bytes());
        body[base + 8..base + 10].copy_from_slice(&bo.to_le_bytes());
        body[base + 10..base + 12].copy_from_slice(&ao.to_le_bytes());
        body[base + 12..base + 16].copy_from_slice(&bp.to_le_bytes());
        body[base + 16..base + 20].copy_from_slice(&ap.to_le_bytes());
    }

    let frame = build_raw_frame(RESPONSE_CODE_FULL, EXCHANGE_SEGMENT_NSE_FNO, 52432, &body);
    assert_eq!(
        frame.len(),
        FULL_QUOTE_PACKET_SIZE,
        "frame must be exactly 162 bytes"
    );

    let result = dispatch_frame(&frame, 0).unwrap();
    match result {
        tickvault_core::parser::types::ParsedFrame::TickWithDepth(tick, depth) => {
            assert_eq!(tick.security_id, 52432);
            assert_eq!(tick.last_traded_price, ltp);
            assert_eq!(tick.open_interest, oi);
            assert_eq!(tick.oi_day_high, oi_high);
            assert_eq!(tick.oi_day_low, oi_low);
            assert_eq!(tick.day_open, open);
            assert_eq!(tick.day_close, close);
            assert_eq!(tick.day_high, high);
            assert_eq!(tick.day_low, low);

            // Verify all 5 depth levels
            for (i, (bq, aq, bo, ao, bp, ap)) in depth_data.iter().enumerate() {
                assert_eq!(depth[i].bid_quantity, *bq, "depth[{i}].bid_qty");
                assert_eq!(depth[i].ask_quantity, *aq, "depth[{i}].ask_qty");
                assert_eq!(depth[i].bid_orders, *bo, "depth[{i}].bid_orders");
                assert_eq!(depth[i].ask_orders, *ao, "depth[{i}].ask_orders");
                assert!(
                    (depth[i].bid_price - bp).abs() < 0.01,
                    "depth[{i}].bid_price"
                );
                assert!(
                    (depth[i].ask_price - ap).abs() < 0.01,
                    "depth[{i}].ask_price"
                );
            }
        }
        other => panic!("expected TickWithDepth, got {other:?}"),
    }
}

#[test]
fn test_e2e_oi_packet() {
    let oi_value: u32 = 12_345_678;
    let mut body = [0u8; 4];
    body[0..4].copy_from_slice(&oi_value.to_le_bytes());

    let frame = build_raw_frame(RESPONSE_CODE_OI, EXCHANGE_SEGMENT_NSE_FNO, 52432, &body);

    let result = dispatch_frame(&frame, 0).unwrap();
    match result {
        tickvault_core::parser::types::ParsedFrame::OiUpdate {
            security_id,
            exchange_segment_code,
            open_interest,
        } => {
            assert_eq!(security_id, 52432);
            assert_eq!(exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(open_interest, oi_value);
        }
        other => panic!("expected OiUpdate, got {other:?}"),
    }
}

#[test]
fn test_e2e_previous_close_packet() {
    let prev_close: f32 = 24300.5;
    let prev_oi: u32 = 120_000;
    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&prev_close.to_le_bytes());
    body[4..8].copy_from_slice(&prev_oi.to_le_bytes());

    let frame = build_raw_frame(
        RESPONSE_CODE_PREVIOUS_CLOSE,
        EXCHANGE_SEGMENT_NSE_FNO,
        52432,
        &body,
    );

    let result = dispatch_frame(&frame, 0).unwrap();
    match result {
        tickvault_core::parser::types::ParsedFrame::PreviousClose {
            security_id,
            exchange_segment_code,
            previous_close,
            previous_oi,
        } => {
            assert_eq!(security_id, 52432);
            assert_eq!(exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(previous_close, prev_close);
            assert_eq!(previous_oi, prev_oi);
        }
        other => panic!("expected PreviousClose, got {other:?}"),
    }
}

#[test]
fn test_e2e_market_status_packet() {
    let frame = build_raw_frame(RESPONSE_CODE_MARKET_STATUS, EXCHANGE_SEGMENT_NSE_EQ, 0, &[]);

    let result = dispatch_frame(&frame, 0).unwrap();
    match result {
        tickvault_core::parser::types::ParsedFrame::MarketStatus {
            security_id,
            exchange_segment_code,
        } => {
            assert_eq!(security_id, 0);
            assert_eq!(exchange_segment_code, EXCHANGE_SEGMENT_NSE_EQ);
        }
        other => panic!("expected MarketStatus, got {other:?}"),
    }
}

#[test]
fn test_e2e_disconnect_packet_all_codes() {
    let test_cases: [(u16, DisconnectCode); 7] = [
        (805, DisconnectCode::ExceededActiveConnections),
        (806, DisconnectCode::DataApiSubscriptionRequired),
        (807, DisconnectCode::AccessTokenExpired),
        (808, DisconnectCode::AuthenticationFailed),
        (809, DisconnectCode::AccessTokenInvalid),
        (810, DisconnectCode::ClientIdInvalid),
        (999, DisconnectCode::Unknown(999)),
    ];

    for (raw_code, expected) in &test_cases {
        let mut body = [0u8; 2];
        body[0..2].copy_from_slice(&raw_code.to_le_bytes());
        let frame = build_raw_frame(RESPONSE_CODE_DISCONNECT, 0, 0, &body);

        let result = dispatch_frame(&frame, 0).unwrap();
        match result {
            tickvault_core::parser::types::ParsedFrame::Disconnect(code) => {
                assert_eq!(code, *expected, "disconnect code {raw_code}");
            }
            other => panic!("expected Disconnect for code {raw_code}, got {other:?}"),
        }
    }
}

// =========================================================================
// SECTION 19: PRICE INTEGRITY — f32 PRESERVES RUPEE VALUES
// Dhan sends prices as f32 in RUPEES (not paise). No division needed.
// =========================================================================

#[test]
fn test_f32_price_no_division_needed() {
    // Real NIFTY price: ₹24,567.85
    let wire_price: f32 = 24567.85;
    // Build a ticker packet with this price
    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&wire_price.to_le_bytes());
    body[4..8].copy_from_slice(&1741608945u32.to_le_bytes());

    let frame = build_raw_frame(RESPONSE_CODE_TICKER, EXCHANGE_SEGMENT_IDX_I, 13, &body);
    let result = dispatch_frame(&frame, 0).unwrap();

    match result {
        tickvault_core::parser::types::ParsedFrame::Tick(tick) => {
            // Price must be used AS-IS from wire — no ÷100 or ÷10000
            assert_eq!(
                tick.last_traded_price, wire_price,
                "price must NOT be divided"
            );
            assert!(
                tick.last_traded_price > 24000.0,
                "price must be in rupee range"
            );
        }
        other => panic!("expected Tick, got {other:?}"),
    }
}

// =========================================================================
// SECTION 20: SUBSCRIPTION JSON FORMAT VERIFICATION
// Cross-reference: dhanhq.co/docs/v2/live-market-feed/#adding-instruments
// =========================================================================

#[test]
fn test_subscription_json_format() {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::build_subscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    let instruments = vec![
        InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
        InstrumentSubscription::new(ExchangeSegment::NseEquity, 2885),
        InstrumentSubscription::new(ExchangeSegment::NseFno, 52432),
    ];

    let messages =
        build_subscription_messages(&instruments, FeedMode::Full, SUBSCRIPTION_BATCH_SIZE);
    assert_eq!(messages.len(), 1, "3 instruments fit in one batch");

    let json: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
    assert_eq!(json["RequestCode"], 21, "Full subscribe = 21");
    assert_eq!(json["InstrumentCount"], 3);

    let list = json["InstrumentList"].as_array().unwrap();
    assert_eq!(list[0]["ExchangeSegment"], "IDX_I");
    assert_eq!(
        list[0]["SecurityId"], "13",
        "SecurityId must be STRING not int"
    );
    assert_eq!(list[1]["ExchangeSegment"], "NSE_EQ");
    assert_eq!(list[1]["SecurityId"], "2885");
    assert_eq!(list[2]["ExchangeSegment"], "NSE_FNO");
    assert_eq!(list[2]["SecurityId"], "52432");
}

#[test]
fn test_subscription_batch_splitting() {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::build_subscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    // 250 instruments should split into 3 batches (100+100+50)
    let instruments: Vec<InstrumentSubscription> = (0..250)
        .map(|i| InstrumentSubscription::new(ExchangeSegment::NseEquity, i as u32))
        .collect();

    let messages =
        build_subscription_messages(&instruments, FeedMode::Full, SUBSCRIPTION_BATCH_SIZE);
    assert_eq!(messages.len(), 3, "250 instruments → 3 batches");

    let batch1: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
    let batch2: serde_json::Value = serde_json::from_str(&messages[1]).unwrap();
    let batch3: serde_json::Value = serde_json::from_str(&messages[2]).unwrap();

    assert_eq!(batch1["InstrumentCount"], 100);
    assert_eq!(batch2["InstrumentCount"], 100);
    assert_eq!(batch3["InstrumentCount"], 50);
}

#[test]
fn test_unsubscription_uses_correct_codes() {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::build_unsubscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    let instruments = vec![InstrumentSubscription::new(
        ExchangeSegment::NseEquity,
        2885,
    )];

    // Test each feed mode → expected unsubscribe code
    for (feed_mode, expected_unsub) in [
        (FeedMode::Ticker, FEED_UNSUBSCRIBE_TICKER),
        (FeedMode::Quote, FEED_UNSUBSCRIBE_QUOTE),
        (FeedMode::Full, FEED_UNSUBSCRIBE_FULL),
    ] {
        let messages =
            build_unsubscription_messages(&instruments, feed_mode, SUBSCRIPTION_BATCH_SIZE);
        let json: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            u64::from(expected_unsub),
            "unsub code for {feed_mode:?} must be {expected_unsub}"
        );
    }
}

#[test]
fn test_disconnect_message_format() {
    use tickvault_core::websocket::subscription_builder::build_disconnect_message;

    let msg = build_disconnect_message();
    let json: serde_json::Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(
        json["RequestCode"], FEED_REQUEST_DISCONNECT as i64,
        "disconnect = 12"
    );
    // Disconnect message must not have instrument list
    assert!(
        json.get("InstrumentList").is_none()
            || json["InstrumentList"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true),
        "disconnect must have no instruments"
    );
}

// =========================================================================
// SECTION 21: SECURITY_ID IS STRING IN JSON, u32 IN BINARY
// This is a CRITICAL mapping requirement — getting this wrong = wrong data
// =========================================================================

#[test]
fn test_security_id_is_string_in_subscription_json() {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::build_subscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    let instruments = vec![InstrumentSubscription::new(
        ExchangeSegment::NseEquity,
        u32::MAX,
    )];
    let messages =
        build_subscription_messages(&instruments, FeedMode::Ticker, SUBSCRIPTION_BATCH_SIZE);
    let json: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();

    let sid = &json["InstrumentList"][0]["SecurityId"];
    assert!(
        sid.is_string(),
        "SecurityId must be a JSON STRING, got {sid:?}"
    );
    assert_eq!(sid.as_str().unwrap(), &u32::MAX.to_string());
}

// =========================================================================
// SECTION 22: PARSEDTICK TYPE SAFETY — COPY + DEFAULT
// =========================================================================

#[test]
fn test_parsed_tick_is_copy_and_default() {
    let tick = ParsedTick::default();
    let copy = tick; // Copy, not move
    assert_eq!(tick.security_id, copy.security_id);
    assert_eq!(tick.last_traded_price, copy.last_traded_price);
    assert_eq!(tick.volume, copy.volume);
}

#[test]
fn test_market_depth_level_is_copy_and_default() {
    let level = MarketDepthLevel::default();
    let copy = level;
    assert_eq!(level, copy);
    assert_eq!(level.bid_quantity, 0);
    assert_eq!(level.ask_quantity, 0);
}

// =========================================================================
// SECTION 23: READ HELPERS — BOUNDARY VALIDATION
// These are pub(super) in the parser, tested indirectly via dispatch_frame.
// =========================================================================

#[test]
fn test_read_helpers_via_ticker_exact_boundary() {
    // Build a ticker packet at exact size (16 bytes)
    let frame = build_raw_frame(RESPONSE_CODE_TICKER, 0, 42, &[0u8; 8]);
    assert_eq!(frame.len(), TICKER_PACKET_SIZE);
    let result = dispatch_frame(&frame, 0);
    assert!(result.is_ok(), "exact size must parse");
}

#[test]
fn test_read_helpers_via_ticker_one_byte_short() {
    // 15 bytes — one short of ticker requirement
    let frame = &[0u8; 15];
    let mut frame_with_code = frame.to_vec();
    frame_with_code[0] = RESPONSE_CODE_TICKER;
    let result = dispatch_frame(&frame_with_code, 0);
    assert!(result.is_err(), "one byte short must fail");
}

#[test]
fn test_read_helpers_via_quote_one_byte_short() {
    let mut frame = vec![0u8; 49];
    frame[0] = RESPONSE_CODE_QUOTE;
    frame[1..3].copy_from_slice(&49u16.to_le_bytes());
    frame[3] = 2;
    frame[4..8].copy_from_slice(&42u32.to_le_bytes());
    let result = dispatch_frame(&frame, 0);
    assert!(result.is_err(), "quote needs 50 bytes, got 49");
}

#[test]
fn test_read_helpers_via_full_one_byte_short() {
    let mut frame = vec![0u8; 161];
    frame[0] = RESPONSE_CODE_FULL;
    frame[1..3].copy_from_slice(&161u16.to_le_bytes());
    frame[3] = 2;
    frame[4..8].copy_from_slice(&42u32.to_le_bytes());
    let result = dispatch_frame(&frame, 0);
    assert!(result.is_err(), "full needs 162 bytes, got 161");
}

// =========================================================================
// SECTION 24: ALL EXCHANGE SEGMENTS PARSE THROUGH DISPATCHER
// =========================================================================

#[test]
fn test_all_exchange_segments_parse_as_ticker() {
    let all_segments = [
        EXCHANGE_SEGMENT_IDX_I,
        EXCHANGE_SEGMENT_NSE_EQ,
        EXCHANGE_SEGMENT_NSE_FNO,
        EXCHANGE_SEGMENT_NSE_CURRENCY,
        EXCHANGE_SEGMENT_BSE_EQ,
        EXCHANGE_SEGMENT_MCX_COMM,
        EXCHANGE_SEGMENT_BSE_CURRENCY,
        EXCHANGE_SEGMENT_BSE_FNO,
    ];

    for seg in &all_segments {
        let frame = build_raw_frame(RESPONSE_CODE_TICKER, *seg, 42, &[0u8; 8]);
        let result = dispatch_frame(&frame, 0);
        assert!(result.is_ok(), "segment {seg} must parse as ticker");

        match result.unwrap() {
            tickvault_core::parser::types::ParsedFrame::Tick(tick) => {
                assert_eq!(tick.exchange_segment_code, *seg, "segment code preserved");
            }
            other => panic!("expected Tick for segment {seg}, got {other:?}"),
        }
    }
}

// =========================================================================
// SECTION 25: UNKNOWN RESPONSE CODES REJECTED
// =========================================================================

#[test]
fn test_unknown_response_codes_rejected() {
    // Test all values not in {1,2,3,4,5,6,7,8,50}
    let valid_codes = [1u8, 2, 3, 4, 5, 6, 7, 8, 50];
    for code in 0..=255u8 {
        if valid_codes.contains(&code) {
            continue;
        }
        let frame = build_raw_frame(code, 0, 0, &[0u8; 200]); // oversized body to avoid size errors
        let result = dispatch_frame(&frame, 0);
        assert!(
            result.is_err(),
            "response code {code} must be rejected as unknown"
        );
    }
}

// =========================================================================
// SECTION 26: OI OFFSET VERIFICATION
// =========================================================================

#[test]
fn test_oi_offset_match() {
    assert_eq!(OI_OFFSET_VALUE, 8, "OI value at byte 8 (after header)");
}

#[test]
fn test_prev_close_offsets_match() {
    assert_eq!(PREV_CLOSE_OFFSET_PRICE, 8, "prev close price at byte 8");
    assert_eq!(PREV_CLOSE_OFFSET_OI, 12, "prev OI at byte 12");
}

#[test]
fn test_disconnect_offset_match() {
    assert_eq!(DISCONNECT_OFFSET_CODE, 8, "disconnect code at byte 8");
}

// =========================================================================
// SECTION 27: MECHANICAL ENFORCEMENT — STRUCT SIZE INVARIANTS
// ParsedTick must remain Copy for zero-alloc hot path.
// =========================================================================

#[test]
fn test_parsed_tick_size_is_known() {
    let size = std::mem::size_of::<ParsedTick>();
    // Expected: 22 fields (17 original + 5 Greeks f64), mostly u32/f32 + i64 + 5×f64 + u16 + u8
    // = (4*13) + 8 + 2 + 1 + (5*8) + padding = ~107-120 bytes (depends on alignment)
    assert!(
        size <= 120,
        "ParsedTick must be compact: actual {size} bytes"
    );
    assert!(
        size >= 96,
        "ParsedTick has at least 96 bytes of data: actual {size}"
    );
}

#[test]
fn test_market_depth_level_size_is_known() {
    let size = std::mem::size_of::<MarketDepthLevel>();
    // 2 × u32 + 2 × u16 + 2 × f32 = 8 + 4 + 8 = 20 + padding
    assert!(
        size <= 24,
        "MarketDepthLevel must be compact: actual {size} bytes"
    );
}

#[test]
fn test_parsed_tick_implements_required_traits() {
    fn assert_copy<T: Copy>() {}
    fn assert_clone<T: Clone>() {}
    fn assert_debug<T: std::fmt::Debug>() {}
    assert_copy::<ParsedTick>();
    assert_clone::<ParsedTick>();
    assert_debug::<ParsedTick>();
}

#[test]
fn test_market_depth_level_implements_required_traits() {
    fn assert_copy<T: Copy>() {}
    fn assert_default<T: Default>() {}
    fn assert_partial_eq<T: PartialEq>() {}
    assert_copy::<MarketDepthLevel>();
    assert_default::<MarketDepthLevel>();
    assert_partial_eq::<MarketDepthLevel>();
}

// =========================================================================
// SECTION 28: ENDIANNESS VERIFICATION — ALL FIELDS ARE LITTLE ENDIAN
// =========================================================================

#[test]
fn test_all_fields_are_little_endian() {
    // Build a frame with known byte pattern and verify LE parsing
    // Security ID = 0x01020304 → LE bytes: [04, 03, 02, 01]
    let security_id: u32 = 0x01020304;
    let ltp: f32 = 1234.5;

    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&ltp.to_le_bytes());
    body[4..8].copy_from_slice(&100u32.to_le_bytes());

    let frame = build_raw_frame(RESPONSE_CODE_TICKER, 2, security_id, &body);

    // Verify raw LE byte layout
    assert_eq!(frame[4], 0x04, "security_id byte 0 = LSB = 0x04");
    assert_eq!(frame[5], 0x03, "security_id byte 1 = 0x03");
    assert_eq!(frame[6], 0x02, "security_id byte 2 = 0x02");
    assert_eq!(frame[7], 0x01, "security_id byte 3 = MSB = 0x01");

    let result = dispatch_frame(&frame, 0).unwrap();
    match result {
        tickvault_core::parser::types::ParsedFrame::Tick(tick) => {
            assert_eq!(tick.security_id, security_id, "LE parsed correctly");
        }
        other => panic!("expected Tick, got {other:?}"),
    }
}

// =========================================================================
// SECTION 29: SUBSCRIPTION BUILDER — EDGE CASES
// =========================================================================

#[test]
fn test_subscription_empty_instruments() {
    use tickvault_common::types::FeedMode;
    use tickvault_core::websocket::subscription_builder::build_subscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    let instruments: Vec<InstrumentSubscription> = vec![];
    let messages =
        build_subscription_messages(&instruments, FeedMode::Full, SUBSCRIPTION_BATCH_SIZE);
    assert!(messages.is_empty(), "no instruments = no messages");
}

#[test]
fn test_subscription_exactly_100_instruments_one_batch() {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::build_subscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    let instruments: Vec<InstrumentSubscription> = (0..100)
        .map(|i| InstrumentSubscription::new(ExchangeSegment::NseEquity, i as u32))
        .collect();

    let messages =
        build_subscription_messages(&instruments, FeedMode::Full, SUBSCRIPTION_BATCH_SIZE);
    assert_eq!(messages.len(), 1, "exactly 100 = 1 batch");
}

#[test]
fn test_subscription_101_instruments_two_batches() {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::build_subscription_messages;
    use tickvault_core::websocket::types::InstrumentSubscription;

    let instruments: Vec<InstrumentSubscription> = (0..101)
        .map(|i| InstrumentSubscription::new(ExchangeSegment::NseEquity, i as u32))
        .collect();

    let messages =
        build_subscription_messages(&instruments, FeedMode::Full, SUBSCRIPTION_BATCH_SIZE);
    assert_eq!(messages.len(), 2, "101 = 2 batches (100+1)");
}

// =========================================================================
// SECTION 30: CONNECTION POOL DISTRIBUTION
// =========================================================================

#[test]
fn test_connection_pool_capacity_check() {
    // 25001 instruments exceeds MAX_TOTAL_SUBSCRIPTIONS (25000)
    // The pool constructor should reject this
    // Note: We can't easily test this without a full setup, so we verify constants
    assert_eq!(MAX_TOTAL_SUBSCRIPTIONS, 25000);
    const {
        assert!(
            MAX_TOTAL_SUBSCRIPTIONS
                == MAX_WEBSOCKET_CONNECTIONS * MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION,
        );
    }
}
