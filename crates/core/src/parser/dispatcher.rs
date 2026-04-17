//! Frame dispatcher — top-level entry point for binary protocol parsing.
//!
//! Routes raw WebSocket binary frames to the appropriate packet parser
//! based on the response code in the 8-byte header.

use tickvault_common::constants::{
    RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL, RESPONSE_CODE_INDEX_TICKER,
    RESPONSE_CODE_MARKET_DEPTH, RESPONSE_CODE_MARKET_STATUS, RESPONSE_CODE_OI,
    RESPONSE_CODE_PREVIOUS_CLOSE, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER,
};

use tickvault_common::constants::DEEP_DEPTH_HEADER_SIZE;

use super::deep_depth::{parse_deep_depth_header, parse_twenty_depth_packet};
use super::disconnect::parse_disconnect_packet;
use super::full_packet::parse_full_packet;
use super::header::parse_header;
use super::market_depth::parse_market_depth_packet;
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
#[inline]
pub fn dispatch_frame(raw: &[u8], received_at_nanos: i64) -> Result<ParsedFrame, ParseError> {
    let header = parse_header(raw)?;

    // Observability (§10.3): per-response-code packet counter so operators
    // can trend traffic mix in Grafana without scraping logs. Labelled by
    // the numeric code; the `unknown` bucket catches protocol drift.
    metrics::counter!(
        "tv_packets_by_response_code",
        "code" => response_code_label(header.response_code)
    )
    .increment(1);

    match header.response_code {
        RESPONSE_CODE_INDEX_TICKER | RESPONSE_CODE_TICKER => {
            let tick = parse_ticker_packet(raw, &header, received_at_nanos)?;
            Ok(ParsedFrame::Tick(tick))
        }
        RESPONSE_CODE_MARKET_DEPTH => {
            let (tick, depth) = parse_market_depth_packet(raw, &header, received_at_nanos)?;
            Ok(ParsedFrame::TickWithDepth(tick, depth))
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
        code => {
            // Observability (§10.3): protocol drift / Dhan sending a code
            // we don't handle. ERROR-level log triggers Telegram via Loki.
            metrics::counter!("tv_unknown_response_codes_total").increment(1);
            Err(ParseError::UnknownResponseCode(code))
        }
    }
}

/// Returns a static label for `tv_packets_by_response_code`. Returns
/// `&'static str` so Prometheus doesn't see a high-cardinality explosion
/// from unknown codes — everything unrecognized maps to the `unknown`
/// bucket (the actual numeric code is captured separately by
/// `tv_unknown_response_codes_total`).
fn response_code_label(code: u8) -> &'static str {
    match code {
        RESPONSE_CODE_INDEX_TICKER => "index_ticker",
        RESPONSE_CODE_TICKER => "ticker",
        RESPONSE_CODE_MARKET_DEPTH => "market_depth_v1",
        RESPONSE_CODE_QUOTE => "quote",
        RESPONSE_CODE_OI => "oi",
        RESPONSE_CODE_PREVIOUS_CLOSE => "prev_close",
        RESPONSE_CODE_MARKET_STATUS => "market_status",
        RESPONSE_CODE_FULL => "full",
        RESPONSE_CODE_DISCONNECT => "disconnect",
        _ => "unknown",
    }
}

/// Dispatches a single deep depth binary frame (20-level or 200-level).
///
/// Deep depth packets use a 12-byte header (not 8-byte) with different byte
/// layout from the standard live market feed. Bid (feed code 41) and Ask
/// (feed code 51) arrive as separate packets.
///
/// # Arguments
/// * `raw` — Complete binary frame from the depth WebSocket connection.
/// * `received_at_nanos` — Local receive timestamp in nanoseconds since Unix epoch.
///
/// # Returns
/// * `Ok(ParsedFrame::DeepDepth { .. })` — Successfully parsed depth frame.
/// * `Err(ParseError)` — Frame too short or unknown feed code.
///
/// # Performance
/// O(N) where N is level count (20 or 200) — fixed, bounded reads.
// TEST-EXEMPT: tested via test_dispatch_deep_depth_bid, test_dispatch_deep_depth_ask, test_dispatch_deep_depth_too_short
pub fn dispatch_deep_depth_frame(
    raw: &[u8],
    received_at_nanos: i64,
) -> Result<ParsedFrame, ParseError> {
    let parsed = parse_twenty_depth_packet(raw, received_at_nanos)?;

    Ok(ParsedFrame::DeepDepth {
        security_id: parsed.header.security_id,
        exchange_segment_code: parsed.header.exchange_segment_code,
        side: parsed.side,
        levels: parsed.levels,
        message_sequence: parsed.header.seq_or_row_count,
        received_at_nanos,
    })
}

/// Splits stacked 20-level depth packets from a single WebSocket message.
///
/// When multiple instruments are subscribed on a 20-level depth connection,
/// Dhan stacks their bid/ask packets sequentially in one WebSocket message:
/// `[Inst1 Bid][Inst1 Ask][Inst2 Bid][Inst2 Ask]...`
///
/// Each packet's length is read from its 12-byte header (bytes 0-1, u16 LE).
///
/// # Returns
/// Vec of byte slices, each pointing to one individual depth packet within `raw`.
///
/// # Performance
/// O(N) where N is number of stacked packets — bounded by subscription count.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: offset advances by validated message_length
// TEST-EXEMPT: tested via test_split_stacked_single_packet, test_split_stacked_bid_ask_pair, etc.
pub fn split_stacked_depth_packets(raw: &[u8]) -> Result<Vec<&[u8]>, ParseError> {
    // O(1) EXEMPT: begin — stacked packet splitting, runs once per WS message at receive time
    let mut packets = Vec::with_capacity(100); // max 50 instruments x 2 sides
    let mut offset = 0;

    while offset < raw.len() {
        let remaining = &raw[offset..];
        if remaining.len() < DEEP_DEPTH_HEADER_SIZE {
            break; // Trailing bytes shorter than header — ignore
        }

        let header = parse_deep_depth_header(remaining)?;
        let msg_len = header.message_length as usize;

        if msg_len == 0 || remaining.len() < msg_len {
            break; // Invalid or truncated packet — stop splitting
        }

        packets.push(&remaining[..msg_len]);
        offset += msg_len;
    }

    Ok(packets)
    // O(1) EXEMPT: end
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction
mod tests {
    use super::*;
    use crate::websocket::types::DisconnectCode;
    use tickvault_common::constants::{
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, MARKET_DEPTH_PACKET_SIZE,
        MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE, PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE,
        TICKER_PACKET_SIZE,
    };
    use tickvault_common::tick_types::{MarketDepthLevel, ParsedTick};

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
    fn test_dispatch_market_depth() {
        let buf = make_minimal_packet(RESPONSE_CODE_MARKET_DEPTH, MARKET_DEPTH_PACKET_SIZE);
        let (tick, depth) = unwrap_tick_with_depth(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
        assert_eq!(depth.len(), 5);
    }

    #[test]
    fn test_dispatch_market_depth_insufficient_body() {
        let mut buf = [0u8; 8];
        buf[0] = RESPONSE_CODE_MARKET_DEPTH;
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&buf, 0).unwrap_err());
        assert_eq!(expected, MARKET_DEPTH_PACKET_SIZE);
        assert_eq!(actual, 8);
    }

    #[test]
    fn test_dispatch_received_at_nanos_propagated_market_depth() {
        let buf = make_minimal_packet(RESPONSE_CODE_MARKET_DEPTH, MARKET_DEPTH_PACKET_SIZE);
        let nanos = 7_777_777_777_i64;
        let (tick, _) = unwrap_tick_with_depth(dispatch_frame(&buf, nanos).unwrap());
        assert_eq!(tick.received_at_nanos, nanos);
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

    #[test]
    fn test_dispatch_response_code_zero_is_unknown() {
        let buf = make_minimal_packet(0, 8);
        let err = dispatch_frame(&buf, 0).unwrap_err();
        assert!(matches!(err, ParseError::UnknownResponseCode(0)));
    }

    #[test]
    fn test_dispatch_zero_length_frame() {
        let (expected, actual) = unwrap_insufficient_bytes(dispatch_frame(&[], 0).unwrap_err());
        assert_eq!(expected, 8);
        assert_eq!(actual, 0);
    }

    #[test]
    fn test_dispatch_oversized_frame_parses_normally() {
        // Extra bytes beyond packet size are silently ignored
        let mut buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        buf.extend_from_slice(&[0xFF; 500]);
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
    }

    // -----------------------------------------------------------------------
    // Deep depth dispatch + stacked packet tests
    // -----------------------------------------------------------------------

    use crate::parser::deep_depth::DepthSide;
    use tickvault_common::constants::{
        DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID, DEEP_DEPTH_HEADER_SIZE,
        DEEP_DEPTH_LEVEL_SIZE, TWENTY_DEPTH_LEVELS,
    };

    fn make_depth_packet(feed_code: u8, security_id: u32, level_count: usize) -> Vec<u8> {
        let size = DEEP_DEPTH_HEADER_SIZE + level_count * DEEP_DEPTH_LEVEL_SIZE;
        let mut buf = vec![0u8; size];
        buf[0..2].copy_from_slice(&(size as u16).to_le_bytes());
        buf[2] = feed_code;
        buf[3] = 2; // NSE_FNO
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&1u32.to_le_bytes()); // sequence
        buf
    }

    #[test]
    fn test_dispatch_deep_depth_bid() {
        let buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 52432, TWENTY_DEPTH_LEVELS);
        let frame = dispatch_deep_depth_frame(&buf, 999).unwrap();
        match frame {
            ParsedFrame::DeepDepth {
                security_id,
                exchange_segment_code,
                side,
                levels,
                received_at_nanos,
                ..
            } => {
                assert_eq!(security_id, 52432);
                assert_eq!(exchange_segment_code, 2);
                assert_eq!(side, DepthSide::Bid);
                assert_eq!(levels.len(), 20);
                assert_eq!(received_at_nanos, 999);
            }
            other => panic!("expected DeepDepth, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_deep_depth_ask() {
        let buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_ASK, 2885, TWENTY_DEPTH_LEVELS);
        let frame = dispatch_deep_depth_frame(&buf, 0).unwrap();
        match frame {
            ParsedFrame::DeepDepth { side, .. } => assert_eq!(side, DepthSide::Ask),
            other => panic!("expected DeepDepth, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_deep_depth_too_short() {
        let buf = [0u8; 11]; // Less than 12-byte header
        let err = dispatch_deep_depth_frame(&buf, 0).unwrap_err();
        assert!(matches!(
            err,
            ParseError::InsufficientBytes {
                expected: 12,
                actual: 11
            }
        ));
    }

    #[test]
    fn test_split_stacked_single_packet() {
        let buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 52432, TWENTY_DEPTH_LEVELS);
        let packets = split_stacked_depth_packets(&buf).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), buf.len());
    }

    #[test]
    fn test_split_stacked_bid_ask_pair() {
        let bid = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 52432, TWENTY_DEPTH_LEVELS);
        let ask = make_depth_packet(DEEP_DEPTH_FEED_CODE_ASK, 52432, TWENTY_DEPTH_LEVELS);
        let mut stacked = Vec::new();
        stacked.extend_from_slice(&bid);
        stacked.extend_from_slice(&ask);

        let packets = split_stacked_depth_packets(&stacked).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].len(), bid.len());
        assert_eq!(packets[1].len(), ask.len());
    }

    #[test]
    fn test_split_stacked_multiple_instruments() {
        let mut stacked = Vec::new();
        for id in [52432, 52433, 52434] {
            stacked.extend_from_slice(&make_depth_packet(
                DEEP_DEPTH_FEED_CODE_BID,
                id,
                TWENTY_DEPTH_LEVELS,
            ));
            stacked.extend_from_slice(&make_depth_packet(
                DEEP_DEPTH_FEED_CODE_ASK,
                id,
                TWENTY_DEPTH_LEVELS,
            ));
        }

        let packets = split_stacked_depth_packets(&stacked).unwrap();
        assert_eq!(packets.len(), 6); // 3 instruments × 2 sides
    }

    #[test]
    fn test_split_stacked_empty() {
        let packets = split_stacked_depth_packets(&[]).unwrap();
        assert!(packets.is_empty());
    }

    #[test]
    fn test_split_stacked_trailing_bytes_ignored() {
        let mut buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 1, TWENTY_DEPTH_LEVELS);
        buf.extend_from_slice(&[0xFF; 5]); // trailing garbage < header size
        let packets = split_stacked_depth_packets(&buf).unwrap();
        assert_eq!(packets.len(), 1);
    }

    // --- Mutant-killing tests for split_stacked_depth_packets boundary conditions ---

    #[test]
    fn test_split_stacked_zero_msg_length_stops_splitting() {
        // A packet with message_length=0 in header must stop the splitter.
        // Kills mutant: `msg_len == 0` → `msg_len == 1`.
        let mut buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 1, TWENTY_DEPTH_LEVELS);
        // Append a 12-byte "header" with message_length = 0
        let mut zero_header = vec![0u8; DEEP_DEPTH_HEADER_SIZE];
        zero_header[2] = DEEP_DEPTH_FEED_CODE_BID;
        buf.extend_from_slice(&zero_header);
        let packets = split_stacked_depth_packets(&buf).unwrap();
        assert_eq!(
            packets.len(),
            1,
            "zero msg_length header must stop splitting"
        );
    }

    #[test]
    fn test_split_stacked_exact_fit_packet_included() {
        // A packet whose message_length exactly matches remaining bytes must be included.
        // Kills mutant: `remaining.len() < msg_len` → `remaining.len() <= msg_len`.
        let buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_ASK, 42, TWENTY_DEPTH_LEVELS);
        // The single packet's message_length == buf.len(), so remaining.len() == msg_len exactly.
        let packets = split_stacked_depth_packets(&buf).unwrap();
        assert_eq!(packets.len(), 1, "exact-fit packet must be included");
        assert_eq!(packets[0].len(), buf.len());
    }

    #[test]
    fn test_split_stacked_exactly_header_size_trailing_parsed() {
        // Exactly DEEP_DEPTH_HEADER_SIZE trailing bytes form a valid header
        // but with msg_len=12 (header only, no depth levels). The splitter should
        // include it as a packet since remaining.len() >= msg_len.
        // Kills mutant: `remaining.len() < DEEP_DEPTH_HEADER_SIZE` → `... <= ...`.
        let bid = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 1, TWENTY_DEPTH_LEVELS);
        let mut stacked = bid.clone();
        // Build a 12-byte header-only packet with msg_len = 12
        let mut tiny_header = vec![0u8; DEEP_DEPTH_HEADER_SIZE];
        tiny_header[0..2].copy_from_slice(&(DEEP_DEPTH_HEADER_SIZE as u16).to_le_bytes());
        tiny_header[2] = DEEP_DEPTH_FEED_CODE_BID;
        stacked.extend_from_slice(&tiny_header);
        let packets = split_stacked_depth_packets(&stacked).unwrap();
        assert_eq!(
            packets.len(),
            2,
            "12-byte header-only packet with msg_len=12 should be included"
        );
    }

    // -----------------------------------------------------------------------
    // Additional dispatcher coverage: deep depth received_at propagation,
    // split_stacked truncated mid-packet, market depth variant fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_dispatch_deep_depth_received_at_nanos_propagated() {
        let buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 42, TWENTY_DEPTH_LEVELS);
        let nanos = 1_234_567_890_123_456_789_i64;
        let frame = dispatch_deep_depth_frame(&buf, nanos).unwrap();
        match frame {
            ParsedFrame::DeepDepth {
                received_at_nanos, ..
            } => {
                assert_eq!(received_at_nanos, nanos);
            }
            other => panic!("expected DeepDepth, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_deep_depth_message_sequence_propagated() {
        let mut buf = make_depth_packet(DEEP_DEPTH_FEED_CODE_ASK, 42, TWENTY_DEPTH_LEVELS);
        // Set sequence number in header bytes 8-11
        buf[8..12].copy_from_slice(&777u32.to_le_bytes());
        let frame = dispatch_deep_depth_frame(&buf, 0).unwrap();
        match frame {
            ParsedFrame::DeepDepth {
                message_sequence, ..
            } => {
                assert_eq!(message_sequence, 777);
            }
            other => panic!("expected DeepDepth, got {other:?}"),
        }
    }

    #[test]
    fn test_split_stacked_truncated_mid_packet_stops() {
        // First packet is valid, second packet has msg_len > remaining bytes
        let valid_bid = make_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 1, TWENTY_DEPTH_LEVELS);
        let valid_len = valid_bid.len();
        let mut stacked = valid_bid;
        // Append a header claiming 500 bytes but only provide 12
        let mut truncated = vec![0u8; DEEP_DEPTH_HEADER_SIZE];
        truncated[0..2].copy_from_slice(&500u16.to_le_bytes());
        truncated[2] = DEEP_DEPTH_FEED_CODE_ASK;
        stacked.extend_from_slice(&truncated);
        let packets = split_stacked_depth_packets(&stacked).unwrap();
        assert_eq!(
            packets.len(),
            1,
            "truncated second packet should be ignored"
        );
        assert_eq!(packets[0].len(), valid_len);
    }

    #[test]
    fn test_dispatch_market_depth_has_correct_tick_fields() {
        let mut buf = make_minimal_packet(RESPONSE_CODE_MARKET_DEPTH, MARKET_DEPTH_PACKET_SIZE);
        // Set LTP at offset 8-11
        buf[8..12].copy_from_slice(&1500.5_f32.to_le_bytes());
        let (tick, depth) = unwrap_tick_with_depth(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
        assert_eq!(tick.exchange_segment_code, 2);
        // Depth should have 5 levels
        assert_eq!(depth.len(), 5);
    }

    #[test]
    fn test_dispatch_all_unknown_response_codes() {
        // Test several codes that are not valid response codes
        for code in [9, 10, 11, 12, 13, 14, 15, 20, 30, 40, 100, 255] {
            let buf = make_minimal_packet(code, 8);
            let err = dispatch_frame(&buf, 0).unwrap_err();
            assert!(
                matches!(err, ParseError::UnknownResponseCode(c) if c == code),
                "code {code} should be unknown"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Additional coverage: stacked frames with correct instrument extraction,
    // header unknown exchange byte propagation
    // -----------------------------------------------------------------------

    #[test]
    fn test_dispatch_header_unknown_exchange_byte_propagated() {
        // Exchange segment byte 6 (the gap in Dhan's enum) is passed through
        // by the dispatcher into the parsed frame.
        let mut buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        buf[3] = 6; // Unknown segment byte
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.exchange_segment_code, 6);
    }

    #[test]
    fn test_dispatch_header_exchange_byte_255_propagated() {
        // Byte 255 is also unknown but must parse without panic
        let mut buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        buf[3] = 255;
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.exchange_segment_code, 255);
    }

    #[test]
    fn test_split_stacked_frames_correct_security_ids() {
        // Stack 3 bid packets for different instruments and verify each
        // packet's security_id is decoded correctly after splitting.
        let ids = [52432u32, 2885, 99999];
        let mut stacked = Vec::new();
        for &id in &ids {
            stacked.extend_from_slice(&make_depth_packet(
                DEEP_DEPTH_FEED_CODE_BID,
                id,
                TWENTY_DEPTH_LEVELS,
            ));
        }

        let packets = split_stacked_depth_packets(&stacked).unwrap();
        assert_eq!(packets.len(), 3);

        for (i, &id) in ids.iter().enumerate() {
            let frame = dispatch_deep_depth_frame(packets[i], 0).unwrap();
            match frame {
                ParsedFrame::DeepDepth { security_id, .. } => {
                    assert_eq!(
                        security_id, id,
                        "stacked packet {i} should have security_id={id}"
                    );
                }
                other => panic!("expected DeepDepth, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_split_stacked_interleaved_bid_ask_security_ids() {
        // Bid + Ask for instrument A, then Bid + Ask for instrument B
        let mut stacked = Vec::new();
        stacked.extend_from_slice(&make_depth_packet(
            DEEP_DEPTH_FEED_CODE_BID,
            1000,
            TWENTY_DEPTH_LEVELS,
        ));
        stacked.extend_from_slice(&make_depth_packet(
            DEEP_DEPTH_FEED_CODE_ASK,
            1000,
            TWENTY_DEPTH_LEVELS,
        ));
        stacked.extend_from_slice(&make_depth_packet(
            DEEP_DEPTH_FEED_CODE_BID,
            2000,
            TWENTY_DEPTH_LEVELS,
        ));
        stacked.extend_from_slice(&make_depth_packet(
            DEEP_DEPTH_FEED_CODE_ASK,
            2000,
            TWENTY_DEPTH_LEVELS,
        ));

        let packets = split_stacked_depth_packets(&stacked).unwrap();
        assert_eq!(packets.len(), 4);

        // Verify sides alternate and security_ids match
        let expected = [
            (1000u32, DepthSide::Bid),
            (1000, DepthSide::Ask),
            (2000, DepthSide::Bid),
            (2000, DepthSide::Ask),
        ];
        for (i, (expected_id, expected_side)) in expected.iter().enumerate() {
            let frame = dispatch_deep_depth_frame(packets[i], 0).unwrap();
            match frame {
                ParsedFrame::DeepDepth {
                    security_id, side, ..
                } => {
                    assert_eq!(security_id, *expected_id, "packet {i} security_id");
                    assert_eq!(side, *expected_side, "packet {i} side");
                }
                other => panic!("expected DeepDepth, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_dispatch_ticker_nan_ltp_parsed() {
        // Ticker packet with NaN LTP: parser reads NaN without panic,
        // downstream tick_processor will filter it.
        let mut buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        buf[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert!(tick.last_traded_price.is_nan());
    }
}
