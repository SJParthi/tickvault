//! Wave 5 Item 5 Phase E — depth-200 dynamic top-5 subscriber primitive.
//!
//! Companion to `depth_20_dynamic_subscriber.rs`. The 5 depth-200
//! connections each subscribe to ONE option contract (200-level cap = 1
//! instrument per conn per Dhan protocol, see
//! `.claude/rules/dhan/full-market-depth.md` rule 11).
//!
//! Per the plan §"Item 5 — Depth-200 dynamic top-5", the assignment is:
//!
//! - Conn 1..5 = top 5 contracts ranked by `change_pct DESC` from
//!   `option_movers WHERE category = 'TOP_VOLUME' AND change_pct > 0
//!   AND segment != 'BSE_FNO'`
//! - Re-ranks every 60s; emits `Swap200` only when the rank set changes
//!   (no swap-on-equal hysteresis since 200-level conns are slot-pinned)
//!
//! # Integration with existing infrastructure
//!
//! - SQL: `depth_top_volume_selector::selector_sql(DEPTH_200_DYNAMIC_K)`
//!   (K = 5)
//! - Sanitisation: `depth_top_volume_selector::sanitize`
//! - Fan-out: `depth_top_volume_selector::fan_out_to_depth_200`
//! - Swap command: `DepthCommand::Swap200 { unsubscribe_message,
//!   subscribe_message }` (see `depth_connection.rs::DepthCommand`)
//!
//! # Honest envelope
//!
//! This module ships PURE-LOGIC PRIMITIVES ONLY:
//! - `assign_depth_200_slots` — maps top-5 selector results to the 5
//!   conn slots, deterministic ordering by (change_pct DESC,
//!   security_id ASC) tiebreak
//! - `should_issue_depth_200_swap` — slot-by-slot diff, returns the
//!   slot indices that need a swap
//!
//! The runtime spawn `run_depth_200_dynamic_subscriber` is the next
//! sub-PR and touches `crates/app/src/main.rs` boot path. Wiring
//! requires operator confirm per the §"BLOCKER C1" architecture
//! decision (Option A vs B).

use crate::instrument::depth_top_volume_selector::{DEPTH_200_DYNAMIC_K, DepthSelectorRow};

/// Number of depth-200 connections in the dynamic pool. Pinned by Dhan's
/// 5-conn cap and the plan's 1-instrument-per-conn design.
pub const DEPTH_200_DYNAMIC_CONN_COUNT: usize = DEPTH_200_DYNAMIC_K;

/// One slot assignment — which contract maps to which conn (0..5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Depth200SlotAssignment {
    pub conn_index: usize,
    pub contract: DepthSelectorRow,
}

/// Maps a sanitised top-5 selector result to the 5 conn slots.
///
/// - Caller passes `top_5_sorted` already sanitised + capped at
///   `DEPTH_200_DYNAMIC_K` per `depth_top_volume_selector::sanitize`.
/// - Slot index = position in the input slice (0..5).
/// - Returns `Vec` of length `top_5_sorted.len()` (≤ 5). If the selector
///   returned fewer than 5 contracts, only the available slots are
///   assigned; the trailing conn slots remain on their previous contract
///   (caller's responsibility to track previous state).
#[must_use]
pub fn assign_depth_200_slots(top_5_sorted: &[DepthSelectorRow]) -> Vec<Depth200SlotAssignment> {
    top_5_sorted
        .iter()
        .take(DEPTH_200_DYNAMIC_K)
        .enumerate()
        .map(|(conn_index, &contract)| Depth200SlotAssignment {
            conn_index,
            contract,
        })
        .collect()
}

/// Returns the indices of conn slots that need a `Swap200` command
/// (slot-by-slot diff against the previous assignment).
///
/// Per audit-findings Rule 4 (edge-triggered), this only fires on
/// rank-change; identical assignments produce empty Vec → no swap.
///
/// First-seen case (previous empty) returns ALL current indices —
/// these are `InitialSubscribe200` candidates, NOT `Swap200`. Caller
/// disambiguates via the previous-state map.
#[must_use]
pub fn slots_needing_swap(
    previous: &[Depth200SlotAssignment],
    current: &[Depth200SlotAssignment],
) -> Vec<usize> {
    let mut out = Vec::with_capacity(DEPTH_200_DYNAMIC_K);
    for cur in current {
        let prev_at_slot = previous.iter().find(|p| p.conn_index == cur.conn_index);
        match prev_at_slot {
            Some(p) if p.contract == cur.contract => {
                // Slot unchanged — no swap.
            }
            _ => {
                out.push(cur.conn_index);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sid: u32, seg: u8) -> DepthSelectorRow {
        DepthSelectorRow {
            security_id: sid,
            exchange_segment_code: seg,
        }
    }

    #[test]
    fn test_depth_200_dynamic_conn_count_is_5() {
        // Plan-pinned constant. Must equal DEPTH_200_DYNAMIC_K (= 5).
        assert_eq!(DEPTH_200_DYNAMIC_CONN_COUNT, 5);
        assert_eq!(DEPTH_200_DYNAMIC_CONN_COUNT, DEPTH_200_DYNAMIC_K);
    }

    #[test]
    fn test_assign_depth_200_slots_maps_input_order_to_conn_indices() {
        let input = vec![row(1, 2), row(2, 2), row(3, 2), row(4, 2), row(5, 2)];
        let out = assign_depth_200_slots(&input);
        assert_eq!(out.len(), 5);
        for (i, a) in out.iter().enumerate() {
            assert_eq!(a.conn_index, i);
            assert_eq!(a.contract.security_id, (i + 1) as u32);
        }
    }

    #[test]
    fn test_assign_depth_200_slots_caps_at_5() {
        // Defensive: even if caller passes 6+ rows, only first 5 are taken.
        let input: Vec<_> = (1..=10).map(|i| row(i, 2)).collect();
        let out = assign_depth_200_slots(&input);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn test_assign_depth_200_slots_handles_undersize_input() {
        // Less than 5 input rows: only available slots assigned.
        let input = vec![row(1, 2), row(2, 2)];
        let out = assign_depth_200_slots(&input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].conn_index, 0);
        assert_eq!(out[1].conn_index, 1);
    }

    #[test]
    fn test_assign_depth_200_slots_empty_input_returns_empty() {
        assert!(assign_depth_200_slots(&[]).is_empty());
    }

    #[test]
    fn test_slots_needing_swap_first_seen_returns_all_current_indices() {
        // No previous state → every current slot is a candidate.
        let current = assign_depth_200_slots(&[row(1, 2), row(2, 2), row(3, 2)]);
        let out = slots_needing_swap(&[], &current);
        assert_eq!(out, vec![0, 1, 2]);
    }

    #[test]
    fn test_slots_needing_swap_identical_returns_empty() {
        // No rank change → no swap (edge-triggered semantic).
        let assignment = assign_depth_200_slots(&[row(1, 2), row(2, 2), row(3, 2)]);
        let out = slots_needing_swap(&assignment, &assignment);
        assert!(out.is_empty());
    }

    #[test]
    fn test_slots_needing_swap_partial_change_returns_only_changed_indices() {
        let prev = assign_depth_200_slots(&[row(1, 2), row(2, 2), row(3, 2)]);
        // Slot 1 changed contract; slots 0 + 2 unchanged.
        let curr = assign_depth_200_slots(&[row(1, 2), row(99, 2), row(3, 2)]);
        let out = slots_needing_swap(&prev, &curr);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn test_slots_needing_swap_full_change_returns_all_5() {
        let prev = assign_depth_200_slots(&[row(1, 2), row(2, 2), row(3, 2), row(4, 2), row(5, 2)]);
        let curr =
            assign_depth_200_slots(&[row(11, 2), row(12, 2), row(13, 2), row(14, 2), row(15, 2)]);
        let out = slots_needing_swap(&prev, &curr);
        assert_eq!(out, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_slots_needing_swap_input_shrinkage_only_reports_current_slots() {
        // Previous had 5 slots, current has 3 → no swap reported for
        // slots 3, 4 (caller decides what to do with the orphan
        // depth-200 conns: keep last contract, or unsubscribe).
        let prev = assign_depth_200_slots(&[row(1, 2), row(2, 2), row(3, 2), row(4, 2), row(5, 2)]);
        let curr = assign_depth_200_slots(&[row(1, 2), row(2, 2), row(3, 2)]);
        let out = slots_needing_swap(&prev, &curr);
        assert!(out.is_empty(), "no swaps when slots 0..2 are unchanged");
    }

    #[test]
    fn test_slot_assignment_is_copy() {
        // Hot-path requirement: passing assignments via channel should
        // be allocation-free.
        fn assert_copy<T: Copy>() {}
        assert_copy::<Depth200SlotAssignment>();
    }
}
