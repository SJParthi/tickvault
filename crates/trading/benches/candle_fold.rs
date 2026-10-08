//! Benchmark: the per-tick candle fold, `MultiTfAggregator::consume_tick`.
//!
//! Measures the steady-state HIT path at a realistic table size: 20,000
//! instruments already hold a slot, and each measured tick lands inside the
//! bucket its slot already has open (no seal). That is one hash probe on the
//! composite key plus `TF_COUNT` scalar folds — the path every live tick takes.
//!
//! Added with audit plan PR10 (faster hashing on the per-tick maps) so the
//! hash switch is measured rather than claimed. No budget is set in
//! `quality/benchmark-budgets.toml` yet: the gate reports it as INFO until a
//! first measured run on CI gives a number to set one from (never guessed).

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;

/// An exact multiple of 86_400, so `DAY + 33_300` is 09:15:00 IST (the same
/// anchor `dhat_multi_tf_fold.rs` uses; a tick outside the candle session is
/// not folded and the bench would measure nothing).
const DAY: u32 = 1_779_321_600;
/// 09:20:00 IST: inside the session, past the 09:15 open bar.
const T0: u32 = DAY + 33_600;
/// Instruments holding a slot before measurement starts: the realistic live
/// table, and a small one that stays in cache, so the hash cost is visible
/// apart from memory stalls on slot data.
const TABLE_SIZES: [(u64, &str); 2] = [
    (20_000, "consume_tick_hit_20k"),
    (500, "consume_tick_hit_500"),
];
/// Ticks per measured batch, spread across the table.
const TICKS_PER_BATCH: u32 = 500;
/// NSE_FNO segment code.
const NSE_FNO: u8 = 2;

fn tick(security_id: u64, price: f32, volume: u32) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: NSE_FNO,
        last_traded_price: price,
        exchange_timestamp: T0,
        volume,
        day_open: 100.0,
        day_close: 99.5,
        day_high: price,
        day_low: price,
        received_at_nanos: (i64::from(T0) - 19_800 + 1) * 1_000_000_000,
        ..Default::default()
    }
}

fn warmed_aggregator(instruments: u64) -> MultiTfAggregator {
    let mut agg =
        MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, instruments as usize + 1_000);
    for sid in 1..=instruments {
        agg.consume_tick(Feed::Dhan, &tick(sid, 100.0, 10), None, |_, _, _, _, _| {});
    }
    // Prove the measured shape is the in-bucket fold and not a refusal: a
    // tick the fold turns away is cheap, and timing it would flatter the
    // number. This probe uses volume 11, so the batches start at 12.
    let stats = agg.consume_tick(Feed::Dhan, &tick(1, 100.05, 11), None, |_, _, _, _, _| {});
    assert!(
        !stats.refused_price
            && !stats.out_of_session
            && !stats.future_trading_day
            && !stats.slot_exhausted
            && !stats.untraded_sentinel
            && stats.late_count == 0
            && stats.sealed_count == 0,
        "bench tick was not an in-bucket fold: {stats:?}"
    );
    agg
}

fn bench_consume_tick_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("candles");
    group.throughput(Throughput::Elements(u64::from(TICKS_PER_BATCH)));
    for (instruments, name) in TABLE_SIZES {
        let mut agg = warmed_aggregator(instruments);
        // Cumulative volume only moves forward, so each batch continues from
        // the last one. A batch never visits an instrument twice (it is no
        // longer than the smaller table), so no tick repeats a volume.
        let mut volume: u32 = 11;
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    volume = volume.wrapping_add(1).max(12);
                    volume
                },
                |v| {
                    // A stride coprime to the table size visits it out of
                    // insertion order, as live ticks do.
                    let mut sid = 1u64;
                    for i in 0..TICKS_PER_BATCH {
                        let price = 100.0 + (i % 20) as f32 * 0.05;
                        black_box(agg.consume_tick(
                            Feed::Dhan,
                            &tick(sid, price, v),
                            None,
                            |_, _, _, _, _| {},
                        ));
                        sid = (sid + 7_919) % instruments + 1;
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_consume_tick_hit);
criterion_main!(benches);
