//! Pure value-resolution helpers for the daily reconciler's apply step.
//!
//! `apply_reconcile_plan` (the async I/O loop, a follow-up) walks the
//! `Vec<ReconcileAction>` from `lifecycle_reconcile_plan` and, per action,
//! applies the §24 audit-first ordering: write the
//! `instrument_lifecycle_audit` row, then EITHER a full-row UPSERT (present
//! transitions) OR an in-place state UPDATE (disappearances). This module
//! holds the **pure, I/O-free** value decisions that step needs:
//!
//! * [`expiry_date_to_ist_nanos`] — `"YYYY-MM-DD"` → IST-wall-clock nanos
//!   for the lifecycle row's `expiry_date` column;
//! * [`resolve_first_seen_nanos`] — preserve the prior `first_seen_date`
//!   on an existing-row UPSERT (never reset it to today);
//! * [`is_upsert_transition`] — route present (→ UPSERT) vs disappearance
//!   (→ UPDATE).
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.
//!
//! # §24 write-audit-first ordering (for the follow-up apply loop)
//!
//! The append-only `instrument_lifecycle_audit` table carries
//! `transition_kind` IN its DEDUP key, so the literal §24 "INSERT pending
//! → UPSERT → UPDATE audit to real kind" cannot work (the kind change
//! re-identifies the row). The canonical ordering for THIS append-only
//! table is therefore: **(1) append the audit row with the REAL
//! transition_kind, (2) then apply the lifecycle UPSERT/UPDATE.** A crash
//! between leaves an audit row recording the intended transition; the
//! next boot's idempotent re-run (deterministic CSV diff + DEDUP) completes
//! the lifecycle write. This achieves §24's intent (never a lifecycle
//! change without a forensic row) and is documented in the rule file.
//!
//! # Timestamp convention
//!
//! An IST-local calendar date is represented as nanoseconds by treating
//! the local midnight AS UTC (`naive.and_utc().timestamp_nanos`), so the
//! stored value renders as that IST wall-clock date in QuestDB — matching
//! how the lifecycle row's other date columns are produced. NO IST offset
//! is added here (the offset add is only for converting a real `Utc::now()`
//! instant; see `data-integrity.md`).

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;

use chrono::NaiveDate;
use tickvault_storage::instrument_lifecycle_persistence::LifecycleState;

use crate::lifecycle_cache_loader::{LifecycleKey, PrevLifecycleAttrs};

/// Convert a Dhan `"YYYY-MM-DD"` expiry string to IST-wall-clock
/// nanoseconds (midnight of that date). Returns `0` for an empty or
/// unparseable string (spot/index rows have no expiry — `0` is the
/// documented "no expiry" sentinel, consistent with the writer's other
/// optional date columns).
///
/// The date is treated as IST-local midnight rendered as nanos (no UTC
/// offset add) so QuestDB displays the calendar date as-is.
#[must_use]
pub fn expiry_date_to_ist_nanos(expiry: &str) -> i64 {
    let trimmed = expiry.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") else {
        return 0;
    };
    let Some(midnight) = date.and_hms_opt(0, 0, 0) else {
        return 0;
    };
    midnight.and_utc().timestamp_nanos_opt().unwrap_or(0)
}

/// Resolve the `first_seen_date` nanos to write on a present-row UPSERT.
///
/// If a prior row exists with a real (`> 0`) `first_seen_date`, PRESERVE
/// it — a full-row UPSERT must not reset "first ever seen" to today
/// (SEBI forensic, Rule 11). Otherwise (brand-new `Appeared`, or a prior
/// row whose `first_seen` was never set) use `today_ist_nanos`.
#[must_use]
pub fn resolve_first_seen_nanos(
    key: &LifecycleKey,
    prev: &HashMap<LifecycleKey, PrevLifecycleAttrs>,
    today_ist_nanos: i64,
) -> i64 {
    match prev.get(key) {
        Some(p) if p.first_seen_date_nanos > 0 => p.first_seen_date_nanos,
        _ => today_ist_nanos,
    }
}

/// Route a transition's write mechanism: `true` → full-row UPSERT
/// (present transitions, all of which target `Active`); `false` →
/// in-place state UPDATE (disappearances, which target an `expired_*`
/// state). Reads the action's post-transition state.
#[must_use]
pub fn is_upsert_transition(to_state: LifecycleState) -> bool {
    to_state.is_active()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expiry_date_to_ist_nanos_valid() {
        // 2025-12-25 00:00:00 treated as UTC-nanos so it renders as the
        // IST calendar date. Cross-checked against chrono directly.
        let expected = NaiveDate::from_ymd_opt(2025, 12, 25)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap();
        assert_eq!(expiry_date_to_ist_nanos("2025-12-25"), expected);
        assert!(expected > 0);
    }

    #[test]
    fn test_expiry_date_to_ist_nanos_trims_whitespace() {
        assert_eq!(
            expiry_date_to_ist_nanos("  2025-12-25  "),
            expiry_date_to_ist_nanos("2025-12-25")
        );
    }

    #[test]
    fn test_expiry_date_to_ist_nanos_empty_is_zero() {
        assert_eq!(expiry_date_to_ist_nanos(""), 0);
        assert_eq!(expiry_date_to_ist_nanos("   "), 0);
    }

    #[test]
    fn test_expiry_date_to_ist_nanos_garbage_is_zero() {
        assert_eq!(expiry_date_to_ist_nanos("not-a-date"), 0);
        assert_eq!(
            expiry_date_to_ist_nanos("2025-13-99"),
            0,
            "invalid month/day"
        );
        assert_eq!(expiry_date_to_ist_nanos("25/12/2025"), 0, "wrong format");
    }

    #[test]
    fn test_expiry_date_to_ist_nanos_leap_day() {
        // 2028-02-29 is a valid leap day; must parse, not zero.
        assert!(expiry_date_to_ist_nanos("2028-02-29") > 0);
        // 2027-02-29 is NOT a leap day → invalid → 0.
        assert_eq!(expiry_date_to_ist_nanos("2027-02-29"), 0);
    }

    fn prev_with_first_seen(first_seen: i64) -> HashMap<LifecycleKey, PrevLifecycleAttrs> {
        let mut m = HashMap::new();
        m.insert(
            (13, "IDX_I".to_string()),
            PrevLifecycleAttrs {
                state: LifecycleState::Active,
                locked: false,
                instrument_type: "INDEX".to_string(),
                lot_size: 0,
                tick_size: 0.05,
                symbol_name: "NIFTY".to_string(),
                first_seen_date_nanos: first_seen,
            },
        );
        m
    }

    #[test]
    fn test_resolve_first_seen_nanos_preserves_prior() {
        let prev = prev_with_first_seen(1_699_920_000_000_000_000);
        let today = 1_700_500_000_000_000_000;
        assert_eq!(
            resolve_first_seen_nanos(&(13, "IDX_I".to_string()), &prev, today),
            1_699_920_000_000_000_000,
            "existing first_seen must be preserved, NOT reset to today"
        );
    }

    #[test]
    fn test_resolve_first_seen_new_row_uses_today() {
        let prev = HashMap::new();
        let today = 1_700_500_000_000_000_000;
        assert_eq!(
            resolve_first_seen_nanos(&(999, "NSE_EQ".to_string()), &prev, today),
            today,
            "a brand-new instrument's first_seen is today"
        );
    }

    #[test]
    fn test_resolve_first_seen_prior_zero_uses_today() {
        // A prior row whose first_seen was never set (0) falls back to
        // today rather than persisting a meaningless 0.
        let prev = prev_with_first_seen(0);
        let today = 1_700_500_000_000_000_000;
        assert_eq!(
            resolve_first_seen_nanos(&(13, "IDX_I".to_string()), &prev, today),
            today
        );
    }

    #[test]
    fn test_is_upsert_transition_routing() {
        assert!(
            is_upsert_transition(LifecycleState::Active),
            "present → UPSERT"
        );
        assert!(
            !is_upsert_transition(LifecycleState::ExpiredFromFno),
            "expiry → UPDATE"
        );
        assert!(!is_upsert_transition(LifecycleState::ExpiredContract));
        assert!(!is_upsert_transition(LifecycleState::ExpiredIndex));
        assert!(
            !is_upsert_transition(LifecycleState::Delisted),
            "delisted → UPDATE (not a present/active write)"
        );
    }
}
