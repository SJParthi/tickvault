//! Parser types: PacketHeader, ParsedFrame, ParseError.
//!
//! These types are internal to the parser module. The `ParsedTick` and
//! `MarketDepthLevel` types live in `common::tick_types` for cross-crate use.

use dhan_live_trader_common::tick_types::{DeepDepthLevel, MarketDepthLevel, ParsedTick};

use crate::parser::deep_depth::DepthSide;
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
    /// Deep depth packet (20-level or 200-level) — one side (bid or ask).
    DeepDepth {
        security_id: u32,
        exchange_segment_code: u8,
        side: DepthSide,
        levels: Vec<DeepDepthLevel>,
        message_sequence: u32,
        received_at_nanos: i64,
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
    fn test_parsed_frame_deep_depth_variant() {
        let frame = ParsedFrame::DeepDepth {
            security_id: 52432,
            exchange_segment_code: 2,
            side: DepthSide::Bid,
            levels: vec![DeepDepthLevel {
                price: 24500.0,
                quantity: 1000,
                orders: 50,
            }],
            message_sequence: 1,
            received_at_nanos: 999,
        };
        match frame {
            ParsedFrame::DeepDepth {
                security_id,
                side,
                levels,
                ..
            } => {
                assert_eq!(security_id, 52432);
                assert_eq!(side, DepthSide::Bid);
                assert_eq!(levels.len(), 1);
                assert!((levels[0].price - 24500.0).abs() < f64::EPSILON);
            }
            other => panic!("expected DeepDepth, got {other:?}"),
        }
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

    #[test]
    fn test_parse_error_display_invalid_row_count() {
        let err = ParseError::InvalidRowCount {
            actual: 300,
            max: 200,
        };
        let msg = err.to_string();
        assert!(msg.contains("300"), "should contain actual: {msg}");
        assert!(msg.contains("200"), "should contain max: {msg}");
        assert!(
            msg.contains("invalid row count"),
            "should contain prefix: {msg}"
        );
    }

    #[test]
    fn test_parse_error_debug_formatting() {
        let err = ParseError::InsufficientBytes {
            expected: 162,
            actual: 50,
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("InsufficientBytes"));
        assert!(debug.contains("162"));
        assert!(debug.contains("50"));
    }

    #[test]
    fn test_parse_error_unknown_response_code_debug() {
        let err = ParseError::UnknownResponseCode(255);
        let debug = format!("{err:?}");
        assert!(debug.contains("255"));
    }

    #[test]
    fn test_parsed_frame_tick_variant() {
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 2,
            last_traded_price: 24500.0,
            exchange_timestamp: 1772073900,
            ..Default::default()
        };
        let frame = ParsedFrame::Tick(tick);
        match frame {
            ParsedFrame::Tick(t) => {
                assert_eq!(t.security_id, 13);
            }
            other => panic!("expected Tick, got {other:?}"),
        }
    }

    #[test]
    fn test_parsed_frame_oi_update_variant() {
        let frame = ParsedFrame::OiUpdate {
            security_id: 42,
            exchange_segment_code: 2,
            open_interest: 5000,
        };
        match frame {
            ParsedFrame::OiUpdate {
                security_id,
                open_interest,
                ..
            } => {
                assert_eq!(security_id, 42);
                assert_eq!(open_interest, 5000);
            }
            other => panic!("expected OiUpdate, got {other:?}"),
        }
    }

    #[test]
    fn test_parsed_frame_previous_close_variant() {
        let frame = ParsedFrame::PreviousClose {
            security_id: 99,
            exchange_segment_code: 1,
            previous_close: 1500.0,
            previous_oi: 0,
        };
        match frame {
            ParsedFrame::PreviousClose {
                previous_close,
                previous_oi,
                ..
            } => {
                assert!((previous_close - 1500.0).abs() < f32::EPSILON);
                assert_eq!(previous_oi, 0);
            }
            other => panic!("expected PreviousClose, got {other:?}"),
        }
    }

    #[test]
    fn test_parsed_frame_market_status_variant() {
        let frame = ParsedFrame::MarketStatus {
            security_id: 0,
            exchange_segment_code: 0,
        };
        match frame {
            ParsedFrame::MarketStatus {
                security_id,
                exchange_segment_code,
            } => {
                assert_eq!(security_id, 0);
                assert_eq!(exchange_segment_code, 0);
            }
            other => panic!("expected MarketStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_parsed_frame_disconnect_variant() {
        let frame = ParsedFrame::Disconnect(DisconnectCode::AccessTokenExpired);
        match frame {
            ParsedFrame::Disconnect(code) => {
                assert!(code.requires_token_refresh());
            }
            other => panic!("expected Disconnect, got {other:?}"),
        }
    }

    #[test]
    fn test_packet_header_debug() {
        let hdr = PacketHeader {
            response_code: 8,
            message_length: 162,
            exchange_segment_code: 2,
            security_id: 49081,
        };
        let debug = format!("{hdr:?}");
        assert!(debug.contains("PacketHeader"));
        assert!(debug.contains("162"));
        assert!(debug.contains("49081"));
    }

    #[test]
    fn test_packet_header_clone() {
        let hdr = PacketHeader {
            response_code: 2,
            message_length: 16,
            exchange_segment_code: 1,
            security_id: 2885,
        };
        let cloned = hdr;
        assert_eq!(hdr.response_code, cloned.response_code);
        assert_eq!(hdr.message_length, cloned.message_length);
        assert_eq!(hdr.exchange_segment_code, cloned.exchange_segment_code);
        assert_eq!(hdr.security_id, cloned.security_id);
    }
}
