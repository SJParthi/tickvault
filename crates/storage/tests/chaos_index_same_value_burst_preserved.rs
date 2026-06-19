//! TICK-SEQ-01 PR-2b chaos regression — the operator's live index tick-loss.
//!
//! Live NIFTY sequence `23,146.45 → 23,146.75 → 23,146.45`: three ticks in the
//! SAME wall-clock second on an INDEX (`volume = 0`, no trades, price is the
//! ONLY varying field). Under the OLD dedup tiebreaker `payload_hash` (a content
//! fingerprint) the two `45` ticks are byte-identical → identical hash → the
//! return-to-45 tick is silently LOST. The NEW tiebreaker `capture_seq`
//! (replay-stable, distinct per arrival) keeps all three, AND a WAL-replay
//! (same `capture_seq`) collapses true duplicates (idempotent).
//!
//! QuestDB's `DEDUP UPSERT KEYS` semantics = "keep exactly one row per distinct
//! key", which a `HashSet` of keys models faithfully without a live QuestDB.

use std::collections::HashSet;

use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::tick_persistence::{tick_dedup_key, tick_payload_hash};

/// One IDX_I (index) tick: `volume = 0`, `LTQ = 0`, no trade fields — only the
/// price varies, exactly as the live index feed delivers.
fn nifty_index_tick(price: f32, exchange_timestamp: u32) -> ParsedTick {
    ParsedTick {
        security_id: 13,          // NIFTY
        exchange_segment_code: 0, // IDX_I
        last_traded_price: price,
        last_trade_quantity: 0,
        exchange_timestamp, // SECOND-granular — identical for the whole burst
        received_at_nanos: 0,
        average_traded_price: 0.0,
        volume: 0,
        total_sell_quantity: 0,
        total_buy_quantity: 0,
        day_open: 23_000.0,
        day_close: 23_100.0,
        day_high: 23_200.0,
        day_low: 22_900.0,
        open_interest: 0,
        oi_day_high: 0,
        oi_day_low: 0,
        iv: 0.0,
        delta: 0.0,
        gamma: 0.0,
        theta: 0.0,
        vega: 0.0,
    }
}

/// Models QuestDB's dedup key `(ts, security_id, segment, tiebreaker)`.
fn key(t: &ParsedTick, tiebreaker: i64) -> (u32, u32, u8, i64) {
    (
        t.exchange_timestamp,
        t.security_id,
        t.exchange_segment_code,
        tiebreaker,
    )
}

#[test]
fn chaos_index_same_value_burst_preserved() {
    let ts: u32 = 1_780_000_000; // an IST epoch second
    // The operator's EXACT live sequence, all in the SAME second.
    let t1 = nifty_index_tick(23_146.45, ts);
    let t2 = nifty_index_tick(23_146.75, ts);
    let t3 = nifty_index_tick(23_146.45, ts); // price RETURNS to 45

    // --- BUG WITNESS (old key = payload_hash): the two 45s are indistinguishable.
    let (h1, h2, h3) = (
        tick_payload_hash(&t1),
        tick_payload_hash(&t2),
        tick_payload_hash(&t3),
    );
    assert_eq!(
        h1, h3,
        "the two 45 ticks have identical content → identical payload_hash"
    );
    assert_ne!(h1, h2, "the 75 tick differs in price → different hash");
    let old_key_set: HashSet<_> = [key(&t1, h1), key(&t2, h2), key(&t3, h3)]
        .into_iter()
        .collect();
    assert_eq!(
        old_key_set.len(),
        2,
        "REGRESSION WITNESS: the payload_hash key collapses the two 45 ticks → \
         the return-to-45 tick is LOST (only 2 of 3 rows survive)"
    );

    // --- THE FIX (new key = capture_seq): distinct arrivals → distinct seq → all kept.
    // capture_seq = the strictly-monotonic read-loop frame_seq, 1 frame = 1 tick.
    let (s1, s2, s3) = (1_i64, 2, 3);
    let new_key_set: HashSet<_> = [key(&t1, s1), key(&t2, s2), key(&t3, s3)]
        .into_iter()
        .collect();
    assert_eq!(
        new_key_set.len(),
        3,
        "FIX: the capture_seq key keeps ALL THREE ticks (45, 75, 45)"
    );

    // The live dedup key really is capture_seq (not payload_hash). `feed`
    // appended 2026-06-19 (operator "same tables + feed column") — within-Dhan
    // behaviour is unchanged (feed='dhan' constant), so the 45->75->45 burst
    // still keeps all three.
    assert_eq!(tick_dedup_key(), "security_id, segment, capture_seq, feed");
}

#[test]
fn chaos_capture_seq_replay_is_idempotent() {
    let ts: u32 = 1_780_000_000;
    let t1 = nifty_index_tick(23_146.45, ts);
    let t2 = nifty_index_tick(23_146.75, ts);
    let t3 = nifty_index_tick(23_146.45, ts);

    // First (live) write: 3 distinct rows.
    let mut rows: HashSet<_> = HashSet::new();
    for (t, seq) in [(&t1, 1_i64), (&t2, 2), (&t3, 3)] {
        rows.insert(key(t, seq));
    }
    assert_eq!(rows.len(), 3);

    // WAL replay re-delivers the SAME frames with the SAME capture_seq (read back
    // from the WAL v2 record, NOT re-stamped) → identical keys → NO new rows.
    for (t, seq) in [(&t1, 1_i64), (&t2, 2), (&t3, 3)] {
        rows.insert(key(t, seq));
    }
    assert_eq!(
        rows.len(),
        3,
        "replaying the same WAL frames must NOT create duplicate rows (idempotent)"
    );
}

#[test]
fn chaos_deep_same_value_burst_all_distinct() {
    // N-deep same-second, same-value burst: every arrival gets a distinct
    // capture_seq, so none are lost even though payload_hash is identical.
    let ts: u32 = 1_780_000_000;
    let tick = nifty_index_tick(23_146.45, ts);
    let h = tick_payload_hash(&tick);

    // Old key: all N collapse to ONE row (catastrophic loss).
    let old: HashSet<_> = (0..50).map(|_| key(&tick, h)).collect();
    assert_eq!(
        old.len(),
        1,
        "payload_hash collapses an N-deep identical burst to 1"
    );

    // New key: N distinct capture_seq → N rows kept.
    let new: HashSet<_> = (0..50).map(|i| key(&tick, i as i64)).collect();
    assert_eq!(
        new.len(),
        50,
        "capture_seq keeps every arrival in an N-deep burst"
    );
}
