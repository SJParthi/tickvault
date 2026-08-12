//! Builds JSON subscription messages for the Dhan WebSocket V2 protocol.
//!
//! # Revival provenance (2026-08-11)
//!
//! This module was DELETED on 2026-07-13 with the Dhan live main-feed lane
//! (PR-C2, commit `73c17517`). It is rebuilt here under the operator's
//! 2026-08-09 dated authorizations recorded in
//! `.claude/rules/project/websocket-connection-scope-lock.md`:
//! - "2026-08-09 — DHAN LIVE MAIN-FEED WS REVIVAL AUTHORIZED", and
//! - "2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS +
//!   depth-20/depth-200 AUTHORIZED"
//!
//! Those are the fresh dated quotes the retirement guard's §D escape hatch
//! demands. Every protocol constant below was RE-VERIFIED against the vendor
//! reference docs rather than trusted from the recovered source — citations
//! are inline at each constant.
//!
//! # Verified protocol limits
//!
//! | Limit | Value | Source |
//! |---|---|---|
//! | Instruments per subscribe MESSAGE | 100 | `docs/dhan-ref/03-live-market-feed-websocket.md:21`, `:71`, `:513` |
//! | Instruments per main-feed CONNECTION | 5,000 | `docs/dhan-ref/03-live-market-feed-websocket.md:20` |
//! | Main-feed connections | 5 | `docs/dhan-ref/03-live-market-feed-websocket.md:19` |
//! | Instruments per depth-20 CONNECTION | 50 | `docs/dhan-ref/04-full-market-depth-websocket.md:76`, `:272` |
//! | Instruments per depth-200 CONNECTION | 1 | `docs/dhan-ref/04-full-market-depth-websocket.md:91`, `:270` |
//! | `SecurityId` JSON type | **string** | `docs/dhan-ref/03-live-market-feed-websocket.md:72` |
//! | Depth segments | NSE only | `docs/dhan-ref/04-full-market-depth-websocket.md:13`, `:274` |
//! | Depth unsubscribe code | 25 (NOT 24) | `docs/dhan-ref/04-full-market-depth-websocket.md:280` |
//!
//! # Why 5 + 5 + 5 + 1 = 16 is legal
//!
//! `docs/dhan-ref/04-full-market-depth-websocket.md:64-69` states the
//! 5-connection cap applies to EACH WebSocket type **independently** — live
//! feed, depth-20 and depth-200 each get their own pool of 5. They are NOT a
//! shared cap. With the single order-update socket that totals 16.
//!
//! # I-P1-11 (segment-aware uniqueness)
//!
//! Dhan REUSES one numeric `security_id` across different `ExchangeSegment`
//! values — the live 2026-04-17 finding was FINNIFTY `IDX_I` id=27 colliding
//! with an `NSE_EQ` id=27. Every dedup in this module therefore keys on the
//! COMPOSITE `(exchange_segment, security_id)`; a `security_id`-only key
//! would silently drop one of the two instruments and leave it unsubscribed.
//! See `.claude/rules/project/security-id-uniqueness.md`.
//!
//! # Performance
//!
//! Cold path only. Every function here runs at connect/disconnect time, never
//! per tick, so allocation is permitted (and marked `O(1) EXEMPT`).

use std::collections::HashSet;

use tickvault_common::constants::{
    MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION, MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION,
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, SUBSCRIPTION_BATCH_SIZE,
};
use tickvault_common::types::{ExchangeSegment, FeedMode};

use crate::websocket::types::{
    InstrumentSubscription, SubscriptionRequest, TwoHundredDepthSubscriptionRequest,
};

// ---------------------------------------------------------------------------
// Request-code mapping
// ---------------------------------------------------------------------------

/// Maps a `FeedMode` to the Dhan WebSocket subscribe RequestCode.
///
/// Verified against `docs/dhan-ref/03-live-market-feed-websocket.md:76-84`:
/// Ticker = 15, Quote = 17, Full = 21.
fn feed_mode_to_subscribe_code(mode: FeedMode) -> u8 {
    match mode {
        FeedMode::Ticker => tickvault_common::constants::FEED_REQUEST_TICKER,
        FeedMode::Quote => tickvault_common::constants::FEED_REQUEST_QUOTE,
        FeedMode::Full => tickvault_common::constants::FEED_REQUEST_FULL,
    }
}

/// Maps a `FeedMode` to the Dhan WebSocket unsubscribe RequestCode.
///
/// Verified against `docs/dhan-ref/03-live-market-feed-websocket.md:76-84`:
/// Ticker = 16, Quote = 18, Full = 22 (i.e. subscribe code + 1 on THIS
/// socket — a coincidence that does NOT hold for depth; see
/// `build_twenty_depth_unsubscription_messages`).
fn feed_mode_to_unsubscribe_code(mode: FeedMode) -> u8 {
    match mode {
        FeedMode::Ticker => tickvault_common::constants::FEED_UNSUBSCRIBE_TICKER,
        FeedMode::Quote => tickvault_common::constants::FEED_UNSUBSCRIBE_QUOTE,
        FeedMode::Full => tickvault_common::constants::FEED_UNSUBSCRIBE_FULL,
    }
}

// ---------------------------------------------------------------------------
// I-P1-11 composite-key dedup
// ---------------------------------------------------------------------------

/// Deduplicates a subscription list on the COMPOSITE `(exchange_segment,
/// security_id)` key, preserving first-seen order.
///
/// # Why the composite key (I-P1-11)
///
/// `security_id` ALONE is not unique — Dhan reuses the same numeric id across
/// segments (live 2026-04-17: FINNIFTY `IDX_I` id=27 vs an `NSE_EQ` id=27).
/// Deduplicating on the id alone drops one of the two, and the dropped
/// instrument is then never subscribed — silent, permanent data loss with no
/// error anywhere. Both entries survive here because the segment is part of
/// the key.
///
/// Note the key is `(&str, &str)` over the already-serialized fields, which is
/// segment-aware by construction: there is no `security_id`-only collection in
/// this module for a future edit to accidentally reintroduce.
pub fn dedup_instruments(instruments: &[InstrumentSubscription]) -> Vec<InstrumentSubscription> {
    // O(1) EXEMPT: begin — cold path, runs once at connect time, not per tick
    let mut seen: HashSet<(&str, &str)> = HashSet::with_capacity(instruments.len());
    let mut out = Vec::with_capacity(instruments.len());
    for instrument in instruments {
        // COMPOSITE key per I-P1-11 — segment FIRST so the pair is never
        // mistaken for an id-only key at a glance.
        let key = (
            instrument.exchange_segment.as_str(),
            instrument.security_id.as_str(),
        );
        if seen.insert(key) {
            out.push(instrument.clone());
        }
    }
    out
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Per-endpoint capacity validation (the 16-connection architecture)
// ---------------------------------------------------------------------------

/// Per-endpoint instrument capacity of ONE connection.
///
/// These three caps are what make a 16-connection layout plannable: each
/// endpoint type has its OWN pool of 5 connections
/// (`docs/dhan-ref/04-full-market-depth-websocket.md:64-69`), so total
/// capacity is `5×5000` main-feed + `5×50` depth-20 + `5×1` depth-200.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// Live Market Feed — 5,000 instruments/connection
    /// (`docs/dhan-ref/03-live-market-feed-websocket.md:20`).
    MainFeed,
    /// 20-level depth — 50 instruments/connection
    /// (`docs/dhan-ref/04-full-market-depth-websocket.md:76`).
    TwentyDepth,
    /// 200-level depth — 1 instrument/connection
    /// (`docs/dhan-ref/04-full-market-depth-websocket.md:91`).
    TwoHundredDepth,
}

impl EndpointKind {
    /// Maximum instruments a SINGLE connection of this kind may carry.
    pub fn max_instruments_per_connection(self) -> usize {
        match self {
            Self::MainFeed => MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION,
            Self::TwentyDepth => MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION,
            Self::TwoHundredDepth => MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION,
        }
    }
}

/// Rejects an instrument list that would exceed ONE connection's capacity.
///
/// Fail-CLOSED and deliberately not a silent truncation: Dhan does not reject
/// an over-subscribed socket, it just stops delivering the excess, which reads
/// as "those instruments are silent" — indistinguishable from a dead feed.
/// Callers must split across connections instead.
pub fn validate_connection_capacity(
    kind: EndpointKind,
    instrument_count: usize,
) -> Result<(), String> {
    // O(1) EXEMPT: begin — cold path, connect-time validation
    let cap = kind.max_instruments_per_connection();
    if instrument_count > cap {
        return Err(format!(
            "{instrument_count} instruments exceeds the {cap}-instrument \
             per-connection limit for {kind:?} — split across connections"
        ));
    }
    Ok(())
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Main-feed subscribe / unsubscribe
// ---------------------------------------------------------------------------

/// Splits `instruments` into batched subscribe messages.
///
/// `batch_size` is CLAMPED to `[1, 100]` — 100 is Dhan's hard per-message
/// limit (`docs/dhan-ref/03-live-market-feed-websocket.md:21`) and 1 is the
/// floor that keeps `chunks()` from panicking on 0.
///
/// An EMPTY instrument list yields ZERO messages, never one empty message:
/// an `InstrumentCount: 0` frame is not a documented request shape.
///
/// # Zero-loss property
///
/// Concatenating the `InstrumentList`s of the returned messages reproduces
/// `instruments` exactly — same items, same order, nothing dropped, nothing
/// duplicated. Proven by `proptest` in
/// `crates/core/tests/subscription_builder_properties.rs`.
pub fn build_subscription_messages(
    instruments: &[InstrumentSubscription],
    feed_mode: FeedMode,
    batch_size: usize,
) -> Vec<String> {
    build_batched(
        instruments,
        feed_mode_to_subscribe_code(feed_mode),
        batch_size,
    )
}

/// Splits `instruments` into batched unsubscribe messages.
///
/// Ticker = 16, Quote = 18, Full = 22
/// (`docs/dhan-ref/03-live-market-feed-websocket.md:76-84`).
pub fn build_unsubscription_messages(
    instruments: &[InstrumentSubscription],
    feed_mode: FeedMode,
    batch_size: usize,
) -> Vec<String> {
    build_batched(
        instruments,
        feed_mode_to_unsubscribe_code(feed_mode),
        batch_size,
    )
}

/// Shared batching core for every `InstrumentList`-shaped request.
///
/// Single implementation so the empty-list rule and the `[1, 100]` clamp
/// cannot drift between the subscribe, unsubscribe and depth-20 paths.
fn build_batched(
    instruments: &[InstrumentSubscription],
    request_code: u8,
    batch_size: usize,
) -> Vec<String> {
    // O(1) EXEMPT: begin — cold path, runs once at connect time, not per tick
    if instruments.is_empty() {
        return Vec::new();
    }

    // Clamp to Dhan's hard per-message limit; `1` floor keeps chunks() valid.
    let effective_batch = batch_size.clamp(1, SUBSCRIPTION_BATCH_SIZE);

    #[allow(clippy::expect_used)] // APPROVED: SubscriptionRequest is a plain
    // struct of String/usize/u8 fields — serde_json cannot fail on it.
    instruments
        .chunks(effective_batch)
        .map(|chunk| {
            let request = SubscriptionRequest {
                request_code,
                instrument_count: chunk.len(),
                instrument_list: chunk.to_vec(),
            };
            serde_json::to_string(&request).expect("SubscriptionRequest serialization cannot fail") // APPROVED: infallible serialize
        })
        .collect()
    // O(1) EXEMPT: end
}

/// Builds the graceful-disconnect message (RequestCode 12).
///
/// Verified: `docs/dhan-ref/03-live-market-feed-websocket.md:84` and `:262`.
pub fn build_disconnect_message() -> String {
    serde_json::json!({
        "RequestCode": tickvault_common::constants::FEED_REQUEST_DISCONNECT
    })
    .to_string() // O(1) EXEMPT: disconnect message — once at shutdown
}

// ---------------------------------------------------------------------------
// 20-level depth
// ---------------------------------------------------------------------------

/// Builds batched 20-level depth subscribe messages (RequestCode 23).
///
/// Verified: `docs/dhan-ref/04-full-market-depth-websocket.md:80`. The
/// per-CONNECTION cap is 50 (`:76`) — enforce it with
/// `validate_connection_capacity(EndpointKind::TwentyDepth, n)`; the
/// per-MESSAGE clamp here remains 100, since the message limit and the
/// connection limit are different constraints.
/// # Segment eligibility
///
/// Refuses if ANY instrument sits in a segment Dhan's Full Market Depth does
/// not serve. `docs/dhan-ref/04-full-market-depth-websocket.md:13` puts "Only
/// NSE Equity and Derivatives segments supported" in the OVERVIEW, above the
/// 20-level and 200-level sections alike — so the restriction the 200-level
/// builder has always enforced applies here identically.
///
/// It was not enforced here until 2026-08-12, and the asymmetry mattered: a
/// BSE_FNO (SENSEX) contract sent to the 200-level builder produced a loud
/// `WS-GAP-02` refusal and a torn-down socket, while the SAME contract sent
/// here went on the wire and came back as **silence** — which is exactly what
/// a legitimately quiet order book looks like. The louder failure was the
/// safer one; this makes both loud.
///
/// Refusing the whole batch rather than filtering the offender out is
/// deliberate: a caller that asked for depth on an ineligible instrument has
/// a selection bug, and silently serving a subset would hide it behind a
/// partially-working feed.
pub fn build_twenty_depth_subscription_messages(
    instruments: &[InstrumentSubscription],
    batch_size: usize,
) -> Result<Vec<String>, String> {
    for instrument in instruments {
        validate_depth_segment_str(&instrument.exchange_segment)?;
    }
    Ok(build_batched(
        instruments,
        tickvault_common::constants::FEED_REQUEST_TWENTY_DEPTH,
        batch_size,
    ))
}

/// Builds batched 20-level depth unsubscribe messages (RequestCode **25**).
///
/// NOT 24. `docs/dhan-ref/04-full-market-depth-websocket.md:280` records that
/// the vendor's own reference client derives unsubscribe as
/// `subscribe_code + 1` = 24, which is a bug in that client; the Dhan
/// Annexure value is 25 and that is what we send.
///
/// (The vendor client's language is named in the referenced doc, not here —
/// `rust-only-forever-lock-2026-07-19.md` keeps the interpreted-runtime's
/// name out of Rust source, and the guard that enforces it does not care
/// that the mention was descriptive.)
pub fn build_twenty_depth_unsubscription_messages(
    instruments: &[InstrumentSubscription],
    batch_size: usize,
) -> Vec<String> {
    build_batched(
        instruments,
        tickvault_common::constants::FEED_UNSUBSCRIBE_TWENTY_DEPTH,
        batch_size,
    )
}

// ---------------------------------------------------------------------------
// 200-level depth (1 instrument per connection — flat JSON, no InstrumentList)
// ---------------------------------------------------------------------------

/// Builds the 200-level depth subscribe message for a SINGLE instrument.
///
/// The signature takes one instrument by design: depth-200 permits exactly 1
/// instrument per connection (`docs/dhan-ref/04-full-market-depth-websocket.md:91`,
/// `:270`), so the type system enforces the cap rather than a runtime check.
/// The JSON is FLAT — no `InstrumentList`, no `InstrumentCount` (`:95-97`).
pub fn build_two_hundred_depth_subscription_message(
    segment: ExchangeSegment,
    security_id: u64,
) -> Result<String, String> {
    build_two_hundred_depth_message(
        tickvault_common::constants::FEED_REQUEST_TWENTY_DEPTH, // 23 for both 20- and 200-level
        segment,
        security_id,
    )
}

/// Builds the 200-level depth unsubscribe message (RequestCode 25).
pub fn build_two_hundred_depth_unsubscription_message(
    segment: ExchangeSegment,
    security_id: u64,
) -> Result<String, String> {
    build_two_hundred_depth_message(
        tickvault_common::constants::FEED_UNSUBSCRIBE_TWENTY_DEPTH, // 25
        segment,
        security_id,
    )
}

/// Shared flat-JSON core for the two 200-level depth builders.
fn build_two_hundred_depth_message(
    request_code: u8,
    segment: ExchangeSegment,
    security_id: u64,
) -> Result<String, String> {
    // O(1) EXEMPT: begin — cold path, connect-time message construction
    validate_depth_segment(segment)?;

    let request = TwoHundredDepthSubscriptionRequest {
        request_code,
        exchange_segment: segment.as_str().to_string(),
        // Dhan requires SecurityId as a STRING, never a JSON number
        // (`docs/dhan-ref/03-live-market-feed-websocket.md:72`).
        security_id: security_id.to_string(),
    };

    #[allow(clippy::expect_used)] // APPROVED: TwoHundredDepthSubscriptionRequest
    // is a plain struct of String/u8 fields — serde_json cannot fail on it.
    Ok(serde_json::to_string(&request)
        .expect("TwoHundredDepthSubscriptionRequest serialization cannot fail"))
    // O(1) EXEMPT: end
}

/// Rejects any non-NSE segment for Full Market Depth.
///
/// Verified: `docs/dhan-ref/04-full-market-depth-websocket.md:13` ("Only NSE
/// Equity and Derivatives segments supported") and `:274` ("No BSE, MCX, or
/// currency"). Rejected at BUILD time so an unsupported subscribe is never
/// put on the wire — Dhan's failure mode for one would be silence, not an
/// error frame.
/// The wire-string form of [`validate_depth_segment`].
///
/// [`InstrumentSubscription::exchange_segment`] is a `String` (it is the JSON
/// field Dhan receives), so the 20-level batch path has a string in hand and
/// no typed segment. Rather than parse-then-validate — which would need a
/// fallible parse whose failure mode is a THIRD outcome to reason about —
/// this compares against the two accepted wire strings directly.
///
/// Fail-closed by construction: anything that is not exactly one of the two
/// accepted strings is refused, including a typo, an empty string, or a
/// segment Dhan adds later. `test_validate_depth_segment_str_agrees_with_the_typed_guard`
/// runs both guards over every `ExchangeSegment` so the two can never drift.
pub fn validate_depth_segment_str(segment: &str) -> Result<(), String> {
    if segment == ExchangeSegment::NseEquity.as_str() || segment == ExchangeSegment::NseFno.as_str()
    {
        return Ok(());
    }
    Err([
        "Full Market Depth only supports NSE_EQ and NSE_FNO, got: ",
        segment,
    ]
    .concat())
}

pub fn validate_depth_segment(segment: ExchangeSegment) -> Result<(), String> {
    match segment {
        ExchangeSegment::NseEquity | ExchangeSegment::NseFno => Ok(()),
        // O(1) EXEMPT: cold-path validation, runs at connect time, not per tick
        other => {
            let seg = other.as_str();
            Err([
                "Full Market Depth only supports NSE_EQ and NSE_FNO, got: ",
                seg,
            ]
            .concat())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, (i as u64) + 1000))
            .collect()
    }

    /// Sum of `InstrumentCount` across every produced message.
    fn total_instruments(messages: &[String]) -> usize {
        messages
            .iter()
            .map(|m| {
                let v: serde_json::Value = serde_json::from_str(m).unwrap();
                v["InstrumentList"].as_array().unwrap().len()
            })
            .sum()
    }

    // --- empty / single / boundary ---

    #[test]
    fn test_empty_instruments_returns_zero_messages_not_one_empty() {
        let messages = build_subscription_messages(&[], FeedMode::Ticker, 100);
        assert!(
            messages.is_empty(),
            "an empty list must produce ZERO messages, never one InstrumentCount:0 frame"
        );
    }

    #[test]
    fn test_single_instrument_single_message() {
        let messages = build_subscription_messages(&make_instruments(1), FeedMode::Ticker, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"RequestCode\":15"));
        assert!(messages[0].contains("\"InstrumentCount\":1"));
    }

    #[test]
    fn test_exactly_100_is_one_message() {
        let messages = build_subscription_messages(&make_instruments(100), FeedMode::Quote, 100);
        assert_eq!(messages.len(), 1, "100 is the per-message limit, not 100+1");
        assert!(messages[0].contains("\"InstrumentCount\":100"));
    }

    #[test]
    fn test_101_splits_into_two_messages() {
        let messages = build_subscription_messages(&make_instruments(101), FeedMode::Full, 100);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"InstrumentCount\":100"));
        assert!(messages[1].contains("\"InstrumentCount\":1"));
    }

    #[test]
    fn test_five_thousand_full_connection_capacity() {
        // 5,000 = one full main-feed connection (doc 03:20) = 50 messages.
        let instruments = make_instruments(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION);
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert_eq!(messages.len(), 50, "5000 / 100 = 50 messages (doc 03:513)");
        assert_eq!(total_instruments(&messages), 5000);
    }

    // --- batch-size clamping ---

    #[test]
    fn test_batch_size_zero_clamps_to_one() {
        let messages = build_subscription_messages(&make_instruments(3), FeedMode::Ticker, 0);
        assert_eq!(messages.len(), 3, "batch_size 0 must clamp to 1, not panic");
        assert_eq!(total_instruments(&messages), 3);
    }

    #[test]
    fn test_batch_size_101_clamps_to_100() {
        let messages = build_subscription_messages(&make_instruments(200), FeedMode::Full, 101);
        assert_eq!(messages.len(), 2, "batch_size 101 must clamp down to 100");
        assert_eq!(total_instruments(&messages), 200);
    }

    #[test]
    fn test_batch_size_usize_max_clamps_to_100() {
        let messages =
            build_subscription_messages(&make_instruments(150), FeedMode::Ticker, usize::MAX);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_unsubscribe_batch_size_clamped_too() {
        let messages = build_unsubscription_messages(&make_instruments(200), FeedMode::Ticker, 999);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"RequestCode\":16"));
    }

    // --- request codes ---

    #[test]
    fn test_subscribe_request_codes_15_17_21() {
        for (mode, code) in [
            (FeedMode::Ticker, 15),
            (FeedMode::Quote, 17),
            (FeedMode::Full, 21),
        ] {
            let messages = build_subscription_messages(&make_instruments(1), mode, 100);
            assert!(
                messages[0].contains(&format!("\"RequestCode\":{code}")),
                "{mode:?} subscribe must be {code} (doc 03:76-84)"
            );
        }
    }

    #[test]
    fn test_unsubscribe_request_codes_16_18_22() {
        for (mode, code) in [
            (FeedMode::Ticker, 16),
            (FeedMode::Quote, 18),
            (FeedMode::Full, 22),
        ] {
            let messages = build_unsubscription_messages(&make_instruments(1), mode, 100);
            assert!(
                messages[0].contains(&format!("\"RequestCode\":{code}")),
                "{mode:?} unsubscribe must be {code} (doc 03:76-84)"
            );
        }
    }

    #[test]
    fn test_unsubscribe_empty_returns_empty() {
        assert!(build_unsubscription_messages(&[], FeedMode::Ticker, 100).is_empty());
    }

    #[test]
    fn test_disconnect_message_is_request_code_12() {
        assert!(build_disconnect_message().contains("\"RequestCode\":12"));
    }

    // --- SecurityId MUST be a JSON string (WS-GAP-02) ---

    #[test]
    fn test_security_id_serializes_as_string_not_number() {
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::IdxI, 13)];
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert!(
            messages[0].contains("\"SecurityId\":\"13\""),
            "WS-GAP-02: SecurityId must be a JSON STRING (doc 03:72); got {}",
            messages[0]
        );
        assert!(
            !messages[0].contains("\"SecurityId\":13"),
            "SecurityId must never serialize as a bare JSON number"
        );
        // Parse-level proof, not just substring matching.
        let v: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        assert!(v["InstrumentList"][0]["SecurityId"].is_string());
        assert!(!v["InstrumentList"][0]["SecurityId"].is_number());
    }

    #[test]
    fn test_large_security_id_still_a_string() {
        let instruments = vec![InstrumentSubscription::new(
            ExchangeSegment::NseFno,
            u64::from(u32::MAX),
        )];
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        let v: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        assert_eq!(v["InstrumentList"][0]["SecurityId"], u32::MAX.to_string());
    }

    // --- I-P1-11: the live id=27 cross-segment case ---

    #[test]
    fn test_id_27_across_segments_produces_both_entries() {
        // The live 2026-04-17 finding: FINNIFTY IDX_I id=27 and an NSE_EQ
        // id=27 are DIFFERENT instruments. Both must reach the wire.
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::IdxI, 27),
            InstrumentSubscription::new(ExchangeSegment::NseEquity, 27),
        ];
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert_eq!(messages.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        let list = v["InstrumentList"].as_array().unwrap();
        assert_eq!(list.len(), 2, "I-P1-11: BOTH id=27 entries must survive");
        assert_eq!(v["InstrumentCount"], 2);
        assert_eq!(list[0]["ExchangeSegment"], "IDX_I");
        assert_eq!(list[1]["ExchangeSegment"], "NSE_EQ");
        assert_eq!(list[0]["SecurityId"], "27");
        assert_eq!(list[1]["SecurityId"], "27");
    }

    #[test]
    fn test_dedup_keeps_both_id_27_segments_but_drops_true_duplicate() {
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::IdxI, 27),
            InstrumentSubscription::new(ExchangeSegment::NseEquity, 27),
            // exact duplicate of the first — same segment AND same id
            InstrumentSubscription::new(ExchangeSegment::IdxI, 27),
        ];
        let deduped = dedup_instruments(&instruments);
        assert_eq!(
            deduped.len(),
            2,
            "I-P1-11: dedup on (segment, id) keeps both segments, drops only the exact repeat"
        );
        assert_eq!(deduped[0].exchange_segment, "IDX_I");
        assert_eq!(deduped[1].exchange_segment, "NSE_EQ");
    }

    #[test]
    fn test_dedup_preserves_first_seen_order_and_handles_empty() {
        assert!(dedup_instruments(&[]).is_empty());
        let instruments = make_instruments(5);
        let deduped = dedup_instruments(&instruments);
        assert_eq!(deduped.len(), 5);
        for (a, b) in instruments.iter().zip(deduped.iter()) {
            assert_eq!(a.security_id, b.security_id);
        }
    }

    #[test]
    fn test_dedup_all_identical_collapses_to_one() {
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 52432); 10];
        assert_eq!(dedup_instruments(&instruments).len(), 1);
    }

    // --- per-endpoint capacity ---

    #[test]
    fn test_endpoint_caps_match_verified_doc_values() {
        assert_eq!(
            EndpointKind::MainFeed.max_instruments_per_connection(),
            5000,
            "doc 03:20"
        );
        assert_eq!(
            EndpointKind::TwentyDepth.max_instruments_per_connection(),
            50,
            "doc 04:76"
        );
        assert_eq!(
            EndpointKind::TwoHundredDepth.max_instruments_per_connection(),
            1,
            "doc 04:91"
        );
    }

    #[test]
    fn test_capacity_validation_accepts_at_limit_rejects_above() {
        for (kind, cap) in [
            (EndpointKind::MainFeed, 5000usize),
            (EndpointKind::TwentyDepth, 50),
            (EndpointKind::TwoHundredDepth, 1),
        ] {
            assert!(validate_connection_capacity(kind, 0).is_ok());
            assert!(validate_connection_capacity(kind, cap).is_ok());
            let over = validate_connection_capacity(kind, cap + 1);
            assert!(over.is_err(), "{kind:?} must reject {} ", cap + 1);
            assert!(over.unwrap_err().contains("split across connections"));
        }
    }

    // --- segments ---

    #[test]
    fn test_mixed_segments_all_serialize() {
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
            InstrumentSubscription::new(ExchangeSegment::BseFno, 2000),
            InstrumentSubscription::new(ExchangeSegment::NseEquity, 2885),
        ];
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert_eq!(messages.len(), 1);
        for expected in ["NSE_FNO", "IDX_I", "BSE_FNO", "NSE_EQ"] {
            assert!(messages[0].contains(expected));
        }
    }

    #[test]
    fn test_all_exchange_segments_serialize_correctly() {
        for (segment, expected) in [
            (ExchangeSegment::IdxI, "IDX_I"),
            (ExchangeSegment::NseEquity, "NSE_EQ"),
            (ExchangeSegment::NseFno, "NSE_FNO"),
            (ExchangeSegment::NseCurrency, "NSE_CURRENCY"),
            (ExchangeSegment::BseEquity, "BSE_EQ"),
            (ExchangeSegment::McxComm, "MCX_COMM"),
            (ExchangeSegment::BseCurrency, "BSE_CURRENCY"),
            (ExchangeSegment::BseFno, "BSE_FNO"),
        ] {
            let instruments = vec![InstrumentSubscription::new(segment, 100)];
            let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
            assert!(messages[0].contains(expected), "segment {segment:?}");
        }
    }

    #[test]
    fn test_output_is_valid_json() {
        let messages = build_subscription_messages(&make_instruments(3), FeedMode::Full, 100);
        let parsed: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        assert_eq!(parsed["RequestCode"], 21);
        assert_eq!(parsed["InstrumentCount"], 3);
        assert_eq!(parsed["InstrumentList"].as_array().unwrap().len(), 3);
    }

    // --- 20-level depth ---

    #[test]
    fn test_twenty_depth_subscribe_is_code_23() {
        let messages = build_twenty_depth_subscription_messages(&make_instruments(1), 100)
            .expect("NSE_FNO is depth-eligible");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"RequestCode\":23"));
    }

    #[test]
    fn test_twenty_depth_unsubscribe_is_code_25_not_24() {
        let messages = build_twenty_depth_unsubscription_messages(&make_instruments(3), 100);
        assert!(
            messages[0].contains("\"RequestCode\":25"),
            "doc 04:280 — the vendor SDK's 24 is a BUG; the Annexure value is 25"
        );
        assert!(!messages[0].contains("\"RequestCode\":24"));
    }

    #[test]
    fn test_twenty_depth_empty_returns_empty() {
        assert!(
            build_twenty_depth_subscription_messages(&[], 100)
                .expect("an empty batch has no segment to refuse")
                .is_empty()
        );
        assert!(build_twenty_depth_unsubscription_messages(&[], 100).is_empty());
    }

    #[test]
    fn test_twenty_depth_fifty_is_one_full_connection() {
        // 50 = the per-connection cap (doc 04:76) and still one message.
        let instruments = make_instruments(MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION);
        let messages = build_twenty_depth_subscription_messages(&instruments, 100)
            .expect("NSE_FNO is depth-eligible");
        assert_eq!(messages.len(), 1);
        assert_eq!(total_instruments(&messages), 50);
        assert!(validate_connection_capacity(EndpointKind::TwentyDepth, 50).is_ok());
        assert!(validate_connection_capacity(EndpointKind::TwentyDepth, 51).is_err());
    }

    // --- 200-level depth ---

    #[test]
    fn test_two_hundred_depth_is_flat_json_no_instrument_list() {
        let msg =
            build_two_hundred_depth_subscription_message(ExchangeSegment::NseEquity, 1333).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["RequestCode"], 23);
        assert_eq!(parsed["ExchangeSegment"], "NSE_EQ");
        assert_eq!(parsed["SecurityId"], "1333");
        assert!(parsed["SecurityId"].is_string());
        assert!(parsed.get("InstrumentList").is_none(), "doc 04:95-97");
        assert!(parsed.get("InstrumentCount").is_none(), "doc 04:95-97");
    }

    #[test]
    fn test_two_hundred_depth_nse_fno_ok() {
        let msg =
            build_two_hundred_depth_subscription_message(ExchangeSegment::NseFno, 52432).unwrap();
        assert!(msg.contains("\"ExchangeSegment\":\"NSE_FNO\""));
        assert!(msg.contains("\"SecurityId\":\"52432\""));
    }

    #[test]
    fn test_two_hundred_depth_unsubscribe_is_code_25() {
        let msg = build_two_hundred_depth_unsubscription_message(ExchangeSegment::NseEquity, 2885)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["RequestCode"], 25);
        assert_eq!(parsed["SecurityId"], "2885");
    }

    #[test]
    fn test_two_hundred_depth_rejects_every_non_nse_segment() {
        for segment in [
            ExchangeSegment::IdxI,
            ExchangeSegment::BseEquity,
            ExchangeSegment::BseFno,
            ExchangeSegment::McxComm,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseCurrency,
        ] {
            assert!(
                build_two_hundred_depth_subscription_message(segment, 1000).is_err(),
                "doc 04:274 — {segment:?} is not an NSE depth segment"
            );
            assert!(
                build_two_hundred_depth_unsubscription_message(segment, 1000).is_err(),
                "doc 04:274 — {segment:?} must be rejected on unsubscribe too"
            );
        }
    }

    #[test]
    fn test_validate_depth_segment_nse_only() {
        assert!(validate_depth_segment(ExchangeSegment::NseEquity).is_ok());
        assert!(validate_depth_segment(ExchangeSegment::NseFno).is_ok());
        for segment in [
            ExchangeSegment::IdxI,
            ExchangeSegment::BseEquity,
            ExchangeSegment::BseFno,
            ExchangeSegment::McxComm,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseCurrency,
        ] {
            let err = validate_depth_segment(segment).unwrap_err();
            assert!(err.contains("NSE_EQ and NSE_FNO"));
            assert!(err.contains(segment.as_str()));
        }
    }

    /// The string guard must agree with the typed one on EVERY segment.
    ///
    /// They are two encodings of one vendor rule, applied on two different
    /// code paths (the 20-level batch path has a `String`, the 200-level path
    /// has an `ExchangeSegment`). If they ever disagree, one path admits an
    /// instrument the other refuses — which is how the 2026-08-12 asymmetry
    /// happened in the first place: the 200-level path checked, the 20-level
    /// path did not, and SENSEX went on the wire and came back as silence.
    #[test]
    fn test_validate_depth_segment_str_agrees_with_the_typed_guard() {
        for segment in [
            ExchangeSegment::IdxI,
            ExchangeSegment::NseEquity,
            ExchangeSegment::NseFno,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseEquity,
            ExchangeSegment::McxComm,
            ExchangeSegment::BseCurrency,
            ExchangeSegment::BseFno,
        ] {
            assert_eq!(
                validate_depth_segment_str(segment.as_str()).is_ok(),
                validate_depth_segment(segment).is_ok(),
                "the two depth guards disagree on {segment:?}"
            );
        }
    }

    /// Anything that is not exactly an accepted wire string is REFUSED.
    ///
    /// The string guard compares literals rather than parsing, so its whole
    /// safety argument rests on being fail-closed: a typo, a case variant, an
    /// empty string, or a segment Dhan adds later must all land in the refusal
    /// arm rather than slipping through some lenient parse.
    #[test]
    fn test_validate_depth_segment_str_is_fail_closed_on_anything_unrecognised() {
        for bad in [
            "",
            " ",
            "nse_fno",  // wrong case — the wire form is upper
            "NSE_FN0",  // digit zero for the letter O
            " NSE_FNO", // leading space
            "NSE_FNO ", // trailing space
            "SOME_FUTURE_SEGMENT",
        ] {
            let err = validate_depth_segment_str(bad).unwrap_err();
            assert!(
                err.contains("NSE_EQ and NSE_FNO"),
                "refusal for {bad:?} must name what IS accepted"
            );
        }
    }
}
