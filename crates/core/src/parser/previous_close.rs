//! Previous Close packet parser (16 bytes).
//!
//! Format: `<BHBIfI>` — Header(8) + PrevClose(f32) + PrevOI(u32).

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

// APPROVED: test code — relaxed lint rules for test fixtures
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]
#[cfg(test)]
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
}
