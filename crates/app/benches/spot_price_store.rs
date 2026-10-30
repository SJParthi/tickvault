//! Criterion benchmarks for the spot price store's per-tick call (plan ITEM
//! DT, side stores, 2026-10-30).
//!
//! `SpotPriceStore::record` runs on the frame drain for every index and
//! NSE_EQ spot tick (`LiveIngest::record_spot_price`); the contract and depth
//! attach tasks read the same store from their own tasks. Every case runs
//! against a store FULL at its cap (`MAX_TRACKED_INSTRUMENTS` = 25,000).
//!
//! Targets (budget key `spot_price_store` in
//! `quality/benchmark-budgets.toml`, one call per iteration):
//!   - `spot_price_store/record_hit_newer`: a tracked instrument, later trade
//!     time (the store path: one probe and one compare-and-swap).
//!   - `spot_price_store/record_hit_same_word`: a tracked instrument, the same
//!     price and trade time as held (a repeated quote).
//!   - `spot_price_store/record_hit_older`: older than held (refused).
//!   - `spot_price_store/record_refused_full`: a new instrument at the cap.
//!   - `spot_price_store/record_stale_day`: yesterday's trade time (refused
//!     before the map).
//!   - `spot_price_store/record_hit_with_reader`: `record_hit_newer` while
//!     another thread reads the same keys (`latest_paise`) in a loop.
//!
//! After Criterion, `record_hit_newer`, `record_hit_older` and
//! `record_refused_full` are timed one call at a time and a p99 over the
//! budget exits 1 (`../../core/benches/support/tail_gate.rs`). Only under
//! `cargo bench` (`--bench`).

#![allow(clippy::unwrap_used)] // APPROVED: bench harness
#![allow(clippy::expect_used)] // APPROVED: bench harness
#![allow(missing_docs)] // APPROVED: criterion_group! macro

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[path = "../../core/benches/support/tail_gate.rs"]
mod tail_gate;

use criterion::{Criterion, criterion_group};
use tickvault_app::spot_price_store::{
    MAX_TRACKED_INSTRUMENTS, RecordOutcome, SpotPriceStore, ist_day_of,
};
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64;
use tickvault_common::types::ExchangeSegment;

/// Budget key in `quality/benchmark-budgets.toml`.
const BUDGET_KEY: &str = "spot_price_store";
/// 2026-10-30 10:00 IST, IST-epoch seconds. Trade times run forward from it.
const T0: u32 = 1_761_818_400;
const SEG: ExchangeSegment = ExchangeSegment::NseEquity;

fn sid(i: usize) -> u64 {
    10_000 + i as u64
}

/// The frame's receipt (UTC nanoseconds), a little after the trade time, so
/// the future-time cap never bites in the steady state.
fn receipt_nanos(exchange_secs: u32) -> i64 {
    (i64::from(exchange_secs) - IST_UTC_OFFSET_SECONDS_I64 + 1) * 1_000_000_000
}

fn full_store() -> Arc<SpotPriceStore> {
    let store = Arc::new(SpotPriceStore::for_trading_day(ist_day_of(T0)));
    for i in 0..MAX_TRACKED_INSTRUMENTS {
        let outcome = store.record(sid(i), SEG, 100.0, T0, receipt_nanos(T0));
        assert_eq!(outcome, RecordOutcome::Stored);
    }
    assert_eq!(store.tracked(), MAX_TRACKED_INSTRUMENTS);
    store
}

/// A newer-than-held trade time for call `n` (one second per full pass).
fn newer_secs(n: u64) -> u32 {
    T0 + 1 + (n / MAX_TRACKED_INSTRUMENTS as u64) as u32
}

fn record_newer(store: &SpotPriceStore, n: u64) -> RecordOutcome {
    let secs = newer_secs(n);
    let price = 100.0 + (n % 64) as f32 * 0.05;
    store.record(
        black_box(sid((n as usize) % MAX_TRACKED_INSTRUMENTS)),
        SEG,
        black_box(price),
        black_box(secs),
        receipt_nanos(secs),
    )
}

fn record_older(store: &SpotPriceStore, n: u64) -> RecordOutcome {
    store.record(
        black_box(sid((n as usize) % MAX_TRACKED_INSTRUMENTS)),
        SEG,
        black_box(99.0),
        black_box(T0 - 1),
        receipt_nanos(T0),
    )
}

fn record_new_at_cap(store: &SpotPriceStore, n: u64) -> RecordOutcome {
    store.record(
        black_box(sid(MAX_TRACKED_INSTRUMENTS + 1 + (n % 1_000) as usize)),
        SEG,
        black_box(100.0),
        black_box(T0),
        receipt_nanos(T0),
    )
}

fn bench_spot_price_store(c: &mut Criterion) {
    let mut group = c.benchmark_group("spot_price_store");
    let store = full_store();
    let mut n = 0u64;

    group.bench_function("record_hit_newer", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_newer(&store, n))
        });
    });

    // Pin every key's word, then repeat it exactly.
    let fixed = full_store();
    group.bench_function("record_hit_same_word", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(fixed.record(
                black_box(sid((n as usize) % MAX_TRACKED_INSTRUMENTS)),
                SEG,
                black_box(100.0),
                black_box(T0),
                receipt_nanos(T0),
            ))
        });
    });

    group.bench_function("record_hit_older", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_older(&fixed, n))
        });
    });

    group.bench_function("record_refused_full", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_new_at_cap(&store, n))
        });
    });

    group.bench_function("record_stale_day", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(store.record(
                black_box(sid((n as usize) % MAX_TRACKED_INSTRUMENTS)),
                SEG,
                black_box(100.0),
                black_box(T0 - 86_400),
                receipt_nanos(T0),
            ))
        });
    });

    // A concurrent reader, as the attach tasks are.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (store, stop) = (Arc::clone(&store), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                black_box(store.latest_paise(sid(i % MAX_TRACKED_INSTRUMENTS), SEG));
                i = i.wrapping_add(7);
            }
        })
    };
    group.bench_function("record_hit_with_reader", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_newer(&store, n))
        });
    });
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();

    group.finish();
}

fn tail_gate() -> Result<(), String> {
    let mut gate = tail_gate::Gate::new("spot_price_store", BUDGET_KEY)?;
    let store = full_store();
    gate.case("record_hit_newer", |n| {
        black_box(record_newer(&store, n));
    });
    gate.case("record_hit_older", |n| {
        black_box(record_older(&store, n));
    });
    gate.case("record_refused_full", |n| {
        black_box(record_new_at_cap(&store, n));
    });
    gate.finish()
}

criterion_group!(benches, bench_spot_price_store);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    tail_gate::run_under_bench("spot_price_store", tail_gate);
}
