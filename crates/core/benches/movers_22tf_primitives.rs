//! Phase 12-runtime — Criterion benchmarks for movers 22-tf primitives.
//!
//! Targets:
//! - `compute_swap_delta` — set diff for the depth-20 dynamic top-150
//!   selector. Budget: ≤ 50 µs for 150-entry sets (typical universe
//!   crossover < 30 entries; the 150-vs-150 case is worst case).
//! - `compute_next_tick_secs` / `compute_lag_secs` /
//!   `should_emit_snapshot` — the scheduler-loop primitives.
//!   Budget: ≤ 50 ns each (pure integer arithmetic).
//! - `parse_questdb_dataset_json` — JSON-to-HashSet for top-150.
//!   Budget: ≤ 1 ms for 150-row payload.
//!
//! Run: `cargo bench --bench movers_22tf_primitives -p tickvault-core`
//!
//! Budgets are listed in `quality/benchmark-budgets.toml` (run by
//! `scripts/bench-gate.sh` in CI; >5% regression fails the build).

use std::collections::HashSet;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_core::instrument::depth_20_dynamic_subscriber::{
    DynamicContractKey, compute_swap_delta, parse_questdb_dataset_json,
};
use tickvault_core::pipeline::movers_22tf_scheduler::{
    compute_lag_secs, compute_next_tick_secs, should_emit_snapshot,
};

/// Builds a HashSet of N consecutive `(security_id, 'D')` keys for
/// benchmark scenarios.
fn build_set(n: usize, offset: u32) -> HashSet<DynamicContractKey> {
    let mut s = HashSet::with_capacity(n);
    for i in 0..n {
        s.insert((offset + i as u32, 'D'));
    }
    s
}

/// 150-vs-150 with 30-entry crossover (typical worst case for the
/// per-minute swap delta).
fn bench_compute_swap_delta_150_with_30_entry_crossover(c: &mut Criterion) {
    let prev = build_set(150, 0); // 0..149
    let curr = build_set(150, 120); // 120..269 — 30 overlap (120..149)

    c.bench_function("compute_swap_delta_150_30_overlap", |b| {
        b.iter(|| {
            let delta = compute_swap_delta(black_box(&prev), black_box(&curr));
            black_box(delta);
        })
    });
}

/// 150-vs-150 with full 150 overlap (no-op swap — very common during
/// stable market periods).
fn bench_compute_swap_delta_full_overlap(c: &mut Criterion) {
    let prev = build_set(150, 0);
    let curr = build_set(150, 0);

    c.bench_function("compute_swap_delta_full_overlap", |b| {
        b.iter(|| {
            let delta = compute_swap_delta(black_box(&prev), black_box(&curr));
            black_box(delta);
        })
    });
}

/// 150-vs-150 with zero overlap (worst-case full swap).
fn bench_compute_swap_delta_zero_overlap(c: &mut Criterion) {
    let prev = build_set(150, 0);
    let curr = build_set(150, 1000);

    c.bench_function("compute_swap_delta_zero_overlap", |b| {
        b.iter(|| {
            let delta = compute_swap_delta(black_box(&prev), black_box(&curr));
            black_box(delta);
        })
    });
}

/// Pure-integer scheduler primitives — should easily land < 50 ns.
fn bench_scheduler_primitives_le_50ns(c: &mut Criterion) {
    c.bench_function("compute_next_tick_secs", |b| {
        b.iter(|| {
            let next = compute_next_tick_secs(black_box(60), black_box(12_345));
            black_box(next);
        })
    });

    c.bench_function("compute_lag_secs", |b| {
        b.iter(|| {
            let lag = compute_lag_secs(black_box(12_345), black_box(12_350));
            black_box(lag);
        })
    });

    c.bench_function("should_emit_snapshot", |b| {
        b.iter(|| {
            let emit = should_emit_snapshot(black_box(60), black_box(12_400), black_box(12_300));
            black_box(emit);
        })
    });
}

/// JSON parsing for the QuestDB response — 150-row payload mirrors the
/// production worst case.
fn bench_parse_questdb_dataset_150_rows(c: &mut Criterion) {
    let mut body = String::from("{\"dataset\":[");
    for i in 0..150 {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!("[{},\"D\"]", i + 1));
    }
    body.push_str("]}");

    c.bench_function("parse_questdb_dataset_json_150_rows", |b| {
        b.iter(|| {
            let parsed = parse_questdb_dataset_json(black_box(&body));
            black_box(parsed);
        })
    });
}

criterion_group!(
    movers_22tf_primitives,
    bench_compute_swap_delta_150_with_30_entry_crossover,
    bench_compute_swap_delta_full_overlap,
    bench_compute_swap_delta_zero_overlap,
    bench_scheduler_primitives_le_50ns,
    bench_parse_questdb_dataset_150_rows
);
criterion_main!(movers_22tf_primitives);
