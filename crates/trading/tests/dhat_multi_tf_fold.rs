//! DHAT allocation test for the multi-timeframe candle fold hot path.
//!
//! `MultiTfAggregator::consume_tick` is the per-tick fold: one hash lookup
//! into the dense slot table followed by `TF_COUNT` scalar folds. It is the
//! hottest loop in the tick pipeline once a live feed exists, and it carried
//! NO allocation guard — the 2026-08-09 rebuild shipped a zero-alloc claim in
//! its own doc comment that nothing mechanically proved.
//!
//! HONEST SCOPE: this test proves the fold does not ALLOCATE. It does not and
//! cannot prove the fold is CHEAP. The 2026-08-10 hoisting defect — widening
//! the same three `f32` prices once per TIMEFRAME instead of once per TICK,
//! i.e. `TF_COUNT × 3` = 63 decimal round-trips per tick against a 100 ns/tick
//! budget — was pure CPU cost with zero allocations, so this test would have
//! stayed green throughout. The guard against that regression is the
//! `TickPrices` doc contract plus `aggregator_cell`'s own unit tests; claiming
//! this file covers it would be exactly the false-OK the charter forbids.
//!
//! ONE profiler per test binary: `dhat::Profiler` is process-global and
//! panics if a second is built while one is live, so the whole hot path is
//! measured in a single test (the `dhat_risk_engine.rs` convention).

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;

mod dhat_support;

/// An exact multiple of 86_400 so `DAY + 33_300` is the 09:15 IST open.
/// The fold gates on session seconds-of-day, so a tick outside the window is
/// silently not folded — using a raw small timestamp made an earlier draft of
/// this test measure nothing.
const DAY: u32 = 1_779_321_600;
/// 09:15:00 IST of [`DAY`].
const OPEN: u32 = DAY + 33_300;

/// A sane, fully-populated tick — price positive and finite so it passes the
/// ingest gate, `day_open`/`day_close` positive so the widening path is
/// actually exercised rather than short-circuited by the `> 0.0` sentinel.
fn tick_at(ts: u32, price: f32, volume: u32) -> ParsedTick {
    ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        last_traded_price: price,
        exchange_timestamp: ts,
        volume,
        day_open: 24_000.25,
        day_close: 23_950.75,
        day_high: price,
        day_low: price,
        ..Default::default()
    }
}

#[test]
fn dhat_consume_tick_zero_alloc_in_bucket_and_across_boundaries() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    // Pre-warm OUTSIDE the measured region: the first tick for an instrument
    // legitimately allocates its slot (one hash entry + one dense-vec push).
    // Steady state is what the hot-path claim is about.
    let warm = tick_at(OPEN, 24_000.0, 10);
    agg.consume_tick(Feed::Dhan, &warm, None, |_, _, _, _, _| {});

    let (_, allocs) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            // (a) 10,000 IN-BUCKET folds — the common case, all 21 timeframes
            // per tick, no boundary crossed.
            for i in 0..10_000u32 {
                let t = tick_at(OPEN, 24_000.0 + (i % 50) as f32 * 0.05, 10 + i);
                agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
            }

            // (b) 60 BOUNDARY crossings — the seal path hands a `Copy`
            // `LiveCandleState` to the callback. A minute rollover fires this
            // across timeframes at once, so an allocation here would arrive as
            // a burst rather than a steady drip.
            let mut sealed = 0usize;
            for minute in 1..=60u32 {
                let t = tick_at(
                    OPEN + minute * 60,
                    24_000.0 + minute as f32,
                    100_000 + minute,
                );
                agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {
                    sealed += 1;
                });
            }
            assert!(
                sealed > 0,
                "boundary crossings must actually seal — otherwise this test \
                 measures nothing and passes vacuously"
            );
        },
    );

    assert_eq!(
        allocs, 0,
        "MultiTfAggregator::consume_tick must be zero-alloc for both in-bucket \
         folds and bucket-boundary seals, got {allocs} allocation blocks"
    );
}
