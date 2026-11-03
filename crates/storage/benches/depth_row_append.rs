//! Criterion benchmarks for building one depth row (ILP text in the depth
//! writer's buffer) — plan ITEM DT, the depth row stage (2026-10-30).
//!
//! Each iteration appends [`ROWS_PER_ITER`] in-session depth rows to a
//! disconnected [`DepthWriter`] through the production entry points, so the
//! timed region holds what the frame drain pays per row: the session-window
//! gate, the contract-name lookup (once per instrument run, as the drain
//! writes a packet's rows together) and the ILP line itself. The network send
//! runs on another thread and is not here. A writer serves
//! [`BATCHES_PER_WRITER`] iterations and is rebuilt with the timer stopped
//! (`iter_custom`), so its pre-sized buffer never regrows inside the timed
//! region. Every bench instrument has a published contract name.
//!
//! Targets (budget key `depth_row_append` in `quality/benchmark-budgets.toml`,
//! per row via its `[elements]` count of 1,000):
//!   - `depth_row_append/level_row`: `market_depth`, one row per level, 20
//!     levels per instrument run (a depth-20 side).
//!   - `depth_row_append/book_row_5`: `market_depth_book`, one array row per
//!     side of five levels (the inline depth a Full packet carries, the
//!     production shape since depth sockets were switched off).
//!
//! After Criterion, both shapes are timed one row at a time and a p99 over
//! the budget exits 1 (`../../core/benches/support/tail_gate.rs`). Only under
//! `cargo bench` (`--bench`).
//!
//! Companion zero-allocation evidence: `tests/dhat_depth_append_zero_alloc.rs`.
//! Byte-identity evidence: the proptest in `src/depth_persistence.rs`
//! (`proptest_prechecked_depth_lines_match_the_checked_lines`).
//!
//! Run: `cargo bench --bench depth_row_append -p tickvault-storage`

#![allow(clippy::unwrap_used)] // APPROVED: bench harness
#![allow(clippy::expect_used)] // APPROVED: bench harness
#![allow(missing_docs)] // APPROVED: criterion_group! macro

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "../../core/benches/support/tail_gate.rs"]
mod tail_gate;

use criterion::{Criterion, Throughput, criterion_group};

use tickvault_common::feed::Feed;
use tickvault_storage::candle_contract_labels::{
    CandleContractLabels, publish_candle_contract_labels,
};
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_5, DEPTH_KIND_20, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthBookRow, DepthRow,
    DepthWriter,
};

/// Budget key in `quality/benchmark-budgets.toml`.
const BUDGET_KEY: &str = "depth_row_append";

/// Rows appended per timed iteration; the `[elements]` divisor.
const ROWS_PER_ITER: usize = 1_000;

/// Levels per instrument run of level rows (one depth-20 side).
const LEVELS_PER_RUN: usize = 20;

/// Levels in one inline book row (a Full packet's five-level book).
const BOOK_LEVELS: usize = 5;

/// Timed batches one writer serves before it is rebuilt with the timer
/// stopped. 5 × 1,000 level rows of about 170 bytes stays inside the level
/// writer's pre-sized buffer, and 5 × 1,000 book rows inside the book one.
const BATCHES_PER_WRITER: u64 = 5;

/// 2026-10-29 10:00:00 IST as IST-epoch nanoseconds (the depth `ts` is the
/// receipt instant in IST), inside the session window.
const SESSION_TS_NANOS: i64 = 1_793_268_000_000_000_000;

/// Instruments the bench cycles through.
const INSTRUMENTS: usize = 50;

fn security_id(run: usize) -> i64 {
    52_000 + (run % INSTRUMENTS) as i64
}

/// The `n`-th level row: runs of [`LEVELS_PER_RUN`] levels per instrument,
/// prices and sizes changing per level so the float formatter sees real work.
fn level_row(n: usize, seq: i64) -> DepthRow {
    let run = n / LEVELS_PER_RUN;
    let level = n % LEVELS_PER_RUN;
    DepthRow {
        security_id: security_id(run),
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_20,
        side: if run.is_multiple_of(2) {
            DEPTH_SIDE_BID
        } else {
            DEPTH_SIDE_ASK
        },
        level: level as i64 + 1,
        price: 245.35 + level as f64 * 0.05 + (n % 7) as f64 * 0.1,
        quantity: 75 * (level as i64 + 1),
        orders: (level as i64 % 9) + 1,
        capture_seq: seq,
        ts_nanos: SESSION_TS_NANOS + (n as i64) * 1_000,
    }
}

/// Five-level price, quantity and order arrays for book row `n`.
fn book_arrays(n: usize) -> ([f64; BOOK_LEVELS], [f64; BOOK_LEVELS], [f64; BOOK_LEVELS]) {
    let mut prices = [0.0; BOOK_LEVELS];
    let mut quantities = [0.0; BOOK_LEVELS];
    let mut orders = [0.0; BOOK_LEVELS];
    for level in 0..BOOK_LEVELS {
        prices[level] = 245.35 + level as f64 * 0.05 + (n % 7) as f64 * 0.1;
        quantities[level] = (75 * (level + 1)) as f64;
        orders[level] = (level % 9 + 1) as f64;
    }
    (prices, quantities, orders)
}

fn book_row<'a>(
    n: usize,
    seq: i64,
    arrays: &'a ([f64; BOOK_LEVELS], [f64; BOOK_LEVELS], [f64; BOOK_LEVELS]),
) -> DepthBookRow<'a> {
    // Bid then ask for each instrument, as the drain writes one packet.
    DepthBookRow {
        security_id: security_id(n / 2),
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_5,
        side: if n.is_multiple_of(2) {
            DEPTH_SIDE_BID
        } else {
            DEPTH_SIDE_ASK
        },
        prices: &arrays.0,
        quantities: &arrays.1,
        orders: &arrays.2,
        capture_seq: seq,
        ts_nanos: SESSION_TS_NANOS + (n as i64) * 1_000,
    }
}

/// Production rows carry a `contract` name, so the bench publishes one for
/// every bench instrument.
fn publish_bench_labels() {
    let mut labels = CandleContractLabels::default();
    for run in 0..INSTRUMENTS {
        labels.insert(
            (security_id(run), "NSE_FNO"),
            Arc::from("NIFTY-25Nov2026-24500-CE"),
        );
    }
    assert!(publish_candle_contract_labels(labels) > 0);
}

/// One timed shape: `append(writer, n, seq)` appends row `n` and reports
/// whether it was written.
fn bench_shape(
    c: &mut Criterion,
    name: &str,
    new_writer: fn() -> DepthWriter,
    append: fn(&mut DepthWriter, usize, i64) -> bool,
) {
    let mut group = c.benchmark_group("depth_row_append");
    group.throughput(Throughput::Elements(ROWS_PER_ITER as u64));
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut writer = new_writer();
            for it in 0..iters {
                if it > 0 && it % BATCHES_PER_WRITER == 0 {
                    // Rebuilt outside the timed region.
                    writer = new_writer();
                }
                let base_seq = 1_000_000 + (it as i64) * ROWS_PER_ITER as i64;
                let start = Instant::now();
                for n in 0..ROWS_PER_ITER {
                    black_box(append(&mut writer, n, base_seq + n as i64));
                }
                total += start.elapsed();
            }
            black_box(writer.pending());
            total
        });
    });
    group.finish();
}

fn new_level_writer() -> DepthWriter {
    DepthWriter::for_test(Feed::Dhan)
}

fn new_book_writer() -> DepthWriter {
    DepthWriter::for_test_book(Feed::Dhan)
}

fn append_level(writer: &mut DepthWriter, n: usize, seq: i64) -> bool {
    writer.append_row(black_box(&level_row(n, seq))).is_ok()
}

fn append_book(writer: &mut DepthWriter, n: usize, seq: i64) -> bool {
    let arrays = book_arrays(n);
    writer
        .append_book_row(black_box(&book_row(n, seq, &arrays)))
        .is_ok()
}

/// Non-vacuity: every bench row must land in the buffer, not be refused by
/// the session window (a refused row would time a cheaper path).
fn assert_rows_land() {
    let mut levels = new_level_writer();
    let mut books = new_book_writer();
    for n in 0..ROWS_PER_ITER {
        assert!(append_level(&mut levels, n, n as i64 + 1));
        assert!(append_book(&mut books, n, n as i64 + 1));
    }
    assert_eq!(
        levels.pending(),
        ROWS_PER_ITER,
        "a level row was not written"
    );
    assert_eq!(
        books.pending(),
        ROWS_PER_ITER * BOOK_LEVELS,
        "a book row was not written"
    );
    let text = String::from_utf8_lossy(levels.buffer_bytes());
    assert_eq!(text.matches("market_depth,").count(), ROWS_PER_ITER);
    assert!(text.contains("contract=NIFTY-25Nov2026-24500-CE"));
}

fn depth_row_append(c: &mut Criterion) {
    publish_bench_labels();
    assert_rows_land();
    bench_shape(c, "level_row", new_level_writer, append_level);
    bench_shape(c, "book_row_5", new_book_writer, append_book);
}

/// Times one row per call. A writer is replaced every
/// [`BATCHES_PER_WRITER`] × [`ROWS_PER_ITER`] rows inside the call that
/// needs it, as the timed loop above does outside its timer; that call is
/// counted, so the replacement shows in the p99 only if it is that common.
fn gate_shape(
    gate: &mut tail_gate::Gate,
    name: &str,
    new_writer: fn() -> DepthWriter,
    append: fn(&mut DepthWriter, usize, i64) -> bool,
    rows_per_written: usize,
) -> Result<(), String> {
    let per_writer = BATCHES_PER_WRITER as usize * ROWS_PER_ITER;
    let mut writer = new_writer();
    let mut written = 0usize;
    gate.case(name, |n| {
        let n = n as usize;
        if n > 0 && n.is_multiple_of(per_writer) {
            written += writer.pending();
            writer = new_writer();
        }
        append(&mut writer, n % ROWS_PER_ITER, n as i64 + 1)
    });
    written += writer.pending();
    let expected = tail_gate::PERCENTILE_SAMPLES * rows_per_written;
    if written != expected {
        return Err(format!(
            "{name}: {written} rows counted, expected {expected}"
        ));
    }
    Ok(())
}

fn tail_gate() -> Result<(), String> {
    publish_bench_labels();
    let mut gate = tail_gate::Gate::new("depth_row_append", BUDGET_KEY)?;
    gate_shape(&mut gate, "level_row", new_level_writer, append_level, 1)?;
    gate_shape(
        &mut gate,
        "book_row_5",
        new_book_writer,
        append_book,
        BOOK_LEVELS,
    )?;
    gate.finish()
}

criterion_group!(benches, depth_row_append);

fn main() {
    benches();
    criterion::Criterion::default()
        .configure_from_args()
        .final_summary();
    tail_gate::run_under_bench("depth_row_append", tail_gate);
}
