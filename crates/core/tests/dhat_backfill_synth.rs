//! S3-4: DHAT allocation bound test for the A3 backfill synth function.
//!
//! # Invariant under test
//!
//! `synthesize_ticks_from_minute_candles()` allocates **at most one**
//! heap block regardless of how many candles are passed in. The one
//! block is the output `Vec::with_capacity(n)`. Any additional block
//! means the per-candle loop is allocating — a performance regression
//! that would show up as GC-like pauses during gap-recovery bursts.
//!
//! # Why not zero-alloc?
//!
//! The synth function is cold path (runs once per detected gap), so a
//! strict zero-alloc rule would require passing a pre-allocated buffer
//! in. That's a premature optimisation for a function that runs a few
//! times per outage, not per tick. The practical invariant is:
//!
//! > The function's allocation count must NOT scale with the number
//! > of input candles — only with the number of calls.
//!
//! `1` is the current bound. If this test fails, a change either
//! introduced a per-candle allocation OR split the output into multiple
//! Vecs. Both are performance regressions.
//!
//! # Measurement
//!
//! DHAT counts heap blocks allocated between two `HeapStats::get()`
//! snapshots. We pre-build the input candles OUTSIDE the measured
//! region so the harness allocations don't pollute the count.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::tick_types::HistoricalCandle;
use tickvault_core::historical::backfill::synthesize_ticks_from_minute_candles;

fn make_candle(ts_utc: i64, close: f64) -> HistoricalCandle {
    HistoricalCandle {
        timestamp_utc_secs: ts_utc,
        security_id: 1333,
        exchange_segment_code: 1,
        timeframe: "1m",
        open: close - 0.5,
        high: close + 1.0,
        low: close - 1.0,
        close,
        volume: 12345,
        open_interest: 0,
    }
}

/// S3-4: DHAT allocation bound test. Runs inside a single process-wide
/// profiler (DHAT only allows one per process).
#[test]
fn dhat_backfill_synth_bounded_allocations() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Pre-build inputs OUTSIDE the measured region. These allocations
    // belong to the harness and must not count toward the synth's budget.
    let candles_small: Vec<HistoricalCandle> = (0..10)
        .map(|i| make_candle(1_700_000_000 + i * 60, 100.0 + i as f64))
        .collect();
    let candles_medium: Vec<HistoricalCandle> = (0..100)
        .map(|i| make_candle(1_700_000_000 + i * 60, 100.0 + i as f64))
        .collect();
    let candles_large: Vec<HistoricalCandle> = (0..1000)
        .map(|i| make_candle(1_700_000_000 + i * 60, 100.0 + i as f64))
        .collect();
    let received_at_nanos = 1_700_000_000_000_000_000_i64;

    // ---- Measure: synth with 10 candles ----
    let before = dhat::HeapStats::get();
    let ticks_small = synthesize_ticks_from_minute_candles(&candles_small, received_at_nanos);
    let after = dhat::HeapStats::get();
    let allocs_small = after.total_blocks.saturating_sub(before.total_blocks);
    assert_eq!(ticks_small.len(), 10);
    assert!(
        allocs_small <= 1,
        "S3-4: synth(10 candles) allocated {allocs_small} blocks — expected <= 1 \
         (just the output Vec::with_capacity). A per-candle allocation has been \
         introduced somewhere in the loop."
    );

    // ---- Measure: synth with 100 candles — same bound ----
    let before = dhat::HeapStats::get();
    let ticks_medium = synthesize_ticks_from_minute_candles(&candles_medium, received_at_nanos);
    let after = dhat::HeapStats::get();
    let allocs_medium = after.total_blocks.saturating_sub(before.total_blocks);
    assert_eq!(ticks_medium.len(), 100);
    assert!(
        allocs_medium <= 1,
        "S3-4: synth(100 candles) allocated {allocs_medium} blocks — expected <= 1. \
         The allocation count MUST NOT scale with the number of candles."
    );

    // ---- Measure: synth with 1000 candles — same bound ----
    let before = dhat::HeapStats::get();
    let ticks_large = synthesize_ticks_from_minute_candles(&candles_large, received_at_nanos);
    let after = dhat::HeapStats::get();
    let allocs_large = after.total_blocks.saturating_sub(before.total_blocks);
    assert_eq!(ticks_large.len(), 1000);
    assert!(
        allocs_large <= 1,
        "S3-4: synth(1000 candles) allocated {allocs_large} blocks — expected <= 1. \
         A 100x input size increase must not increase alloc count above 1."
    );

    // ---- Final sanity: empty input allocates at most 1 block ----
    let before = dhat::HeapStats::get();
    let empty_ticks = synthesize_ticks_from_minute_candles(&[], received_at_nanos);
    let after = dhat::HeapStats::get();
    let allocs_empty = after.total_blocks.saturating_sub(before.total_blocks);
    assert!(empty_ticks.is_empty());
    assert!(
        allocs_empty <= 1,
        "S3-4: synth([]) allocated {allocs_empty} blocks — expected <= 1 for the \
         empty Vec header."
    );
}
