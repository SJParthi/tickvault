//! Coverage for the public, clock-free parts of `expiry_scope_check` that the
//! in-file tests leave unrun: the empty-row branch of the candle-row parser
//! and the logging/day-memory branches of the subscribe-time recorder.

mod cov_xvs_support;

use tickvault_app::expiry_scope_check::{
    SubscribeScopeReport, parse_candle_contract_rows, record_subscribe_expiry_scope,
    subscribe_refused_for_day,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

#[test]
fn test_expiry_scope_check_parse_candle_contract_rows_empty_row_reads_id_minus_one() {
    let body = r#"{"dataset":[[], [" 42 ","NSE_FNO","  ABC-28Nov2026-100-CE  "], [7.5,"NSE_FNO",""], "not-an-array", [99]]}"#;
    let rows = parse_candle_contract_rows(body).expect("valid body parses");
    // The bare string entry is skipped; the other four rows are kept.
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].security_id, -1, "an empty row has no id");
    assert_eq!(rows[0].segment, "");
    assert_eq!(rows[0].contract, None);
    assert_eq!(rows[1].security_id, 42, "a string id is trimmed and parsed");
    assert_eq!(rows[1].segment, "NSE_FNO");
    assert_eq!(rows[1].contract.as_deref(), Some("ABC-28Nov2026-100-CE"));
    assert_eq!(rows[2].security_id, -1, "a non-integer number is not an id");
    assert_eq!(rows[2].contract, None, "an empty name reads as unlabelled");
    assert_eq!(rows[3].security_id, 99);
    assert_eq!(rows[3].segment, "", "a missing segment column reads empty");
}

#[test]
fn test_expiry_scope_check_parse_candle_contract_rows_rejects_bad_bodies() {
    assert!(parse_candle_contract_rows("").is_err());
    assert!(parse_candle_contract_rows("{not json").is_err());
    let no_dataset = parse_candle_contract_rows(r#"{"columns":[]}"#).unwrap_err();
    assert!(no_dataset.contains("dataset"), "{no_dataset}");
    assert!(parse_candle_contract_rows(r#"{"dataset":{}}"#).is_err());
    assert_eq!(
        parse_candle_contract_rows(r#"{"dataset":[]}"#).map(|r| r.len()),
        Ok(0)
    );
}

fn instrument(id: u64) -> SubscribeInstrument {
    SubscribeInstrument {
        security_id: id,
        segment: ExchangeSegment::NseFno,
    }
}

/// One test, in order, because the recorder's day memory is process-wide.
#[test]
fn test_expiry_scope_check_record_subscribe_expiry_scope_day_memory_and_repeat_branches() {
    cov_xvs_support::init_tracing();
    // Far-future days no other test in this binary touches.
    let day_a = 20_990_101_u32;
    let day_b = 20_990_102_u32;

    let refused = SubscribeScopeReport {
        checked: 10,
        off_expiry: 2,
        expired: 1,
        unknown: 0,
        refused: vec![instrument(11), instrument(12), instrument(13)],
        sample: Some("ABC-28Nov2026-100-CE".to_owned()),
    };
    // First sight of the set: the coded error branch.
    record_subscribe_expiry_scope(&refused, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));
    assert_eq!(subscribe_refused_for_day(day_b), None);

    // The same set again on a later attach attempt: the repeat branch.
    record_subscribe_expiry_scope(&refused, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));

    // A smaller set the same day keeps the day's maximum.
    let smaller = SubscribeScopeReport {
        refused: vec![instrument(11)],
        sample: None,
        ..refused.clone()
    };
    record_subscribe_expiry_scope(&smaller, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));

    // A clean check the same day keeps it too.
    let clean = SubscribeScopeReport {
        checked: 10,
        ..SubscribeScopeReport::default()
    };
    record_subscribe_expiry_scope(&clean, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));

    // A new day resets the memory to that day's count.
    record_subscribe_expiry_scope(&clean, day_b);
    assert_eq!(subscribe_refused_for_day(day_b), Some(0));
    assert_eq!(
        subscribe_refused_for_day(day_a),
        None,
        "only one day is held"
    );

    // A bigger set on the new day raises it; the same set again is a repeat.
    let bigger = SubscribeScopeReport {
        refused: (100..110).map(instrument).collect(),
        sample: Some("XYZ".to_owned()),
        ..refused.clone()
    };
    record_subscribe_expiry_scope(&bigger, day_b);
    record_subscribe_expiry_scope(&bigger, day_b);
    assert_eq!(subscribe_refused_for_day(day_b), Some(10));
}
