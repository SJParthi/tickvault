//! Previous Close packet parser (16 bytes).
//!
//! Format: `<BHBIfI>` — Header(8) + PrevClose(f32) + PrevOI(u32).

use dhan_live_trader_common::constants::{
    PREV_CLOSE_OFFSET_OI, PREV_CLOSE_OFFSET_PRICE, PREVIOUS_CLOSE_PACKET_SIZE,
};

use super::types::{PacketHeader, ParseError};

/// Parsed previous close data.
#[derive(Debug)]
pub struct PreviousCloseData {
    pub previous_close: f32,
    pub previous_oi: u32,
}

/// Parses a Previous Close packet.
///
/// # Performance
/// O(1) — two `from_le_bytes` reads.
#[inline]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by PREVIOUS_CLOSE_PACKET_SIZE check
pub fn parse_previous_close_packet(
    raw: &[u8],
    _header: &PacketHeader,
) -> Result<PreviousCloseData, ParseError> {
    if raw.len() < PREVIOUS_CLOSE_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: PREVIOUS_CLOSE_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    let prev_close = f32::from_le_bytes([
        raw[PREV_CLOSE_OFFSET_PRICE],
        raw[PREV_CLOSE_OFFSET_PRICE + 1],
        raw[PREV_CLOSE_OFFSET_PRICE + 2],
        raw[PREV_CLOSE_OFFSET_PRICE + 3],
    ]);
    let prev_oi = u32::from_le_bytes([
        raw[PREV_CLOSE_OFFSET_OI],
        raw[PREV_CLOSE_OFFSET_OI + 1],
        raw[PREV_CLOSE_OFFSET_OI + 2],
        raw[PREV_CLOSE_OFFSET_OI + 3],
    ]);

    Ok(PreviousCloseData {
        previous_close: prev_close,
        previous_oi: prev_oi,
    })
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets
mod tests {
    use super::*;

    fn make_prev_close_packet(security_id: u32, close: f32, oi: u32) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; PREVIOUS_CLOSE_PACKET_SIZE];
        buf[0] = 6;
        buf[1..3].copy_from_slice(&(PREVIOUS_CLOSE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = 0;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&close.to_le_bytes());
        buf[12..16].copy_from_slice(&oi.to_le_bytes());
        let hdr = PacketHeader {
            response_code: 6,
            message_length: PREVIOUS_CLOSE_PACKET_SIZE as u16,
            exchange_segment_code: 0,
            security_id,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_previous_close_valid() {
        let (buf, hdr) = make_prev_close_packet(13, 24_300.5, 120000);
        let data = parse_previous_close_packet(&buf, &hdr).unwrap();
        assert!((data.previous_close - 24_300.5).abs() < 0.01);
        assert_eq!(data.previous_oi, 120000);
    }

    #[test]
    fn test_parse_previous_close_zero_oi() {
        let (buf, hdr) = make_prev_close_packet(1, 100.0, 0);
        let data = parse_previous_close_packet(&buf, &hdr).unwrap();
        assert_eq!(data.previous_oi, 0);
    }

    #[test]
    fn test_parse_previous_close_debug_format() {
        let data = PreviousCloseData {
            previous_close: 24300.5,
            previous_oi: 120000,
        };
        let debug = format!("{data:?}");
        assert!(debug.contains("24300.5"));
        assert!(debug.contains("120000"));
    }

    #[test]
    fn test_parse_previous_close_empty_buffer() {
        let hdr = PacketHeader {
            response_code: 6,
            message_length: 16,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_previous_close_packet(&[], &hdr).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 16,
            actual: 0,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }

    #[test]
    fn test_parse_previous_close_truncated() {
        let buf = vec![0u8; 15];
        let hdr = PacketHeader {
            response_code: 6,
            message_length: 16,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_previous_close_packet(&buf, &hdr).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 16,
            actual: 15,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }

    #[test]
    fn test_parse_previous_close_max_oi() {
        let (buf, hdr) = make_prev_close_packet(1, 100.0, u32::MAX);
        let data = parse_previous_close_packet(&buf, &hdr).unwrap();
        assert_eq!(data.previous_oi, u32::MAX);
    }

    #[test]
    fn test_parse_previous_close_nan_price() {
        let (buf, hdr) = make_prev_close_packet(1, f32::NAN, 0);
        let data = parse_previous_close_packet(&buf, &hdr).unwrap();
        assert!(data.previous_close.is_nan());
    }

    #[test]
    fn test_parse_previous_close_infinity_price() {
        let (buf, hdr) = make_prev_close_packet(1, f32::INFINITY, 0);
        let data = parse_previous_close_packet(&buf, &hdr).unwrap();
        assert!(data.previous_close.is_infinite());
    }

    #[test]
    fn test_parse_previous_close_negative_price() {
        let (buf, hdr) = make_prev_close_packet(1, -500.0, 0);
        let data = parse_previous_close_packet(&buf, &hdr).unwrap();
        assert!(data.previous_close < 0.0);
    }

    #[test]
    fn test_parse_previous_close_extra_bytes_ignored() {
        let (mut buf, hdr) = make_prev_close_packet(1, 100.0, 5000);
        buf.extend_from_slice(&[0xFF; 20]);
        let data = parse_previous_close_packet(&buf, &hdr).unwrap();
        assert!((data.previous_close - 100.0).abs() < f32::EPSILON);
        assert_eq!(data.previous_oi, 5000);
    }

    #[test]
    fn test_parse_previous_close_header_only_fails() {
        let buf = vec![0u8; 8];
        let hdr = PacketHeader {
            response_code: 6,
            message_length: 16,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_previous_close_packet(&buf, &hdr).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 16,
            actual: 8,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }
}
