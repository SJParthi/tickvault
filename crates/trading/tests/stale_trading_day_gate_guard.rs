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

// ---------------------------------------------------------------------------
// The vendor's "no last trade time" sentinel (2026-08-26)
// ---------------------------------------------------------------------------
//
// Measured on prod: `tv_dhan_feed_ingest_refused_total{reason="timestamp"}` =
// 825,783 in one session — 4.0% of every tick decoded — and the drain treats a
// timestamp refusal as a HARD refusal, so all of them were discarded with NO
// ROW AT ALL. Not a missing candle: no record the instrument was even seen.
//
// The 2026-08-20 fix had already made this call correctly for the PRICE
// sentinel. An instrument that has never traded has no last price AND no last
// trade time, so the two co-occur — but the timestamp check ran first and hard-
// refused, silently defeating that fix for exactly the instruments it was for.

fn tick_with_receipt(ts: u32, ltp: f32, received_at_nanos: i64) -> ParsedTick {
    ParsedTick {
        security_id: 4242,
        exchange_segment_code: 2,
        exchange_timestamp: ts,
        last_traded_price: ltp,
        received_at_nanos,
        ..Default::default()
    }
}

#[test]
fn a_zero_timestamp_with_a_receipt_time_keeps_its_row() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_with_receipt(0, 0.0, 1_787_000_000_000_000_000),
        None,
        |_, _, _, _, _| panic!("a zero timestamp must never seal a bucket"),
    );

    assert!(
        s.untraded_timestamp,
        "ts == 0 is the vendor's 'no last trade time' sentinel, not corruption"
    );
    assert!(
        !s.refused_timestamp && !s.refused_price && !s.slot_exhausted,
        "it must NOT be a hard refusal — the drain drops the row on those, and \
         that is how 825,783 ticks a session were being discarded outright"
    );
    assert!(!s.folded(), "no candle may be folded from a zero timestamp");
}

#[test]
fn a_zero_timestamp_without_a_receipt_time_is_still_refused_outright() {
    // The guard that keeps this compatible with the 2026-08-09 / 2026-08-25
    // adversarial regressions instead of reversing them.
    //
    // `row_timestamp_ist_nanos` substitutes the receipt time for an
    // out-of-band LTT, but ONLY when the caller has one. With no receipt time
    // it falls back to the raw value, so a kept ts=0 row would land in a 1970
    // partition that retention and archival — both keyed on the trading day —
    // can never reach. That is the same unreachable-partition defect as
    // year-2106, approached from the other end of the number line.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    let s = agg.consume_tick(
        Feed::Dhan,
        &tick_with_receipt(0, 0.0, 0),
        None,
        |_, _, _, _, _| panic!("must never seal"),
    );

    assert!(
        s.refused_timestamp,
        "with NO receipt time the writer cannot stamp the row safely, so the \
         fold must refuse it outright"
    );
    assert!(!s.untraded_timestamp);
}

#[test]
fn corrupt_timestamps_keep_their_row_when_a_receipt_time_can_stamp_it() {
    // CORRECTED 2026-08-28. This test previously asserted the opposite --
    // that a corrupt (non-zero, out-of-band) LTT stayed a HARD refusal even
    // with a receipt -- on the stated grounds that "writing them puts a row
    // under a garbage designated timestamp".
    //
    // That premise is FALSE, and the code it describes says so verbatim.
    // `tick_persistence::row_timestamp_ist_nanos` bands the LTT and then:
    //
    //     received_at_ist_nanos.unwrap_or(ltt_nanos)
    //
    // under its own comment "Out of band falls back to the receipt time,
    // exactly as below-floor does." The fallback is keyed on the BAND, not on
    // the value being exactly zero -- so a row for ts = u32::MAX carried with
    // a receipt is stamped from the RECEIPT, in band, in the live range, and
    // reachable by retention and archival. There is no garbage timestamp to
    // avoid.
    //
    // The cost of the old behaviour was measured, not theoretical: a hard
    // refusal DISCARDS THE ROW ENTIRELY, so the tick's price never reached the
    // database at all. On 2026-08-27 that was 2,008,916 ticks -- 2.41% of the
    // session -- thrown away to avoid writing a timestamp the writer already
    // knew how to replace. This is the same shape as the zero-sentinel case
    // the sibling test above covers, and it is resolved the same way.
    //
    // What is NOT relaxed: the tick is still refused for CANDLES. A corrupt
    // LTT cannot place a bar, so no bucket opens and nothing seals. The row is
    // kept; the fold is not.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    for poison in [u32::MAX, 1, 1_599_999_999, 2_524_608_001] {
        let s = agg.consume_tick(
            Feed::Dhan,
            &tick_with_receipt(poison, 100.0, 1_787_000_000_000_000_000),
            None,
            |_, _, _, _, _| panic!("a corrupt timestamp must never seal a bucket"),
        );
        assert!(
            s.out_of_band_timestamp,
            "ts {poison} is out of band and must be classified as such"
        );
        assert!(
            !s.refused_timestamp,
            "ts {poison} carries a receipt, so the writer can stamp the row \
             from it -- discarding the row costs real market data for nothing"
        );
        assert!(
            !s.folded(),
            "ts {poison} cannot place a bar, so no candle may be folded"
        );
        assert!(
            !s.untraded_timestamp,
            "ts {poison} is not the zero sentinel"
        );
    }
}

#[test]
fn corrupt_timestamps_without_a_receipt_time_are_still_refused_outright() {
    // The half of the old assertion that REMAINS true, kept as its own test so
    // relaxing the receipt case above cannot be mistaken for relaxing both.
    //
    // With no receipt, `row_timestamp_ist_nanos` has nothing to fall back to
    // and returns the raw value -- so keeping the row would put ts = u32::MAX
    // (~year 2106) or ts = 1 (1970) into the DESIGNATED timestamp, creating a
    // QuestDB partition that retention and archival, both keyed on the trading
    // day, can never reach, while every `max(ts)` over `ticks` silently
    // includes it. That is the unreachable-partition defect the 2026-08-25
    // ceiling was added for, and it stays closed.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);

    for poison in [u32::MAX, 1, 1_599_999_999, 2_524_608_001] {
        let s = agg.consume_tick(
            Feed::Dhan,
            &tick_with_receipt(poison, 100.0, 0),
            None,
            |_, _, _, _, _| panic!("must never seal"),
        );
        assert!(
            s.refused_timestamp,
            "ts {poison} with NO receipt cannot be stamped safely, so the row \
             must be refused outright"
        );
        assert!(
            !s.out_of_band_timestamp,
            "the hard refusal is the classification"
        );
        assert!(
            !s.untraded_timestamp,
            "ts {poison} is not the zero sentinel"
        );
    }
}

/// `late_count` had ZERO production readers, and it counts DATA LOSS.
///
/// Every consumer of `ConsumeStats` reads `sealed_count` and `amended_count`;
/// `IngestOutcome::Folded` carries only those two. So `late_count` — the
/// number of timeframes that DISCARDED this tick as unplaceable — was computed
/// on every tick, for all 24 timeframes, and reached nothing. A bar missing a
/// trade is a wrong bar, and nothing moved.
///
/// This is a source assertion because the alternative is worse, not because it
/// is stronger: `metrics::Counter` offers no readback without installing a
/// process-global recorder, and installing one inside a unit test makes every
/// other test in the binary race on it. The BEHAVIOUR that `DiscardLate`
/// increments `late_count` is already covered by the aggregator's own tests;
/// what is unproven without this is that the value escapes the function.
#[test]
fn the_late_discard_is_counted_and_not_merely_tallied_into_a_dropped_struct() {
    let src = include_str!("../src/candles/multi_tf_aggregator.rs");
    // Split at the test MODULE boundary, not at the first `#[cfg(test)]`.
    //
    // This file carries test-only HELPERS inside production code — the first
    // marker sits ~500 lines ABOVE the arm this test is about — so the usual
    // `split("#[cfg(test)]").next()` idiom silently hands back the wrong half
    // and the test fails on correct code. A scanner that looks in the wrong
    // place is worse than no scanner: it teaches whoever hits it to delete the
    // assertion rather than to read it.
    let production = src.split("#[cfg(test)]\nmod tests").next().unwrap_or(src);

    let arm = production
        .find("ConsumeOutcome::DiscardLate =>")
        .expect("the late-discard arm must exist");
    // The arm's ACTUAL block, by brace matching — not a fixed byte window.
    //
    // A byte window is a proximity assertion, and proximity is not the
    // invariant: adding a comment inside the arm fails it, while moving the
    // increment OUT of the arm to a line just inside the window passes it.
    // Brittle where it blocks correct edits and permissive where it matters.
    // Two guards in this repo were rewritten on 2026-08-26 for exactly that,
    // so this one is written the right way round to begin with.
    let body = {
        let open = arm
            + production[arm..]
                .find('{')
                .expect("the arm must have a block");
        let mut depth = 0usize;
        let mut end = open;
        for (i, ch) in production[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        &production[open..=end]
    };
    assert!(
        body.contains("tick_discarded_late"),
        "the DiscardLate arm must increment a counter. Without it the drop is \
         tallied into a struct field that no production caller reads, which is \
         indistinguishable from not counting it at all"
    );

    // Pre-resolved handle, never the bare macro: this arm sits INSIDE the
    // 24-timeframe loop on the per-tick path, and `fold_counters.rs` exists
    // precisely because a `counter!` macro there costs a sharded-registry
    // lookup 24 times per tick.
    assert!(
        body.contains("fold_counters()"),
        "the increment must go through the pre-resolved handle set; a bare \
         `metrics::counter!` on the 24-times-per-tick path is the defect \
         fold_counters.rs was created to remove"
    );
    assert!(
        !body.contains("metrics::counter!"),
        "no bare counter macro inside the per-timeframe loop"
    );

    // And the handle must actually exist with a distinct metric name — not be
    // folded into the tick_refused family, which means something stronger and
    // different: "the whole tick was refused and nothing was folded". A tick
    // counted here DID fold into other timeframes.
    let counters_src = include_str!("../src/candles/fold_counters.rs");
    assert!(
        counters_src.contains("tv_candle_tick_discarded_late_total"),
        "the late discard needs its own metric name, not a `reason` label on \
         tv_aggregator_tick_refused_total — those mean different things"
    );
}
