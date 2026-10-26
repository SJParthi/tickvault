//! Audit L3 (2026-10-26) regression tests for `multi_tf_aggregator`.
//!
//! Kept out of the main file so its size pin does not move.

use super::tests::{DAY, tick};
use super::*;

/// 09:15:00 IST on `DAY`.
const OPEN: u32 = DAY + 33_300;
/// NSE_FNO: the segment that carries open interest.
const SEG_FNO: u8 = 2;

fn fno_tick(ts: u32, price: f32, cum: u32, oi: u32, buy: u32, sell: u32) -> ParsedTick {
    let mut t = tick(68_407, SEG_FNO, ts, price, cum);
    t.open_interest = oi;
    t.total_buy_quantity = buy;
    t.total_sell_quantity = sell;
    t
}

// Regression: 2026-10-26 — a packet the aggregator called stale (day
// cumulative below the last accepted one) but stamped at the same trade second
// was taken as the newest by the fold, so the bar's close, close time, OI and
// book totals moved to the older reading (measured live 2026-09-11, security
// 68407). It may widen high/low; nothing else.
#[test]
fn test_regression_stale_packet_never_moves_close_oi_or_book_totals() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(OPEN, 100.0, 10_000, 500, 10, 11),
        None,
        sink,
    );
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(OPEN + 1, 101.0, 20_000, 600, 20, 21),
        None,
        sink,
    );
    let volume_before = agg
        .snapshot(Feed::Dhan, 68_407, SEG_FNO, TfIndex::M1)
        .expect("open 1-minute bar")
        .volume;
    // Same trade second, LOWER cumulative: stale.
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(OPEN + 1, 105.0, 19_000, 550, 15, 16),
        None,
        sink,
    );
    let bar = agg
        .snapshot(Feed::Dhan, 68_407, SEG_FNO, TfIndex::M1)
        .expect("open 1-minute bar");
    assert_eq!(bar.close, 101.0, "a stale packet never moves the close");
    assert_eq!(bar.close_ts_ist_secs, OPEN + 1);
    assert_eq!(bar.oi, 600, "a stale packet never moves OI");
    assert_eq!(bar.total_buy_qty, 20, "nor the buy total");
    assert_eq!(bar.total_sell_qty, 21, "nor the sell total");
    assert_eq!(bar.high, 105.0, "it may widen the high");
    assert_eq!(bar.volume, volume_before, "and never moves volume");
}

// Regression: 2026-10-26 — the same rule on a SEALED bar: a stale packet that
// traded inside the sealed bucket, at or after its close time, amended the
// sealed close (`fold_late_hlc`).
#[test]
fn test_regression_stale_packet_never_moves_a_sealed_close() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let mut amended: Vec<LiveCandleState> = Vec::new();
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(OPEN, 100.0, 10_000, 500, 10, 11),
        None,
        |_, _, _, _, _| {},
    );
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(OPEN + 30, 101.0, 20_000, 600, 20, 21),
        None,
        |_, _, _, _, _| {},
    );
    // Opens 09:16 and seals 09:15.
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(OPEN + 60, 102.0, 30_000, 700, 30, 31),
        None,
        |_, _, _, _, _| {},
    );
    // Stale (cumulative below 30,000), traded inside the sealed 09:15 bar
    // AFTER its close time.
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(OPEN + 45, 110.0, 25_000, 650, 25, 26),
        None,
        |_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                amended.push(st);
            }
        },
    );
    let bar = amended
        .last()
        .copied()
        .expect("the stale packet still amends the sealed bar's range");
    assert_eq!(bar.bucket_start_ist_secs, OPEN);
    assert_eq!(bar.high, 110.0, "it may widen the high");
    assert_eq!(
        bar.close, 101.0,
        "a stale packet never moves a sealed close"
    );
    assert_eq!(bar.close_ts_ist_secs, OPEN + 30);
}

/// 18:05:00 IST on `DAY`, inside the Muhurat window [18:00, 19:30).
const MUHURAT_TICK: u32 = DAY + 65_100;

// Regression: 2026-10-26 — the fold refused every tick at or after 15:40 IST,
// so on a Muhurat day the evening session reached `ticks` (the writers widen
// for it) and never reached a candle.
#[test]
fn test_regression_muhurat_tick_folds_into_candles_on_a_muhurat_day() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    agg.set_muhurat_session(true);
    let mut sealed: Vec<(TfIndex, u32)> = Vec::new();
    let first = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(MUHURAT_TICK, 100.0, 1_000, 0, 0, 0),
        None,
        |_, _, _, _, _| {},
    );
    assert!(!first.out_of_session, "a Muhurat tick is in session");
    let bar = agg
        .snapshot(Feed::Dhan, 68_407, SEG_FNO, TfIndex::M1)
        .expect("the Muhurat tick opened a 1-minute bar");
    assert_eq!(bar.bucket_start_ist_secs, MUHURAT_TICK);
    assert_eq!(bar.close, 100.0);
    // The next minute seals the 18:05 bar.
    let _ = agg.consume_tick(
        Feed::Dhan,
        &fno_tick(MUHURAT_TICK + 60, 101.0, 2_000, 0, 0, 0),
        None,
        |_, _, _, tf, st| sealed.push((tf, st.bucket_start_ist_secs)),
    );
    assert!(sealed.contains(&(TfIndex::M1, MUHURAT_TICK)), "{sealed:?}");
}

/// The same tick on an ordinary day is still refused, and a Muhurat day does
/// not open the gap between the regular close and 18:00.
#[test]
fn test_set_muhurat_session_gates_the_widening_on_the_flag_and_the_window() {
    let mut ordinary = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    ordinary.set_muhurat_session(false);
    let refused = ordinary.consume_tick(
        Feed::Dhan,
        &fno_tick(MUHURAT_TICK, 100.0, 1_000, 0, 0, 0),
        None,
        |_, _, _, _, _| {},
    );
    assert!(refused.out_of_session);

    let mut muhurat = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    muhurat.set_muhurat_session(true);
    for secs_of_day in [
        56_400_u32,                                // 15:40, regular close
        MUHURAT_PERSIST_START_SECS_OF_DAY_IST - 1, // 17:59:59
        MUHURAT_PERSIST_END_SECS_OF_DAY_IST,       // 19:30, Muhurat close
    ] {
        let s = muhurat.consume_tick(
            Feed::Dhan,
            &fno_tick(DAY + secs_of_day, 100.0, 1_000, 0, 0, 0),
            None,
            |_, _, _, _, _| {},
        );
        assert!(s.out_of_session, "{secs_of_day} is outside both windows");
    }
    let edge = muhurat.consume_tick(
        Feed::Dhan,
        &fno_tick(
            DAY + MUHURAT_PERSIST_END_SECS_OF_DAY_IST - 1,
            100.0,
            1_000,
            0,
            0,
            0,
        ),
        None,
        |_, _, _, _, _| {},
    );
    assert!(
        !edge.out_of_session,
        "19:29:59 is inside the Muhurat window"
    );
}
