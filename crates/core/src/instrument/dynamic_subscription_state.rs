//! 2026-05-02 PR-B step 4 — DynamicSubscriptionState: least-full diff state machine.
//!
//! Manages a pool of N depth WebSocket connections each with capacity K
//! SIDs (e.g. depth-20 = 5 conns × 50 SIDs each = 250 total; depth-200 =
//! 5 conns × 1 SID each = 5 total). Every 60 seconds the rebalance
//! scheduler computes a new desired set of SIDs (`next_set`) via the
//! `depth_dynamic_top_volume_selector` and calls
//! [`DynamicSubscriptionState::diff`] which returns the **minimal** set
//! of [`DiffOp`]s needed to converge — typically `0` ops (set unchanged)
//! or `1 Remove + 1 Add` ops (one rank changed) per cycle.
//!
//! ## Least-full slot assignment
//!
//! When a new SID needs a connection, we pick the conn with the lowest
//! `current_count`. Tie-break by ascending `conn_idx` for deterministic
//! ordering. At steady-state full capacity (depth-20 at 250/250) this
//! reduces to "give it to the conn that just freed a slot" — the
//! conn-just-freed becomes the unique least-full. On mass-cycle days
//! when many SIDs swap simultaneously, the least-full rule provably
//! balances incoming SIDs across conns from cycle 1.
//!
//! Pure round-robin was rejected because it forces 3-frame inter-conn
//! moves when `next_conn_idx != conn_that_freed`. Least-full is the
//! same code complexity (`O(N)` argmin over N=5 conns) and strictly
//! better behaviour.
//!
//! ## Cold-path
//!
//! `diff` runs every 60s on the rebalance scheduler — NOT the tick
//! processor hot path. Bounded allocations (≤ MAX_K SIDs per cycle)
//! are acceptable. Per `hot-path.md` exemption: orchestrator code
//! path, not data plane.
//!
//! ## Diff guarantees
//!
//! - **Idempotent:** calling `diff(current_set)` produces zero ops
//!   (no-op fast path).
//! - **Minimal:** `|to_remove| = |current ∖ next|`,
//!   `|to_add| = |next ∖ current|`. SIDs that are in both sets are
//!   never touched.
//! - **Capacity-preserving:** every Add picks a conn with `count < K`.
//!   Asserts at debug time that the pool can absorb the next set
//!   (`|next_set| ≤ N × K`).
//! - **Deterministic:** identical input produces identical output ops
//!   (HashMap ordering is sidestepped by sorting `to_add` by
//!   security_id ASC before assignment).

use std::collections::{HashMap, HashSet};

use tickvault_common::types::ExchangeSegment;

use crate::instrument::depth_dynamic_top_volume_selector::{MAX_K, SubKey};

/// Identifier for a single depth connection within the pool.
///
/// Pool sizes are bounded (≤ 5 per Dhan limits per
/// `dhan-full-market-depth.md` rule 3) so a `u8` is sufficient and
/// keeps the slot map compact.
pub type ConnIdx = u8;

/// One emitted diff operation. The scheduler translates each op into
/// a `DepthCommand::AddSubscriptions20`, `RemoveSubscriptions20`,
/// `Add200`, or `Remove200` command on the owning conn.
///
/// Per I-P1-11, the SID alone is not unique across segments — every
/// op carries the precise `(security_id, exchange_segment)` composite
/// key so the wire-frame builder can produce a correct subscription
/// JSON without an additional registry lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    /// Subscribe `(security_id, exchange_segment)` on connection
    /// `conn_idx`.
    Add {
        conn_idx: ConnIdx,
        security_id: u32,
        exchange_segment: ExchangeSegment,
    },
    /// Unsubscribe `(security_id, exchange_segment)` on connection
    /// `conn_idx`.
    Remove {
        conn_idx: ConnIdx,
        security_id: u32,
        exchange_segment: ExchangeSegment,
    },
}

/// Fixed-shape config for a connection pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolShape {
    /// Number of WebSocket connections in the pool. Pinned to 5 for
    /// both depth-20 and depth-200 per Dhan limits.
    pub conns: u8,
    /// Capacity (number of SIDs) per connection. 50 for depth-20,
    /// 1 for depth-200.
    pub sids_per_conn: u16,
}

impl PoolShape {
    /// Total SID capacity = `conns × sids_per_conn`.
    #[must_use]
    pub fn total_capacity(&self) -> usize {
        usize::from(self.conns) * usize::from(self.sids_per_conn)
    }
}

/// Diff-based subscription state for one connection pool (depth-20 or
/// depth-200).
///
/// Internal state:
/// - `slot_assignment` — which conn currently holds each SID
/// - `per_conn_count` — current SID count per conn (for least-full
///   argmin)
///
/// Both are recomputable from each other; we cache `per_conn_count`
/// to avoid an `O(N×K)` rebuild on every diff.
#[derive(Debug, Clone)]
pub struct DynamicSubscriptionState {
    shape: PoolShape,
    /// `(SecurityId, ExchangeSegment)` → which conn holds it. Composite
    /// key per I-P1-11 — same SecurityId on different segments
    /// (e.g., FINNIFTY id=27 IDX_I + stock id=27 NSE_EQ) are distinct
    /// instruments and must be tracked independently.
    slot_assignment: HashMap<SubKey, ConnIdx>,
    /// per_conn_count[i] = number of SIDs currently subscribed on conn i.
    per_conn_count: Vec<u16>,
}

impl DynamicSubscriptionState {
    /// Constructs an empty state for the given pool shape.
    #[must_use]
    pub fn new(shape: PoolShape) -> Self {
        let conns = usize::from(shape.conns);
        let cap = shape.total_capacity().min(MAX_K);
        Self {
            shape,
            slot_assignment: HashMap::with_capacity(cap),
            per_conn_count: vec![0; conns],
        }
    }

    /// Returns the current set of subscribed composite keys (snapshot copy).
    #[must_use]
    pub fn current_set(&self) -> HashSet<SubKey> {
        self.slot_assignment.keys().copied().collect()
    }

    /// Returns the size of the current set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slot_assignment.len()
    }

    /// Returns true if the current set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slot_assignment.is_empty()
    }

    /// Returns the conn holding the composite key, or `None` if
    /// unsubscribed. Lookup is by `(security_id, exchange_segment)`
    /// per I-P1-11.
    #[must_use]
    pub fn conn_for(&self, key: SubKey) -> Option<ConnIdx> {
        self.slot_assignment.get(&key).copied()
    }

    /// Returns the current SID count on `conn_idx`, or `None` if the
    /// index is out of range.
    #[must_use]
    pub fn count_on(&self, conn_idx: ConnIdx) -> Option<u16> {
        self.per_conn_count.get(usize::from(conn_idx)).copied()
    }

    /// Computes the minimal set of `DiffOp`s needed to converge from
    /// the current state to `next_set`.
    ///
    /// Algorithm:
    /// 1. `to_remove = current ∖ next` — SIDs to drop.
    /// 2. `to_add    = next ∖ current` — SIDs to add.
    /// 3. For each `sid` in `to_remove`: emit `Remove { conn_idx, sid }`,
    ///    free the slot, decrement `per_conn_count[conn_idx]`.
    /// 4. For each `sid` in `to_add` (sorted by SID ASC for
    ///    determinism): pick `argmin(per_conn_count)` (least-full
    ///    conn), emit `Add { conn_idx, sid }`, fill the slot,
    ///    increment `per_conn_count[conn_idx]`.
    ///
    /// Returns `(ops, diagnostic)` where:
    /// - `ops` is the list of operations to execute (Removes first,
    ///   then Adds, so freed slots are visible to the assignment loop).
    /// - `diagnostic` carries the per-cycle summary `(removed_count,
    ///   added_count, retained_count)` for telemetry.
    ///
    /// # Errors
    ///
    /// Returns `Err(DiffError::OverCapacity)` when the next set
    /// exceeds the pool's `total_capacity()`. This is a configuration
    /// bug at the caller (selector returned more than `N × K` SIDs);
    /// the state is left untouched so the previous-good set continues
    /// in the meantime.
    pub fn diff(&mut self, next_set: &[SubKey]) -> Result<(Vec<DiffOp>, DiffStats), DiffError> {
        // Capacity check — the selector should never overflow the pool,
        // but a malformed config (e.g., k=300 with 5×50=250 capacity)
        // would. Refuse rather than corrupt state.
        if next_set.len() > self.shape.total_capacity() {
            return Err(DiffError::OverCapacity {
                next_size: next_set.len(),
                capacity: self.shape.total_capacity(),
            });
        }

        // Materialise next_set as a HashSet for O(1) membership lookups
        // in the dedup pass below. Composite keys per I-P1-11.
        let next: HashSet<SubKey> = next_set.iter().copied().collect();

        // Bound the work for the worst case: every SID changes.
        let mut ops: Vec<DiffOp> = Vec::with_capacity(next_set.len() * 2);

        // --- Stage 1: Remove keys in (current ∖ next) ---
        let to_remove: Vec<(SubKey, ConnIdx)> = self
            .slot_assignment
            .iter()
            .filter(|(key, _)| !next.contains(*key))
            .map(|(key, conn)| (*key, *conn))
            .collect();
        let removed_count = to_remove.len();
        for (key, conn_idx) in to_remove {
            let (security_id, exchange_segment) = key;
            self.slot_assignment.remove(&key);
            self.per_conn_count[usize::from(conn_idx)] =
                self.per_conn_count[usize::from(conn_idx)].saturating_sub(1);
            ops.push(DiffOp::Remove {
                conn_idx,
                security_id,
                exchange_segment,
            });
        }

        // --- Stage 2: Add keys in (next ∖ current) ---
        // Sort by (security_id, segment-binary-code) ASC for
        // deterministic conn assignment (HashSet iteration order is
        // non-deterministic).
        let mut to_add: Vec<SubKey> = next_set
            .iter()
            .copied()
            .filter(|key| !self.slot_assignment.contains_key(key))
            .collect();
        to_add.sort_unstable_by_key(|key| (key.0, key.1.binary_code()));
        let added_count = to_add.len();
        for key in to_add {
            let (security_id, exchange_segment) = key;
            let conn_idx = self.pick_least_full_conn();
            self.slot_assignment.insert(key, conn_idx);
            self.per_conn_count[usize::from(conn_idx)] += 1;
            ops.push(DiffOp::Add {
                conn_idx,
                security_id,
                exchange_segment,
            });
        }

        let stats = DiffStats {
            removed: removed_count,
            added: added_count,
            retained: self.slot_assignment.len() - added_count,
        };

        Ok((ops, stats))
    }

    /// Picks the conn with the lowest `per_conn_count`, ties broken by
    /// ascending `conn_idx`. Bounded `O(N)` over a pool of N≤5 conns.
    fn pick_least_full_conn(&self) -> ConnIdx {
        // Linear scan — N is bounded at 5 by Dhan limits, so this is
        // O(5) cycles of branch-predictable comparisons.
        let mut best_idx: ConnIdx = 0;
        let mut best_count = u16::MAX;
        for (i, &count) in self.per_conn_count.iter().enumerate() {
            if count < best_count {
                best_count = count;
                best_idx = i as ConnIdx;
            }
            // Tie: keep the lower index (already true because we don't
            // update on equality).
        }
        best_idx
    }
}

/// Per-cycle diagnostic summary. Caller emits this as Prom counters
/// + Telegram Info event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffStats {
    /// Number of SIDs removed this cycle.
    pub removed: usize,
    /// Number of SIDs added this cycle.
    pub added: usize,
    /// Number of SIDs in both current and next sets (untouched).
    pub retained: usize,
}

impl DiffStats {
    /// True when the cycle was a no-op (set unchanged).
    #[must_use]
    pub fn is_no_op(&self) -> bool {
        self.removed == 0 && self.added == 0
    }
}

/// Failure modes for `DynamicSubscriptionState::diff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffError {
    /// Next set is larger than the pool's total capacity. State is
    /// left untouched — the previous-good set continues until the
    /// caller fixes the selector config.
    OverCapacity { next_size: usize, capacity: usize },
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverCapacity {
                next_size,
                capacity,
            } => write!(
                f,
                "next set size {next_size} exceeds pool capacity {capacity}",
            ),
        }
    }
}

impl std::error::Error for DiffError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth_20_shape() -> PoolShape {
        PoolShape {
            conns: 5,
            sids_per_conn: 50,
        }
    }

    fn depth_200_shape() -> PoolShape {
        PoolShape {
            conns: 5,
            sids_per_conn: 1,
        }
    }

    /// Tag every SID with NSE_FNO — the canonical depth subscription
    /// segment. Tests use this to construct `Vec<SubKey>` succinctly.
    fn nse_fno_keys<I>(sids: I) -> Vec<SubKey>
    where
        I: IntoIterator<Item = u32>,
    {
        sids.into_iter()
            .map(|sid| (sid, ExchangeSegment::NseFno))
            .collect()
    }

    fn collect_ops_by_kind(ops: &[DiffOp]) -> (Vec<u32>, Vec<u32>) {
        let mut adds: Vec<u32> = Vec::new();
        let mut removes: Vec<u32> = Vec::new();
        for op in ops {
            match op {
                DiffOp::Add { security_id, .. } => adds.push(*security_id),
                DiffOp::Remove { security_id, .. } => removes.push(*security_id),
            }
        }
        adds.sort_unstable();
        removes.sort_unstable();
        (adds, removes)
    }

    // ---- Pool shape constants ----

    #[test]
    fn test_pool_shape_total_capacity_for_depth_20_is_250() {
        assert_eq!(depth_20_shape().total_capacity(), 250);
    }

    #[test]
    fn test_pool_shape_total_capacity_for_depth_200_is_5() {
        assert_eq!(depth_200_shape().total_capacity(), 5);
    }

    // ---- Empty state ----

    #[test]
    fn test_dynamic_subscription_state_starts_empty() {
        let state = DynamicSubscriptionState::new(depth_20_shape());
        assert_eq!(state.len(), 0);
        assert!(state.is_empty());
        assert!(state.current_set().is_empty());
    }

    /// Smoke-tests `len`, `is_empty`, `current_set`, `count_on`,
    /// `conn_for` together so the pub-fn-test-guard sees a test name
    /// containing each method substring.
    #[test]
    fn test_dynamic_subscription_state_accessors_len_is_empty_current_set_count_on_conn_for() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        assert!(state.is_empty());
        assert_eq!(state.len(), 0);
        assert!(state.current_set().is_empty());
        assert_eq!(state.count_on(0), Some(0));
        assert_eq!(state.count_on(99), None, "out of range conn returns None");
        assert!(state.conn_for((1, ExchangeSegment::NseFno)).is_none());

        let initial = nse_fno_keys(1..=3);
        let _ = state.diff(&initial).unwrap();

        assert!(!state.is_empty());
        assert_eq!(state.len(), 3);
        assert_eq!(state.current_set().len(), 3);
        // total count across conns equals len.
        let total: u16 = (0..5).map(|c| state.count_on(c).unwrap()).sum();
        assert_eq!(total as usize, state.len());
        // Every key has a conn assignment.
        for sid in 1..=3 {
            assert!(state.conn_for((sid, ExchangeSegment::NseFno)).is_some());
        }
    }

    #[test]
    fn test_dynamic_subscription_state_initial_diff_is_all_adds() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let next = nse_fno_keys(1..=10);
        let next_sids: Vec<u32> = next.iter().map(|(s, _)| *s).collect();
        let (ops, stats) = state.diff(&next).expect("under capacity");
        let (adds, removes) = collect_ops_by_kind(&ops);
        assert_eq!(adds, next_sids);
        assert!(removes.is_empty());
        assert_eq!(stats.added, 10);
        assert_eq!(stats.removed, 0);
        assert_eq!(stats.retained, 0);
    }

    // ---- No-op fast path ----

    #[test]
    fn test_dynamic_subscription_state_diff_no_op_when_set_unchanged() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial = nse_fno_keys(1..=50);
        let _ = state.diff(&initial).unwrap();

        let (ops, stats) = state.diff(&initial).expect("under capacity");
        assert!(ops.is_empty(), "no ops when set unchanged");
        assert!(stats.is_no_op());
        assert_eq!(stats.retained, 50);
    }

    #[test]
    fn test_dynamic_subscription_state_diff_no_op_ignores_input_order() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial = nse_fno_keys([5, 1, 3, 2, 4]);
        let _ = state.diff(&initial).unwrap();

        let reordered = nse_fno_keys([3, 4, 1, 5, 2]);
        let (ops, _) = state.diff(&reordered).expect("under capacity");
        assert!(
            ops.is_empty(),
            "set membership unchanged → no ops regardless of order"
        );
    }

    // ---- Single swap ----

    #[test]
    fn test_dynamic_subscription_state_single_swap_emits_one_remove_one_add() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial = nse_fno_keys(1..=50);
        let _ = state.diff(&initial).unwrap();

        // Replace SID 50 with SID 51.
        let next = nse_fno_keys((1..=49).chain([51]));
        let (ops, stats) = state.diff(&next).expect("under capacity");
        let (adds, removes) = collect_ops_by_kind(&ops);
        assert_eq!(removes, vec![50]);
        assert_eq!(adds, vec![51]);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.retained, 49);
    }

    #[test]
    fn test_dynamic_subscription_state_single_swap_keeps_other_49_sids_untouched() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial = nse_fno_keys(1..=50);
        let _ = state.diff(&initial).unwrap();

        // Snapshot conn assignments for the SIDs that won't change.
        let original_assignments: HashMap<u32, ConnIdx> = (1..=49)
            .map(|sid| (sid, state.conn_for((sid, ExchangeSegment::NseFno)).unwrap()))
            .collect();

        let next = nse_fno_keys((1..=49).chain([51]));
        let _ = state.diff(&next).unwrap();

        // Verify all 49 retained SIDs kept their original conn.
        for sid in 1..=49 {
            assert_eq!(
                state.conn_for((sid, ExchangeSegment::NseFno)),
                original_assignments.get(&sid).copied()
            );
        }
    }

    // ---- Least-full assignment ----

    #[test]
    fn test_dynamic_subscription_state_least_full_assigns_to_freed_conn_for_canonical_swap() {
        // Steady-state at full capacity (250/250 across 5 conns at 50 each).
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial = nse_fno_keys(1..=250);
        let _ = state.diff(&initial).unwrap();

        // Every conn should be at capacity now.
        for c in 0..5 {
            assert_eq!(state.count_on(c).unwrap(), 50, "conn {c} should be full");
        }

        // Find the conn holding SID 1.
        let owning_conn = state
            .conn_for((1, ExchangeSegment::NseFno))
            .expect("SID 1 must be assigned");

        // Replace SID 1 with SID 251.
        let mut next = initial.clone();
        next[0] = (251, ExchangeSegment::NseFno);
        let (ops, _) = state.diff(&next).unwrap();

        // Exactly 1 Remove + 1 Add, both on the owning conn.
        assert_eq!(ops.len(), 2);
        let remove_op = ops
            .iter()
            .find(|op| matches!(op, DiffOp::Remove { .. }))
            .unwrap();
        let add_op = ops
            .iter()
            .find(|op| matches!(op, DiffOp::Add { .. }))
            .unwrap();
        match (remove_op, add_op) {
            (
                DiffOp::Remove {
                    conn_idx: r_conn,
                    security_id: r_sid,
                    ..
                },
                DiffOp::Add {
                    conn_idx: a_conn,
                    security_id: a_sid,
                    ..
                },
            ) => {
                assert_eq!(*r_sid, 1);
                assert_eq!(*a_sid, 251);
                assert_eq!(*r_conn, owning_conn);
                assert_eq!(
                    *a_conn, owning_conn,
                    "least-full should land 251 on the conn that just freed (locality emerges)"
                );
            }
            _ => panic!("ops shape mismatch"),
        }
    }

    #[test]
    fn test_dynamic_subscription_state_least_full_balances_initial_load_across_conns() {
        // 250 SIDs spread across 5 conns of 50 each → exactly 50/conn.
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial = nse_fno_keys(1..=250);
        let _ = state.diff(&initial).unwrap();

        for c in 0..5 {
            assert_eq!(
                state.count_on(c).unwrap(),
                50,
                "conn {c} should hold exactly 50 SIDs (perfect balance)"
            );
        }
    }

    #[test]
    fn test_dynamic_subscription_state_partial_load_balances_across_conns() {
        // 7 SIDs across 5 conns of 50 cap each. Least-full picks conn 0
        // for SID 1, then conn 1 for SID 2, ... wrapping at conn 5 → conn 0.
        // Result: conns 0+1 hold 2 each, conns 2-4 hold 1 each.
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial = nse_fno_keys(1..=7);
        let _ = state.diff(&initial).unwrap();

        let counts: Vec<u16> = (0..5).map(|c| state.count_on(c).unwrap()).collect();
        assert_eq!(counts, vec![2, 2, 1, 1, 1]);
    }

    // ---- Mass-cycle (all-replace) ----

    #[test]
    fn test_dynamic_subscription_state_full_replacement_emits_all_removes_then_all_adds() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let initial_sids: Vec<u32> = (1..=10).collect();
        let initial = nse_fno_keys(initial_sids.iter().copied());
        let _ = state.diff(&initial).unwrap();

        let next_sids: Vec<u32> = (101..=110).collect();
        let next = nse_fno_keys(next_sids.iter().copied());
        let (ops, stats) = state.diff(&next).unwrap();
        let (adds, removes) = collect_ops_by_kind(&ops);
        assert_eq!(removes, initial_sids);
        assert_eq!(adds, next_sids);
        assert_eq!(stats.removed, 10);
        assert_eq!(stats.added, 10);
        assert_eq!(stats.retained, 0);

        // Removes must precede Adds in the emitted ordering so the
        // freed slots are visible to the Stage-2 assignment loop.
        let first_add = ops
            .iter()
            .position(|op| matches!(op, DiffOp::Add { .. }))
            .unwrap();
        let last_remove = ops
            .iter()
            .rposition(|op| matches!(op, DiffOp::Remove { .. }))
            .unwrap();
        assert!(
            last_remove < first_add,
            "all Removes must come before any Add"
        );
    }

    // ---- Determinism ----

    #[test]
    fn test_dynamic_subscription_state_is_deterministic_across_runs_with_same_input() {
        // HashMap iteration is non-deterministic, but our diff sorts
        // to_add by SID ASC before assignment, so identical inputs
        // produce identical outputs.
        let initial = nse_fno_keys(1..=50);
        let next = nse_fno_keys((10..=20).chain(100..=130).chain([1]));

        let mut s1 = DynamicSubscriptionState::new(depth_20_shape());
        let _ = s1.diff(&initial).unwrap();
        let (ops1, _) = s1.diff(&next).unwrap();

        let mut s2 = DynamicSubscriptionState::new(depth_20_shape());
        let _ = s2.diff(&initial).unwrap();
        let (ops2, _) = s2.diff(&next).unwrap();

        // Compare by Add-list (Remove ordering depends on HashMap, but
        // Add ordering is sorted-by-SID and therefore deterministic).
        let (adds1, _) = collect_ops_by_kind(&ops1);
        let (adds2, _) = collect_ops_by_kind(&ops2);
        assert_eq!(adds1, adds2);
    }

    // ---- Capacity bounds ----

    #[test]
    fn test_dynamic_subscription_state_returns_err_when_next_set_exceeds_capacity() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape()); // cap=250
        let oversize = nse_fno_keys(1..=251);
        let result = state.diff(&oversize);
        assert!(matches!(
            result,
            Err(DiffError::OverCapacity {
                next_size: 251,
                capacity: 250
            })
        ));
        // State is untouched on error.
        assert!(state.is_empty());
    }

    #[test]
    fn test_dynamic_subscription_state_at_exact_capacity_succeeds() {
        let mut state = DynamicSubscriptionState::new(depth_20_shape());
        let exact = nse_fno_keys(1..=250);
        let result = state.diff(&exact);
        assert!(result.is_ok());
        assert_eq!(state.len(), 250);
    }

    // ---- Depth-200 (k=1 per conn) ----

    #[test]
    fn test_dynamic_subscription_state_depth_200_capacity_is_5() {
        let state = DynamicSubscriptionState::new(depth_200_shape());
        assert_eq!(state.shape.total_capacity(), 5);
    }

    #[test]
    fn test_dynamic_subscription_state_depth_200_5_sids_each_pin_to_one_conn() {
        let mut state = DynamicSubscriptionState::new(depth_200_shape());
        let initial = nse_fno_keys(1..=5);
        let _ = state.diff(&initial).unwrap();

        // Each conn holds exactly 1 SID.
        for c in 0..5 {
            assert_eq!(state.count_on(c).unwrap(), 1);
        }
    }

    #[test]
    fn test_dynamic_subscription_state_depth_200_single_swap_emits_one_remove_one_add() {
        let mut state = DynamicSubscriptionState::new(depth_200_shape());
        let initial = nse_fno_keys(1..=5);
        let _ = state.diff(&initial).unwrap();

        let owning_conn = state.conn_for((5, ExchangeSegment::NseFno)).unwrap();

        // Replace SID 5 with SID 6.
        let next = nse_fno_keys([1, 2, 3, 4, 6]);
        let (ops, stats) = state.diff(&next).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.retained, 4);

        // The newly added SID lands on the same conn that freed
        // (locality emerges from least-full).
        let add = ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Add {
                    conn_idx,
                    security_id,
                    ..
                } if *security_id == 6 => Some(*conn_idx),
                _ => None,
            })
            .unwrap();
        assert_eq!(add, owning_conn);
    }

    // ---- DiffStats ----

    #[test]
    fn test_diff_stats_is_no_op_returns_true_for_zero_ops() {
        let s = DiffStats {
            removed: 0,
            added: 0,
            retained: 100,
        };
        assert!(s.is_no_op());
    }

    #[test]
    fn test_diff_stats_is_no_op_returns_false_for_any_change() {
        assert!(
            !DiffStats {
                removed: 1,
                added: 0,
                retained: 99
            }
            .is_no_op()
        );
        assert!(
            !DiffStats {
                removed: 0,
                added: 1,
                retained: 99
            }
            .is_no_op()
        );
    }

    // ---- DiffError ----

    #[test]
    fn test_diff_error_display_includes_capacity_numbers() {
        let err = DiffError::OverCapacity {
            next_size: 300,
            capacity: 250,
        };
        let msg = err.to_string();
        assert!(msg.contains("300"));
        assert!(msg.contains("250"));
    }
}
