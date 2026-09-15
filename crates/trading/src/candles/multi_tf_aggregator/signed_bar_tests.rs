use super::*;
use std::collections::BTreeMap;

const DAY: u32 = 1_779_321_600;
const OPEN: u32 = DAY + 33_300;

fn tick(at: u32, price: f32, cumulative: u32) -> ParsedTick {
    ParsedTick {
        security_id: 39582,
        exchange_segment_code: 2,
        exchange_timestamp: at,
        received_at_nanos: (i64::from(at) - 19_800) * 1_000_000_000,
        last_traded_price: price,
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

#[test]
fn five_second_rollovers_publish_the_same_whole_bar_cache_as_the_candle() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    let mut seals = BTreeMap::new();
    let mut updates = BTreeMap::new();
    // First snapshot remains a quantity baseline. The later candle magnitudes
    // are explicit fixtures, independent of the actual live timestamp issue.
    for (offset, price, cumulative) in [
        (0, 20.65, 600),
        (5, 31.60, 1_000),
        (6, 30.95, 46_800),
        (10, 32.60, 65_400),
        (15, 33.35, 93_000),
        (20, 32.40, 123_600),
        (25, 34.50, 163_200),
        (30, 34.50, 163_200),
    ] {
        agg.consume_tick_with_context(
            Feed::Dhan,
            &tick(OPEN + offset, price, cumulative),
            None,
            metadata(),
            |_, _, _, tf, state| {
                seals.insert((tf, state.bucket_start_ist_secs), state);
            },
            |update| {
                updates.insert((update.tf, update.bucket_start_secs), update);
            },
        );
    }
    let first = seals.get(&(TfIndex::S5, OPEN)).expect("opening candle");
    assert_eq!(
        first.volume, 0,
        "first observed cumulative remains only a baseline"
    );
    assert_eq!(first.signed_bar_volume(), None, "no prior same-TF close");
    for (offset, gross, signed) in [
        (5, 46_200, 46_200),
        (10, 18_600, 18_600),
        (15, 27_600, 27_600),
        (20, 30_600, -30_600),
        (25, 39_600, 39_600),
    ] {
        let key = (TfIndex::S5, OPEN + offset);
        let candle = seals.get(&key).expect("sealed five-second candle");
        let update = updates.get(&key).expect("same candle publication");
        assert_eq!(candle.volume, gross);
        assert_eq!(candle.signed_bar_volume(), Some(signed));
        assert_eq!(update.gross_volume, gross);
        assert_eq!(update.estimated_net_volume, Some(signed));
        assert_eq!(update.volume_quality, candle.volume_quality);
        assert_eq!(update.revision, candle.bucket_revision);
    }
    let red = seals.get(&(TfIndex::S5, OPEN + 5)).unwrap();
    assert!(red.close < red.open);
    assert_eq!(red.signed_bar_volume(), Some(46_200));
}

#[test]
fn a_new_session_never_reuses_yesterdays_direction_baseline() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    for (at, price, cumulative) in [
        (OPEN, 100.0, 0),
        (OPEN + 5, 101.0, 50),
        (OPEN + 86_400, 99.0, 0),
        (OPEN + 86_401, 98.0, 30),
    ] {
        agg.consume_tick_with_context(
            Feed::Dhan,
            &tick(at, price, cumulative),
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
    }
    let candle = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S5).unwrap();
    assert_eq!(candle.volume, 30);
    assert_eq!(candle.bucket_open_prev_close, 0.0);
    assert_eq!(candle.signed_bar_volume(), None);
}

#[test]
fn an_unchanged_close_is_zero_even_when_intrabar_prices_change() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    for (offset, price, cumulative) in
        [(0, 100.0, 0), (5, 102.0, 10), (6, 99.0, 40), (7, 100.0, 60)]
    {
        agg.consume_tick_with_context(
            Feed::Dhan,
            &tick(OPEN + offset, price, cumulative),
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
    }
    let candle = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S5).unwrap();
    assert_eq!(candle.volume, 60);
    assert_eq!(candle.signed_bar_volume(), Some(0));
}

#[test]
fn a_late_close_amendment_revises_its_own_sign_and_keeps_successor_baseline_frozen() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 1);
    for (offset, price, cumulative) in [(0, 100.0, 0), (5, 101.0, 20), (10, 102.0, 30)] {
        agg.consume_tick_with_context(
            Feed::Dhan,
            &tick(OPEN + offset, price, cumulative),
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
    }
    let before = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S5).unwrap();
    assert_eq!(before.bucket_open_prev_close, 101.0);
    assert_eq!(before.signed_bar_volume(), Some(10));
    let mut amended = None;
    let mut publication = None;
    // Same accepted cumulative: the fixture amends price without creating a
    // counter regression. Its trusted receipt belongs to the preceding bucket.
    agg.consume_tick_with_context(
        Feed::Dhan,
        &tick(OPEN + 9, 99.0, 30),
        None,
        metadata(),
        |_, _, _, tf, state| {
            if tf == TfIndex::S5 && state.bucket_start_ist_secs == OPEN + 5 {
                amended = Some(state);
            }
        },
        |update| {
            if update.tf == TfIndex::S5 && update.bucket_start_secs == OPEN + 5 {
                publication = Some(update);
            }
        },
    );
    let amended = amended.expect("retained previous candle is amended");
    assert_eq!(amended.volume, 20);
    assert_eq!(amended.signed_bar_volume(), Some(-20));
    assert_eq!(publication.unwrap().estimated_net_volume, Some(-20));
    let current = agg.snapshot(Feed::Dhan, 39582, 2, TfIndex::S5).unwrap();
    assert_eq!(current.bucket_open_prev_close, 101.0);
    assert_eq!(current.signed_bar_volume(), Some(10));
}
