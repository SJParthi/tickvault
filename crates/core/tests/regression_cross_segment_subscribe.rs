//! I-P1-11 regression: cross-segment security_id collision in subscribe path.
//!
//! Live finding on 2026-04-17: Parthiban's QuestDB showed NIFTY IDX_I
//! (security_id=13) ticks NEVER arrived — only NSE_EQ id=13 did. Root
//! cause: the legacy `InstrumentRegistry.instruments: HashMap<SecurityId, _>`
//! dropped one of two colliding entries on insert; the WebSocket
//! subscribe-message builder iterated that single-segment map and
//! therefore never emitted a subscribe frame for the dropped entry.
//!
//! This test asserts the full end-to-end path: given a synthetic
//! universe that contains id=13 in BOTH IDX_I and NSE_EQ, the
//! serialized WebSocket subscribe JSON MUST contain TWO entries for
//! id=13 — one per segment. Failure means Dhan never learns to send
//! the second instrument and we silently lose data for it.

use tickvault_common::instrument_registry::{
    InstrumentRegistry, SubscribedInstrument, SubscriptionCategory,
};
use tickvault_common::types::{ExchangeSegment, FeedMode};
use tickvault_core::websocket::subscription_builder::build_subscription_messages;
use tickvault_core::websocket::types::InstrumentSubscription;

fn make_instrument(
    security_id: u32,
    segment: ExchangeSegment,
    symbol: &str,
    category: SubscriptionCategory,
) -> SubscribedInstrument {
    SubscribedInstrument {
        security_id,
        exchange_segment: segment,
        category,
        display_label: symbol.to_string(),
        underlying_symbol: symbol.to_string(),
        instrument_kind: None,
        expiry_date: None,
        strike_price: None,
        option_type: None,
        feed_mode: FeedMode::Ticker,
    }
}

/// Rebuilds the exact pipeline that `crates/app/src/main.rs:3451` runs:
/// `plan.registry.iter() → InstrumentSubscription::new(segment, id) →
/// build_subscription_messages() → serde_json`.
fn subscribe_json_for(
    registry: &InstrumentRegistry,
    feed_mode: FeedMode,
) -> Vec<serde_json::Value> {
    let instruments: Vec<InstrumentSubscription> = registry
        .iter()
        .map(|inst| InstrumentSubscription::new(inst.exchange_segment, inst.security_id))
        .collect();
    let messages = build_subscription_messages(&instruments, feed_mode, 100);
    messages
        .iter()
        .map(|m| serde_json::from_str::<serde_json::Value>(m).expect("valid JSON"))
        .collect()
}

fn flatten_instruments(batches: &[serde_json::Value]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for batch in batches {
        let list = batch["InstrumentList"]
            .as_array()
            .expect("InstrumentList is array");
        for entry in list {
            let seg = entry["ExchangeSegment"].as_str().unwrap().to_string();
            let id = entry["SecurityId"].as_str().unwrap().to_string();
            out.push((seg, id));
        }
    }
    out
}

#[test]
fn test_regression_id13_idx_i_and_nse_eq_both_emit_subscribe_messages() {
    // Exact shape of the live 2026-04-17 collision:
    //   - NIFTY IDX_I id=13 (major index value feed)
    //   - Some stock NSE_EQ id=13 (Dhan reused the number)
    // Plus two unrelated instruments to ensure we don't accidentally
    // emit everything regardless of the registry state.
    let registry = InstrumentRegistry::from_instruments(vec![
        make_instrument(
            13,
            ExchangeSegment::IdxI,
            "NIFTY",
            SubscriptionCategory::MajorIndexValue,
        ),
        make_instrument(
            13,
            ExchangeSegment::NseEquity,
            "ZZSTOCK",
            SubscriptionCategory::StockEquity,
        ),
        make_instrument(
            25,
            ExchangeSegment::IdxI,
            "BANKNIFTY",
            SubscriptionCategory::MajorIndexValue,
        ),
        make_instrument(
            2885,
            ExchangeSegment::NseEquity,
            "RELIANCE",
            SubscriptionCategory::StockEquity,
        ),
    ]);

    let batches = subscribe_json_for(&registry, FeedMode::Ticker);
    let flat = flatten_instruments(&batches);

    // Both colliding entries MUST appear.
    assert!(
        flat.contains(&("IDX_I".to_string(), "13".to_string())),
        "missing IDX_I id=13 subscribe frame — iter() is not segment-aware. \
         Actual: {:?}",
        flat,
    );
    assert!(
        flat.contains(&("NSE_EQ".to_string(), "13".to_string())),
        "missing NSE_EQ id=13 subscribe frame — iter() is not segment-aware. \
         Actual: {:?}",
        flat,
    );

    // Total count = 4 (two colliding + two distinct).
    assert_eq!(
        flat.len(),
        4,
        "expected 4 subscribe entries, got: {:?}",
        flat,
    );
}

#[test]
fn test_regression_id27_finnifty_collision_emits_both_segments() {
    // The ORIGINAL live collision observed on 2026-04-17 was id=27:
    //   - FINNIFTY IDX_I id=27
    //   - Some stock NSE_EQ id=27
    // FINNIFTY didn't appear in depth-20 dashboard because its index
    // LTP never streamed (IDX_I subscribe frame never sent).
    let registry = InstrumentRegistry::from_instruments(vec![
        make_instrument(
            27,
            ExchangeSegment::IdxI,
            "FINNIFTY",
            SubscriptionCategory::MajorIndexValue,
        ),
        make_instrument(
            27,
            ExchangeSegment::NseEquity,
            "ACMESTOCK",
            SubscriptionCategory::StockEquity,
        ),
    ]);

    let batches = subscribe_json_for(&registry, FeedMode::Quote);
    let flat = flatten_instruments(&batches);

    assert!(
        flat.contains(&("IDX_I".to_string(), "27".to_string())),
        "FINNIFTY IDX_I subscribe frame missing — regression of 2026-04-17 \
         live bug. Actual: {:?}",
        flat,
    );
    assert!(
        flat.contains(&("NSE_EQ".to_string(), "27".to_string())),
        "NSE_EQ id=27 subscribe frame missing — regression. Actual: {:?}",
        flat,
    );
}

#[test]
fn test_regression_by_exchange_segment_emits_both_colliding_ids() {
    // Guards `InstrumentRegistry::by_exchange_segment()` which is used
    // by some planner/diagnostic paths. Must return id=13 in BOTH the
    // IDX_I bucket and the NSE_EQ bucket.
    let registry = InstrumentRegistry::from_instruments(vec![
        make_instrument(
            13,
            ExchangeSegment::IdxI,
            "NIFTY",
            SubscriptionCategory::MajorIndexValue,
        ),
        make_instrument(
            13,
            ExchangeSegment::NseEquity,
            "ZZSTOCK",
            SubscriptionCategory::StockEquity,
        ),
    ]);

    let grouped = registry.by_exchange_segment();
    let idx_bucket = grouped
        .get(&ExchangeSegment::IdxI)
        .expect("IDX_I bucket exists");
    let eq_bucket = grouped
        .get(&ExchangeSegment::NseEquity)
        .expect("NSE_EQ bucket exists");

    assert!(
        idx_bucket.contains(&13),
        "IDX_I bucket missing id=13: {:?}",
        idx_bucket,
    );
    assert!(
        eq_bucket.contains(&13),
        "NSE_EQ bucket missing id=13: {:?}",
        eq_bucket,
    );
}

#[test]
fn test_regression_registry_len_counts_both_segments_for_same_id() {
    // If `len()` returned the legacy single-segment map count, the
    // metric "total subscribed instruments" would under-report by the
    // number of cross-segment collisions present in Dhan's CSV. That
    // would also mean the downstream iter() miscounts.
    let registry = InstrumentRegistry::from_instruments(vec![
        make_instrument(
            13,
            ExchangeSegment::IdxI,
            "NIFTY",
            SubscriptionCategory::MajorIndexValue,
        ),
        make_instrument(
            13,
            ExchangeSegment::NseEquity,
            "ZZSTOCK",
            SubscriptionCategory::StockEquity,
        ),
        make_instrument(
            27,
            ExchangeSegment::IdxI,
            "FINNIFTY",
            SubscriptionCategory::MajorIndexValue,
        ),
        make_instrument(
            27,
            ExchangeSegment::NseEquity,
            "ACMESTOCK",
            SubscriptionCategory::StockEquity,
        ),
    ]);

    assert_eq!(
        registry.len(),
        4,
        "registry.len() must count ALL (id, segment) pairs, not just distinct ids"
    );
    assert_eq!(registry.iter().count(), 4);
}
