//! Benchmark: f32→f64 precision-preserving conversion and ILP buffer building.
//!
//! STORAGE-GAP-02: f32_to_f64_clean must be zero-allocation and fast.
//! Budget: < 50ns per conversion (stack buffer, no heap).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use dhan_live_trader_storage::tick_persistence_testing::f32_to_f64_clean_pub;

fn bench_f32_to_f64_clean_typical_price(c: &mut Criterion) {
    c.bench_function("f32_to_f64_clean/typical_price", |b| {
        b.iter(|| {
            let v = black_box(21004.95_f32);
            black_box(f32_to_f64_clean_pub(v))
        });
    });
}

fn bench_f32_to_f64_clean_zero(c: &mut Criterion) {
    c.bench_function("f32_to_f64_clean/zero", |b| {
        b.iter(|| {
            let v = black_box(0.0_f32);
            black_box(f32_to_f64_clean_pub(v))
        });
    });
}

fn bench_f32_to_f64_clean_small_decimal(c: &mut Criterion) {
    c.bench_function("f32_to_f64_clean/small_decimal", |b| {
        b.iter(|| {
            let v = black_box(0.05_f32);
            black_box(f32_to_f64_clean_pub(v))
        });
    });
}

fn bench_f32_to_f64_clean_large_value(c: &mut Criterion) {
    c.bench_function("f32_to_f64_clean/large_value", |b| {
        b.iter(|| {
            let v = black_box(99999.99_f32);
            black_box(f32_to_f64_clean_pub(v))
        });
    });
}

criterion_group!(
    benches,
    bench_f32_to_f64_clean_typical_price,
    bench_f32_to_f64_clean_zero,
    bench_f32_to_f64_clean_small_decimal,
    bench_f32_to_f64_clean_large_value
);
criterion_main!(benches);
