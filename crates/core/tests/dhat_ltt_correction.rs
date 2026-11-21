//! DHAT: the core dispatcher allocates nothing while the Dhan trade-time
//! correction is ARMED (scope lock 2026-11-20). The unarmed path is gated by
//! `dhat_allocation.rs`; this binary publishes an armed table first.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod dhat_support;

use tickvault_common::constants::{QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE};
use tickvault_storage::ltt_correction::{LttEntry, LttTable, install_table, receipt_ist_day};

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
fn dhat_armed_ltt_correction_dispatch_is_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let mut table = LttTable::empty();
    assert!(table.arm(
        receipt_ist_day(RECEIPT),
        LttEntry {
            id: 0,
            segment: 0,
            from_nanos: RECEIPT - 1_000_000_000,
            to_nanos: i64::MAX,
        },
    ));
    install_table(table);
    let confirmed = packet(1, TICKER_PACKET_SIZE, 12, 0, 1_795_152_600);
    let unconfirmed = packet(4, QUOTE_PACKET_SIZE, 14, 2, 1_795_152_000);
    let normal = packet(1, TICKER_PACKET_SIZE, 12, 0, 1_795_152_600 + 19_800);
    tickvault_core::parser::prewarm_dispatcher_counters();
    // Warm the ArcSwap thread-local debt slot before measuring.
    let _ = tickvault_core::parser::dispatch_frame(&confirmed, RECEIPT);
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
            }
        },
    );
    assert_eq!(
        blocks, 0,
        "the armed correction must not allocate on the dispatch path"
    );
    install_table(LttTable::empty());
}
