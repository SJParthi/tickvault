#![cfg(test)]

use super::*;
use std::collections::BTreeMap;

use crate::candles::volume_update::{
    MAX_VOLUME_UPDATES_PER_TICK, VOLUME_QUALITY_COUNTER_AMBIGUOUS,
    VOLUME_QUALITY_MISSING_OBSERVATION,
};

const DAY: u32 = 1_779_321_600;
const OPEN: u32 = DAY + MARKET_OPEN_SECS_OF_DAY_IST;
// All ten configured frames share this boundary on the 09:00 grid.
const BOUNDARY: u32 = DAY + 10 * 3_600;

fn tick(at: u32, cumulative: u32, quantity: u16) -> ParsedTick {
    ParsedTick {
        security_id: 39582,
        exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
        exchange_timestamp: at,
        received_at_nanos: (i64::from(at) - 19_800) * 1_000_000_000,
        last_traded_price: 100.0,
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

fn consume(agg: &mut MultiTfAggregator, value: &ParsedTick) {
    assert!(
        agg.consume_tick_with_context(
            Feed::Dhan,
            value,
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        )
        .folded()
    );
}

fn current(agg: &MultiTfAggregator, tf: TfIndex) -> LiveCandleState {
    agg.snapshot(Feed::Dhan, 39582, EXCHANGE_SEGMENT_NSE_FNO, tf)
        .unwrap()
}

fn uncertain(state: LiveCandleState) -> bool {
    state.volume_quality & VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN != 0
}

#[test]
fn observed_17400_to_21000_jump_flags_only_frames_whose_boundary_it_crosses() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(&mut agg, &tick(OPEN, 0, 0));
    consume(&mut agg, &tick(OPEN + 4, 17_400, 17_400));
    let before: BTreeMap<_, _> = TfIndex::ALL
        .into_iter()
        .map(|tf| (tf, current(&agg, tf)))
        .collect();
    let mut seals = BTreeMap::new();
    let mut updates = BTreeMap::new();
    agg.consume_tick_with_context(
        Feed::Dhan,
        &tick(OPEN + 5, 21_000, 600),
        None,
        metadata(),
        |_, _, _, tf, state| {
            seals.insert(tf, state);
        },
        |update| {
            updates.insert((update.tf, update.bucket_start_secs), update);
        },
    );
    for tf in TfIndex::ALL {
        let crossed = matches!(tf, TfIndex::S1 | TfIndex::S5);
        let state = current(&agg, tf);
        assert_eq!(uncertain(state), crossed, "{tf:?}");
        assert_eq!(
            state.volume,
            if crossed {
                3_600
            } else {
                before[&tf].volume + 3_600
            },
            "all accepted gross quantity is retained for {tf:?}"
        );
        if crossed {
            let previous = seals[&tf];
            assert!(
                uncertain(previous),
                "the outgoing {tf:?} row is also uncertain"
            );
            assert_eq!(previous.volume, before[&tf].volume);
            assert_eq!(
                previous.signed_bar_volume(),
                if tf == TfIndex::S1 { Some(0) } else { None },
                "the first 5s bar has no predecessor; the later 1s bar has an equal close"
            );
            assert_eq!(state.signed_bar_volume(), Some(0));
            for state in [previous, state] {
                let update = updates[&(tf, state.bucket_start_ist_secs)];
                assert_eq!(update.gross_volume, state.volume);
                assert_eq!(update.volume_quality, state.volume_quality);
                assert!(update.quality.attribution_uncertain);
                assert!(!update.quality.is_eligible());
                assert_eq!(
                    update.estimated_net_volume,
                    state.signed_bar_volume(),
                    "preserve the diagnostic projection; quality refuses ranking"
                );
            }
        }
    }
}

#[test]
fn every_configured_frame_refuses_a_multitrade_jump_across_its_boundary() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(&mut agg, &tick(BOUNDARY - 3, 1_000, 0));
    consume(&mut agg, &tick(BOUNDARY - 1, 1_100, 100));
    let mut outgoing = BTreeMap::new();
    agg.consume_tick_with_context(
        Feed::Dhan,
        &tick(BOUNDARY, 4_700, 600),
        None,
        metadata(),
        |_, _, _, tf, state| {
            outgoing.insert(tf, state);
        },
        |_| {},
    );
    assert_eq!(outgoing.len(), TF_COUNT);
    for tf in TfIndex::ALL {
        assert!(uncertain(outgoing[&tf]), "outgoing {tf:?}");
        assert_eq!(outgoing[&tf].volume, 100);
        let state = current(&agg, tf);
        assert!(uncertain(state), "incoming {tf:?}");
        assert_eq!(state.volume, 3_600);
    }
}

#[test]
fn only_a_matching_nonzero_last_trade_quantity_locates_a_positive_boundary_increment() {
    for (increment, quantity, expected_uncertain) in [
        (3_600, 3_600, false),
        (3_600, 600, true),
        (3_600, 0, true),
        (3_600, 3_601, true),
        (0, 0, false),
        (0, 600, false),
    ] {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        consume(&mut agg, &tick(BOUNDARY - 1, 17_400, 600));
        consume(&mut agg, &tick(BOUNDARY, 17_400 + increment, quantity));
        for tf in TfIndex::ALL {
            let state = current(&agg, tf);
            assert_eq!(state.volume, u64::from(increment));
            assert_eq!(
                uncertain(state),
                expected_uncertain,
                "{tf:?}: {increment}/{quantity}"
            );
        }
    }
}

#[test]
fn multiple_trades_within_one_bucket_do_not_create_cross_boundary_uncertainty() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(&mut agg, &tick(BOUNDARY + 1, 1_000, 100));
    consume(&mut agg, &tick(BOUNDARY + 1, 4_600, 0));
    for tf in TfIndex::ALL {
        assert_eq!(current(&agg, tf).volume, 3_600);
        assert!(!uncertain(current(&agg, tf)), "{tf:?}");
    }
}

#[test]
fn price_only_packets_cannot_shorten_the_counter_interval_or_leave_an_old_row_certified() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(&mut agg, &tick(BOUNDARY - 1, 1_000, 100));
    let mut seals = BTreeMap::new();
    for at in [BOUNDARY, BOUNDARY + 1, BOUNDARY + 2] {
        let mut missing = tick(at, 0, 0);
        missing.volume_present = false;
        agg.consume_tick_with_context(
            Feed::Dhan,
            &missing,
            None,
            metadata(),
            |_, _, _, tf, state| {
                seals.insert((tf, state.bucket_start_ist_secs), state);
            },
            |_| {},
        );
        let index = agg
            .lookup(Feed::Dhan, 39582, EXCHANGE_SEGMENT_NSE_FNO)
            .unwrap();
        assert_eq!(
            agg.slots[index].last_counter_observed_secs,
            Some(BOUNDARY - 1)
        );
    }
    for tf in TfIndex::ALL {
        let old = seals[&(tf, tf.bucket_start(BOUNDARY - 1))];
        assert!(
            uncertain(old),
            "outgoing {tf:?} flagged before retention can evict it"
        );
    }
    consume(&mut agg, &tick(BOUNDARY + 3, 4_600, 600));
    let state = current(&agg, TfIndex::S5);
    assert_eq!(state.volume, 3_600);
    assert!(uncertain(state));
    assert_ne!(state.volume_quality & VOLUME_QUALITY_MISSING_OBSERVATION, 0);
    // A subsequent well-located trade cannot clear this bucket's history.
    consume(&mut agg, &tick(BOUNDARY + 4, 5_200, 600));
    assert_eq!(current(&agg, TfIndex::S5).volume, 4_200);
    assert!(uncertain(current(&agg, TfIndex::S5)));
}

#[test]
fn a_gap_after_timer_seal_republishes_the_retained_outgoing_row_without_changing_quantity() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(&mut agg, &tick(BOUNDARY - 1, 17_400, 600));
    let mut original = BTreeMap::new();
    agg.catch_up_seal_all_with_volume_updates(
        BOUNDARY,
        |_, _, _, tf, state| {
            original.insert(tf, state);
        },
        |_| {},
    );
    assert_eq!(original.len(), TF_COUNT);
    let mut persisted = BTreeMap::new();
    let mut publications = Vec::new();
    let stats = agg.consume_tick_with_context(
        Feed::Dhan,
        &tick(BOUNDARY + 30, 21_000, 600),
        None,
        metadata(),
        |_, _, _, tf, state| {
            persisted.insert(tf, state);
        },
        |update| publications.push(update),
    );
    assert_eq!(usize::from(stats.amended_count), TF_COUNT);
    assert!(publications.len() <= MAX_VOLUME_UPDATES_PER_TICK);
    for tf in TfIndex::ALL {
        let state = persisted[&tf];
        assert_eq!(
            state.bucket_start_ist_secs,
            original[&tf].bucket_start_ist_secs
        );
        assert_eq!(state.volume, original[&tf].volume);
        assert_eq!(state.close, original[&tf].close);
        assert!(uncertain(state));
        assert!(state.bucket_revision > original[&tf].bucket_revision);
        let publication = publications
            .iter()
            .find(|update| {
                update.tf == tf && update.bucket_start_secs == state.bucket_start_ist_secs
            })
            .unwrap();
        assert!(
            publication.closed,
            "existing timer qualification is retained"
        );
        assert!(!publication.quality.is_eligible());
        assert!(uncertain(current(&agg, tf)));
        assert_eq!(current(&agg, tf).volume, 3_600);
    }
    // A duplicate cannot fabricate another quantity or endlessly re-amend the old row.
    let stats = agg.consume_tick_with_context(
        Feed::Dhan,
        &tick(BOUNDARY + 30, 21_000, 600),
        None,
        metadata(),
        |_, _, _, _, _| panic!("an unchanged old flag is not a new amendment"),
        |_| {},
    );
    assert_eq!(stats.amended_count, 0);
}

#[test]
fn late_counter_advances_and_rejected_regressions_preserve_gross_and_sticky_uncertainty() {
    for strategy in [FeedStrategy::REFOLD, FeedStrategy::DISCARD] {
        let mut agg = MultiTfAggregator::with_capacity(strategy, 1);
        let mut persisted = BTreeMap::new();
        for (at, counter, quantity) in [
            (BOUNDARY - 2, 1_000, 0),
            (BOUNDARY - 1, 1_100, 100),
            (BOUNDARY + 1, 1_200, 100),
            (BOUNDARY - 1, 1_500, 100),
            (BOUNDARY + 3, 1_700, 100),
            (BOUNDARY + 4, 900, 100),
            (BOUNDARY + 5, 1_800, 100),
        ] {
            let mut value = tick(at, counter, quantity);
            if counter == 1_500 {
                value.received_at_nanos = tick(BOUNDARY + 2, 0, 0).received_at_nanos;
            }
            let mut count = 0;
            agg.consume_tick_with_context(
                Feed::Dhan,
                &value,
                None,
                metadata(),
                |_, _, _, tf, state| {
                    persisted.insert((tf, state.bucket_start_ist_secs), state);
                },
                |_| count += 1,
            );
            assert!(count <= MAX_VOLUME_UPDATES_PER_TICK);
            let index = agg
                .lookup(Feed::Dhan, 39582, EXCHANGE_SEGMENT_NSE_FNO)
                .unwrap();
            if counter == 1_500 {
                assert_eq!(
                    agg.slots[index].last_counter_observed_secs,
                    Some(BOUNDARY - 1)
                );
            }
            if counter == 900 {
                assert_eq!(
                    agg.slots[index].last_counter_observed_secs,
                    Some(BOUNDARY + 3)
                );
            }
        }
        assert!(uncertain(current(&agg, TfIndex::M1)));
        for tf in TfIndex::ALL {
            assert_ne!(
                current(&agg, tf).volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS,
                0
            );
            assert_eq!(current(&agg, tf).signed_bar_volume(), None);
        }
        agg.force_seal_all(|_, _, _, tf, state| {
            persisted.insert((tf, state.bucket_start_ist_secs), state);
        });
        for tf in TfIndex::ALL {
            let gross: u64 = persisted
                .iter()
                .filter_map(|((frame, _), state)| (*frame == tf).then_some(state.volume))
                .sum();
            assert_eq!(
                gross, 800,
                "{strategy:?}/{tf:?}: preserve the accepted counter ledger"
            );
        }
    }
}

#[test]
fn a_positive_counter_regression_also_revokes_a_timer_sealed_winner() {
    for strategy in [FeedStrategy::REFOLD, FeedStrategy::DISCARD] {
        let mut agg = MultiTfAggregator::with_capacity(strategy, 1);
        consume(&mut agg, &tick(BOUNDARY - 1, 1_000, 100));
        let mut traded = tick(BOUNDARY, 1_100, 100);
        traded.last_traded_price = 101.0;
        consume(&mut agg, &traded);
        assert_eq!(current(&agg, TfIndex::S1).volume_quality, 0);
        agg.catch_up_seal_all(BOUNDARY + 1, |_, _, _, _, _| {});
        let mut old = None;
        agg.consume_tick_with_context(
            Feed::Dhan,
            &tick(BOUNDARY + 1, 900, 100),
            None,
            metadata(),
            |_, _, _, _, _| {},
            |update| {
                if update.tf == TfIndex::S1 && update.bucket_start_secs == BOUNDARY {
                    old = Some(update);
                }
            },
        );
        let old = old.expect("the existing winner is explicitly revised");
        assert!(old.closed);
        assert_eq!(old.gross_volume, 100);
        assert_ne!(old.volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS, 0);
        assert!(old.quality.attribution_uncertain);
        assert!(!old.quality.is_eligible());
        let index = agg
            .lookup(Feed::Dhan, 39582, EXCHANGE_SEGMENT_NSE_FNO)
            .unwrap();
        assert_eq!(agg.slots[index].last_counter_observed_secs, Some(BOUNDARY));
    }
}

#[test]
fn a_new_session_does_not_inherit_the_previous_counter_interval() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    consume(&mut agg, &tick(BOUNDARY - 1, 1_000, 100));
    consume(&mut agg, &tick(BOUNDARY, 4_600, 600));
    for tf in TfIndex::ALL {
        assert!(uncertain(current(&agg, tf)));
    }
    consume(&mut agg, &tick(OPEN + 86_400, 600, 600));
    for tf in TfIndex::ALL {
        assert_eq!(current(&agg, tf).volume, 600);
        assert!(
            !uncertain(current(&agg, tf)),
            "the new session has its own evidence: {tf:?}"
        );
    }
    let index = agg
        .lookup(Feed::Dhan, 39582, EXCHANGE_SEGMENT_NSE_FNO)
        .unwrap();
    assert_eq!(
        agg.slots[index].last_counter_observed_secs,
        Some(OPEN + 86_400)
    );
    agg.force_seal_all(|_, _, _, _, _| {});
    assert_eq!(agg.slots[index].last_counter_observed_secs, None);
}

#[test]
fn the_other_feed_keeps_its_existing_clock_and_volume_attribution_contract() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    for value in [tick(BOUNDARY - 1, 1_000, 0), tick(BOUNDARY, 4_600, 0)] {
        agg.consume_tick_with_context(
            Feed::Truedata,
            &value,
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
    }
    for tf in TfIndex::ALL {
        let state = agg
            .snapshot(Feed::Truedata, 39582, EXCHANGE_SEGMENT_NSE_FNO, tf)
            .unwrap();
        assert_eq!(state.volume, 3_600);
        assert!(!uncertain(state), "{tf:?}");
    }
}
