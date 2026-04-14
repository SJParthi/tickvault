//! Disconnect packet parser (10 bytes).
//!
//! Format: `<BHBIH>` — Header(8) + DisconnectCode(u16).

use tickvault_common::constants::{DISCONNECT_OFFSET_CODE, DISCONNECT_PACKET_SIZE};

use crate::websocket::types::DisconnectCode;

use super::types::{PacketHeader, ParseError};

/// Parses a Disconnect packet and returns the disconnect reason code.
///
/// # Performance
/// O(1) — one `from_le_bytes` read + enum match.
#[inline]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by DISCONNECT_PACKET_SIZE check
pub fn parse_disconnect_packet(
    raw: &[u8],
    _header: &PacketHeader,
) -> Result<DisconnectCode, ParseError> {
    if raw.len() < DISCONNECT_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: DISCONNECT_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    let code = u16::from_le_bytes([raw[DISCONNECT_OFFSET_CODE], raw[DISCONNECT_OFFSET_CODE + 1]]);

    Ok(DisconnectCode::from_u16(code))
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets
mod tests {
    use super::*;
    use tickvault_common::constants::{
        DATA_API_ACCESS_TOKEN_EXPIRED, DATA_API_ACCESS_TOKEN_INVALID,
        DATA_API_AUTHENTICATION_FAILED, DATA_API_EXCEEDED_ACTIVE_CONNECTIONS,
        DATA_API_INSTRUMENTS_EXCEED_LIMIT, DATA_API_INTERNAL_SERVER_ERROR, DATA_API_NOT_SUBSCRIBED,
    };

    fn make_disconnect_packet(code: u16) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; DISCONNECT_PACKET_SIZE];
        buf[0] = 50;
        buf[1..3].copy_from_slice(&(DISCONNECT_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = 0;
        buf[4..8].copy_from_slice(&0u32.to_le_bytes());
        buf[8..10].copy_from_slice(&code.to_le_bytes());
        let hdr = PacketHeader {
            response_code: 50,
            message_length: DISCONNECT_PACKET_SIZE as u16,
            exchange_segment_code: 0,
            security_id: 0,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_disconnect_805() {
        let (buf, hdr) = make_disconnect_packet(DATA_API_EXCEEDED_ACTIVE_CONNECTIONS);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::ExceededActiveConnections);
    }

    #[test]
    fn test_parse_disconnect_806() {
        let (buf, hdr) = make_disconnect_packet(DATA_API_NOT_SUBSCRIBED);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::DataApiSubscriptionRequired);
    }

    #[test]
    fn test_parse_disconnect_807() {
        let (buf, hdr) = make_disconnect_packet(DATA_API_ACCESS_TOKEN_EXPIRED);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::AccessTokenExpired);
    }

    #[test]
    fn test_parse_disconnect_808() {
        let (buf, hdr) = make_disconnect_packet(DATA_API_AUTHENTICATION_FAILED);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::AuthenticationFailed);
    }

    #[test]
    fn test_parse_disconnect_809() {
        let (buf, hdr) = make_disconnect_packet(DATA_API_ACCESS_TOKEN_INVALID);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::AccessTokenInvalid);
    }

    #[test]
    fn test_parse_disconnect_unknown_code() {
        let (buf, hdr) = make_disconnect_packet(999);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::Unknown(999));
    }

    #[test]
    fn test_parse_disconnect_empty_buffer() {
        let hdr = PacketHeader {
            response_code: 50,
            message_length: 10,
            exchange_segment_code: 0,
            security_id: 0,
        };
        let err = parse_disconnect_packet(&[], &hdr).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 10,
            actual: 0,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }

    #[test]
    fn test_parse_disconnect_truncated() {
        let buf = vec![0u8; 9];
        let hdr = PacketHeader {
            response_code: 50,
            message_length: 10,
            exchange_segment_code: 0,
            security_id: 0,
        };
        let err = parse_disconnect_packet(&buf, &hdr).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 10,
            actual: 9,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }

    #[test]
    fn test_parse_disconnect_800() {
        let (buf, hdr) = make_disconnect_packet(DATA_API_INTERNAL_SERVER_ERROR);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::InternalServerError);
        assert!(code.is_reconnectable());
    }

    #[test]
    fn test_parse_disconnect_804() {
        let (buf, hdr) = make_disconnect_packet(DATA_API_INSTRUMENTS_EXCEED_LIMIT);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::InstrumentsExceedLimit);
        assert!(!code.is_reconnectable());
    }

    #[test]
    fn test_parse_disconnect_extra_bytes_ignored() {
        let (mut buf, hdr) = make_disconnect_packet(807);
        buf.extend_from_slice(&[0xFF; 10]);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::AccessTokenExpired);
    }

    #[test]
    fn test_parse_disconnect_zero_code() {
        let (buf, hdr) = make_disconnect_packet(0);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::Unknown(0));
    }

    #[test]
    fn test_parse_disconnect_u16_max_code() {
        let (buf, hdr) = make_disconnect_packet(u16::MAX);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::Unknown(u16::MAX));
    }

    #[test]
    fn test_parse_disconnect_non_existent_codes_801_802_803() {
        // Codes 801, 802, 803 are NOT in Dhan annexure — map to Unknown
        for code_val in [801, 802, 803] {
            let (buf, hdr) = make_disconnect_packet(code_val);
            let code = parse_disconnect_packet(&buf, &hdr).unwrap();
            assert_eq!(code, DisconnectCode::Unknown(code_val));
        }
    }

    #[test]
    fn test_parse_disconnect_all_known_codes_roundtrip() {
        let known_codes = [800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814];
        for &code_val in &known_codes {
            let (buf, hdr) = make_disconnect_packet(code_val);
            let code = parse_disconnect_packet(&buf, &hdr).unwrap();
            assert_eq!(code.as_u16(), code_val, "roundtrip failed for {code_val}");
        }
    }
}
