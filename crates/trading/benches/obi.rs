//! Benchmark: OBI (Order Book Imbalance) computation latency.
//!
//! Measures `compute_obi()` performance — must be O(1) with no allocation.
//! Budget: < 10μs per computation (similar to signal_processing budget).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::tick_types::DeepDepthLevel;
use tickvault_trading::indicator::obi::compute_obi;

/// Creates a realistic 20-level order book side.
fn make_levels(base_price: f64, step: f64, base_qty: u32) -> Vec<DeepDepthLevel> {
    (0..20)
        .map(|i| DeepDepthLevel {
            price: base_price + step * i as f64,
            quantity: base_qty.saturating_sub(i * (base_qty / 25)),
            orders: 20 - i,
        })
        .collect()
}

fn bench_obi_computation(c: &mut Criterion) {
    let bids = make_levels(24500.0, -0.5, 1000);
    let asks = make_levels(24500.5, 0.5, 1000);

    c.bench_function("obi_compute_20_levels", |b| {
        b.iter(|| {
            let snap = compute_obi(
                black_box(49081),
                black_box(2),
                black_box(&bids),
                black_box(&asks),
            );
            black_box(snap);
        });
    });
}

fn bench_obi_empty_book(c: &mut Criterion) {
    let empty: Vec<DeepDepthLevel> = Vec::new();

    c.bench_function("obi_compute_empty_book", |b| {
        b.iter(|| {
            let snap = compute_obi(
                black_box(49081),
                black_box(2),
                black_box(&empty),
                black_box(&empty),
            );
            black_box(snap);
        });
    });
}

fn bench_obi_wall_detection(c: &mut Criterion) {
    let mut bids = make_levels(24500.0, -0.5, 100);
    bids[5] = DeepDepthLevel {
        price: 24497.5,
        quantity: 50000,
        orders: 1,
    };
    let asks = make_levels(24500.5, 0.5, 1000);

    c.bench_function("obi_compute_with_wall", |b| {
        b.iter(|| {
            let snap = compute_obi(
                black_box(49081),
                black_box(2),
                black_box(&bids),
                black_box(&asks),
            );
            black_box(snap);
        });
    });
}

criterion_group!(
    benches,
    bench_obi_computation,
    bench_obi_empty_book,
    bench_obi_wall_detection,
);
criterion_main!(benches);
