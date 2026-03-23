//! Market Depth standalone packet parser (112 bytes, response code 3).
//!
//! Format: `<BHBIf100s>` — Header(8) + LTP(f32) + Depth(5×20 bytes).
//! This packet provides LTP + 5-level market depth but NO timestamp,
//! volume, OHLC, or OI data.
//!
//! Source: Dhan Python SDK `process_market_depth()` in `marketfeed.py`.

use dhan_live_trader_common::constants::{
    DEPTH_LEVEL_OFFSET_ASK_ORDERS, DEPTH_LEVEL_OFFSET_ASK_PRICE, DEPTH_LEVEL_OFFSET_ASK_QTY,
    DEPTH_LEVEL_OFFSET_BID_ORDERS, DEPTH_LEVEL_OFFSET_BID_PRICE, DEPTH_LEVEL_OFFSET_BID_QTY,
    MARKET_DEPTH_LEVEL_SIZE, MARKET_DEPTH_LEVELS, MARKET_DEPTH_OFFSET_DEPTH_START,
    MARKET_DEPTH_OFFSET_LTP, MARKET_DEPTH_PACKET_SIZE,
};
use dhan_live_trader_common::tick_types::{MarketDepthLevel, ParsedTick};

use super::read_helpers::{read_f32_le, read_u16_le, read_u32_le};
use super::types::{PacketHeader, ParseError};

/// Parses a Market Depth standalone packet into a `ParsedTick` and 5 depth levels.
///
/// Only LTP is populated in the tick — all other fields (timestamp, volume, OHLC,
/// OI) default to 0. The tick will be filtered by the pipeline's junk filter
/// since `exchange_timestamp` is 0.
///
/// # Performance
/// O(1) — one f32 read + fixed 5-iteration depth parse.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by MARKET_DEPTH_PACKET_SIZE check; depth loop is fixed 5 iterations
pub fn parse_market_depth_packet(
    raw: &[u8],
    header: &PacketHeader,
    received_at_nanos: i64,
) -> Result<(ParsedTick, [MarketDepthLevel; 5]), ParseError> {
    if raw.len() < MARKET_DEPTH_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: MARKET_DEPTH_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    let ltp = read_f32_le(raw, MARKET_DEPTH_OFFSET_LTP);

    // Depth: 5 levels × 20 bytes starting at offset 12.
    // Dhan SDK format per level: `<IIHHff>` (u32, u32, u16, u16, f32, f32).
    let mut depth = [MarketDepthLevel::default(); MARKET_DEPTH_LEVELS];
    for (i, level) in depth.iter_mut().enumerate() {
        let base = MARKET_DEPTH_OFFSET_DEPTH_START + i * MARKET_DEPTH_LEVEL_SIZE;
        *level = MarketDepthLevel {
            bid_quantity: read_u32_le(raw, base + DEPTH_LEVEL_OFFSET_BID_QTY),
            ask_quantity: read_u32_le(raw, base + DEPTH_LEVEL_OFFSET_ASK_QTY),
            bid_orders: read_u16_le(raw, base + DEPTH_LEVEL_OFFSET_BID_ORDERS),
            ask_orders: read_u16_le(raw, base + DEPTH_LEVEL_OFFSET_ASK_ORDERS),
            bid_price: read_f32_le(raw, base + DEPTH_LEVEL_OFFSET_BID_PRICE),
            ask_price: read_f32_le(raw, base + DEPTH_LEVEL_OFFSET_ASK_PRICE),
        };
    }

    let tick = ParsedTick {
        security_id: header.security_id,
        exchange_segment_code: header.exchange_segment_code,
        last_traded_price: ltp,
        received_at_nanos,
        ..Default::default()
    };

    Ok((tick, depth))
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::EXCHANGE_SEGMENT_NSE_FNO;

    fn make_market_depth_packet(
        segment: u8,
        security_id: u32,
        ltp: f32,
        depth_data: Option<[(u32, u32, u16, u16, f32, f32); 5]>,
    ) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; MARKET_DEPTH_PACKET_SIZE];
        // header
        buf[0] = 3; // RESPONSE_CODE_MARKET_DEPTH
        buf[1..3].copy_from_slice(&(MARKET_DEPTH_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        // LTP
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        // Depth
        if let Some(depth) = depth_data {
            for (i, (bq, aq, bo, ao, bp, ap)) in depth.iter().enumerate() {
                let base = 12 + i * 20;
                buf[base..base + 4].copy_from_slice(&bq.to_le_bytes());
                buf[base + 4..base + 8].copy_from_slice(&aq.to_le_bytes());
                buf[base + 8..base + 10].copy_from_slice(&bo.to_le_bytes());
                buf[base + 10..base + 12].copy_from_slice(&ao.to_le_bytes());
                buf[base + 12..base + 16].copy_from_slice(&bp.to_le_bytes());
                buf[base + 16..base + 20].copy_from_slice(&ap.to_le_bytes());
            }
        }

        let hdr = PacketHeader {
            response_code: 3,
            message_length: MARKET_DEPTH_PACKET_SIZE as u16,
            exchange_segment_code: segment,
            security_id,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_market_depth_ltp_and_header() {
        let (buf, hdr) = make_market_depth_packet(EXCHANGE_SEGMENT_NSE_FNO, 2885, 2450.5, None);
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 999).unwrap();
        assert_eq!(tick.security_id, 2885);
        assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
        assert!((tick.last_traded_price - 2450.5).abs() < 0.01);
        assert_eq!(tick.received_at_nanos, 999);
    }

    #[test]
    fn test_parse_market_depth_no_timestamp() {
        let (buf, hdr) = make_market_depth_packet(0, 13, 21004.95, None);
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.exchange_timestamp, 0);
        assert_eq!(tick.volume, 0);
        assert_eq!(tick.open_interest, 0);
    }

    #[test]
    fn test_parse_market_depth_all_depth_levels() {
        let depth_data = [
            (1000u32, 500u32, 10u16, 5u16, 24490.0f32, 24500.0f32),
            (800, 600, 8, 6, 24485.0, 24505.0),
            (600, 400, 6, 4, 24480.0, 24510.0),
            (400, 200, 4, 2, 24475.0, 24515.0),
            (200, 100, 2, 1, 24470.0, 24520.0),
        ];
        let (buf, hdr) = make_market_depth_packet(2, 13, 24500.0, Some(depth_data));
        let (_, depth) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();

        assert_eq!(depth[0].bid_quantity, 1000);
        assert_eq!(depth[0].ask_quantity, 500);
        assert_eq!(depth[0].bid_orders, 10);
        assert_eq!(depth[0].ask_orders, 5);
        assert!((depth[0].bid_price - 24490.0).abs() < 0.01);
        assert!((depth[0].ask_price - 24500.0).abs() < 0.01);

        assert_eq!(depth[4].bid_quantity, 200);
        assert_eq!(depth[4].ask_quantity, 100);
    }

    #[test]
    fn test_parse_market_depth_empty_buffer() {
        let hdr = PacketHeader {
            response_code: 3,
            message_length: 112,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_market_depth_packet(&[], &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 112,
            actual: 0,
        } = err
        else {
            panic!("wrong error variant")
        };
    }

    #[test]
    fn test_parse_market_depth_truncated() {
        let buf = vec![0u8; 111]; // one byte short
        let hdr = PacketHeader {
            response_code: 3,
            message_length: 112,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_market_depth_packet(&buf, &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes { expected, actual } = err else {
            panic!("wrong error variant")
        };
        assert_eq!(expected, 112);
        assert_eq!(actual, 111);
    }

    #[test]
    fn test_parse_market_depth_zero_depth() {
        let (buf, hdr) = make_market_depth_packet(0, 1, 100.0, None);
        let (_, depth) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        for level in &depth {
            assert_eq!(level.bid_quantity, 0);
            assert_eq!(level.ask_quantity, 0);
            assert_eq!(level.bid_price, 0.0);
            assert_eq!(level.ask_price, 0.0);
        }
    }

    #[test]
    fn test_parse_market_depth_nan_ltp_parses_without_panic() {
        let (buf, hdr) = make_market_depth_packet(2, 13, f32::NAN, None);
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_nan());
    }

    #[test]
    fn test_parse_market_depth_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_market_depth_packet(2, 13, f32::INFINITY, None);
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
    }

    #[test]
    fn test_parse_market_depth_neg_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_market_depth_packet(2, 13, f32::NEG_INFINITY, None);
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
        assert!(tick.last_traded_price.is_sign_negative());
    }

    #[test]
    fn test_parse_market_depth_negative_zero_ltp() {
        let (buf, hdr) = make_market_depth_packet(2, 13, -0.0, None);
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, 0.0); // -0.0 == 0.0 in IEEE 754
    }

    #[test]
    fn test_parse_market_depth_nan_depth_prices_parse_without_panic() {
        let depth_data = [
            (1000u32, 500u32, 10u16, 5u16, f32::NAN, f32::NAN),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
        ];
        let (buf, hdr) = make_market_depth_packet(2, 13, 24500.0, Some(depth_data));
        let (_, depth) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert!(depth[0].bid_price.is_nan());
        assert!(depth[0].ask_price.is_nan());
    }

    #[test]
    fn test_parse_market_depth_infinity_depth_prices_parse_without_panic() {
        let depth_data = [
            (
                1000u32,
                500u32,
                10u16,
                5u16,
                f32::INFINITY,
                f32::NEG_INFINITY,
            ),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
        ];
        let (buf, hdr) = make_market_depth_packet(2, 13, 24500.0, Some(depth_data));
        let (_, depth) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert!(depth[0].bid_price.is_infinite());
        assert!(depth[0].ask_price.is_infinite());
        assert!(depth[0].ask_price.is_sign_negative());
    }

    #[test]
    fn test_parse_market_depth_extra_bytes_ignored() {
        let (mut buf, hdr) = make_market_depth_packet(2, 13, 24500.0, None);
        buf.extend_from_slice(&[0xFF; 20]); // extra garbage
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert!((tick.last_traded_price - 24500.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_market_depth_defaults_zero() {
        let (buf, hdr) = make_market_depth_packet(0, 1, 100.0, None);
        let (tick, _) = parse_market_depth_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.exchange_timestamp, 0);
        assert_eq!(tick.volume, 0);
        assert_eq!(tick.open_interest, 0);
        assert_eq!(tick.average_traded_price, 0.0);
        assert_eq!(tick.day_open, 0.0);
        assert_eq!(tick.day_close, 0.0);
        assert_eq!(tick.day_high, 0.0);
        assert_eq!(tick.day_low, 0.0);
    }
}
