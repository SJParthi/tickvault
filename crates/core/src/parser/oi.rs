//! OI packet parser (12 bytes).
//!
//! Format: `<BHBII>` — Header(8) + OI(u32).

use dhan_live_trader_common::constants::{OI_OFFSET_VALUE, OI_PACKET_SIZE};

use super::types::{PacketHeader, ParseError};

/// Parses a standalone OI packet.
///
/// # Performance
/// O(1) — one `from_le_bytes` read.
pub fn parse_oi_packet(raw: &[u8], header: &PacketHeader) -> Result<u32, ParseError> {
    if raw.len() < OI_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: OI_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    let oi = u32::from_le_bytes([
        raw[OI_OFFSET_VALUE],
        raw[OI_OFFSET_VALUE + 1],
        raw[OI_OFFSET_VALUE + 2],
        raw[OI_OFFSET_VALUE + 3],
    ]);
    let _ = header; // used for context in caller
    Ok(oi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_oi_packet(security_id: u32, oi: u32) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; OI_PACKET_SIZE];
        buf[0] = 5;
        buf[1..3].copy_from_slice(&(OI_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&oi.to_le_bytes());
        let hdr = PacketHeader {
            response_code: 5,
            message_length: OI_PACKET_SIZE as u16,
            exchange_segment_code: 2,
            security_id,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_oi_valid() {
        let (buf, hdr) = make_oi_packet(13, 150000);
        let oi = parse_oi_packet(&buf, &hdr).unwrap();
        assert_eq!(oi, 150000);
    }

    #[test]
    fn test_parse_oi_zero() {
        let (buf, hdr) = make_oi_packet(1, 0);
        assert_eq!(parse_oi_packet(&buf, &hdr).unwrap(), 0);
    }

    #[test]
    fn test_parse_oi_max() {
        let (buf, hdr) = make_oi_packet(1, u32::MAX);
        assert_eq!(parse_oi_packet(&buf, &hdr).unwrap(), u32::MAX);
    }

    #[test]
    fn test_parse_oi_truncated() {
        let buf = vec![0u8; 11];
        let hdr = PacketHeader {
            response_code: 5,
            message_length: 12,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_oi_packet(&buf, &hdr).unwrap_err();
        match err {
            ParseError::InsufficientBytes {
                expected: 12,
                actual: 11,
            } => {}
            _ => panic!("wrong error: {err:?}"),
        }
    }
}
