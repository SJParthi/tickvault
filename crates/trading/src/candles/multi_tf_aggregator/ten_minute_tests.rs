use super::*;
use std::collections::BTreeMap;

const DAY: u32 = 1_779_321_600;
const START: u32 = DAY + 9 * 3_600 + 20 * 60;

fn metadata() -> CandleMetadata {
    CandleMetadata {
        lot_size: 50,
        instrument_definition_version: 7,
        underlying_id: 17,
        family_code: 1,
    }
}

fn observation(at: u32, price: f32, cumulative: u32) -> ParsedTick {
    ParsedTick {
        security_id: 77,
        exchange_segment_code: 2,
        exchange_timestamp: at,
        received_at_nanos: (i64::from(at) - 19_800) * 1_000_000_000,
        last_traded_price: price,
        volume: cumulative,
        volume_present: true,
        ..ParsedTick::default()
    }
}

#[test]
fn ten_minute_rollover_publishes_the_canonical_quantity_without_a_second_calculation() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 1);
    let mut persisted = BTreeMap::new();
    let mut published = BTreeMap::new();
    for (at, price, cumulative) in [
        (START - 1, 100.0, 1_000),
        (START, 101.0, 1_010),
        (START + 599, 100.0, 1_020),
        (START + 600, 101.0, 1_030),
    ] {
        let outcome = agg.consume_tick_with_context(
            Feed::Dhan,
            &observation(at, price, cumulative),
            None,
            metadata(),
            |_, _, _, tf, state| {
                persisted.insert((tf, state.bucket_start_ist_secs), state);
            },
            |update| {
                published.insert((update.tf, update.bucket_start_secs), update);
            },
        );
        assert!(outcome.folded());
        let state = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M10).unwrap();
        let update = published
            .get(&(TfIndex::M10, state.bucket_start_ist_secs))
            .unwrap();
        assert_eq!(update.gross_volume, state.volume);
        assert_eq!(update.estimated_net_volume, state.net_volume());
        assert_eq!(update.metadata, state.metadata);
        assert_eq!(update.volume_quality, state.volume_quality);
        assert_eq!(update.revision, state.bucket_revision);
        assert_eq!(update.bucket_end_secs, state.bucket_start_ist_secs + 600);
        assert_eq!(update.last_observed_secs, at);
    }

    let predecessor = persisted.get(&(TfIndex::M10, START - 600)).unwrap();
    assert_eq!(
        predecessor.volume, 0,
        "the initial snapshot seeds the quantity baseline"
    );
    assert_eq!(predecessor.close, 100.0);
    assert_eq!(
        predecessor.net_volume(),
        None,
        "this initial candle has no predecessor"
    );
    let first = persisted.get(&(TfIndex::M10, START)).unwrap();
    assert_eq!(
        first.volume, 20,
        "both ten-unit increments contribute to whole candle volume"
    );
    assert_eq!(first.bucket_open_prev_close, predecessor.close);
    assert_eq!(first.close, predecessor.close);
    assert_eq!(
        first.net_volume(),
        Some(0),
        "equal consecutive 10m closes produce a known zero signed signal"
    );
    assert_eq!(first.metadata, metadata());
    let first_update = published.get(&(TfIndex::M10, START)).unwrap();
    assert!(first_update.closed);
    assert_eq!(first_update.gross_volume, first.volume);
    assert_eq!(first_update.estimated_net_volume, first.net_volume());
    assert_eq!(first_update.revision, first.bucket_revision);
    let next = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M10).unwrap();
    assert_eq!(next.bucket_start_ist_secs, START + 600);
    assert_eq!(next.volume, 10);
    assert_eq!(next.net_volume(), Some(10));

    agg.force_seal_all(|_, _, _, tf, state| {
        persisted.insert((tf, state.bucket_start_ist_secs), state);
    });
    for tf in TfIndex::ALL {
        let gross: u64 = persisted
            .iter()
            .filter_map(|((frame, _), state)| (*frame == tf).then_some(state.volume))
            .sum();
        assert_eq!(gross, 30, "received quantity ledger for {tf:?}");
    }
}

#[test]
fn ten_minute_late_carry_survives_draining_and_counter_ambiguity_once() {
    use crate::candles::volume_update::VOLUME_QUALITY_COUNTER_AMBIGUOUS;

    for strategy in [FeedStrategy::REFOLD, FeedStrategy::DISCARD] {
        for catch_up in [false, true] {
            let mut agg = MultiTfAggregator::with_capacity(strategy, 1);
            let mut persisted = BTreeMap::new();
            for (at, price, cumulative) in [(START, 100.0, 1_000), (START + 600, 101.0, 1_100)] {
                agg.consume_tick_with_context(
                    Feed::Dhan,
                    &observation(at, price, cumulative),
                    None,
                    metadata(),
                    |_, _, _, tf, state| {
                        persisted.insert((tf, state.bucket_start_ist_secs), state);
                    },
                    |_| {},
                );
            }
            if catch_up {
                agg.catch_up_seal_all(START + 1_200, |_, _, _, tf, state| {
                    persisted.insert((tf, state.bucket_start_ist_secs), state);
                });
            }
            assert_eq!(
                agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M10)
                    .unwrap()
                    .is_uninitialised(),
                catch_up,
                "the optional drain must actually exercise the empty 10m slot"
            );

            // The late +50 is real accepted counter growth. The subsequent
            // same-session decrease has no provider reset proof, so neither
            // 10 nor 20 may be invented as another quantity axis.
            for (at, price, cumulative) in [
                (START + 10, 102.0, 1_150),
                (START + 601, 103.0, 10),
                (START + 1_201, 104.0, 20),
            ] {
                agg.consume_tick_with_context(
                    Feed::Dhan,
                    &observation(at, price, cumulative),
                    None,
                    metadata(),
                    |_, _, _, tf, state| {
                        persisted.insert((tf, state.bucket_start_ist_secs), state);
                    },
                    |_| {},
                );
            }
            let current = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M10).unwrap();
            assert_ne!(current.volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS, 0);
            assert_eq!(current.net_volume(), None);
            agg.force_seal_all(|_, _, _, tf, state| {
                persisted.insert((tf, state.bucket_start_ist_secs), state);
            });
            for tf in TfIndex::ALL {
                let gross: u64 = persisted
                    .iter()
                    .filter(|((frame, _), _)| *frame == tf)
                    .map(|(_, state)| state.volume)
                    .sum();
                assert_eq!(gross, 150, "{strategy:?}, catch_up {catch_up}, {tf:?}");
            }
        }
    }
}
