//! Property-based tests for the Groww auto-scale shard cutter (plan Item 9,
//! `.claude/plans/active-plan-groww-autoscale.md` PR-2; §34 authorization in
//! `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`).
//!
//! Invariants pinned over ARBITRARY watch-sets (the GROWW-SCALE-03 contract):
//! - shards are DISJOINT — no `(exchange, segment, security_id)` identity in
//!   two shards
//! - the shard union EXACTLY covers the input watch-set (no drop, no invent)
//! - every shard holds at most `instruments_per_conn` entries; only the tail
//!   shard may be smaller
//! - conn ids are contiguous `0..n` and shard count == `required_connections`
//! - the cut is DETERMINISTIC: any permutation of the same input produces
//!   byte-identical shards
//! - duplicate identities in the input always fail closed
//!   (`ShardCutError::DuplicateIdentity`), never a silent double-subscribe

use std::collections::BTreeSet;

use proptest::prelude::*;
use tickvault_core::feed::groww::instruments::{WatchEntry, WatchKind};
use tickvault_core::feed::groww::shard_cutter::{ShardCutError, cut_shards, required_connections};

/// Identity key per the I-P1-11-style composite the cutter dedupes on.
type Identity = (String, String, i64);

fn identity(e: &WatchEntry) -> Identity {
    (e.exchange.clone(), e.segment.clone(), e.security_id)
}

fn make_entry(exchange: &str, segment: &str, security_id: i64) -> WatchEntry {
    WatchEntry {
        exchange: exchange.to_owned(),
        segment: segment.to_owned(),
        exchange_token: security_id.to_string(),
        kind: WatchKind::Ltp,
        security_id,
        isin: None,
        symbol_name: None,
        index_name: None,
        expiry_date: None,
        underlying_symbol: None,
    }
}

/// Strategy: a watch-set of UNIQUE identities across 2 exchanges × 2 segments
/// with security ids in a smallish range (forces cross-exchange/segment
/// interleaving so the sort order genuinely matters).
fn unique_watch_set() -> impl Strategy<Value = Vec<WatchEntry>> {
    proptest::collection::btree_set(
        (
            prop_oneof![Just("NSE"), Just("BSE")],
            prop_oneof![Just("CASH"), Just("FNO")],
            0i64..500,
        ),
        0..120,
    )
    .prop_map(|set| {
        set.into_iter()
            .map(|(ex, seg, sid)| make_entry(ex, seg, sid))
            .collect()
    })
}

proptest! {
    /// Disjoint + covering + capacity + contiguous conn ids, for arbitrary
    /// unique watch-sets and arbitrary valid per-conn caps.
    #[test]
    fn arbitrary_watch_sets_always_disjoint_and_covering(
        entries in unique_watch_set(),
        per_conn in 1usize..40,
    ) {
        let shards = cut_shards(&entries, per_conn)
            .expect("unique watch-set with valid cap must cut");

        // Shard count matches the ceil-division contract.
        prop_assert_eq!(shards.len(), required_connections(entries.len(), per_conn));

        // Conn ids are contiguous 0..n.
        for (i, shard) in shards.iter().enumerate() {
            prop_assert_eq!(shard.conn_id, i);
        }

        // Capacity: every shard ≤ per_conn; only the LAST may be smaller.
        for (i, shard) in shards.iter().enumerate() {
            prop_assert!(shard.entries.len() <= per_conn);
            prop_assert!(!shard.entries.is_empty());
            if i + 1 < shards.len() {
                prop_assert_eq!(shard.entries.len(), per_conn);
            }
        }

        // DISJOINT: no identity appears in two shards.
        let mut seen: BTreeSet<Identity> = BTreeSet::new();
        for shard in &shards {
            for e in &shard.entries {
                prop_assert!(
                    seen.insert(identity(e)),
                    "identity {:?} appeared in two shards", identity(e)
                );
            }
        }

        // COVERING: union of shard identities == input identities exactly.
        let input: BTreeSet<Identity> = entries.iter().map(identity).collect();
        prop_assert_eq!(seen, input);
    }

    /// Determinism: any permutation of the same watch-set cuts into
    /// byte-identical shards (restart / re-fetch order independence).
    #[test]
    fn arbitrary_permutations_cut_identically(
        entries in unique_watch_set(),
        per_conn in 1usize..40,
        seed in any::<u64>(),
    ) {
        let baseline = cut_shards(&entries, per_conn)
            .expect("unique watch-set with valid cap must cut");

        // Deterministic pseudo-shuffle from the seed (no rand dep needed).
        let mut shuffled = entries.clone();
        let n = shuffled.len();
        if n > 1 {
            let mut state = seed | 1;
            for i in (1..n).rev() {
                // xorshift64* step
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let j = (state as usize) % (i + 1);
                shuffled.swap(i, j);
            }
        }

        let permuted = cut_shards(&shuffled, per_conn)
            .expect("permutation of a valid watch-set must cut");
        prop_assert_eq!(baseline, permuted);
    }

    /// Duplicate identities always fail closed with DuplicateIdentity —
    /// never a silent drop, never a double-subscribe.
    #[test]
    fn duplicate_identity_always_fails_closed(
        entries in unique_watch_set().prop_filter("need ≥1 entry", |v| !v.is_empty()),
        per_conn in 1usize..40,
        dup_index in any::<proptest::sample::Index>(),
    ) {
        let mut with_dup = entries.clone();
        let dup = with_dup[dup_index.index(with_dup.len())].clone();
        with_dup.push(dup.clone());

        let err = cut_shards(&with_dup, per_conn)
            .expect_err("duplicate identity must be rejected");
        prop_assert_eq!(
            err,
            ShardCutError::DuplicateIdentity {
                exchange: dup.exchange,
                segment: dup.segment,
                security_id: dup.security_id,
            }
        );
    }
}
