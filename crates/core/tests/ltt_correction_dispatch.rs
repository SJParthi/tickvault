//! The Dhan trade-time correction applied by the core dispatcher (scope lock
//! 2026-11-21, "DHAN LIVE LTT IS UTC EPOCH FROM RECEIPT 2026-11-20 00:00 IST";
//! Dhan ticket #6684672). From that receipt on, every Ticker, Quote and Full
//! trade time of a WAL-stamped frame is raw + 19,800 s; before it, raw.

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
    DHAN_LTT_UTC_FROM_RECEIPT_NANOS, LTT_UTC_OFFSET_SECS, unstamped_count,
};

/// 2026-11-20 11:00:00 IST as a UTC receipt (the scope lock's NIFTY example).
const RECEIPT: i64 = 1_795_152_600 * 1_000_000_000;
const RAW_UTC: u32 = 1_795_152_600;

fn packet(code: u8, size: usize, ltt_offset: usize, segment: u8, raw: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; size];
    bytes[0] = code;
    bytes[1..3].copy_from_slice(&(size as u16).to_le_bytes());
    bytes[3] = segment;
    bytes[4..8].copy_from_slice(&13u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&25_000.0f32.to_le_bytes());
    bytes[ltt_offset..ltt_offset + 4].copy_from_slice(&raw.to_le_bytes());
    bytes
}

fn ltt(frame: Result<ParsedFrame, tickvault_core::parser::types::ParseError>) -> u32 {
    match frame.expect("parses") {
        ParsedFrame::Tick(t) | ParsedFrame::TickWithDepth(t, _) => t.exchange_timestamp,
        other => panic!("not a tick: {other:?}"),
    }
}

fn three(segment: u8, raw: u32) -> [Vec<u8>; 3] {
    [
        packet(
            RESPONSE_CODE_INDEX_TICKER,
            TICKER_PACKET_SIZE,
            TICKER_OFFSET_LTT,
            segment,
            raw,
        ),
        packet(
            RESPONSE_CODE_QUOTE,
            QUOTE_PACKET_SIZE,
            QUOTE_OFFSET_LTT,
            segment,
            raw,
        ),
        packet(
            RESPONSE_CODE_FULL,
            FULL_QUOTE_PACKET_SIZE,
            QUOTE_OFFSET_LTT,
            segment,
            raw,
        ),
    ]
}

#[test]
fn test_dispatcher_corrects_ticker_quote_and_full_from_the_cutover() {
    let fixed = RAW_UTC + LTT_UTC_OFFSET_SECS;
    for segment in [0u8, 1, 2, 8] {
        for bytes in three(segment, RAW_UTC) {
            assert_eq!(ltt(dispatch_frame(&bytes, RECEIPT)), fixed);
            assert_eq!(ltt(dispatch_frame_uncounted(&bytes, RECEIPT)), fixed);
            // No WAL stamp: never corrected, and counted.
            let before = unstamped_count();
            assert_eq!(
                ltt(dispatch_frame_uncounted_unstamped(&bytes, RECEIPT)),
                RAW_UTC
            );
            assert!(unstamped_count() > before);
        }
    }
    // Before the cutover: raw, whatever the value.
    for bytes in three(0, RAW_UTC) {
        assert_eq!(
            ltt(dispatch_frame(&bytes, DHAN_LTT_UTC_FROM_RECEIPT_NANOS - 1)),
            RAW_UTC
        );
        assert_eq!(ltt(dispatch_frame(&bytes, 0)), RAW_UTC);
    }
    // An IST-epoch fresh print (Dhan reverted) stays raw: the cap.
    for bytes in three(0, fixed) {
        assert_eq!(ltt(dispatch_frame(&bytes, RECEIPT)), fixed);
    }
    // A raw 0 stays 0.
    for bytes in three(0, 0) {
        assert_eq!(ltt(dispatch_frame(&bytes, RECEIPT)), 0);
    }
    // A non-tick packet is untouched and still parses.
    let mut oi = vec![0u8; OI_PACKET_SIZE];
    oi[0] = RESPONSE_CODE_OI;
    assert!(matches!(
        dispatch_frame(&oi, RECEIPT),
        Ok(ParsedFrame::OiUpdate { .. })
    ));
}

#[test]
fn test_regression_2026_11_20_first_dispatch_of_the_day_is_corrected() {
    // 2026-11-20 08:31:22 IST, the box's first NIFTY packet: the dispatcher
    // corrects it with no warm-up, no record and no armed table.
    let first_utc_secs: i64 = 1_795_113_000 + 8 * 3_600 + 31 * 60 + 22;
    let raw = u32::try_from(first_utc_secs).expect("u32");
    let receipt = first_utc_secs * 1_000_000_000 + 250_000_000;
    let [ticker, _, _] = three(0, raw);
    let out = ltt(dispatch_frame(&ticker, receipt));
    assert_eq!(out, raw + LTT_UTC_OFFSET_SECS);
    assert_eq!(i64::from(out) % 86_400, 8 * 3_600 + 31 * 60 + 22);
}
