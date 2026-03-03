//! Binary header parser for Dhan WebSocket V2 protocol.
//!
//! All packets share an 8-byte header:
//! `response_code(u8) + msg_length(u16 LE) + exchange_segment(u8) + security_id(u32 LE)`.

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
    BINARY_HEADER_SIZE, HEADER_OFFSET_EXCHANGE_SEGMENT, HEADER_OFFSET_MESSAGE_LENGTH,
    HEADER_OFFSET_RESPONSE_CODE, HEADER_OFFSET_SECURITY_ID,
};

use super::types::{PacketHeader, ParseError};

/// Parses the 8-byte binary header from a raw frame.
///
/// # Errors
/// Returns `ParseError::InsufficientBytes` if `raw` is shorter than 8 bytes.
///
/// # Performance
/// O(1) — four `from_le_bytes` reads from contiguous memory.
pub fn parse_header(raw: &[u8]) -> Result<PacketHeader, ParseError> {
    if raw.len() < BINARY_HEADER_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: BINARY_HEADER_SIZE,
            actual: raw.len(),
        });
    }

    let response_code = raw[HEADER_OFFSET_RESPONSE_CODE];
    let message_length = u16::from_le_bytes([
        raw[HEADER_OFFSET_MESSAGE_LENGTH],
        raw[HEADER_OFFSET_MESSAGE_LENGTH + 1],
    ]);
    let exchange_segment_code = raw[HEADER_OFFSET_EXCHANGE_SEGMENT];
    let security_id = u32::from_le_bytes([
        raw[HEADER_OFFSET_SECURITY_ID],
        raw[HEADER_OFFSET_SECURITY_ID + 1],
        raw[HEADER_OFFSET_SECURITY_ID + 2],
        raw[HEADER_OFFSET_SECURITY_ID + 3],
    ]);

    Ok(PacketHeader {
        response_code,
        message_length,
        exchange_segment_code,
        security_id,
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
    use dhan_live_trader_common::constants::{
        EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_NSE_FNO, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER,
    };

    /// Helper: builds a valid 8-byte header.
    fn make_header(response_code: u8, msg_len: u16, segment: u8, security_id: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 8];
        buf[0] = response_code;
        buf[1..3].copy_from_slice(&msg_len.to_le_bytes());
        buf[3] = segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf
    }

    #[test]
    fn test_parse_header_valid_ticker() {
        let buf = make_header(RESPONSE_CODE_TICKER, 16, EXCHANGE_SEGMENT_NSE_FNO, 2885);
        let hdr = parse_header(&buf).unwrap();
        assert_eq!(hdr.response_code, RESPONSE_CODE_TICKER);
        assert_eq!(hdr.message_length, 16);
        assert_eq!(hdr.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
        assert_eq!(hdr.security_id, 2885);
    }

    #[test]
    fn test_parse_header_valid_quote() {
        let buf = make_header(RESPONSE_CODE_QUOTE, 50, EXCHANGE_SEGMENT_IDX_I, 13);
        let hdr = parse_header(&buf).unwrap();
        assert_eq!(hdr.response_code, RESPONSE_CODE_QUOTE);
        assert_eq!(hdr.message_length, 50);
        assert_eq!(hdr.exchange_segment_code, EXCHANGE_SEGMENT_IDX_I);
        assert_eq!(hdr.security_id, 13);
    }

    #[test]
    fn test_parse_header_max_security_id() {
        let buf = make_header(RESPONSE_CODE_TICKER, 16, 0, u32::MAX);
        let hdr = parse_header(&buf).unwrap();
        assert_eq!(hdr.security_id, u32::MAX);
    }

    #[test]
    fn test_parse_header_zero_security_id() {
        let buf = make_header(RESPONSE_CODE_TICKER, 16, 0, 0);
        let hdr = parse_header(&buf).unwrap();
        assert_eq!(hdr.security_id, 0);
    }

    #[test]
    fn test_parse_header_too_short_empty() {
        let err = parse_header(&[]).unwrap_err();
        let ParseError::InsufficientBytes { expected, actual } = err else {
            panic!("wrong error variant")
        };
        assert_eq!(expected, 8);
        assert_eq!(actual, 0);
    }

    #[test]
    fn test_parse_header_too_short_seven_bytes() {
        let err = parse_header(&[0u8; 7]).unwrap_err();
        let ParseError::InsufficientBytes { expected, actual } = err else {
            panic!("wrong error variant")
        };
        assert_eq!(expected, 8);
        assert_eq!(actual, 7);
    }

    #[test]
    fn test_parse_header_exactly_eight_bytes_ok() {
        let buf = make_header(1, 16, 0, 42);
        assert!(parse_header(&buf).is_ok());
    }

    #[test]
    fn test_parse_header_all_segments() {
        for seg in [0u8, 1, 2, 3, 4, 5, 7, 8] {
            let buf = make_header(RESPONSE_CODE_TICKER, 16, seg, 1);
            let hdr = parse_header(&buf).unwrap();
            assert_eq!(hdr.exchange_segment_code, seg);
        }
    }
}
