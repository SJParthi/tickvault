//! Property-based tests for the binary protocol parser.
//!
//! Uses proptest to generate random-ish packets and verify invariants:
//! - Parser never panics on any input
//! - Valid packets parse with correct security_id and segment
//! - Parsed fields are within valid ranges

use dhan_live_trader_common::constants::{
    DISCONNECT_PACKET_SIZE, EXCHANGE_SEGMENT_NSE_FNO, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_PACKET_SIZE, OI_PACKET_SIZE, PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE,
    TICKER_PACKET_SIZE,
};
use dhan_live_trader_core::parser::dispatch_frame;
use dhan_live_trader_core::parser::types::ParsedFrame;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers — construct valid packets with random field values
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_valid_ticker(security_id: u32, ltp: f32, ltt: u32, segment: u8) -> Vec<u8> {
    let mut buf = vec![0u8; TICKER_PACKET_SIZE];
    buf[0] = 2; // RESPONSE_CODE_TICKER
    buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[12..16].copy_from_slice(&ltt.to_le_bytes());
    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_valid_quote(security_id: u32, ltp: f32, volume: u32) -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = 4; // RESPONSE_CODE_QUOTE
    buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[22..26].copy_from_slice(&volume.to_le_bytes());
    buf
}

// ---------------------------------------------------------------------------
// Property: parser never panics on arbitrary bytes
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_parser_never_panics_on_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..256)) {
        // Must not panic, regardless of input
        let _ = dispatch_frame(&data, 0);
    }

    #[test]
    fn proptest_parser_never_panics_on_large_frames(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = dispatch_frame(&data, 0);
    }
}

// ---------------------------------------------------------------------------
// Property: valid ticker packets parse with correct security_id
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_ticker_security_id_preserved(
        security_id in 1..u32::MAX,
        ltp in 0.01_f32..100_000.0,
        ltt in 1_600_000_000_u32..1_800_000_000,
    ) {
        let packet = make_valid_ticker(security_id, ltp, ltt, EXCHANGE_SEGMENT_NSE_FNO);
        let parsed = dispatch_frame(&packet, 0).unwrap();

        match parsed {
            ParsedFrame::Tick(tick) => {
                prop_assert_eq!(tick.security_id, security_id);
                prop_assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
                prop_assert_eq!(tick.exchange_timestamp, ltt);
                // LTP should round-trip exactly (same f32 bits)
                prop_assert_eq!(tick.last_traded_price, ltp);
            }
            other => prop_assert!(false, "expected Tick, got: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: valid quote packets parse with correct fields
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_quote_fields_preserved(
        security_id in 1..u32::MAX,
        ltp in 0.01_f32..100_000.0,
        volume in 0_u32..10_000_000,
    ) {
        let packet = make_valid_quote(security_id, ltp, volume);
        let parsed = dispatch_frame(&packet, 42).unwrap();

        match parsed {
            ParsedFrame::Tick(tick) => {
                prop_assert_eq!(tick.security_id, security_id);
                prop_assert_eq!(tick.last_traded_price, ltp);
                prop_assert_eq!(tick.volume, volume);
                prop_assert_eq!(tick.received_at_nanos, 42);
            }
            other => prop_assert!(false, "expected Tick, got: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: OI packet preserves open interest value
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_oi_value_preserved(
        security_id in 1..u32::MAX,
        oi in 0_u32..50_000_000,
    ) {
        let mut buf = vec![0u8; OI_PACKET_SIZE];
        buf[0] = 5; // RESPONSE_CODE_OI
        buf[1..3].copy_from_slice(&(OI_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&oi.to_le_bytes());

        let parsed = dispatch_frame(&buf, 0).unwrap();

        match parsed {
            ParsedFrame::OiUpdate { security_id: sid, open_interest, .. } => {
                prop_assert_eq!(sid, security_id);
                prop_assert_eq!(open_interest, oi);
            }
            other => prop_assert!(false, "expected OiUpdate, got: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: exchange segment code round-trips for all valid segments
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_segment_code_preserved(
        segment in prop_oneof![Just(0_u8), Just(1), Just(2), Just(3), Just(4), Just(5), Just(7), Just(8)],
        security_id in 1..u32::MAX,
    ) {
        let packet = make_valid_ticker(security_id, 100.0, 1_700_000_000, segment);
        let parsed = dispatch_frame(&packet, 0).unwrap();

        match parsed {
            ParsedFrame::Tick(tick) => {
                prop_assert_eq!(tick.exchange_segment_code, segment);
            }
            other => prop_assert!(false, "expected Tick, got: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: previous close packet preserves both fields
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_prev_close_fields_preserved(
        security_id in 1..u32::MAX,
        prev_close in 0.01_f32..100_000.0,
        prev_oi in 0_u32..50_000_000,
    ) {
        let mut buf = vec![0u8; PREVIOUS_CLOSE_PACKET_SIZE];
        buf[0] = 6; // RESPONSE_CODE_PREVIOUS_CLOSE
        buf[1..3].copy_from_slice(&(PREVIOUS_CLOSE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&prev_close.to_le_bytes());
        buf[12..16].copy_from_slice(&prev_oi.to_le_bytes());

        let parsed = dispatch_frame(&buf, 0).unwrap();

        match parsed {
            ParsedFrame::PreviousClose { security_id: sid, previous_close, previous_oi, .. } => {
                prop_assert_eq!(sid, security_id);
                prop_assert_eq!(previous_close, prev_close);
                prop_assert_eq!(previous_oi, prev_oi);
            }
            other => prop_assert!(false, "expected PreviousClose, got: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: full packet preserves tick + depth level 0
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_full_packet_security_id_preserved(
        security_id in 1..u32::MAX,
    ) {
        let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
        buf[0] = 8; // RESPONSE_CODE_FULL
        buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());

        let parsed = dispatch_frame(&buf, 0).unwrap();

        match parsed {
            ParsedFrame::TickWithDepth(tick, depth) => {
                prop_assert_eq!(tick.security_id, security_id);
                prop_assert_eq!(depth.len(), 5);
            }
            other => prop_assert!(false, "expected TickWithDepth, got: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: market depth packet preserves security_id and LTP
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    #[allow(clippy::arithmetic_side_effects)]
    fn proptest_market_depth_fields_preserved(
        security_id in 1..u32::MAX,
        ltp in 0.01_f32..100_000.0,
    ) {
        let mut buf = vec![0u8; MARKET_DEPTH_PACKET_SIZE];
        buf[0] = 3; // RESPONSE_CODE_MARKET_DEPTH
        buf[1..3].copy_from_slice(&(MARKET_DEPTH_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());

        let parsed = dispatch_frame(&buf, 0).unwrap();

        match parsed {
            ParsedFrame::TickWithDepth(tick, depth) => {
                prop_assert_eq!(tick.security_id, security_id);
                prop_assert_eq!(tick.last_traded_price, ltp);
                prop_assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
                // Market depth has no timestamp, volume, OI
                prop_assert_eq!(tick.exchange_timestamp, 0);
                prop_assert_eq!(tick.volume, 0);
                prop_assert_eq!(tick.open_interest, 0);
                prop_assert_eq!(depth.len(), 5);
            }
            other => prop_assert!(false, "expected TickWithDepth, got: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: disconnect packet preserves reason code, never panics
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    #[allow(clippy::arithmetic_side_effects)]
    fn proptest_disconnect_code_never_panics(
        code in 0_u16..1000,
    ) {
        let mut buf = vec![0u8; DISCONNECT_PACKET_SIZE];
        buf[0] = 50; // RESPONSE_CODE_DISCONNECT
        buf[1..3].copy_from_slice(&(DISCONNECT_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = 0;
        buf[4..8].copy_from_slice(&0_u32.to_le_bytes());
        buf[8..10].copy_from_slice(&code.to_le_bytes());

        let parsed = dispatch_frame(&buf, 0).unwrap();

        match parsed {
            ParsedFrame::Disconnect(_) => {
                // Must parse without panic for any u16 code
            }
            other => prop_assert!(false, "expected Disconnect, got: {other:?}"),
        }
    }

    #[test]
    #[allow(clippy::arithmetic_side_effects)]
    fn proptest_market_status_security_id_preserved(
        security_id in 1..u32::MAX,
        segment in prop_oneof![Just(0_u8), Just(1), Just(2), Just(3), Just(4), Just(5), Just(7), Just(8)],
    ) {
        let mut buf = vec![0u8; 8]; // MARKET_STATUS_PACKET_SIZE = 8
        buf[0] = 7; // RESPONSE_CODE_MARKET_STATUS
        buf[1..3].copy_from_slice(&8_u16.to_le_bytes());
        buf[3] = segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());

        let parsed = dispatch_frame(&buf, 0).unwrap();

        match parsed {
            ParsedFrame::MarketStatus { security_id: sid, exchange_segment_code } => {
                prop_assert_eq!(sid, security_id);
                prop_assert_eq!(exchange_segment_code, segment);
            }
            other => prop_assert!(false, "expected MarketStatus, got: {other:?}"),
        }
    }
}
