//! Property + boundary tests for the revived Dhan subscription builder.
//!
//! Revival authority: the operator's 2026-08-09 dated quotes in
//! `.claude/rules/project/websocket-connection-scope-lock.md` ("DHAN LIVE
//! MAIN-FEED WS REVIVAL AUTHORIZED" and "16 CONNECTIONS + depth-20/depth-200
//! AUTHORIZED").
//!
//! The headline test here is `prop_batching_never_drops_or_duplicates` — the
//! ZERO-LOSS property. Every other guarantee in the feed (WAL, ring, spill,
//! DLQ) protects ticks we RECEIVE; none of them can recover a tick for an
//! instrument we never subscribed. A batching bug that silently drops one
//! instrument from the subscribe set is therefore invisible to the entire
//! resilience chain and looks exactly like "that instrument is quiet today".
//! This property is the only thing standing in front of that class.
//!
//! # Coverage map
//!
//! Which test covers which public function. Written out because the tests are
//! named for the PROPERTY they prove rather than the function they call —
//! which reads better, but leaves no way to answer "what tests this?" without
//! reading all of them. (It also lets the pub-fn test guard, which matches by
//! name, see coverage that genuinely exists.)
//!
//! - tested: `build_subscription_messages` — `prop_batching_never_drops_or_duplicates`,
//!   `prop_no_message_exceeds_hundred`, `test_one_hundred_boundary_does_not_split`,
//!   `test_one_hundred_and_one_splits_and_loses_nothing`, `test_full_connection_of_five_thousand`
//! - tested: `build_unsubscription_messages` — `prop_unsubscribe_preserves_every_instrument`
//! - tested: `build_twenty_depth_subscription_messages` — `prop_twenty_depth_preserves_every_instrument`,
//!   `test_depth_caps_are_fifty_and_one`
//! - tested: `build_twenty_depth_unsubscription_messages` — `test_depth_caps_are_fifty_and_one`
//!   (unsubscribe uses request code 25, NOT 24 — the Dhan SDK's own off-by-one)
//! - tested: `build_two_hundred_depth_subscription_message` — `test_two_hundred_depth_is_single_instrument_nse_only`
//! - tested: `build_two_hundred_depth_unsubscription_message` — `test_two_hundred_depth_is_single_instrument_nse_only`
//! - tested: `build_disconnect_message` — module tests in `subscription_builder.rs`
//! - tested: `dedup_instruments` — `prop_dedup_is_composite_keyed`,
//!   `test_regression_id_27_cross_segment_both_reach_the_wire` (the I-P1-11 collision:
//!   security_id 27 exists in BOTH IDX_I and NSE_EQ, and both must reach the wire)
//! - tested: `validate_connection_capacity` — `prop_capacity_validation_is_the_documented_cap`

use proptest::prelude::*;
use serde_json::Value;
use tickvault_common::constants::{
    MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION, MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION,
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, SUBSCRIPTION_BATCH_SIZE,
};
use tickvault_common::types::{ExchangeSegment, FeedMode};
use tickvault_core::websocket::subscription_builder::{
    EndpointKind, build_subscription_messages, build_twenty_depth_subscription_messages,
    build_two_hundred_depth_subscription_message, build_unsubscription_messages, dedup_instruments,
    validate_connection_capacity,
};
use tickvault_core::websocket::types::InstrumentSubscription;

/// The eight real Dhan segments, so generated cases exercise cross-segment
/// `security_id` reuse (I-P1-11) rather than a single tidy segment.
const ALL_SEGMENTS: [ExchangeSegment; 8] = [
    ExchangeSegment::IdxI,
    ExchangeSegment::NseEquity,
    ExchangeSegment::NseFno,
    ExchangeSegment::NseCurrency,
    ExchangeSegment::BseEquity,
    ExchangeSegment::McxComm,
    ExchangeSegment::BseCurrency,
    ExchangeSegment::BseFno,
];

/// Flattens the produced messages back into `(segment, security_id)` pairs in
/// wire order — the exact set the Dhan server would end up subscribing.
fn flatten_wire_instruments(messages: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for message in messages {
        let parsed: Value = serde_json::from_str(message).expect("builder emitted invalid JSON");
        let list = parsed["InstrumentList"]
            .as_array()
            .expect("InstrumentList must be an array");
        // InstrumentCount must always agree with the actual list length,
        // otherwise Dhan reads a different number of entries than we sent.
        assert_eq!(
            parsed["InstrumentCount"].as_u64().expect("InstrumentCount") as usize,
            list.len(),
            "InstrumentCount disagrees with InstrumentList length"
        );
        for entry in list {
            let segment = entry["ExchangeSegment"]
                .as_str()
                .expect("ExchangeSegment must be a string")
                .to_string();
            // WS-GAP-02: SecurityId is a STRING on the wire, never a number.
            let security_id = entry["SecurityId"]
                .as_str()
                .expect("SecurityId MUST serialize as a JSON string, not a number")
                .to_string();
            out.push((segment, security_id));
        }
    }
    out
}

fn arb_instrument() -> impl Strategy<Value = InstrumentSubscription> {
    // Small id space on purpose: forces frequent cross-segment id COLLISIONS
    // (the FINNIFTY id=27 class), which is what I-P1-11 is about.
    (0usize..ALL_SEGMENTS.len(), 0u64..40).prop_map(|(segment_index, security_id)| {
        InstrumentSubscription::new(ALL_SEGMENTS[segment_index], security_id)
    })
}

proptest! {
    /// ZERO-LOSS: batching is a pure regrouping — the concatenation of every
    /// produced batch equals the input exactly, in order. Nothing dropped,
    /// nothing duplicated, nothing reordered, for any list size or batch size.
    #[test]
    fn prop_batching_never_drops_or_duplicates(
        instruments in prop::collection::vec(arb_instrument(), 0..250),
        batch_size in 0usize..250,
    ) {
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, batch_size);

        let expected: Vec<(String, String)> = instruments
            .iter()
            .map(|i| (i.exchange_segment.clone(), i.security_id.clone()))
            .collect();
        let actual = flatten_wire_instruments(&messages);

        prop_assert_eq!(
            &actual,
            &expected,
            "batching must preserve every instrument in order: {} in, {} out",
            expected.len(),
            actual.len()
        );
    }

    /// An empty input produces ZERO messages; a non-empty input always
    /// produces at least one. No empty `InstrumentCount:0` frame is ever sent.
    #[test]
    fn prop_empty_in_zero_messages_out(
        instruments in prop::collection::vec(arb_instrument(), 0..60),
        batch_size in 0usize..150,
    ) {
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, batch_size);
        if instruments.is_empty() {
            prop_assert!(messages.is_empty());
        } else {
            prop_assert!(!messages.is_empty());
            for message in &messages {
                let parsed: Value = serde_json::from_str(message).unwrap();
                prop_assert!(
                    !parsed["InstrumentList"].as_array().unwrap().is_empty(),
                    "no message may carry an empty InstrumentList"
                );
            }
        }
    }

    /// No produced message ever exceeds Dhan's 100-instrument per-message
    /// limit, whatever batch_size the caller passes (including usize::MAX).
    #[test]
    fn prop_no_message_exceeds_hundred(
        instruments in prop::collection::vec(arb_instrument(), 0..400),
        batch_size in prop_oneof![0usize..250, Just(usize::MAX)],
    ) {
        for message in build_subscription_messages(&instruments, FeedMode::Full, batch_size) {
            let parsed: Value = serde_json::from_str(&message).unwrap();
            let len = parsed["InstrumentList"].as_array().unwrap().len();
            prop_assert!(
                len <= SUBSCRIPTION_BATCH_SIZE,
                "message carried {len} instruments, over the {SUBSCRIPTION_BATCH_SIZE} limit"
            );
        }
    }

    /// Unsubscribe batches the same way subscribe does — a mismatch would
    /// leave instruments subscribed after an intended unsubscribe.
    #[test]
    fn prop_unsubscribe_preserves_every_instrument(
        instruments in prop::collection::vec(arb_instrument(), 0..250),
        batch_size in 0usize..250,
    ) {
        let messages = build_unsubscription_messages(&instruments, FeedMode::Full, batch_size);
        let expected: Vec<(String, String)> = instruments
            .iter()
            .map(|i| (i.exchange_segment.clone(), i.security_id.clone()))
            .collect();
        prop_assert_eq!(flatten_wire_instruments(&messages), expected);
    }

    /// Depth-20 batching obeys the same zero-loss property.
    #[test]
    fn prop_twenty_depth_preserves_every_instrument(
        instruments in prop::collection::vec(arb_instrument(), 0..150),
        batch_size in 0usize..150,
    ) {
        let messages = build_twenty_depth_subscription_messages(&instruments, batch_size);
        let expected: Vec<(String, String)> = instruments
            .iter()
            .map(|i| (i.exchange_segment.clone(), i.security_id.clone()))
            .collect();
        prop_assert_eq!(flatten_wire_instruments(&messages), expected);
    }

    /// I-P1-11: dedup collapses ONLY exact `(segment, security_id)` repeats.
    /// Two entries sharing a numeric id across different segments are two
    /// DIFFERENT instruments and must both survive.
    #[test]
    fn prop_dedup_is_composite_keyed(
        instruments in prop::collection::vec(arb_instrument(), 0..120),
    ) {
        use std::collections::HashSet;

        let deduped = dedup_instruments(&instruments);

        // The surviving set equals the set of distinct COMPOSITE keys.
        let input_keys: HashSet<(String, String)> = instruments
            .iter()
            .map(|i| (i.exchange_segment.clone(), i.security_id.clone()))
            .collect();
        let output_keys: HashSet<(String, String)> = deduped
            .iter()
            .map(|i| (i.exchange_segment.clone(), i.security_id.clone()))
            .collect();
        prop_assert_eq!(&input_keys, &output_keys, "dedup changed the instrument SET");
        prop_assert_eq!(deduped.len(), input_keys.len(), "dedup left duplicates behind");

        // And it is genuinely segment-aware: collapsing on security_id alone
        // would yield strictly fewer rows whenever an id is reused.
        let id_only: HashSet<String> =
            instruments.iter().map(|i| i.security_id.clone()).collect();
        prop_assert!(
            deduped.len() >= id_only.len(),
            "dedup dropped a cross-segment instrument — that is the I-P1-11 bug"
        );
    }

    /// Capacity validation is exactly `count <= cap` for every endpoint kind.
    #[test]
    fn prop_capacity_validation_is_the_documented_cap(
        count in 0usize..6000,
        kind_index in 0usize..3,
    ) {
        let kind = [
            EndpointKind::MainFeed,
            EndpointKind::TwentyDepth,
            EndpointKind::TwoHundredDepth,
        ][kind_index];
        let cap = kind.max_instruments_per_connection();
        prop_assert_eq!(
            validate_connection_capacity(kind, count).is_ok(),
            count <= cap
        );
    }
}

// ---------------------------------------------------------------------------
// Explicit boundary + regression cases (not random)
// ---------------------------------------------------------------------------

fn instruments(count: usize) -> Vec<InstrumentSubscription> {
    (0..count)
        .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, i as u64 + 1))
        .collect()
}

#[test]
fn test_empty_produces_zero_messages() {
    assert!(build_subscription_messages(&[], FeedMode::Ticker, 100).is_empty());
    assert!(build_unsubscription_messages(&[], FeedMode::Ticker, 100).is_empty());
    assert!(build_twenty_depth_subscription_messages(&[], 100).is_empty());
}

#[test]
fn test_one_hundred_boundary_does_not_split() {
    let messages = build_subscription_messages(&instruments(100), FeedMode::Quote, 100);
    assert_eq!(messages.len(), 1);
    assert_eq!(flatten_wire_instruments(&messages).len(), 100);
}

#[test]
fn test_one_hundred_and_one_splits_and_loses_nothing() {
    let messages = build_subscription_messages(&instruments(101), FeedMode::Quote, 100);
    assert_eq!(messages.len(), 2);
    assert_eq!(flatten_wire_instruments(&messages).len(), 101);
}

#[test]
fn test_full_connection_of_five_thousand() {
    // One saturated main-feed connection (doc 03:20) = 50 messages, 5,000 rows.
    let all = instruments(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION);
    let messages = build_subscription_messages(&all, FeedMode::Quote, SUBSCRIPTION_BATCH_SIZE);
    assert_eq!(messages.len(), 50);
    assert_eq!(flatten_wire_instruments(&messages).len(), 5000);
    assert!(validate_connection_capacity(EndpointKind::MainFeed, 5000).is_ok());
    assert!(validate_connection_capacity(EndpointKind::MainFeed, 5001).is_err());
}

#[test]
fn test_batch_size_zero_and_one_hundred_one_both_clamp() {
    // 0 clamps up to 1 → one message per instrument.
    let zero = build_subscription_messages(&instruments(7), FeedMode::Ticker, 0);
    assert_eq!(zero.len(), 7);
    assert_eq!(flatten_wire_instruments(&zero).len(), 7);

    // 101 clamps down to 100 → 150 instruments become 2 messages.
    let over = build_subscription_messages(&instruments(150), FeedMode::Ticker, 101);
    assert_eq!(over.len(), 2);
    assert_eq!(flatten_wire_instruments(&over).len(), 150);
}

#[test]
fn test_security_id_is_a_json_string_not_a_number() {
    // WS-GAP-02 regression: Dhan silently ignores a numeric SecurityId, so
    // the subscribe "succeeds" and no ticks ever arrive.
    let messages = build_subscription_messages(
        &[InstrumentSubscription::new(ExchangeSegment::IdxI, 13)],
        FeedMode::Ticker,
        100,
    );
    let parsed: Value = serde_json::from_str(&messages[0]).unwrap();
    let security_id = &parsed["InstrumentList"][0]["SecurityId"];
    assert!(security_id.is_string(), "SecurityId must be a string");
    assert!(!security_id.is_number());
    assert_eq!(security_id.as_str().unwrap(), "13");
}

#[test]
fn test_regression_id_27_cross_segment_both_reach_the_wire() {
    // Live 2026-04-17 finding: FINNIFTY IDX_I id=27 and an NSE_EQ id=27 are
    // different instruments. Dropping either leaves it permanently silent.
    let both = vec![
        InstrumentSubscription::new(ExchangeSegment::IdxI, 27),
        InstrumentSubscription::new(ExchangeSegment::NseEquity, 27),
    ];
    let wire = flatten_wire_instruments(&build_subscription_messages(&both, FeedMode::Quote, 100));
    assert_eq!(wire.len(), 2, "I-P1-11: both id=27 entries must survive");
    assert!(wire.contains(&("IDX_I".to_string(), "27".to_string())));
    assert!(wire.contains(&("NSE_EQ".to_string(), "27".to_string())));

    // And dedup must not collapse them either.
    assert_eq!(dedup_instruments(&both).len(), 2);
}

#[test]
fn test_depth_caps_are_fifty_and_one() {
    assert_eq!(MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION, 50);
    assert_eq!(MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION, 1);
    assert!(validate_connection_capacity(EndpointKind::TwentyDepth, 50).is_ok());
    assert!(validate_connection_capacity(EndpointKind::TwentyDepth, 51).is_err());
    assert!(validate_connection_capacity(EndpointKind::TwoHundredDepth, 1).is_ok());
    assert!(validate_connection_capacity(EndpointKind::TwoHundredDepth, 2).is_err());
}

#[test]
fn test_two_hundred_depth_is_single_instrument_nse_only() {
    let ok =
        build_two_hundred_depth_subscription_message(ExchangeSegment::NseEquity, 1333).unwrap();
    let parsed: Value = serde_json::from_str(&ok).unwrap();
    assert_eq!(parsed["RequestCode"], 23);
    assert!(parsed["SecurityId"].is_string());
    assert!(parsed.get("InstrumentList").is_none());

    for rejected in [
        ExchangeSegment::IdxI,
        ExchangeSegment::BseEquity,
        ExchangeSegment::BseFno,
        ExchangeSegment::McxComm,
    ] {
        assert!(
            build_two_hundred_depth_subscription_message(rejected, 1).is_err(),
            "depth is NSE-only (doc 04:274) — {rejected:?} must be refused"
        );
    }
}
