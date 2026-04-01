//! Full packet parser (162 bytes).
//!
//! Format: `<BHBIfHIfIIIIIIffff100s>` — Header(8) + common fields + OI(3×u32) + OHLC(4×f32) + Depth(100 bytes).
//! CRITICAL: Diverges from Quote at offset 34. Full has OI fields where Quote has OHLC.

use dhan_live_trader_common::constants::{
    DEPTH_LEVEL_OFFSET_ASK_ORDERS, DEPTH_LEVEL_OFFSET_ASK_PRICE, DEPTH_LEVEL_OFFSET_ASK_QTY,
    DEPTH_LEVEL_OFFSET_BID_ORDERS, DEPTH_LEVEL_OFFSET_BID_PRICE, DEPTH_LEVEL_OFFSET_BID_QTY,
    FULL_OFFSET_CLOSE, FULL_OFFSET_DEPTH_START, FULL_OFFSET_HIGH, FULL_OFFSET_LOW, FULL_OFFSET_OI,
    FULL_OFFSET_OI_DAY_HIGH, FULL_OFFSET_OI_DAY_LOW, FULL_OFFSET_OPEN, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_LEVEL_SIZE, MARKET_DEPTH_LEVELS, QUOTE_OFFSET_ATP, QUOTE_OFFSET_LTP,
    QUOTE_OFFSET_LTQ, QUOTE_OFFSET_LTT, QUOTE_OFFSET_TOTAL_BUY_QTY, QUOTE_OFFSET_TOTAL_SELL_QTY,
    QUOTE_OFFSET_VOLUME,
};
use dhan_live_trader_common::tick_types::{MarketDepthLevel, ParsedTick};

use super::read_helpers::{read_f32_le, read_u16_le, read_u32_le};
use super::types::{PacketHeader, ParseError};

/// Parses a Full packet into a `ParsedTick` and 5 market depth levels.
///
/// All fields are populated including OI and depth.
///
/// # Performance
/// O(1) — fixed number of `from_le_bytes` reads regardless of input.
#[inline]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by FULL_QUOTE_PACKET_SIZE check; depth loop is fixed 5 iterations
pub fn parse_full_packet(
    raw: &[u8],
    header: &PacketHeader,
    received_at_nanos: i64,
) -> Result<(ParsedTick, [MarketDepthLevel; 5]), ParseError> {
    if raw.len() < FULL_QUOTE_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: FULL_QUOTE_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    // Shared fields (offsets 8-33, same as Quote)
    let ltp = read_f32_le(raw, QUOTE_OFFSET_LTP);
    let ltq = u16::from_le_bytes([raw[QUOTE_OFFSET_LTQ], raw[QUOTE_OFFSET_LTQ + 1]]);
    let ltt = read_u32_le(raw, QUOTE_OFFSET_LTT);
    let atp = read_f32_le(raw, QUOTE_OFFSET_ATP);
    let volume = read_u32_le(raw, QUOTE_OFFSET_VOLUME);
    let total_sell_qty = read_u32_le(raw, QUOTE_OFFSET_TOTAL_SELL_QTY);
    let total_buy_qty = read_u32_le(raw, QUOTE_OFFSET_TOTAL_BUY_QTY);

    // Full-specific: OI fields at offsets 34-45 (where Quote has OHLC!)
    let open_interest = read_u32_le(raw, FULL_OFFSET_OI);
    let oi_day_high = read_u32_le(raw, FULL_OFFSET_OI_DAY_HIGH);
    let oi_day_low = read_u32_le(raw, FULL_OFFSET_OI_DAY_LOW);

    // Full-specific: OHLC at offsets 46-61 (shifted 12 bytes vs Quote)
    let day_open = read_f32_le(raw, FULL_OFFSET_OPEN);
    let day_close = read_f32_le(raw, FULL_OFFSET_CLOSE);
    let day_high = read_f32_le(raw, FULL_OFFSET_HIGH);
    let day_low = read_f32_le(raw, FULL_OFFSET_LOW);

    // Depth: 5 levels × 20 bytes starting at offset 62
    let mut depth = [MarketDepthLevel::default(); MARKET_DEPTH_LEVELS];
    for (i, level) in depth.iter_mut().enumerate() {
        let base = FULL_OFFSET_DEPTH_START + i * MARKET_DEPTH_LEVEL_SIZE;
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
        last_trade_quantity: ltq,
        exchange_timestamp: ltt,
        received_at_nanos,
        average_traded_price: atp,
        volume,
        total_sell_quantity: total_sell_qty,
        total_buy_quantity: total_buy_qty,
        day_open,
        day_close,
        day_high,
        day_low,
        open_interest,
        oi_day_high,
        oi_day_low,
        iv: f64::NAN,
        delta: f64::NAN,
        gamma: f64::NAN,
        theta: f64::NAN,
        vega: f64::NAN,
    };

    Ok((tick, depth))
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::EXCHANGE_SEGMENT_NSE_FNO;

    /// Builds a complete 162-byte Full packet.
    #[allow(clippy::too_many_arguments)]
    fn make_full_packet(
        segment: u8,
        security_id: u32,
        ltp: f32,
        ltq: u16,
        ltt: u32,
        atp: f32,
        volume: u32,
        total_sell: u32,
        total_buy: u32,
        oi: u32,
        oi_high: u32,
        oi_low: u32,
        open: f32,
        close: f32,
        high: f32,
        low: f32,
        depth_data: Option<[(u32, u32, u16, u16, f32, f32); 5]>,
    ) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
        // header
        buf[0] = 8; // RESPONSE_CODE_FULL
        buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        // shared body (same as quote up to offset 33)
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        buf[12..14].copy_from_slice(&ltq.to_le_bytes());
        buf[14..18].copy_from_slice(&ltt.to_le_bytes());
        buf[18..22].copy_from_slice(&atp.to_le_bytes());
        buf[22..26].copy_from_slice(&volume.to_le_bytes());
        buf[26..30].copy_from_slice(&total_sell.to_le_bytes());
        buf[30..34].copy_from_slice(&total_buy.to_le_bytes());
        // Full-specific: OI at 34-45
        buf[34..38].copy_from_slice(&oi.to_le_bytes());
        buf[38..42].copy_from_slice(&oi_high.to_le_bytes());
        buf[42..46].copy_from_slice(&oi_low.to_le_bytes());
        // Full-specific: OHLC at 46-61
        buf[46..50].copy_from_slice(&open.to_le_bytes());
        buf[50..54].copy_from_slice(&close.to_le_bytes());
        buf[54..58].copy_from_slice(&high.to_le_bytes());
        buf[58..62].copy_from_slice(&low.to_le_bytes());
        // Depth at 62-161
        if let Some(depth) = depth_data {
            for (i, (bq, aq, bo, ao, bp, ap)) in depth.iter().enumerate() {
                let base = 62 + i * 20;
                buf[base..base + 4].copy_from_slice(&bq.to_le_bytes());
                buf[base + 4..base + 8].copy_from_slice(&aq.to_le_bytes());
                buf[base + 8..base + 10].copy_from_slice(&bo.to_le_bytes());
                buf[base + 10..base + 12].copy_from_slice(&ao.to_le_bytes());
                buf[base + 12..base + 16].copy_from_slice(&bp.to_le_bytes());
                buf[base + 16..base + 20].copy_from_slice(&ap.to_le_bytes());
            }
        }

        let hdr = PacketHeader {
            response_code: 8,
            message_length: FULL_QUOTE_PACKET_SIZE as u16,
            exchange_segment_code: segment,
            security_id,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_full_all_fields() {
        let depth_data = [
            (1000, 500, 10, 5, 24490.0f32, 24500.0f32),
            (800, 600, 8, 6, 24485.0, 24505.0),
            (600, 400, 6, 4, 24480.0, 24510.0),
            (400, 200, 4, 2, 24475.0, 24515.0),
            (200, 100, 2, 1, 24470.0, 24520.0),
        ];
        let (buf, hdr) = make_full_packet(
            EXCHANGE_SEGMENT_NSE_FNO,
            13,
            24_500.5,
            100,
            1740556500,
            24450.25,
            5000000,
            2000000,
            3000000,
            150000,
            160000,
            140000,
            24400.0,
            24300.0,
            24550.0,
            24380.0,
            Some(depth_data),
        );
        let (tick, depth) = parse_full_packet(&buf, &hdr, 777).unwrap();

        // Tick fields
        assert_eq!(tick.security_id, 13);
        assert!((tick.last_traded_price - 24_500.5).abs() < 0.01);
        assert_eq!(tick.last_trade_quantity, 100);
        assert_eq!(tick.exchange_timestamp, 1740556500);
        assert_eq!(tick.received_at_nanos, 777);
        assert_eq!(tick.volume, 5000000);
        assert_eq!(tick.open_interest, 150000);
        assert_eq!(tick.oi_day_high, 160000);
        assert_eq!(tick.oi_day_low, 140000);
        assert!((tick.day_open - 24400.0).abs() < 0.01);
        assert!((tick.day_close - 24300.0).abs() < 0.01);
        assert!((tick.day_high - 24550.0).abs() < 0.01);
        assert!((tick.day_low - 24380.0).abs() < 0.01);

        // Depth level 0
        assert_eq!(depth[0].bid_quantity, 1000);
        assert_eq!(depth[0].ask_quantity, 500);
        assert_eq!(depth[0].bid_orders, 10);
        assert_eq!(depth[0].ask_orders, 5);
        assert!((depth[0].bid_price - 24490.0).abs() < 0.01);
        assert!((depth[0].ask_price - 24500.0).abs() < 0.01);

        // Depth level 4
        assert_eq!(depth[4].bid_quantity, 200);
        assert_eq!(depth[4].ask_quantity, 100);
        assert_eq!(depth[4].bid_orders, 2);
        assert_eq!(depth[4].ask_orders, 1);
    }

    #[test]
    fn test_parse_full_oi_divergence_from_quote() {
        // Verify that offset 34 reads OI (u32), NOT Open (f32)
        let oi_value: u32 = 150000;
        let (buf, hdr) = make_full_packet(
            0, 1, 100.0, 1, 100, 100.0, 1000, 500, 500, oi_value, 160000, 140000, 99.0, 98.0,
            101.0, 97.0, None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.open_interest, oi_value);
        // Verify OHLC reads from Full offsets, not Quote offsets
        assert!((tick.day_open - 99.0).abs() < 0.01);
        assert!((tick.day_close - 98.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_full_empty_buffer() {
        let hdr = PacketHeader {
            response_code: 8,
            message_length: 162,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_full_packet(&[], &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 162,
            actual: 0,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }

    #[test]
    fn test_parse_full_truncated() {
        let buf = vec![0u8; 161]; // one byte short
        let hdr = PacketHeader {
            response_code: 8,
            message_length: 162,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_full_packet(&buf, &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 162,
            actual: 161,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }

    #[test]
    fn test_parse_full_zero_depth() {
        let (buf, hdr) = make_full_packet(
            0, 1, 100.0, 1, 100, 100.0, 1000, 500, 500, 0, 0, 0, 99.0, 98.0, 101.0, 97.0, None,
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        for level in &depth {
            assert_eq!(level.bid_quantity, 0);
            assert_eq!(level.ask_quantity, 0);
            assert_eq!(level.bid_price, 0.0);
            assert_eq!(level.ask_price, 0.0);
        }
    }

    #[test]
    fn test_parse_full_all_depth_levels_populated() {
        let depth_data = [
            (100, 200, 1, 2, 99.0f32, 101.0f32),
            (300, 400, 3, 4, 98.0, 102.0),
            (500, 600, 5, 6, 97.0, 103.0),
            (700, 800, 7, 8, 96.0, 104.0),
            (900, 1000, 9, 10, 95.0, 105.0),
        ];
        let (buf, hdr) = make_full_packet(
            0,
            1,
            100.0,
            1,
            100,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            Some(depth_data),
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        for (i, (bq, aq, bo, ao, bp, ap)) in depth_data.iter().enumerate() {
            assert_eq!(depth[i].bid_quantity, *bq, "depth[{i}].bid_quantity");
            assert_eq!(depth[i].ask_quantity, *aq, "depth[{i}].ask_quantity");
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

    #[test]
    fn test_parse_full_max_depth_values() {
        // Dhan SDK uses unsigned types (I=u32, H=u16) for depth fields.
        let depth_data = [
            (u32::MAX, u32::MAX, u16::MAX, u16::MAX, 99.0f32, 101.0f32),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
        ];
        let (buf, hdr) = make_full_packet(
            0,
            1,
            100.0,
            1,
            100,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            Some(depth_data),
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(depth[0].bid_quantity, u32::MAX);
        assert_eq!(depth[0].ask_quantity, u32::MAX);
        assert_eq!(depth[0].bid_orders, u16::MAX);
        assert_eq!(depth[0].ask_orders, u16::MAX);
    }

    #[test]
    fn test_parse_full_max_oi() {
        let (buf, hdr) = make_full_packet(
            0,
            1,
            100.0,
            1,
            100,
            100.0,
            1000,
            500,
            500,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            99.0,
            98.0,
            101.0,
            97.0,
            None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.open_interest, u32::MAX);
        assert_eq!(tick.oi_day_high, u32::MAX);
        assert_eq!(tick.oi_day_low, u32::MAX);
    }

    #[test]
    fn test_parse_full_nan_ltp_parses_without_panic() {
        let (buf, hdr) = make_full_packet(
            2,
            13,
            f32::NAN,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_nan());
    }

    #[test]
    fn test_parse_full_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_full_packet(
            2,
            13,
            f32::INFINITY,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
    }

    #[test]
    fn test_parse_full_neg_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_full_packet(
            2,
            13,
            f32::NEG_INFINITY,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
        assert!(tick.last_traded_price.is_sign_negative());
    }

    #[test]
    fn test_parse_full_nan_ohlc_parses_without_panic() {
        let (buf, hdr) = make_full_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.day_open.is_nan());
        assert!(tick.day_close.is_nan());
        assert!(tick.day_high.is_nan());
        assert!(tick.day_low.is_nan());
    }

    #[test]
    fn test_parse_full_nan_atp_parses_without_panic() {
        let (buf, hdr) = make_full_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            f32::NAN,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.average_traded_price.is_nan());
    }

    #[test]
    fn test_parse_full_nan_depth_prices_parse_without_panic() {
        let depth_data = [
            (1000u32, 500u32, 10u16, 5u16, f32::NAN, f32::NAN),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
            (0, 0, 0, 0, 0.0, 0.0),
        ];
        let (buf, hdr) = make_full_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            Some(depth_data),
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!(depth[0].bid_price.is_nan());
        assert!(depth[0].ask_price.is_nan());
    }

    #[test]
    fn test_parse_full_infinity_depth_prices_parse_without_panic() {
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
        let (buf, hdr) = make_full_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            Some(depth_data),
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!(depth[0].bid_price.is_infinite());
        assert!(depth[0].ask_price.is_infinite());
        assert!(depth[0].ask_price.is_sign_negative());
    }

    #[test]
    fn test_parse_full_negative_zero_ltp() {
        let (buf, hdr) = make_full_packet(
            2, 13, -0.0, 1, 100, 100.0, 1000, 500, 500, 0, 0, 0, 99.0, 98.0, 101.0, 97.0, None,
        );
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, 0.0); // -0.0 == 0.0 in IEEE 754
    }

    #[test]
    fn test_parse_full_extra_bytes_ignored() {
        let (mut buf, hdr) = make_full_packet(
            2, 13, 24500.0, 1, 1772073900, 100.0, 1000, 500, 500, 0, 0, 0, 99.0, 98.0, 101.0, 97.0,
            None,
        );
        buf.extend_from_slice(&[0xFF; 20]); // extra garbage
        let (tick, _) = parse_full_packet(&buf, &hdr, 0).unwrap();
        assert!((tick.last_traded_price - 24500.0).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // Additional coverage: all depth levels invalid, mixed NaN/Inf depth
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_full_all_depth_nan_bid_and_ask() {
        // All 5 depth levels have NaN for both bid and ask prices.
        // Parser reads them without panic — validation is caller's job.
        let depth_data = [
            (100u32, 200u32, 5u16, 10u16, f32::NAN, f32::NAN),
            (100, 200, 5, 10, f32::NAN, f32::NAN),
            (100, 200, 5, 10, f32::NAN, f32::NAN),
            (100, 200, 5, 10, f32::NAN, f32::NAN),
            (100, 200, 5, 10, f32::NAN, f32::NAN),
        ];
        let (buf, hdr) = make_full_packet(
            2,
            13,
            24500.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            Some(depth_data),
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        for (i, level) in depth.iter().enumerate() {
            assert!(
                level.bid_price.is_nan(),
                "depth[{i}].bid_price should be NaN"
            );
            assert!(
                level.ask_price.is_nan(),
                "depth[{i}].ask_price should be NaN"
            );
            // Quantities are still valid
            assert_eq!(level.bid_quantity, 100);
            assert_eq!(level.ask_quantity, 200);
        }
    }

    #[test]
    fn test_parse_full_all_depth_infinity_bid_neg_infinity_ask() {
        // All 5 levels: bid = +Inf, ask = -Inf
        let depth_data = [
            (50u32, 60u32, 3u16, 4u16, f32::INFINITY, f32::NEG_INFINITY),
            (50, 60, 3, 4, f32::INFINITY, f32::NEG_INFINITY),
            (50, 60, 3, 4, f32::INFINITY, f32::NEG_INFINITY),
            (50, 60, 3, 4, f32::INFINITY, f32::NEG_INFINITY),
            (50, 60, 3, 4, f32::INFINITY, f32::NEG_INFINITY),
        ];
        let (buf, hdr) = make_full_packet(
            2,
            13,
            24500.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            Some(depth_data),
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        for (i, level) in depth.iter().enumerate() {
            assert!(
                level.bid_price.is_infinite() && level.bid_price.is_sign_positive(),
                "depth[{i}].bid_price should be +Infinity"
            );
            assert!(
                level.ask_price.is_infinite() && level.ask_price.is_sign_negative(),
                "depth[{i}].ask_price should be -Infinity"
            );
        }
    }

    #[test]
    fn test_parse_full_mixed_valid_and_nan_depth() {
        // Levels 0,2,4 valid; levels 1,3 have NaN prices
        let depth_data = [
            (100u32, 200u32, 5u16, 10u16, 24490.0f32, 24500.0f32),
            (100, 200, 5, 10, f32::NAN, f32::NAN),
            (100, 200, 5, 10, 24480.0, 24510.0),
            (100, 200, 5, 10, f32::NAN, f32::NAN),
            (100, 200, 5, 10, 24470.0, 24520.0),
        ];
        let (buf, hdr) = make_full_packet(
            2,
            13,
            24500.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            0,
            0,
            0,
            99.0,
            98.0,
            101.0,
            97.0,
            Some(depth_data),
        );
        let (_, depth) = parse_full_packet(&buf, &hdr, 0).unwrap();
        // Valid levels
        assert!((depth[0].bid_price - 24490.0).abs() < 0.01);
        assert!((depth[2].bid_price - 24480.0).abs() < 0.01);
        assert!((depth[4].bid_price - 24470.0).abs() < 0.01);
        // NaN levels
        assert!(depth[1].bid_price.is_nan());
        assert!(depth[3].ask_price.is_nan());
    }
}
