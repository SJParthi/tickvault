//! Benchmark: the per-tick candle fold with the instrument's SLOT not in the
//! CPU cache (plan item PF-7, 2026-11-06).
//!
//! `candle_fold.rs` measures the fold with its tables warm: 20,000 slots of
//! about 2.9 KB fit the large L3 of the dev container, and the 2,000-slot live
//! mix fits L2. The live box is different: 25,000 slots are about 72 MB
//! against a 36 MB L3 on the r8g host, so a tick for an instrument that has
//! not traded for a few seconds reads its slot from memory. This bench
//! reproduces that without any hook in production code: it builds
//! `COLD_TABLES` full aggregators (25,000 slots each, about 72 MB each, so
//! several times the dev container's 260 MB L3) and sends each tick to a
//! pseudo-random (table, instrument) pair, so most ticks miss the cache for
//! the slot.
//!
//! What stays warm: each table's index is about 25,000 small entries (about
//! 1 MB, about 12 MB for all tables), which fits the dev L3, so the index
//! probe mostly hits L3 and it is the SLOT that comes from memory. On the r8g
//! host the indexes compete with the slots for a 36 MB L3, so the probe there
//! can miss too; this bench does not model that (Assumed: slightly faster
//! here than on the host).
//!
//! Two shapes, both at 25,000 slots per table:
//!
//! * `candles_cold/consume_tick_hit_25k`: exchange time fixed at 09:20:00, so
//!   every tick folds into the bucket each timeframe already has open (no
//!   seal). This touches only the open-bucket half of the slot.
//! * `candles_cold/consume_tick_roll_25k`: exchange time rises so each
//!   instrument trades about once every `ROLL_SECS_PER_INSTRUMENT` seconds, as
//!   on the live feed: almost every tick rolls its 1-second bucket (a seal),
//!   and every minute the 1-minute buckets roll too.
//!
//! The shape is checked, not assumed (`ShapeCount::check`): the untimed
//! settling pass before Criterion, the Criterion ticks themselves and every
//! report pass must have no refused tick, no seal for Hit and a seal on more
//! than half the ticks for Roll, or the run panics. `shape_check_self_test`
//! runs first and proves the check fails a Roll run labelled Hit, a Hit run
//! labelled Roll, and a refusal.
//!
//! Timing: each Criterion element is one tick, and BOTH the Criterion figure
//! and the report's per-tick percentiles include one `Instant` pair per tick
//! (measured on the x86 dev container over two runs: p50 29–39 ns, p99
//! 39–48 ns; the report prints it each run as "empty Instant pair"). The
//! budget gate,
//! `bench-gate.sh`, reads Criterion's MEDIAN, which is the median of the 30
//! per-sample MEANS (each sample divides its total by its tick count), not the
//! per-tick p50: a sample's mean carries its share of slow outliers, so it
//! reads higher than the report's p50. After Criterion, under `cargo bench`
//! only, the report times ticks one by one and prints p50 / p99 / p99.9 for
//! both shapes, cold (all tables) and warm (one table, which fits the dev L3).
//! The report never fails the run on time, only on shape; the budget is
//! `candles_cold_consume_tick` in `quality/benchmark-budgets.toml`.
//!
//! Memory: about 72 MB per table plus its index, so about 0.9 GB at the
//! default `COLD_TABLES`.
//!
//! Run: `cargo bench -p tickvault-trading --bench candle_fold_cold`

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group};

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::{
    AGGREGATOR_MAX_SLOTS, ConsumeStats, MultiTfAggregator,
};

/// An exact multiple of 86_400 (see `candle_fold.rs`).
const DAY: u32 = 1_779_321_600;
/// 09:20:00 IST: the hit shape's fixed exchange time.
const HIT_TS: u32 = DAY + 33_600;
/// 09:16:00 IST: where the roll shape starts.
const ROLL_START: u32 = DAY + 33_360;
/// NSE_FNO segment code.
const NSE_FNO: u8 = 2;
/// IST is UTC + 5:30.
const IST_OFFSET_SECS: i64 = 19_800;
/// Instruments per table: the production ceiling.
const INSTRUMENTS: u64 = AGGREGATOR_MAX_SLOTS as u64;
/// Tables in the cold shape. 12 × ~72 MB is about 3.3 times the dev
/// container's L3, so about 70% of uniformly random slots miss it.
const COLD_TABLES: usize = 12;
/// In the roll shape, each instrument trades about once this many seconds.
const ROLL_SECS_PER_INSTRUMENT: u64 = 5;
/// Ticks timed one by one per report pass.
const REPORT_TICKS: usize = 200_000;
/// Ticks folded untimed before each report pass (cache state of a session).
const REPORT_WARMUP_TICKS: usize = 50_000;

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

/// What a run of ticks actually did, so a timed figure cannot quietly measure
/// a cheaper path than its name says (a refused tick is cheap; a Hit run that
/// seals, or a Roll run that does not, is a different shape).
#[derive(Clone, Copy, Debug, Default)]
struct ShapeCount {
    ticks: u64,
    seals: u64,
    refusals: u64,
}

impl ShapeCount {
    fn note(&mut self, stats: &ConsumeStats) {
        self.ticks += 1;
        self.seals += u64::from(stats.sealed_count);
        self.refusals += u64::from(refused(stats));
    }

    /// `Ok` when the counted ticks are the shape `shape` claims: none refused,
    /// and no seal at all for `Hit`, a seal on more than half the ticks for
    /// `Roll` (measured: about 0.9 seals per tick).
    fn check(&self, shape: Shape) -> Result<(), String> {
        if self.ticks == 0 {
            return Err("no ticks were counted".to_string());
        }
        if self.refusals != 0 {
            return Err(format!(
                "{} of {} ticks were refused",
                self.refusals, self.ticks
            ));
        }
        match shape {
            Shape::Hit if self.seals != 0 => Err(format!(
                "the Hit shape sealed {} bars in {} ticks (it must fold in-bucket only)",
                self.seals, self.ticks
            )),
            Shape::Roll if self.seals.saturating_mul(2) <= self.ticks => Err(format!(
                "the Roll shape sealed only {} bars in {} ticks",
                self.seals, self.ticks
            )),
            _ => Ok(()),
        }
    }
}

/// The shape check must be able to fail, or it proves nothing: a refusal
/// fails it, a real Hit-labelled check of a run that rolls fails it, and a
/// real Roll-labelled check of a run that does not roll fails it.
fn shape_check_self_test() {
    let refused_one = ShapeCount {
        ticks: 10,
        seals: 0,
        refusals: 1,
    };
    assert!(refused_one.check(Shape::Hit).is_err(), "a refusal passed");
    assert!(
        ShapeCount::default().check(Shape::Hit).is_err(),
        "no ticks passed"
    );
    for (built, other) in [(Shape::Roll, Shape::Hit), (Shape::Hit, Shape::Roll)] {
        let mut run = ColdRun::new(1, built);
        let mut count = ShapeCount::default();
        for _ in 0..2_000 {
            count.note(&run.fold_one());
        }
        assert!(
            count.check(built).is_ok(),
            "a real {built:?} run failed its own check: {count:?}"
        );
        assert!(
            count.check(other).is_err(),
            "a {built:?} run passed the {other:?} check: {count:?}"
        );
    }
}

/// One tick for `security_id` at exchange second `secs`, cumulative `volume`.
fn tick(security_id: u64, secs: u32, price: f32, volume: u32) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: NSE_FNO,
        last_traded_price: price,
        last_trade_quantity: 25,
        exchange_timestamp: secs,
        volume,
        day_open: 100.0,
        day_close: 99.5,
        day_high: 101.8,
        day_low: 99.0,
        open_interest: 50_000,
        total_buy_quantity: 4_000,
        total_sell_quantity: 3_500,
        received_at_nanos: (i64::from(secs) - IST_OFFSET_SECS) * 1_000_000_000 + 500_000_000,
        ..Default::default()
    }
}

/// Which exchange time the ticks carry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// Fixed at `HIT_TS`: no seal.
    Hit,
    /// Rising: each instrument about once every `ROLL_SECS_PER_INSTRUMENT`.
    Roll,
}

/// `tables` full aggregators and a deterministic walk over them.
struct ColdRun {
    tables: Vec<MultiTfAggregator>,
    shape: Shape,
    /// xorshift64 state for the walk.
    rng: u64,
    /// Ticks folded so far (drives volume and, for `Roll`, time).
    folded: u64,
    /// Ticks per exchange second in the roll shape.
    roll_rate: u64,
}

impl ColdRun {
    fn new(table_count: usize, shape: Shape) -> Self {
        let start = match shape {
            Shape::Hit => HIT_TS,
            Shape::Roll => ROLL_START,
        };
        let mut tables = Vec::with_capacity(table_count);
        for _ in 0..table_count {
            let mut agg =
                MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, AGGREGATOR_MAX_SLOTS);
            for sid in 1..=INSTRUMENTS {
                let stats = agg.consume_tick(
                    Feed::Dhan,
                    &tick(sid, start, 100.0, 10),
                    None,
                    |_, _, _, _, _| {},
                );
                assert!(!refused(&stats), "warm-up tick refused: {stats:?}");
            }
            assert_eq!(agg.len(), AGGREGATOR_MAX_SLOTS, "table not full");
            tables.push(agg);
        }
        let instruments_total = INSTRUMENTS * table_count as u64;
        Self {
            tables,
            shape,
            rng: 0x9E37_79B9_7F4A_7C15,
            folded: 0,
            roll_rate: (instruments_total / ROLL_SECS_PER_INSTRUMENT).max(1),
        }
    }

    /// The next tick and the table it goes to.
    fn next(&mut self) -> (usize, ParsedTick) {
        // xorshift64: full period over non-zero states, no allocation.
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        let table = (x % self.tables.len() as u64) as usize;
        let sid = (x >> 20) % INSTRUMENTS + 1;
        self.folded += 1;
        let secs = match self.shape {
            Shape::Hit => HIT_TS,
            Shape::Roll => {
                ROLL_START + 1 + u32::try_from(self.folded / self.roll_rate).unwrap_or(0)
            }
        };
        let price = 100.0 + (self.folded % 37) as f32 * 0.05;
        // Rising globally, so rising per instrument: every tick is a new trade.
        let volume = 11 + u32::try_from(self.folded).unwrap_or(u32::MAX - 11);
        (table, tick(sid, secs, price, volume))
    }

    /// Folds one tick, untimed bookkeeping outside.
    fn fold_one(&mut self) -> ConsumeStats {
        let (table, t) = self.next();
        self.tables[table].consume_tick(Feed::Dhan, &t, None, |_, _, _, _, bar| {
            black_box(bar);
        })
    }
}

fn bench_cold(c: &mut Criterion) {
    let mut group = c.benchmark_group("candles_cold");
    group.throughput(Throughput::Elements(1));
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(5));
    for (name, shape) in [
        ("consume_tick_hit_25k", Shape::Hit),
        ("consume_tick_roll_25k", Shape::Roll),
    ] {
        let mut run = ColdRun::new(COLD_TABLES, shape);
        // Untimed: settle into the steady state before Criterion samples, and
        // prove the run is the shape its name says before timing it.
        let mut settle = ShapeCount::default();
        for _ in 0..REPORT_WARMUP_TICKS {
            settle.note(&run.fold_one());
        }
        if let Err(why) = settle.check(shape) {
            panic!("candles_cold/{name}: untimed pass is the wrong shape: {why}");
        }
        // The timed ticks are checked too: counting happens after `elapsed`.
        let mut timed_count = ShapeCount::default();
        group.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut timed = Duration::ZERO;
                for _ in 0..iters {
                    let (table, t) = run.next();
                    let agg = &mut run.tables[table];
                    let start = Instant::now();
                    let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, bar| {
                        black_box(bar);
                    });
                    timed += start.elapsed();
                    timed_count.note(&stats);
                }
                timed
            });
        });
        if let Err(why) = timed_count.check(shape) {
            panic!("candles_cold/{name}: timed ticks were the wrong shape: {why}");
        }
    }
    group.finish();
}

/// `sorted[ceil(len × num / den) − 1]`, the nearest-rank quantile.
fn quantile_of_sorted(sorted: &[u64], num: usize, den: usize) -> u64 {
    if sorted.is_empty() || den == 0 {
        return 0;
    }
    let rank = sorted
        .len()
        .saturating_mul(num)
        .div_ceil(den)
        .clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Times `REPORT_TICKS` ticks one by one and prints the percentiles.
fn report(label: &str, table_count: usize, shape: Shape) {
    let mut run = ColdRun::new(table_count, shape);
    for _ in 0..REPORT_WARMUP_TICKS {
        black_box(run.fold_one());
    }
    let mut samples: Vec<u64> = Vec::with_capacity(REPORT_TICKS);
    let mut count = ShapeCount::default();
    for _ in 0..REPORT_TICKS {
        let (table, t) = run.next();
        let agg = &mut run.tables[table];
        let start = Instant::now();
        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, bar| {
            black_box(bar);
        });
        samples.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
        count.note(&stats);
    }
    if let Err(why) = count.check(shape) {
        panic!("{label}: the timed report pass is the wrong shape: {why}");
    }
    samples.sort_unstable();
    let (seals, refusals) = (count.seals, count.refusals);
    println!(
        "{label}: {table_count} table(s) x {INSTRUMENTS} slots, {REPORT_TICKS} ticks, \
         {seals} seals, {refusals} refused: p50 = {} ns, p90 = {} ns, p99 = {} ns, \
         p99.9 = {} ns, max = {} ns",
        quantile_of_sorted(&samples, 1, 2),
        quantile_of_sorted(&samples, 9, 10),
        quantile_of_sorted(&samples, 99, 100),
        quantile_of_sorted(&samples, 999, 1_000),
        samples.last().copied().unwrap_or(0),
    );
}

/// The cost of the `Instant` pair every timed tick carries: an empty pair
/// timed `REPORT_TICKS` times, p50 and p99 printed beside the report.
fn report_instant_pair() {
    let mut samples: Vec<u64> = Vec::with_capacity(REPORT_TICKS);
    for _ in 0..REPORT_TICKS {
        let start = Instant::now();
        samples.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    samples.sort_unstable();
    println!(
        "empty Instant pair ({REPORT_TICKS} samples): p50 = {} ns, p99 = {} ns",
        quantile_of_sorted(&samples, 1, 2),
        quantile_of_sorted(&samples, 99, 100),
    );
}

criterion_group!(benches, bench_cold);

fn main() {
    shape_check_self_test();
    // `CANDLE_COLD_REPORT_ONLY=1` skips Criterion and prints only the report.
    let report_only = std::env::var("CANDLE_COLD_REPORT_ONLY").is_ok_and(|v| v == "1");
    if !report_only {
        benches();
        Criterion::default().configure_from_args().final_summary();
    }
    if report_only || std::env::args().any(|a| a == "--bench") {
        report_instant_pair();
        report("cold hit", COLD_TABLES, Shape::Hit);
        report("cold roll", COLD_TABLES, Shape::Roll);
        report("warm hit", 1, Shape::Hit);
        report("warm roll", 1, Shape::Roll);
    }
}
