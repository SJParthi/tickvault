use super::*;
use proptest::prelude::*;
use tickvault_trading::candles::{CandleMetadata, TF_COUNT};

const DAY: u32 = 20_000;
const EPOCH: u64 = 41;
const DEFINITION: u64 = 19;
const METRIC: BucketVolumeMetric = BucketVolumeMetric::SignedEstimatedNetVsOneLotV2;

fn contract(id: u64, family: OptionFamily, lot: u32) -> BucketContract {
    BucketContract {
        security_id: id,
        segment: ExchangeSegment::NseFno,
        underlying_id: id + 1_000,
        family,
        lot_size: lot,
        instrument_definition_version: DEFINITION,
    }
}

fn known_quality() -> VolumeQuality {
    VolumeQuality {
        baseline_known: true,
        counter_ambiguous: false,
        volume_missing: false,
        attribution_uncertain: false,
    }
}

fn start(tf: TfIndex) -> u32 {
    tf.bucket_start(DAY * 86_400 + 9 * 3_600 + 15 * 60)
}

fn update(
    metadata: BucketContract,
    tf: TfIndex,
    gross: u64,
    net: Option<i64>,
    revision: u64,
) -> CandleVolumeUpdate {
    let bucket_start_secs = start(tf);
    CandleVolumeUpdate {
        feed: Feed::Dhan,
        security_id: metadata.security_id,
        segment_code: metadata.segment.binary_code(),
        tf,
        session_day: DAY,
        bucket_start_secs,
        bucket_end_secs: tf.bucket_end(bucket_start_secs),
        gross_volume: gross,
        estimated_net_volume: net,
        revision,
        last_observed_secs: bucket_start_secs,
        quality: known_quality(),
        closed: false,
        metadata: CandleMetadata {
            lot_size: metadata.lot_size,
            instrument_definition_version: metadata.instrument_definition_version,
            underlying_id: metadata.underlying_id,
            family_code: match metadata.family {
                OptionFamily::Stock => 1,
                OptionFamily::Index => 2,
            },
        },
        volume_quality: 0,
    }
}

fn engine(contracts: Vec<BucketContract>) -> BucketTopVolumeEngine {
    BucketTopVolumeEngine::new(Feed::Dhan, DAY, EPOCH, METRIC, contracts, 2).unwrap()
}

#[test]
fn signed_bar_v3_orders_full_candle_magnitudes_without_tick_flow_cancellation() {
    let metric = BucketVolumeMetric::SignedBarVolumeVsOneLotV3;
    assert_eq!(metric.as_str(), "signed_bar_volume_vs_one_lot_v3");
    assert_ne!(metric.as_str(), METRIC.as_str());
    for tf in TfIndex::TOP_VOLUME_ALL {
        let positive = contract(39_582, OptionFamily::Stock, 600);
        let negative = contract(39_583, OptionFamily::Stock, 600);
        let mut board =
            BucketTopVolumeEngine::new(Feed::Dhan, DAY, EPOCH, metric, [positive, negative], 2)
                .unwrap();
        board
            .apply(&update(positive, tf, 39_600, Some(39_600), 1))
            .unwrap();
        board
            .apply(&update(negative, tf, 30_600, Some(-30_600), 1))
            .unwrap();
        let snapshot = board
            .snapshot(OptionFamily::Stock, tf, start(tf), start(tf))
            .unwrap();
        assert_eq!(snapshot.rows[0].security_id, 39_582);
        assert_eq!(snapshot.rows[0].score_ratio(metric), Some((3_900_000, 600)));
        assert_eq!(snapshot.rows[1].estimated_net_volume, Some(-30_600));
        assert_eq!(
            snapshot.rows[1].score_ratio(metric),
            Some((-3_120_000, 600))
        );
        let runtime = BucketTopVolumeRuntime::new();
        board
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf))
            .unwrap();
        let mut read = request(tf, OptionFamily::Stock);
        read.metric = metric;
        assert_eq!(
            runtime
                .load_winner_for_decision(read)
                .unwrap()
                .first()
                .unwrap()
                .security_id,
            39_582
        );
    }
}

#[test]
fn signed_bar_v3_refuses_partial_legacy_flow_and_preserves_unknown_and_tie() {
    let metric = BucketVolumeMetric::SignedBarVolumeVsOneLotV3;
    let metadata = contract(39_582, OptionFamily::Stock, 600);
    for tf in TfIndex::TOP_VOLUME_ALL {
        for (gross, signed, accepted) in [
            (39_600, Some(18_000), false),
            (30_600, Some(-3_000), false),
            (39_600, Some(39_600), true),
            (30_600, Some(-30_600), true),
            (30_600, Some(0), true),
            (30_600, None, true),
            (i64::MAX as u64 + 1, Some(i64::MIN), false),
        ] {
            let mut board =
                BucketTopVolumeEngine::new(Feed::Dhan, DAY, EPOCH, metric, [metadata], 2).unwrap();
            let result = board.apply(&update(metadata, tf, gross, signed, 1));
            assert_eq!(result.is_ok(), accepted, "{tf:?}: {gross}/{signed:?}");
            if !accepted {
                assert_eq!(
                    result.unwrap_err(),
                    BucketUpdateRefusal::InvalidSignedVolume
                );
            } else {
                let snapshot = board
                    .snapshot(OptionFamily::Stock, tf, start(tf), start(tf))
                    .unwrap();
                assert_eq!(snapshot.rows.len(), usize::from(signed.is_some()));
                if signed.is_none() {
                    assert_eq!(snapshot.coverage.unavailable_net_contracts, 1);
                    assert!(!snapshot.coverage.complete_for_declared_universe());
                }
            }
        }
    }
}

fn request(tf: TfIndex, family: OptionFamily) -> BucketDecisionRequest {
    BucketDecisionRequest {
        feed: Feed::Dhan,
        family,
        tf,
        session_day: DAY,
        universe_version: EPOCH,
        metric: METRIC,
        bucket_start_secs: start(tf),
        now_secs: start(tf),
        max_age_secs: 5,
        require_closed: false,
    }
}

#[test]
fn strict_final_window_decisions_preserve_all_four_top_volume_identities() {
    let close = DAY * 86_400 + 56_400;
    let metadata = contract(1, OptionFamily::Stock, 100);
    for tf in TfIndex::TOP_VOLUME_ALL {
        let mut engine = engine(vec![metadata]);
        let runtime = BucketTopVolumeRuntime::new();
        let mut candle = update(metadata, tf, 100, Some(90), 1);
        candle.bucket_start_secs = tf.bucket_start(close - 1);
        candle.bucket_end_secs = tf.bucket_end(candle.bucket_start_secs);
        candle.last_observed_secs = close - 1;
        candle.closed = true;
        engine.apply(&candle).unwrap();
        engine
            .publish_latest(&runtime, OptionFamily::Stock, tf, close)
            .unwrap();
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, close)
            .unwrap();
        let mut read = request(tf, OptionFamily::Stock);
        read.bucket_start_secs = candle.bucket_start_secs;
        read.require_closed = true;
        read.max_age_secs = 2;
        read.now_secs = close - 1;
        assert_eq!(
            runtime.load_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::OpenBucket
        );
        assert_eq!(
            runtime.load_winner_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::OpenBucket
        );
        read.now_secs = close;
        let full = runtime.load_for_decision(read).unwrap();
        let winner = runtime.load_winner_for_decision(read).unwrap();
        assert_eq!(full.bucket_start_secs, candle.bucket_start_secs);
        assert_eq!(full.bucket_end_secs, candle.bucket_end_secs);
        assert_eq!(winner.bucket_end_secs, candle.bucket_end_secs);
        assert_eq!(winner.first().unwrap().last_observed_secs, close - 1);
        assert_eq!(winner.bucket_end_secs, close, "{tf:?}");
        read.now_secs = close + 2;
        assert_eq!(
            runtime.load_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::StaleInstrument
        );
        assert_eq!(
            runtime.load_winner_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::StaleInstrument
        );
    }
}

#[test]
fn a_partial_final_minute_publication_stays_unqualified_after_the_window_ends() {
    let close = DAY * 86_400 + 56_400;
    let metadata = contract(1, OptionFamily::Stock, 100);
    let tf = TfIndex::M1;
    for reported_closed in [false, true] {
        let mut engine = engine(vec![metadata]);
        let runtime = BucketTopVolumeRuntime::new();
        let mut candle = update(metadata, tf, 100, Some(90), 1);
        candle.bucket_start_secs = tf.bucket_start(close - 1);
        candle.bucket_end_secs = tf.bucket_end(candle.bucket_start_secs);
        candle.last_observed_secs = close - 1;
        candle.closed = reported_closed;
        engine.apply(&candle).unwrap();
        // Even a malformed producer's early closed=true cannot borrow elapsed
        // wall time: this publication predates the admitted observation end.
        engine
            .publish_latest(&runtime, OptionFamily::Stock, tf, close - 1)
            .unwrap();
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, close - 1)
            .unwrap();
        let mut read = request(tf, OptionFamily::Stock);
        read.bucket_start_secs = candle.bucket_start_secs;
        read.require_closed = true;
        read.now_secs = close;
        assert_eq!(
            runtime.load_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::OpenBucket
        );
        assert_eq!(
            runtime.load_winner_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::OpenBucket
        );
    }
}

#[test]
fn an_expired_final_minute_window_does_not_relax_coverage_or_source_age() {
    let close = DAY * 86_400 + 56_400;
    let metadata = contract(1, OptionFamily::Stock, 100);
    let tf = TfIndex::M1;
    for complete in [false, true] {
        let contracts = if complete {
            vec![metadata]
        } else {
            vec![metadata, contract(2, OptionFamily::Stock, 100)]
        };
        let mut engine = engine(contracts);
        let runtime = BucketTopVolumeRuntime::new();
        let mut candle = update(metadata, tf, 100, Some(90), 1);
        candle.bucket_start_secs = tf.bucket_start(close - 1);
        candle.bucket_end_secs = tf.bucket_end(candle.bucket_start_secs);
        candle.last_observed_secs = close - 31;
        candle.closed = true;
        engine.apply(&candle).unwrap();
        engine
            .publish_latest(&runtime, OptionFamily::Stock, tf, close)
            .unwrap();
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, close)
            .unwrap();
        let mut read = request(tf, OptionFamily::Stock);
        read.bucket_start_secs = candle.bucket_start_secs;
        read.require_closed = true;
        read.now_secs = close;
        read.max_age_secs = 30;
        let expected = if complete {
            BucketDecisionRefusal::StaleInstrument
        } else {
            BucketDecisionRefusal::IncompleteUniverse
        };
        assert_eq!(runtime.load_for_decision(read).unwrap_err(), expected);
        assert_eq!(
            runtime.load_winner_for_decision(read).unwrap_err(),
            expected
        );
    }
}

#[test]
fn the_close_margin_does_not_hide_the_three_second_age_of_the_freshest_input() {
    let close = DAY * 86_400 + 56_400;
    let metadata = contract(1, OptionFamily::Stock, 100);
    let tf = TfIndex::M1;
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    let mut candle = update(metadata, tf, 100, Some(90), 1);
    candle.bucket_start_secs = tf.bucket_start(close - 1);
    candle.bucket_end_secs = tf.bucket_end(candle.bucket_start_secs);
    candle.last_observed_secs = close - 1;
    candle.closed = true;
    engine.apply(&candle).unwrap();
    engine
        .publish_latest(&runtime, OptionFamily::Stock, tf, close + 2)
        .unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, close + 2)
        .unwrap();
    let mut read = request(tf, OptionFamily::Stock);
    read.bucket_start_secs = candle.bucket_start_secs;
    read.require_closed = true;
    read.now_secs = close + 2;
    read.max_age_secs = 2;
    assert_eq!(
        runtime.load_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::StaleInstrument
    );
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::StaleInstrument
    );
    read.max_age_secs = 3;
    assert!(runtime.load_for_decision(read).is_ok());
    assert!(runtime.load_winner_for_decision(read).is_ok());
}

#[test]
fn a_historical_session_cannot_self_authorize_a_current_decision_with_large_age() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    for tf in [TfIndex::S1, TfIndex::M1] {
        let mut engine = engine(vec![metadata]);
        let runtime = BucketTopVolumeRuntime::new();
        engine
            .apply(&update(metadata, tf, 100, Some(90), 1))
            .unwrap();
        engine
            .publish_latest(&runtime, OptionFamily::Stock, tf, start(tf))
            .unwrap();
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf))
            .unwrap();
        let mut read = request(tf, OptionFamily::Stock);
        read.max_age_secs = u32::MAX;
        read.now_secs += 86_400;
        assert_eq!(
            runtime.load_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::DifferentEpoch
        );
        assert_eq!(
            runtime.load_winner_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::DifferentEpoch
        );
    }
}

fn board(
    engine: &BucketTopVolumeEngine,
    family: OptionFamily,
    tf: TfIndex,
) -> Arc<BucketTopVolumeSnapshot> {
    engine.snapshot(family, tf, start(tf), start(tf)).unwrap()
}

#[test]
fn signed_candle_quantity_decides_order_not_gross_or_absolute_value() {
    let metadata = [
        contract(1, OptionFamily::Stock, 100),
        contract(2, OptionFamily::Stock, 100),
        contract(3, OptionFamily::Stock, 100),
        contract(4, OptionFamily::Stock, 100),
    ];
    let mut engine = engine(metadata.to_vec());
    let rows = [
        (100_000, Some(-80_000)),
        (10, Some(10)),
        (20_000, Some(0)),
        (1_000, Some(-1)),
    ];
    for (item, &(gross, net)) in metadata.into_iter().zip(&rows) {
        assert_eq!(
            engine.apply(&update(item, TfIndex::M1, gross, net, 1)),
            Ok(BucketApplyOutcome::Applied)
        );
    }
    let ranked = board(&engine, OptionFamily::Stock, TfIndex::M1);
    assert_eq!(
        ranked
            .rows
            .iter()
            .map(|row| row.security_id)
            .collect::<Vec<_>>(),
        vec![2, 3, 4, 1]
    );
    assert!(ranked.coverage.complete_for_declared_universe());
    assert_eq!(ranked.rows[0].gross_volume, 10);
    assert_eq!(ranked.rows[0].estimated_net_volume, Some(10));
}

#[test]
fn null_net_is_unavailable_while_genuinely_classified_zero_is_ranked() {
    let one = contract(1, OptionFamily::Stock, 100);
    let two = contract(2, OptionFamily::Stock, 100);
    let mut engine = engine(vec![one, two]);
    engine
        .apply(&update(one, TfIndex::S1, 0, Some(0), 1))
        .unwrap();
    engine
        .apply(&update(two, TfIndex::S1, 100, None, 1))
        .unwrap();
    let ranked = board(&engine, OptionFamily::Stock, TfIndex::S1);
    assert_eq!(ranked.rows.len(), 1);
    assert_eq!(ranked.rows[0].estimated_net_volume, Some(0));
    assert_eq!(ranked.rows[0].score_milli_pct(METRIC), Some(-100_000));
    assert_eq!(ranked.coverage.unavailable_net_contracts, 1);
    assert!(!ranked.coverage.complete_for_declared_universe());
}

#[test]
fn percentage_offset_is_explicit_and_negative_percentage_is_not_negative_flow() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    engine
        .apply(&update(metadata, TfIndex::M1, 150, Some(50), 1))
        .unwrap();
    let first = board(&engine, OptionFamily::Stock, TfIndex::M1).rows[0];
    assert_eq!(first.gross_lots_milli(), Some(1_500));
    assert_eq!(first.estimated_net_lots_milli(), Some(500));
    assert_eq!(first.score_ratio(METRIC), Some((-5_000, 100)));
    assert_eq!(first.score_milli_pct(METRIC), Some(-50_000));
    assert_eq!(
        first.score_milli_pct(BucketVolumeMetric::GrossActivityVsOneLotV1),
        Some(50_000)
    );
    assert_eq!(METRIC.as_str(), "signed_estimated_net_vs_one_lot_v2");
}

#[test]
fn sub_display_precision_ratios_are_not_identity_ties_including_negative_values() {
    let metadata = [
        contract(90, OptionFamily::Stock, u32::MAX),
        contract(1, OptionFamily::Stock, u32::MAX),
        contract(2, OptionFamily::Stock, u32::MAX),
        contract(3, OptionFamily::Stock, u32::MAX),
    ];
    let mut engine = engine(metadata.to_vec());
    for (item, net) in metadata.into_iter().zip([2_i64, 1, -1, -2]) {
        engine
            .apply(&update(item, TfIndex::S3, net.unsigned_abs(), Some(net), 1))
            .unwrap();
    }
    let ranked = board(&engine, OptionFamily::Stock, TfIndex::S3);
    assert_eq!(
        ranked
            .rows
            .iter()
            .map(|row| row.security_id)
            .collect::<Vec<_>>(),
        vec![90, 1, 2, 3]
    );
    assert!(
        ranked
            .rows
            .iter()
            .all(|row| row.estimated_net_lots_milli() == Some(0))
    );
}

#[test]
fn exact_equal_ratios_use_security_id_then_segment_only() {
    let mut first = contract(7, OptionFamily::Stock, 100);
    first.segment = ExchangeSegment::BseFno;
    let second = contract(7, OptionFamily::Stock, 200);
    let third = contract(6, OptionFamily::Stock, 300);
    let mut engine = engine(vec![first, second, third]);
    for (item, net) in [(first, -100_i64), (second, -200), (third, -300)] {
        engine
            .apply(&update(item, TfIndex::S3, net.unsigned_abs(), Some(net), 1))
            .unwrap();
    }
    let ranked = board(&engine, OptionFamily::Stock, TfIndex::S3);
    assert_eq!(
        ranked.rows.iter().map(|row| row.key()).collect::<Vec<_>>(),
        vec![(6, 2), (7, 2), (7, 8)]
    );
}

#[test]
fn all_four_top_volume_frames_and_both_families_have_independent_slots() {
    let stock = contract(1, OptionFamily::Stock, 100);
    let index = contract(2, OptionFamily::Index, 25);
    let mut engine = engine(vec![stock, index]);
    let runtime = BucketTopVolumeRuntime::new();
    let mut published_slots = 0;
    for tf in TfIndex::TOP_VOLUME_ALL {
        engine
            .apply(&update(stock, tf, 200, Some(-100), 1))
            .unwrap();
        engine
            .apply(&update(index, tf, 1_000, Some(900), 1))
            .unwrap();
        for family in [OptionFamily::Stock, OptionFamily::Index] {
            engine
                .publish_latest(&runtime, family, tf, start(tf))
                .unwrap();
            engine
                .publish_winner(&runtime, family, tf, start(tf), start(tf))
                .unwrap();
            let full = runtime.load_for_decision(request(tf, family)).unwrap();
            let winner = runtime
                .load_winner_for_decision(request(tf, family))
                .unwrap();
            assert_eq!(full.tf, tf);
            assert_eq!(full.family, family);
            assert_eq!(full.bucket_start_secs, tf.bucket_start(start(tf)));
            assert_eq!(full.bucket_end_secs, tf.bucket_end(start(tf)));
            assert_eq!(full.first(), winner.first());
            assert_eq!(full.coverage, winner.coverage);
            published_slots += 1;
        }
    }
    assert_eq!((TF_COUNT, TOP_VOLUME_TF_COUNT), (10, 4));
    assert_eq!(published_slots, 8);
    assert_eq!(
        runtime
            .slots
            .iter()
            .map(|family| family.len())
            .sum::<usize>(),
        8
    );
    assert_eq!(
        runtime
            .winners
            .iter()
            .map(|family| family.len())
            .sum::<usize>(),
        8
    );
}

#[test]
fn candle_only_frames_cannot_allocate_or_publish_rankings_through_any_public_entry() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    engine
        .apply(&update(metadata, TfIndex::S1, 100, Some(100), 1))
        .unwrap();
    engine
        .publish_winner_latest(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S1,
            start(TfIndex::S1),
        )
        .unwrap();
    let original = runtime
        .load_winner(OptionFamily::Stock, TfIndex::S1)
        .unwrap();
    for tf in [
        TfIndex::M3,
        TfIndex::M5,
        TfIndex::M10,
        TfIndex::M15,
        TfIndex::M30,
        TfIndex::M60,
    ] {
        let candle = update(metadata, tf, 100, Some(100), 1);
        assert_eq!(
            engine.apply(&candle),
            Err(BucketUpdateRefusal::UnsupportedTimeframe)
        );
        assert!(engine.latest_bucket_start(tf).is_none());
        assert!(
            engine
                .snapshot(OptionFamily::Stock, tf, start(tf), start(tf))
                .is_none()
        );
        assert!(
            engine
                .winner_snapshot(OptionFamily::Stock, tf, start(tf), start(tf))
                .is_none()
        );
        assert_eq!(
            engine
                .publish_latest(&runtime, OptionFamily::Stock, tf, start(tf))
                .unwrap_err(),
            BucketPublishRefusal::UnsupportedTimeframe
        );
        assert_eq!(
            engine
                .publish_winner(&runtime, OptionFamily::Stock, tf, start(tf), start(tf))
                .unwrap_err(),
            BucketPublishRefusal::UnsupportedTimeframe
        );
        assert_eq!(
            engine
                .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf))
                .unwrap_err(),
            BucketPublishRefusal::UnsupportedTimeframe
        );
        assert!(runtime.load(OptionFamily::Stock, tf).is_none());
        assert!(runtime.load_winner(OptionFamily::Stock, tf).is_none());
        assert_eq!(
            runtime
                .load_for_decision(request(tf, OptionFamily::Stock))
                .unwrap_err(),
            BucketDecisionRefusal::UnsupportedTimeframe
        );
        assert_eq!(
            runtime
                .load_winner_for_decision(request(tf, OptionFamily::Stock))
                .unwrap_err(),
            BucketDecisionRefusal::UnsupportedTimeframe
        );
        let mut offered = *original;
        offered.tf = tf;
        offered.bucket_start_secs = start(tf);
        offered.bucket_end_secs = tf.bucket_end(start(tf));
        assert_eq!(
            runtime.publish_winner(Arc::new(offered)),
            Err(BucketPublishRefusal::UnsupportedTimeframe)
        );
        assert_eq!(
            runtime.publish(Arc::new(BucketTopVolumeSnapshot {
                feed: offered.feed,
                family: offered.family,
                tf,
                session_day: offered.session_day,
                universe_version: offered.universe_version,
                metric: offered.metric,
                bucket_start_secs: offered.bucket_start_secs,
                bucket_end_secs: offered.bucket_end_secs,
                revision: offered.revision,
                published_secs: offered.published_secs,
                coverage: offered.coverage,
                rows: vec![offered.winner.unwrap()],
            })),
            Err(BucketPublishRefusal::UnsupportedTimeframe)
        );
        assert!(Arc::ptr_eq(
            &original,
            &runtime
                .load_winner(OptionFamily::Stock, TfIndex::S1)
                .unwrap()
        ));
    }
    assert_eq!(engine.buckets.len(), 4);
    assert_eq!(engine.buckets.iter().map(BTreeMap::len).sum::<usize>(), 1);
}

#[test]
fn one_minute_winners_use_exact_bucket_boundaries_and_signed_per_lot_order() {
    let tf = TfIndex::M1;
    let first = contract(1, OptionFamily::Stock, 100);
    let second = contract(2, OptionFamily::Stock, 25);
    let mut engine = engine(vec![first, second]);
    let runtime = BucketTopVolumeRuntime::new();
    let previous_start = DAY * 86_400 + 9 * 3_600 + 15 * 60;
    let boundary = DAY * 86_400 + 9 * 3_600 + 16 * 60;
    assert_eq!(tf.bucket_start(boundary - 1), previous_start);
    assert_eq!(tf.bucket_start(boundary), boundary);
    assert_eq!(tf.bucket_end(previous_start), boundary);

    // Net / pinned lot alone sets the order. Gross activity is deliberately
    // much larger on the losing contract and does not act as a sort key.
    for (metadata, gross, net) in [(first, 100_000, 80), (second, 25, 25)] {
        let mut candle = update(metadata, tf, gross, Some(net), 1);
        candle.bucket_start_secs = previous_start;
        candle.bucket_end_secs = boundary;
        candle.last_observed_secs = boundary - 1;
        candle.closed = true;
        engine.apply(&candle).unwrap();
    }
    for (metadata, gross, net) in [(first, 100_000, 150), (second, 40, 40)] {
        let mut candle = update(metadata, tf, gross, Some(net), 2);
        candle.bucket_start_secs = boundary;
        candle.bucket_end_secs = boundary + 60;
        candle.last_observed_secs = boundary;
        engine.apply(&candle).unwrap();
    }
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, boundary)
        .unwrap();
    let mut read = request(tf, OptionFamily::Stock);
    read.bucket_start_secs = previous_start;
    read.now_secs = boundary;
    read.require_closed = true;
    let previous = runtime.load_winner_for_decision(read).unwrap();
    assert_eq!(previous.first().unwrap().security_id, 2);
    assert_eq!(previous.first().unwrap().score_milli_pct(METRIC), Some(0));
    read.bucket_start_secs = boundary;
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::OpenBucket
    );
    read.require_closed = false;
    let current = runtime.load_winner_for_decision(read).unwrap();
    assert_eq!(current.first().unwrap().security_id, 2);
    assert_eq!(
        current.first().unwrap().score_milli_pct(METRIC),
        Some(60_000)
    );

    let mut correction = update(second, tf, 500, Some(-500), 3);
    correction.bucket_start_secs = boundary;
    correction.bucket_end_secs = boundary + 60;
    correction.last_observed_secs = boundary;
    engine.apply(&correction).unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, boundary)
        .unwrap();
    let corrected = runtime.load_winner_for_decision(read).unwrap();
    assert_eq!(corrected.first().unwrap().security_id, 1);
    assert_eq!(corrected.first().unwrap().estimated_net_volume, Some(150));
    assert_eq!(previous.first().unwrap().estimated_net_volume, Some(25));
    read.bucket_start_secs = previous_start;
    read.require_closed = true;
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap().first(),
        previous.first()
    );
    // Winner decisions did not require or materialize a full table.
    assert!(runtime.load(OptionFamily::Stock, tf).is_none());
}

#[test]
fn same_bucket_corrections_can_reduce_volume_change_sign_and_remove_the_winner() {
    let first = contract(1, OptionFamily::Stock, 100);
    let second = contract(2, OptionFamily::Stock, 100);
    let mut engine = engine(vec![first, second]);
    let runtime = BucketTopVolumeRuntime::new();
    engine
        .apply(&update(first, TfIndex::S5, 1_000, Some(900), 1))
        .unwrap();
    engine
        .apply(&update(second, TfIndex::S5, 200, Some(100), 1))
        .unwrap();
    engine
        .publish_latest(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S5,
            start(TfIndex::S5),
        )
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S5,
            start(TfIndex::S5),
            start(TfIndex::S5),
        )
        .unwrap();
    let retained = runtime
        .load_winner_for_decision(request(TfIndex::S5, OptionFamily::Stock))
        .unwrap();
    assert_eq!(retained.first().unwrap().security_id, 1);
    engine
        .apply(&update(first, TfIndex::S5, 10, Some(-10), 2))
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S5,
            start(TfIndex::S5),
            start(TfIndex::S5),
        )
        .unwrap();
    assert_eq!(
        runtime
            .load_winner_for_decision(request(TfIndex::S5, OptionFamily::Stock))
            .unwrap()
            .first()
            .unwrap()
            .security_id,
        2
    );
    assert_eq!(
        retained.first().unwrap().security_id,
        1,
        "old Arc remains immutable"
    );
    engine
        .apply(&update(first, TfIndex::S5, 10, None, 3))
        .unwrap();
    engine
        .apply(&update(second, TfIndex::S5, 200, None, 2))
        .unwrap();
    engine
        .publish_latest(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S5,
            start(TfIndex::S5),
        )
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S5,
            start(TfIndex::S5),
            start(TfIndex::S5),
        )
        .unwrap();
    assert!(
        runtime
            .load(OptionFamily::Stock, TfIndex::S5)
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        runtime
            .load_winner(OptionFamily::Stock, TfIndex::S5)
            .unwrap()
            .winner
            .is_none()
    );
    assert_eq!(
        runtime
            .load_winner_for_decision(request(TfIndex::S5, OptionFamily::Stock))
            .unwrap_err(),
        BucketDecisionRefusal::IncompleteUniverse
    );
}

#[test]
fn duplicate_observations_cannot_refresh_publication_time_or_increment_revision() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    let event = update(metadata, TfIndex::M1, 100, Some(10), 1);
    engine.apply(&event).unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1),
        )
        .unwrap();
    assert_eq!(engine.apply(&event), Ok(BucketApplyOutcome::Duplicate));
    assert!(
        engine
            .publish_winner(
                &runtime,
                OptionFamily::Stock,
                TfIndex::M1,
                start(TfIndex::M1),
                start(TfIndex::M1) + 5
            )
            .unwrap()
            .is_none()
    );
    let duplicate = engine
        .winner_snapshot(
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1) + 5,
        )
        .unwrap();
    assert_eq!(
        runtime.publish_winner(duplicate),
        Err(BucketPublishRefusal::StaleRevision)
    );
    assert_eq!(
        runtime
            .load_winner(OptionFamily::Stock, TfIndex::M1)
            .unwrap()
            .published_secs,
        start(TfIndex::M1)
    );
}

#[test]
fn equal_revision_conflict_poisoning_replaces_the_decision_ready_state() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    engine
        .apply(&update(metadata, TfIndex::M1, 100, Some(10), 4))
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1),
        )
        .unwrap();
    assert!(
        runtime
            .load_winner_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .is_ok()
    );
    assert_eq!(
        engine.apply(&update(metadata, TfIndex::M1, 100, Some(-10), 4)),
        Err(BucketUpdateRefusal::ConflictingRevision)
    );
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1),
        )
        .unwrap();
    assert_eq!(
        runtime
            .load_winner_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .unwrap_err(),
        BucketDecisionRefusal::IncompleteUniverse
    );
}

#[test]
fn malformed_bounds_revoke_published_decisions_for_only_the_affected_timeframe_and_family() {
    let stock = contract(1, OptionFamily::Stock, 100);
    let index = contract(2, OptionFamily::Index, 25);
    let tf = TfIndex::S5;
    let latest_start = start(tf) + tf.seconds_per_bucket();
    for malformed_case in 0..4 {
        let mut engine = engine(vec![stock, index]);
        let runtime = BucketTopVolumeRuntime::new();
        for metadata in [stock, index] {
            for bucket_start in [start(tf), latest_start] {
                let mut event = update(metadata, tf, 100, Some(10), 1);
                event.bucket_start_secs = bucket_start;
                event.bucket_end_secs = tf.bucket_end(bucket_start);
                event.last_observed_secs = bucket_start;
                engine.apply(&event).unwrap();
            }
        }
        engine
            .apply(&update(stock, TfIndex::M1, 100, Some(10), 1))
            .unwrap();
        engine
            .publish_latest(&runtime, OptionFamily::Stock, tf, latest_start)
            .unwrap();
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, latest_start)
            .unwrap();
        let mut read = request(tf, OptionFamily::Stock);
        read.bucket_start_secs = latest_start;
        read.now_secs = latest_start;
        assert!(runtime.load_for_decision(read).is_ok());
        assert!(runtime.load_winner_for_decision(read).is_ok());
        let old_revision = runtime
            .load_winner(OptionFamily::Stock, tf)
            .unwrap()
            .revision;

        let mut malformed = update(stock, tf, 200, Some(20), 2);
        malformed.bucket_start_secs = latest_start;
        malformed.bucket_end_secs = tf.bucket_end(latest_start);
        malformed.last_observed_secs = latest_start;
        match malformed_case {
            0 => malformed.bucket_end_secs += 1,
            1 => malformed.bucket_start_secs += 1,
            2 => malformed.bucket_start_secs -= 86_400,
            3 => {
                malformed.bucket_start_secs += 1;
                malformed.revision = 0;
            }
            _ => unreachable!(),
        }
        assert_eq!(
            engine.apply(&malformed),
            Err(BucketUpdateRefusal::InvalidBucket)
        );
        // Both publication paths must replace the former green revision;
        // the dirty-revision shortcut must not preserve its eligibility.
        assert!(
            engine
                .publish_latest(&runtime, OptionFamily::Stock, tf, latest_start)
                .unwrap()
                .is_some()
        );
        assert!(
            engine
                .publish_winner_latest(&runtime, OptionFamily::Stock, tf, latest_start)
                .unwrap()
                .is_some()
        );
        assert!(
            runtime
                .load_winner(OptionFamily::Stock, tf)
                .unwrap()
                .revision
                > old_revision
        );
        assert_eq!(
            runtime.load_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::IncompleteUniverse
        );
        assert_eq!(
            runtime.load_winner_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::IncompleteUniverse
        );
        for bucket_start in [start(tf), latest_start] {
            assert!(
                engine
                    .winner_snapshot(OptionFamily::Stock, tf, bucket_start, latest_start)
                    .unwrap()
                    .coverage
                    .integrity_fault
            );
            assert!(
                engine
                    .winner_snapshot(OptionFamily::Index, tf, bucket_start, latest_start)
                    .unwrap()
                    .coverage
                    .complete_for_declared_universe()
            );
        }
        assert!(
            board(&engine, OptionFamily::Stock, TfIndex::M1)
                .coverage
                .complete_for_declared_universe()
        );
    }
}

#[test]
fn exhausted_board_revision_revokes_eligibility_even_without_a_new_counter_value() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    engine
        .apply(&update(metadata, TfIndex::M1, 100, Some(10), 1))
        .unwrap();
    engine.buckets[TfIndex::M1.top_volume_ordinal().unwrap()]
        .get_mut(&start(TfIndex::M1))
        .unwrap()
        .families[family_slot(OptionFamily::Stock)]
    .revision = u64::MAX;
    engine
        .publish_latest(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
        )
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1),
        )
        .unwrap();
    assert!(
        runtime
            .load_winner_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .is_ok()
    );
    assert_eq!(
        engine.apply(&update(metadata, TfIndex::M1, 200, Some(20), 2)),
        Err(BucketUpdateRefusal::BoardRevisionExhausted)
    );
    engine
        .publish_latest(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
        )
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1),
        )
        .unwrap();
    assert_eq!(
        runtime
            .load_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .unwrap_err(),
        BucketDecisionRefusal::IncompleteUniverse
    );
    assert_eq!(
        runtime
            .load_winner_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .unwrap_err(),
        BucketDecisionRefusal::IncompleteUniverse
    );
}

#[test]
fn declared_universe_does_not_silently_shrink_to_contracts_that_happened_to_update() {
    let first = contract(1, OptionFamily::Stock, 100);
    let second = contract(2, OptionFamily::Stock, 100);
    let mut engine = engine(vec![first, second]);
    let runtime = BucketTopVolumeRuntime::new();
    engine
        .apply(&update(first, TfIndex::M1, 100, Some(10), 1))
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1),
        )
        .unwrap();
    let partial = runtime
        .load_winner(OptionFamily::Stock, TfIndex::M1)
        .unwrap();
    assert_eq!(
        (
            partial.coverage.expected_contracts,
            partial.coverage.observed_contracts
        ),
        (2, 1)
    );
    assert_eq!(
        runtime
            .load_winner_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .unwrap_err(),
        BucketDecisionRefusal::IncompleteUniverse
    );
    engine
        .apply(&update(second, TfIndex::M1, 0, Some(0), 1))
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1),
        )
        .unwrap();
    assert!(
        runtime
            .load_winner_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .is_ok()
    );
}

#[test]
fn refreshed_winner_does_not_hide_a_stale_lower_ranked_instrument() {
    let first = contract(1, OptionFamily::Stock, 100);
    let second = contract(2, OptionFamily::Stock, 100);
    let mut engine = engine(vec![first, second]);
    let runtime = BucketTopVolumeRuntime::new();
    engine
        .apply(&update(first, TfIndex::M1, 100, Some(50), 1))
        .unwrap();
    engine
        .apply(&update(second, TfIndex::M1, 100, Some(10), 1))
        .unwrap();
    let mut fresh = update(first, TfIndex::M1, 200, Some(100), 2);
    fresh.last_observed_secs += 20;
    engine.apply(&fresh).unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::M1,
            start(TfIndex::M1),
            start(TfIndex::M1) + 20,
        )
        .unwrap();
    let mut read = request(TfIndex::M1, OptionFamily::Stock);
    read.now_secs += 20;
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::StaleInstrument
    );
}

#[test]
fn closed_bucket_gate_does_not_accept_one_closed_contract_as_a_closed_board() {
    let first = contract(1, OptionFamily::Stock, 100);
    let second = contract(2, OptionFamily::Stock, 100);
    let mut engine = engine(vec![first, second]);
    let runtime = BucketTopVolumeRuntime::new();
    let observation_end = TfIndex::S1
        .observation_window_end(start(TfIndex::S1))
        .unwrap();
    let mut one = update(first, TfIndex::S1, 100, Some(10), 1);
    one.closed = true;
    engine.apply(&one).unwrap();
    engine
        .apply(&update(second, TfIndex::S1, 100, Some(10), 1))
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S1,
            start(TfIndex::S1),
            observation_end,
        )
        .unwrap();
    let mut read = request(TfIndex::S1, OptionFamily::Stock);
    read.require_closed = true;
    read.now_secs = observation_end;
    // Publication time is valid, so this refusal isolates incomplete closure.
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::OpenBucket
    );
    let mut two = update(second, TfIndex::S1, 100, Some(10), 2);
    two.closed = true;
    engine.apply(&two).unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S1,
            start(TfIndex::S1),
            observation_end,
        )
        .unwrap();
    runtime
        .load_winner_for_decision(read)
        .expect("all contracts closed and published at the observation end");
    let mut too_early = read;
    too_early.now_secs = start(TfIndex::S1);
    assert_eq!(
        runtime.load_winner_for_decision(too_early).unwrap_err(),
        BucketDecisionRefusal::OpenBucket
    );
}

#[test]
fn actual_candle_fold_and_seal_publish_identical_quantities_into_all_ranked_frames() {
    use tickvault_common::tick_types::ParsedTick;
    use tickvault_trading::candles::MultiTfAggregator;

    let metadata = contract(1, OptionFamily::Stock, 100);
    let pinned = update(metadata, TfIndex::S1, 0, Some(0), 1).metadata;
    let metric = BucketVolumeMetric::SignedBarVolumeVsOneLotV3;
    let mut engine =
        BucketTopVolumeEngine::new(Feed::Dhan, DAY, EPOCH, metric, [metadata], 2).unwrap();
    let mut candles = MultiTfAggregator::default();
    let ts = DAY * 86_400 + 9 * 3_600;
    let mut canonical = Vec::new();
    let mut seals = Vec::new();
    for (volume, price) in [(0, 100.0), (100, 101.0), (150, 100.0)] {
        let tick = ParsedTick {
            security_id: metadata.security_id,
            exchange_segment_code: metadata.segment.binary_code(),
            last_traded_price: price,
            exchange_timestamp: ts,
            received_at_nanos: (i64::from(ts) - 19_800) * 1_000_000_000,
            volume,
            volume_present: true,
            ..ParsedTick::default()
        };
        let stats = candles.consume_tick_with_context(
            Feed::Dhan,
            &tick,
            None,
            pinned,
            |_, _, _, tf, state| seals.push((tf, state)),
            |event| {
                if event.tf.is_top_volume() {
                    engine.apply(&event).unwrap();
                }
                canonical.push(event);
            },
        );
        assert!(stats.folded());
    }
    assert!(seals.is_empty());
    for tf in TfIndex::ALL {
        let quantity = canonical.iter().rev().find(|event| event.tf == tf).unwrap();
        assert_eq!(quantity.gross_volume, 150);
        assert_eq!(
            quantity.estimated_net_volume, None,
            "the first candle has no previous same-timeframe close"
        );
        if !tf.is_top_volume() {
            assert!(
                engine
                    .snapshot(OptionFamily::Stock, tf, quantity.bucket_start_secs, ts)
                    .is_none()
            );
            continue;
        }
        let ranked = engine
            .snapshot(OptionFamily::Stock, tf, quantity.bucket_start_secs, ts)
            .unwrap();
        assert!(
            ranked.first().is_none(),
            "unknown direction cannot produce a winner"
        );
        assert_eq!(ranked.coverage.observed_contracts, 1);
        assert_eq!(ranked.coverage.unavailable_net_contracts, 1);
    }
    candles.force_seal_all_with_volume_updates(
        |_, _, _, tf, state| seals.push((tf, state)),
        |event| {
            if event.tf.is_top_volume() {
                engine.apply(&event).unwrap();
            }
        },
    );
    assert_eq!(seals.len(), TF_COUNT);
    for (tf, candle) in seals {
        if !tf.is_top_volume() {
            assert!(
                engine
                    .snapshot(OptionFamily::Stock, tf, candle.bucket_start_ist_secs, ts)
                    .is_none()
            );
            continue;
        }
        let ranked = engine
            .snapshot(OptionFamily::Stock, tf, candle.bucket_start_ist_secs, ts)
            .unwrap();
        assert_eq!(
            candle.volume, 150,
            "direction must not change gross conservation"
        );
        assert_eq!(candle.net_volume(), None);
        assert!(ranked.first().is_none());
        assert_eq!(ranked.coverage.unavailable_net_contracts, 1);
        assert_eq!(
            ranked.coverage.closed_contracts, 0,
            "an administrative early seal does not qualify closure"
        );
    }
}

#[test]
fn closed_corrections_are_allowed_but_reopening_a_closed_revision_is_refused() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let mut closed = update(metadata, TfIndex::M1, 100, Some(10), 1);
    closed.closed = true;
    engine.apply(&closed).unwrap();
    closed.revision = 2;
    closed.estimated_net_volume = Some(-10);
    engine.apply(&closed).unwrap();
    closed.revision = 3;
    closed.closed = false;
    assert_eq!(
        engine.apply(&closed),
        Err(BucketUpdateRefusal::ClosedBucketReopened)
    );
    assert!(
        board(&engine, OptionFamily::Stock, TfIndex::M1)
            .coverage
            .integrity_fault
    );
}

#[test]
fn prior_session_metric_universe_and_wrong_bucket_requests_are_rejected() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    engine
        .apply(&update(metadata, TfIndex::S5, 100, Some(10), 1))
        .unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S5,
            start(TfIndex::S5),
            start(TfIndex::S5),
        )
        .unwrap();
    let good = request(TfIndex::S5, OptionFamily::Stock);
    for bad in [
        BucketDecisionRequest {
            session_day: DAY + 1,
            ..good
        },
        BucketDecisionRequest {
            universe_version: EPOCH + 1,
            ..good
        },
        BucketDecisionRequest {
            metric: BucketVolumeMetric::GrossActivityVsOneLotV1,
            ..good
        },
        BucketDecisionRequest {
            feed: Feed::Truedata,
            ..good
        },
    ] {
        assert_eq!(
            runtime.load_winner_for_decision(bad).unwrap_err(),
            BucketDecisionRefusal::DifferentEpoch
        );
    }
    let different_bucket = BucketDecisionRequest {
        bucket_start_secs: good.bucket_start_secs + 5,
        ..good
    };
    assert_eq!(
        runtime
            .load_winner_for_decision(different_bucket)
            .unwrap_err(),
        BucketDecisionRefusal::DifferentBucket
    );
}

#[test]
fn bounded_history_allows_retained_correction_without_regressing_latest_winner() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    let original = update(metadata, TfIndex::S1, 100, Some(10), 1);
    engine.apply(&original).unwrap();
    let mut newer = original;
    newer.bucket_start_secs += 1;
    newer.bucket_end_secs += 1;
    newer.last_observed_secs += 1;
    newer.revision += 1;
    engine.apply(&newer).unwrap();
    engine
        .publish_winner(
            &runtime,
            OptionFamily::Stock,
            TfIndex::S1,
            newer.bucket_start_secs,
            newer.last_observed_secs,
        )
        .unwrap();
    let mut correction = original;
    correction.revision = 3;
    correction.estimated_net_volume = Some(90);
    engine.apply(&correction).unwrap();
    assert_eq!(
        engine
            .snapshot(
                OptionFamily::Stock,
                TfIndex::S1,
                original.bucket_start_secs,
                newer.last_observed_secs
            )
            .unwrap()
            .first()
            .unwrap()
            .estimated_net_volume,
        Some(90)
    );
    assert_eq!(
        engine
            .publish_winner(
                &runtime,
                OptionFamily::Stock,
                TfIndex::S1,
                original.bucket_start_secs,
                newer.last_observed_secs
            )
            .unwrap_err(),
        BucketPublishRefusal::OlderBucket
    );
    newer.bucket_start_secs += 1;
    newer.bucket_end_secs += 1;
    newer.last_observed_secs += 1;
    newer.revision += 1;
    engine.apply(&newer).unwrap();
    correction.revision += 1;
    assert_eq!(
        engine.apply(&correction),
        Err(BucketUpdateRefusal::ExpiredBucket)
    );
    assert!(
        engine
            .snapshot(
                OptionFamily::Stock,
                TfIndex::S1,
                original.bucket_start_secs,
                newer.last_observed_secs
            )
            .is_none()
    );
}

#[test]
fn metadata_mismatch_cannot_reprice_old_numerator_with_a_new_lot_size() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let mut event = update(metadata, TfIndex::M1, 100, Some(50), 1);
    engine.apply(&event).unwrap();
    event.revision = 2;
    event.metadata.lot_size = 1;
    assert_eq!(
        engine.apply(&event),
        Err(BucketUpdateRefusal::MetadataMismatch)
    );
    let result = board(&engine, OptionFamily::Stock, TfIndex::M1);
    assert_eq!(result.rows[0].lot_size, 100);
    assert!(result.coverage.integrity_fault);
}

#[test]
fn each_uncertainty_dimension_prevents_a_decision_ready_board() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut quality_variants = [known_quality(); 4];
    quality_variants[0].baseline_known = false;
    quality_variants[1].counter_ambiguous = true;
    quality_variants[2].volume_missing = true;
    quality_variants[3].attribution_uncertain = true;
    for quality in quality_variants {
        let mut engine = engine(vec![metadata]);
        let mut event = update(metadata, TfIndex::M1, 100, Some(50), 1);
        event.quality = quality;
        engine.apply(&event).unwrap();
        let result = board(&engine, OptionFamily::Stock, TfIndex::M1);
        assert!(result.rows.is_empty());
        assert_eq!(result.coverage.uncertain_contracts, 1);
        assert!(!result.coverage.complete_for_declared_universe());
    }
    for bit in 0..8 {
        let mut engine = engine(vec![metadata]);
        let mut event = update(metadata, TfIndex::M1, 100, Some(50), 1);
        event.volume_quality = 1 << bit;
        engine.apply(&event).unwrap();
        assert!(
            board(&engine, OptionFamily::Stock, TfIndex::M1)
                .rows
                .is_empty()
        );
    }
}

#[test]
fn full_signed_and_gross_integer_domains_are_ordered_without_overflow() {
    let metadata = [
        contract(1, OptionFamily::Stock, 1),
        contract(2, OptionFamily::Stock, u32::MAX),
        contract(3, OptionFamily::Stock, 1),
        contract(4, OptionFamily::Stock, u32::MAX),
    ];
    let mut signed = engine(metadata.to_vec());
    for (item, net) in metadata
        .into_iter()
        .zip([i64::MIN, i64::MIN, i64::MAX, i64::MAX])
    {
        signed
            .apply(&update(item, TfIndex::M1, u64::MAX, Some(net), 1))
            .unwrap();
    }
    assert_eq!(
        board(&signed, OptionFamily::Stock, TfIndex::M1)
            .rows
            .iter()
            .map(|row| row.security_id)
            .collect::<Vec<_>>(),
        vec![3, 4, 2, 1]
    );
    let mut gross = BucketTopVolumeEngine::new(
        Feed::Dhan,
        DAY,
        EPOCH,
        BucketVolumeMetric::GrossActivityVsOneLotV1,
        metadata,
        2,
    )
    .unwrap();
    for (item, value) in metadata
        .into_iter()
        .zip([u64::MAX - 1, u64::MAX, u64::MAX, 0])
    {
        gross
            .apply(&update(item, TfIndex::M1, value, None, 1))
            .unwrap();
    }
    let ordered = board(&gross, OptionFamily::Stock, TfIndex::M1);
    assert_eq!(
        ordered
            .rows
            .iter()
            .map(|row| row.security_id)
            .collect::<Vec<_>>(),
        vec![3, 1, 2, 4]
    );
    assert_eq!(
        ordered.rows[0].gross_lots_milli(),
        Some(u128::from(u64::MAX) * 1_000)
    );
    assert!(
        ordered.rows[0]
            .score_milli_pct(BucketVolumeMetric::GrossActivityVsOneLotV1)
            .unwrap()
            > i128::from(i64::MAX)
    );
}

#[test]
fn fixed_winner_updates_do_not_materialize_or_refresh_the_full_board() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    for revision in 1..=100 {
        engine
            .apply(&update(
                metadata,
                TfIndex::M1,
                revision,
                Some(i64::try_from(revision).unwrap()),
                revision,
            ))
            .unwrap();
        engine
            .publish_winner(
                &runtime,
                OptionFamily::Stock,
                TfIndex::M1,
                start(TfIndex::M1),
                start(TfIndex::M1),
            )
            .unwrap();
    }
    assert!(runtime.load(OptionFamily::Stock, TfIndex::M1).is_none());
    assert_eq!(
        runtime
            .load_winner_for_decision(request(TfIndex::M1, OptionFamily::Stock))
            .unwrap()
            .first()
            .unwrap()
            .estimated_net_volume,
        Some(100)
    );
}

#[test]
fn closed_previous_winner_survives_a_full_rollover_batch_for_every_intraday_frame() {
    for tf in TfIndex::TOP_VOLUME_ALL {
        let metadata = contract(1, OptionFamily::Stock, 100);
        let mut engine = engine(vec![metadata]);
        let runtime = BucketTopVolumeRuntime::new();
        let mut closed = update(metadata, tf, 100, Some(90), 1);
        let boundary = closed.bucket_end_secs;
        closed.closed = true;
        closed.last_observed_secs = boundary;
        let mut live = closed;
        live.bucket_start_secs = boundary;
        live.bucket_end_secs = tf.bucket_end(boundary);
        live.estimated_net_volume = Some(-20);
        live.revision = 2;
        live.closed = false;
        // The real bridge publishes after the complete tick batch, rather
        // than between the old seal and the newly opened state.
        engine.apply(&closed).unwrap();
        engine.apply(&live).unwrap();
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, boundary)
            .unwrap();
        let mut read = request(tf, OptionFamily::Stock);
        read.now_secs = boundary;
        read.require_closed = true;
        let previous = runtime.load_winner_for_decision(read).unwrap();
        assert_eq!(previous.bucket_start_secs, closed.bucket_start_secs);
        assert_eq!(previous.first().unwrap().estimated_net_volume, Some(90));
        read.bucket_start_secs = boundary;
        assert_eq!(
            runtime.load_winner_for_decision(read).unwrap_err(),
            BucketDecisionRefusal::OpenBucket
        );
        read.require_closed = false;
        let current = runtime.load_winner_for_decision(read).unwrap();
        assert_eq!(current.first().unwrap().estimated_net_volume, Some(-20));
        assert_eq!(
            runtime
                .load_winner(OptionFamily::Stock, tf)
                .unwrap()
                .bucket_start_secs,
            boundary
        );
        assert!(runtime.load(OptionFamily::Stock, tf).is_none());
    }
}

#[test]
fn winner_retention_tracks_admitted_buckets_even_when_timestamps_have_large_gaps() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let tf = TfIndex::S1;
    for retention in 1..=MAX_RETAINED_BUCKETS_PER_TIMEFRAME {
        let mut engine =
            BucketTopVolumeEngine::new(Feed::Dhan, DAY, EPOCH, METRIC, [metadata], retention)
                .unwrap();
        let runtime = BucketTopVolumeRuntime::new();
        for index in 0..retention + 2 {
            let mut value = update(metadata, tf, 100, Some(90), 1);
            // A modulo-by-time ring would alias all these starts. Retention
            // means the last B admitted buckets, including sparse streams.
            value.bucket_start_secs += u32::try_from(index).unwrap() * 16;
            value.bucket_end_secs = value.bucket_start_secs + 1;
            value.last_observed_secs = value.bucket_start_secs;
            value.closed = true;
            engine.apply(&value).unwrap();
            engine
                .publish_winner_latest(&runtime, OptionFamily::Stock, tf, value.bucket_end_secs)
                .unwrap();
        }
        let mut read = request(tf, OptionFamily::Stock);
        read.require_closed = true;
        read.max_age_secs = 1_000;
        read.now_secs = start(tf) + u32::try_from(retention + 1).unwrap() * 16 + 1;
        for index in 0..retention + 2 {
            read.bucket_start_secs = start(tf) + u32::try_from(index).unwrap() * 16;
            if index < 2 {
                assert_eq!(
                    runtime.load_winner_for_decision(read).unwrap_err(),
                    BucketDecisionRefusal::DifferentBucket
                );
            } else {
                assert!(runtime.load_winner_for_decision(read).is_ok());
            }
        }
    }
}

#[test]
fn retained_correction_and_fault_update_the_old_bucket_without_refreshing_the_live_winner() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let tf = TfIndex::S1;
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    let mut closed = update(metadata, tf, 100, Some(10), 1);
    closed.closed = true;
    let mut live = closed;
    live.bucket_start_secs += 1;
    live.bucket_end_secs += 1;
    live.last_observed_secs += 1;
    live.closed = false;
    engine.apply(&closed).unwrap();
    engine.apply(&live).unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf) + 1)
        .unwrap();
    let latest = runtime.load_winner(OptionFamily::Stock, tf).unwrap();
    closed.revision = 2;
    closed.estimated_net_volume = Some(90);
    engine.apply(&closed).unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf) + 2)
        .unwrap();
    let mut read = request(tf, OptionFamily::Stock);
    read.now_secs += 2;
    read.require_closed = true;
    let corrected = runtime.load_winner_for_decision(read).unwrap();
    assert_eq!(corrected.first().unwrap().estimated_net_volume, Some(90));
    assert_eq!(corrected.published_secs, start(tf) + 2);
    assert!(Arc::ptr_eq(
        &latest,
        &runtime.load_winner(OptionFamily::Stock, tf).unwrap()
    ));
    assert!(
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf) + 3,)
            .unwrap()
            .is_none()
    );
    assert!(Arc::ptr_eq(
        &corrected,
        &runtime.load_winner_for_decision(read).unwrap()
    ));

    closed.revision = 0;
    assert_eq!(
        engine.apply(&closed),
        Err(BucketUpdateRefusal::InvalidRevision)
    );
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf) + 2)
        .unwrap();
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::IncompleteUniverse
    );
    read.bucket_start_secs += 1;
    read.require_closed = false;
    assert!(runtime.load_winner_for_decision(read).is_ok());
}

#[test]
fn retained_closed_winner_keeps_freshness_epoch_and_reset_gates() {
    let metadata = contract(1, OptionFamily::Stock, 100);
    let tf = TfIndex::S1;
    let mut engine = engine(vec![metadata]);
    let runtime = BucketTopVolumeRuntime::new();
    let mut closed = update(metadata, tf, 100, Some(90), 1);
    closed.closed = true;
    engine.apply(&closed).unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf) + 1)
        .unwrap();
    let mut live = closed;
    live.bucket_start_secs += 1;
    live.bucket_end_secs += 1;
    live.last_observed_secs += 1;
    live.closed = false;
    engine.apply(&live).unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf) + 2)
        .unwrap();
    let mut read = request(tf, OptionFamily::Stock);
    read.now_secs += 10;
    read.max_age_secs = 2;
    read.require_closed = true;
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::StalePublication
    );
    let mut replacement =
        BucketTopVolumeEngine::new(Feed::Dhan, DAY, EPOCH + 1, METRIC, [metadata], 2).unwrap();
    replacement.apply(&closed).unwrap();
    replacement
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, start(tf) + 1)
        .unwrap();
    read.now_secs = start(tf) + 1;
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::DifferentEpoch
    );
    read.universe_version += 1;
    assert!(runtime.load_winner_for_decision(read).is_ok());
    runtime.reset();
    assert_eq!(
        runtime.load_winner_for_decision(read).unwrap_err(),
        BucketDecisionRefusal::Unavailable
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn incremental_replacements_match_an_independent_complete_signed_sort(
        lots in prop::collection::vec(1_u32..=u32::MAX, 1..40),
        events in prop::collection::vec((0_usize..100, any::<i64>(), any::<bool>()), 1..150),
    ) {
        let metadata: Vec<_> = lots.iter().enumerate().map(|(id, &lot)| contract(u64::try_from(id).unwrap() + 1, OptionFamily::Stock, lot)).collect();
        let mut engine = engine(metadata.clone());
        let mut last: BTreeMap<usize, (i64, bool, u64)> = BTreeMap::new();
        for (index, net, classified) in events {
            let index = index % metadata.len();
            let revision = last.get(&index).map_or(1, |entry| entry.2 + 1);
            engine.apply(&update(metadata[index], TfIndex::M1, net.unsigned_abs(), classified.then_some(net), revision)).unwrap();
            last.insert(index, (net, classified, revision));
            let mut reference: Vec<_> = last.iter().filter_map(|(&index, &(net, classified, _))| classified.then_some((metadata[index].security_id, net, metadata[index].lot_size))).collect();
            reference.sort_unstable_by(|a, b| {
                let left = i128::from(a.1) * i128::from(b.2);
                let right = i128::from(b.1) * i128::from(a.2);
                right.cmp(&left).then_with(|| a.0.cmp(&b.0))
            });
            let actual = board(&engine, OptionFamily::Stock, TfIndex::M1);
            prop_assert_eq!(actual.rows.iter().map(|row| row.security_id).collect::<Vec<_>>(), reference.iter().map(|row| row.0).collect::<Vec<_>>());
            prop_assert_eq!(actual.coverage.observed_contracts, last.len());
            prop_assert_eq!(actual.coverage.eligible_contracts, reference.len());
            let cached = engine.winner_snapshot(OptionFamily::Stock, TfIndex::M1, start(TfIndex::M1), start(TfIndex::M1)).unwrap();
            prop_assert_eq!(actual.first(), cached.first());
        }
    }
}
