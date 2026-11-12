//! Benchmark: `LiveCandleBook::on_tick`, the per-tick strategy candle step.
//!
//! * `live_candles/on_tick_500x3`: 500 instruments × 3 frames (1 s, 5 s,
//!   60 s), time moving forward at 500 ticks a second of exchange time, so
//!   each instrument trades once a second: almost every tick closes its 1 s
//!   bar by tick and opens the next; every fifth second the 5 s bars roll.
//!   One clock reading every 500 ticks, at the start of the feed's current
//!   second (outside the timed call in the manual loop, inside the Criterion
//!   batch, which is 500 ticks plus one clock). The ticks close the bars, so
//!   the clock finds nothing due, BUT it still scans: a tick close never
//!   raises a frame's due bound, so every reading passes the 1 s frame's
//!   bound and walks all 500 open 1 s bars (and the 5 s frame's every fifth
//!   second). That O(open bars) scan is in the Criterion figure, not in the
//!   manual per-tick percentiles.
//! * After Criterion, under `cargo bench` only, 1,000,000 `on_tick` calls are
//!   timed one by one with `Instant` and p50 / p99 / p99.9 / max printed (each
//!   figure includes one `Instant` pair, so it reads slightly high). No budget
//!   key exists for this bench; nothing is gated.
//!
//! Run: `cargo bench -p tickvault-trading --bench live_candles`

use std::hint::black_box;
use std::time::Instant;

use criterion::{Criterion, Throughput, criterion_group};

use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::live_candles::{ClosedBars, LiveCandleBook};

/// IST midnight in IST epoch seconds (multiple of 86,400).
const DAY: u32 = 1_779_321_600;
/// 09:15:00 IST.
const OPEN: u32 = DAY + 33_300;
const NS: i64 = 1_000_000_000;
const KEYS: u32 = 500;
const FRAMES: [u32; 3] = [1, 5, 60];
/// Timed calls in the manual percentile loop.
const MANUAL_CALLS: usize = 1_000_000;

fn key(id: u32) -> InstrumentKey {
    InstrumentKey {
        security_id: u64::from(id),
        exchange_segment: 2,
    }
}

struct Feed {
    book: LiveCandleBook,
    out: ClosedBars,
    i: u64,
    cum: Vec<u64>,
}

impl Feed {
    fn new() -> Self {
        let keys: Vec<InstrumentKey> = (1..=KEYS).map(key).collect();
        Self {
            book: LiveCandleBook::configure(&keys, &FRAMES).expect("valid"),
            out: ClosedBars::with_capacity(4 * KEYS as usize),
            i: 0,
            cum: vec![0; KEYS as usize + 1],
        }
    }

    /// The next tick's arguments; clock reading every `KEYS` ticks.
    #[inline]
    fn next(&mut self) -> (InstrumentKey, u32, i64, f64, u64, bool) {
        let i = self.i;
        self.i += 1;
        let id = (i % u64::from(KEYS)) as u32 + 1;
        let secs = OPEN + (i / u64::from(KEYS)) as u32;
        self.cum[id as usize] += i % 7;
        let px = 100.0 + (i % 50) as f64;
        (
            key(id),
            secs,
            i64::from(secs) * NS + 1_000,
            px,
            self.cum[id as usize],
            id == KEYS,
        )
    }
}

fn bench_on_tick(c: &mut Criterion) {
    let mut feed = Feed::new();
    let mut group = c.benchmark_group("live_candles");
    group.throughput(Throughput::Elements(u64::from(KEYS)));
    group.bench_function("on_tick_500x3", |b| {
        b.iter(|| {
            for _ in 0..KEYS {
                let (k, s, r, px, cum, last) = feed.next();
                black_box(feed.book.on_tick(k, s, r, px, cum, &mut feed.out));
                if last {
                    feed.book.on_clock(i64::from(s) * NS, &mut feed.out);
                }
            }
            feed.out.clear();
        });
    });
    group.finish();
}

fn manual_percentiles() {
    let mut feed = Feed::new();
    let mut samples: Vec<u64> = Vec::with_capacity(MANUAL_CALLS);
    for _ in 0..MANUAL_CALLS {
        let (k, s, r, px, cum, last) = feed.next();
        let t0 = Instant::now();
        let v = feed.book.on_tick(k, s, r, px, cum, &mut feed.out);
        let dt = t0.elapsed();
        black_box(v);
        samples.push(u64::try_from(dt.as_nanos()).unwrap_or(u64::MAX));
        if last {
            feed.book.on_clock(i64::from(s) * NS, &mut feed.out);
            feed.out.clear();
        }
    }
    samples.sort_unstable();
    let at = |num: usize, den: usize| samples[(samples.len() * num / den).min(samples.len() - 1)];
    let c = feed.book.counters();
    println!(
        "live_candles on_tick ({KEYS} keys x {} frames, {MANUAL_CALLS} calls): \
         p50 {} ns, p99 {} ns, p99.9 {} ns, max {} ns; closed by tick {}, by clock {}",
        FRAMES.len(),
        at(50, 100),
        at(99, 100),
        at(999, 1_000),
        samples.last().copied().unwrap_or(0),
        c.closed_by_tick,
        c.closed_by_clock,
    );
}

criterion_group!(benches, bench_on_tick);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    if std::env::args().any(|a| a == "--bench") {
        manual_percentiles();
    }
}
