//! Criterion benchmarks for the tick-gap detector's per-tick call (plan ITEM
//! DT, side stores, 2026-10-30).
//!
//! `TickGapDetector::observe` runs on the frame drain for every main-feed tick
//! (`LiveIngest`, before the candle fold), and `observe_tracked` replaces it
//! during a WAL replay. Every case runs against a detector FULL at the
//! production ceiling (25,000 instruments, the `with_detector_capacity`
//! sizing), so the index probe is measured at its worst load factor.
//!
//! Targets (budget key `tick_gap_detector` in
//! `quality/benchmark-budgets.toml`, one call per iteration):
//!   - `tick_gap_detector/observe_hit`: a tracked instrument, keys walked
//!     across all 25,000 so the slot is not always in cache.
//!   - `tick_gap_detector/observe_refused_full`: an untracked instrument
//!     against the full table (the fail-closed refusal path).
//!   - `tick_gap_detector/observe_tracked_hit` and
//!     `tick_gap_detector/observe_tracked_miss`: the replay path.
//!
//! # The p99 gate
//!
//! Criterion reports the median. After it, `observe_hit` and
//! `observe_refused_full` are timed one call at a time over 200,000 calls
//! with one `Instant` pair per call (the pair is inside every sample, so the
//! figures read high), and a p99 above the `tick_gap_detector` budget (times
//! `BENCH_BUDGET_MULTIPLIER`) exits 1 (under `BENCH_TAIL_GATE_DEFER=1`, set by
//! `bench.yml`, it prints `BENCH-TAIL-GATE-FAILED` and a later step fails the
//! job after the baseline save). A synthetic
//! over-budget sample set is checked to fail first, so the gate can fail.
//! Only under `cargo bench` (`--bench`).

#![allow(clippy::unwrap_used)] // APPROVED: bench harness
#![allow(clippy::expect_used)] // APPROVED: bench harness
#![allow(missing_docs)] // APPROVED: criterion_group! macro

use std::hint::black_box;

#[path = "support/tail_gate.rs"]
mod tail_gate;

use criterion::{Criterion, criterion_group};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::pipeline::tick_gap_detector::{
    DetectorConfig, InstrumentKey, TickGapDetector, TickObservation,
};

/// The production ceiling (`MAX_TRACKED_INSTRUMENTS` of the sibling stores).
const CAPACITY: usize = 25_000;
/// Budget key in `quality/benchmark-budgets.toml`.
const BUDGET_KEY: &str = "tick_gap_detector";
/// A plausible IST-epoch trade time.
const LTT: u32 = 1_761_800_000;

fn key(i: usize) -> InstrumentKey {
    (40_000 + i as u64, ExchangeSegment::NseFno)
}

fn full_detector() -> (TickGapDetector, Vec<InstrumentKey>) {
    let mut detector = TickGapDetector::with_capacity(CAPACITY, DetectorConfig::default());
    let keys: Vec<InstrumentKey> = (0..CAPACITY).map(key).collect();
    for (i, k) in keys.iter().enumerate() {
        assert!(detector.seed(*k, i as u64));
    }
    assert_eq!(detector.tracked_instruments(), CAPACITY);
    (detector, keys)
}

fn observation(key: InstrumentKey, n: u64) -> TickObservation {
    TickObservation {
        key,
        ltt_epoch_secs: LTT.wrapping_add((n / 4) as u32),
        volume: 1_000u32.wrapping_add(n as u32),
        open_interest: 77,
        recv_monotonic_millis: 1_000 + n,
    }
}

const UNTRACKED: InstrumentKey = (9_999_999, ExchangeSegment::NseFno);

fn bench_tick_gap_detector(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_gap_detector");

    let (mut detector, keys) = full_detector();
    let mut n = 0u64;
    group.bench_function("observe_hit", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            let k = keys[(n as usize) % CAPACITY];
            black_box(detector.observe(black_box(observation(k, n))))
        });
    });

    group.bench_function("observe_refused_full", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(detector.observe(black_box(observation(UNTRACKED, n))))
        });
    });

    group.bench_function("observe_tracked_hit", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            let k = keys[(n as usize) % CAPACITY];
            black_box(detector.observe_tracked(black_box(observation(k, n))))
        });
    });

    group.bench_function("observe_tracked_miss", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(detector.observe_tracked(black_box(observation(UNTRACKED, n))))
        });
    });

    group.finish();
}

fn tail_gate() -> Result<(), String> {
    let mut gate = tail_gate::Gate::new("tick_gap_detector", BUDGET_KEY)?;
    let (mut detector, keys) = full_detector();
    gate.case("observe_hit", |n| {
        let k = keys[(n as usize) % CAPACITY];
        !black_box(detector.observe(black_box(observation(k, n)))).refused
    });
    gate.case("observe_refused_full", |n| {
        black_box(detector.observe(black_box(observation(UNTRACKED, n)))).refused
    });
    gate.finish()
}

criterion_group!(benches, bench_tick_gap_detector);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    tail_gate::run_under_bench("tick_gap_detector", tail_gate);
}
