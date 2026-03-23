//! Quote packet parser (50 bytes).
//!
//! Format: `<BHBIfHIfIIIffff>` — Header(8) + LTP + LTQ + LTT + ATP + Vol + TSQ + TBQ + OHLC.
//! CRITICAL: OHLC starts at offset 34 (diverges from Full packet at this point).

use dhan_live_trader_common::constants::{
    QUOTE_OFFSET_ATP, QUOTE_OFFSET_CLOSE, QUOTE_OFFSET_HIGH, QUOTE_OFFSET_LOW, QUOTE_OFFSET_LTP,
    QUOTE_OFFSET_LTQ, QUOTE_OFFSET_LTT, QUOTE_OFFSET_OPEN, QUOTE_OFFSET_TOTAL_BUY_QTY,
    QUOTE_OFFSET_TOTAL_SELL_QTY, QUOTE_OFFSET_VOLUME, QUOTE_PACKET_SIZE,
};
use dhan_live_trader_common::tick_types::ParsedTick;

use super::read_helpers::{read_f32_le, read_u32_le};
use super::types::{PacketHeader, ParseError};

/// Parses a Quote packet into a `ParsedTick`.
///
/// All fields except OI-related ones are populated. OI fields stay at 0.
///
/// # Performance
/// O(1) — eleven `from_le_bytes` reads.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by QUOTE_PACKET_SIZE check
pub fn parse_quote_packet(
    raw: &[u8],
    header: &PacketHeader,
    received_at_nanos: i64,
) -> Result<ParsedTick, ParseError> {
    if raw.len() < QUOTE_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: QUOTE_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    let ltp = read_f32_le(raw, QUOTE_OFFSET_LTP);
    let ltq = u16::from_le_bytes([raw[QUOTE_OFFSET_LTQ], raw[QUOTE_OFFSET_LTQ + 1]]);
    let ltt = read_u32_le(raw, QUOTE_OFFSET_LTT);
    let atp = read_f32_le(raw, QUOTE_OFFSET_ATP);
    let volume = read_u32_le(raw, QUOTE_OFFSET_VOLUME);
    let total_sell_qty = read_u32_le(raw, QUOTE_OFFSET_TOTAL_SELL_QTY);
    let total_buy_qty = read_u32_le(raw, QUOTE_OFFSET_TOTAL_BUY_QTY);
    let day_open = read_f32_le(raw, QUOTE_OFFSET_OPEN);
    let day_close = read_f32_le(raw, QUOTE_OFFSET_CLOSE);
    let day_high = read_f32_le(raw, QUOTE_OFFSET_HIGH);
    let day_low = read_f32_le(raw, QUOTE_OFFSET_LOW);

    Ok(ParsedTick {
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
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::EXCHANGE_SEGMENT_NSE_FNO;

    /// Builds a complete 50-byte Quote packet with all fields.
    #[allow(clippy::too_many_arguments)]
    fn make_quote_packet(
        segment: u8,
        security_id: u32,
        ltp: f32,
        ltq: u16,
        ltt: u32,
        atp: f32,
        volume: u32,
        total_sell: u32,
        total_buy: u32,
        open: f32,
        close: f32,
        high: f32,
        low: f32,
    ) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
        // header
        buf[0] = 4; // RESPONSE_CODE_QUOTE
        buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        // body
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        buf[12..14].copy_from_slice(&ltq.to_le_bytes());
        buf[14..18].copy_from_slice(&ltt.to_le_bytes());
        buf[18..22].copy_from_slice(&atp.to_le_bytes());
        buf[22..26].copy_from_slice(&volume.to_le_bytes());
        buf[26..30].copy_from_slice(&total_sell.to_le_bytes());
        buf[30..34].copy_from_slice(&total_buy.to_le_bytes());
        buf[34..38].copy_from_slice(&open.to_le_bytes());
        buf[38..42].copy_from_slice(&close.to_le_bytes());
        buf[42..46].copy_from_slice(&high.to_le_bytes());
        buf[46..50].copy_from_slice(&low.to_le_bytes());

        let hdr = PacketHeader {
            response_code: 4,
            message_length: QUOTE_PACKET_SIZE as u16,
            exchange_segment_code: segment,
            security_id,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_quote_all_fields() {
        let (buf, hdr) = make_quote_packet(
            EXCHANGE_SEGMENT_NSE_FNO,
            13,
            24_500.5,
            100,
            1740556500,
            24450.25,
            5000000,
            2000000,
            3000000,
            24400.0,
            24300.0,
            24550.0,
            24380.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 888).unwrap();

        assert_eq!(tick.security_id, 13);
        assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
        assert!((tick.last_traded_price - 24_500.5).abs() < 0.01);
        assert_eq!(tick.last_trade_quantity, 100);
        assert_eq!(tick.exchange_timestamp, 1740556500);
        assert_eq!(tick.received_at_nanos, 888);
        assert!((tick.average_traded_price - 24450.25).abs() < 0.01);
        assert_eq!(tick.volume, 5000000);
        assert_eq!(tick.total_sell_quantity, 2000000);
        assert_eq!(tick.total_buy_quantity, 3000000);
        assert!((tick.day_open - 24400.0).abs() < 0.01);
        assert!((tick.day_close - 24300.0).abs() < 0.01);
        assert!((tick.day_high - 24550.0).abs() < 0.01);
        assert!((tick.day_low - 24380.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_quote_oi_fields_zero() {
        let (buf, hdr) = make_quote_packet(
            0, 1, 100.0, 1, 100, 100.0, 1000, 500, 500, 99.0, 98.0, 101.0, 97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.open_interest, 0);
        assert_eq!(tick.oi_day_high, 0);
        assert_eq!(tick.oi_day_low, 0);
    }

    #[test]
    fn test_parse_quote_empty_buffer() {
        let hdr = PacketHeader {
            response_code: 4,
            message_length: 50,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_quote_packet(&[], &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 50,
            actual: 0,
        } = err
        else {
            panic!("wrong error variant")
        };
    }

    #[test]
    fn test_parse_quote_truncated_at_49() {
        let buf = vec![0u8; 49];
        let hdr = PacketHeader {
            response_code: 4,
            message_length: 50,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_quote_packet(&buf, &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes { expected, actual } = err else {
            panic!("wrong error variant")
        };
        assert_eq!(expected, 50);
        assert_eq!(actual, 49);
    }

    #[test]
    fn test_parse_quote_zero_values() {
        let (buf, hdr) = make_quote_packet(0, 0, 0.0, 0, 0, 0.0, 0, 0, 0, 0.0, 0.0, 0.0, 0.0);
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, 0.0);
        assert_eq!(tick.volume, 0);
    }

    #[test]
    fn test_parse_quote_max_ltq() {
        let (buf, hdr) = make_quote_packet(
            0,
            1,
            100.0,
            u16::MAX,
            100,
            100.0,
            1000,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_trade_quantity, u16::MAX);
    }

    #[test]
    fn test_parse_quote_max_volume() {
        let (buf, hdr) = make_quote_packet(
            0,
            1,
            100.0,
            1,
            100,
            100.0,
            u32::MAX,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.volume, u32::MAX);
    }

    #[test]
    fn test_parse_quote_extra_bytes_ignored() {
        // Buffer larger than 50 bytes — extra bytes should be ignored
        let (mut buf, hdr) = make_quote_packet(
            0, 1, 100.0, 1, 100, 100.0, 1000, 500, 500, 99.0, 98.0, 101.0, 97.0,
        );
        buf.extend_from_slice(&[0xFF; 20]); // extra garbage
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.volume, 1000);
    }

    #[test]
    fn test_parse_quote_truncated_at_header() {
        let buf = vec![0u8; 8]; // only header, no body
        let hdr = PacketHeader {
            response_code: 4,
            message_length: 50,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_quote_packet(&buf, &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 50,
            actual: 8,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }

    #[test]
    fn test_parse_quote_nan_ltp_parses_without_panic() {
        let (buf, hdr) = make_quote_packet(
            2,
            13,
            f32::NAN,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_nan());
    }

    #[test]
    fn test_parse_quote_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_quote_packet(
            2,
            13,
            f32::INFINITY,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
    }

    #[test]
    fn test_parse_quote_neg_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_quote_packet(
            2,
            13,
            f32::NEG_INFINITY,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
        assert!(tick.last_traded_price.is_sign_negative());
    }

    #[test]
    fn test_parse_quote_nan_atp_parses_without_panic() {
        let (buf, hdr) = make_quote_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            f32::NAN,
            1000,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.average_traded_price.is_nan());
    }

    #[test]
    fn test_parse_quote_nan_ohlc_parses_without_panic() {
        let (buf, hdr) = make_quote_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            500,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            f32::NAN,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.day_open.is_nan());
        assert!(tick.day_close.is_nan());
        assert!(tick.day_high.is_nan());
        assert!(tick.day_low.is_nan());
    }

    #[test]
    fn test_parse_quote_negative_ltp_parses_without_panic() {
        let (buf, hdr) = make_quote_packet(
            2, 13, -500.0, 1, 1772073900, 100.0, 1000, 500, 500, 99.0, 98.0, 101.0, 97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price < 0.0);
        // Parser accepts negative LTP — tick_processor filters it downstream
    }

    #[test]
    fn test_parse_quote_negative_zero_ltp() {
        let (buf, hdr) = make_quote_packet(
            2, 13, -0.0, 1, 100, 100.0, 1000, 500, 500, 99.0, 98.0, 101.0, 97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, 0.0); // -0.0 == 0.0 in IEEE 754
    }

    #[test]
    fn test_parse_quote_f32_precision() {
        let price: f32 = 24567.85;
        let (buf, hdr) = make_quote_packet(
            2, 13, price, 1, 100, 100.0, 1000, 500, 500, 99.0, 98.0, 101.0, 97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, price);
    }

    // -----------------------------------------------------------------------
    // Additional coverage: all max values simultaneously, edge field combos
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_quote_all_max_u32_fields() {
        // All u32 fields at their maximum — verifies no truncation or overflow
        let (buf, hdr) = make_quote_packet(
            2,
            u32::MAX, // security_id
            f32::MAX, // ltp
            u16::MAX, // ltq
            u32::MAX, // ltt
            f32::MAX, // atp
            u32::MAX, // volume
            u32::MAX, // total_sell
            u32::MAX, // total_buy
            f32::MAX, // open
            f32::MAX, // close
            f32::MAX, // high
            f32::MAX, // low
        );
        let tick = parse_quote_packet(&buf, &hdr, i64::MAX).unwrap();
        assert_eq!(tick.security_id, u32::MAX);
        assert_eq!(tick.last_trade_quantity, u16::MAX);
        assert_eq!(tick.exchange_timestamp, u32::MAX);
        assert_eq!(tick.volume, u32::MAX);
        assert_eq!(tick.total_sell_quantity, u32::MAX);
        assert_eq!(tick.total_buy_quantity, u32::MAX);
        assert_eq!(tick.received_at_nanos, i64::MAX);
    }

    #[test]
    fn test_parse_quote_max_total_sell_qty() {
        let (buf, hdr) = make_quote_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            100.0,
            1000,
            u32::MAX,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.total_sell_quantity, u32::MAX);
    }

    #[test]
    fn test_parse_quote_max_total_buy_qty() {
        let (buf, hdr) = make_quote_packet(
            2,
            13,
            100.0,
            1,
            1772073900,
            100.0,
            1000,
            500,
            u32::MAX,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.total_buy_quantity, u32::MAX);
    }

    #[test]
    fn test_parse_quote_negative_ltp_with_valid_ohlc() {
        // Negative LTP + valid OHLC: parser reads all fields correctly
        let (buf, hdr) = make_quote_packet(
            2, 13, -500.0, 10, 1772073900, 100.0, 5000, 1000, 2000, 99.0, 98.0, 101.0, 97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price < 0.0);
        assert!((tick.day_open - 99.0).abs() < 0.01);
        assert!((tick.day_close - 98.0).abs() < 0.01);
        assert!((tick.day_high - 101.0).abs() < 0.01);
        assert!((tick.day_low - 97.0).abs() < 0.01);
    }
}
