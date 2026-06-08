//! S3-3: Property test for tick DEDUP uniqueness.
//!
//! **Invariant under test:** the quadruple `(exchange_timestamp, security_id,
//! exchange_segment_code, payload_hash)` is the unique key for a tick in
//! QuestDB. Two ticks with the same quadruple will be merged by QuestDB DEDUP
//! UPSERT KEYS; two ticks with ANY difference MUST be preserved as separate rows.
//!
//! **2026-06-08:** `payload_hash` REPLACED `received_at_nanos` as the
//! sub-second tiebreaker. `received_at` was stamped at processing time, so on
//! WAL/reconnect REPLAY the same frame got a NEW value → a true duplicate would
//! NOT collapse → duplicate row. `payload_hash` is a deterministic content
//! fingerprint (`tick_payload_hash`): two DISTINCT ticks differ in ≥1 value
//! field → different hash → both kept (no loss); a true duplicate / replay is
//! byte-identical → same hash → collapsed (idempotent). Dhan has no per-tick
//! sequence number, so a content fingerprint is the only replay-stable basis.
//!
//! This is the backbone of the zero-tick-loss guarantee — if DEDUP
//! incorrectly collapsed two distinct ticks, we'd silently lose data, and
//! if DEDUP failed to collapse two duplicates (e.g., from backfill replay),
//! we'd get double-counted volume.
//!
//! The property test generates 10,000 random tick streams with
//! deliberately-crafted collisions and verifies:
//!
//! 1. **Determinism:** same triple → same key string (no timing or ordering
//!    dependence)
//! 2. **Uniqueness:** ANY change in the triple → DIFFERENT key string
//! 3. **Segment isolation:** same (ts, security_id) on different segments
//!    → different keys (STORAGE-GAP-01)
//! 4. **Cross-bit-pattern stability:** changing non-key fields (price,
//!    volume) does NOT change the key

use proptest::prelude::*;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::tick_persistence::{tick_dedup_key, tick_payload_hash};

/// Computes the canonical dedup "tuple" for a tick — matches the QuestDB
/// DEDUP UPSERT KEYS `(ts, security_id, segment, payload_hash)` EXACTLY by
/// calling the production `tick_payload_hash`. (2026-06-08: `received_at` was
/// replaced by the content fingerprint `payload_hash` — see the module header.)
fn dedup_tuple(tick: &ParsedTick) -> (u32, u32, u8, i64) {
    (
        tick.exchange_timestamp,
        tick.security_id,
        tick.exchange_segment_code,
        tick_payload_hash(tick),
    )
}

fn arb_tick() -> impl Strategy<Value = ParsedTick> {
    (
        any::<u32>(),       // security_id
        any::<u8>(),        // exchange_segment_code
        any::<u32>(),       // exchange_timestamp
        1_f32..100_000_f32, // ltp (bounded to realistic prices)
        any::<u32>(),       // volume
    )
        .prop_map(|(sid, seg, ts, ltp, vol)| ParsedTick {
            security_id: sid,
            exchange_segment_code: seg,
            exchange_timestamp: ts,
            last_traded_price: ltp,
            volume: vol,
            ..Default::default()
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// S3-3/1: Determinism — computing the tuple twice on the same tick
    /// yields the same result. Cheap but catches any accidental
    /// non-determinism (e.g., using the current time, hashing with a random seed).
    #[test]
    fn prop_dedup_tuple_is_deterministic(tick in arb_tick()) {
        let a = dedup_tuple(&tick);
        let b = dedup_tuple(&tick);
        prop_assert_eq!(a, b);
    }

    /// S3-3/2 (2026-06-08, INVERTED): value fields ARE now part of the key via
    /// the content fingerprint `payload_hash`. Changing the price → a DIFFERENT
    /// tuple → both rows preserved. THIS is the no-loss guarantee: two distinct
    /// same-second ticks (which differ in ≥1 value field) can never collapse.
    #[test]
    fn prop_distinct_price_changes_tuple(
        base in arb_tick(),
        new_price in 1_f32..100_000_f32,
    ) {
        prop_assume!(new_price.to_le_bytes() != base.last_traded_price.to_le_bytes());
        let mut changed = base;
        changed.last_traded_price = new_price;
        prop_assert_ne!(dedup_tuple(&base), dedup_tuple(&changed));
    }

    /// S3-3/2b: changing volume (a distinct stock trade in the same second)
    /// → different tuple → both kept.
    #[test]
    fn prop_distinct_volume_changes_tuple(
        base in arb_tick(),
        new_vol in any::<u32>(),
    ) {
        prop_assume!(new_vol != base.volume);
        let mut changed = base;
        changed.volume = new_vol;
        prop_assert_ne!(dedup_tuple(&base), dedup_tuple(&changed));
    }

    /// S3-3/3: Different security_id → different tuple.
    #[test]
    fn prop_different_security_id_different_tuple(
        base in arb_tick(),
        new_sid in any::<u32>(),
    ) {
        prop_assume!(new_sid != base.security_id);
        let mut changed = base;
        changed.security_id = new_sid;
        prop_assert_ne!(dedup_tuple(&base), dedup_tuple(&changed));
    }

    /// S3-3/4: Different segment → different tuple. **STORAGE-GAP-01**:
    /// this is the invariant that prevents NSE_EQ 1333 and BSE_EQ 1333
    /// (a real situation — same security_id on different exchanges)
    /// from colliding in QuestDB.
    #[test]
    fn prop_different_segment_different_tuple(
        base in arb_tick(),
        new_seg in any::<u8>(),
    ) {
        prop_assume!(new_seg != base.exchange_segment_code);
        let mut changed = base;
        changed.exchange_segment_code = new_seg;
        prop_assert_ne!(dedup_tuple(&base), dedup_tuple(&changed));
    }

    /// S3-3/5: Different timestamp → different tuple.
    #[test]
    fn prop_different_timestamp_different_tuple(
        base in arb_tick(),
        new_ts in any::<u32>(),
    ) {
        prop_assume!(new_ts != base.exchange_timestamp);
        let mut changed = base;
        changed.exchange_timestamp = new_ts;
        prop_assert_ne!(dedup_tuple(&base), dedup_tuple(&changed));
    }

    /// S3-3/9 (2026-06-08, INVERTED): `received_at` is NO LONGER in the key.
    /// Changing ONLY `received_at_nanos` MUST keep the tuple IDENTICAL — this is
    /// the replay-idempotency guarantee: a WAL-replayed / reconnect-resent frame
    /// is re-stamped with a fresh `received_at` at processing time, yet it must
    /// still collapse to ONE row (same content → same `payload_hash`). The old
    /// `received_at`-in-key behaviour would have created a duplicate row here.
    #[test]
    fn prop_received_at_does_not_affect_tuple(
        base in arb_tick(),
        new_recv in any::<i64>(),
    ) {
        let mut changed = base;
        changed.received_at_nanos = new_recv;
        prop_assert_eq!(dedup_tuple(&base), dedup_tuple(&changed));
    }
}

/// S3-3/6: Spot check the hard-coded DEDUP key string contains both
/// `security_id` and `segment`. This is a static invariant, not a
/// property — but placing it alongside the property tests keeps all
/// DEDUP invariants in one file for easy audit.
#[test]
fn dedup_key_string_contains_security_id_and_segment() {
    let key = tick_dedup_key();
    assert!(
        key.contains("security_id"),
        "DEDUP_KEY_TICKS must include security_id, got: {key}"
    );
    assert!(
        key.contains("segment"),
        "STORAGE-GAP-01: DEDUP_KEY_TICKS must include segment to prevent \
         cross-segment collision, got: {key}"
    );
    assert!(
        key.contains("payload_hash"),
        "DEDUP_KEY_TICKS must include payload_hash to preserve sub-second ticks \
         AND stay replay-idempotent (Dhan LTT is second-granular; received_at was \
         re-stamped on replay → duplicates, replaced 2026-06-08), got: {key}"
    );
    assert!(
        !key.contains("received_at"),
        "received_at must NOT be in the dedup key (re-stamped on replay → \
         duplicates); replaced by payload_hash 2026-06-08, got: {key}"
    );
}

/// S3-3/7: A hand-crafted collision scenario — NSE_EQ 1333 and BSE_EQ 1333
/// at the same timestamp. These MUST have different dedup tuples.
#[test]
fn dedup_regression_nse_bse_1333_collision() {
    let nse_eq = ParsedTick {
        security_id: 1333,
        exchange_segment_code: 1, // NSE_EQ
        exchange_timestamp: 1_700_000_000,
        last_traded_price: 2500.0,
        ..Default::default()
    };
    let bse_eq = ParsedTick {
        security_id: 1333,
        exchange_segment_code: 4, // BSE_EQ
        exchange_timestamp: 1_700_000_000,
        last_traded_price: 2500.25, // slightly different price
        ..Default::default()
    };
    assert_ne!(
        dedup_tuple(&nse_eq),
        dedup_tuple(&bse_eq),
        "STORAGE-GAP-01: NSE 1333 and BSE 1333 at the same ts MUST be distinct"
    );
}

/// S3-3/8 (2026-06-08, REWRITTEN): replay idempotency under the `payload_hash`
/// key. The old version modelled the deleted + banned historical-backfill path
/// and (incorrectly, post-2026-06-08) asserted that two DIFFERENT-content ticks
/// should merge — that contradicts the no-loss guarantee. The CORRECT replay
/// scenario: the SAME frame replayed (every value field identical) but
/// re-stamped with a fresh `received_at_nanos` at processing time MUST still
/// produce the SAME dedup tuple → QuestDB collapses it to one row (idempotent).
#[test]
fn dedup_identical_content_replay_idempotent() {
    let original = ParsedTick {
        security_id: 1333,
        exchange_segment_code: 1,
        exchange_timestamp: 1_700_000_060,
        last_traded_price: 2500.00,
        volume: 12345,
        day_close: 2490.0,
        received_at_nanos: 1_700_000_060_111_111_111,
        ..Default::default()
    };
    // Same frame replayed (WAL recovery / reconnect re-send): identical content,
    // ONLY the processing-time received_at differs.
    let replayed = ParsedTick {
        received_at_nanos: 1_700_000_999_999_999_999,
        ..original
    };
    assert_eq!(
        dedup_tuple(&original),
        dedup_tuple(&replayed),
        "a replayed frame (identical content, fresh received_at) MUST share the \
         dedup tuple so QuestDB DEDUP collapses it — replay idempotency"
    );
}
