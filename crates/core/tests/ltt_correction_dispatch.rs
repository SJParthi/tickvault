//! The Dhan trade-time correction applied by the core dispatcher (scope lock
//! 2026-11-20, "DHAN TRADE TIME SENT AS UTC"). Its own test binary because it
//! installs the process-wide correction table.

use tickvault_common::constants::{
    FULL_QUOTE_PACKET_SIZE, OI_PACKET_SIZE, QUOTE_OFFSET_LTT, QUOTE_PACKET_SIZE,
    RESPONSE_CODE_FULL, RESPONSE_CODE_INDEX_TICKER, RESPONSE_CODE_OI, RESPONSE_CODE_QUOTE,
    TICKER_OFFSET_LTT, TICKER_PACKET_SIZE,
};
use tickvault_core::parser::dispatcher::{
    dispatch_frame, dispatch_frame_uncounted, dispatch_frame_uncounted_unstamped,
};
use tickvault_core::parser::types::ParsedFrame;
use tickvault_storage::ltt_correction::{
    LTT_UTC_OFFSET_SECS, LttEntry, LttTable, install_table, receipt_ist_day,
};

/// 2026-11-20 11:00:00 IST as a UTC receipt (the scope lock's NIFTY example).
const RECEIPT: i64 = 1_795_152_600 * 1_000_000_000;
const RAW_UTC: u32 = 1_795_152_600;

fn packet(code: u8, size: usize, ltt_offset: usize, segment: u8) -> Vec<u8> {
    let mut bytes = vec![0u8; size];
    bytes[0] = code;
    bytes[1..3].copy_from_slice(&(size as u16).to_le_bytes());
    bytes[3] = segment;
    bytes[4..8].copy_from_slice(&13u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&25_000.0f32.to_le_bytes());
    bytes[ltt_offset..ltt_offset + 4].copy_from_slice(&RAW_UTC.to_le_bytes());
    bytes
}

fn ltt(frame: Result<ParsedFrame, tickvault_core::parser::types::ParseError>) -> u32 {
    match frame.expect("parses") {
        ParsedFrame::Tick(t) | ParsedFrame::TickWithDepth(t, _) => t.exchange_timestamp,
        other => panic!("not a tick: {other:?}"),
    }
}

#[test]
fn test_dispatcher_corrects_ticker_quote_and_full_only_while_armed() {
    let ticker = packet(
        RESPONSE_CODE_INDEX_TICKER,
        TICKER_PACKET_SIZE,
        TICKER_OFFSET_LTT,
        0,
    );
    let quote = packet(RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE, QUOTE_OFFSET_LTT, 0);
    let full = packet(
        RESPONSE_CODE_FULL,
        FULL_QUOTE_PACKET_SIZE,
        QUOTE_OFFSET_LTT,
        0,
    );

    // Nothing armed: raw.
    install_table(LttTable::empty());
    assert_eq!(ltt(dispatch_frame(&ticker, RECEIPT)), RAW_UTC);

    let mut table = LttTable::empty();
    assert!(table.arm(
        receipt_ist_day(RECEIPT),
        LttEntry {
            id: 0,
            segment: 0,
            from_nanos: RECEIPT,
            to_nanos: i64::MAX,
        },
    ));
    install_table(table);
    let fixed = RAW_UTC + LTT_UTC_OFFSET_SECS;
    for bytes in [&ticker, &quote, &full] {
        assert_eq!(ltt(dispatch_frame(bytes, RECEIPT)), fixed);
        assert_eq!(ltt(dispatch_frame_uncounted(bytes, RECEIPT)), fixed);
        // No WAL stamp: never corrected.
        assert_eq!(
            ltt(dispatch_frame_uncounted_unstamped(bytes, RECEIPT)),
            RAW_UTC
        );
        // Before the entry's `from`: raw.
        assert_eq!(ltt(dispatch_frame(bytes, RECEIPT - 1)), RAW_UTC);
    }
    // A non-tick packet is untouched and still parses.
    let mut oi = vec![0u8; OI_PACKET_SIZE];
    oi[0] = RESPONSE_CODE_OI;
    assert!(matches!(
        dispatch_frame(&oi, RECEIPT),
        Ok(ParsedFrame::OiUpdate { .. })
    ));
    install_table(LttTable::empty());
    assert_eq!(ltt(dispatch_frame(&ticker, RECEIPT)), RAW_UTC);
}
