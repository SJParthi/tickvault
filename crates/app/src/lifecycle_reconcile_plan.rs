//! Daily lifecycle reconcile-plan computation — PURE.
//!
//! Given TODAY's validated universe (primitive attrs) and YESTERDAY's
//! read-back map ([`lifecycle_cache_loader::PrevLifecycleAttrs`]), compute
//! the ordered list of [`ReconcileAction`]s — one per instrument whose
//! state transitions per §5/§6/§23. This is the I/O-free planning step;
//! a follow-up `apply_reconcile_plan` performs the audit-first → UPSERT
//! writes (the §24 ordering) for each action.
//!
//! Lives in `app` because it is the only crate that sees all three
//! inputs: today's universe (from `core`), the read-back map (this
//! crate), and the classifier + enums (from `storage`).
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.
//!
//! # Two passes (the §5 algorithm)
//!
//! 1. **Present today** — for each of today's instruments, look up its
//!    prior row and classify (split / update / reactivate / appeared /
//!    no-op).
//! 2. **Disappeared** — for each prior row NOT in today's universe,
//!    classify the disappearance (expired / segment-moved / no-op). A
//!    disappearance is labelled `SegmentMoved` when the SAME `security_id`
//!    appears under a DIFFERENT segment in today's universe (§23).
//!
//! `fields_changed` is computed by comparing the prior row's
//! `(lot_size, tick_size, symbol_name, instrument_type)` against today's;
//! `is_split` via [`is_stock_split`].

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::{HashMap, HashSet};

use tickvault_storage::instrument_lifecycle_persistence::{LifecycleState, TransitionKind};
use tickvault_storage::lifecycle_reconciler::{
    ReconcileInput, classify_transition, is_stock_split,
};

use crate::lifecycle_cache_loader::{LifecycleKey, PrevLifecycleAttrs};

/// Today's primitive attributes for one instrument, extracted from the
/// daily universe. (The `core::DailyUniverse` → `Vec<TodayInstrumentAttrs>`
/// extraction is a separate follow-up; this module's pure planner takes
/// the already-extracted slice so it is testable with synthetic data.)
#[derive(Debug, Clone, PartialEq)]
pub struct TodayInstrumentAttrs {
    pub security_id: i64,
    pub exchange_segment: String,
    pub instrument_type: String,
    pub lot_size: i32,
    pub tick_size: f64,
    pub symbol_name: String,
}

impl TodayInstrumentAttrs {
    fn key(&self) -> LifecycleKey {
        (self.security_id, self.exchange_segment.clone())
    }
}

/// One planned write: a state transition to persist via the §24
/// audit-first → UPSERT ordering (the `apply` follow-up consumes this).
#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileAction {
    pub key: LifecycleKey,
    /// `None` for an `Appeared` transition (no prior state).
    pub from_state: Option<LifecycleState>,
    pub to_state: LifecycleState,
    pub transition_kind: TransitionKind,
    /// The attributes to write into the lifecycle row + the audit
    /// `*_after` snapshot. For a disappearance (Expired/SegmentMoved)
    /// these are the PRIOR attributes (the instrument isn't in today's
    /// CSV, so today has no fresh values).
    pub instrument_type: String,
    pub lot_size: i32,
    pub tick_size: f64,
    pub symbol_name: String,
}

/// Compute the full reconcile plan. PURE — no I/O. Deterministic given
/// the same inputs.
#[must_use]
pub fn compute_reconcile_plan(
    today: &[TodayInstrumentAttrs],
    prev: &HashMap<LifecycleKey, PrevLifecycleAttrs>,
) -> Vec<ReconcileAction> {
    let mut plan = Vec::new();

    // Index today's keys (for the disappearance pass) and today's
    // security_ids → segments (for §23 segment-move detection).
    let today_keys: HashSet<LifecycleKey> = today.iter().map(TodayInstrumentAttrs::key).collect();
    // §26 contract: the validated universe has one row per
    // (security_id, exchange_segment). If a duplicate slipped past the
    // CSV parser, pass 1 would emit two actions for one instrument.
    // Surface the contract violation in debug/test (no release cost).
    debug_assert_eq!(
        today_keys.len(),
        today.len(),
        "today universe must have unique (security_id, exchange_segment) keys (§26)"
    );
    let mut today_sid_segments: HashMap<i64, HashSet<String>> = HashMap::new();
    for t in today {
        today_sid_segments
            .entry(t.security_id)
            .or_default()
            .insert(t.exchange_segment.clone());
    }

    // ---- Pass 1: present today ----
    for t in today {
        let key = t.key();
        let prior = prev.get(&key);
        let prev_state = prior.map(|p| p.state);
        let prev_locked = prior.is_some_and(|p| p.locked);
        let fields_changed = prior.is_some_and(|p| {
            p.lot_size != t.lot_size
                || (p.tick_size - t.tick_size).abs() > f64::EPSILON
                || p.symbol_name != t.symbol_name
                || p.instrument_type != t.instrument_type
        });
        let is_split =
            prior.is_some_and(|p| is_stock_split(p.lot_size, t.lot_size, p.tick_size, t.tick_size));
        let input = ReconcileInput {
            prev_state,
            prev_locked,
            present_in_today_csv: true,
            segment_changed: false, // segment moves are recorded on the OLD key's disappearance
            fields_changed,
            is_split,
            instrument_type: &t.instrument_type,
        };
        if let Some((kind, to_state)) = classify_transition(&input) {
            plan.push(ReconcileAction {
                key,
                from_state: prev_state,
                to_state,
                transition_kind: kind,
                instrument_type: t.instrument_type.clone(),
                lot_size: t.lot_size,
                tick_size: t.tick_size,
                symbol_name: t.symbol_name.clone(),
            });
        }
    }

    // ---- Pass 2: disappeared (prior rows not in today's universe) ----
    for (key, p) in prev {
        if today_keys.contains(key) {
            continue; // handled in pass 1
        }
        let (security_id, old_segment) = key;
        // §23: the same security_id appearing under a DIFFERENT segment
        // today means this disappearance is a segment move, not a plain
        // expiry.
        let segment_changed = today_sid_segments
            .get(security_id)
            .is_some_and(|segs| segs.iter().any(|s| s != old_segment));
        let input = ReconcileInput {
            prev_state: Some(p.state),
            prev_locked: p.locked,
            present_in_today_csv: false,
            segment_changed,
            fields_changed: false,
            is_split: false,
            instrument_type: &p.instrument_type,
        };
        if let Some((kind, to_state)) = classify_transition(&input) {
            plan.push(ReconcileAction {
                key: key.clone(),
                from_state: Some(p.state),
                to_state,
                transition_kind: kind,
                // Disappearance: carry the PRIOR attrs forward.
                instrument_type: p.instrument_type.clone(),
                lot_size: p.lot_size,
                tick_size: p.tick_size,
                symbol_name: p.symbol_name.clone(),
            });
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today(
        sid: i64,
        seg: &str,
        itype: &str,
        lot: i32,
        tick: f64,
        sym: &str,
    ) -> TodayInstrumentAttrs {
        TodayInstrumentAttrs {
            security_id: sid,
            exchange_segment: seg.to_string(),
            instrument_type: itype.to_string(),
            lot_size: lot,
            tick_size: tick,
            symbol_name: sym.to_string(),
        }
    }

    fn prev(
        state: LifecycleState,
        locked: bool,
        itype: &str,
        lot: i32,
        tick: f64,
        sym: &str,
    ) -> PrevLifecycleAttrs {
        PrevLifecycleAttrs {
            state,
            locked,
            instrument_type: itype.to_string(),
            lot_size: lot,
            tick_size: tick,
            symbol_name: sym.to_string(),
            first_seen_date_nanos: 0,
        }
    }

    #[test]
    fn test_compute_reconcile_plan_new_instrument_appears() {
        let today_v = vec![today(13, "IDX_I", "INDEX", 0, 0.05, "NIFTY")];
        let prev_m = HashMap::new();
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].transition_kind, TransitionKind::Appeared);
        assert_eq!(plan[0].to_state, LifecycleState::Active);
        assert_eq!(plan[0].from_state, None);
        assert_eq!(plan[0].symbol_name, "NIFTY");
    }

    #[test]
    fn test_compute_reconcile_plan_unchanged_active_is_no_action() {
        let today_v = vec![today(13, "IDX_I", "INDEX", 0, 0.05, "NIFTY")];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (13, "IDX_I".to_string()),
            prev(LifecycleState::Active, false, "INDEX", 0, 0.05, "NIFTY"),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert!(plan.is_empty(), "unchanged active row → no action");
    }

    #[test]
    fn test_compute_reconcile_plan_fields_changed_is_updated() {
        let today_v = vec![today(13, "IDX_I", "INDEX", 0, 0.05, "NIFTY 50")];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (13, "IDX_I".to_string()),
            prev(LifecycleState::Active, false, "INDEX", 0, 0.05, "NIFTY"),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].transition_kind, TransitionKind::Updated);
    }

    #[test]
    fn test_compute_reconcile_plan_split_detected() {
        let today_v = vec![today(99887, "NSE_EQ", "EQUITY", 250, 0.05, "TCS")];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (99887, "NSE_EQ".to_string()),
            prev(LifecycleState::Active, false, "EQUITY", 1000, 0.05, "TCS"),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].transition_kind, TransitionKind::Split);
    }

    #[test]
    fn test_compute_reconcile_plan_disappeared_active_expires() {
        let today_v: Vec<TodayInstrumentAttrs> = vec![];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (99887, "NSE_EQ".to_string()),
            prev(LifecycleState::Active, false, "EQUITY", 250, 0.05, "TCS"),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].transition_kind, TransitionKind::Expired);
        assert_eq!(plan[0].to_state, LifecycleState::ExpiredFromFno);
        // Disappearance carries the prior attrs forward.
        assert_eq!(plan[0].symbol_name, "TCS");
        assert_eq!(plan[0].lot_size, 250);
    }

    #[test]
    fn test_compute_reconcile_plan_segment_move_labels_old_row() {
        // sid 500 was NSE_EQ yesterday; today it's NSE_FNO. The old
        // NSE_EQ row disappears → SegmentMoved (expired_*); the new
        // NSE_FNO row appears → Appeared. Two actions.
        let today_v = vec![today(500, "NSE_FNO", "FUTSTK", 50, 0.05, "XYZ")];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (500, "NSE_EQ".to_string()),
            prev(LifecycleState::Active, false, "EQUITY", 100, 0.05, "XYZ"),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert_eq!(plan.len(), 2);
        let appeared = plan
            .iter()
            .find(|a| a.transition_kind == TransitionKind::Appeared)
            .expect("new NSE_FNO row appears");
        assert_eq!(appeared.key, (500, "NSE_FNO".to_string()));
        let moved = plan
            .iter()
            .find(|a| a.transition_kind == TransitionKind::SegmentMoved)
            .expect("old NSE_EQ row segment-moved");
        assert_eq!(moved.key, (500, "NSE_EQ".to_string()));
        assert!(moved.to_state.is_expired(), "moved old row must expire");
    }

    #[test]
    fn test_compute_reconcile_plan_reactivation() {
        let today_v = vec![today(99887, "NSE_EQ", "EQUITY", 250, 0.05, "TCS")];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (99887, "NSE_EQ".to_string()),
            prev(
                LifecycleState::ExpiredFromFno,
                false,
                "EQUITY",
                250,
                0.05,
                "TCS",
            ),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].transition_kind, TransitionKind::Reactivated);
        assert_eq!(plan[0].to_state, LifecycleState::Active);
        assert_eq!(plan[0].from_state, Some(LifecycleState::ExpiredFromFno));
    }

    #[test]
    fn test_compute_reconcile_plan_locked_disappeared_is_skipped() {
        let today_v: Vec<TodayInstrumentAttrs> = vec![];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (99887, "NSE_EQ".to_string()),
            prev(LifecycleState::Active, true, "EQUITY", 250, 0.05, "TCS"),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert!(plan.is_empty(), "locked row must not auto-expire");
    }

    #[test]
    fn test_compute_reconcile_plan_already_expired_absent_no_action() {
        let today_v: Vec<TodayInstrumentAttrs> = vec![];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (99887, "NSE_EQ".to_string()),
            prev(
                LifecycleState::ExpiredContract,
                false,
                "OPTSTK",
                250,
                0.05,
                "TCS",
            ),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert!(plan.is_empty(), "already-expired absent row → no action");
    }

    #[test]
    fn test_compute_reconcile_plan_segment_move_when_old_already_expired_no_action() {
        // sid 500 moved NSE_EQ → NSE_FNO, but the old NSE_EQ row was
        // ALREADY expired last run. The new NSE_FNO row appears; the old
        // row produces NO action (disappearance only fires from Active).
        let today_v = vec![today(500, "NSE_FNO", "FUTSTK", 50, 0.05, "XYZ")];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (500, "NSE_EQ".to_string()),
            prev(
                LifecycleState::ExpiredFromFno,
                false,
                "EQUITY",
                100,
                0.05,
                "XYZ",
            ),
        );
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        assert_eq!(plan.len(), 1, "only the new NSE_FNO row appears");
        assert_eq!(plan[0].transition_kind, TransitionKind::Appeared);
        assert_eq!(plan[0].key, (500, "NSE_FNO".to_string()));
    }

    #[test]
    fn test_compute_reconcile_plan_mixed_universe() {
        // 1 new index + 1 unchanged stock + 1 disappeared stock.
        let today_v = vec![
            today(13, "IDX_I", "INDEX", 0, 0.05, "NIFTY"),   // new
            today(100, "NSE_EQ", "EQUITY", 50, 0.05, "AAA"), // unchanged
        ];
        let mut prev_m = HashMap::new();
        prev_m.insert(
            (100, "NSE_EQ".to_string()),
            prev(LifecycleState::Active, false, "EQUITY", 50, 0.05, "AAA"),
        );
        prev_m.insert(
            (200, "NSE_EQ".to_string()),
            prev(LifecycleState::Active, false, "EQUITY", 75, 0.05, "BBB"),
        ); // disappeared
        let plan = compute_reconcile_plan(&today_v, &prev_m);
        // NIFTY appeared + BBB expired = 2 actions (AAA unchanged → none).
        assert_eq!(plan.len(), 2);
        assert!(
            plan.iter()
                .any(|a| a.transition_kind == TransitionKind::Appeared
                    && a.key == (13, "IDX_I".to_string()))
        );
        assert!(
            plan.iter()
                .any(|a| a.transition_kind == TransitionKind::Expired
                    && a.key == (200, "NSE_EQ".to_string()))
        );
    }
}
