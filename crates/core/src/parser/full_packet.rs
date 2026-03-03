//! Full packet parser (162 bytes).
//!
//! Format: `<BHBIfHIfIIIIIIffff100s>` — Header(8) + common fields + OI(3×u32) + OHLC(4×f32) + Depth(100 bytes).
//! CRITICAL: Diverges from Quote at offset 34. Full has OI fields where Quote has OHLC.

// SAFETY: Binary protocol parser — byte offsets, indexing, and type conversions are
// inherent to parsing fixed-layout binary packets. Bounds are validated at the packet
// entry point by header length checks before individual field parsers execute.
// APPROVED: binary parser requires indexing, arithmetic, and type conversions by design
#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

use dhan_live_trader_common::constants::{
    FULL_OFFSET_CLOSE, FULL_OFFSET_DEPTH_START, FULL_OFFSET_HIGH, FULL_OFFSET_LOW, FULL_OFFSET_OI,
    FULL_OFFSET_OI_DAY_HIGH, FULL_OFFSET_OI_DAY_LOW, FULL_OFFSET_OPEN, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_LEVEL_SIZE, MARKET_DEPTH_LEVELS, QUOTE_OFFSET_ATP, QUOTE_OFFSET_LTP,
    QUOTE_OFFSET_LTQ, QUOTE_OFFSET_LTT, QUOTE_OFFSET_TOTAL_BUY_QTY, QUOTE_OFFSET_TOTAL_SELL_QTY,
    QUOTE_OFFSET_VOLUME,
};
use dhan_live_trader_common::tick_types::{MarketDepthLevel, ParsedTick};

use super::types::{PacketHeader, ParseError};

/// Helper: reads a little-endian f32 from a byte slice at the given offset.
#[inline(always)]
fn read_f32_le(raw: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

/// Helper: reads a little-endian u32 from a byte slice at the given offset.
#[inline(always)]
fn read_u32_le(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

/// Helper: reads a little-endian i32 from a byte slice at the given offset.
#[inline(always)]
fn read_i32_le(raw: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

/// Helper: reads a little-endian i16 from a byte slice at the given offset.
#[inline(always)]
fn read_i16_le(raw: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([raw[offset], raw[offset + 1]])
}

/// Parses a Full packet into a `ParsedTick` and 5 market depth levels.
///
/// All fields are populated including OI and depth.
///
/// # Performance
/// O(1) — fixed number of `from_le_bytes` reads regardless of input.
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
            bid_quantity: read_i32_le(raw, base),
            ask_quantity: read_i32_le(raw, base + 4),
            bid_orders: read_i16_le(raw, base + 8),
            ask_orders: read_i16_le(raw, base + 10),
            bid_price: read_f32_le(raw, base + 12),
            ask_price: read_f32_le(raw, base + 16),
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
    };

    Ok((tick, depth))
}

// APPROVED: test code — relaxed lint rules for test fixtures
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]
#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::EXCHANGE_SEGMENT_NSE_FNO;

    /// Builds a complete 162-byte Full packet.
    // APPROVED: test code — relaxed lint rules for test fixtures
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
        depth_data: Option<[(i32, i32, i16, i16, f32, f32); 5]>,
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
    fn test_parse_full_negative_depth_values() {
        // depth quantities can be negative in edge cases
        let depth_data = [
            (-100, -200, -1, -2, 99.0f32, 101.0f32),
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
        assert_eq!(depth[0].bid_quantity, -100);
        assert_eq!(depth[0].ask_quantity, -200);
        assert_eq!(depth[0].bid_orders, -1);
        assert_eq!(depth[0].ask_orders, -2);
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
}
