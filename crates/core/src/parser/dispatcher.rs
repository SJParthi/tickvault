//! Frame dispatcher — top-level entry point for binary protocol parsing.
//!
//! Routes raw WebSocket binary frames to the appropriate packet parser
//! based on the response code in the 8-byte header.

use std::sync::OnceLock;

use metrics::Counter;
use tickvault_common::constants::{
    DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID, DISCONNECT_PACKET_SIZE,
    FULL_QUOTE_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE, PREVIOUS_CLOSE_PACKET_SIZE,
    QUOTE_PACKET_SIZE, RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL, RESPONSE_CODE_INDEX_TICKER,
    RESPONSE_CODE_MARKET_DEPTH, RESPONSE_CODE_MARKET_STATUS, RESPONSE_CODE_OI,
    RESPONSE_CODE_PREVIOUS_CLOSE, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE,
};

// PR #4 (2026-05-19): DEEP_DEPTH_HEADER_SIZE retired alongside the
// deleted deep_depth + market_depth parser modules.

use super::depth::{
    DepthFeedKind, DepthLevelBuffer, DepthPacket, DepthPayload, DepthSide, parse_depth_packet,
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

// O(1) hot-path Counter handles. The `metrics::counter!()` macro with
// labels allocates a Vec<Label> per invocation; caching the resolved
// Counter handle in a OnceLock keeps the hot path zero-allocation
// (Principle #1) after the first call. See `dhat_all_parsers_zero_alloc`.
static C_INDEX_TICKER: OnceLock<Counter> = OnceLock::new();
static C_TICKER: OnceLock<Counter> = OnceLock::new();
static C_MARKET_DEPTH_V1: OnceLock<Counter> = OnceLock::new();
static C_QUOTE: OnceLock<Counter> = OnceLock::new();
static C_OI: OnceLock<Counter> = OnceLock::new();
static C_PREV_CLOSE: OnceLock<Counter> = OnceLock::new();
static C_MARKET_STATUS: OnceLock<Counter> = OnceLock::new();
static C_FULL: OnceLock<Counter> = OnceLock::new();
static C_DISCONNECT: OnceLock<Counter> = OnceLock::new();
static C_UNKNOWN: OnceLock<Counter> = OnceLock::new();
static C_UNKNOWN_RESPONSE_CODES_TOTAL: OnceLock<Counter> = OnceLock::new();
// Depth-feed arms. Deliberately reuse the SAME metric name with new label
// values rather than minting a new metric — same "packets by code" semantics,
// zero new registration surface.
static C_DEPTH_BID: OnceLock<Counter> = OnceLock::new();
static C_DEPTH_ASK: OnceLock<Counter> = OnceLock::new();
static C_DEPTH_DISCONNECT: OnceLock<Counter> = OnceLock::new();
static C_DEPTH_UNKNOWN: OnceLock<Counter> = OnceLock::new();

#[inline]
fn dispatcher_counter(code: u8) -> &'static Counter {
    match code {
        RESPONSE_CODE_INDEX_TICKER => C_INDEX_TICKER.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "index_ticker"),
        ),
        RESPONSE_CODE_TICKER => C_TICKER
            .get_or_init(|| metrics::counter!("tv_packets_by_response_code", "code" => "ticker")),
        RESPONSE_CODE_MARKET_DEPTH => C_MARKET_DEPTH_V1.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "market_depth_v1"),
        ),
        RESPONSE_CODE_QUOTE => C_QUOTE
            .get_or_init(|| metrics::counter!("tv_packets_by_response_code", "code" => "quote")),
        RESPONSE_CODE_OI => {
            C_OI.get_or_init(|| metrics::counter!("tv_packets_by_response_code", "code" => "oi"))
        }
        RESPONSE_CODE_PREVIOUS_CLOSE => C_PREV_CLOSE.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "prev_close"),
        ),
        RESPONSE_CODE_MARKET_STATUS => C_MARKET_STATUS.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "market_status"),
        ),
        RESPONSE_CODE_FULL => C_FULL
            .get_or_init(|| metrics::counter!("tv_packets_by_response_code", "code" => "full")),
        RESPONSE_CODE_DISCONNECT => C_DISCONNECT.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "disconnect"),
        ),
        _ => C_UNKNOWN
            .get_or_init(|| metrics::counter!("tv_packets_by_response_code", "code" => "unknown")),
    }
}

/// O(1) cached Counter handle for a depth-feed packet code (41 / 51 / 50).
#[inline]
fn depth_dispatcher_counter(code: u8) -> &'static Counter {
    match DepthSide::from_feed_code(code) {
        Some(DepthSide::Bid) => C_DEPTH_BID.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "depth_bid"),
        ),
        Some(DepthSide::Ask) => C_DEPTH_ASK.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "depth_ask"),
        ),
        None if code == RESPONSE_CODE_DISCONNECT => C_DEPTH_DISCONNECT.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "depth_disconnect"),
        ),
        None => C_DEPTH_UNKNOWN.get_or_init(
            || metrics::counter!("tv_packets_by_response_code", "code" => "depth_unknown"),
        ),
    }
}

#[inline]
fn unknown_response_codes_counter() -> &'static Counter {
    C_UNKNOWN_RESPONSE_CODES_TOTAL
        .get_or_init(|| metrics::counter!("tv_unknown_response_codes_total"))
}

/// Pre-register every dispatcher Counter handle so the hot path never
/// allocates. Must be called once at boot AFTER the global metrics
/// recorder is installed (otherwise the cached handles will be `noop`
/// counters that ignore increments forever).
///
/// Safe to call multiple times — `OnceLock::get_or_init` is idempotent.
pub fn prewarm_dispatcher_counters() {
    // Touch every label arm so each cell holds a real Counter handle.
    // Using a sentinel byte for the "unknown" arm; any value not in the
    // known set drops into `_ =>` and initializes `C_UNKNOWN`.
    dispatcher_counter(RESPONSE_CODE_INDEX_TICKER);
    dispatcher_counter(RESPONSE_CODE_TICKER);
    dispatcher_counter(RESPONSE_CODE_MARKET_DEPTH);
    dispatcher_counter(RESPONSE_CODE_QUOTE);
    dispatcher_counter(RESPONSE_CODE_OI);
    dispatcher_counter(RESPONSE_CODE_PREVIOUS_CLOSE);
    dispatcher_counter(RESPONSE_CODE_MARKET_STATUS);
    dispatcher_counter(RESPONSE_CODE_FULL);
    dispatcher_counter(RESPONSE_CODE_DISCONNECT);
    dispatcher_counter(0xFF);
    depth_dispatcher_counter(DEEP_DEPTH_FEED_CODE_BID);
    depth_dispatcher_counter(DEEP_DEPTH_FEED_CODE_ASK);
    depth_dispatcher_counter(RESPONSE_CODE_DISCONNECT);
    depth_dispatcher_counter(0xFF);
    unknown_response_codes_counter();
}

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
    // Counter handle is cached in a per-label OnceLock to keep the hot
    // path zero-allocation (Principle #1) — see `dispatcher_counter`.
    dispatcher_counter(header.response_code).increment(1);

    match header.response_code {
        RESPONSE_CODE_INDEX_TICKER | RESPONSE_CODE_TICKER => {
            let tick = parse_ticker_packet(raw, &header, received_at_nanos)?;
            Ok(ParsedFrame::Tick(tick))
        }
        // PR #4 (2026-05-19): RESPONSE_CODE_MARKET_DEPTH (v1 legacy, code 3)
        // arm retired — v1 is deprecated in v2 (replaced by Full code 8).
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
            unknown_response_codes_counter().increment(1);
            Err(ParseError::UnknownResponseCode(code))
        }
    }
}

// PR #4 (2026-05-19): `dispatch_deep_depth_frame` + `split_stacked_depth_packets`
// fns retired alongside deleted deep_depth + market_depth parser modules.
// 2026-08-09: the depth entry point returns under a NEW shape — the frame
// splitter lives in `depth::split_depth_frame` (an allocation-free iterator,
// because one frame can carry many stacked packets) and this dispatcher parses
// ONE already-split packet.

/// Dispatches ONE depth-feed packet, already split out of its frame.
///
/// This is the depth-feed twin of [`dispatch_frame`] and is a SEPARATE entry
/// point on purpose: depth packets carry a 12-byte header with `f64` prices,
/// so routing them through `dispatch_frame`'s 8-byte header would mis-read
/// every field and produce plausible-but-wrong prices.
///
/// Callers MUST split the frame first — a single WebSocket frame may carry
/// several stacked packets:
///
/// ```ignore
/// let mut buf = DepthLevelBuffer::new();
/// let mut frames = split_depth_frame(raw, DepthFeedKind::Twenty);
/// for packet in frames.by_ref() {
///     let parsed = dispatch_depth_packet(packet, DepthFeedKind::Twenty, &mut buf)?;
/// }
/// // `frames.stop_reason()` reports a malformed tail rather than hiding it.
/// ```
///
/// `kind` cannot be derived from the bytes — depth-20 and depth-200 share
/// codes 41/51 and differ only in the meaning of header bytes 8..12 — so the
/// connection supplies it.
///
/// # Errors
/// Propagates every [`ParseError`] from [`parse_depth_packet`]: a short frame,
/// an unknown response code, or a depth-200 row count above 200.
///
/// # Performance
/// O(levels), bounded at 200. Zero heap allocation — levels are written into
/// the caller's reusable buffer and the counter handles are cached.
#[inline]
pub fn dispatch_depth_packet<'b>(
    raw: &[u8],
    kind: DepthFeedKind,
    buf: &'b mut DepthLevelBuffer,
) -> Result<DepthPacket<'b>, ParseError> {
    let packet = match parse_depth_packet(raw, kind, buf) {
        Ok(packet) => packet,
        Err(err) => {
            if let ParseError::UnknownResponseCode(code) = err {
                depth_dispatcher_counter(code).increment(1);
                unknown_response_codes_counter().increment(1);
            }
            return Err(err);
        }
    };
    let code = match packet.payload {
        DepthPayload::Levels { side, .. } => side.as_feed_code(),
        DepthPayload::Disconnect { .. } => RESPONSE_CODE_DISCONNECT,
    };
    depth_dispatcher_counter(code).increment(1);
    Ok(packet)
}

/// Byte length of the main-feed packet starting at `bytes`, from its response
/// code. `None` for an unknown code or a header too short to classify.
///
/// The header carries its own message length at bytes 1..3, but that field is
/// vendor-supplied: trusting it would let a malformed length walk the parser
/// off the end of one packet and into the middle of the next. The code -> size
/// table is ours and is fixed by the protocol.
///
/// This lives in `core` rather than beside its caller because TWO walks depend
/// on agreeing byte-for-byte: the frame drain that decodes packets, and
/// [`crate::websocket::connection::classify_frame`], which walks the same bytes
/// looking for a stacked disconnect. When those two disagree, the drain decodes
/// a disconnect the classifier never saw -- which is exactly the defect the
/// 2026-08-25 audit found. One function makes them agree by construction.
#[must_use]
pub fn main_feed_packet_len(bytes: &[u8]) -> Option<usize> {
    let code = *bytes.first()?;
    let size = match code {
        // Code 1 and code 2 are the SAME 16-byte ticker packet, and
        // `dispatch_frame` accepts them in ONE arm. Code 1 was missing from
        // this table until 2026-08-26, so a frame whose first packet was an
        // INDEX ticker walked no further: the drain counted `unparseable`,
        // abandoned the WHOLE remaining frame, and `stacked_disconnect_reason`
        // returned `None` -- putting an 804 back out of reach behind exactly
        // the shape the walk was built to see through. Latent rather than
        // live only because IDX_I subscribes in Quote mode today; the
        // 2026-08-21 scope-lock names `Ticker` as the next value to try if
        // Dhan refuses Quote for indices, which is one config line away from
        // discarding every index frame.
        RESPONSE_CODE_INDEX_TICKER => TICKER_PACKET_SIZE,
        RESPONSE_CODE_TICKER => TICKER_PACKET_SIZE,
        RESPONSE_CODE_QUOTE => QUOTE_PACKET_SIZE,
        RESPONSE_CODE_OI => OI_PACKET_SIZE,
        RESPONSE_CODE_PREVIOUS_CLOSE => PREVIOUS_CLOSE_PACKET_SIZE,
        RESPONSE_CODE_MARKET_STATUS => MARKET_STATUS_PACKET_SIZE,
        RESPONSE_CODE_FULL => FULL_QUOTE_PACKET_SIZE,
        RESPONSE_CODE_DISCONNECT => DISCONNECT_PACKET_SIZE,
        _ => return None,
    };
    Some(size)
}

/// The reason code of a disconnect packet STACKED anywhere inside a main-feed
/// frame, found by walking the packet boundaries rather than scanning bytes.
///
/// A Dhan frame stacks packets, so a disconnect can arrive with data ahead of
/// it -- and until 2026-08-25 that shape was invisible: the classifier compared
/// the WHOLE frame length against 10 and called anything else data, so a
/// `[quote][disconnect 804]` frame was handed to the drain, which decoded the
/// disconnect and folded it into an untyped "non-tick" count. The reason code
/// reached no log, no metric and no classifier. 804 means the subscribe set
/// exceeds the per-connection cap, and the repo already rules it Fatal
/// precisely because retrying re-sends the identical over-limit set forever and
/// can earn a 805 block -- so the one code that must never be retried was the
/// one being retried blind.
///
/// Structural, not a byte scan: a value of 50 inside a quote packet's payload
/// is never at a packet boundary and so is never read as a code. An unwalkable
/// frame yields `None` and stays data, keeping the fail-safe direction the
/// classifier already documents.
///
/// O(packets per frame), no allocation.
#[must_use]
pub fn stacked_disconnect_reason(frame: &[u8], max_packets: u32) -> Option<u16> {
    let mut offset = 0usize;
    let mut packets = 0u32;
    let mut found: Option<u16> = None;
    while offset < frame.len() {
        let rest = frame.get(offset..)?;
        let len = main_feed_packet_len(rest)?;
        let end = offset.checked_add(len)?;
        if end > frame.len() {
            // A trailing partial packet: refuse to guess at its contents.
            return None;
        }
        if found.is_none() && rest.first().copied() == Some(RESPONSE_CODE_DISCONNECT) {
            // `try_into` rather than `raw[0]`/`raw[1]`: this module carries
            // `deny(clippy::indexing_slicing)` under `not(test)`, and it is
            // right to. A panic in a parser reading vendor bytes is the one
            // failure a feed cannot absorb, so the reason code is read through
            // a fallible conversion that yields `None` instead.
            let raw: [u8; 2] = rest.get(len.checked_sub(2)?..len)?.try_into().ok()?;
            found = Some(u16::from_le_bytes(raw));
        }
        offset = end;
        packets = packets.saturating_add(1);
        if packets >= max_packets {
            return None;
        }
    }
    // The WHOLE frame must walk cleanly before a disconnect inside it counts.
    //
    // An earlier draft returned as soon as it saw a code-50 packet at offset
    // 0, and two existing tests caught it: a 16-byte frame whose first byte
    // happens to be 50 is a valid 10-byte disconnect plus six bytes that
    // resolve to nothing, and honouring that would park a healthy socket on a
    // reason read out of random data. Requiring the frame to walk end to end
    // means a fabricated disconnect needs the peer to send an entirely
    // well-formed frame that genuinely contains one -- which is the fail-safe
    // direction `classify_frame` already documents, preserved rather than
    // traded away for the stacked case.
    found
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant offsets for packet construction
mod tests {
    use super::*;
    use crate::websocket::types::DisconnectCode;
    use tickvault_common::constants::{
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
        PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
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
    fn unwrap_oi(frame: ParsedFrame) -> (u64, u8, u32) {
        match frame {
            ParsedFrame::OiUpdate {
                security_id,
                exchange_segment_code,
                open_interest,
            } => (security_id, exchange_segment_code, open_interest),
            other => panic!("expected OiUpdate, got {other:?}"),
        }
    }
    fn unwrap_prev_close(frame: ParsedFrame) -> (u64, u8, f32, u32) {
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
    fn unwrap_market_status(frame: ParsedFrame) -> (u64, u8) {
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
    fn test_prewarm_dispatcher_counters_is_idempotent() {
        // Calling prewarm twice must not panic — OnceLock::get_or_init
        // is idempotent and the second call is a cheap pointer load.
        // This also verifies dispatch_frame can run after prewarm without
        // corrupting the cached Counter handles.
        prewarm_dispatcher_counters();
        prewarm_dispatcher_counters();

        let buf = make_minimal_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE);
        let tick = unwrap_tick(dispatch_frame(&buf, 0).unwrap());
        assert_eq!(tick.security_id, 42);
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

    // PR #4 (2026-05-19): 3 market_depth (v1 code 3) tests retired —
    // the legacy v1 code path was deleted in this PR.

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
        assert_eq!(tick.security_id, u64::from(u32::MAX));
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

// ---------------------------------------------------------------------------
// Depth-feed dispatch tests (2026-08-09)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test builders use constant offsets for packet construction
mod depth_dispatch_tests {
    use super::*;
    use tickvault_common::constants::{
        DEEP_DEPTH_HEADER_SIZE, DEEP_DEPTH_LEVEL_SIZE, TWENTY_DEPTH_LEVELS,
    };

    fn build_twenty(code: u8, sid: u32) -> Vec<u8> {
        let total = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
        let mut buf = vec![0u8; total];
        buf[0..2].copy_from_slice(&(total as u16).to_le_bytes());
        buf[2] = code;
        buf[3] = 1; // NSE_EQ
        buf[4..8].copy_from_slice(&sid.to_le_bytes());
        for i in 0..TWENTY_DEPTH_LEVELS {
            let base = DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE;
            buf[base..base + 8].copy_from_slice(&(1000.0_f64 + i as f64).to_le_bytes());
            buf[base + 8..base + 12].copy_from_slice(&(5_u32 * (i as u32 + 1)).to_le_bytes());
            buf[base + 12..base + 16].copy_from_slice(&(i as u32 + 1).to_le_bytes());
        }
        buf
    }

    #[test]
    fn test_dispatch_depth_packet_routes_bid_and_ask() {
        let mut buf = DepthLevelBuffer::new();
        for (code, expected) in [
            (DEEP_DEPTH_FEED_CODE_BID, DepthSide::Bid),
            (DEEP_DEPTH_FEED_CODE_ASK, DepthSide::Ask),
        ] {
            let raw = build_twenty(code, 1333);
            let packet =
                dispatch_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).expect("parses");
            assert_eq!(packet.header.security_id, 1333);
            match packet.payload {
                DepthPayload::Levels { side, levels } => {
                    assert_eq!(side, expected);
                    assert_eq!(levels.len(), TWENTY_DEPTH_LEVELS);
                    assert!((levels[0].price - 1000.0).abs() < f64::EPSILON);
                    assert_eq!(levels[0].quantity, 5);
                }
                DepthPayload::Disconnect { .. } => panic!("expected levels"),
            }
        }
    }

    #[test]
    fn test_dispatch_depth_packet_refuses_unknown_code() {
        let mut raw = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1);
        raw[2] = 77;
        let mut buf = DepthLevelBuffer::new();
        let err = dispatch_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownResponseCode(77)),
            "{err:?}"
        );
    }

    #[test]
    fn test_dispatch_depth_packet_refuses_short_frame() {
        let raw = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1);
        let mut buf = DepthLevelBuffer::new();
        let err = dispatch_depth_packet(&raw[..40], DepthFeedKind::Twenty, &mut buf).unwrap_err();
        assert!(
            matches!(err, ParseError::InsufficientBytes { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn test_dispatch_depth_packet_drives_a_stacked_frame_via_split() {
        // The documented consumption pattern: split first, dispatch each packet.
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1333);
        frame.extend_from_slice(&build_twenty(DEEP_DEPTH_FEED_CODE_ASK, 1333));

        let mut buf = DepthLevelBuffer::new();
        let mut iter = super::super::depth::split_depth_frame(&frame, DepthFeedKind::Twenty);
        let mut sides = Vec::new();
        // Collect first so the buffer borrow ends before the next dispatch.
        let packets: Vec<&[u8]> = iter.by_ref().collect();
        for packet in packets {
            let parsed = dispatch_depth_packet(packet, DepthFeedKind::Twenty, &mut buf)
                .expect("stacked packet parses");
            if let DepthPayload::Levels { side, .. } = parsed.payload {
                sides.push(side);
            }
        }
        assert_eq!(sides, vec![DepthSide::Bid, DepthSide::Ask]);
        assert!(iter.stop_reason().is_some());
    }

    #[test]
    fn test_dispatch_depth_packet_never_panics_on_garbage() {
        let mut state: u32 = 0x0BAD_F00D;
        let mut buf = DepthLevelBuffer::new();
        for len in 0..300_usize {
            let mut frame = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                frame.push((state >> 8) as u8);
            }
            for kind in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
                let _ = dispatch_depth_packet(&frame, kind, &mut buf);
            }
        }
    }

    #[test]
    fn test_prewarm_dispatcher_counters_includes_depth_arms() {
        // Idempotent; must not panic and must touch the depth label arms.
        prewarm_dispatcher_counters();
        prewarm_dispatcher_counters();
        assert!(std::ptr::eq(
            depth_dispatcher_counter(DEEP_DEPTH_FEED_CODE_BID),
            depth_dispatcher_counter(DEEP_DEPTH_FEED_CODE_BID)
        ));
        assert!(!std::ptr::eq(
            depth_dispatcher_counter(DEEP_DEPTH_FEED_CODE_BID),
            depth_dispatcher_counter(DEEP_DEPTH_FEED_CODE_ASK)
        ));
    }
}

/// The stacked-disconnect walk (2026-08-25). Its own module because it is a
/// MAIN-FEED concern and the module above is the depth dispatcher's.
#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: fixed protocol offsets
mod stacked_disconnect_tests {
    use super::*;
    // ---- stacked disconnect (2026-08-25) --------------------------------
    //
    // The classifier compared the WHOLE frame length against 10 and called
    // anything else data, so a disconnect stacked behind a data packet was
    // handed to the drain as ordinary traffic and its reason code reached no
    // log, no metric and no classifier. These pin the walk that fixed it.

    /// A 10-byte main-feed disconnect packet for `reason`.
    fn disc(reason: u16) -> Vec<u8> {
        let mut p = vec![0u8; DISCONNECT_PACKET_SIZE];
        p[0] = RESPONSE_CODE_DISCONNECT;
        let r = reason.to_le_bytes();
        p[DISCONNECT_PACKET_SIZE - 2] = r[0];
        p[DISCONNECT_PACKET_SIZE - 1] = r[1];
        p
    }

    /// One well-formed packet of `code`, zero-filled after the code byte.
    fn packet(code: u8) -> Vec<u8> {
        let len = main_feed_packet_len(&[code]).expect("known code");
        let mut p = vec![0u8; len];
        p[0] = code;
        p
    }

    #[test]
    fn a_disconnect_alone_is_still_found() {
        assert_eq!(stacked_disconnect_reason(&disc(804), 70_000), Some(804));
    }

    #[test]
    fn a_disconnect_stacked_behind_data_is_found() {
        // THE regression. Before the fix this frame was classified `Data`,
        // the drain folded the disconnect into an untyped "non-tick" count,
        // and 804 -- the one code the repo rules Fatal because retrying
        // re-sends the identical over-limit subscribe set forever -- was
        // retried blind.
        let mut frame = packet(RESPONSE_CODE_QUOTE);
        frame.extend_from_slice(&disc(804));
        assert_eq!(stacked_disconnect_reason(&frame, 70_000), Some(804));
    }

    #[test]
    fn a_disconnect_behind_several_packets_is_found() {
        let mut frame = packet(RESPONSE_CODE_TICKER);
        frame.extend_from_slice(&packet(RESPONSE_CODE_FULL));
        frame.extend_from_slice(&packet(RESPONSE_CODE_OI));
        frame.extend_from_slice(&disc(805));
        assert_eq!(stacked_disconnect_reason(&frame, 70_000), Some(805));
    }

    #[test]
    fn a_pure_data_frame_yields_no_disconnect() {
        let mut frame = packet(RESPONSE_CODE_QUOTE);
        frame.extend_from_slice(&packet(RESPONSE_CODE_FULL));
        assert_eq!(stacked_disconnect_reason(&frame, 70_000), None);
    }

    #[test]
    fn a_byte_50_inside_a_payload_is_not_read_as_a_code() {
        // The whole reason this walks packet boundaries instead of scanning
        // for the byte: a quote packet's payload is free to contain 50, and a
        // scan would park a healthy socket on a reason invented from a price.
        let mut p = packet(RESPONSE_CODE_QUOTE);
        for b in p.iter_mut().skip(1) {
            *b = RESPONSE_CODE_DISCONNECT;
        }
        assert_eq!(stacked_disconnect_reason(&p, 70_000), None);
    }

    #[test]
    fn an_unknown_code_stops_the_walk_rather_than_guessing() {
        let mut frame = vec![99u8, 0, 0, 0];
        frame.extend_from_slice(&disc(804));
        assert_eq!(stacked_disconnect_reason(&frame, 70_000), None);
    }

    #[test]
    fn a_trailing_partial_packet_is_refused_not_guessed() {
        let mut frame = packet(RESPONSE_CODE_QUOTE);
        frame.extend_from_slice(&disc(804)[..6]); // truncated disconnect
        assert_eq!(stacked_disconnect_reason(&frame, 70_000), None);
    }

    #[test]
    fn the_packet_cap_bounds_the_walk() {
        let mut frame = Vec::new();
        for _ in 0..5 {
            frame.extend_from_slice(&packet(RESPONSE_CODE_TICKER));
        }
        frame.extend_from_slice(&disc(804));
        // Within the cap it is found; below it the walk stops first.
        assert_eq!(stacked_disconnect_reason(&frame, 70_000), Some(804));
        assert_eq!(stacked_disconnect_reason(&frame, 3), None);
    }

    #[test]
    fn an_empty_frame_yields_nothing() {
        assert_eq!(stacked_disconnect_reason(&[], 70_000), None);
    }

    #[test]
    fn every_documented_main_feed_code_has_a_length() {
        for code in [
            RESPONSE_CODE_INDEX_TICKER,
            RESPONSE_CODE_TICKER,
            RESPONSE_CODE_QUOTE,
            RESPONSE_CODE_OI,
            RESPONSE_CODE_PREVIOUS_CLOSE,
            RESPONSE_CODE_MARKET_STATUS,
            RESPONSE_CODE_FULL,
            RESPONSE_CODE_DISCONNECT,
        ] {
            assert!(
                main_feed_packet_len(&[code]).is_some(),
                "code {code} has no length; the walk would stop on it"
            );
        }
        assert_eq!(main_feed_packet_len(&[99]), None);
        assert_eq!(main_feed_packet_len(&[]), None);
    }

    /// The enumeration above is a readable smoke test and CANNOT be the
    /// guard: it is a hand-written list, and a hand-written list is exactly
    /// how `RESPONSE_CODE_INDEX_TICKER` (code 1) went missing from
    /// `main_feed_packet_len` while `dispatch_frame` accepted it — the list in
    /// the test omitted the same code the table omitted, so it agreed with the
    /// bug. This sweeps the WHOLE byte space and derives the expectation from
    /// `dispatch_frame` itself, so the two can never disagree again no matter
    /// which codes are added or removed.
    ///
    /// The contract, in one line: a code the dispatcher will DECODE must have
    /// a length the walk can STEP by. Anything else strands the rest of the
    /// frame — the drain abandons it, and a stacked disconnect inside it is
    /// never seen.
    #[test]
    fn every_code_the_dispatcher_accepts_has_a_walkable_length() {
        // Long enough for the widest packet (FULL, 162 B) so a dispatchable
        // code can never be misread as "unknown" merely for being truncated.
        let mut buf = [0u8; 256];
        for code in 0u8..=255 {
            buf[0] = code;
            let dispatchable = !matches!(
                dispatch_frame(&buf, 0),
                Err(ParseError::UnknownResponseCode(_))
            );
            let walkable = main_feed_packet_len(&buf).is_some();
            assert_eq!(
                dispatchable, walkable,
                "code {code}: dispatch_frame accepts it = {dispatchable}, but \
                 main_feed_packet_len knows a length = {walkable}. These must \
                 agree — a decodable packet with no length stops the walk and \
                 abandons the rest of the frame; a length for a code nothing \
                 decodes steps the walk over bytes no parser has validated."
            );
        }
    }

    /// The bite: code 1 specifically, because that is the one that was wrong,
    /// and because it is one config line from being live. The 2026-08-21
    /// scope-lock names `Ticker` as the next `IDX_I_FEED_MODE` value to try if
    /// Dhan refuses Quote for indices — at which point every one of ~119 index
    /// frames would arrive as code 1.
    #[test]
    fn an_index_ticker_leading_a_frame_does_not_hide_a_stacked_disconnect() {
        let mut frame = packet(RESPONSE_CODE_INDEX_TICKER);
        frame.extend_from_slice(&disc(804));
        assert_eq!(
            stacked_disconnect_reason(&frame, 70_000),
            Some(804),
            "an 804 behind an index ticker must still be seen — 804 is Fatal \
             and retrying it can earn an 805 account block"
        );
        assert_eq!(
            main_feed_packet_len(&[RESPONSE_CODE_INDEX_TICKER]),
            Some(TICKER_PACKET_SIZE)
        );
    }
}
