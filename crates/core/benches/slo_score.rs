//! Wave 3-D Item 13 — Criterion benchmark for the composite SLO score
//! evaluator (`evaluate_slo_score`).
//!
//! Hot-path budget: ≤ 1 µs per call (tracked in
//! `quality/benchmark-budgets.toml` as `slo_score_evaluate`). In
//! practice the evaluator is six clamps + five multiplies + a 6-way
//! min-scan, so p99 lands ≤ 50 ns on c7i.xlarge — the 1 µs budget is
//! the SCOPE-mandated ceiling, not a tight bound.
//!
//! Run: `cargo bench --bench slo_score -p tickvault-core`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_core::instrument::slo_score::{SloInputs, evaluate_slo_score};

/// All-green inputs — the steady-state code path. The score is `1.0`,
/// the classifier returns `Healthy`, and no allocation occurs.
fn bench_score_compute_le_1us(c: &mut Criterion) {
    let inputs = SloInputs {
        ws_health: 1.0,
        qdb_health: 1.0,
        tick_freshness: 1.0,
        token_freshness: 1.0,
        spill_health: 1.0,
        phase2_health: 1.0,
    };
    c.bench_function("slo_score/evaluate_all_green", |b| {
        b.iter(|| {
            let outcome = evaluate_slo_score(black_box(&inputs));
            black_box(outcome);
        });
    });
}

/// Degraded path — exercises the find-weakest scan plus the threshold
/// branches that all-green skips. Same budget; different code path.
fn bench_score_compute_degraded(c: &mut Criterion) {
    let inputs = SloInputs {
        ws_health: 0.85,
        qdb_health: 1.0,
        tick_freshness: 1.0,
        token_freshness: 1.0,
        spill_health: 1.0,
        phase2_health: 1.0,
    };
    c.bench_function("slo_score/evaluate_degraded", |b| {
        b.iter(|| {
            let outcome = evaluate_slo_score(black_box(&inputs));
            black_box(outcome);
        });
    });
}

criterion_group!(
    benches,
    bench_score_compute_le_1us,
    bench_score_compute_degraded
);
criterion_main!(benches);
