//! Coverage for the public, clock-free parts of `expiry_scope_check` that the
//! in-file tests leave unrun: the empty-row branch of the candle-row parser
//! and the logging/day-memory branches of the subscribe-time recorder.

mod cov_xvs_support;

use tickvault_app::expiry_scope_check::{
    SubscribeScopeReport, parse_candle_contract_rows, record_subscribe_expiry_scope,
    subscribe_refused_for_day,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

#[test]
fn test_expiry_scope_check_parse_candle_contract_rows_empty_row_reads_id_minus_one() {
    cov_xvs_support::init_no_proxy();
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
    cov_xvs_support::init_no_proxy();
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
    instrument_in(id, ExchangeSegment::NseFno)
}

fn instrument_in(id: u64, segment: ExchangeSegment) -> SubscribeInstrument {
    SubscribeInstrument {
        security_id: id,
        segment,
    }
}

/// `(coded EXPIRY-SCOPE-01 refusal lines, "refused again" repeat lines)`
/// captured so far in this binary. Only the recorder test below logs either.
fn refusal_and_repeat_lines() -> (usize, usize) {
    let code = ErrorCode::ExpiryScope01OffExpiry.code_str();
    let events = cov_xvs_support::captured_events();
    let coded = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::ERROR
                && e.field("code") == Some(code)
                && e.field("stage") == Some("subscribe")
        })
        .count();
    let repeats = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::DEBUG
                && e.message()
                    .contains("the same contracts were refused again")
        })
        .count();
    (coded, repeats)
}

/// One test, in order, because the recorder's day memory is process-wide.
#[test]
fn test_expiry_scope_check_record_subscribe_expiry_scope_day_memory_and_repeat_branches() {
    cov_xvs_support::init_no_proxy();
    cov_xvs_support::init_tracing();
    // Far-future days no other test in this binary touches.
    let day_a = 20_990_101_u32;
    let day_b = 20_990_102_u32;
    let (coded0, repeats0) = refusal_and_repeat_lines();

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
    assert_eq!(
        refusal_and_repeat_lines(),
        (coded0 + 1, repeats0),
        "first sight of a refused set is one coded line"
    );

    // The same set again on a later attach attempt: the repeat branch, so the
    // coded line appears exactly once across first + repeat.
    record_subscribe_expiry_scope(&refused, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));
    assert_eq!(
        refusal_and_repeat_lines(),
        (coded0 + 1, repeats0 + 1),
        "the same set again is a repeat, not a second coded line"
    );

    // The same set in REVERSE order is the same set: still a repeat.
    let reversed = SubscribeScopeReport {
        refused: vec![instrument(13), instrument(12), instrument(11)],
        ..refused.clone()
    };
    record_subscribe_expiry_scope(&reversed, day_a);
    assert_eq!(
        refusal_and_repeat_lines(),
        (coded0 + 1, repeats0 + 2),
        "order does not make a new set"
    );

    // The same ids with 11 in BSE_FNO instead of NSE_FNO is a NEW set: the
    // key is (security_id, exchange_segment), never the id alone.
    let other_segment = SubscribeScopeReport {
        refused: vec![
            instrument_in(11, ExchangeSegment::BseFno),
            instrument(12),
            instrument(13),
        ],
        ..refused.clone()
    };
    record_subscribe_expiry_scope(&other_segment, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));
    assert_eq!(
        refusal_and_repeat_lines(),
        (coded0 + 2, repeats0 + 2),
        "id 11 in another segment is another contract, so a new set"
    );

    // A smaller set the same day keeps the day's maximum (and, being a new
    // set, logs once).
    let smaller = SubscribeScopeReport {
        refused: vec![instrument(11)],
        sample: None,
        ..refused.clone()
    };
    record_subscribe_expiry_scope(&smaller, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));
    assert_eq!(refusal_and_repeat_lines(), (coded0 + 3, repeats0 + 2));

    // A clean check the same day keeps it too, and logs no refusal.
    let clean = SubscribeScopeReport {
        checked: 10,
        ..SubscribeScopeReport::default()
    };
    record_subscribe_expiry_scope(&clean, day_a);
    assert_eq!(subscribe_refused_for_day(day_a), Some(3));
    assert_eq!(refusal_and_repeat_lines(), (coded0 + 3, repeats0 + 2));

    // A new day resets the memory to that day's count.
    record_subscribe_expiry_scope(&clean, day_b);
    assert_eq!(subscribe_refused_for_day(day_b), Some(0));
    assert_eq!(
        subscribe_refused_for_day(day_a),
        None,
        "only one day is held"
    );

    // A set already logged on day A is new again on day B.
    record_subscribe_expiry_scope(&refused, day_b);
    assert_eq!(refusal_and_repeat_lines(), (coded0 + 4, repeats0 + 2));

    // A bigger set on the new day raises it; the same set again is a repeat.
    let bigger = SubscribeScopeReport {
        refused: (100..110).map(instrument).collect(),
        sample: Some("XYZ".to_owned()),
        ..refused.clone()
    };
    record_subscribe_expiry_scope(&bigger, day_b);
    record_subscribe_expiry_scope(&bigger, day_b);
    assert_eq!(subscribe_refused_for_day(day_b), Some(10));
    assert_eq!(refusal_and_repeat_lines(), (coded0 + 5, repeats0 + 3));
}
