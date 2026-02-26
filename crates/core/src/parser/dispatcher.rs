//! Frame dispatcher — top-level entry point for binary protocol parsing.
//!
//! Routes raw WebSocket binary frames to the appropriate packet parser
//! based on the response code in the 8-byte header.

use dhan_live_trader_common::constants::{
    RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL, RESPONSE_CODE_INDEX_TICKER,
    RESPONSE_CODE_MARKET_STATUS, RESPONSE_CODE_OI, RESPONSE_CODE_PREVIOUS_CLOSE,
    RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER,
};

use super::disconnect::parse_disconnect_packet;
use super::full_packet::parse_full_packet;
use super::header::parse_header;
use super::market_status::validate_market_status_packet;
use super::oi::parse_oi_packet;
use super::previous_close::parse_previous_close_packet;
use super::quote::parse_quote_packet;
use super::ticker::parse_ticker_packet;
use super::types::{ParseError, ParsedFrame};

/// Dispatches a raw WebSocket binary frame to the correct parser.
///
/// This is the single entry point for the binary protocol parser.
/// It reads the 8-byte header, determines the packet type from the
/// response code, and delegates to the appropriate parser function.
///
/// # Arguments
/// * `raw` — Complete binary frame from the WebSocket connection.
/// * `received_at_nanos` — Local receive timestamp in nanoseconds since Unix epoch.
///
/// # Returns
/// * `Ok(ParsedFrame)` — Successfully parsed frame.
/// * `Err(ParseError)` — Frame too short or unknown response code.
///
/// # Performance
/// O(1) — header parse + single packet parse. No heap allocation.
pub fn dispatch_frame(raw: &[u8], received_at_nanos: i64) -> Result<ParsedFrame, ParseError> {
    let header = parse_header(raw)?;

    match header.response_code {
        RESPONSE_CODE_INDEX_TICKER | RESPONSE_CODE_TICKER => {
            let tick = parse_ticker_packet(raw, &header, received_at_nanos)?;
            Ok(ParsedFrame::Tick(tick))
        }
        RESPONSE_CODE_QUOTE => {
            let tick = parse_quote_packet(raw, &header, received_at_nanos)?;
            Ok(ParsedFrame::Tick(tick))
        }
        RESPONSE_CODE_FULL => {
            let (tick, depth) = parse_full_packet(raw, &header, received_at_nanos)?;
            Ok(ParsedFrame::TickWithDepth(tick, depth))
        }
        RESPONSE_CODE_OI => {
            let oi = parse_oi_packet(raw, &header)?;
            Ok(ParsedFrame::OiUpdate {
                security_id: header.security_id,
                exchange_segment_code: header.exchange_segment_code,
                open_interest: oi,
            })
        }
        RESPONSE_CODE_PREVIOUS_CLOSE => {
            let data = parse_previous_close_packet(raw, &header)?;
            Ok(ParsedFrame::PreviousClose {
                security_id: header.security_id,
                exchange_segment_code: header.exchange_segment_code,
                previous_close: data.previous_close,
                previous_oi: data.previous_oi,
            })
        }
        RESPONSE_CODE_MARKET_STATUS => {
            validate_market_status_packet(raw)?;
            Ok(ParsedFrame::MarketStatus {
                security_id: header.security_id,
                exchange_segment_code: header.exchange_segment_code,
            })
        }
        RESPONSE_CODE_DISCONNECT => {
            let code = parse_disconnect_packet(raw, &header)?;
            Ok(ParsedFrame::Disconnect(code))
        }
        code => Err(ParseError::UnknownResponseCode(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::types::DisconnectCode;
    use dhan_live_trader_common::constants::{
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
        PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
    };

    /// Helper: builds a minimal valid packet for a given response code.
    fn make_minimal_packet(response_code: u8, size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; size];
        buf[0] = response_code;
        buf[1..3].copy_from_slice(&(size as u16).to_le_bytes());
        buf[3] = 2; // NSE_FNO
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        buf
    }

    #[test]
    fn test_dispatch_index_ticker() {
        let buf = make_minimal_packet(RESPONSE_CODE_INDEX_TICKER, TICKER_PACKET_SIZE);
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::Tick(tick) => assert_eq!(tick.security_id, 42),
            other => panic!("expected Tick, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_ticker() {
        let buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::Tick(tick) => assert_eq!(tick.security_id, 42),
            other => panic!("expected Tick, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_quote() {
        let buf = make_minimal_packet(RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE);
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::Tick(tick) => assert_eq!(tick.security_id, 42),
            other => panic!("expected Tick, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_full() {
        let buf = make_minimal_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::TickWithDepth(tick, depth) => {
                assert_eq!(tick.security_id, 42);
                assert_eq!(depth.len(), 5);
            }
            other => panic!("expected TickWithDepth, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_oi() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        buf[8..12].copy_from_slice(&150000u32.to_le_bytes());
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::OiUpdate {
                security_id,
                open_interest,
                ..
            } => {
                assert_eq!(security_id, 42);
                assert_eq!(open_interest, 150000);
            }
            other => panic!("expected OiUpdate, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_previous_close() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[8..12].copy_from_slice(&24300.50f32.to_le_bytes());
        buf[12..16].copy_from_slice(&120000u32.to_le_bytes());
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::PreviousClose {
                security_id,
                previous_close,
                previous_oi,
                ..
            } => {
                assert_eq!(security_id, 42);
                assert!((previous_close - 24300.50).abs() < 0.01);
                assert_eq!(previous_oi, 120000);
            }
            other => panic!("expected PreviousClose, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_market_status() {
        let buf = make_minimal_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::MarketStatus { security_id, .. } => {
                assert_eq!(security_id, 42);
            }
            other => panic!("expected MarketStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_disconnect() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        buf[8..10].copy_from_slice(&807u16.to_le_bytes());
        match dispatch_frame(&buf, 0).unwrap() {
            ParsedFrame::Disconnect(code) => {
                assert_eq!(code, DisconnectCode::AccessTokenExpired);
            }
            other => panic!("expected Disconnect, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_unknown_response_code() {
        let buf = make_minimal_packet(99, 8);
        let err = dispatch_frame(&buf, 0).unwrap_err();
        match err {
            ParseError::UnknownResponseCode(99) => {}
            _ => panic!("wrong error: {err:?}"),
        }
    }

    #[test]
    fn test_dispatch_empty_buffer() {
        let err = dispatch_frame(&[], 0).unwrap_err();
        match err {
            ParseError::InsufficientBytes {
                expected: 8,
                actual: 0,
            } => {}
            _ => panic!("wrong error: {err:?}"),
        }
    }
}
