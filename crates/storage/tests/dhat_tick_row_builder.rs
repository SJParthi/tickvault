//! DHAT zero-allocation test for the C1 shared `ticks` ILP row builder.
//!
//! Separate binary because DHAT allows only one profiler per process.
//! Verifies Principle #1: the converged `build_tick_row_for_feed` adds NO heap
//! allocation on the per-tick path beyond the ILP buffer's own internal growth.
//! The buffer is pre-warmed (first row may grow its internal Vec), then 1000 rows
//! are built into the SAME reused buffer — exactly how the production writer
//! reuses its buffer between flushes.

use questdb::ingress::{Buffer, ProtocolVersion};
use tickvault_storage::tick_row_builder::{RawTickFields, build_tick_row_for_feed};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// A fully-populated Dhan-shape `RawTickFields` (every Option Some → widest row).
fn dhan_fields(capture_seq: i64) -> RawTickFields {
    RawTickFields {
        security_id: 49081,
        segment: "NSE_FNO",
        ltp: 21_000.0,
        volume: 50_000,
        ts_ist_nanos: 1_740_556_500_000_000_000,
        capture_seq,
        open: Some(20_950.0),
        high: Some(21_020.0),
        low: Some(20_920.0),
        close: Some(20_900.0),
        oi: Some(120_000),
        avg_price: Some(20_990.0),
        last_trade_qty: Some(75),
        total_buy_qty: Some(25_000),
        total_sell_qty: Some(25_000),
        exchange_timestamp: Some(1_740_556_500),
        received_at_ist_nanos: Some(1_740_576_300_123_456_789),
        payload_hash: Some(-2_154_897_302_305_733_279),
    }
}

#[test]
fn dhat_build_tick_row_for_feed_zero_alloc() {
    use tickvault_common::feed::Feed;
    let _profiler = dhat::Profiler::builder().testing().build();

    let mut buffer = Buffer::new(ProtocolVersion::V1);
    // Pre-warm: the very first row grows the buffer's internal Vec. Build one
    // widest row then `clear()` (which drops length, keeps capacity) so the
    // buffer's internal Vec is at its steady-state capacity before measuring —
    // exactly how the production writer reuses one buffer and `flush` drains it.
    build_tick_row_for_feed(&mut buffer, &dhan_fields(0), Feed::Dhan).expect("prewarm");
    buffer.clear();

    let stats_before = dhat::HeapStats::get();

    // Build one widest row per iteration, clearing between rows (mirrors a
    // flush). With capacity already at steady state, no allocation should occur.
    for i in 0..1000 {
        build_tick_row_for_feed(&mut buffer, &dhan_fields(i), Feed::Dhan).expect("build dhan");
        buffer.clear();
    }

    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert_eq!(
        allocs_during, 0,
        "build_tick_row_for_feed allocated {allocs_during} blocks over 1000 rows into a \
         pre-grown buffer — PRINCIPLE #1 VIOLATED. The converged builder must be zero-alloc \
         beyond the ILP buffer's one-time internal growth."
    );
}
