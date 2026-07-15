//! Dhan+Groww same-instrument coexistence regression — ROW-LEVEL proof.
//!
//! Binding operator mandate (`.claude/rules/project/security-id-uniqueness.md`
//! feed-in-key override 2026-06-28 + `.claude/rules/project/data-integrity.md`
//! "feed-in-key EVERYWHERE" + the daily-universe locks): a Dhan row and a Groww
//! row for the SAME instrument MUST coexist as DISTINCT rows because `feed` is
//! part of the `instrument_lifecycle` DEDUP key
//! (`DEDUP_KEY_INSTRUMENT_LIFECYCLE` = `ts, security_id, exchange_segment, feed`).
//!
//! PR-C's existing F5 test
//! (`shared_master_writer::tests::test_regression_dhan_and_groww_same_id_distinct_under_composite_key`)
//! proves abstract HashSet tuple distinctness with hardcoded literals; the
//! `dedup_segment_meta_guard` pins the DEDUP-key STRING. This test closes the
//! gap between them: it builds TWO REAL `InstrumentLifecycleRow` structs (one
//! `feed = "dhan"`, one `feed = "groww"`) with IDENTICAL `security_id` +
//! `exchange_segment` + designated `ts`, projects the DEDUP-key columns from
//! each, and asserts (1) the full key tuples are DISTINCT (both survive UPSERT)
//! and (2) the same rows WOULD collide if `feed` were dropped from the key —
//! proving `feed` is the discriminator.
//!
//! PR-C3 (2026-07-14): the `daily_universe_fetcher` feature predicate was
//! REMOVED from the cfg below — the feature was deleted with the Dhan
//! instrument chain, `instrument_lifecycle_persistence` (and
//! `InstrumentLifecycleRow`) now compile unconditionally, and keeping the
//! dead predicate would have silently compiled this LIVE operator-mandate
//! ratchet to an EMPTY binary forever (Rule-11 false-OK class).

#![cfg(test)]

use tickvault_common::feed::Feed;
use tickvault_storage::instrument_lifecycle_persistence::{
    DEDUP_KEY_INSTRUMENT_LIFECYCLE, InstrumentLifecycleRow, LIFECYCLE_FEED_DHAN, LifecycleState,
    lifecycle_designated_ts_nanos,
};

/// The DEDUP-key column set for one lifecycle row, projected from the row +
/// the pinned constant designated `ts` (the row struct does not carry `ts` —
/// the writer stamps it from [`lifecycle_designated_ts_nanos`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LifecycleDedupKey {
    ts: i64,
    security_id: i64,
    exchange_segment: String,
    feed: String,
}

/// The same key MINUS `feed` — the projection QuestDB would dedup on if `feed`
/// were removed from `DEDUP_KEY_INSTRUMENT_LIFECYCLE`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LifecycleDedupKeyNoFeed {
    ts: i64,
    security_id: i64,
    exchange_segment: String,
}

impl<'a> From<&InstrumentLifecycleRow<'a>> for LifecycleDedupKey {
    fn from(row: &InstrumentLifecycleRow<'a>) -> Self {
        Self {
            ts: lifecycle_designated_ts_nanos(),
            security_id: row.security_id,
            exchange_segment: row.exchange_segment.to_owned(),
            feed: row.feed.to_owned(),
        }
    }
}

impl<'a> From<&InstrumentLifecycleRow<'a>> for LifecycleDedupKeyNoFeed {
    fn from(row: &InstrumentLifecycleRow<'a>) -> Self {
        Self {
            ts: lifecycle_designated_ts_nanos(),
            security_id: row.security_id,
            exchange_segment: row.exchange_segment.to_owned(),
        }
    }
}

/// Build one real `InstrumentLifecycleRow` for the given identity + feed,
/// filling every other field with deterministic sentinels. Private + test-only
/// (no new `pub fn` — the pub-fn ratchet must stay unchanged).
fn lifecycle_row<'a>(
    security_id: i64,
    exchange_segment: &'a str,
    feed: &'a str,
) -> InstrumentLifecycleRow<'a> {
    InstrumentLifecycleRow {
        last_update_ts_nanos: 0,
        security_id,
        exchange_segment,
        exchange_id: "NSE",
        instrument_type: "EQUITY",
        symbol_name: "SENTINEL",
        display_name: "Sentinel Instrument",
        underlying_security_id: 0,
        underlying_symbol: "",
        lot_size: 1,
        tick_size: 0.05,
        expiry_date_nanos: 0,
        strike_price: 0.0,
        option_type: "",
        lifecycle_state: LifecycleState::Active,
        lifecycle_state_locked: false,
        first_seen_date_nanos: 0,
        last_seen_date_nanos: 0,
        last_active_date_nanos: 0,
        expired_date_nanos: 0,
        prev_symbol_chain: "",
        source_csv_sha256: "",
        dry_run: false,
        feed,
    }
}

#[test]
fn test_dhan_and_groww_lifecycle_rows_coexist_under_feed_in_key() {
    // Sanity-pin the DEDUP-key column order this test projects against, so a
    // future re-order of `DEDUP_KEY_INSTRUMENT_LIFECYCLE` is caught here too.
    assert_eq!(
        DEDUP_KEY_INSTRUMENT_LIFECYCLE,
        "ts, security_id, exchange_segment, feed"
    );

    // IDENTICAL identity (security_id + exchange_segment); only `feed` differs.
    const SECURITY_ID: i64 = 11536;
    const SEGMENT: &str = "NSE_EQ";

    let dhan_row = lifecycle_row(SECURITY_ID, SEGMENT, LIFECYCLE_FEED_DHAN);
    let groww_row = lifecycle_row(SECURITY_ID, SEGMENT, Feed::Groww.as_str());

    // Guard the premise: the two rows really do share id + segment + designated
    // `ts`, and differ ONLY in `feed`.
    assert_eq!(
        dhan_row.security_id, groww_row.security_id,
        "same security_id"
    );
    assert_eq!(
        dhan_row.exchange_segment, groww_row.exchange_segment,
        "same exchange_segment"
    );
    assert_eq!(dhan_row.feed, "dhan");
    assert_eq!(groww_row.feed, "groww");
    assert_ne!(dhan_row.feed, groww_row.feed, "feed is the discriminator");

    // (1) FULL DEDUP key (incl. feed): the two rows are DISTINCT → both survive
    // the UPSERT (a Dhan observation and a Groww observation coexist).
    let dhan_key = LifecycleDedupKey::from(&dhan_row);
    let groww_key = LifecycleDedupKey::from(&groww_row);
    assert_ne!(
        dhan_key, groww_key,
        "with `feed` in the key, the two feeds' rows are DISTINCT and BOTH survive"
    );
    use std::collections::HashSet;
    let mut full_keys: HashSet<LifecycleDedupKey> = HashSet::new();
    assert!(full_keys.insert(dhan_key), "dhan row inserts");
    assert!(
        full_keys.insert(groww_key),
        "groww row inserts as a DISTINCT row (feed differs)"
    );
    assert_eq!(
        full_keys.len(),
        2,
        "both feeds' rows coexist under the feed-in-key DEDUP key"
    );

    // (2) DROP `feed` from the key: the SAME two rows now COLLIDE — proving
    // `feed` is exactly what keeps them distinct. Same designated `ts` is used
    // for both (constant `lifecycle_designated_ts_nanos`), so this is a true
    // collision-but-for-feed, not an artifact of differing timestamps.
    let dhan_key_no_feed = LifecycleDedupKeyNoFeed::from(&dhan_row);
    let groww_key_no_feed = LifecycleDedupKeyNoFeed::from(&groww_row);
    assert_eq!(
        dhan_key_no_feed, groww_key_no_feed,
        "WITHOUT `feed`, the two rows share the same (ts, security_id, \
         exchange_segment) key and WOULD collapse into one — feed is the discriminator"
    );
    let mut no_feed_keys: HashSet<LifecycleDedupKeyNoFeed> = HashSet::new();
    assert!(no_feed_keys.insert(dhan_key_no_feed), "first row inserts");
    assert!(
        !no_feed_keys.insert(groww_key_no_feed),
        "without `feed` the second row COLLIDES (dedup would drop one) — the loss \
         the feed-in-key mandate prevents"
    );
    assert_eq!(
        no_feed_keys.len(),
        1,
        "without `feed` only one row survives"
    );
}
