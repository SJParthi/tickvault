//! `advanced` (a real new trade, plan PR AL-1, 2026-11-06) tests for
//! `multi_tf_aggregator`.
//!
//! Kept out of the main file so its size pin does not move.

use super::tests::{OPEN, SEG_IDX, quote, tick};
use super::*;

#[test]
fn test_advanced_is_false_on_the_first_packet_after_a_subscribe() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let s = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 10, 75.0, 4_000, 0, 0),
        None,
        ignore_seal,
    );
    assert!(s.folded(), "the fixture must fold");
    assert!(!s.advanced, "a seeding packet is never a new trade");
}

#[test]
fn test_advanced_is_true_only_when_the_cumulative_strictly_increases() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let _ = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 10, 75.0, 4_000, 0, 0),
        None,
        ignore_seal,
    );
    let traded = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 12, 75.5, 4_050, 0, 0),
        None,
        ignore_seal,
    );
    assert!(traded.advanced, "new volume is a new trade");
    let repeat = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 12, 75.5, 4_050, 7, 0),
        None,
        ignore_seal,
    );
    assert!(repeat.repeat_quote && !repeat.advanced);
    let stale = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 11, 75.25, 4_020, 0, 0),
        None,
        ignore_seal,
    );
    assert!(!stale.advanced, "a cumulative regression is stale, not new");
    // Same volume, a later trade second: not a new trade on a contract
    // whose cumulative moves (only a zero-volume index uses the time).
    let book = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 40, 75.5, 4_050, 0, 0),
        None,
        ignore_seal,
    );
    assert!(!book.advanced);
}

#[test]
fn test_advanced_on_a_zero_volume_index_follows_the_trade_second() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let seed = agg.consume_tick(
        Feed::Dhan,
        &tick(13, SEG_IDX, OPEN + 40, 23_146.45, 0),
        None,
        ignore_seal,
    );
    assert!(!seed.advanced, "the seeding print is never counted");
    let same_second = agg.consume_tick(
        Feed::Dhan,
        &tick(13, SEG_IDX, OPEN + 40, 23_146.75, 0),
        None,
        ignore_seal,
    );
    assert!(!same_second.advanced);
    let next = agg.consume_tick(
        Feed::Dhan,
        &tick(13, SEG_IDX, OPEN + 41, 23_147.0, 0),
        None,
        ignore_seal,
    );
    assert!(next.advanced, "a later index print is new");
    let older = agg.consume_tick(
        Feed::Dhan,
        &tick(13, SEG_IDX, OPEN + 39, 23_146.0, 0),
        None,
        ignore_seal,
    );
    assert!(!older.advanced, "an older index print is not new");
}

#[test]
fn test_regression_advanced_is_false_on_the_packet_that_ends_the_hand_over_gap() {
    // Review 2026-11-06 (plan PR AL-1): after a WAL replay, the first live
    // packet ends the hand-over gap; its delta spans the downtime and its
    // trade time can be an old downtime trade, so it is not a lag sample.
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    agg.set_replay_mode(true);
    let _ = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 10, 75.0, 4_000, 0, 0),
        None,
        ignore_seal,
    );
    let _ = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 20, 75.5, 4_100, 0, 0),
        None,
        ignore_seal,
    );
    agg.finish_replay(false, ignore_seal);
    let first_live = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 600, 76.0, 4_600, 0, 0),
        None,
        ignore_seal,
    );
    assert!(
        !first_live.advanced,
        "the packet that ends the hand-over gap is never a new-trade sample"
    );
    let next = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 602, 76.25, 4_700, 0, 0),
        None,
        ignore_seal,
    );
    assert!(next.advanced, "the following trade is new again");
}

/// UTC receipt nanos of an IST epoch second.
fn recv_at(ist_secs: u32) -> i64 {
    (i64::from(ist_secs) - crate::candles::tf_index::IST_UTC_OFFSET_SECS) * 1_000_000_000
}

/// LF-6b review finding 2: the first packet folded for an instrument is a
/// fresh trade when it traded while this process listened (the 09:15:00
/// opening trade after a pre-market boot), and not when it is a connect
/// snapshot of an earlier trade, a late delivery, or there is no capture
/// start.
#[test]
fn test_fresh_first_marks_the_opening_trade_and_not_a_connect_snapshot() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    agg.set_live_capture_start(OPEN - 2_700); // an 08:30 boot
    let open = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN, 75.0, 4_000, 0, recv_at(OPEN)),
        None,
        ignore_seal,
    );
    assert!(open.folded());
    assert!(!open.advanced, "the seeding packet is never `advanced`");
    assert!(
        open.fresh_first,
        "the 09:15:00 trade happened while we listened"
    );
    assert!(!open.older_trade);
    let next = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 1, 75.5, 4_050, 0, recv_at(OPEN + 1)),
        None,
        ignore_seal,
    );
    assert!(next.advanced && !next.fresh_first);
    // A mid-session boot: the connect snapshot carries a trade from before
    // the capture start.
    let mut mid = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    mid.set_live_capture_start(OPEN + 3_600);
    let snap = mid.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 1_000, 75.0, 4_000, 0, recv_at(OPEN + 3_601)),
        None,
        ignore_seal,
    );
    assert!(
        snap.folded() && !snap.fresh_first,
        "a snapshot is not fresh"
    );
    // Delivered more than the lag bound after it traded: not fresh.
    let mut late = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    late.set_live_capture_start(OPEN - 2_700);
    let lagged = late.consume_tick(
        Feed::Dhan,
        &quote(
            OPEN + 100,
            75.0,
            4_000,
            0,
            recv_at(OPEN + 100 + FRESH_FIRST_MAX_LAG_SECS + 1),
        ),
        None,
        ignore_seal,
    );
    assert!(lagged.folded() && !lagged.fresh_first);
    // No capture start recorded: never fresh.
    let mut none = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let s = none.consume_tick(
        Feed::Dhan,
        &quote(OPEN, 75.0, 4_000, 0, recv_at(OPEN)),
        None,
        ignore_seal,
    );
    assert!(s.folded() && !s.fresh_first);
}

/// LF-6b review finding 4: a real trade older than the newest one seen is
/// `older_trade`; a repeat quote, the seeding packet and a new trade are not.
#[test]
fn test_older_trade_marks_regressions_and_same_second_index_prints_only() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let seed = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 10, 75.0, 4_000, 0, 0),
        None,
        ignore_seal,
    );
    assert!(!seed.older_trade);
    let new = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 12, 75.5, 4_050, 0, 0),
        None,
        ignore_seal,
    );
    assert!(new.advanced && !new.older_trade);
    let repeat = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 12, 75.5, 4_050, 7, 0),
        None,
        ignore_seal,
    );
    assert!(repeat.repeat_quote && !repeat.older_trade);
    let stale = agg.consume_tick(
        Feed::Dhan,
        &quote(OPEN + 11, 75.25, 4_020, 0, 0),
        None,
        ignore_seal,
    );
    assert!(stale.folded() && stale.older_trade, "an older real trade");
    let mut idx = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let _ = idx.consume_tick(
        Feed::Dhan,
        &tick(13, SEG_IDX, OPEN + 40, 23_146.45, 0),
        None,
        ignore_seal,
    );
    let same = idx.consume_tick(
        Feed::Dhan,
        &tick(13, SEG_IDX, OPEN + 40, 23_146.75, 0),
        None,
        ignore_seal,
    );
    assert!(!same.advanced && same.older_trade);
}
