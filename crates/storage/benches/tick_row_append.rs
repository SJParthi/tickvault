//! Criterion benchmarks for building one tick's database row (ILP text in the
//! writer's buffer) — plan ITEM DT, the "row build" stage (2026-10-30).
//!
//! Each iteration appends [`ROWS_PER_ITER`] in-session ticks to a
//! disconnected [`TickWriter`] through the production entry points, so the
//! timed region holds exactly what the frame drain pays per row: building the
//! `TickRow` (price conversions, rounding), the session-window gate, the
//! contract-name lookup and the ILP line itself. The network send runs on
//! another thread and is not here. A writer serves [`BATCHES_PER_WRITER`]
//! iterations and is rebuilt with the timer stopped (`iter_custom`), so its
//! 2 MiB pre-sized buffer never regrows inside the timed region. Every bench
//! instrument has a published contract name, as in production.
//!
//! Targets (budget key `tick_row_append` in `quality/benchmark-budgets.toml`,
//! per-row via its `[elements]` count of 1,000):
//!   - `tick_row_append/full_row`: NSE_FNO Full ticks, every column set.
//!   - `tick_row_append/quote_row`: NSE_EQ Quote ticks (no OI columns).
//!   - `tick_row_append/tick_quotes_row`: the same Full ticks routed to
//!     `tick_quotes` (arrival `ts` plus `exchange_timestamp`).
//!
//! Companion zero-allocation evidence: `tests/dhat_tick_row_append.rs`.
//! Byte-identity evidence: the proptests in `src/tick_row_identity_tests.rs`.
//!
//! Run: `cargo bench --bench tick_row_append -p tickvault-storage`

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::candle_contract_labels::{
    CandleContractLabels, publish_candle_contract_labels,
};
use tickvault_storage::tick_persistence::TickWriter;

/// Rows appended per timed iteration; the `[elements]` divisor.
const ROWS_PER_ITER: usize = 1_000;

/// 2026-10-29 10:00:00 IST as IST epoch seconds (how Dhan sends LTT).
const SESSION_IST_SECS: u32 = 1_793_268_000;

/// IST minus UTC, seconds.
const IST_OFFSET_SECS: i64 = 19_800;

/// A Full NSE_FNO tick with every column carried. `i` varies the prices,
/// volume and times so the formatters see realistic, changing values.
fn full_tick(i: usize) -> ParsedTick {
    let step = i as f32 * 0.05;
    let ltt = SESSION_IST_SECS + (i as u32 / 10);
    ParsedTick {
        security_id: 40_000 + (i as u64 % 500),
        exchange_segment_code: 2,
        last_traded_price: 245.35 + step,
        last_trade_quantity: 75,
        exchange_timestamp: ltt,
        received_at_nanos: (i64::from(ltt) - IST_OFFSET_SECS) * 1_000_000_000 + 123_456_789,
        average_traded_price: 244.10 + step,
        volume: 1_250_000 + i as u32 * 75,
        total_sell_quantity: 98_250,
        total_buy_quantity: 112_500,
        day_open: 238.05,
        day_close: 240.95,
        day_high: 251.20 + step,
        day_low: 236.60,
        open_interest: 4_512_300,
        oi_day_high: 4_600_125,
        oi_day_low: 4_401_000,
        ..ParsedTick::default()
    }
}

/// A Quote NSE_EQ tick (no OI columns).
fn quote_tick(i: usize) -> ParsedTick {
    ParsedTick {
        security_id: 2_885 + (i as u64 % 200),
        exchange_segment_code: 1,
        open_interest: 0,
        oi_day_high: 0,
        oi_day_low: 0,
        last_traded_price: 2_841.65 + i as f32 * 0.05,
        ..full_tick(i)
    }
}

/// Timed batches one writer serves before it is rebuilt with the timer
/// stopped. 5 × 1,000 rows of about 290 bytes stays inside the writer's
/// 2 MiB pre-sized buffer, so no timed append ever regrows it.
const BATCHES_PER_WRITER: u64 = 5;

fn append_batch(writer: &mut TickWriter, ticks: &[ParsedTick], base_seq: i64, to_quotes: bool) {
    for (i, tick) in ticks.iter().enumerate() {
        let seq = base_seq + i as i64;
        let result = if to_quotes {
            writer.append_tick_quote_with_seq(black_box(tick), seq)
        } else {
            writer.append_tick_with_seq(black_box(tick), seq)
        };
        black_box(result.is_ok());
    }
}

/// Production rows carry a `contract` name (one table probe plus a symbol per
/// row), so the bench publishes one for every bench instrument.
fn publish_bench_labels() {
    let mut labels = CandleContractLabels::default();
    for i in 0..ROWS_PER_ITER {
        let f = full_tick(i);
        let q = quote_tick(i);
        let id = i64::try_from(f.security_id).unwrap_or_default();
        labels.insert((id, "NSE_FNO"), Arc::from("NIFTY-25Nov2026-24500-CE"));
        let id = i64::try_from(q.security_id).unwrap_or_default();
        labels.insert((id, "NSE_EQ"), Arc::from("RELIANCE"));
    }
    assert!(publish_candle_contract_labels(labels) > 0);
}

fn bench_rows(c: &mut Criterion, name: &str, ticks: &[ParsedTick], to_quotes: bool) {
    let mut group = c.benchmark_group("tick_row_append");
    group.throughput(Throughput::Elements(ROWS_PER_ITER as u64));
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut writer = TickWriter::for_test(Feed::Dhan);
            for n in 0..iters {
                if n > 0 && n % BATCHES_PER_WRITER == 0 {
                    // Rebuilt outside the timed region. The allocator hands
                    // back the memory the old buffer used, so the new buffer's
                    // pages are already mapped, as a production spare is.
                    writer = TickWriter::for_test(Feed::Dhan);
                }
                let base_seq = 1_000_000 + (n as i64) * ROWS_PER_ITER as i64;
                let start = Instant::now();
                append_batch(&mut writer, ticks, base_seq, to_quotes);
                total += start.elapsed();
            }
            black_box(writer.pending());
            total
        });
    });
    group.finish();
}

fn tick_row_append(c: &mut Criterion) {
    publish_bench_labels();
    let full: Vec<ParsedTick> = (0..ROWS_PER_ITER).map(full_tick).collect();
    let quote: Vec<ParsedTick> = (0..ROWS_PER_ITER).map(quote_tick).collect();
    // Non-vacuity: every bench tick must land as a row, not be refused by the
    // session window (a refused tick would time a cheaper path).
    let mut probe = TickWriter::for_test(Feed::Dhan);
    for (i, t) in full.iter().chain(quote.iter()).enumerate() {
        assert!(probe.append_tick_with_seq(t, i as i64 + 1).is_ok());
    }
    assert_eq!(
        probe.pending(),
        2 * ROWS_PER_ITER,
        "a bench tick was not appended"
    );
    assert_eq!(
        probe.aux_rows_appended(),
        0,
        "a bench tick was refused by the window"
    );
    for (i, t) in full.iter().enumerate() {
        assert!(
            probe
                .append_tick_quote_with_seq(t, 10_000 + i as i64)
                .is_ok()
        );
    }
    assert_eq!(
        probe.quote_rows_appended(),
        ROWS_PER_ITER as u64,
        "a bench tick did not reach tick_quotes"
    );
    assert_eq!(probe.aux_rows_appended(), 0, "a quote tick was refused");

    bench_rows(c, "full_row", &full, false);
    bench_rows(c, "quote_row", &quote, false);
    bench_rows(c, "tick_quotes_row", &full, true);
}

criterion_group!(benches, tick_row_append);
criterion_main!(benches);
