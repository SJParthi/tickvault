//! Benchmark: InstrumentRegistry O(1) lookup.
//!
//! Measures the hot-path operations for instrument resolution.
//! Budget: < 50ns (`quality/benchmark-budgets.toml` registry_get).
//!
//! 2026-08-08: the measured methods moved from the id-only `registry.get()` /
//! `contains()` to `get_with_segment()` / `contains_with_segment()`. The old
//! id-only accessors were DELETED — they read a legacy single-segment index and
//! returned the wrong instrument on a cross-segment id reuse (I-P1-11), and had
//! zero production callers, so the 50ns budget was guarding a method nothing
//! executed. It now guards the composite lookup production actually performs.
//! The Criterion benchmark IDs below are deliberately UNCHANGED
//! (`registry/get_hit`, `registry/get_miss`, `registry/contains_hit`) so the
//! `registry_get` budget key in benchmark-budgets.toml keeps matching and the
//! historical Criterion baselines stay comparable.
//!
//! PR-C2 trim (2026-07-13): the two `subscription/build_messages_*` benches
//! retired with `subscription_builder.rs` — deleted with the Dhan live
//! main-feed WS lane (operator retirement directive —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment"). No budget
//! key referenced them.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::instrument_registry::{InstrumentRegistry, make_display_index_instrument};
use tickvault_common::types::{ExchangeSegment, FeedMode};

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
        b.iter(|| registry.get_with_segment(black_box(10050), ExchangeSegment::IdxI));
    });
}

fn bench_registry_get_miss(c: &mut Criterion) {
    let registry = build_registry(5000);
    c.bench_function("registry/get_miss", |b| {
        b.iter(|| registry.get_with_segment(black_box(99999), ExchangeSegment::IdxI));
    });
}

fn bench_registry_contains(c: &mut Criterion) {
    let registry = build_registry(5000);
    c.bench_function("registry/contains_hit", |b| {
        b.iter(|| registry.contains_with_segment(black_box(10050), ExchangeSegment::IdxI));
    });
}

criterion_group!(
    benches,
    bench_registry_get_hit,
    bench_registry_get_miss,
    bench_registry_contains,
);
criterion_main!(benches);
