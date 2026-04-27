//! Routing ratchet tests for previous-day close per Dhan Ticket #5525125
//! (Wave 1 Item 4, gap closure for G2/G3 + Item 4.5 routing matrix).
//!
//! ## Authoritative source
//!
//! Per `.claude/rules/dhan/live-market-feed.md` rule 7/8/10 (rewritten
//! 2026-04-10 after Dhan support Ticket #5525125), the previous-day
//! close is delivered by **different** Dhan WebSocket packet types
//! depending on the instrument's `ExchangeSegment`:
//!
//! | Segment | Source packet | Byte offset (0-based) | f32 LE |
//! |---------|---------------|-----------------------|--------|
//! | IDX_I   | PrevClose (response code 6) | 8..12  | yes |
//! | NSE_EQ  | Quote (response code 4)     | 38..42 | yes |
//! | NSE_FNO | Full  (response code 8)     | 50..54 | yes |
//!
//! Subscribing to Ticker mode on NSE_EQ / NSE_FNO and waiting for code 6
//! is a known anti-pattern that causes "prev close missing for 24,972
//! of 25,000 instruments, only 28 IDX_I indices have it". These three
//! ratchets ensure the parsers continue to extract the field at the
//! correct byte offsets for each segment, so the persistence layer
//! (Item 4.2 — `previous_close_persistence::write_previous_close`,
//! shipped in a follow-up commit) can route based on response code +
//! segment with confidence.
//!
//! ## What this file does NOT do
//!
//! - It does **not** test the persistence layer (Item 4.2) or the
//!   first-seen-set IST-midnight reset (Item 4.3). Those are separate
//!   modules with their own ratchets.
//! - It does **not** exercise the `ParsedFrame::PrevClose` arm in
//!   `tick_processor.rs` — only the parsers in `crates/core/src/parser/`.
//!
//! ## Cross-references
//!
//! - `crates/common/src/constants.rs` — `QUOTE_OFFSET_CLOSE = 38`,
//!   `FULL_OFFSET_CLOSE = 50`, `PREV_CLOSE_OFFSET_PRICE = 8`.
//! - `.claude/rules/dhan/live-market-feed.md` — Mechanical Rule 7/8/10.

use tickvault_common::constants::{
    EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO, FULL_OFFSET_CLOSE,
    FULL_QUOTE_PACKET_SIZE, PREV_CLOSE_OFFSET_PRICE, PREVIOUS_CLOSE_PACKET_SIZE,
    QUOTE_OFFSET_CLOSE, QUOTE_PACKET_SIZE,
};
use tickvault_core::parser::full_packet::parse_full_packet;
use tickvault_core::parser::previous_close::parse_previous_close_packet;
use tickvault_core::parser::quote::parse_quote_packet;
use tickvault_core::parser::types::PacketHeader;

/// Builds an 8-byte response header at the start of the buffer.
fn write_header(buf: &mut [u8], response_code: u8, segment: u8, security_id: u32, msg_len: u16) {
    buf[0] = response_code;
    buf[1..3].copy_from_slice(&msg_len.to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
}

fn make_header(response_code: u8, segment: u8, security_id: u32, msg_len: u16) -> PacketHeader {
    PacketHeader {
        response_code,
        message_length: msg_len,
        exchange_segment_code: segment,
        security_id,
    }
}

/// IDX_I (NIFTY=13) — previous-day close arrives as a dedicated
/// `PrevClose` packet (response code 6) with the close at bytes 8-11
/// as f32 LE. This is the ONLY source for indices.
#[test]
fn test_prev_close_routing_idx_i_from_code6_bytes_8_to_11() {
    const NIFTY_PREV_CLOSE: f32 = 19_500.25;
    const NIFTY_SECURITY_ID: u32 = 13;
    const NIFTY_PREV_OI: u32 = 0; // indices: PrevClose carries 0 OI

    let mut buf = vec![0u8; PREVIOUS_CLOSE_PACKET_SIZE];
    write_header(
        &mut buf,
        6,
        EXCHANGE_SEGMENT_IDX_I,
        NIFTY_SECURITY_ID,
        PREVIOUS_CLOSE_PACKET_SIZE as u16,
    );
    // bytes 8-11 — previous close (f32 LE)
    let prev_close_bytes = NIFTY_PREV_CLOSE.to_le_bytes();
    buf[PREV_CLOSE_OFFSET_PRICE..PREV_CLOSE_OFFSET_PRICE + 4].copy_from_slice(&prev_close_bytes);
    // bytes 12-15 — previous OI (u32 LE)
    buf[12..16].copy_from_slice(&NIFTY_PREV_OI.to_le_bytes());

    let header = make_header(
        6,
        EXCHANGE_SEGMENT_IDX_I,
        NIFTY_SECURITY_ID,
        PREVIOUS_CLOSE_PACKET_SIZE as u16,
    );

    // `parse_previous_close_packet` returns `PreviousCloseData` directly
    // (not wrapped in `ParsedFrame`). The struct has just two fields —
    // `previous_close` and `previous_oi`. The dispatcher in
    // `tick_processor.rs` is what re-binds these to the IDX_I segment
    // via the original `PacketHeader`.
    let data = parse_previous_close_packet(&buf, &header)
        .expect("PrevClose parser must accept a 16-byte IDX_I packet");
    assert!(
        (data.previous_close - NIFTY_PREV_CLOSE).abs() < f32::EPSILON,
        "Ticket #5525125: IDX_I prev_close MUST come from code-6 \
         packet bytes 8-11 (f32 LE). Got {got}, expected {expected}.",
        got = data.previous_close,
        expected = NIFTY_PREV_CLOSE,
    );
    assert_eq!(data.previous_oi, NIFTY_PREV_OI);
    // The header is the segment carrier — ratchet that the segment
    // round-trips through it as well.
    assert_eq!(header.exchange_segment_code, EXCHANGE_SEGMENT_IDX_I);
    assert_eq!(header.security_id, NIFTY_SECURITY_ID);
}

/// NSE_EQ (RELIANCE) — previous-day close is INSIDE the Quote packet
/// (response code 4) at bytes 38-41 as f32 LE. There is NO dedicated
/// PrevClose packet for equities (per Ticket #5525125). The parser
/// already exposes this value via `ParsedTick::day_close` — semantic
/// note: the field is labelled `day_close` in our struct because Dhan
/// labels it `close` in their wire docs, but Ticket #5525125 confirms
/// it represents the **PREVIOUS** trading session's close.
#[test]
fn test_prev_close_routing_nse_eq_from_quote_close_field_bytes_38_to_41() {
    const RELIANCE_PREV_CLOSE: f32 = 2_847.50;
    const RELIANCE_SECURITY_ID: u32 = 2885;

    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    write_header(
        &mut buf,
        4,
        EXCHANGE_SEGMENT_NSE_EQ,
        RELIANCE_SECURITY_ID,
        QUOTE_PACKET_SIZE as u16,
    );
    // Bytes 8-11 LTP, 12-13 LTQ, 14-17 LTT, 18-21 ATP, 22-25 Vol,
    // 26-29 TSQ, 30-33 TBQ, 34-37 OPEN, 38-41 CLOSE (= prev day close
    // per Ticket #5525125), 42-45 HIGH, 46-49 LOW.
    buf[QUOTE_OFFSET_CLOSE..QUOTE_OFFSET_CLOSE + 4]
        .copy_from_slice(&RELIANCE_PREV_CLOSE.to_le_bytes());

    let header = make_header(
        4,
        EXCHANGE_SEGMENT_NSE_EQ,
        RELIANCE_SECURITY_ID,
        QUOTE_PACKET_SIZE as u16,
    );

    let tick = parse_quote_packet(&buf, &header, 0)
        .expect("Quote parser must accept a 50-byte NSE_EQ packet");

    assert!(
        (tick.day_close - RELIANCE_PREV_CLOSE).abs() < f32::EPSILON,
        "Ticket #5525125: NSE_EQ prev_close MUST be read from Quote \
         (code 4) bytes 38-41 (f32 LE). Got {got}, expected {expected}.",
        got = tick.day_close,
        expected = RELIANCE_PREV_CLOSE,
    );
    assert_eq!(tick.security_id, RELIANCE_SECURITY_ID);
    assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_EQ);
    assert_eq!(
        QUOTE_OFFSET_CLOSE, 38,
        "QUOTE_OFFSET_CLOSE constant MUST equal 38 per Dhan wire format \
         and Ticket #5525125 routing matrix.",
    );
}

/// NSE_FNO (option contract) — previous-day close is INSIDE the Full
/// packet (response code 8) at bytes 50-53 as f32 LE. Same semantic
/// as NSE_EQ per Ticket #5525125: field is labelled `close` in Dhan
/// docs but represents the previous trading session's close.
#[test]
fn test_prev_close_routing_nse_fno_from_full_close_field_bytes_50_to_53() {
    const NIFTY_OPTION_PREV_CLOSE: f32 = 125.75;
    const NIFTY_OPTION_SECURITY_ID: u32 = 73_242;

    let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
    write_header(
        &mut buf,
        8,
        EXCHANGE_SEGMENT_NSE_FNO,
        NIFTY_OPTION_SECURITY_ID,
        FULL_QUOTE_PACKET_SIZE as u16,
    );
    // Bytes 50-53 — close (= prev day close per Ticket #5525125).
    buf[FULL_OFFSET_CLOSE..FULL_OFFSET_CLOSE + 4]
        .copy_from_slice(&NIFTY_OPTION_PREV_CLOSE.to_le_bytes());

    let header = make_header(
        8,
        EXCHANGE_SEGMENT_NSE_FNO,
        NIFTY_OPTION_SECURITY_ID,
        FULL_QUOTE_PACKET_SIZE as u16,
    );

    // `parse_full_packet` returns `(ParsedTick, [MarketDepthLevel; 5])`
    // directly. The tick carries the `day_close` field which Ticket
    // #5525125 confirms is the PREVIOUS day's close for NSE_FNO.
    let (tick, _depth) = parse_full_packet(&buf, &header, 0)
        .expect("Full parser must accept a 162-byte NSE_FNO packet");
    assert!(
        (tick.day_close - NIFTY_OPTION_PREV_CLOSE).abs() < f32::EPSILON,
        "Ticket #5525125: NSE_FNO prev_close MUST be read from Full \
         (code 8) bytes 50-53 (f32 LE). Got {got}, expected {expected}.",
        got = tick.day_close,
        expected = NIFTY_OPTION_PREV_CLOSE,
    );
    assert_eq!(tick.security_id, NIFTY_OPTION_SECURITY_ID);
    assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
    assert_eq!(
        FULL_OFFSET_CLOSE, 50,
        "FULL_OFFSET_CLOSE constant MUST equal 50 per Dhan wire format \
         and Ticket #5525125 routing matrix.",
    );
}

/// Defense-in-depth: the three offsets MUST stay distinct. If a future
/// edit accidentally aliases `QUOTE_OFFSET_CLOSE` to `FULL_OFFSET_CLOSE`
/// (or sets either to `PREV_CLOSE_OFFSET_PRICE`), this assertion fails.
#[test]
fn test_prev_close_routing_offsets_are_distinct_per_ticket_5525125() {
    assert_eq!(PREV_CLOSE_OFFSET_PRICE, 8);
    assert_eq!(QUOTE_OFFSET_CLOSE, 38);
    assert_eq!(FULL_OFFSET_CLOSE, 50);
    assert_ne!(QUOTE_OFFSET_CLOSE, FULL_OFFSET_CLOSE);
    assert_ne!(QUOTE_OFFSET_CLOSE, PREV_CLOSE_OFFSET_PRICE);
    assert_ne!(FULL_OFFSET_CLOSE, PREV_CLOSE_OFFSET_PRICE);
}
