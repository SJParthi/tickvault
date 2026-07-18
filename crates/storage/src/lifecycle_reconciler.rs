//! Daily lifecycle reconciler — PURE decision logic.
//!
//! The daily orchestrator (§5) compares today's validated CSV against
//! yesterday's `instrument_lifecycle` rows and decides, per instrument,
//! what state transition (if any) occurred. This module is the **pure,
//! I/O-free heart** of that decision: given primitive attributes it
//! returns the [`TransitionKind`] + post-transition [`LifecycleState`].
//!
//! The app boot orchestrator (final Sub-PR #10) owns the I/O loop:
//! it reads yesterday's rows (storage SELECT), reads today's CSV
//! (`core`), calls these pure functions, then applies the §24
//! write-audit-first ordering (INSERT pending audit row → UPSERT
//! lifecycle row → finalize audit row). Keeping the decision pure means
//! every branch is unit-tested here without a live QuestDB.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.
//!
//! # Cross-references
//!
//! * Rule §5 (daily orchestrator algorithm) / §6 (transition_kind) /
//!   §23 (split + segment-move) — `daily-universe-scope-expansion-2026-05-27.md`
//! * `instrument_lifecycle_persistence` — the table contracts + writers
//!   these decisions feed.

use crate::instrument_lifecycle_persistence::{LifecycleState, TransitionKind};

/// §23 corporate-action (split / bonus) detection.
///
/// A split is inferred when today's lot size OR tick size dropped to
/// **half or less** of the previously-recorded value. Both the old and
/// new values must be positive for the ratio to be meaningful (a 0 old
/// value means "unknown" — not a split).
#[must_use]
pub fn is_stock_split(
    old_lot_size: i32,
    new_lot_size: i32,
    old_tick_size: f64,
    new_tick_size: f64,
) -> bool {
    // `new <= old * 0.5` expressed in exact integer arithmetic (i64 to
    // avoid i32 overflow): `2*new <= old`. Avoids f64 entirely — lot
    // sizes are counts, not Dhan prices.
    let lot_split = old_lot_size > 0
        && new_lot_size > 0
        && i64::from(new_lot_size) * 2 <= i64::from(old_lot_size);
    let tick_split = old_tick_size.is_finite()
        && new_tick_size.is_finite()
        && old_tick_size > 0.0
        && new_tick_size > 0.0
        && new_tick_size <= old_tick_size * 0.5;
    lot_split || tick_split
}

/// §5: map a DISAPPEARED instrument's `instrument_type` to the
/// appropriate `expired_*` state.
///
/// - `INDEX` → `ExpiredIndex` (an index no longer published)
/// - `EQUITY` → `ExpiredFromFno` (a stock that dropped out of the F&O
///   list — equities are in our universe only as F&O underlyings)
/// - everything else (derivative contracts FUTSTK/OPTSTK/FUTIDX/OPTIDX/…)
///   → `ExpiredContract` (a specific contract reached expiry)
#[must_use]
pub fn classify_disappearance_state(instrument_type: &str) -> LifecycleState {
    match instrument_type {
        "INDEX" => LifecycleState::ExpiredIndex,
        "EQUITY" => LifecycleState::ExpiredFromFno,
        _ => LifecycleState::ExpiredContract,
    }
}

/// The per-instrument inputs the reconciler needs to classify a
/// transition. All primitive so this module needs neither `core`'s CSV
/// row types (deleted 2026-07-18, dead-code batch 2) nor a live DB.
#[derive(Debug, Clone, Copy)]
pub struct ReconcileInput<'a> {
    /// Yesterday's `lifecycle_state`, or `None` if this
    /// `(security_id, exchange_segment)` has no prior row (brand new).
    pub prev_state: Option<LifecycleState>,
    /// Yesterday's `lifecycle_state_locked` flag — an operator override
    /// that the orchestrator MUST NOT auto-flip (§5).
    pub prev_locked: bool,
    /// Whether this instrument is present in TODAY's validated CSV.
    pub present_in_today_csv: bool,
    /// Whether the same `security_id` now appears under a DIFFERENT
    /// `exchange_segment` than the prior row (§23 segment move). When
    /// true, this is treated as a NEW lifecycle row (composite key
    /// includes segment).
    pub segment_changed: bool,
    /// Whether any tracked mutable field (symbol, lot, tick, …) changed
    /// vs the prior row.
    pub fields_changed: bool,
    /// Whether [`is_stock_split`] flagged a corporate action.
    pub is_split: bool,
    /// The instrument's `instrument_type` — used to pick the right
    /// `expired_*` state on disappearance.
    pub instrument_type: &'a str,
}

/// Classify the transition for one instrument. Returns `None` when there
/// is NO transition to record (no audit row, no UPSERT-state-change) —
/// e.g. an unchanged active row, an already-expired row that stays
/// absent, or a locked row the operator pinned.
///
/// This classifier is called keyed on a PRIOR row's
/// `(security_id, exchange_segment)`. A segment MOVE is therefore handled
/// across two separate calls: the NEW-segment row has no prior row →
/// `Appeared`; the OLD-segment row disappears from today's CSV (under its
/// old segment) → this call's disappearance branch. `segment_changed`
/// only REFINES that disappearance label from `Expired` to `SegmentMoved`
/// (both → an `expired_*` state) so the forensic chain records WHY the
/// old-segment row expired (§23). It is NOT a top-priority branch and
/// never flips the old key back to `Active`.
///
/// Decision order (first match wins):
/// 1. No prior row → `Appeared` (→ `Active`).
/// 2. Prior row is `lifecycle_state_locked` → `None` (operator override —
///    never auto-flip, even on a segment move).
/// 3. Present today:
///    - prior `Expired*` → `Reactivated` (→ `Active`)
///    - prior `Delisted` → `None` (manual terminal — never auto-reactivate)
///    - prior `Active` + split → `Split` (→ `Active`)
///    - prior `Active` + other fields changed → `Updated` (→ `Active`)
///    - prior `Active` + nothing changed → `None`
/// 4. Absent today (disappeared):
///    - prior `Active` + `segment_changed` → `SegmentMoved` (→ `expired_*`)
///    - prior `Active` → `Expired` (→ `classify_disappearance_state`)
///    - prior already `Expired*`/`Delisted` → `None`
#[must_use]
pub fn classify_transition(input: &ReconcileInput<'_>) -> Option<(TransitionKind, LifecycleState)> {
    let Some(prev) = input.prev_state else {
        return Some((TransitionKind::Appeared, LifecycleState::Active));
    };

    if input.prev_locked {
        return None;
    }

    if input.present_in_today_csv {
        if prev.is_expired() {
            return Some((TransitionKind::Reactivated, LifecycleState::Active));
        }
        if prev == LifecycleState::Delisted {
            // Manual terminal state — never auto-reactivate.
            return None;
        }
        // prev == Active
        if input.is_split {
            return Some((TransitionKind::Split, LifecycleState::Active));
        }
        if input.fields_changed {
            return Some((TransitionKind::Updated, LifecycleState::Active));
        }
        None
    } else {
        // Disappeared from today's CSV (under this key's segment).
        if prev == LifecycleState::Active {
            // The old-segment row expires either way; the label records
            // WHY — a segment move vs a plain expiry (§23).
            let kind = if input.segment_changed {
                TransitionKind::SegmentMoved
            } else {
                TransitionKind::Expired
            };
            Some((kind, classify_disappearance_state(input.instrument_type)))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_stock_split ---------------------------------------------------

    #[test]
    fn test_is_stock_split_detected_when_lot_size_halves() {
        // TCS 1000 → 250 lot (split).
        assert!(is_stock_split(1000, 250, 0.05, 0.05));
        // Exactly half also counts.
        assert!(is_stock_split(1000, 500, 0.05, 0.05));
    }

    #[test]
    fn test_split_detected_when_tick_size_halves() {
        assert!(is_stock_split(100, 100, 0.10, 0.05));
    }

    #[test]
    fn test_no_split_on_small_lot_change() {
        // 1000 → 750 is not a split (> half).
        assert!(!is_stock_split(1000, 750, 0.05, 0.05));
    }

    #[test]
    fn test_no_split_when_old_values_zero() {
        // Unknown old value — never a split (avoids false positive on
        // first-seen rows).
        assert!(!is_stock_split(0, 250, 0.0, 0.05));
        assert!(!is_stock_split(0, 0, 0.0, 0.0));
    }

    #[test]
    fn test_no_split_when_lot_grows() {
        assert!(!is_stock_split(250, 1000, 0.05, 0.05));
    }

    // ---- classify_disappearance_state ------------------------------------

    #[test]
    fn test_classify_disappearance_state_index_maps_to_expired_index() {
        assert_eq!(
            classify_disappearance_state("INDEX"),
            LifecycleState::ExpiredIndex
        );
    }

    #[test]
    fn test_disappearance_equity_maps_to_expired_from_fno() {
        assert_eq!(
            classify_disappearance_state("EQUITY"),
            LifecycleState::ExpiredFromFno
        );
    }

    #[test]
    fn test_disappearance_derivative_maps_to_expired_contract() {
        for t in ["FUTSTK", "OPTSTK", "FUTIDX", "OPTIDX", "UNKNOWN"] {
            assert_eq!(
                classify_disappearance_state(t),
                LifecycleState::ExpiredContract,
                "instrument_type {t} must map to ExpiredContract"
            );
        }
    }

    // ---- classify_transition ---------------------------------------------

    fn base_input() -> ReconcileInput<'static> {
        ReconcileInput {
            prev_state: Some(LifecycleState::Active),
            prev_locked: false,
            present_in_today_csv: true,
            segment_changed: false,
            fields_changed: false,
            is_split: false,
            instrument_type: "EQUITY",
        }
    }

    #[test]
    fn test_classify_transition_no_prior_row_is_appeared() {
        let mut i = base_input();
        i.prev_state = None;
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::Appeared, LifecycleState::Active))
        );
    }

    #[test]
    fn test_transition_segment_move_expires_old_row_with_segment_moved_label() {
        // The OLD-segment row disappears (present_in_today_csv=false for
        // its segment) and `segment_changed` flags WHY: SegmentMoved, with
        // an expired_* state (NOT Active). The new-segment row is handled
        // by the Appeared path (prev_state=None) in a separate call.
        let mut i = base_input();
        i.present_in_today_csv = false;
        i.segment_changed = true;
        i.instrument_type = "EQUITY";
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::SegmentMoved, LifecycleState::ExpiredFromFno))
        );
    }

    #[test]
    fn test_transition_segment_change_while_present_is_not_special() {
        // segment_changed only matters on disappearance. A present,
        // unchanged active row with segment_changed set is still "no
        // transition" — the move is recorded on the OLD key's call.
        let mut i = base_input();
        i.segment_changed = true;
        i.present_in_today_csv = true;
        assert_eq!(classify_transition(&i), None);
    }

    #[test]
    fn test_transition_locked_row_is_skipped() {
        let mut i = base_input();
        i.prev_locked = true;
        i.present_in_today_csv = false; // would otherwise expire
        assert_eq!(classify_transition(&i), None);
    }

    #[test]
    fn test_transition_expired_then_present_is_reactivated() {
        let mut i = base_input();
        i.prev_state = Some(LifecycleState::ExpiredFromFno);
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::Reactivated, LifecycleState::Active))
        );
    }

    #[test]
    fn test_transition_delisted_present_is_not_reactivated() {
        let mut i = base_input();
        i.prev_state = Some(LifecycleState::Delisted);
        assert_eq!(
            classify_transition(&i),
            None,
            "Delisted is a manual terminal state — must not auto-reactivate"
        );
    }

    #[test]
    fn test_transition_active_split_is_split() {
        let mut i = base_input();
        i.is_split = true;
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::Split, LifecycleState::Active))
        );
    }

    #[test]
    fn test_transition_split_takes_priority_over_plain_update() {
        let mut i = base_input();
        i.is_split = true;
        i.fields_changed = true;
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::Split, LifecycleState::Active)),
            "split must win over a generic Updated when both hold"
        );
    }

    #[test]
    fn test_transition_active_fields_changed_is_updated() {
        let mut i = base_input();
        i.fields_changed = true;
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::Updated, LifecycleState::Active))
        );
    }

    #[test]
    fn test_transition_active_unchanged_is_none() {
        let i = base_input();
        assert_eq!(classify_transition(&i), None);
    }

    #[test]
    fn test_transition_active_absent_is_expired_with_type_state() {
        let mut i = base_input();
        i.present_in_today_csv = false;
        i.instrument_type = "INDEX";
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::Expired, LifecycleState::ExpiredIndex))
        );
        i.instrument_type = "EQUITY";
        assert_eq!(
            classify_transition(&i),
            Some((TransitionKind::Expired, LifecycleState::ExpiredFromFno))
        );
    }

    #[test]
    fn test_transition_already_expired_absent_is_none() {
        let mut i = base_input();
        i.prev_state = Some(LifecycleState::ExpiredContract);
        i.present_in_today_csv = false;
        assert_eq!(
            classify_transition(&i),
            None,
            "an already-expired row that stays absent has no new transition"
        );
    }

    #[test]
    fn test_transition_locked_blocks_segment_move_expiry() {
        // An operator-locked old-segment row is NOT auto-expired even
        // when its sid moved segments — the lock takes priority (§5
        // "skip locked rows when flipping states").
        let mut i = base_input();
        i.present_in_today_csv = false;
        i.segment_changed = true;
        i.prev_locked = true;
        assert_eq!(classify_transition(&i), None);
    }

    #[test]
    fn test_no_split_on_infinite_tick_size() {
        // A corrupt CSV yielding a non-finite tick size must NOT be
        // mistaken for a split (M1 guard).
        assert!(!is_stock_split(100, 100, f64::INFINITY, 1.0));
        assert!(!is_stock_split(100, 100, 1.0, f64::NAN));
        assert!(!is_stock_split(100, 100, f64::NAN, 0.05));
    }
}
