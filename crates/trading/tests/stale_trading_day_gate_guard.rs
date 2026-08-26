//! Guard — a stale last-trade time may not fabricate a candle on a closed day.
//!
//! # The bug, reproduced
//!
//! Dhan sends the **last trade time**. A contract that last traded days ago is
//! snapshotted NOW carrying a timestamp from THEN — measured across all 20.5M
//! rows of one live session: mean `received_at - ts` **~5 hours**, max **34
//! days**.
//!
//! The candle-window gate tests `exchange_timestamp % 86_400` — seconds of day
//! only, with no notion of WHICH day. The window is `[09:15:00, 15:40:00)`, so
//! a stale trade time of *yesterday 15:39:41* (56,381 s) sat inside it and
//! passed as "in session".
//!
//! Verified consequences on prod, 2026-08-26:
//!
//! - `candles_1m` held **8,898 bars on past dates**, 8,898 distinct
//!   instruments, oldest `2026-07-23T09:39` — in a QuestDB volume created
//!   **empty at 08:59:50 that same morning**. They were written that day.
//! - With a bucket already open on the stale date, today's real 09:15 tick
//!   takes the CONTINUE path rather than the OPEN path, so the day-open arm
//!   never fires — across all 24 timeframes.
//!
//! # What must NOT regress
//!
//! The refusal is CANDLE-ONLY. The tick is a real last-traded price carrying
//! live open interest and bid/ask; discarding the row would lose the ability to
//! tell "did not trade today" from "did not capture" — the same false-OK the
//! 2026-08-20 `untraded_sentinel` fix removed.

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;

/// IST epoch seconds for a given day-offset and time-of-day.
///
/// `exchange_timestamp` is IST epoch seconds directly — never add the offset
/// (`data-integrity.md`: "NEVER ADD +5:30 TO ts").
fn ist_at(day: u32, hh: u32, mm: u32, ss: u32) -> u32 {
    day * 86_400 + hh * 3_600 + mm * 60 + ss
}

fn tick_at(ts: u32, ltp: f32) -> ParsedTick {
    ParsedTick {
        security_id: 4242,
        exchange_segment_code: 2,
        exchange_timestamp: ts,
        last_traded_price: ltp,
        ..Default::default()
    }
}

/// A day index comfortably inside the plausible band
/// `[1_600_000_000, 2_524_608_000]`.
const TODAY: u32 = 20_690; // ~2026-08-26

#[test]
fn a_stale_trade_time_inside_the_session_window_does_not_fold() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    // Today, mid-session — establishes the watermark.
    let today = ist_at(TODAY, 11, 0, 0);
    let s = agg.consume_tick(Feed::Dhan, &tick_at(today, 100.0), None, |_, _, _, _, _| {});
    assert!(s.folded(), "a normal in-session tick must fold");

    // YESTERDAY 15:39:41. Seconds-of-day = 56,381, which is INSIDE the
    // [09:15:00, 15:40:00) window — this is exactly the shape that produced
    // the 8,898 fabricated bars.
    let stale = ist_at(TODAY - 1, 15, 39, 41);
    let secs_of_day = stale % 86_400;
    assert!(
        (33_300..56_400).contains(&secs_of_day),
        "precondition: {secs_of_day} must be inside the session window, or this \
         test is not reproducing the bug"
    );

    let s = agg.consume_tick(Feed::Dhan, &tick_at(stale, 99.0), None, |_, _, _, _, _| {});
    assert!(
        s.stale_trading_day,
        "a trade time from a previous DAY must be refused for folding even \
         though its seconds-of-day is inside the session window"
    );
    assert!(
        !s.folded(),
        "a stale-day tick must not report itself as folded"
    );
    assert_eq!(s.sealed_count, 0, "no bucket may be sealed on a closed day");
}

#[test]
fn the_row_is_still_kept_it_is_a_candle_only_refusal() {
    // The distinction that matters: `stale_trading_day` must NOT set any of
    // the hard-refusal flags, because the drain writes the tick row unless one
    // of those is set.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);
    agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY, 11, 0, 0), 100.0),
        None,
        |_, _, _, _, _| {},
    );

    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY - 3, 10, 0, 0), 99.0),
        None,
        |_, _, _, _, _| {},
    );

    assert!(s.stale_trading_day);
    assert!(
        !s.refused_price && !s.refused_timestamp && !s.slot_exhausted,
        "stale_trading_day must not set a HARD refusal flag — the drain drops \
         the row on those, and this tick is real data with a real old trade \
         time"
    );
}

#[test]
fn todays_ticks_still_fold_after_a_stale_one_arrives() {
    // The failure this whole change exists to prevent: a stale tick must not
    // leave state behind that breaks the next real tick.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    let open = ist_at(TODAY, 9, 15, 0);
    let s = agg.consume_tick(Feed::Dhan, &tick_at(open, 100.0), None, |_, _, _, _, _| {});
    assert!(s.folded(), "the 09:15 open must fold");

    // A stale straggler lands mid-session.
    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY - 1, 15, 39, 41), 55.0),
        None,
        |_, _, _, _, _| {},
    );
    assert!(s.stale_trading_day);

    // Today keeps working, and the stale price did not contaminate it.
    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY, 11, 0, 0), 101.0),
        None,
        |_, _, _, _, _| {},
    );
    assert!(
        s.folded(),
        "a stale tick must not poison the fold for subsequent real ticks"
    );
}

#[test]
fn a_stale_tick_does_not_move_the_watermark() {
    // The gate compares against the watermark, so if a stale tick could move
    // it backwards the gate would disarm itself. The advance is `>`, so it
    // cannot — pinned here because it is load-bearing for the gate above.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY, 11, 0, 0), 100.0),
        None,
        |_, _, _, _, _| {},
    );
    agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY - 10, 11, 0, 0), 50.0),
        None,
        |_, _, _, _, _| {},
    );

    // If the watermark had regressed, this second stale tick would now be
    // "current" and would fold.
    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY - 9, 11, 0, 0), 51.0),
        None,
        |_, _, _, _, _| {},
    );
    assert!(
        s.stale_trading_day,
        "the watermark must not have regressed to the stale tick's day"
    );
}

#[test]
fn same_day_earlier_ticks_still_fold() {
    // Non-vacuity, and the important negative case: within ONE day, ticks
    // legitimately arrive out of order (different instruments trade at
    // different times). Only a whole-DAY regression is refused; a
    // seconds-earlier tick on the same day must still fold, or this gate would
    // silently discard normal intraday out-of-order data.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY, 14, 0, 0), 100.0),
        None,
        |_, _, _, _, _| {},
    );
    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY, 9, 30, 0), 99.0),
        None,
        |_, _, _, _, _| {},
    );

    assert!(
        !s.stale_trading_day,
        "an EARLIER tick on the SAME day must still fold — intraday \
         out-of-order arrival is normal and must not be swept up by a gate \
         aimed at stale DAYS"
    );
}

#[test]
fn the_first_tick_of_a_process_is_not_refused() {
    // Cold start: the watermark begins at 0, so day 0. Nothing can be older
    // than that, and the first honest tick must fold rather than be gated by
    // an empty reference — the same cold-start trap the timestamp band's own
    // comment records having been written and caught once already.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);
    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_at(ist_at(TODAY, 9, 15, 0), 100.0),
        None,
        |_, _, _, _, _| {},
    );
    assert!(
        !s.stale_trading_day,
        "the first tick after boot must not be refused as stale"
    );
    assert!(s.folded());
}
