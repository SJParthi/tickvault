//! Deep depth parser for 20-level and 200-level market depth feeds.
//!
//! These feeds use a SEPARATE WebSocket endpoint from the standard feed:
//! - 20-depth: `wss://depth-api-feed.dhan.co/twentydepth`
//! - 200-depth: `wss://full-depth-api.dhan.co/` (matches Dhan API (Python SDK ref))
//!
//! # Protocol differences from standard feed
//! - 12-byte header (vs 8-byte standard feed header)
//! - Bid and ask sides arrive as SEPARATE binary packets (feed codes 41/51)
//! - Prices are f64 (vs f32 in standard feed)
//! - Per-level format: price(f64) + quantity(u32) + orders(u32) = 16 bytes
//!
//! # Performance
//! All parsing is O(1) — fixed number of reads per packet.

use tickvault_common::constants::{
    DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID, DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT,
    DEEP_DEPTH_HEADER_OFFSET_FEED_CODE, DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH,
    DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE, DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID,
    DEEP_DEPTH_HEADER_SIZE, DEEP_DEPTH_LEVEL_OFFSET_ORDERS, DEEP_DEPTH_LEVEL_OFFSET_PRICE,
    DEEP_DEPTH_LEVEL_OFFSET_QUANTITY, DEEP_DEPTH_LEVEL_SIZE, TWENTY_DEPTH_LEVELS,
    TWENTY_DEPTH_PACKET_SIZE, TWO_HUNDRED_DEPTH_LEVELS,
};
use tickvault_common::tick_types::DeepDepthLevel;

use super::read_helpers::{read_f64_le, read_u16_le, read_u32_le};
use super::types::ParseError;

// ---------------------------------------------------------------------------
// Deep Depth Header
// ---------------------------------------------------------------------------

/// Parsed header from a 20-level or 200-level depth binary packet.
///
/// 12 bytes: msg_length(u16) + feed_code(u8) + exchange_segment(u8) + security_id(u32) + seq_or_row_count(u32).
#[derive(Debug, Clone, Copy)]
pub struct DeepDepthHeader {
    /// Total message length in bytes.
    pub message_length: u16,
    /// Feed response code (41 = Bid, 51 = Ask).
    pub feed_code: u8,
    /// Binary exchange segment code (1=NSE_EQ, 2=NSE_FNO, etc.).
    pub exchange_segment_code: u8,
    /// Dhan security identifier.
    pub security_id: u32,
    /// Dual-purpose field (bytes 8-11):
    /// - 20-level depth: sequence number (informational, for ordering/gap detection)
    /// - 200-level depth: row count (number of levels with actual data, determines packet size)
    pub seq_or_row_count: u32,
}

/// Which side of the order book this packet represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthSide {
    /// Bid (buy) side — feed code 41.
    Bid,
    /// Ask (sell) side — feed code 51.
    Ask,
}

// ---------------------------------------------------------------------------
// Header parser
// ---------------------------------------------------------------------------

/// Parses the 12-byte deep depth header.
///
/// # Errors
/// Returns `ParseError::InsufficientBytes` if `raw.len() < 12`.
pub fn parse_deep_depth_header(raw: &[u8]) -> Result<DeepDepthHeader, ParseError> {
    if raw.len() < DEEP_DEPTH_HEADER_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: DEEP_DEPTH_HEADER_SIZE,
            actual: raw.len(),
        });
    }

    Ok(DeepDepthHeader {
        message_length: read_u16_le(raw, DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH),
        feed_code: raw[DEEP_DEPTH_HEADER_OFFSET_FEED_CODE],
        exchange_segment_code: raw[DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT],
        security_id: read_u32_le(raw, DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID),
        seq_or_row_count: read_u32_le(raw, DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE),
    })
}

/// Determines the depth side from the feed code.
///
/// # Errors
/// Returns `ParseError::UnknownResponseCode` if feed_code is neither 41 nor 51.
pub fn feed_code_to_side(feed_code: u8) -> Result<DepthSide, ParseError> {
    match feed_code {
        DEEP_DEPTH_FEED_CODE_BID => Ok(DepthSide::Bid),
        DEEP_DEPTH_FEED_CODE_ASK => Ok(DepthSide::Ask),
        code => Err(ParseError::UnknownResponseCode(code)),
    }
}

// ---------------------------------------------------------------------------
// Level parsers
// ---------------------------------------------------------------------------

/// Parses N levels of deep depth from a byte slice starting after the header.
///
/// Each level is 16 bytes: price(f64 LE) + quantity(u32 LE) + orders(u32 LE).
///
/// # Arguments
/// * `raw` — Full packet bytes (including header).
/// * `level_count` — Number of levels to parse (20 or 200).
///
/// # Returns
/// Vec of `DeepDepthLevel`. Not Copy-based array due to variable size (20 vs 200).
///
/// # Performance
/// O(N) where N is level_count — fixed, known at call site.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by packet size check
fn parse_deep_depth_levels(
    raw: &[u8],
    level_count: usize,
    expected_packet_size: usize,
) -> Result<Vec<DeepDepthLevel>, ParseError> {
    // O(1) EXEMPT: begin — allocation for depth levels, bounded by level_count (20 or 200)
    if raw.len() < expected_packet_size {
        return Err(ParseError::InsufficientBytes {
            expected: expected_packet_size,
            actual: raw.len(),
        });
    }

    let mut levels = Vec::with_capacity(level_count);
    for i in 0..level_count {
        let base = DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE;
        levels.push(DeepDepthLevel {
            price: read_f64_le(raw, base + DEEP_DEPTH_LEVEL_OFFSET_PRICE),
            quantity: read_u32_le(raw, base + DEEP_DEPTH_LEVEL_OFFSET_QUANTITY),
            orders: read_u32_le(raw, base + DEEP_DEPTH_LEVEL_OFFSET_ORDERS),
        });
    }

    Ok(levels)
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Public parsing functions
// ---------------------------------------------------------------------------

/// Parsed result from a deep depth packet (one side: bid or ask).
#[derive(Debug)]
pub struct ParsedDeepDepth {
    /// Parsed 12-byte header.
    pub header: DeepDepthHeader,
    /// Which side this packet represents (Bid or Ask).
    pub side: DepthSide,
    /// Depth levels for this side.
    pub levels: Vec<DeepDepthLevel>,
    /// Local receive timestamp in nanoseconds since Unix epoch.
    pub received_at_nanos: i64,
}

/// Parses a 20-level depth packet (one side: bid or ask).
///
/// Expected packet size: 332 bytes (12 header + 20 × 16 body).
///
/// # Performance
/// O(1) — fixed 20 level reads.
pub fn parse_twenty_depth_packet(
    raw: &[u8],
    received_at_nanos: i64,
) -> Result<ParsedDeepDepth, ParseError> {
    let header = parse_deep_depth_header(raw)?;
    let side = feed_code_to_side(header.feed_code)?;
    let levels = parse_deep_depth_levels(raw, TWENTY_DEPTH_LEVELS, TWENTY_DEPTH_PACKET_SIZE)?;

    Ok(ParsedDeepDepth {
        header,
        side,
        levels,
        received_at_nanos,
    })
}

/// Parses a 200-level depth packet (one side: bid or ask).
///
/// Unlike 20-level packets which are fixed-size (332 bytes), 200-level packets
/// are variable-size. The header's `seq_or_row_count` field (bytes 8-11) contains
/// the actual number of levels with data. The packet size is:
/// `12 + (row_count × 16)`, up to a maximum of 3212 bytes (200 levels).
///
/// # Performance
/// O(N) where N = row_count (at most 200).
#[allow(clippy::arithmetic_side_effects)] // APPROVED: row_count bounded by TWO_HUNDRED_DEPTH_LEVELS check
pub fn parse_two_hundred_depth_packet(
    raw: &[u8],
    received_at_nanos: i64,
) -> Result<ParsedDeepDepth, ParseError> {
    let header = parse_deep_depth_header(raw)?;
    let side = feed_code_to_side(header.feed_code)?;

    // For 200-level depth, bytes 8-11 = row count (not sequence number).
    let row_count = header.seq_or_row_count;
    if row_count as usize > TWO_HUNDRED_DEPTH_LEVELS {
        return Err(ParseError::InvalidRowCount {
            actual: row_count,
            max: TWO_HUNDRED_DEPTH_LEVELS,
        });
    }

    let row_count_usize = row_count as usize;
    let expected_size = DEEP_DEPTH_HEADER_SIZE + row_count_usize * DEEP_DEPTH_LEVEL_SIZE;
    let levels = parse_deep_depth_levels(raw, row_count_usize, expected_size)?;

    Ok(ParsedDeepDepth {
        header,
        side,
        levels,
        received_at_nanos,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction
mod tests {
    use super::*;
    use tickvault_common::constants::TWO_HUNDRED_DEPTH_PACKET_SIZE;

    /// Builds a deep depth packet for testing.
    fn make_deep_depth_packet(
        feed_code: u8,
        exchange_segment: u8,
        security_id: u32,
        msg_sequence: u32,
        level_count: usize,
        level_data: Option<&[(f64, u32, u32)]>,
    ) -> Vec<u8> {
        let packet_size = DEEP_DEPTH_HEADER_SIZE + level_count * DEEP_DEPTH_LEVEL_SIZE;
        let mut buf = vec![0u8; packet_size];

        // Header
        buf[0..2].copy_from_slice(&(packet_size as u16).to_le_bytes());
        buf[2] = feed_code;
        buf[3] = exchange_segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&msg_sequence.to_le_bytes());

        // Levels
        if let Some(data) = level_data {
            for (i, (price, qty, orders)) in data.iter().enumerate() {
                if i >= level_count {
                    break;
                }
                let base = DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE;
                buf[base..base + 8].copy_from_slice(&price.to_le_bytes());
                buf[base + 8..base + 12].copy_from_slice(&qty.to_le_bytes());
                buf[base + 12..base + 16].copy_from_slice(&orders.to_le_bytes());
            }
        }

        buf
    }

    // --- Header parsing ---

    #[test]
    fn test_parse_deep_depth_header_valid() {
        let buf = make_deep_depth_packet(41, 2, 52432, 100, 20, None);
        let header = parse_deep_depth_header(&buf).unwrap();
        assert_eq!(header.feed_code, 41);
        assert_eq!(header.exchange_segment_code, 2);
        assert_eq!(header.security_id, 52432);
        assert_eq!(header.seq_or_row_count, 100);
    }

    #[test]
    fn test_parse_deep_depth_header_too_short() {
        let buf = [0u8; 11]; // One byte short
        let err = parse_deep_depth_header(&buf).unwrap_err();
        match err {
            ParseError::InsufficientBytes {
                expected: 12,
                actual: 11,
            } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_parse_deep_depth_header_empty() {
        let err = parse_deep_depth_header(&[]).unwrap_err();
        match err {
            ParseError::InsufficientBytes {
                expected: 12,
                actual: 0,
            } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // --- Feed code → side ---

    #[test]
    fn test_feed_code_to_side_bid() {
        assert_eq!(feed_code_to_side(41).unwrap(), DepthSide::Bid);
    }

    #[test]
    fn test_feed_code_to_side_ask() {
        assert_eq!(feed_code_to_side(51).unwrap(), DepthSide::Ask);
    }

    #[test]
    fn test_feed_code_to_side_unknown() {
        let err = feed_code_to_side(99).unwrap_err();
        assert!(matches!(err, ParseError::UnknownResponseCode(99)));
    }

    // --- 20-depth parsing ---

    #[test]
    fn test_parse_twenty_depth_bid_packet() {
        let level_data: Vec<(f64, u32, u32)> = (0..20)
            .map(|i| {
                let i_f64 = f64::from(i);
                (
                    24500.0 - i_f64 * 0.05,
                    (1000 - i * 50) as u32,
                    (100 - i * 5) as u32,
                )
            })
            .collect();

        let buf = make_deep_depth_packet(41, 2, 52432, 1, 20, Some(&level_data));
        let result = parse_twenty_depth_packet(&buf, 999).unwrap();

        assert_eq!(result.side, DepthSide::Bid);
        assert_eq!(result.header.security_id, 52432);
        assert_eq!(result.header.exchange_segment_code, 2);
        assert_eq!(result.header.seq_or_row_count, 1);
        assert_eq!(result.received_at_nanos, 999);
        assert_eq!(result.levels.len(), 20);

        // Verify first level
        assert!((result.levels[0].price - 24500.0).abs() < 1e-9);
        assert_eq!(result.levels[0].quantity, 1000);
        assert_eq!(result.levels[0].orders, 100);

        // Verify last level
        assert!((result.levels[19].price - 24499.05).abs() < 1e-9);
        assert_eq!(result.levels[19].quantity, 50);
        assert_eq!(result.levels[19].orders, 5);
    }

    #[test]
    fn test_parse_twenty_depth_ask_packet() {
        let level_data: Vec<(f64, u32, u32)> = (0..20)
            .map(|i| {
                let i_f64 = f64::from(i);
                (
                    24500.05 + i_f64 * 0.05,
                    500 + i as u32 * 25,
                    50 + i as u32 * 3,
                )
            })
            .collect();

        let buf = make_deep_depth_packet(51, 2, 52432, 2, 20, Some(&level_data));
        let result = parse_twenty_depth_packet(&buf, 888).unwrap();

        assert_eq!(result.side, DepthSide::Ask);
        assert_eq!(result.levels.len(), 20);
        assert!((result.levels[0].price - 24500.05).abs() < 1e-9);
        assert_eq!(result.levels[0].quantity, 500);
    }

    #[test]
    fn test_parse_twenty_depth_truncated() {
        let mut buf = vec![0u8; TWENTY_DEPTH_PACKET_SIZE - 1];
        buf[2] = DEEP_DEPTH_FEED_CODE_BID; // Valid feed code so we hit the size check
        let err = parse_twenty_depth_packet(&buf, 0).unwrap_err();
        match err {
            ParseError::InsufficientBytes { expected, actual } => {
                assert_eq!(expected, TWENTY_DEPTH_PACKET_SIZE);
                assert_eq!(actual, TWENTY_DEPTH_PACKET_SIZE - 1);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_parse_twenty_depth_unknown_feed_code() {
        let buf = make_deep_depth_packet(99, 2, 52432, 1, 20, None);
        let err = parse_twenty_depth_packet(&buf, 0).unwrap_err();
        assert!(matches!(err, ParseError::UnknownResponseCode(99)));
    }

    #[test]
    fn test_parse_twenty_depth_zero_levels() {
        let buf = make_deep_depth_packet(41, 2, 52432, 1, 20, None);
        let result = parse_twenty_depth_packet(&buf, 0).unwrap();
        for level in &result.levels {
            assert_eq!(level.price, 0.0);
            assert_eq!(level.quantity, 0);
            assert_eq!(level.orders, 0);
        }
    }

    #[test]
    fn test_parse_twenty_depth_extra_bytes_ignored() {
        let mut buf = make_deep_depth_packet(41, 1, 2885, 5, 20, None);
        buf.extend_from_slice(&[0xFF; 100]); // extra garbage
        let result = parse_twenty_depth_packet(&buf, 0).unwrap();
        assert_eq!(result.levels.len(), 20);
        assert_eq!(result.header.security_id, 2885);
    }

    #[test]
    fn test_parse_twenty_depth_max_security_id() {
        let buf = make_deep_depth_packet(41, 2, u32::MAX, 1, 20, None);
        let result = parse_twenty_depth_packet(&buf, 0).unwrap();
        assert_eq!(result.header.security_id, u32::MAX);
    }

    #[test]
    fn test_parse_twenty_depth_nan_price() {
        let level_data = [(f64::NAN, 100, 10)];
        let buf = make_deep_depth_packet(41, 2, 1, 1, 20, Some(&level_data));
        let result = parse_twenty_depth_packet(&buf, 0).unwrap();
        assert!(result.levels[0].price.is_nan());
    }

    #[test]
    fn test_parse_twenty_depth_infinity_price() {
        let level_data = [(f64::INFINITY, 100, 10)];
        let buf = make_deep_depth_packet(51, 2, 1, 1, 20, Some(&level_data));
        let result = parse_twenty_depth_packet(&buf, 0).unwrap();
        assert!(result.levels[0].price.is_infinite());
    }

    // --- 200-depth parsing ---

    #[test]
    fn test_parse_two_hundred_depth_bid_packet_full() {
        let level_data: Vec<(f64, u32, u32)> = (0..200)
            .map(|i| {
                let i_f64 = f64::from(i);
                (24500.0 - i_f64 * 0.05, 100 + i as u32, 10 + i as u32)
            })
            .collect();

        // row_count=200 means all 200 levels have data
        let buf = make_deep_depth_packet(41, 2, 13, 200, 200, Some(&level_data));
        let result = parse_two_hundred_depth_packet(&buf, 777).unwrap();

        assert_eq!(result.side, DepthSide::Bid);
        assert_eq!(result.levels.len(), 200);
        assert!((result.levels[0].price - 24500.0).abs() < 1e-9);
        assert_eq!(result.levels[0].quantity, 100);
        assert_eq!(result.levels[0].orders, 10);
        assert_eq!(result.received_at_nanos, 777);

        // Verify level 199
        assert!((result.levels[199].price - (24500.0 - 199.0 * 0.05)).abs() < 1e-9);
        assert_eq!(result.levels[199].quantity, 299);
        assert_eq!(result.levels[199].orders, 209);
    }

    #[test]
    fn test_parse_two_hundred_depth_partial_row_count() {
        // Simulate a 200-level packet where only 42 levels have data
        let level_data: Vec<(f64, u32, u32)> = (0..42)
            .map(|i| {
                let i_f64 = f64::from(i);
                (24500.0 - i_f64 * 0.05, 100 + i as u32, 10 + i as u32)
            })
            .collect();

        // Build a buffer large enough for 42 levels, row_count=42
        let buf = make_deep_depth_packet(51, 1, 2885, 42, 42, Some(&level_data));
        let result = parse_two_hundred_depth_packet(&buf, 0).unwrap();

        assert_eq!(result.side, DepthSide::Ask);
        assert_eq!(result.levels.len(), 42);
        assert_eq!(result.header.security_id, 2885);
        assert_eq!(result.header.seq_or_row_count, 42);
        assert!((result.levels[0].price - 24500.0).abs() < 1e-9);
        assert!((result.levels[41].price - (24500.0 - 41.0 * 0.05)).abs() < 1e-9);
    }

    #[test]
    fn test_parse_two_hundred_depth_zero_row_count() {
        // row_count=0 → no levels, only header required
        let buf = make_deep_depth_packet(41, 2, 13, 0, 0, None);
        let result = parse_two_hundred_depth_packet(&buf, 0).unwrap();
        assert_eq!(result.levels.len(), 0);
    }

    #[test]
    fn test_parse_two_hundred_depth_row_count_exceeds_max() {
        // Build a minimal buffer with row_count=201 in header
        let mut buf = vec![0u8; DEEP_DEPTH_HEADER_SIZE];
        buf[0..2].copy_from_slice(&(DEEP_DEPTH_HEADER_SIZE as u16).to_le_bytes());
        buf[2] = DEEP_DEPTH_FEED_CODE_BID;
        buf[3] = 2;
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&201u32.to_le_bytes()); // row_count > 200
        let err = parse_two_hundred_depth_packet(&buf, 0).unwrap_err();
        match err {
            ParseError::InvalidRowCount { actual, max } => {
                assert_eq!(actual, 201);
                assert_eq!(max, TWO_HUNDRED_DEPTH_LEVELS);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_parse_two_hundred_depth_truncated() {
        // row_count=100 but buffer is too short for 100 levels
        let expected_size = DEEP_DEPTH_HEADER_SIZE + 100 * DEEP_DEPTH_LEVEL_SIZE;
        let short_size = expected_size - 1;
        let mut buf = vec![0u8; short_size];
        buf[0..2].copy_from_slice(&(short_size as u16).to_le_bytes());
        buf[2] = DEEP_DEPTH_FEED_CODE_ASK;
        buf[3] = 2;
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&100u32.to_le_bytes()); // row_count=100
        let err = parse_two_hundred_depth_packet(&buf, 0).unwrap_err();
        match err {
            ParseError::InsufficientBytes { expected, actual } => {
                assert_eq!(expected, expected_size);
                assert_eq!(actual, short_size);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_parse_two_hundred_depth_unknown_feed_code() {
        let buf = make_deep_depth_packet(0, 2, 1, 200, 200, None);
        let err = parse_two_hundred_depth_packet(&buf, 0).unwrap_err();
        assert!(matches!(err, ParseError::UnknownResponseCode(0)));
    }

    #[test]
    fn test_parse_two_hundred_depth_packet_size_constants() {
        assert_eq!(TWENTY_DEPTH_PACKET_SIZE, 12 + 20 * 16);
        assert_eq!(TWO_HUNDRED_DEPTH_PACKET_SIZE, 12 + 200 * 16);
    }

    // --- Stacked packets ---

    #[test]
    fn test_stacked_twenty_depth_bid_ask_parsing() {
        let bid_buf = make_deep_depth_packet(41, 2, 52432, 1, 20, None);
        let ask_buf = make_deep_depth_packet(51, 2, 52432, 2, 20, None);

        // Simulate stacked packets in one WS message
        let mut stacked = Vec::with_capacity(bid_buf.len() + ask_buf.len());
        stacked.extend_from_slice(&bid_buf);
        stacked.extend_from_slice(&ask_buf);

        // Parse first packet (bid)
        let bid_result =
            parse_twenty_depth_packet(&stacked[..TWENTY_DEPTH_PACKET_SIZE], 0).unwrap();
        assert_eq!(bid_result.side, DepthSide::Bid);
        assert_eq!(bid_result.header.security_id, 52432);

        // Parse second packet (ask)
        let ask_result =
            parse_twenty_depth_packet(&stacked[TWENTY_DEPTH_PACKET_SIZE..], 0).unwrap();
        assert_eq!(ask_result.side, DepthSide::Ask);
        assert_eq!(ask_result.header.security_id, 52432);
    }

    // --- Header field edge cases ---

    #[test]
    fn test_deep_depth_header_all_exchange_segments() {
        for segment in 0..=8 {
            let buf = make_deep_depth_packet(41, segment, 1, 1, 20, None);
            let header = parse_deep_depth_header(&buf).unwrap();
            assert_eq!(header.exchange_segment_code, segment);
        }
    }

    #[test]
    fn test_deep_depth_header_max_msg_sequence() {
        let buf = make_deep_depth_packet(51, 2, 1, u32::MAX, 20, None);
        let header = parse_deep_depth_header(&buf).unwrap();
        assert_eq!(header.seq_or_row_count, u32::MAX);
    }

    // =====================================================================
    // Additional coverage: DepthSide Debug/PartialEq, 200-level edge cases,
    // header message_length, feed_code_to_side boundary
    // =====================================================================

    #[test]
    fn test_depth_side_debug_and_eq() {
        assert_eq!(DepthSide::Bid, DepthSide::Bid);
        assert_eq!(DepthSide::Ask, DepthSide::Ask);
        assert_ne!(DepthSide::Bid, DepthSide::Ask);
        let debug_bid = format!("{:?}", DepthSide::Bid);
        assert_eq!(debug_bid, "Bid");
        let debug_ask = format!("{:?}", DepthSide::Ask);
        assert_eq!(debug_ask, "Ask");
    }

    #[test]
    fn test_depth_side_clone_copy() {
        let side = DepthSide::Bid;
        let cloned = side;
        assert_eq!(side, cloned);
    }

    #[test]
    fn test_feed_code_to_side_zero() {
        let err = feed_code_to_side(0).unwrap_err();
        assert!(matches!(err, ParseError::UnknownResponseCode(0)));
    }

    #[test]
    fn test_feed_code_to_side_adjacent_codes() {
        // 40 and 42 are NOT valid feed codes (only 41 and 51)
        assert!(feed_code_to_side(40).is_err());
        assert!(feed_code_to_side(42).is_err());
        assert!(feed_code_to_side(50).is_err());
        assert!(feed_code_to_side(52).is_err());
    }

    #[test]
    fn test_parse_two_hundred_depth_exactly_200_levels() {
        let buf = make_deep_depth_packet(41, 2, 13, 200, 200, None);
        let result = parse_two_hundred_depth_packet(&buf, 0).unwrap();
        assert_eq!(result.levels.len(), 200);
        assert_eq!(result.header.seq_or_row_count, 200);
    }

    #[test]
    fn test_parse_two_hundred_depth_single_level() {
        let level_data = [(24500.05, 500_u32, 42_u32)];
        let buf = make_deep_depth_packet(51, 1, 2885, 1, 1, Some(&level_data));
        let result = parse_two_hundred_depth_packet(&buf, 123).unwrap();
        assert_eq!(result.levels.len(), 1);
        assert!((result.levels[0].price - 24500.05).abs() < 1e-9);
        assert_eq!(result.levels[0].quantity, 500);
        assert_eq!(result.levels[0].orders, 42);
        assert_eq!(result.received_at_nanos, 123);
    }

    #[test]
    fn test_parse_deep_depth_header_exactly_12_bytes() {
        let mut buf = vec![0u8; 12];
        buf[0..2].copy_from_slice(&332u16.to_le_bytes());
        buf[2] = DEEP_DEPTH_FEED_CODE_BID;
        buf[3] = 2;
        buf[4..8].copy_from_slice(&12345u32.to_le_bytes());
        buf[8..12].copy_from_slice(&99u32.to_le_bytes());
        let header = parse_deep_depth_header(&buf).unwrap();
        assert_eq!(header.message_length, 332);
        assert_eq!(header.feed_code, DEEP_DEPTH_FEED_CODE_BID);
        assert_eq!(header.security_id, 12345);
        assert_eq!(header.seq_or_row_count, 99);
    }

    #[test]
    fn test_deep_depth_header_debug_format() {
        let buf = make_deep_depth_packet(41, 1, 2885, 5, 20, None);
        let header = parse_deep_depth_header(&buf).unwrap();
        let debug = format!("{header:?}");
        assert!(debug.contains("DeepDepthHeader"));
        assert!(debug.contains("2885"));
    }

    #[test]
    fn test_parse_two_hundred_depth_row_count_max_valid() {
        // row_count = 200 (maximum valid)
        let buf = make_deep_depth_packet(41, 2, 13, 200, 200, None);
        let result = parse_two_hundred_depth_packet(&buf, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parsed_deep_depth_debug_format() {
        let buf = make_deep_depth_packet(41, 2, 13, 1, 20, None);
        let result = parse_twenty_depth_packet(&buf, 999).unwrap();
        let debug = format!("{result:?}");
        assert!(debug.contains("ParsedDeepDepth"));
        assert!(debug.contains("Bid"));
    }
}
