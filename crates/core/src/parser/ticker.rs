//! Ticker packet parser (16 bytes).
//!
//! Format: `<BHBIfI>` — Header(8) + LTP(f32) + LTT(u32).
//! Used for both Index Ticker (response_code 1) and Ticker (response_code 2).

use tickvault_common::constants::{TICKER_OFFSET_LTP, TICKER_OFFSET_LTT, TICKER_PACKET_SIZE};
use tickvault_common::tick_types::ParsedTick;

use super::types::{PacketHeader, ParseError};

/// Parses a Ticker packet into a `ParsedTick`.
///
/// Only LTP and exchange timestamp are populated — all other fields default to 0.
///
/// # Performance
/// O(1) — two `from_le_bytes` reads.
#[inline]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by TICKER_PACKET_SIZE check above
pub fn parse_ticker_packet(
    raw: &[u8],
    header: &PacketHeader,
    received_at_nanos: i64,
) -> Result<ParsedTick, ParseError> {
    if raw.len() < TICKER_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: TICKER_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    let ltp = f32::from_le_bytes([
        raw[TICKER_OFFSET_LTP],
        raw[TICKER_OFFSET_LTP + 1],
        raw[TICKER_OFFSET_LTP + 2],
        raw[TICKER_OFFSET_LTP + 3],
    ]);
    let ltt = u32::from_le_bytes([
        raw[TICKER_OFFSET_LTT],
        raw[TICKER_OFFSET_LTT + 1],
        raw[TICKER_OFFSET_LTT + 2],
        raw[TICKER_OFFSET_LTT + 3],
    ]);

    Ok(ParsedTick {
        security_id: header.security_id,
        exchange_segment_code: header.exchange_segment_code,
        last_traded_price: ltp,
        exchange_timestamp: ltt,
        received_at_nanos,
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction
mod tests {
    use super::*;
    use tickvault_common::constants::EXCHANGE_SEGMENT_NSE_FNO;

    fn make_ticker_packet(
        segment: u8,
        security_id: u32,
        ltp: f32,
        ltt: u32,
    ) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; TICKER_PACKET_SIZE];
        // header
        buf[0] = 2; // RESPONSE_CODE_TICKER
        buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        // body
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        buf[12..16].copy_from_slice(&ltt.to_le_bytes());

        let hdr = PacketHeader {
            response_code: 2,
            message_length: TICKER_PACKET_SIZE as u16,
            exchange_segment_code: segment,
            security_id,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_ticker_valid() {
        let (buf, hdr) = make_ticker_packet(EXCHANGE_SEGMENT_NSE_FNO, 2885, 2450.50, 1740556500);
        let tick = parse_ticker_packet(&buf, &hdr, 999).unwrap();
        assert_eq!(tick.security_id, 2885);
        assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
        assert!((tick.last_traded_price - 2450.50).abs() < 0.01);
        assert_eq!(tick.exchange_timestamp, 1740556500);
        assert_eq!(tick.received_at_nanos, 999);
    }

    #[test]
    fn test_parse_ticker_zero_ltp() {
        let (buf, hdr) = make_ticker_packet(0, 13, 0.0, 100);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, 0.0);
    }

    #[test]
    fn test_parse_ticker_max_ltt() {
        let (buf, hdr) = make_ticker_packet(0, 1, 100.0, u32::MAX);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.exchange_timestamp, u32::MAX);
    }

    #[test]
    fn test_parse_ticker_defaults_zero() {
        let (buf, hdr) = make_ticker_packet(0, 1, 100.0, 100);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.volume, 0);
        assert_eq!(tick.open_interest, 0);
        assert_eq!(tick.last_trade_quantity, 0);
        assert_eq!(tick.average_traded_price, 0.0);
        assert_eq!(tick.day_open, 0.0);
    }

    #[test]
    fn test_parse_ticker_truncated() {
        let buf = vec![0u8; 15]; // one byte short
        let hdr = PacketHeader {
            response_code: 2,
            message_length: 16,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_ticker_packet(&buf, &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes { expected, actual } = err else {
            panic!("wrong error variant")
        };
        assert_eq!(expected, 16);
        assert_eq!(actual, 15);
    }

    #[test]
    fn test_parse_ticker_f32_precision() {
        // Test that f32 precision is preserved (prices in rupees)
        let price: f32 = 24567.85;
        let (buf, hdr) = make_ticker_packet(2, 13, price, 100);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, price);
    }

    #[test]
    fn test_parse_ticker_nan_ltp_parses_without_panic() {
        let (buf, hdr) = make_ticker_packet(2, 13, f32::NAN, 1772073900);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_nan());
    }

    #[test]
    fn test_parse_ticker_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_ticker_packet(2, 13, f32::INFINITY, 1772073900);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
    }

    #[test]
    fn test_parse_ticker_neg_infinity_ltp_parses_without_panic() {
        let (buf, hdr) = make_ticker_packet(2, 13, f32::NEG_INFINITY, 1772073900);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price.is_infinite());
        assert!(tick.last_traded_price.is_sign_negative());
    }

    #[test]
    fn test_parse_ticker_negative_ltp_parses_without_panic() {
        let (buf, hdr) = make_ticker_packet(2, 13, -500.0, 1772073900);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert!(tick.last_traded_price < 0.0);
        // Parser accepts negative LTP — tick_processor filters it downstream
    }

    #[test]
    fn test_parse_ticker_negative_zero_ltp() {
        let (buf, hdr) = make_ticker_packet(2, 13, -0.0, 100);
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, 0.0); // -0.0 == 0.0 in IEEE 754
    }

    #[test]
    fn test_parse_ticker_empty_buffer() {
        let hdr = PacketHeader {
            response_code: 2,
            message_length: 16,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_ticker_packet(&[], &hdr, 0).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 16,
            actual: 0,
        } = err
        else {
            panic!("wrong error variant")
        };
    }

    #[test]
    fn test_parse_ticker_extra_bytes_ignored() {
        let (mut buf, hdr) = make_ticker_packet(2, 13, 24500.0, 1772073900);
        buf.extend_from_slice(&[0xFF; 20]); // extra garbage
        let tick = parse_ticker_packet(&buf, &hdr, 0).unwrap();
        assert!((tick.last_traded_price - 24500.0).abs() < 0.01);
    }
}
