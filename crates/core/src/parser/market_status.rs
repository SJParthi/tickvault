//! Market Status packet parser (8 bytes).
//!
//! Format: `<BHBI>` — Header only, no additional payload.
//! The market status information is encoded in the exchange_segment byte
//! and security_id fields of the header.

use dhan_live_trader_common::constants::MARKET_STATUS_PACKET_SIZE;

use super::types::ParseError;

/// Validates a Market Status packet (header-only).
///
/// Since this packet has no body beyond the header, we just validate
/// the minimum size. The caller uses `header.exchange_segment_code`
/// and `header.security_id` directly.
///
/// # Performance
/// O(1) — just a length check.
pub fn validate_market_status_packet(raw: &[u8]) -> Result<(), ParseError> {
    if raw.len() < MARKET_STATUS_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: MARKET_STATUS_PACKET_SIZE,
            actual: raw.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_market_status_valid() {
        let buf = vec![0u8; 8];
        assert!(validate_market_status_packet(&buf).is_ok());
    }

    #[test]
    fn test_validate_market_status_extra_bytes() {
        let buf = vec![0u8; 16];
        assert!(validate_market_status_packet(&buf).is_ok());
    }

    #[test]
    fn test_validate_market_status_truncated() {
        let buf = vec![0u8; 7];
        let err = validate_market_status_packet(&buf).unwrap_err();
        let ParseError::InsufficientBytes {
            expected: 8,
            actual: 7,
        } = err
        else {
            panic!("wrong error: {err:?}")
        };
    }
}
