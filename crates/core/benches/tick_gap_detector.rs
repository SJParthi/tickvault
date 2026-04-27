//! Wave 2 Item 8.5 — Criterion benchmark for `TickGapDetector::record_tick`.
//!
//! Hot-path budget: ≤ 100 ns per call (tracked in
//! `quality/benchmark-budgets.toml` as `tick_gap_record_tick`). The
//! 5%-regression gate (`scripts/bench-gate.sh`) blocks any PR whose
//! median latency exceeds the budget by more than 5%.
//!
//! Run: `cargo bench --bench tick_gap_detector -p tickvault-core`

use std::hint::black_box;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::types::ExchangeSegment;
use tickvault_core::pipeline::TickGapDetector;

/// Pre-seed the detector with a realistic universe (~5,000 entries
/// across two segments) and benchmark a single steady-state
/// `record_tick` call.
fn bench_record_tick_steady_state(c: &mut Criterion) {
    let detector = TickGapDetector::new(30);
    let now = Instant::now();
    for id in 0..2_500_u32 {
        detector.record_tick(id, ExchangeSegment::IdxI, now);
        detector.record_tick(id, ExchangeSegment::NseFno, now);
    }
    c.bench_function("tick_gap/record_tick_steady_state", |b| {
        let mut sid: u32 = 0;
        b.iter(|| {
            sid = sid.wrapping_add(1) % 2_500;
            detector.record_tick(
                black_box(sid),
                black_box(ExchangeSegment::NseFno),
                black_box(now),
            );
        });
    });
}

/// Benchmark scan_gaps over a populated map. Cold path — runs once per
/// 60s coalesce window — but operator-visible latency matters when the
/// map is at its full ~24K-entry steady state.
fn bench_scan_gaps_full_universe(c: &mut Criterion) {
    let detector = TickGapDetector::new(30);
    let now = Instant::now();
    // Pre-seed with ~24K entries to mimic the real F&O universe.
    for id in 0..12_000_u32 {
        detector.record_tick(id, ExchangeSegment::IdxI, now);
        detector.record_tick(id, ExchangeSegment::NseFno, now);
    }
    c.bench_function("tick_gap/scan_gaps_full_universe", |b| {
        b.iter(|| {
            let gaps = detector.scan_gaps(black_box(now));
            black_box(gaps);
        });
    });
}

criterion_group!(
    benches,
    bench_record_tick_steady_state,
    bench_scan_gaps_full_universe
);
criterion_main!(benches);
