//! DHAT zero-allocation ratchet for `MultiTfAggregator::consume_tick` — the
//! 21-timeframe fold that is the single hottest per-tick loop for BOTH feeds
//! (Dhan + Groww share the ONE engine, threaded by `FeedStrategy` value).
//!
//! Why (WARNING from the 2026-07-01 adversarial O(1) hunt): the row builder
//! (`dhat_tick_row_builder.rs`), the bar-cache lookup (`dhat_bar_cache_lookup.rs`)
//! and the Dhan binary WS read (`dhat_ws_reader_zero_alloc.rs`) are DHAT-guarded,
//! but the 21-TF fold itself — `consume_tick` folding a tick into all 21 cells —
//! was alloc-free only BY INSPECTION, with no ratchet. A future edit that adds a
//! per-tick allocation (a `format!` in a log, a `Vec`, a `.collect()`, a boxed
//! closure) to the hottest loop would compound into hundreds of allocs/sec at
//! tick rate and blow the 100 ns p99 budget — with nothing catching it. This
//! test makes that a BUILD FAILURE.
//!
//! Pattern mirrors `crates/storage/tests/dhat_tick_row_builder.rs`: pre-warm the
//! papaya entry + one fold (settles any one-time lazy init), snapshot
//! `dhat::HeapStats`, fold 1000 ticks in steady state, assert `total_blocks`
//! delta is 0. Covered for `FeedStrategy::DHAN` (None override) AND
//! `FeedStrategy::GROWW` (Some(cumulative) override — the i64→u64 path).

use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::MultiTfAggregator;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;

mod dhat_support;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// IST-midnight-aligned base epoch second (per the module's own test vectors),
/// + 34_200 s = 09:30:00 IST — squarely inside the [09:15, 15:30) candle window
/// so the tick actually folds (an out-of-window tick early-returns and would not
/// exercise the fold path we are measuring).
const IST_0930_SECS: u32 = 1_779_235_200 + 34_200;

fn tick(security_id: u64, segment: u8, volume: u32) -> ParsedTick {
    let mut t = ParsedTick::default();
    t.security_id = security_id;
    t.exchange_segment_code = segment;
    t.exchange_timestamp = IST_0930_SECS;
    t.last_traded_price = 21_000.0;
    t.volume = volume;
    t.open_interest = 120_000;
    t
}

/// Fold `iters` ticks through the 21-TF engine and return the `total_blocks`
/// delta measured around ONLY the steady-state loop (after a prewarm fold).
fn measure_fold(strategy: FeedStrategy, cumulative_override: Option<u64>) -> u64 {
    let security_id: u64 = 13;
    let segment: u8 = 0;
    let agg = MultiTfAggregator::new();
    // pre_populate builds the InstrumentEntry (all 21 TF cells) up front — the
    // production path (main.rs / groww_bridge.rs) does the same before ticks flow.
    agg.pre_populate(std::iter::once((security_id, segment)));

    let t = tick(security_id, segment, 50_000);
    // Prewarm: the FIRST fold opens the bucket / settles any one-time lazy work.
    agg.consume_tick(&t, segment, strategy, cumulative_override, |_, _| {});

    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners — see
    // dhat_support/mod.rs). Budget UNCHANGED: exactly 0 blocks. Re-running
    // folds the SAME tick into the SAME open bucket — steady state holds
    // across attempts.
    let (_, blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            // Steady state: 1000 folds into the SAME open bucket (same timestamp →
            // no seal), which is the dominant per-tick hot path. No heap alloc
            // expected.
            for _ in 0..1000 {
                agg.consume_tick(&t, segment, strategy, cumulative_override, |_, _| {});
            }
        },
    );
    blocks
}

// ONE test fn: DHAT allows only ONE `Profiler` per process, so both feeds are
// measured inside a single profiler (each `measure_fold` builds its own
// aggregator + prewarm BEFORE its `before` snapshot, so the delta wraps only
// that feed's 1000-fold steady-state loop).
#[test]
fn dhat_consume_tick_both_feeds_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Dhan — None override (reads the u32 tick.volume).
    let dhan_blocks = measure_fold(FeedStrategy::DHAN, None);
    assert_eq!(
        dhan_blocks, 0,
        "DHAT ratchet: MultiTfAggregator::consume_tick (Dhan, 21-TF fold) allocated \
         {dhan_blocks} heap block(s) across 1000 steady-state folds — the hottest \
         per-tick loop MUST be zero-alloc. A new format!/Vec/collect/box on this path \
         compounds into hundreds of allocs/sec and blows the 100 ns p99 budget."
    );

    // Groww — Some(cumulative) override (the u64 path avoiding i64→u32 truncation).
    let groww_blocks = measure_fold(FeedStrategy::GROWW, Some(6_000_000_000));
    assert_eq!(
        groww_blocks, 0,
        "DHAT ratchet: MultiTfAggregator::consume_tick (Groww, 21-TF fold, u64 override) \
         allocated {groww_blocks} heap block(s) across 1000 steady-state folds — the Groww \
         hot path MUST be zero-alloc, identical to Dhan."
    );
}
