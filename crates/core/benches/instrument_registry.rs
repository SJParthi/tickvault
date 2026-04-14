//! Benchmark: InstrumentRegistry O(1) lookup and subscription builder.
//!
//! Measures the hot-path operations for instrument resolution and
//! WebSocket subscription message generation.
//! Budget: registry.get() < 50ns, build_subscription_messages() < 1ms/batch.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::instrument_registry::{InstrumentRegistry, make_display_index_instrument};
use tickvault_common::types::{ExchangeSegment, FeedMode};
use tickvault_core::websocket::subscription_builder::build_subscription_messages;
use tickvault_core::websocket::types::InstrumentSubscription;

/// Build a registry with N instruments for benchmarking.
fn build_registry(count: usize) -> InstrumentRegistry {
    let mut instruments = Vec::with_capacity(count);
    // Fill with display indices (simplest constructor, no FnoUnderlying needed)
    for i in 0..count {
        let security_id = 10000_u32.saturating_add(i as u32);
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

fn bench_subscription_messages_100(c: &mut Criterion) {
    let instruments: Vec<InstrumentSubscription> = (0..100)
        .map(|i: u32| {
            InstrumentSubscription::new(ExchangeSegment::NseFno, 10000_u32.saturating_add(i))
        })
        .collect();
    c.bench_function("subscription/build_messages_100", |b| {
        b.iter(|| {
            build_subscription_messages(
                black_box(&instruments),
                black_box(FeedMode::Ticker),
                black_box(100),
            )
        });
    });
}

fn bench_subscription_messages_5000(c: &mut Criterion) {
    let instruments: Vec<InstrumentSubscription> = (0..5000)
        .map(|i: u32| {
            InstrumentSubscription::new(ExchangeSegment::NseFno, 10000_u32.saturating_add(i))
        })
        .collect();
    c.bench_function("subscription/build_messages_5000", |b| {
        b.iter(|| {
            build_subscription_messages(
                black_box(&instruments),
                black_box(FeedMode::Ticker),
                black_box(100),
            )
        });
    });
}

criterion_group!(
    benches,
    bench_registry_get_hit,
    bench_registry_get_miss,
    bench_registry_contains,
    bench_subscription_messages_100,
    bench_subscription_messages_5000,
);
criterion_main!(benches);
