use super::*;
use std::collections::BTreeMap;

const DAY: u32 = 1_779_321_600;
const OPEN: u32 = DAY + CANDLE_SESSION_OPEN_SECS_OF_DAY_IST;
const CLOSE: u32 = DAY + MARKET_CLOSE_SECS_OF_DAY_IST;

fn metadata() -> CandleMetadata {
    CandleMetadata {
        lot_size: 100,
        instrument_definition_version: 1,
        underlying_id: 17,
        family_code: 1,
    }
}

fn tick(at: u32, price: f32, cumulative: u32) -> ParsedTick {
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

fn fold(agg: &mut MultiTfAggregator, at: u32, price: f32, cumulative: u32) {
    let outcome = agg.consume_tick_with_context(
        Feed::Dhan,
        &tick(at, price, cumulative),
        None,
        metadata(),
        |_, _, _, _, _| {},
        |_| {},
    );
    assert!(outcome.folded());
}

#[test]
fn all_final_capture_windows_end_at_close_without_moving_the_nominal_grid() {
    for tf in TfIndex::ALL {
        let start = tf.bucket_start(CLOSE - 1);
        assert_eq!(tf.observation_window_end(start), Some(CLOSE), "{tf:?}");
        assert_eq!(tf.bucket_end(start), start + tf.seconds_per_bucket());
        assert_eq!(tf.bucket_start(start), start);
    }
    // M10 divides the 400-minute capture window exactly. Only these
    // active longer periods have a nominal final bucket beyond 15:40.
    assert_eq!(TfIndex::M10.bucket_start(CLOSE - 1), CLOSE - 600);
    assert_eq!(TfIndex::M10.bucket_end(CLOSE - 600), CLOSE);
    for tf in [TfIndex::M3, TfIndex::M15, TfIndex::M30, TfIndex::M60] {
        assert!(tf.bucket_end(tf.bucket_start(CLOSE - 1)) > CLOSE, "{tf:?}");
    }
    assert_eq!(
        crate::candles::CANDLE_OBSERVATION_WINDOW_POLICY,
        "regular_capture_window_v1"
    );
}

#[test]
fn an_invalid_grid_or_outside_capture_window_has_no_closure_authority() {
    for tf in TfIndex::ALL {
        assert_eq!(tf.observation_window_end(OPEN - 1), None, "{tf:?}");
        assert_eq!(tf.observation_window_end(CLOSE), None, "{tf:?}");
        assert_eq!(tf.observation_window_end(u32::MAX), None, "{tf:?}");
        if tf.seconds_per_bucket() > 1 {
            assert_eq!(tf.observation_window_end(OPEN + 1), None, "{tf:?}");
        }
    }
}

#[test]
fn explicit_expiry_closes_all_active_final_tails_without_freshening_data() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    fold(&mut agg, OPEN, 100.0, 0);
    fold(&mut agg, CLOSE - 1, 101.0, 100);
    let prior: BTreeMap<_, _> = TfIndex::ALL
        .into_iter()
        .map(|tf| (tf, agg.snapshot(Feed::Dhan, 77, 2, tf).unwrap()))
        .collect();

    assert_eq!(
        agg.seal_expired_observation_windows_with_volume_updates(
            CLOSE - 1,
            |_, _, _, _, _| panic!("the final observation second is still open"),
            |_| panic!("no closing publication before the admitted boundary"),
        ),
        0
    );
    let mut persisted = BTreeMap::new();
    let mut updates = BTreeMap::new();
    assert_eq!(
        agg.seal_expired_observation_windows_with_volume_updates(
            CLOSE,
            |_, _, _, tf, state| {
                persisted.insert(tf, state);
            },
            |update| {
                updates.insert(update.tf, update);
            },
        ),
        TF_COUNT
    );
    for tf in TfIndex::ALL {
        let update = updates.get(&tf).unwrap();
        let state = persisted.get(&tf).unwrap();
        let before = prior.get(&tf).unwrap();
        assert!(update.closed, "{tf:?}");
        assert_eq!(update.last_observed_secs, CLOSE - 1, "{tf:?}");
        assert_eq!(update.bucket_start_secs, before.bucket_start_ist_secs);
        assert_eq!(
            update.bucket_end_secs,
            tf.bucket_end(update.bucket_start_secs)
        );
        assert_eq!(update.gross_volume, before.volume);
        assert_eq!(update.estimated_net_volume, before.net_volume());
        assert_eq!(update.volume_quality, before.volume_quality);
        assert_eq!(update.metadata, before.metadata);
        assert_eq!(update.gross_volume, state.volume);
        assert_eq!(update.estimated_net_volume, state.net_volume());
        assert_eq!(update.revision, state.bucket_revision);
    }
    assert_eq!(agg.watermark_secs(), CLOSE - 1);
    assert_eq!(
        agg.seal_expired_observation_windows_with_volume_updates(
            CLOSE + 240,
            |_, _, _, _, _| panic!("unchanged state must not emit twice"),
            |_| panic!("a later timer must not refresh an unchanged publication"),
        ),
        0
    );
}

#[test]
fn a_shutdown_before_the_boundary_cannot_become_complete_when_time_passes() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    fold(&mut agg, OPEN, 100.0, 0);
    fold(&mut agg, CLOSE - 1, 101.0, 100);
    let mut partial = BTreeMap::new();
    assert_eq!(
        agg.force_seal_all_with_volume_updates(
            |_, _, _, _, _| {},
            |update| {
                partial.insert(update.tf, update);
            },
        ),
        TF_COUNT
    );
    assert!(partial.values().all(|update| !update.closed));
    assert_eq!(
        agg.seal_expired_observation_windows_with_volume_updates(
            CLOSE + 2,
            |_, _, _, _, _| panic!("the prematurely flushed state cannot be reconstructed"),
            |_| panic!("time alone cannot qualify a partial prior seal"),
        ),
        0
    );
    assert!(partial.values().all(|update| !update.closed));
}

#[test]
fn a_late_amendment_keeps_the_exact_prior_closure_and_its_original_source_clock() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    fold(&mut agg, OPEN, 100.0, 0);
    fold(&mut agg, CLOSE - 1, 101.0, 100);
    let mut closed = BTreeMap::new();
    agg.seal_expired_observation_windows_with_volume_updates(
        CLOSE + 2,
        |_, _, _, _, _| {},
        |update| {
            closed.insert(update.tf, update);
        },
    );
    let mut amended = BTreeMap::new();
    let outcome = agg.consume_tick_with_context(
        Feed::Dhan,
        &tick(CLOSE - 2, 102.0, 150),
        None,
        metadata(),
        |_, _, _, _, _| {},
        |update| {
            amended.insert(update.tf, update);
        },
    );
    assert!(outcome.amended_count > 0);
    for tf in [TfIndex::M10, TfIndex::M15, TfIndex::M30, TfIndex::M60] {
        let prior = closed.get(&tf).unwrap();
        let update = amended
            .get(&tf)
            .expect("the retained final tail remains amendable");
        assert_eq!(update.bucket_start_secs, prior.bucket_start_secs);
        assert!(
            update.closed,
            "a late source timestamp cannot reopen {tf:?}"
        );
        assert_eq!(update.last_observed_secs, prior.last_observed_secs);
        assert!(update.revision > prior.revision);
        assert!(
            !update.quality.is_eligible(),
            "late quantity attribution remains uncertain"
        );
    }
}

#[test]
fn closure_of_a_later_bucket_cannot_certify_an_earlier_administrative_partial() {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
    fold(&mut agg, OPEN, 100.0, 0);
    fold(&mut agg, OPEN + 59, 101.0, 100);
    let mut partial = None;
    agg.force_seal_all_with_volume_updates(
        |_, _, _, tf, state| {
            if tf == TfIndex::M1 {
                partial = Some(state);
            }
        },
        |_| {},
    );
    fold(&mut agg, OPEN + 60, 101.0, 100);
    fold(&mut agg, OPEN + 119, 102.0, 150);
    agg.seal_expired_observation_windows_with_volume_updates(
        OPEN + 120,
        |_, _, _, _, _| {},
        |_| {},
    );
    let slot = agg.slots.first_mut().unwrap();
    let old = slot.volume_update(TfIndex::M1, partial.unwrap(), None);
    assert_eq!(old.bucket_start_secs, OPEN);
    assert!(
        !old.closed,
        "a different exact bucket's proof cannot be borrowed"
    );
}

#[test]
fn a_late_new_final_bucket_needs_another_local_sweep_when_the_watermark_does_not_move() {
    const FIRST: u64 = 77;
    const SECOND: u64 = 88;

    fn fold_named(
        agg: &mut MultiTfAggregator,
        security_id: u64,
        at: u32,
        price: f32,
        cumulative: u32,
    ) {
        let mut observation = tick(at, price, cumulative);
        observation.security_id = security_id;
        let outcome = agg.consume_tick_with_context(
            Feed::Dhan,
            &observation,
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
        assert!(outcome.folded());
    }

    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 2);
    fold_named(&mut agg, FIRST, OPEN, 100.0, 0);
    fold_named(&mut agg, SECOND, OPEN, 100.0, 0);
    fold_named(&mut agg, FIRST, CLOSE - 4, 101.0, 100);
    fold_named(&mut agg, SECOND, CLOSE - 1, 101.0, 100);
    let unchanged_watermark = agg.watermark_secs();
    assert_eq!(unchanged_watermark, CLOSE - 1);

    // FIRST's earlier S1 has already reached the ordinary watermark cutoff.
    // Its eventual close-1 frame has been captured but has not yet been folded.
    let mut earlier_closed = false;
    agg.catch_up_seal_all_with_volume_updates(
        unchanged_watermark - 2,
        |_, _, _, _, _| {},
        |update| {
            if update.security_id == FIRST && update.tf == TfIndex::S1 {
                assert_eq!(update.bucket_start_secs, CLOSE - 4);
                earlier_closed = update.closed;
            }
        },
    );
    assert!(earlier_closed);

    let mut first_sweep = BTreeMap::new();
    agg.seal_expired_observation_windows_with_volume_updates(
        CLOSE + 2,
        |_, _, _, _, _| {},
        |update| {
            first_sweep.insert((update.security_id, update.tf), update);
        },
    );
    assert!(!first_sweep.contains_key(&(FIRST, TfIndex::S1)));
    assert!(first_sweep.get(&(SECOND, TfIndex::S1)).unwrap().closed);
    assert!(
        agg.snapshot(Feed::Dhan, FIRST, 2, TfIndex::S1)
            .unwrap()
            .is_uninitialised()
    );

    // A receiver/disk handoff can enqueue this captured frame after the first
    // sweep. It creates a NEW final S1, rather than amending FIRST's older S1.
    // SECOND already holds the same global watermark, so it does not advance.
    fold_named(&mut agg, FIRST, CLOSE - 1, 102.0, 150);
    assert_eq!(agg.watermark_secs(), unchanged_watermark);
    let before = agg.snapshot(Feed::Dhan, FIRST, 2, TfIndex::S1).unwrap();
    assert_eq!(before.bucket_start_ist_secs, CLOSE - 1);
    assert_eq!(before.volume, 50);
    assert_eq!(before.net_volume(), Some(50));
    assert_eq!(
        agg.catch_up_seal_all_with_volume_updates(
            unchanged_watermark - 2,
            |_, _, _, _, _| panic!("the unchanged watermark cannot expire the new final tail"),
            |_| panic!("ordinary catch-up must not fabricate a later cutoff"),
        ),
        0
    );

    let mut persisted = None;
    let mut published = None;
    agg.seal_expired_observation_windows_with_volume_updates(
        CLOSE + 3,
        |_, security_id, _, tf, state| {
            assert_eq!(security_id, FIRST, "SECOND has no new state to seal");
            if tf == TfIndex::S1 {
                persisted = Some(state);
            }
        },
        |update| {
            assert_eq!(update.security_id, FIRST);
            if update.tf == TfIndex::S1 {
                published = Some(update);
            }
        },
    );
    let persisted = persisted.expect("the late new candle reaches the persistence callback");
    let published = published.expect("the same late candle reaches the RAM callback");
    assert!(published.closed);
    assert_eq!(published.bucket_start_secs, before.bucket_start_ist_secs);
    assert_eq!(published.last_observed_secs, CLOSE - 1);
    assert_eq!(published.gross_volume, before.volume);
    assert_eq!(published.estimated_net_volume, before.net_volume());
    assert_eq!(published.volume_quality, before.volume_quality);
    assert_eq!(published.metadata, before.metadata);
    assert_eq!(published.gross_volume, persisted.volume);
    assert_eq!(published.estimated_net_volume, persisted.net_volume());
    assert_eq!(published.revision, persisted.bucket_revision);
    assert_eq!(agg.watermark_secs(), unchanged_watermark);
}
