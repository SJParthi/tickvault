//! DHAT: the core dispatcher allocates nothing while correcting Dhan trade
//! times after the 2026-11-20 00:00 IST cutover (scope lock 2026-11-21): the
//! corrected path, the cap refusal (an IST-epoch print), the band refusal
//! (raw 0) and the unstamped count. Pre-cutover receipts are gated by
//! `dhat_allocation.rs`.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod dhat_support;

use tickvault_common::constants::{QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE};

/// 2026-11-20 11:00:00 IST as a UTC receipt.
const RECEIPT: i64 = 1_795_152_600 * 1_000_000_000;

#[allow(clippy::arithmetic_side_effects)]
fn packet(code: u8, size: usize, ltt_offset: usize, segment: u8, ltt: u32) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    buf[0] = code;
    buf[1..3].copy_from_slice(&(size as u16).to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&13u32.to_le_bytes());
    buf[8..12].copy_from_slice(&25_000.0f32.to_le_bytes());
    buf[ltt_offset..ltt_offset + 4].copy_from_slice(&ltt.to_le_bytes());
    buf
}

#[test]
fn dhat_utc_ltt_correction_dispatch_is_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let confirmed = packet(1, TICKER_PACKET_SIZE, 12, 0, 1_795_152_600);
    let unconfirmed = packet(4, QUOTE_PACKET_SIZE, 14, 2, 1_795_152_000);
    let normal = packet(1, TICKER_PACKET_SIZE, 12, 0, 1_795_152_600 + 19_800);
    let zero = packet(4, QUOTE_PACKET_SIZE, 14, 2, 0);
    tickvault_core::parser::prewarm_dispatcher_counters();
    let _ = tickvault_core::parser::dispatch_frame(&confirmed, RECEIPT);
    // Outside the measured window: the corrected path really corrects.
    match tickvault_core::parser::dispatch_frame(&confirmed, RECEIPT) {
        Ok(tickvault_core::parser::ParsedFrame::Tick(t)) => {
            assert_eq!(t.exchange_timestamp, 1_795_152_600 + 19_800);
        }
        other => panic!("not a tick: {other:?}"),
    }
    let refused_before = tickvault_storage::ltt_correction::refusal_counts();
    let unstamped_before = tickvault_storage::ltt_correction::unstamped_count();
    let (_, blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for i in 0..10_000i64 {
                let r = RECEIPT + i * 1_000_000;
                assert!(tickvault_core::parser::dispatch_frame(&confirmed, r).is_ok());
                assert!(tickvault_core::parser::dispatch_frame_uncounted(&unconfirmed, r).is_ok());
                assert!(tickvault_core::parser::dispatch_frame(&normal, r).is_ok());
                assert!(tickvault_core::parser::dispatch_frame(&zero, r).is_ok());
                assert!(
                    tickvault_core::parser::dispatch_frame_uncounted_unstamped(&confirmed, r)
                        .is_ok()
                );
            }
        },
    );
    assert_eq!(
        blocks, 0,
        "the trade-time correction must not allocate on the dispatch path"
    );
    // Non-vacuity (2026-11-21 review): every path the header names ran inside
    // the window, read from the process counters (outside the window).
    let refused = tickvault_storage::ltt_correction::refusal_counts();
    assert!(
        refused.cap.saturating_sub(refused_before.cap) >= 10_000,
        "the cap refusal (IST-epoch print) ran: {refused:?}"
    );
    assert!(
        refused.band.saturating_sub(refused_before.band) >= 10_000,
        "the band refusal (raw 0) ran: {refused:?}"
    );
    assert!(
        tickvault_storage::ltt_correction::unstamped_count().saturating_sub(unstamped_before)
            >= 10_000,
        "the unstamped path ran"
    );
}
