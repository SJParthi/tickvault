//! Benchmark: the per-tick candle fold, `MultiTfAggregator::consume_tick`.
//!
//! Three shapes. The two hit benches time 500-tick batches (Criterion reports
//! per batch; `bench-gate.sh` divides by the `[elements]` entry); the live mix
//! times one tick per element:
//!
//! * `candles/consume_tick_hit_20k` and `candles/consume_tick_hit_500`: the
//!   steady-state HIT path. 20,000 (or 500) instruments already hold a slot,
//!   and each measured tick lands inside the bucket its slot already has open
//!   in every timeframe (no seal). One hash probe on the composite key plus
//!   `TF_COUNT` in-bucket folds. 09:20 IST, so three timeframes still have the
//!   day's first bucket open and adopt the exchange day high and low.
//! * `candles/consume_tick_live_mix_2k`: time moves forward, about 500 ticks a
//!   second over 2,000 instruments, so each instrument trades every few
//!   seconds: almost every tick rolls its 1-second bucket (a seal and an
//!   emission) and every minute the 1-minute buckets roll too. This is the
//!   shape the live feed has, and the dearer one.
//!
//! Added with audit plan PR10 (faster hashing on the per-tick maps). The
//! budget `candles_consume_tick` (per tick) and its `[elements]` divisor for
//! the 500-tick hit batches are in `quality/benchmark-budgets.toml` since plan
//! ITEM DT (2026-10-30), from the measured run recorded there.
//!
//! # The p99 gate (plan ITEM DT, 2026-10-30)
//!
//! Criterion's budget check reads a MEDIAN. After Criterion, under
//! `cargo bench` only, the live-mix stream is timed tick by tick (65,536
//! ticks, one `Instant` pair each, so the figures read slightly high) and its
//! p99 is held to the same `candles_consume_tick` budget times
//! `BENCH_BUDGET_MULTIPLIER` (the multiplier the median arm uses). Above it
//! the run exits 1 and the bench job fails. The gate fails an over-budget
//! sample first (self-test), so it cannot pass vacuously.
//!
//! Run: `cargo bench -p tickvault-trading --bench candle_fold`

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group};

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::{ConsumeStats, MultiTfAggregator};

/// An exact multiple of 86_400, so `DAY + 33_300` is 09:15:00 IST (the same
/// anchor `dhat_multi_tf_fold.rs` uses; a tick outside the candle session is
/// not folded and the bench would measure nothing).
const DAY: u32 = 1_779_321_600;
/// 09:20:00 IST: inside the session, past the 09:15 open bar.
const T0: u32 = DAY + 33_600;
/// 09:16:00 IST: where the live-mix stream starts.
const LIVE_START: u32 = DAY + 33_360;
/// Ticks per measured hit batch, spread across the table.
const TICKS_PER_BATCH: u32 = 500;
/// NSE_FNO segment code.
const NSE_FNO: u8 = 2;
/// IST is UTC + 5:30.
const IST_OFFSET_SECS: i64 = 19_800;
/// Instruments in the live-mix stream.
const LIVE_INSTRUMENTS: u64 = 2_000;
/// Live-mix ticks per second of exchange time (about the live feed's rate).
const LIVE_TICKS_PER_SEC: u64 = 500;
/// Live-mix ticks in one pool; the aggregator is rebuilt (untimed) after each.
/// 65,536 ticks at 500 a second is about 131 s, so every 1-minute bucket rolls
/// twice inside one pool.
const LIVE_POOL: usize = 65_536;
/// The budget key the fold's median and p99 are both held to.
const FOLD_BUDGET_KEY: &str = "candles_consume_tick";
/// Same variable the drain's p99 gate and `bench-gate.sh` read.
const BUDGET_MULTIPLIER_ENV: &str = "BENCH_BUDGET_MULTIPLIER";
/// Largest multiplier accepted (the drain gate's bound).
const MAX_BUDGET_MULTIPLIER: f64 = 10.0;

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
        received_at_nanos: (i64::from(T0) - IST_OFFSET_SECS + 1) * 1_000_000_000,
        ..Default::default()
    }
}

fn refused(stats: &ConsumeStats) -> bool {
    stats.refused_price
        || stats.out_of_session
        || stats.future_trading_day
        || stats.slot_exhausted
        || stats.untraded_sentinel
        || stats.untraded_timestamp
        || stats.out_of_band_timestamp
        || stats.refused_timestamp
        || stats.stale_trading_day
        || stats.repeat_quote
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
        !refused(&stats) && stats.late_count == 0 && stats.sealed_count == 0,
        "bench tick was not an in-bucket fold: {stats:?}"
    );
    agg
}

fn hit_bench(c: &mut Criterion, instruments: u64, run: fn(&mut Criterion, HitRun)) {
    run(
        c,
        HitRun {
            agg: warmed_aggregator(instruments),
            instruments,
            // Cumulative volume only moves forward, so each batch continues
            // from the last one. A batch never visits an instrument twice (it
            // is no longer than the smaller table), so no tick repeats a volume.
            volume: 11,
        },
    );
}

struct HitRun {
    agg: MultiTfAggregator,
    instruments: u64,
    volume: u32,
}

/// One measured batch: `TICKS_PER_BATCH` in-bucket ticks at volume `v`. A
/// stride coprime to the table size visits it out of insertion order, as
/// live ticks do.
fn hit_batch(agg: &mut MultiTfAggregator, instruments: u64, v: u32) {
    let mut sid = 1u64;
    for i in 0..TICKS_PER_BATCH {
        let price = 100.0 + (i % 20) as f32 * 0.05;
        black_box(agg.consume_tick(Feed::Dhan, &tick(sid, price, v), None, |_, _, _, _, _| {}));
        sid = (sid + 7_919) % instruments + 1;
    }
}

fn hit_20k(c: &mut Criterion, r: HitRun) {
    let HitRun {
        mut agg,
        instruments,
        mut volume,
    } = r;
    let mut group = c.benchmark_group("candles");
    group.throughput(Throughput::Elements(u64::from(TICKS_PER_BATCH)));
    group.bench_function("consume_tick_hit_20k", |b| {
        b.iter_batched(
            || {
                volume = volume.wrapping_add(1).max(12);
                volume
            },
            |v| hit_batch(&mut agg, instruments, v),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn hit_500(c: &mut Criterion, r: HitRun) {
    let HitRun {
        mut agg,
        instruments,
        mut volume,
    } = r;
    let mut group = c.benchmark_group("candles");
    group.throughput(Throughput::Elements(u64::from(TICKS_PER_BATCH)));
    group.bench_function("consume_tick_hit_500", |b| {
        b.iter_batched(
            || {
                volume = volume.wrapping_add(1).max(12);
                volume
            },
            |v| hit_batch(&mut agg, instruments, v),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_consume_tick_hit(c: &mut Criterion) {
    hit_bench(c, 20_000, hit_20k);
    hit_bench(c, 500, hit_500);
}

/// The live-mix tick `n` of a pool: instrument by a stride coprime to the
/// table, exchange time rising `LIVE_TICKS_PER_SEC` ticks a second from
/// 09:16:01, cumulative volume rising with `n` (so each instrument's rises),
/// received half a second after its trade.
fn live_tick(n: u64) -> ParsedTick {
    let security_id = (n * 7_919) % LIVE_INSTRUMENTS + 1;
    let secs = LIVE_START + 1 + u32::try_from(n / LIVE_TICKS_PER_SEC).unwrap_or(0);
    let price = 100.0 + (n % 37) as f32 * 0.05;
    ParsedTick {
        security_id,
        exchange_segment_code: NSE_FNO,
        last_traded_price: price,
        last_trade_quantity: 25,
        exchange_timestamp: secs,
        volume: 1_000 + u32::try_from(n).unwrap_or(0),
        day_open: 100.0,
        day_close: 99.5,
        day_high: 101.8,
        day_low: 99.9,
        open_interest: 50_000 + u32::try_from(n % 1_000).unwrap_or(0),
        total_buy_quantity: 4_000,
        total_sell_quantity: 3_500,
        received_at_nanos: (i64::from(secs) - IST_OFFSET_SECS) * 1_000_000_000 + 500_000_000,
        ..Default::default()
    }
}

/// A fresh aggregator with one tick per instrument at 09:16:00, so every
/// pool tick folds into a seeded slot.
fn live_aggregator() -> MultiTfAggregator {
    let mut agg = MultiTfAggregator::with_capacity(
        FeedStrategy::DEFAULT,
        usize::try_from(LIVE_INSTRUMENTS).unwrap_or(0) + 1_000,
    );
    for sid in 1..=LIVE_INSTRUMENTS {
        let mut t = live_tick(0);
        t.security_id = sid;
        t.exchange_timestamp = LIVE_START;
        t.received_at_nanos = (i64::from(LIVE_START) - IST_OFFSET_SECS) * 1_000_000_000;
        t.volume = 900;
        agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
    }
    agg
}

/// One pool's ticks, built before any timing.
fn live_pool() -> Vec<ParsedTick> {
    (0..LIVE_POOL as u64).map(live_tick).collect()
}

/// Proves the live-mix pool is folded, not refused, and that it seals.
fn assert_live_mix_shape(pool: &[ParsedTick]) {
    let mut agg = live_aggregator();
    let (mut seals, mut refusals) = (0u64, 0u64);
    for t in pool {
        let stats = agg.consume_tick(Feed::Dhan, t, None, |_, _, _, _, _| {});
        seals += u64::from(stats.sealed_count);
        refusals += u64::from(refused(&stats));
    }
    assert_eq!(refusals, 0, "live-mix ticks were refused");
    assert!(
        seals > LIVE_POOL as u64,
        "live-mix pool sealed only {seals} bars in {LIVE_POOL} ticks"
    );
}

fn bench_consume_tick_live_mix(c: &mut Criterion) {
    let pool = live_pool();
    assert_live_mix_shape(&pool);
    let mut group = c.benchmark_group("candles");
    group.throughput(Throughput::Elements(1));
    group.bench_function("consume_tick_live_mix_2k", |b| {
        let mut agg = live_aggregator();
        let mut cursor = 0usize;
        b.iter_custom(|iters| {
            let mut timed = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                if cursor == pool.len() {
                    // Untimed: time must not run past the session close.
                    agg = live_aggregator();
                    cursor = 0;
                }
                let take = usize::try_from(left)
                    .unwrap_or(usize::MAX)
                    .min(pool.len() - cursor);
                let start = Instant::now();
                for t in &pool[cursor..cursor + take] {
                    black_box(agg.consume_tick(Feed::Dhan, t, None, |_, _, _, _, bar| {
                        black_box(bar);
                    }));
                }
                timed += start.elapsed();
                cursor += take;
                left -= take as u64;
            }
            timed
        });
    });
    group.finish();
}

/// `sorted[ceil(len × num / den) − 1]`, the nearest-rank quantile.
fn quantile_of_sorted(sorted: &[u64], num: usize, den: usize) -> Option<u64> {
    if sorted.is_empty() || den == 0 {
        return None;
    }
    let rank = sorted
        .len()
        .saturating_mul(num)
        .div_ceil(den)
        .clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

/// The positive integer `key` in the `[budgets]` section of `text`.
fn budget_from_toml(text: &str, key: &str) -> Option<u64> {
    let mut in_budgets = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_budgets = line == "[budgets]";
            continue;
        }
        if !in_budgets {
            continue;
        }
        if let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            return value.trim().parse::<u64>().ok().filter(|v| *v > 0);
        }
    }
    None
}

/// `true` when the p99 of `sorted` is within `budget × multiplier`.
fn p99_within(sorted: &[u64], budget_ns: u64, multiplier: f64) -> Option<(u64, f64, bool)> {
    let p99 = quantile_of_sorted(sorted, 99, 100)?;
    let ceiling = budget_ns as f64 * multiplier;
    Some((p99, ceiling, p99 as f64 <= ceiling))
}

/// A gate that cannot fail proves nothing: an over-budget p99 must fail and
/// one exactly at the budget must pass.
fn gate_self_test(budget_ns: u64) -> Result<(), String> {
    let build = |p99: u64| -> Vec<u64> {
        let mut v = vec![1u64; 98];
        v.push(p99);
        v.push(p99.saturating_mul(2));
        v
    };
    match p99_within(&build(budget_ns + 1), budget_ns, 1.0) {
        Some((_, _, false)) => {}
        other => return Err(format!("an over-budget p99 passed the gate: {other:?}")),
    }
    match p99_within(&build(budget_ns), budget_ns, 1.0) {
        Some((_, _, true)) => Ok(()),
        other => Err(format!("a p99 at the budget failed the gate: {other:?}")),
    }
}

/// Times the live-mix pool tick by tick and holds its p99 to the budget.
fn fold_tail_gate() -> Result<(), String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../quality/benchmark-budgets.toml"
    );
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let budget = budget_from_toml(&text, FOLD_BUDGET_KEY)
        .ok_or_else(|| format!("no positive `{FOLD_BUDGET_KEY}` in [budgets] of {path}"))?;
    let env_value = std::env::var(BUDGET_MULTIPLIER_ENV).ok();
    let multiplier = match env_value.as_deref() {
        None => 1.0,
        Some(v) => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|m| m.is_finite() && (1.0..=MAX_BUDGET_MULTIPLIER).contains(m))
            .ok_or_else(|| format!("{BUDGET_MULTIPLIER_ENV} = {v:?} is not a number in [1, 10]"))?,
    };
    gate_self_test(budget)?;

    let pool = live_pool();
    // One untimed pass warms caches and the branch predictor the way a live
    // session already is; the timed pass starts from a fresh aggregator.
    let mut agg = live_aggregator();
    for t in &pool {
        black_box(agg.consume_tick(Feed::Dhan, t, None, |_, _, _, _, _| {}));
    }
    let mut agg = live_aggregator();
    let mut samples: Vec<u64> = Vec::with_capacity(pool.len());
    for t in &pool {
        let start = Instant::now();
        black_box(agg.consume_tick(Feed::Dhan, t, None, |_, _, _, _, bar| {
            black_box(bar);
        }));
        samples.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    samples.sort_unstable();
    let at = |num: usize, den: usize| quantile_of_sorted(&samples, num, den).unwrap_or(0);
    println!(
        "candles/consume_tick_live_mix_2k percentiles over {} ticks: p50 = {} ns, \
         p99 = {} ns, p99.9 = {} ns, max = {} ns (includes one Instant pair per tick)",
        samples.len(),
        at(1, 2),
        at(99, 100),
        at(999, 1_000),
        samples.last().copied().unwrap_or(0),
    );
    let (p99, ceiling, within) =
        p99_within(&samples, budget, multiplier).ok_or_else(|| "no samples".to_string())?;
    println!(
        "candle fold p99 gate: p99 = {p99} ns per tick, ceiling {ceiling:.0} ns \
         (budget {budget} x {multiplier}): {}",
        if within { "PASS" } else { "FAIL" }
    );
    if within {
        Ok(())
    } else {
        Err(format!(
            "candle fold p99 {p99} ns per tick is over the {ceiling:.0} ns ceiling"
        ))
    }
}

criterion_group!(benches, bench_consume_tick_hit, bench_consume_tick_live_mix);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    if std::env::args().any(|a| a == "--bench")
        && let Err(why) = fold_tail_gate()
    {
        eprintln!("candle fold p99 gate FAILED: {why}");
        std::process::exit(1);
    }
}
