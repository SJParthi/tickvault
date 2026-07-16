//! Benchmark: InstrumentRegistry O(1) lookup.
//!
//! Measures the hot-path operations for instrument resolution.
//! Budget: registry.get() < 50ns (`quality/benchmark-budgets.toml` registry_get).
//!
//! PR-C2 trim (2026-07-13): the two `subscription/build_messages_*` benches
//! retired with `subscription_builder.rs` — deleted with the Dhan live
//! main-feed WS lane (operator retirement directive —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment"). No budget
//! key referenced them.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::instrument_registry::{InstrumentRegistry, make_display_index_instrument};
use tickvault_common::types::FeedMode;

/// Build a registry with N instruments for benchmarking.
fn build_registry(count: usize) -> InstrumentRegistry {
    let mut instruments = Vec::with_capacity(count);
    // Fill with display indices (simplest constructor, no FnoUnderlying needed)
    for i in 0..count {
        let security_id = 10000_u64.saturating_add(i as u64);
        instruments.push(make_display_index_instrument(
            security_id,
            "BENCH_INDEX",
            FeedMode::Ticker,
        ));
    }
    InstrumentRegistry::from_instruments(instruments)
}

fn bench_registry_get_hit(c: &mut Criterion) {
    let registry = build_registry(5000);
    c.bench_function("registry/get_hit", |b| {
        b.iter(|| registry.get(black_box(10050)));
    });
}

fn bench_registry_get_miss(c: &mut Criterion) {
    let registry = build_registry(5000);
    c.bench_function("registry/get_miss", |b| {
        b.iter(|| registry.get(black_box(99999)));
    });
}

fn bench_registry_contains(c: &mut Criterion) {
    let registry = build_registry(5000);
    c.bench_function("registry/contains_hit", |b| {
        b.iter(|| registry.contains(black_box(10050)));
    });
}

criterion_group!(
    benches,
    bench_registry_get_hit,
    bench_registry_get_miss,
    bench_registry_contains,
);
criterion_main!(benches);
