use super::*;
use crate::candles::volume_update::{
    VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN, VOLUME_QUALITY_UNKNOWN_BASELINE,
};

const DAY: u32 = 1_779_321_600;
const OPEN: u32 = DAY + 33_300;

fn receipt(at: u32) -> i64 {
    (i64::from(at) - 19_800) * 1_000_000_000
}

fn tick(at: u32, received: u32, price: f32, cumulative: u32, quantity: u16) -> ParsedTick {
    ParsedTick {
        security_id: 39582,
        exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
        exchange_timestamp: at,
        received_at_nanos: receipt(received),
        last_traded_price: price,
        last_trade_quantity: quantity,
        volume: cumulative,
        volume_present: true,
        ..ParsedTick::default()
    }
}

fn metadata() -> CandleMetadata {
    CandleMetadata {
        lot_size: 600,
        instrument_definition_version: 1,
        underlying_id: 17,
        family_code: 1,
    }
}

fn consume(agg: &mut MultiTfAggregator, feed: Feed, tick: &ParsedTick) -> ConsumeStats {
    agg.consume_tick_with_context(feed, tick, None, metadata(), |_, _, _, _, _| {}, |_| {})
}

#[test]
fn observed_600_boundary_uses_ltt_for_quantity_and_receipt_for_freshness() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 9, OPEN + 9, 30.95, 46_800, 600),
    );
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 10, OPEN + 10, 32.25, 64_800, 600),
    );
    let mut boundary = tick(OPEN + 14, OPEN + 15, 32.60, 65_400, 600);
    boundary.received_at_nanos += 112_105_000;
    let mut update = None;
    let result = agg.consume_tick_with_context(
        Feed::Dhan,
        &boundary,
        None,
        metadata(),
        |_, _, _, _, _| {},
        |value| {
            if value.tf == TfIndex::S1 && value.bucket_start_secs == OPEN + 14 {
                update = Some(value);
            }
        },
    );
    assert!(result.folded());
    let update = update.expect("event-second publication");
    assert_eq!(update.gross_volume, 600);
    assert_eq!(update.bucket_start_secs, OPEN + 14);
    assert_eq!(update.last_observed_secs, OPEN + 15);
    assert!(
        !update.closed,
        "receipt crossing the boundary cannot seal the event bucket"
    );
    let five = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S5).unwrap();
    assert_eq!(five.bucket_start_ist_secs, OPEN + 10);
    assert_eq!(five.volume, 18_600);
    assert_eq!(five.close, 32.60);

    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 15, OPEN + 16, 33.35, 93_000, 600),
    );
    let next = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S5).unwrap();
    assert_eq!(next.bucket_start_ist_secs, OPEN + 15);
    assert_eq!(next.volume, 27_600);
}

#[test]
fn valid_opening_trade_counts_600_once_in_every_timeframe() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    let mut first = tick(OPEN, OPEN, 20.65, 600, 600);
    first.received_at_nanos += 67_930_000;
    assert!(consume(&mut agg, Feed::Dhan, &first).folded());
    assert!(consume(&mut agg, Feed::Dhan, &first).folded());
    for tf in TfIndex::ALL {
        let state = agg.snapshot(Feed::Dhan, 39582, 2, tf).unwrap();
        assert_eq!(
            state.volume, 600,
            "{tf:?}: count the observed first trade exactly once"
        );
        assert_eq!(state.signed_bar_volume(), None, "no guessed previous close");
        if state.bucket_start_ist_secs == OPEN {
            assert_eq!(state.volume_quality & VOLUME_QUALITY_UNKNOWN_BASELINE, 0);
        }
    }
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 1, OPEN + 1, 21.0, 1_200, 600),
    );
    assert_eq!(
        agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S1)
            .unwrap()
            .volume,
        600
    );
    assert_eq!(
        agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S5)
            .unwrap()
            .volume,
        1_200
    );
}

#[test]
fn opening_counter_after_price_only_packet_counts_its_first_trade_without_minting_a_sign() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    let mut price_only = tick(OPEN, OPEN, 20.65, 0, 0);
    price_only.volume_present = false;
    consume(&mut agg, Feed::Dhan, &price_only);
    consume(&mut agg, Feed::Dhan, &tick(OPEN, OPEN, 20.65, 600, 600));
    for tf in TfIndex::ALL {
        let state = agg.snapshot(Feed::Dhan, 39582, 2, tf).unwrap();
        assert_eq!(state.volume, 600);
        assert_eq!(state.signed_bar_volume(), None);
    }
}

#[test]
fn next_session_can_count_its_own_first_trade_without_carrying_the_previous_quantity() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(&mut agg, Feed::Dhan, &tick(OPEN, OPEN, 20.65, 600, 600));
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 1, OPEN + 1, 21.0, 1_200, 600),
    );
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 86_400, OPEN + 86_400, 19.0, 600, 600),
    );
    for tf in TfIndex::ALL {
        let state = agg.snapshot(Feed::Dhan, 39582, 2, tf).unwrap();
        assert_eq!(state.volume, 600);
        assert_eq!(
            state.signed_bar_volume(),
            None,
            "previous session cannot supply direction"
        );
    }
}

#[test]
fn ordinary_restart_and_unproved_opening_snapshots_remain_baseline_only() {
    let ordinary = tick(OPEN, OPEN, 20.65, 600, 600);
    let mut missing = ordinary;
    missing.volume_present = false;
    let mut no_receipt = ordinary;
    no_receipt.received_at_nanos = 0;
    let mut before_receipt = ordinary;
    before_receipt.received_at_nanos -= 1;
    let mut zero_quantity = ordinary;
    zero_quantity.last_trade_quantity = 0;
    for candidate in [
        tick(OPEN + 60, OPEN + 60, 20.65, 600, 600),
        tick(OPEN, OPEN + 1, 20.65, 600, 600),
        tick(OPEN, OPEN + 60, 20.65, 600, 600),
        tick(OPEN, OPEN, 20.65, 1_200, 600),
        missing,
        no_receipt,
        before_receipt,
        zero_quantity,
    ] {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        assert!(consume(&mut agg, Feed::Dhan, &candidate).folded());
        let state = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S1).unwrap();
        assert_eq!(state.volume, 0);
        assert_ne!(state.volume_quality & VOLUME_QUALITY_UNKNOWN_BASELINE, 0);
    }
}

#[test]
fn opening_exception_does_not_apply_to_other_feeds_or_other_session_segments() {
    for (feed, segment) in [
        (Feed::Truedata, EXCHANGE_SEGMENT_NSE_FNO),
        (
            Feed::Dhan,
            tickvault_common::constants::EXCHANGE_SEGMENT_MCX_COMM,
        ),
        (
            Feed::Dhan,
            tickvault_common::constants::EXCHANGE_SEGMENT_NSE_CURRENCY,
        ),
        (
            Feed::Dhan,
            tickvault_common::constants::EXCHANGE_SEGMENT_IDX_I,
        ),
    ] {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let mut candidate = tick(OPEN, OPEN, 20.65, 600, 600);
        candidate.exchange_segment_code = segment;
        assert!(consume(&mut agg, feed, &candidate).folded());
        assert_eq!(
            agg.snapshot(feed, 39582, segment, TfIndex::S1)
                .unwrap()
                .volume,
            0
        );
    }
}

#[test]
fn opening_receipt_never_qualifies_a_stale_or_future_session() {
    for event in [OPEN - 86_400, OPEN + 86_400] {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let candidate = tick(event, OPEN, 20.65, 600, 600);
        let result = consume(&mut agg, Feed::Dhan, &candidate);
        assert!(!result.folded());
        assert!(result.stale_trading_day || result.future_trading_day);
        assert!(agg.is_empty());
    }
}

#[test]
fn delayed_close_trade_stays_in_its_event_window() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    let at = DAY + MARKET_CLOSE_SECS_OF_DAY_IST - 30;
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(at - 30, at - 30, 100.0, 1_000, 100),
    );
    let result = consume(&mut agg, Feed::Dhan, &tick(at, at + 35, 101.0, 1_600, 600));
    assert!(result.folded());
    let state = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::M1).unwrap();
    assert_eq!(state.bucket_start_ist_secs, at - 30);
    assert_eq!(state.volume, 600);
    assert_eq!(state.close, 101.0);
}

#[test]
fn receipt_cannot_move_a_trade_across_either_capture_window_edge() {
    for (event, arrival) in [
        (
            DAY + CANDLE_SESSION_OPEN_SECS_OF_DAY_IST - 1,
            DAY + CANDLE_SESSION_OPEN_SECS_OF_DAY_IST + 1,
        ),
        (
            DAY + MARKET_CLOSE_SECS_OF_DAY_IST,
            DAY + MARKET_CLOSE_SECS_OF_DAY_IST - 1,
        ),
    ] {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let result = consume(&mut agg, Feed::Dhan, &tick(event, arrival, 100.0, 600, 600));
        assert!(result.out_of_session);
        assert!(!result.folded());
        assert!(agg.is_empty());
    }
}

#[test]
fn same_bucket_event_regression_preserves_latest_close_and_marks_quantity_uncertain() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 20, OPEN + 25, 100.0, 1_000, 100),
    );
    consume(
        &mut agg,
        Feed::Dhan,
        &tick(OPEN + 5, OPEN + 45, 107.0, 1_100, 100),
    );
    let state = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::M1).unwrap();
    assert_eq!(state.close, 100.0);
    assert_eq!(state.volume, 100);
    assert_ne!(
        state.volume_quality & VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN,
        0
    );
    assert_eq!(agg.last_ltp(Feed::Dhan, 39582, 2), Some(100.0));
}
