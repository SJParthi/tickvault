//! Disconnect packet parser (10 bytes).
//!
//! Format: `<BHBIH>` — Header(8) + DisconnectCode(u16).

use dhan_live_trader_common::constants::{DISCONNECT_OFFSET_CODE, DISCONNECT_PACKET_SIZE};

use crate::websocket::types::DisconnectCode;

use super::types::{PacketHeader, ParseError};

/// Parses a Disconnect packet and returns the disconnect reason code.
///
/// # Performance
/// O(1) — one `from_le_bytes` read + enum match.
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
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::{
        DISCONNECT_ACCESS_TOKEN_EXPIRED, DISCONNECT_AUTH_FAILED,
        DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED, DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS,
        DISCONNECT_INVALID_CLIENT_ID,
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
        let (buf, hdr) = make_disconnect_packet(DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::ExceededActiveConnections);
    }

    #[test]
    fn test_parse_disconnect_806() {
        let (buf, hdr) = make_disconnect_packet(DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::DataApiSubscriptionRequired);
    }

    #[test]
    fn test_parse_disconnect_807() {
        let (buf, hdr) = make_disconnect_packet(DISCONNECT_ACCESS_TOKEN_EXPIRED);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::AccessTokenExpired);
    }

    #[test]
    fn test_parse_disconnect_808() {
        let (buf, hdr) = make_disconnect_packet(DISCONNECT_INVALID_CLIENT_ID);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::InvalidClientId);
    }

    #[test]
    fn test_parse_disconnect_809() {
        let (buf, hdr) = make_disconnect_packet(DISCONNECT_AUTH_FAILED);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::AuthenticationFailed);
    }

    #[test]
    fn test_parse_disconnect_unknown_code() {
        let (buf, hdr) = make_disconnect_packet(999);
        let code = parse_disconnect_packet(&buf, &hdr).unwrap();
        assert_eq!(code, DisconnectCode::Unknown(999));
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
}
