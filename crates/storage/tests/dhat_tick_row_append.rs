//! DHAT zero-allocation ratchet for building one tick's database row (plan
//! ITEM DT, 2026-10-30): `TickWriter::append_tick_with_seq` and
//! `append_tick_quote_with_seq`, the per-tick row build on the frame drain.
//!
//! Each call builds the `TickRow` (price conversions), runs the session-window
//! gate, looks up the contract name and writes one ILP line into the writer's
//! pre-sized buffer. None of it may allocate: at ~5,000 ticks a second an
//! allocation per row would be millions per session on the task that empties
//! the sockets. Pinned at zero across `ticks` rows (Full and Quote shapes, a
//! named and an unnamed contract) and `tick_quotes` rows, for BOTH encoders:
//! the questdb-rs library and our own (`[tick_storage] own_ilp_encoder`,
//! plan ITEM DT, owner 2026-10-30).
//!
//! Companion bench: `benches/tick_row_append.rs`.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::candle_contract_labels::{
    CandleContractLabels, publish_candle_contract_labels,
};
use tickvault_storage::tick_ilp::TickIlpEncoder;
use tickvault_storage::tick_persistence::TickWriter;

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

/// Rows per measured attempt. Three attempts plus the warm-up stay well inside
/// the writer's 2 MiB pre-sized buffer, so no append regrows it.
const ROWS_PER_ATTEMPT: usize = 1_200;

/// 2026-10-29 10:00:00 IST, IST epoch seconds (how Dhan sends LTT).
const SESSION_IST_SECS: u32 = 1_793_268_000;

fn tick(i: usize) -> ParsedTick {
    let ltt = SESSION_IST_SECS + (i as u32 / 10);
    let fno = i % 2 == 0;
    ParsedTick {
        security_id: 40_000 + (i as u64 % 64),
        exchange_segment_code: if fno { 2 } else { 1 },
        last_traded_price: 245.35 + i as f32 * 0.05,
        last_trade_quantity: 75,
        exchange_timestamp: ltt,
        received_at_nanos: (i64::from(ltt) - 19_800) * 1_000_000_000 + 250_000_000,
        average_traded_price: 244.1,
        volume: 1_250_000 + i as u32,
        total_sell_quantity: 98_250,
        total_buy_quantity: 112_500,
        day_open: 238.05,
        day_close: 240.95,
        day_high: 251.2,
        day_low: 236.6,
        open_interest: if fno { 4_512_300 } else { 0 },
        oi_day_high: if fno { 4_600_125 } else { 0 },
        oi_day_low: if fno { 4_401_000 } else { 0 },
        ..ParsedTick::default()
    }
}

/// Appends `ticks` through `writer`, a third on the `tick_quotes` route,
/// inside the measured window; returns (bytes, blocks).
fn measure_rows(writer: &mut TickWriter, ticks: &[ParsedTick], seq_base: i64) -> (u64, u64) {
    let mut seq = seq_base;
    dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for (i, t) in ticks.iter().enumerate() {
                seq += 1;
                let appended = if i % 3 == 0 {
                    writer.append_tick_quote_with_seq(t, seq)
                } else {
                    writer.append_tick_with_seq(t, seq)
                };
                std::hint::black_box(appended.is_ok());
            }
        },
    )
}

/// A writer warmed outside the window: first-use statics (counters, the
/// label snapshot, our encoder's f64 scratch buffer) are initialised here,
/// not inside the measurement.
fn warmed_writer(encoder: TickIlpEncoder, ticks: &[ParsedTick]) -> TickWriter {
    let mut writer = TickWriter::for_test_with_encoder(Feed::Dhan, encoder);
    for (i, t) in ticks.iter().take(16).enumerate() {
        writer
            .append_tick_with_seq(t, 1 + i as i64)
            .expect("warm-up row");
        writer
            .append_tick_quote_with_seq(t, 100 + i as i64)
            .expect("warm-up quote row");
    }
    writer
}

/// Non-vacuity: every measured tick became a `ticks` or `tick_quotes` row
/// (none was refused to `feed_aux_packets`, which also counts as pending),
/// and the `tick_quotes` route really ran.
fn assert_rows_landed(writer: &TickWriter, before: usize, encoder: TickIlpEncoder) {
    assert!(
        writer.pending() >= before + ROWS_PER_ATTEMPT,
        "{encoder:?}: the measured appends wrote no rows"
    );
    assert_eq!(
        writer.aux_rows_appended(),
        0,
        "{encoder:?}: a tick was refused by the window"
    );
    assert!(
        writer.quote_rows_appended() >= (ROWS_PER_ATTEMPT / 3) as u64,
        "{encoder:?}: the tick_quotes route wrote no rows"
    );
}

#[test]
fn dhat_tick_row_append_is_zero_allocation() {
    // Names for half the instruments, so both the named and the unnamed
    // `contract` arm run. Published before the window opens.
    let mut labels = CandleContractLabels::default();
    for id in (40_000_i64..40_064).step_by(2) {
        labels.insert((id, "NSE_FNO"), Arc::from("NIFTY-25Nov2026-24500-CE"));
    }
    assert!(publish_candle_contract_labels(labels) > 0);

    let ticks: Vec<ParsedTick> = (0..ROWS_PER_ATTEMPT).map(tick).collect();
    // Both encoders: the questdb-rs library (default) and our own
    // (`[tick_storage] own_ilp_encoder`, plan ITEM DT, owner 2026-10-30).
    let mut library = warmed_writer(TickIlpEncoder::Library, &ticks);
    let mut own = warmed_writer(TickIlpEncoder::Own, &ticks);
    let (lib_before, own_before) = (library.pending(), own.pending());

    let _profiler = dhat::Profiler::builder().testing().build();
    for (encoder, writer, before) in [
        (TickIlpEncoder::Library, &mut library, lib_before),
        (TickIlpEncoder::Own, &mut own, own_before),
    ] {
        let (bytes, blocks) = measure_rows(writer, &ticks, 1_000);
        assert!(
            bytes == BUDGET_BYTES && blocks == BUDGET_BLOCKS,
            "{encoder:?}: the tick row build allocated {bytes} bytes / {blocks} blocks over \
             {ROWS_PER_ATTEMPT} rows. It runs on the frame drain for every tick and must \
             write into the pre-sized buffer only — no String, no Vec, no format!."
        );
        assert_rows_landed(writer, before, encoder);
    }
}
