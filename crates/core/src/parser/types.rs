//! Parser types: PacketHeader, ParsedFrame, ParseError.
//!
//! These types are internal to the parser module. The `ParsedTick` and
//! `MarketDepthLevel` types live in `common::tick_types` for cross-crate use.

use dhan_live_trader_common::tick_types::{MarketDepthLevel, ParsedTick};

use crate::websocket::types::DisconnectCode;

// ---------------------------------------------------------------------------
// Packet Header
// ---------------------------------------------------------------------------

/// Parsed binary header shared by all Dhan WebSocket V2 packets.
///
/// 8 bytes: response_code(u8) + msg_length(u16) + exchange_segment(u8) + security_id(u32).
#[derive(Debug, Clone, Copy)]
pub struct PacketHeader {
    /// Response code identifying the packet type (1, 2, 4, 5, 6, 7, 8, 50).
    pub response_code: u8,
    /// Total message length in bytes (as reported by Dhan).
    pub message_length: u16,
    /// Binary exchange segment code (0=IDX, 1=NSE_EQ, 2=NSE_FNO, etc.).
    pub exchange_segment_code: u8,
    /// Dhan security identifier.
    pub security_id: u32,
}

// ---------------------------------------------------------------------------
// Parsed Frame — dispatcher output
// ---------------------------------------------------------------------------

/// A fully parsed WebSocket binary frame from Dhan.
///
/// The dispatcher returns one of these variants for each frame received.
#[derive(Debug)]
pub enum ParsedFrame {
    /// Ticker or Quote packet parsed into a tick (no market depth).
    Tick(ParsedTick),
    /// Full packet parsed into a tick with 5-level market depth.
    TickWithDepth(ParsedTick, [MarketDepthLevel; 5]),
    /// Standalone OI update packet.
    OiUpdate {
        security_id: u32,
        exchange_segment_code: u8,
        open_interest: u32,
    },
    /// Previous close + previous OI packet.
    PreviousClose {
        security_id: u32,
        exchange_segment_code: u8,
        previous_close: f32,
        previous_oi: u32,
    },
    /// Market status change notification.
    MarketStatus {
        security_id: u32,
        exchange_segment_code: u8,
    },
    /// Server-initiated disconnect with reason code.
    Disconnect(DisconnectCode),
}

// ---------------------------------------------------------------------------
// Parse Error
// ---------------------------------------------------------------------------

/// Errors that can occur during binary frame parsing.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Frame is too short for the expected packet type.
    #[error("frame too short: need {expected} bytes, got {actual}")]
    InsufficientBytes { expected: usize, actual: usize },

    /// Response code is not recognized.
    #[error("unknown response code: {0}")]
    UnknownResponseCode(u8),

    /// Row count in 200-level depth header exceeds maximum.
    #[error("invalid row count: {actual} exceeds max {max}")]
    InvalidRowCount { actual: u32, max: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_error_display_insufficient_bytes() {
        let err = ParseError::InsufficientBytes {
            expected: 50,
            actual: 8,
        };
        assert_eq!(err.to_string(), "frame too short: need 50 bytes, got 8");
    }

    #[test]
    fn test_parse_error_display_unknown_response_code() {
        let err = ParseError::UnknownResponseCode(99);
        assert_eq!(err.to_string(), "unknown response code: 99");
    }

    #[test]
    fn test_packet_header_is_copy() {
        let hdr = PacketHeader {
            response_code: 4,
            message_length: 50,
            exchange_segment_code: 2,
            security_id: 13,
        };
        let hdr2 = hdr;
        assert_eq!(hdr.response_code, hdr2.response_code);
    }
}
