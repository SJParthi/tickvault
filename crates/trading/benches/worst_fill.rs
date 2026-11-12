//! Benchmark: the brutex host's order-side helpers.
//!
//! * `worst_fill/buy_5s_depth`: one buy fill over five fill seconds against a
//!   four-level ask book, deep enough to exhaust it.
//! * `order_price/marketable_limit`: one buy limit with a slot cap and circuit.
//! * `send_queue/push_pop_64`: one push and one pop on a queue held at 63 of
//!   its 64 slots (the deepest sift).
//!
//! After Criterion, under `cargo bench` only, each function is timed one call
//! at a time for 1,000,000 calls (one `Instant` pair each, so the figures read
//! slightly high) and p50 / p99 / p99.9 / max are printed. Not a gate: no
//! budget key exists in `quality/benchmark-budgets.toml` for these yet.
//!
//! Run: `cargo bench -p tickvault-trading --bench worst_fill`

use std::hint::black_box;
use std::time::Instant;

use criterion::{Criterion, criterion_group};

use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::brutex_host::order_price::{DEFAULT_BAND_BPS, marketable_limit};
use tickvault_trading::brutex_host::send_queue::SendQueue;
use tickvault_trading::brutex_host::worst_fill::{
    BookAtDecision, FillParams, FillSchedule, SecondBar, worst_fill,
};

const DECISION: i64 = 33_305;
const SECONDS: [SecondBar; 6] = [
    SecondBar {
        sec: DECISION + 1,
        high_paise: 10_000,
        low_paise: 9_995,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 2,
        high_paise: 10_005,
        low_paise: 9_995,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 3,
        high_paise: 10_000,
        low_paise: 9_990,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 4,
        high_paise: 10_020,
        low_paise: 9_980,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 5,
        high_paise: 10_000,
        low_paise: 9_995,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 6,
        high_paise: 10_010,
        low_paise: 9_990,
        volume: 400,
    },
];
const DECISION_BAR: SecondBar = SecondBar {
    sec: DECISION,
    high_paise: 10_010,
    low_paise: 9_990,
    volume: 1_000,
};
const LEVELS: [(i64, u64); 4] = [(10_020, 60), (10_040, 60), (10_060, 60), (10_080, 60)];
const BOOK: BookAtDecision<'static> = BookAtDecision {
    best_paise: 10_000,
    best_qty: 60,
    levels: &LEVELS,
};
const MANUAL_CALLS: usize = 1_000_000;

fn fill_once(out: &mut FillSchedule) -> u64 {
    let params = FillParams::new(Side::Buy, 450, 5, DECISION, 5);
    match worst_fill(&params, &DECISION_BAR, &SECONDS, Some(&BOOK), out) {
        Ok(s) => s.filled_qty,
        Err(_) => 0,
    }
}

fn limit_once(ltp: i64) -> i64 {
    marketable_limit(
        Side::Buy,
        ltp,
        DEFAULT_BAND_BPS,
        5,
        50,
        Some(600_000),
        Some((9_000, 12_000)),
    )
    .map_or(0, |l| l.price_paise)
}

fn filled_queue() -> SendQueue<u64, 64> {
    let mut q = SendQueue::new();
    for i in 0..63u32 {
        let _ = q.push(u64::from(i), i % 9, 0);
    }
    q
}

fn push_pop(q: &mut SendQueue<u64, 64>, i: u32) -> u64 {
    let _ = q.push(u64::from(i), i % 9, i64::from(i));
    q.pop(i64::from(i)).map_or(0, |(v, w)| v.wrapping_add(w))
}

fn bench_all(c: &mut Criterion) {
    let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(5, LEVELS.len()));
    assert_eq!(
        fill_once(&mut out),
        450,
        "bench fill must fill (and exhaust the book)"
    );
    c.benchmark_group("worst_fill")
        .bench_function("buy_5s_depth", |b| {
            b.iter(|| black_box(fill_once(&mut out)))
        });

    let mut ltp = 10_000i64;
    assert!(limit_once(ltp) > 0, "bench limit must not be refused");
    c.benchmark_group("order_price")
        .bench_function("marketable_limit", |b| {
            b.iter(|| {
                ltp = 10_000 + (ltp + 7) % 97;
                black_box(limit_once(black_box(ltp)))
            })
        });

    let mut q = filled_queue();
    let mut i = 0u32;
    c.benchmark_group("send_queue")
        .bench_function("push_pop_64", |b| {
            b.iter(|| {
                i = i.wrapping_add(1);
                black_box(push_pop(&mut q, i))
            })
        });
}

fn report(name: &str, samples: &mut [u64]) {
    samples.sort_unstable();
    let at = |permille: usize| samples[(samples.len() - 1) * permille / 1_000];
    println!(
        "{name}: p50 {} ns, p99 {} ns, p99.9 {} ns, max {} ns ({} calls)",
        at(500),
        at(990),
        at(999),
        samples.last().copied().unwrap_or(0),
        samples.len()
    );
}

fn manual_loops() {
    let mut samples = vec![0u64; MANUAL_CALLS];
    let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(5, LEVELS.len()));
    for s in samples.iter_mut() {
        let t = Instant::now();
        black_box(fill_once(&mut out));
        *s = t.elapsed().as_nanos() as u64;
    }
    report("worst_fill/buy_5s_depth", &mut samples);

    for (n, s) in samples.iter_mut().enumerate() {
        let ltp = 10_000 + (n as i64 % 97);
        let t = Instant::now();
        black_box(limit_once(black_box(ltp)));
        *s = t.elapsed().as_nanos() as u64;
    }
    report("order_price/marketable_limit", &mut samples);

    let mut q = filled_queue();
    for (n, s) in samples.iter_mut().enumerate() {
        let t = Instant::now();
        black_box(push_pop(&mut q, n as u32));
        *s = t.elapsed().as_nanos() as u64;
    }
    report("send_queue/push_pop_64", &mut samples);
}

criterion_group!(benches, bench_all);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    if std::env::args().any(|a| a == "--bench") {
        manual_loops();
    }
}
