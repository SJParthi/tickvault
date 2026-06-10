//! Benchmark: f32→f64 precision-preserving conversion and ILP buffer building.
//!
//! STORAGE-GAP-02: f32_to_f64_clean must be zero-allocation and fast.
//! Budget: < 50ns per conversion (stack buffer, no heap).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_storage::tick_persistence_testing::f32_to_f64_clean_pub;

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

/// A realistic in-window Quote-mode tick (NIFTY spot shape) — same fixture
/// as `crates/core/benches/full_tick_processing.rs`.
fn sample_tick() -> tickvault_common::tick_types::ParsedTick {
    tickvault_common::tick_types::ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        last_traded_price: 23_146.45,
        last_trade_quantity: 0,
        exchange_timestamp: 1_770_000_000,
        received_at_nanos: 1_770_000_000_000_000_000,
        average_traded_price: 23_140.10,
        volume: 1_234_567,
        total_sell_quantity: 100,
        total_buy_quantity: 200,
        day_open: 23_100.0,
        day_close: 23_050.0,
        day_high: 23_200.0,
        day_low: 23_000.0,
        open_interest: 0,
        oi_day_high: 0,
        oi_day_low: 0,
        iv: f64::NAN,
        delta: f64::NAN,
        gamma: f64::NAN,
        theta: f64::NAN,
        vega: f64::NAN,
    }
}

/// Current production row encoder (string names → per-row validation).
fn bench_build_tick_row_current(c: &mut Criterion) {
    let mut buffer = tickvault_storage::tick_persistence_testing::new_ilp_buffer_pub();
    let tick = sample_tick();
    let mut rows: usize = 0;
    let mut seq: i64 = 1;
    c.bench_function("ilp_row/build_current", |b| {
        b.iter(|| {
            seq = seq.wrapping_add(1);
            let _ = black_box(
                tickvault_storage::tick_persistence_testing::build_tick_row_seq_pub(
                    &mut buffer,
                    black_box(&tick),
                    seq,
                ),
            );
            rows += 1;
            if rows >= 1000 {
                buffer.clear();
                rows = 0;
            }
        });
    });
}

/// Variant: identical row, but table/column names wrapped in pre-validated
/// `TableName`/`ColumnName` so questdb-rs skips per-row name validation
/// (its docs: "it doesn't have to validate it again. This saves CPU
/// cycles."). Proves the delta BEFORE the production encoder is changed.
#[allow(clippy::too_many_lines)] // APPROVED: bench replica of the 17-column row
fn bench_build_tick_row_prevalidated_names(c: &mut Criterion) {
    use questdb::ingress::{Buffer, ColumnName, ProtocolVersion, TableName, TimestampNanos};
    use tickvault_storage::tick_persistence::{f32_to_f64_clean, round_to_2dp, tick_payload_hash};

    const IST_UTC_OFFSET_NANOS: i64 = 19_800 * 1_000_000_000;

    let t_ticks = TableName::new("ticks").expect("valid");
    let c_segment = ColumnName::new("segment").expect("valid");
    let c_security_id = ColumnName::new("security_id").expect("valid");
    let c_ltp = ColumnName::new("ltp").expect("valid");
    let c_open = ColumnName::new("open").expect("valid");
    let c_high = ColumnName::new("high").expect("valid");
    let c_low = ColumnName::new("low").expect("valid");
    let c_close = ColumnName::new("close").expect("valid");
    let c_volume = ColumnName::new("volume").expect("valid");
    let c_oi = ColumnName::new("oi").expect("valid");
    let c_avg_price = ColumnName::new("avg_price").expect("valid");
    let c_last_trade_qty = ColumnName::new("last_trade_qty").expect("valid");
    let c_total_buy_qty = ColumnName::new("total_buy_qty").expect("valid");
    let c_total_sell_qty = ColumnName::new("total_sell_qty").expect("valid");
    let c_exchange_timestamp = ColumnName::new("exchange_timestamp").expect("valid");
    let c_received_at = ColumnName::new("received_at").expect("valid");
    let c_payload_hash = ColumnName::new("payload_hash").expect("valid");
    let c_capture_seq = ColumnName::new("capture_seq").expect("valid");

    let mut buffer = Buffer::new(ProtocolVersion::V1);
    let tick = sample_tick();
    let mut rows: usize = 0;
    let mut seq: i64 = 1;
    c.bench_function("ilp_row/build_prevalidated_names", |b| {
        b.iter(|| {
            seq = seq.wrapping_add(1);
            let ts_nanos = TimestampNanos::new(
                i64::from(tick.exchange_timestamp).saturating_mul(1_000_000_000),
            );
            let received_nanos =
                TimestampNanos::new(tick.received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS));
            let result: questdb::Result<()> = (|| {
                buffer
                    .table(t_ticks)?
                    .symbol(c_segment, "IDX_I")?
                    .column_i64(c_security_id, i64::from(tick.security_id))?
                    .column_f64(
                        c_ltp,
                        round_to_2dp(f32_to_f64_clean(tick.last_traded_price)),
                    )?
                    .column_f64(c_open, round_to_2dp(f32_to_f64_clean(tick.day_open)))?
                    .column_f64(c_high, round_to_2dp(f32_to_f64_clean(tick.day_high)))?
                    .column_f64(c_low, round_to_2dp(f32_to_f64_clean(tick.day_low)))?
                    .column_f64(c_close, round_to_2dp(f32_to_f64_clean(tick.day_close)))?
                    .column_i64(c_volume, i64::from(tick.volume))?
                    .column_i64(c_oi, i64::from(tick.open_interest))?
                    .column_f64(
                        c_avg_price,
                        round_to_2dp(f32_to_f64_clean(tick.average_traded_price)),
                    )?
                    .column_i64(c_last_trade_qty, i64::from(tick.last_trade_quantity))?
                    .column_i64(c_total_buy_qty, i64::from(tick.total_buy_quantity))?
                    .column_i64(c_total_sell_qty, i64::from(tick.total_sell_quantity))?
                    .column_i64(c_exchange_timestamp, i64::from(tick.exchange_timestamp))?
                    .column_ts(c_received_at, received_nanos)?
                    .column_i64(c_payload_hash, tick_payload_hash(&tick))?
                    .column_i64(c_capture_seq, seq)?
                    .at(ts_nanos)?;
                Ok(())
            })();
            let _ = black_box(result);
            rows += 1;
            if rows >= 1000 {
                buffer.clear();
                rows = 0;
            }
        });
    });
}

/// The six per-row price conversions (`round_to_2dp(f32_to_f64_clean(v))`)
/// in isolation — the other major row-build cost candidate.
fn bench_price_conversion_x6(c: &mut Criterion) {
    use tickvault_storage::tick_persistence::{f32_to_f64_clean, round_to_2dp};
    let tick = sample_tick();
    c.bench_function("ilp_row/price_conversion_x6", |b| {
        b.iter(|| {
            black_box(round_to_2dp(f32_to_f64_clean(black_box(
                tick.last_traded_price,
            ))));
            black_box(round_to_2dp(f32_to_f64_clean(black_box(tick.day_open))));
            black_box(round_to_2dp(f32_to_f64_clean(black_box(tick.day_high))));
            black_box(round_to_2dp(f32_to_f64_clean(black_box(tick.day_low))));
            black_box(round_to_2dp(f32_to_f64_clean(black_box(tick.day_close))));
            black_box(round_to_2dp(f32_to_f64_clean(black_box(
                tick.average_traded_price,
            ))));
        });
    });
}

criterion_group!(
    benches,
    bench_f32_to_f64_clean_typical_price,
    bench_f32_to_f64_clean_zero,
    bench_f32_to_f64_clean_small_decimal,
    bench_f32_to_f64_clean_large_value,
    bench_build_tick_row_current,
    bench_build_tick_row_prevalidated_names,
    bench_price_conversion_x6
);
criterion_main!(benches);
