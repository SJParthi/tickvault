//! Criterion benchmark for the tick conservation ledger's per-row call (plan
//! ITEM DT, side stores, 2026-10-30).
//!
//! `TickConservationLedger::note_row_appended` runs on the frame drain for
//! every `ticks` / `tick_quotes` row the Dhan writer appends (inside
//! `TickWriter::append_tick_with_seq`).
//!
//! Targets (budget key `tick_conservation_ledger` in
//! `quality/benchmark-budgets.toml`, one call per iteration):
//!   - `tick_conservation_ledger/note_row_appended_same_day`: the steady
//!     state, every row on the slot's day.
//!   - `tick_conservation_ledger/note_row_appended_day_rollover`: every call
//!     lands on a different day than its slot holds, so each pays the reset
//!     (the worst case; production pays it once per day per slot).
//!
//! After Criterion, the same two cases are timed one call at a time and a
//! p99 over the budget exits 1 (`../../core/benches/support/tail_gate.rs`).
//! Only under `cargo bench` (`--bench`).

#![allow(clippy::unwrap_used)] // APPROVED: bench harness
#![allow(clippy::expect_used)] // APPROVED: bench harness
#![allow(missing_docs)] // APPROVED: criterion_group! macro

use std::hint::black_box;

#[path = "../../core/benches/support/tail_gate.rs"]
mod tail_gate;

use criterion::{Criterion, criterion_group};
use tickvault_storage::tick_conservation_ledger::{LEDGER_DAY_SLOTS, TickConservationLedger};

/// Budget key in `quality/benchmark-budgets.toml`.
const BUDGET_KEY: &str = "tick_conservation_ledger";
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;
/// 2026-10-30 10:00 IST as IST-epoch nanoseconds (IST day 20,756).
const TS: i64 = 20_756 * NANOS_PER_DAY + 36_000 * 1_000_000_000;

fn same_day(n: u64) -> i64 {
    TS + (n % 1_000_000) as i64
}

/// A day one slot-ring turn plus one away from the previous call's, so every
/// call finds its slot holding another day.
fn rolling_day(n: u64) -> i64 {
    TS + (n % 1_000) as i64 * (LEDGER_DAY_SLOTS as i64 + 1) * NANOS_PER_DAY
}

fn bench_ledger(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_conservation_ledger");
    let ledger = TickConservationLedger::new();
    let mut n = 0u64;
    group.bench_function("note_row_appended_same_day", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            ledger.note_row_appended(black_box(same_day(n)));
        });
    });
    group.bench_function("note_row_appended_day_rollover", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            ledger.note_row_appended(black_box(rolling_day(n)));
        });
    });
    group.finish();
    black_box(ledger.accepted_rows_on(TickConservationLedger::ist_day_of(TS)));
}

fn tail_gate() -> Result<(), String> {
    let mut gate = tail_gate::Gate::new("tick_conservation_ledger", BUDGET_KEY)?;
    let ledger = TickConservationLedger::new();
    // `note_row_appended` returns nothing; every call is its own path, so
    // the path check is the day's count after the same-day case.
    gate.case("note_row_appended_same_day", |n| {
        ledger.note_row_appended(black_box(same_day(n)));
        true
    });
    let day = TickConservationLedger::ist_day_of(TS);
    let counted = ledger.accepted_rows_on(day);
    if counted != tail_gate::PERCENTILE_SAMPLES as u64 {
        return Err(format!(
            "note_row_appended_same_day counted {counted} rows, expected {}",
            tail_gate::PERCENTILE_SAMPLES
        ));
    }
    gate.case("note_row_appended_day_rollover", |n| {
        ledger.note_row_appended(black_box(rolling_day(n)));
        true
    });
    gate.finish()
}

criterion_group!(benches, bench_ledger);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    tail_gate::run_under_bench("tick_conservation_ledger", tail_gate);
}
