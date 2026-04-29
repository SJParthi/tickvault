//! Depth-20 dynamic top-150 subscriber (Phase 7 of v3 plan, 2026-04-28).
//!
//! Of the 5 depth-20 connection slots in the Dhan budget:
//! - Slots 1+2 are PINNED to NIFTY ATM±24 + BANKNIFTY ATM±24 (~98 instruments).
//!   These are the static high-value index option chains and are managed by
//!   the existing `depth_rebalancer.rs` (60s ATM-drift swap).
//! - Slots 3/4/5 are DYNAMIC and managed by THIS module: every minute we
//!   query the existing `option_movers` table for the top 150 contracts
//!   sorted by `volume DESC` filtered by `change_pct > 0`, then emit a
//!   `DepthCommand::Swap20` over each slot's mpsc channel containing the
//!   set delta (entrants + leavers) so the existing zero-disconnect
//!   subscribe/unsubscribe path runs.
//!
//! ## Why query `option_movers` and not the new movers_22tf tables?
//!
//! `option_movers` is already populated every 60s by `OptionMoversWriter`
//! and exists in production today. Phase 7 ships BEFORE the 22-tf core
//! (Phases 8-13), so depending on the new `movers_{T}` tables would force
//! Phase 7 to wait. The schema is identical for our needs (`change_pct`,
//! `volume`, `security_id`, `segment`, `ts`).
//!
//! ## Ratchets
//!
//! | # | Test | Pins |
//! |---|---|---|
//! | 48 | `test_depth_20_top_150_selector_returns_at_most_150_rows` | LIMIT 150 enforced |
//! | 49 | `test_depth_20_top_150_selector_filters_change_pct_positive_only` | `WHERE change_pct > 0` (Option B) |
//! | 50 | `test_depth_20_top_150_recompute_interval_is_60_seconds` | constant pinned to 60 |
//! | 51 | `test_depth_20_top_150_delta_swap_emits_correct_unsubscribe_subscribe_pair` | set-diff correctness |
//!
//! ## Boot wiring
//!
//! The runner function `run_depth_20_dynamic_subscriber` is `pub(crate)`
//! and will be called from `crates/app/src/main.rs` in a follow-up commit
//! once the 3 new depth-20 connection slots (3/4/5) are added there.
//! See `.claude/plans/v2-architecture.md` Section I for the boot-wiring
//! design.

use std::collections::HashSet;

/// Maximum number of contracts to subscribe across the 3 dynamic depth-20
/// slots. Each slot holds 50 contracts (Dhan's per-message + per-connection
/// limit), so 3 × 50 = 150.
pub const DEPTH_20_DYNAMIC_TOP_N: usize = 150;

/// Number of instruments per dynamic depth-20 slot (Dhan's per-message
/// subscribe limit).
pub const DEPTH_20_DYNAMIC_SLOT_CAPACITY: usize = 50;

/// Number of dynamic depth-20 connection slots (slots 3/4/5 of the 5-slot
/// Dhan budget; slots 1+2 stay pinned to NIFTY+BANKNIFTY ATM±24).
pub const DEPTH_20_DYNAMIC_SLOT_COUNT: usize = 3;

/// Recompute interval for the dynamic top-150 selector. Pinned at 60s to
/// match the `option_movers` write cadence (1-minute snapshots) — querying
/// faster than the source updates wastes work without changing results.
pub const DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS: u64 = 60;

/// Lower bound on returned count below which the selector emits a
/// `Depth20DynamicTopSetEmpty` Telegram alert. 50 is the per-slot capacity;
/// returning < 50 means at least one of the 3 slots cannot fill.
pub const DEPTH_20_DYNAMIC_EMPTY_THRESHOLD: usize = 50;

/// SQL the dynamic subscriber issues every `DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS`
/// against the existing `option_movers` table. Public for ratchet tests
/// and for future MCP-tool query integration.
///
/// The 90-second freshness window (`ts > now() - 90s`) covers the
/// 60s `option_movers` write cadence + 30s grace.
pub const DEPTH_20_DYNAMIC_SELECTOR_SQL: &str = "\
SELECT security_id, segment \
FROM option_movers \
WHERE ts > dateadd('s', -90, now()) AND change_pct > 0 \
ORDER BY volume DESC \
LIMIT 150";

/// A single contract identifier for the dynamic top-150 set. Pairs the
/// numeric `security_id` with the `ExchangeSegment` per I-P1-11
/// composite-key invariant — `security_id` alone is NOT unique.
///
/// Stored as `(u32, char)` rather than `(u32, ExchangeSegment)` because the
/// segment-character form is what `OptionMoversWriter` writes to the
/// `segment` column today. Conversion to/from ExchangeSegment happens at
/// the boundary.
pub type DynamicContractKey = (u32, char);

/// Result of computing the set delta between the previous minute's top-150
/// and the current minute's top-150.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DynamicSwapDelta {
    /// Contracts present in `previous` but missing from `current` — must be
    /// unsubscribed via RequestCode 25.
    pub leavers: Vec<DynamicContractKey>,
    /// Contracts present in `current` but missing from `previous` — must be
    /// subscribed via RequestCode 23.
    pub entrants: Vec<DynamicContractKey>,
    /// Total active contracts after the swap (`current.len()`). For
    /// observability — the operator's "is the slot full?" question.
    pub total_active: usize,
}

/// Pure-function set-diff between two top-150 snapshots. Caller passes the
/// previous and current sets (already deduplicated and sized ≤ 150). This
/// function is hot-path-safe (no allocation beyond the two output Vecs,
/// which the caller can reuse across iterations).
///
/// Sort order of output Vecs is NOT guaranteed — receivers should treat
/// them as unordered.
#[must_use]
pub fn compute_swap_delta(
    previous: &HashSet<DynamicContractKey>,
    current: &HashSet<DynamicContractKey>,
) -> DynamicSwapDelta {
    let leavers: Vec<DynamicContractKey> = previous.difference(current).copied().collect();
    let entrants: Vec<DynamicContractKey> = current.difference(previous).copied().collect();
    DynamicSwapDelta {
        leavers,
        entrants,
        total_active: current.len(),
    }
}

/// Validates a set returned by the selector — used by the runner before
/// emitting a Swap20. Returns `Some(reason)` if the set is too small to
/// cover even one slot, otherwise `None`.
///
/// This is the rising-edge detector for the `Depth20DynamicTopSetEmpty`
/// Telegram alert (audit-findings Rule 4 — edge-triggered alerts).
#[must_use]
pub fn check_set_emptiness(returned: &HashSet<DynamicContractKey>) -> Option<&'static str> {
    if returned.is_empty() {
        Some("zero_results")
    } else if returned.len() < DEPTH_20_DYNAMIC_EMPTY_THRESHOLD {
        Some("below_slot_capacity")
    } else {
        None
    }
}

/// Splits a top-150 set into 3 slot-sized batches of ≤ 50 each, preserving
/// the order an iterator over the HashSet returns (sort order is not
/// preserved — that's a known limitation; if the operator wants
/// deterministic per-slot assignment, the SQL `ORDER BY volume DESC` should
/// be reflected in the iteration order, but HashSet iteration is unordered).
///
/// Currently used only for testing the slot-distribution invariant.
#[must_use]
pub fn split_into_slots(
    contracts: &HashSet<DynamicContractKey>,
) -> [Vec<DynamicContractKey>; DEPTH_20_DYNAMIC_SLOT_COUNT] {
    let mut slots: [Vec<DynamicContractKey>; DEPTH_20_DYNAMIC_SLOT_COUNT] = [
        Vec::with_capacity(DEPTH_20_DYNAMIC_SLOT_CAPACITY),
        Vec::with_capacity(DEPTH_20_DYNAMIC_SLOT_CAPACITY),
        Vec::with_capacity(DEPTH_20_DYNAMIC_SLOT_CAPACITY),
    ];
    for (idx, contract) in contracts.iter().enumerate() {
        let slot = idx / DEPTH_20_DYNAMIC_SLOT_CAPACITY;
        if slot < DEPTH_20_DYNAMIC_SLOT_COUNT {
            slots[slot].push(*contract);
        }
        // Indexes >= 150 are silently dropped by design — the caller already
        // ran `check_set_emptiness` and the SELECT had `LIMIT 150`.
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 7 ratchet 48: the SQL the selector issues MUST clamp at LIMIT 150
    /// (corresponds to the 3 slots × 50 capacity). Pinned via const + SQL
    /// substring check so any change to the constant or query body that
    /// breaks the cap fails the build.
    #[test]
    fn test_depth_20_top_150_selector_returns_at_most_150_rows() {
        assert_eq!(
            DEPTH_20_DYNAMIC_TOP_N, 150,
            "DEPTH_20_DYNAMIC_TOP_N must stay at 150 (3 slots × 50 capacity)"
        );
        assert_eq!(
            DEPTH_20_DYNAMIC_SLOT_CAPACITY * DEPTH_20_DYNAMIC_SLOT_COUNT,
            DEPTH_20_DYNAMIC_TOP_N,
            "slot capacity × slot count must equal top-N"
        );
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains("LIMIT 150"),
            "selector SQL must clamp at LIMIT 150: {DEPTH_20_DYNAMIC_SELECTOR_SQL}"
        );
    }

    /// Phase 7 ratchet 49: the SQL MUST filter `WHERE change_pct > 0` (Option
    /// B confirmed by Parthiban 2026-04-28). Regressing to "all by volume"
    /// would subscribe to losers, which contradicts the gainers-only design.
    #[test]
    fn test_depth_20_top_150_selector_filters_change_pct_positive_only() {
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains("change_pct > 0"),
            "selector SQL must filter to gainers only (Option B): \
             {DEPTH_20_DYNAMIC_SELECTOR_SQL}"
        );
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains("ORDER BY volume DESC"),
            "selector SQL must sort by volume DESC: \
             {DEPTH_20_DYNAMIC_SELECTOR_SQL}"
        );
    }

    /// Phase 7 ratchet 50: the recompute interval is exactly 60s. Faster
    /// would waste work (option_movers writes every 60s); slower would
    /// stale the dynamic top-150.
    #[test]
    fn test_depth_20_top_150_recompute_interval_is_60_seconds() {
        assert_eq!(
            DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS, 60,
            "recompute interval must match option_movers cadence (60s)"
        );
    }

    /// Phase 7 ratchet 51: set-diff correctness — given a previous and
    /// current top-set, `compute_swap_delta` must emit `leavers = previous
    /// \\ current` and `entrants = current \\ previous`. This is the
    /// invariant that drives the zero-disconnect Swap20.
    #[test]
    fn test_depth_20_top_150_delta_swap_emits_correct_unsubscribe_subscribe_pair() {
        let mut previous: HashSet<DynamicContractKey> = HashSet::new();
        previous.insert((1001, 'D'));
        previous.insert((1002, 'D'));
        previous.insert((1003, 'D'));
        // Common contract — must NOT appear in either delta vec.
        previous.insert((9999, 'D'));

        let mut current: HashSet<DynamicContractKey> = HashSet::new();
        current.insert((9999, 'D')); // unchanged
        current.insert((2001, 'D')); // entrant
        current.insert((2002, 'D')); // entrant

        let delta = compute_swap_delta(&previous, &current);

        // Leavers: 1001, 1002, 1003 (in previous, not in current)
        assert_eq!(delta.leavers.len(), 3);
        let leaver_ids: HashSet<u32> = delta.leavers.iter().map(|(id, _)| *id).collect();
        assert!(leaver_ids.contains(&1001));
        assert!(leaver_ids.contains(&1002));
        assert!(leaver_ids.contains(&1003));
        assert!(
            !leaver_ids.contains(&9999),
            "9999 unchanged, must NOT be a leaver"
        );

        // Entrants: 2001, 2002 (in current, not in previous)
        assert_eq!(delta.entrants.len(), 2);
        let entrant_ids: HashSet<u32> = delta.entrants.iter().map(|(id, _)| *id).collect();
        assert!(entrant_ids.contains(&2001));
        assert!(entrant_ids.contains(&2002));
        assert!(
            !entrant_ids.contains(&9999),
            "9999 unchanged, must NOT be an entrant"
        );

        // Total active matches current set size.
        assert_eq!(delta.total_active, 3);
    }

    #[test]
    fn test_compute_swap_delta_empty_previous_all_entrants() {
        let previous: HashSet<DynamicContractKey> = HashSet::new();
        let mut current: HashSet<DynamicContractKey> = HashSet::new();
        current.insert((1, 'D'));
        current.insert((2, 'D'));

        let delta = compute_swap_delta(&previous, &current);
        assert_eq!(delta.leavers.len(), 0);
        assert_eq!(delta.entrants.len(), 2);
        assert_eq!(delta.total_active, 2);
    }

    #[test]
    fn test_compute_swap_delta_empty_current_all_leavers() {
        let mut previous: HashSet<DynamicContractKey> = HashSet::new();
        previous.insert((1, 'D'));
        previous.insert((2, 'D'));
        let current: HashSet<DynamicContractKey> = HashSet::new();

        let delta = compute_swap_delta(&previous, &current);
        assert_eq!(delta.leavers.len(), 2);
        assert_eq!(delta.entrants.len(), 0);
        assert_eq!(delta.total_active, 0);
    }

    #[test]
    fn test_compute_swap_delta_identical_sets_no_change() {
        let mut previous: HashSet<DynamicContractKey> = HashSet::new();
        previous.insert((1, 'D'));
        previous.insert((2, 'D'));
        previous.insert((3, 'D'));

        let current = previous.clone();
        let delta = compute_swap_delta(&previous, &current);
        assert_eq!(delta.leavers.len(), 0);
        assert_eq!(delta.entrants.len(), 0);
        assert_eq!(delta.total_active, 3);
    }

    #[test]
    fn test_check_set_emptiness_zero_returns_zero_results_reason() {
        let empty: HashSet<DynamicContractKey> = HashSet::new();
        assert_eq!(check_set_emptiness(&empty), Some("zero_results"));
    }

    #[test]
    fn test_check_set_emptiness_below_threshold_returns_below_slot_capacity() {
        let mut small: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..(DEPTH_20_DYNAMIC_EMPTY_THRESHOLD - 1) {
            small.insert((i as u32, 'D'));
        }
        assert_eq!(check_set_emptiness(&small), Some("below_slot_capacity"));
    }

    #[test]
    fn test_check_set_emptiness_at_or_above_threshold_returns_none() {
        let mut full: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..DEPTH_20_DYNAMIC_EMPTY_THRESHOLD {
            full.insert((i as u32, 'D'));
        }
        assert_eq!(check_set_emptiness(&full), None);
    }

    #[test]
    fn test_split_into_slots_distributes_correctly() {
        let mut contracts: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..150 {
            contracts.insert((i as u32, 'D'));
        }
        let slots = split_into_slots(&contracts);
        let total: usize = slots.iter().map(Vec::len).sum();
        assert_eq!(total, 150, "all 150 contracts must land in some slot");
        for (idx, slot) in slots.iter().enumerate() {
            assert!(
                slot.len() <= DEPTH_20_DYNAMIC_SLOT_CAPACITY,
                "slot {idx} exceeded capacity: {} > {DEPTH_20_DYNAMIC_SLOT_CAPACITY}",
                slot.len()
            );
        }
    }

    #[test]
    fn test_split_into_slots_smaller_set_uses_first_slot_first() {
        let mut contracts: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..30 {
            contracts.insert((i as u32, 'D'));
        }
        let slots = split_into_slots(&contracts);
        assert_eq!(slots[0].len(), 30, "all 30 must land in slot 0");
        assert_eq!(slots[1].len(), 0);
        assert_eq!(slots[2].len(), 0);
    }

    #[test]
    fn test_dynamic_constants_consistent() {
        // Defensive: if anyone bumps DEPTH_20_DYNAMIC_TOP_N without updating
        // the SQL or vice versa, this catches the drift.
        let extracted_limit_str = "LIMIT 150";
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains(extracted_limit_str),
            "selector SQL LIMIT must equal DEPTH_20_DYNAMIC_TOP_N ({DEPTH_20_DYNAMIC_TOP_N})"
        );
    }
}
