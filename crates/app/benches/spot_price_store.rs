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
//!   - `spot_price_store/record_hit_stored`: a tracked instrument, a trade
//!     time at or after the held one and a changing price (the store path:
//!     one probe and one compare-and-swap). The time is held one second
//!     after the seed, never advanced, so no run can drift into another
//!     trading day (review 2026-10-30: a drifting time ended up timing the
//!     next-day refusal).
//!   - `spot_price_store/record_hit_same_word`: a tracked instrument, the same
//!     price and trade time as held (a repeated quote).
//!   - `spot_price_store/record_hit_older`: older than held (refused).
//!   - `spot_price_store/record_refused_full`: a new instrument at the cap.
//!   - `spot_price_store/record_stale_day`: yesterday's trade time (refused
//!     before the map).
//!   - `spot_price_store/record_hit_with_reader`: `record_hit_stored` while
//!     another thread reads the same keys (`latest_paise`) in a loop. This
//!     is a busy sibling core, not a contention test: production has one
//!     writer (the drain) and its readers run seconds apart.
//!
//! Each case has its own call counter, and each is checked after it runs
//! (one more call must give the case's outcome), so a case cannot quietly
//! time a different path.
//!
//! After Criterion, `record_hit_stored`, `record_hit_older` and
//! `record_refused_full` are timed one call at a time and a p99 over the
//! budget exits 1; every timed call must also give its expected outcome (`../../core/benches/support/tail_gate.rs`). Only under
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
/// One minute into IST day 20,756 (2026-10-30) as `ist_day_of` counts it:
/// the store's trading day.
const T0: u32 = 1_793_298_660;
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
    // The store is deliberately not pre-sized (see `SpotPriceStore::new`), so
    // filling it to the cap resizes the map, and papaya finishes a resize
    // incrementally: every operation that overlaps one also copies entries.
    // Production reaches its ~870 instruments in the first minutes after
    // boot and never resizes again, so the cases measure the steady state:
    // repeat the held word for every key until the copy is done.
    for _ in 0..4 {
        for i in 0..MAX_TRACKED_INSTRUMENTS {
            black_box(store.record(sid(i), SEG, 100.0, T0, receipt_nanos(T0)));
        }
    }
    store
}

/// The trade time every store-path call carries: after the seed, and equal
/// on every later call (an equal time is stored, later-or-equal wins).
const STORE_SECS: u32 = T0 + 1;

fn record_stored(store: &SpotPriceStore, n: u64) -> RecordOutcome {
    let secs = STORE_SECS;
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

fn record_same_word(store: &SpotPriceStore, n: u64) -> RecordOutcome {
    store.record(
        black_box(sid((n as usize) % MAX_TRACKED_INSTRUMENTS)),
        SEG,
        black_box(100.0),
        black_box(T0),
        receipt_nanos(T0),
    )
}

fn record_stale_day(store: &SpotPriceStore, n: u64) -> RecordOutcome {
    store.record(
        black_box(sid((n as usize) % MAX_TRACKED_INSTRUMENTS)),
        SEG,
        black_box(100.0),
        black_box(T0 - 86_400),
        receipt_nanos(T0),
    )
}

/// One more call after a case ran must give the case's outcome, so a case
/// that drifted onto another path fails the run instead of reporting it.
fn expect_outcome(case: &str, got: RecordOutcome, want: RecordOutcome) {
    assert_eq!(got, want, "spot_price_store/{case} timed the wrong path");
}

fn bench_spot_price_store(c: &mut Criterion) {
    let mut group = c.benchmark_group("spot_price_store");
    let store = full_store();

    let mut n = 0u64;
    group.bench_function("record_hit_stored", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_stored(&store, n))
        });
    });
    expect_outcome(
        "record_hit_stored",
        record_stored(&store, n + 1),
        RecordOutcome::Stored,
    );

    // Every key holds exactly (T0, 100.0); the case repeats it.
    let fixed = full_store();
    let mut n = 0u64;
    group.bench_function("record_hit_same_word", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_same_word(&fixed, n))
        });
    });
    expect_outcome(
        "record_hit_same_word",
        record_same_word(&fixed, n + 1),
        RecordOutcome::Stored,
    );

    let mut n = 0u64;
    group.bench_function("record_hit_older", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_older(&fixed, n))
        });
    });
    expect_outcome(
        "record_hit_older",
        record_older(&fixed, n + 1),
        RecordOutcome::OlderThanHeld,
    );

    let mut n = 0u64;
    group.bench_function("record_refused_full", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_new_at_cap(&store, n))
        });
    });
    expect_outcome(
        "record_refused_full",
        record_new_at_cap(&store, n + 1),
        RecordOutcome::RefusedAtCapacity,
    );

    let mut n = 0u64;
    group.bench_function("record_stale_day", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_stale_day(&store, n))
        });
    });
    expect_outcome(
        "record_stale_day",
        record_stale_day(&store, n + 1),
        RecordOutcome::StaleTradingDay,
    );

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
    let mut n = 0u64;
    group.bench_function("record_hit_with_reader", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(record_stored(&store, n))
        });
    });
    expect_outcome(
        "record_hit_with_reader",
        record_stored(&store, n + 1),
        RecordOutcome::Stored,
    );
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();

    group.finish();
}

fn tail_gate() -> Result<(), String> {
    let mut gate = tail_gate::Gate::new("spot_price_store", BUDGET_KEY)?;
    let store = full_store();
    gate.case("record_hit_stored", |n| {
        black_box(record_stored(&store, n)) == RecordOutcome::Stored
    });
    gate.case("record_hit_older", |n| {
        black_box(record_older(&store, n)) == RecordOutcome::OlderThanHeld
    });
    gate.case("record_refused_full", |n| {
        black_box(record_new_at_cap(&store, n)) == RecordOutcome::RefusedAtCapacity
    });
    gate.finish()
}

criterion_group!(benches, bench_spot_price_store);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    tail_gate::run_under_bench("spot_price_store", tail_gate);
}
