use super::*;
use secrecy::SecretString;
use tickvault_common::tick_types::VolumeQuality;
use tickvault_common::types::ExchangeSegment;

// 09:15 under the shared IST wall-clock axis; pre-09:00 timestamps are
// deliberately reanchored by TfIndex and are not valid bucket identities.
const START: u32 = 1_800_004_500;
const BIG_ID: u64 = 9_007_199_254_740_993;

fn now() -> Option<u32> {
    Some(START + 1)
}

fn late_now() -> Option<u32> {
    Some(START + 40)
}

fn clock_unavailable() -> Option<u32> {
    None
}

fn row(id: u64, volume: i64) -> BucketRankedContract {
    BucketRankedContract {
        security_id: id,
        segment: ExchangeSegment::NseFno,
        underlying_id: BIG_ID,
        lot_size: 100,
        instrument_definition_version: BIG_ID,
        gross_volume: volume.unsigned_abs(),
        estimated_net_volume: Some(volume),
        revision: 7,
        last_observed_secs: START,
        quality: VolumeQuality {
            baseline_known: true,
            ..VolumeQuality::default()
        },
        volume_quality: 0,
        closed: true,
    }
}

fn signed_quantity(row: &RowResponse) -> &SignedBarVolumeResponse {
    match &row.quantity {
        RowQuantityResponse::SignedBar(quantity) => quantity,
        RowQuantityResponse::Legacy(_) => panic!("expected the signed whole-bar volume contract"),
    }
}

fn fixture(rows: Vec<BucketRankedContract>, expected: usize) -> ApiState {
    fixture_with_metric(
        rows,
        expected,
        BucketVolumeMetric::SignedBarVolumeVsOneLotV3,
    )
}

fn fixture_with_metric(
    rows: Vec<BucketRankedContract>,
    expected: usize,
    metric: BucketVolumeMetric,
) -> ApiState {
    let runtime = Arc::new(BucketTopVolumeRuntime::new());
    let coverage = BucketCoverage {
        expected_contracts: expected,
        observed_contracts: rows.len(),
        eligible_contracts: rows.len(),
        uncertain_contracts: 0,
        unavailable_net_contracts: 0,
        closed_contracts: rows.iter().filter(|row| row.closed).count(),
        oldest_observed_secs: rows.iter().map(|row| row.last_observed_secs).min(),
        newest_observed_secs: rows.iter().map(|row| row.last_observed_secs).max(),
        integrity_fault: false,
    };
    runtime
        .publish_winner(Arc::new(BucketWinnerSnapshot {
            feed: Feed::Dhan,
            family: OptionFamily::Stock,
            tf: TfIndex::S1,
            session_day: START / 86_400,
            universe_version: BIG_ID,
            metric,
            bucket_start_secs: START,
            bucket_end_secs: START + 1,
            revision: 5,
            published_secs: START + 1,
            coverage,
            winner: rows.first().copied(),
        }))
        .unwrap();
    runtime
        .publish(Arc::new(BucketTopVolumeSnapshot {
            feed: Feed::Dhan,
            family: OptionFamily::Stock,
            tf: TfIndex::S1,
            session_day: START / 86_400,
            universe_version: BIG_ID,
            metric,
            bucket_start_secs: START,
            bucket_end_secs: START + 1,
            revision: 5,
            published_secs: START + 1,
            coverage,
            rows,
        }))
        .unwrap();
    ApiState {
        runtime: RuntimeSource::Isolated(runtime),
        now_ist_secs: now,
    }
}

fn list_query() -> ListQuery {
    ListQuery {
        family: FamilyQuery::Stock,
        timeframe: "1s".to_owned(),
        limit: 50,
        offset: 0,
        bucket_start_secs: None,
        session_day: None,
        universe_version: None,
        revision: None,
        max_age_secs: 2,
    }
}

fn winner_query() -> WinnerQuery {
    WinnerQuery {
        family: FamilyQuery::Stock,
        timeframe: "1s".to_owned(),
        feed: "dhan".to_owned(),
        session_day: START / 86_400,
        universe_version: BIG_ID,
        bucket_start_secs: START,
        metric: BucketVolumeMetric::SignedBarVolumeVsOneLotV3
            .as_str()
            .to_owned(),
        max_age_secs: 2,
        require_closed: true,
    }
}

fn regular_capture_close_now() -> Option<u32> {
    Some((START / 86_400) * 86_400 + 56_400)
}

fn next_day_capture_close_now() -> Option<u32> {
    regular_capture_close_now().map(|at| at + 86_400)
}

#[test]
fn final_minute_winner_exposes_the_revisable_local_window_beside_its_nominal_end() {
    use crate::bucket_top_volume::{BucketContract, BucketTopVolumeEngine};
    use tickvault_trading::candles::{CandleMetadata, CandleVolumeUpdate};

    let close = regular_capture_close_now().unwrap();
    let runtime = Arc::new(BucketTopVolumeRuntime::new());
    let definition = BucketContract {
        security_id: 1,
        segment: ExchangeSegment::NseFno,
        underlying_id: BIG_ID,
        family: OptionFamily::Stock,
        lot_size: 100,
        instrument_definition_version: BIG_ID,
    };
    let mut engine = BucketTopVolumeEngine::new(
        Feed::Dhan,
        START / 86_400,
        BIG_ID,
        BucketVolumeMetric::SignedBarVolumeVsOneLotV3,
        [definition],
        2,
    )
    .unwrap();
    let start = TfIndex::M1.bucket_start(close - 1);
    engine
        .apply(&CandleVolumeUpdate {
            feed: Feed::Dhan,
            security_id: 1,
            segment_code: ExchangeSegment::NseFno.binary_code(),
            tf: TfIndex::M1,
            session_day: START / 86_400,
            bucket_start_secs: start,
            bucket_end_secs: TfIndex::M1.bucket_end(start),
            gross_volume: 100,
            estimated_net_volume: Some(100),
            metadata: CandleMetadata {
                lot_size: 100,
                instrument_definition_version: BIG_ID,
                underlying_id: BIG_ID,
                family_code: 1,
            },
            volume_quality: 0,
            revision: 1,
            last_observed_secs: close - 1,
            quality: VolumeQuality {
                baseline_known: true,
                ..VolumeQuality::default()
            },
            closed: true,
        })
        .unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, TfIndex::M1, close)
        .unwrap();
    let mut state = ApiState {
        runtime: RuntimeSource::Isolated(runtime),
        now_ist_secs: regular_capture_close_now,
    };
    let mut query = winner_query();
    query.timeframe = "1m".to_owned();
    query.bucket_start_secs = start;
    let response = read_winner(&state, &query).unwrap();
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(json["header"]["bucket_start_secs"], start);
    assert_eq!(json["header"]["bucket_end_secs"], start + 60);
    assert_eq!(json["header"]["observation_window_end_secs"], close);
    assert_eq!(json["header"]["closure_basis"], "regular_capture_window_v1");
    assert_eq!(
        json["header"]["closure_finality"],
        "revisable_local_window_not_provider_finality"
    );
    assert_eq!(json["header"]["freshness"]["oldest_instrument_age_secs"], 1);
    assert_eq!(json["header"]["freshness"]["same_server_session_day"], true);
    state.now_ist_secs = next_day_capture_close_now;
    assert_eq!(
        read_winner(&state, &query).unwrap_err().code,
        "different_epoch"
    );
}

#[test]
fn one_minute_api_preserves_canonical_quantity_and_exact_bucket_selection() {
    use crate::bucket_top_volume::{BucketContract, BucketTopVolumeEngine};
    use tickvault_trading::candles::{CandleMetadata, CandleVolumeUpdate};

    let tf = TfIndex::M1;
    let bucket_start = (START / 86_400) * 86_400 + 9 * 3_600 + 15 * 60;
    let bucket_end = bucket_start + 60;
    assert_eq!(tf.bucket_start(START), bucket_start);
    let definitions = [(1, 100), (2, 25)].map(|(security_id, lot_size)| BucketContract {
        security_id,
        segment: ExchangeSegment::NseFno,
        underlying_id: BIG_ID,
        family: OptionFamily::Stock,
        lot_size,
        instrument_definition_version: BIG_ID,
    });
    let mut engine = BucketTopVolumeEngine::new(
        Feed::Dhan,
        START / 86_400,
        BIG_ID,
        BucketVolumeMetric::SignedBarVolumeVsOneLotV3,
        definitions,
        2,
    )
    .unwrap();
    for (definition, volume) in definitions.into_iter().zip([250_i64, 100]) {
        engine
            .apply(&CandleVolumeUpdate {
                feed: Feed::Dhan,
                security_id: definition.security_id,
                segment_code: definition.segment.binary_code(),
                tf,
                session_day: START / 86_400,
                bucket_start_secs: bucket_start,
                bucket_end_secs: bucket_end,
                gross_volume: volume.unsigned_abs(),
                estimated_net_volume: Some(volume),
                metadata: CandleMetadata {
                    lot_size: definition.lot_size,
                    instrument_definition_version: BIG_ID,
                    underlying_id: BIG_ID,
                    family_code: 1,
                },
                volume_quality: 0,
                revision: 1,
                last_observed_secs: START,
                quality: VolumeQuality {
                    baseline_known: true,
                    ..VolumeQuality::default()
                },
                closed: false,
            })
            .unwrap();
    }
    let runtime = Arc::new(BucketTopVolumeRuntime::new());
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, tf, START + 1)
        .unwrap();
    let state = ApiState {
        runtime: RuntimeSource::Isolated(runtime.clone()),
        now_ist_secs: now,
    };
    let mut winner_query = winner_query();
    winner_query.timeframe = "1m".to_owned();
    winner_query.bucket_start_secs = bucket_start;
    // A 09:15 observation is inside [09:15,09:16); selecting one
    // minute must not make it a closed or completed candle.
    assert_eq!(
        read_winner(&state, &winner_query).unwrap_err().code,
        "open_bucket"
    );
    winner_query.require_closed = false;
    let winner = read_winner(&state, &winner_query).unwrap();
    assert_eq!(winner.header.timeframe, "1m");
    assert_eq!(winner.header.bucket_start_secs, bucket_start);
    assert_eq!(winner.header.bucket_end_secs, bucket_end);
    assert_eq!(winner.header.observation_window_end_secs, Some(bucket_end));
    assert_eq!(winner.row.security_id, "2");
    assert_eq!(signed_quantity(&winner.row).volume.as_deref(), Some("100"));
    assert_eq!(winner.row.lot_size, "25");
    assert_eq!(
        signed_quantity(&winner.row).signed_lots.as_deref(),
        Some("4.000")
    );
    assert_eq!(
        signed_quantity(&winner.row).volume_chg_pct.as_deref(),
        Some("300.000")
    );
    assert!(runtime.load(OptionFamily::Stock, tf).is_none());

    engine
        .publish_latest(&runtime, OptionFamily::Stock, tf, START + 1)
        .unwrap();
    let mut list_query = list_query();
    list_query.timeframe = "1m".to_owned();
    list_query.bucket_start_secs = Some(bucket_start);
    let list = read_list(&state, &list_query).unwrap();
    assert_eq!(list.returned_rows, 2);
    assert_eq!(list.rows[0].security_id, winner.row.security_id);
    assert_eq!(
        signed_quantity(&list.rows[0]).volume,
        signed_quantity(&winner.row).volume
    );
    assert_eq!(
        signed_quantity(&list.rows[1]).volume.as_deref(),
        Some("250")
    );
    assert_eq!(
        signed_quantity(&list.rows[1]).volume_chg_pct.as_deref(),
        Some("150.000")
    );
    for wrong_bucket in [bucket_start + 1, bucket_end] {
        winner_query.bucket_start_secs = wrong_bucket;
        assert_eq!(
            read_winner(&state, &winner_query).unwrap_err().code,
            "different_bucket"
        );
        list_query.bucket_start_secs = Some(wrong_bucket);
        assert_eq!(
            read_list(&state, &list_query).unwrap_err().code,
            "different_bucket"
        );
    }
}

#[test]
fn winner_api_reads_exact_closed_predecessor_after_the_next_live_bucket_opens() {
    use crate::bucket_top_volume::{BucketContract, BucketTopVolumeEngine};
    use tickvault_trading::candles::{CandleMetadata, CandleVolumeUpdate};

    let runtime = Arc::new(BucketTopVolumeRuntime::new());
    let definition = BucketContract {
        security_id: 1,
        segment: ExchangeSegment::NseFno,
        underlying_id: BIG_ID,
        family: OptionFamily::Stock,
        lot_size: 100,
        instrument_definition_version: BIG_ID,
    };
    let mut engine = BucketTopVolumeEngine::new(
        Feed::Dhan,
        START / 86_400,
        BIG_ID,
        BucketVolumeMetric::SignedBarVolumeVsOneLotV3,
        [definition],
        2,
    )
    .unwrap();
    let mut candle = CandleVolumeUpdate {
        feed: Feed::Dhan,
        security_id: 1,
        segment_code: ExchangeSegment::NseFno.binary_code(),
        tf: TfIndex::S1,
        session_day: START / 86_400,
        bucket_start_secs: START,
        bucket_end_secs: START + 1,
        gross_volume: 150,
        estimated_net_volume: Some(150),
        metadata: CandleMetadata {
            lot_size: 100,
            instrument_definition_version: BIG_ID,
            underlying_id: BIG_ID,
            family_code: 1,
        },
        volume_quality: 0,
        revision: 1,
        last_observed_secs: START,
        quality: VolumeQuality {
            baseline_known: true,
            ..VolumeQuality::default()
        },
        closed: true,
    };
    engine.apply(&candle).unwrap();
    candle.bucket_start_secs += 1;
    candle.bucket_end_secs += 1;
    candle.last_observed_secs += 1;
    candle.gross_volume = 300;
    candle.estimated_net_volume = Some(-300);
    candle.closed = false;
    engine.apply(&candle).unwrap();
    engine
        .publish_winner_latest(&runtime, OptionFamily::Stock, TfIndex::S1, START + 1)
        .unwrap();
    let state = ApiState {
        runtime: RuntimeSource::Isolated(runtime),
        now_ist_secs: now,
    };
    let mut query = winner_query();
    let previous = read_winner(&state, &query).unwrap();
    assert!(previous.data_checks_passed);
    assert!(previous.require_closed);
    assert_eq!(previous.header.bucket_start_secs, START);
    assert_eq!(
        signed_quantity(&previous.row).volume.as_deref(),
        Some("150")
    );
    query.bucket_start_secs += 1;
    assert_eq!(read_winner(&state, &query).unwrap_err().code, "open_bucket");
    query.require_closed = false;
    let live = read_winner(&state, &query).unwrap();
    assert_eq!(live.header.bucket_start_secs, START + 1);
    assert_eq!(signed_quantity(&live.row).volume.as_deref(), Some("-300"));
    query.bucket_start_secs = START - 1;
    assert_eq!(
        read_winner(&state, &query).unwrap_err().code,
        "different_bucket"
    );
}

#[test]
fn list_preserves_exact_decimal_integers_signed_scores_and_quality_provenance() {
    let state = fixture(vec![row(BIG_ID, -50)], 1);
    let response = read_list(&state, &list_query()).unwrap();
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(json["kind"], "diagnostic_board");
    assert_eq!(json["header"]["universe_version"], BIG_ID.to_string());
    assert_eq!(
        json["header"]["timestamp_axis"],
        "IST_wall_clock_epoch_seconds"
    );
    assert_eq!(json["rows"][0]["security_id"], BIG_ID.to_string());
    assert!(json["rows"][0].get("gross_volume").is_none());
    assert_eq!(json["rows"][0]["volume"], "-50");
    assert_eq!(json["rows"][0]["signed_lots"], "-0.500");
    assert_eq!(json["rows"][0]["volume_chg_pct"], "-150.000");
    assert_eq!(json["rows"][0]["score_numerator"], "-15000");
    assert_eq!(json["rows"][0]["score_denominator"], "100");
    assert_eq!(json["rows"][0]["quality"]["baseline_known"], true);
    assert!(json.get("data_checks_passed").is_none());
}

#[test]
fn contract_39582_exposes_the_entire_39600_or_negative_30600_bar_volume() {
    for (volume, gross) in [(39_600_i64, 39_600_u64), (-30_600, 30_600)] {
        let candle_row = row(39_582, volume);
        assert_eq!(candle_row.gross_volume, gross);
        let state = fixture(vec![candle_row], 1);
        let list = serde_json::to_value(read_list(&state, &list_query()).unwrap()).unwrap();
        let winner = serde_json::to_value(read_winner(&state, &winner_query()).unwrap()).unwrap();
        for response in [&list, &winner] {
            assert_eq!(
                response["header"]["metric_version"],
                "signed_bar_volume_vs_one_lot_v3"
            );
            assert_eq!(
                response["header"]["quantity_basis"],
                "bar_close_vs_frozen_previous_close"
            );
            assert_eq!(
                response["header"]["metric_formula"],
                "100 * (volume / lot_size - 1)"
            );
        }
        for response_row in [&list["rows"][0], &winner["row"]] {
            assert_eq!(response_row["security_id"], "39582");
            assert_eq!(response_row["volume"], volume.to_string());
            assert_eq!(response_row["signed_lots"], format!("{}.000", volume / 100));
            assert_eq!(
                response_row["volume_chg_pct"],
                format!("{}.000", volume - 100)
            );
            for old_field in [
                "net_volume",
                "net_volume_chg_pct",
                "signed_net_lots",
                "gross_volume",
                "gross_lots",
            ] {
                assert!(
                    response_row.get(old_field).is_none(),
                    "unexpected field {old_field}"
                );
            }
        }
    }
}

#[test]
fn maximum_quantities_and_identifiers_remain_exact_json_decimal_strings() {
    for volume in [i64::MAX, -i64::MAX] {
        let mut maximum = row(u64::MAX, volume);
        maximum.lot_size = 1;
        maximum.instrument_definition_version = u64::MAX;
        let state = fixture(vec![maximum], 1);
        let json = serde_json::to_value(read_winner(&state, &winner_query()).unwrap()).unwrap();
        let response_row = &json["row"];
        assert_eq!(response_row["security_id"], u64::MAX.to_string());
        assert_eq!(
            response_row["instrument_definition_version"],
            u64::MAX.to_string()
        );
        assert_eq!(response_row["volume"], volume.to_string());
        assert_eq!(response_row["signed_lots"], format!("{volume}.000"));
        let score = (i128::from(volume) - 1) * 100;
        assert_eq!(response_row["volume_chg_pct"], format!("{score}.000"));
        assert_eq!(response_row["score_numerator"], score.to_string());
        assert_eq!(response_row["score_denominator"], "1");
        assert_eq!(
            response_row["volume"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .unwrap(),
            volume
        );
    }
}

#[test]
fn absent_direction_is_null_and_known_tie_is_zero_with_negative_one_lot_change() {
    let mut missing = row(1, 0);
    missing.gross_volume = 39_600;
    missing.estimated_net_volume = None;
    missing.quality.baseline_known = false;
    let response = RowResponse::from_row(missing, 1, BucketVolumeMetric::SignedBarVolumeVsOneLotV3);
    assert!(signed_quantity(&response).volume.is_none());
    assert!(signed_quantity(&response).signed_lots.is_none());
    assert!(signed_quantity(&response).volume_chg_pct.is_none());
    assert!(response.score_numerator.is_none());
    let json = serde_json::to_value(response).unwrap();
    for field in [
        "volume",
        "signed_lots",
        "volume_chg_pct",
        "score_numerator",
        "score_denominator",
    ] {
        assert_eq!(json.get(field), Some(&serde_json::Value::Null), "{field}");
    }
    assert_eq!(json["quality"]["baseline_known"], false);
    let mut tied = row(1, 0);
    tied.gross_volume = 39_600;
    let response = RowResponse::from_row(tied, 1, BucketVolumeMetric::SignedBarVolumeVsOneLotV3);
    assert_eq!(signed_quantity(&response).volume.as_deref(), Some("0"));
    assert_eq!(
        signed_quantity(&response).signed_lots.as_deref(),
        Some("0.000")
    );
    assert_eq!(
        signed_quantity(&response).volume_chg_pct.as_deref(),
        Some("-100.000")
    );
}

#[test]
fn winner_accepts_only_v3_and_does_not_relabel_a_legacy_publication() {
    assert_eq!(
        parse_metric("signed_bar_volume_vs_one_lot_v3").unwrap(),
        BucketVolumeMetric::SignedBarVolumeVsOneLotV3,
    );
    let current = fixture(vec![row(1, 100)], 1);
    for unsupported in [
        "signed_estimated_net_vs_one_lot_v2",
        "gross_activity_vs_one_lot_v1",
        "signed_bar_volume_vs_one_lot_v2",
        "signed_bar_volume_vs_one_lot_v3 ",
        "",
    ] {
        let mut query = winner_query();
        query.metric = unsupported.to_owned();
        assert_eq!(
            read_winner(&current, &query).unwrap_err().code,
            "invalid_metric"
        );
    }
    for (metric, basis, formula, score) in [
        (
            BucketVolumeMetric::SignedEstimatedNetVsOneLotV2,
            "candle_tick_rule_estimate",
            "100 * (estimated_net_volume / lot_size - 1)",
            "-50.000",
        ),
        (
            BucketVolumeMetric::GrossActivityVsOneLotV1,
            "candle_gross_volume",
            "100 * (gross_volume / lot_size - 1)",
            "0.000",
        ),
    ] {
        let mut legacy_row = row(1, 50);
        legacy_row.gross_volume = 100;
        let legacy = fixture_with_metric(vec![legacy_row], 1, metric);
        assert_eq!(
            read_winner(&legacy, &winner_query()).unwrap_err().code,
            "different_epoch"
        );
        let json = serde_json::to_value(read_list(&legacy, &list_query()).unwrap()).unwrap();
        assert_eq!(json["header"]["metric_version"], metric.as_str());
        assert_eq!(json["header"]["quantity_basis"], basis);
        assert_eq!(json["header"]["metric_formula"], formula);
        assert_eq!(json["rows"][0]["gross_volume"], "100");
        assert_eq!(json["rows"][0]["net_volume"], "50");
        assert_eq!(json["rows"][0]["net_volume_chg_pct"], score);
        assert!(json["rows"][0].get("volume").is_none());
        assert!(json["rows"][0].get("volume_chg_pct").is_none());
    }
}

#[test]
fn pagination_is_bounded_and_requires_one_bucket_epoch_and_revision() {
    let rows = (0..300)
        .map(|index| row(index as u64 + 1, 300 - index as i64))
        .collect();
    let state = fixture(rows, 300);
    let mut query = list_query();
    query.limit = 250;
    let first = read_list(&state, &query).unwrap();
    assert_eq!(first.returned_rows, 250);
    assert_eq!(first.total_eligible_rows, 300);
    assert_eq!(first.next_offset, Some(250));
    assert_eq!(first.rows[249].rank, 250);
    query.offset = 250;
    assert_eq!(
        read_list(&state, &query).unwrap_err().code,
        "page_identity_required"
    );
    query.bucket_start_secs = Some(START);
    query.session_day = Some(START / 86_400);
    query.universe_version = Some(BIG_ID);
    query.revision = Some(5);
    let second = read_list(&state, &query).unwrap();
    assert_eq!(second.returned_rows, 50);
    assert_eq!(second.rows[0].rank, 251);
    assert_eq!(second.rows[49].rank, 300);
    assert!(second.next_offset.is_none());
    query.revision = Some(4);
    assert_eq!(
        read_list(&state, &query).unwrap_err().code,
        "different_revision"
    );
    query.limit = 251;
    assert_eq!(read_list(&state, &query).unwrap_err().code, "invalid_page");
    query.limit = 0;
    assert_eq!(read_list(&state, &query).unwrap_err().code, "invalid_page");
}

#[test]
fn exact_bucket_request_refuses_latest_from_another_bucket() {
    let state = fixture(vec![row(1, 10)], 1);
    let mut query = list_query();
    query.bucket_start_secs = Some(START - 1);
    assert_eq!(
        read_list(&state, &query).unwrap_err().code,
        "different_bucket"
    );
    query.bucket_start_secs = Some(START);
    query.universe_version = Some(BIG_ID - 1);
    assert_eq!(
        read_list(&state, &query).unwrap_err().code,
        "different_epoch"
    );
}

#[test]
fn winner_uses_strict_runtime_gates_for_coverage_epoch_bucket_open_and_age() {
    let state = fixture(vec![row(1, -50)], 1);
    let mut query = winner_query();
    let result = read_winner(&state, &query).unwrap();
    assert!(result.data_checks_passed);
    assert_eq!(signed_quantity(&result.row).volume.as_deref(), Some("-50"));
    query.universe_version -= 1;
    assert_eq!(
        read_winner(&state, &query).unwrap_err().code,
        "different_epoch"
    );
    query = winner_query();
    query.bucket_start_secs -= 1;
    assert_eq!(
        read_winner(&state, &query).unwrap_err().code,
        "different_bucket"
    );
    query = winner_query();
    let incomplete = fixture(vec![row(1, 10)], 2);
    assert_eq!(
        read_winner(&incomplete, &query).unwrap_err().code,
        "incomplete_universe"
    );
    let mut open_row = row(1, 10);
    open_row.closed = false;
    let open = fixture(vec![open_row], 1);
    assert_eq!(read_winner(&open, &query).unwrap_err().code, "open_bucket");
    query.require_closed = false;
    assert!(read_winner(&open, &query).unwrap().data_checks_passed);
    let mut stale = state.clone();
    stale.now_ist_secs = late_now;
    assert_eq!(
        read_winner(&stale, &query).unwrap_err().code,
        "stale_publication"
    );
    query.max_age_secs = MAX_TOP_VOLUME_AGE_SECS + 1;
    assert_eq!(
        read_winner(&stale, &query).unwrap_err().code,
        "invalid_max_age"
    );
}

#[test]
fn diagnostic_board_reports_incomplete_and_stale_without_claiming_decision_approval() {
    let mut state = fixture(vec![row(1, 10)], 2);
    state.now_ist_secs = late_now;
    let response = read_list(&state, &list_query()).unwrap();
    assert!(!response.header.coverage.complete_for_declared_universe);
    assert!(!response.header.freshness.publication_within_age);
    assert!(!response.header.freshness.instruments_within_age);
    assert_eq!(response.kind, "diagnostic_board");
    state.now_ist_secs = clock_unavailable;
    assert_eq!(
        read_list(&state, &list_query()).unwrap_err().code,
        "server_clock_out_of_range"
    );
    assert_eq!(
        read_winner(&state, &winner_query()).unwrap_err().code,
        "server_clock_out_of_range"
    );
}

#[test]
fn timeframe_parser_admits_exactly_the_requested_four_top_volume_selectors() {
    let expected = [
        ("1s", TfIndex::S1),
        ("3s", TfIndex::S3),
        ("5s", TfIndex::S5),
        ("1m", TfIndex::M1),
    ];
    assert_eq!(TfIndex::TOP_VOLUME_ALL, expected.map(|(_, tf)| tf));
    for (selector, tf) in expected {
        assert_eq!(parse_timeframe(selector).unwrap(), tf);
        assert_eq!(tf.table_name().trim_start_matches("candles_"), selector);
    }
}

#[test]
fn candle_only_retired_and_noncanonical_frames_are_refused_by_both_api_read_paths() {
    let state = fixture(vec![row(1, 10)], 1);
    for invalid in [
        "3m",
        "5m",
        "10m",
        "15m",
        "30m",
        "60m",
        "2s",
        "4s",
        "6s",
        "7s",
        "8s",
        "9s",
        "10s",
        "11s",
        "12s",
        "13s",
        "14s",
        "15s",
        "30s",
        "2m",
        "1d",
        "",
        "1h",
        "60s",
        "600s",
        "ALL",
        "1s;DROP",
        "-1s",
        "0s",
        "10M",
        " 10m",
        "10m ",
        "10.0m",
        "candles_10m",
    ] {
        assert_eq!(
            parse_timeframe(invalid).unwrap_err().code,
            "invalid_timeframe",
            "{invalid:?}",
        );
        let mut list = list_query();
        list.timeframe = invalid.to_owned();
        assert_eq!(
            read_list(&state, &list).unwrap_err().code,
            "invalid_timeframe"
        );
        let mut winner = winner_query();
        winner.timeframe = invalid.to_owned();
        assert_eq!(
            read_winner(&state, &winner).unwrap_err().code,
            "invalid_timeframe"
        );
    }
}

struct TestServer {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(router: Router) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    TestServer {
        base: format!("http://{address}"),
        task,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap()
}

#[tokio::test]
async fn build_candle_top_volume_router_refuses_disabled_auth() {
    let server = serve(build_candle_top_volume_router(ApiAuthConfig::disabled())).await;
    for path in ["/api/top-volume", "/api/top-volume/winner"] {
        let result = client()
            .get(format!("{}{path}", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = result.json().await.unwrap();
        assert_eq!(body["error"]["code"], "authentication_unavailable");
    }
}

#[tokio::test]
async fn router_authentication_precedes_query_parsing_and_preserves_token_rotation() {
    let auth = ApiAuthConfig::from_token(SecretString::from("unit-test-token-a".to_owned()));
    let server = serve(router_with_state(
        auth.clone(),
        fixture(vec![row(1, -50)], 1),
    ))
    .await;
    let client = client();
    let url = format!("{}/api/top-volume?family=stock&timeframe=1s", server.base);
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("wrong-test-token")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let valid = client
        .get(&url)
        .bearer_auth("unit-test-token-a")
        .send()
        .await
        .unwrap();
    assert_eq!(valid.status(), StatusCode::OK);
    assert_eq!(valid.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        valid.json::<serde_json::Value>().await.unwrap()["rows"][0]["volume"],
        "-50"
    );
    let invalid_query = format!("{url}&now_secs=0");
    assert_eq!(
        client.get(&invalid_query).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&invalid_query)
            .bearer_auth("unit-test-token-a")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let _rotation = auth.rotate_bearer_token(SecretString::from("unit-test-token-b".to_owned()));
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("unit-test-token-a")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("unit-test-token-b")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .post(&url)
            .bearer_auth("unit-test-token-b")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn winner_route_requires_explicit_epoch_and_does_not_accept_a_caller_clock() {
    let auth = ApiAuthConfig::from_token(SecretString::from("unit-test-token".to_owned()));
    let server = serve(router_with_state(auth, fixture(vec![row(1, 100)], 1))).await;
    let url = format!(
        "{}/api/top-volume/winner?family=stock&timeframe=1s&feed=dhan&session_day={}&universe_version={BIG_ID}&bucket_start_secs={START}&metric={}",
        server.base,
        START / 86_400,
        BucketVolumeMetric::SignedBarVolumeVsOneLotV3.as_str(),
    );
    let client = client();
    let result = client
        .get(&url)
        .bearer_auth("unit-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::OK);
    let body: serde_json::Value = result.json().await.unwrap();
    assert_eq!(body["data_checks_passed"], true);
    assert_eq!(
        body["header"]["metric_version"],
        "signed_bar_volume_vs_one_lot_v3"
    );
    assert_eq!(body["row"]["volume"], "100");
    assert_eq!(body["row"]["volume_chg_pct"], "0.000");
    assert!(body["row"].get("net_volume").is_none());
    assert!(body["row"].get("gross_volume").is_none());
    let legacy_url = url.replace(
        BucketVolumeMetric::SignedBarVolumeVsOneLotV3.as_str(),
        BucketVolumeMetric::SignedEstimatedNetVsOneLotV2.as_str(),
    );
    let legacy = client
        .get(legacy_url)
        .bearer_auth("unit-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::BAD_REQUEST);
    assert_eq!(legacy.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        legacy.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "invalid_metric"
    );
    let missing = format!(
        "{}/api/top-volume/winner?family=stock&timeframe=1s",
        server.base
    );
    assert_eq!(
        client
            .get(missing)
            .bearer_auth("unit-test-token")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(format!("{url}&now_secs={START}"))
            .bearer_auth("unit-test-token")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}
