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

// APPROVED: test code — relaxed lint rules for test fixtures
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::types::DisconnectCode;
    use dhan_live_trader_common::constants::{
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
        PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
    };
    use dhan_live_trader_common::tick_types::{MarketDepthLevel, ParsedTick};

    // Extraction helpers — each panic arm appears only once.
    fn unwrap_tick(frame: ParsedFrame) -> ParsedTick {
        match frame {
            ParsedFrame::Tick(t) => t,
            other => panic!("expected Tick, got {other:?}"),
        }
    }
    fn unwrap_tick_with_depth(frame: ParsedFrame) -> (ParsedTick, [MarketDepthLevel; 5]) {
        match frame {
            ParsedFrame::TickWithDepth(t, d) => (t, d),
            other => panic!("expected TickWithDepth, got {other:?}"),
        }
    }
    fn unwrap_oi(frame: ParsedFrame) -> (u32, u8, u32) {
        match frame {
            ParsedFrame::OiUpdate {
                security_id,
                exchange_segment_code,
                open_interest,
            } => (security_id, exchange_segment_code, open_interest),
            other => panic!("expected OiUpdate, got {other:?}"),
        }
    }
    fn unwrap_prev_close(frame: ParsedFrame) -> (u32, u8, f32, u32) {
        match frame {
            ParsedFrame::PreviousClose {
                security_id,
                exchange_segment_code,
                previous_close,
                previous_oi,
            } => (
                security_id,
                exchange_segment_code,
                previous_close,
                previous_oi,
            ),
            other => panic!("expected PreviousClose, got {other:?}"),
        }
    }
    fn unwrap_market_status(frame: ParsedFrame) -> (u32, u8) {
        match frame {
            ParsedFrame::MarketStatus {
                security_id,
                exchange_segment_code,
            } => (security_id, exchange_segment_code),
            other => panic!("expected MarketStatus, got {other:?}"),
        }
    }
    fn unwrap_disconnect(frame: ParsedFrame) -> DisconnectCode {
        match frame {
            ParsedFrame::Disconnect(c) => c,
            other => panic!("expected Disconnect, got {other:?}"),
        }
    }
    fn unwrap_insufficient_bytes(err: ParseError) -> (usize, usize) {
        match err {
            ParseError::InsufficientBytes { expected, actual } => (expected, actual),
            other => panic!("expected InsufficientBytes, got {other:?}"),
        }
    }

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
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
    }

    #[test]
    fn test_dispatch_ticker() {
        let buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
    }

    #[test]
    fn test_dispatch_quote() {
        let buf = make_minimal_packet(RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE);
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
    }

    #[test]
    fn test_dispatch_full() {
        let buf = make_minimal_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        let (tick, depth) = unwrap_tick_with_depth(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
        assert_eq!(depth.len(), 5);
    }

    #[test]
    fn test_dispatch_oi() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        buf[8..12].copy_from_slice(&150000u32.to_le_bytes());
        let (sid, _, oi) = unwrap_oi(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(sid, 42);
        assert_eq!(oi, 150000);
    }

    #[test]
    fn test_dispatch_previous_close() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[8..12].copy_from_slice(&24_300.5_f32.to_le_bytes());
        buf[12..16].copy_from_slice(&120000u32.to_le_bytes());
        let (sid, _, pc, poi) = unwrap_prev_close(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(sid, 42);
        assert!((pc - 24_300.5).abs() < 0.01);
        assert_eq!(poi, 120000);
    }

    #[test]
    fn test_dispatch_market_status() {
        let buf = make_minimal_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        let (sid, _) = unwrap_market_status(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(sid, 42);
    }

    #[test]
    fn test_dispatch_disconnect() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        buf[8..10].copy_from_slice(&807u16.to_le_bytes());
        let code = unwrap_disconnect(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(code, DisconnectCode::AccessTokenExpired);
    }

    #[test]
    fn test_dispatch_unknown_response_code() {
        let buf = make_minimal_packet(99, 8);
        let err = dispatch_frame(&buf, 0).unwrap_err();
        assert!(matches!(err, ParseError::UnknownResponseCode(99)));
    }

    #[test]
    fn test_dispatch_empty_buffer() {
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&[], 0).unwrap_err());
        assert_eq!(expected, 8);
        assert_eq!(actual, 0);
    }

    #[test]
    fn test_dispatch_exactly_8_bytes_header_only_unknown_code() {
        let mut buf = [0u8; 8];
        buf[0] = 200;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let err = dispatch_frame(&buf, 0).unwrap_err();
        assert!(matches!(err, ParseError::UnknownResponseCode(200)));
    }

    #[test]
    fn test_dispatch_8_bytes_ticker_code_insufficient_for_body() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_TICKER;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&buf, 0).unwrap_err());
        assert_eq!(expected, 16);
        assert_eq!(actual, 8);
    }

    #[test]
    fn test_dispatch_received_at_nanos_propagated_ticker() {
        let buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        let nanos = 1_740_556_500_123_456_789_i64;
        let tick = unwrap_tick(dispatch_frame(&buf, nanos).unwrap());
        assert_eq!(tick.received_at_nanos, nanos);
    }

    #[test]
    fn test_dispatch_received_at_nanos_propagated_index_ticker() {
        let buf = make_minimal_packet(RESPONSE_CODE_INDEX_TICKER, TICKER_PACKET_SIZE);
        let nanos = 9_999_999_999_i64;
        let tick = unwrap_tick(dispatch_frame(&buf, nanos).unwrap());
        assert_eq!(tick.received_at_nanos, nanos);
    }

    #[test]
    fn test_dispatch_received_at_nanos_propagated_quote() {
        let buf = make_minimal_packet(RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE);
        let nanos = 1_234_567_890_i64;
        let tick = unwrap_tick(dispatch_frame(&buf, nanos).unwrap());
        assert_eq!(tick.received_at_nanos, nanos);
    }

    #[test]
    fn test_dispatch_received_at_nanos_propagated_full() {
        let buf = make_minimal_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        let nanos = 5_555_555_555_i64;
        let (tick, _) = unwrap_tick_with_depth(dispatch_frame(&buf, nanos).unwrap());
        assert_eq!(tick.received_at_nanos, nanos);
    }

    #[test]
    fn test_dispatch_7_bytes_too_short() {
        let (expected, actual) =
            unwrap_insufficient_bytes(dispatch_frame(&[0u8; 7], 0).unwrap_err());
        assert_eq!(expected, 8);
        assert_eq!(actual, 7);
    }

    #[test]
    fn test_dispatch_oi_does_not_use_received_at_nanos() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        buf[8..12].copy_from_slice(&999u32.to_le_bytes());
        let (sid, seg, oi) = unwrap_oi(dispatch_frame(&buf, 42).unwrap());
        assert_eq!(sid, 42);
        assert_eq!(seg, 2);
        assert_eq!(oi, 999);
    }

    #[test]
    fn test_dispatch_previous_close_exchange_segment_propagated() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[3] = 0; // IDX_I segment
        buf[8..12].copy_from_slice(&100.0_f32.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        let (_, seg, _, _) = unwrap_prev_close(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(seg, 0);
    }

    #[test]
    fn test_dispatch_market_status_exchange_segment_propagated() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        buf[3] = 1; // NSE_EQ segment
        buf[4..8].copy_from_slice(&99u32.to_le_bytes());
        let (sid, seg) = unwrap_market_status(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(sid, 99);
        assert_eq!(seg, 1);
    }

    // -----------------------------------------------------------------------
    // Additional edge cases for packet body parsing errors
    // -----------------------------------------------------------------------

    #[test]
    fn test_dispatch_quote_insufficient_body() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_QUOTE;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&buf, 0).unwrap_err());
        assert!(
            expected > 8,
            "quote needs more than 8 bytes, expected: {expected}"
        );
        assert_eq!(actual, 8);
    }

    #[test]
    fn test_dispatch_full_insufficient_body() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_FULL;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&buf, 0).unwrap_err());
        assert!(
            expected > 8,
            "full packet needs more than 8 bytes, expected: {expected}"
        );
        assert_eq!(actual, 8);
    }

    #[test]
    fn test_dispatch_oi_insufficient_body() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_OI;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&buf, 0).unwrap_err());
        assert!(expected > 8);
        assert_eq!(actual, 8);
    }

    #[test]
    fn test_dispatch_previous_close_insufficient_body() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_PREVIOUS_CLOSE;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&buf, 0).unwrap_err());
        assert!(expected > 8);
        assert_eq!(actual, 8);
    }

    #[test]
    fn test_dispatch_disconnect_insufficient_body() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_DISCONNECT;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&buf, 0).unwrap_err());
        assert!(expected > 8);
        assert_eq!(actual, 8);
    }

    #[test]
    fn test_dispatch_market_status_exactly_header_size_succeeds() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_MARKET_STATUS;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (sid, _) = unwrap_market_status(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(sid, 42);
    }

    #[test]
    fn test_dispatch_index_ticker_with_max_security_id() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_INDEX_TICKER, TICKER_PACKET_SIZE);
        buf[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, u32::MAX);
    }

    #[test]
    fn test_dispatch_disconnect_unknown_code() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        buf[8..10].copy_from_slice(&999u16.to_le_bytes());
        let code = unwrap_disconnect(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(code, DisconnectCode::Unknown(999));
    }

    #[test]
    fn test_dispatch_1_byte_too_short() {
        let (expected, actual) =
            unwrap_insufficient_bytes(dispatch_frame(&[0u8; 1], 0).unwrap_err());
        assert_eq!(expected, 8);
        assert_eq!(actual, 1);
    }

    #[test]
    fn test_dispatch_full_packet_has_five_depth_levels() {
        let buf = make_minimal_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        let (_, depth) = unwrap_tick_with_depth(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(
            depth.len(),
            5,
            "full packet must have exactly 5 depth levels"
        );
    }

    #[test]
    fn test_dispatch_oi_with_zero_interest() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        let (_, _, oi) = unwrap_oi(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(oi, 0);
    }

    #[test]
    fn test_dispatch_previous_close_with_zero_values() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[8..12].copy_from_slice(&0.0_f32.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        let (_, _, pc, poi) = unwrap_prev_close(dispatch_frame(&buf, 0).unwrap());
        assert!((pc - 0.0).abs() < f32::EPSILON);
        assert_eq!(poi, 0);
    }
}
