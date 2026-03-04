//! Integration test: Parser pipeline round-trip.
//!
//! Constructs binary packets → dispatches through `dispatch_frame` →
//! verifies ParsedFrame variants are correct with expected field values.
//! This crosses the parser module boundaries (header + quote + dispatcher).

use dhan_live_trader_common::constants::{
    QUOTE_PACKET_SIZE, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE,
};
use dhan_live_trader_core::parser::dispatcher::dispatch_frame;
use dhan_live_trader_core::parser::types::ParsedFrame;

/// Build a minimal valid binary packet for a given response code + size.
#[allow(clippy::indexing_slicing, clippy::as_conversions)]
fn build_packet(response_code: u8, size: usize, segment: u8, security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    buf[0] = response_code;
    buf[1..3].copy_from_slice(&(size as u16).to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf
}

#[test]
fn ticker_packet_round_trip() {
    let packet = build_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE, 2, 12345);
    let result = dispatch_frame(&packet, 999);
    assert!(result.is_ok(), "valid ticker packet must parse");
    match result.unwrap() {
        ParsedFrame::Tick(tick) => {
            assert_eq!(tick.security_id, 12345);
            assert_eq!(tick.exchange_segment_code, 2);
            assert_eq!(tick.received_at_nanos, 999);
        }
        other => panic!("expected Tick, got {other:?}"),
    }
}

#[test]
fn quote_packet_round_trip() {
    let packet = build_packet(RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE, 3, 67890);
    let result = dispatch_frame(&packet, 1_000_000);
    assert!(result.is_ok(), "valid quote packet must parse");
    match result.unwrap() {
        ParsedFrame::Tick(tick) => {
            assert_eq!(tick.security_id, 67890);
            assert_eq!(tick.exchange_segment_code, 3);
            assert_eq!(tick.received_at_nanos, 1_000_000);
        }
        other => panic!("expected Tick, got {other:?}"),
    }
}

#[test]
fn empty_buffer_returns_error() {
    let result = dispatch_frame(&[], 0);
    assert!(result.is_err(), "empty buffer must fail");
}

#[test]
fn truncated_header_returns_error() {
    let result = dispatch_frame(&[0u8; 7], 0);
    assert!(result.is_err(), "7-byte buffer must fail (header needs 8)");
}

#[test]
fn unknown_response_code_returns_error() {
    let packet = build_packet(255, TICKER_PACKET_SIZE, 2, 1);
    let result = dispatch_frame(&packet, 0);
    assert!(result.is_err(), "unknown response code 255 must fail");
}
