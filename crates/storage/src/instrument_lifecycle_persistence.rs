//! `instrument_lifecycle` + `instrument_lifecycle_audit` table contracts —
//! Sub-PR of the 2026-05-27 daily-universe expansion (§5 / §6 / §23–§25 / §27).
//!
//! **Status:** CONTRACT ONLY. This module ships the stable identifier
//! surface both tables need:
//!
//! * [`QUESTDB_TABLE_INSTRUMENT_LIFECYCLE`] / [`QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT`]
//! * [`DEDUP_KEY_INSTRUMENT_LIFECYCLE`] / [`DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT`]
//! * [`LifecycleState`] — the `lifecycle_state` SYMBOL labels (§5)
//! * [`TransitionKind`] — the audit `transition_kind` SYMBOL labels (§6 + §23)
//! * [`lifecycle_designated_ts_nanos`] — the pinned constant designated
//!   timestamp (see "Designated timestamp" below)
//!
//! The DDL strings, `ensure_*_table` helpers, `*Row` structs, and
//! `append_*_row` helpers land in follow-up sub-PRs (mirroring how
//! `instrument_fetch_audit` shipped its contract in #835 and its
//! persistence helpers in #836). The daily lifecycle reconciler
//! (idempotent UPSERT + state-flip + §24 write-audit-first ordering)
//! is a further follow-up.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build until the boot orchestrator (Sub-PR #10)
//! flips the flag.
//!
//! # Why two tables
//!
//! * `instrument_lifecycle` — the **current** state, ONE row per
//!   instrument EVER observed, UPSERT-updated in place. NEVER deleted
//!   (operator quote §0: "instead of deleting it … marked as expired").
//! * `instrument_lifecycle_audit` — the **forensic chain**, one row per
//!   state transition (appeared / updated / expired / reactivated /
//!   split / segment-moved / …). 5-year SEBI retention. Powers the
//!   §25 point-in-time "what was the universe on date X?" reconstruction.
//!
//! # Designated timestamp (the I-P1-08 resolution)
//!
//! QuestDB requires the designated timestamp column to appear in every
//! `DEDUP UPSERT KEYS(...)` clause (omitting it returns HTTP 400 at
//! boot — the 2026-04-28 regression class). But `instrument_lifecycle`
//! wants ONE row per `(security_id, exchange_segment)` that is UPDATED
//! in place as the instrument's state changes — a *mutable* last-update
//! time in the DEDUP key would make every update a NEW row.
//!
//! Resolution (same as the retired `instrument_persistence` I-P1-08
//! design): the designated `ts` column is pinned to a CONSTANT
//! ([`lifecycle_designated_ts_nanos`] = epoch 0) so the DEDUP fires on
//! the business key `(security_id, exchange_segment)` alone, while the
//! mutable wall-clock last-update time lives in a separate
//! `last_update_ts` column (added by the DDL follow-up). The DEDUP key
//! constant therefore lists `ts` first to satisfy QuestDB, then the
//! I-P1-11 composite business key.
//!
//! The `*_audit` table is append-only (not UPSERT-in-place), so its `ts`
//! is the real per-transition wall-clock; its DEDUP key additionally
//! carries `transition_kind` so two different transitions for the same
//! instrument on the same day both survive (§6).
//!
//! ## ⚠ DDL follow-up MUST read this
//!
//! Because `instrument_lifecycle`'s designated `ts` is pinned to epoch 0,
//! the table's DDL **MUST use `PARTITION BY NONE`** (or partition on a
//! separate non-designated date column) — NOT `PARTITION BY DAY/MONTH`.
//! A date-partitioned table with a constant `ts` would funnel every
//! never-deleted lifecycle row into a single `1970-01-01` partition,
//! defeating QuestDB partition pruning AND breaking the `partition_manager`
//! S3-archive-by-date lifecycle (it detaches partitions by date; a
//! one-partition table can never detach). The append-only
//! `instrument_lifecycle_audit` table keeps a REAL per-transition `ts`,
//! so it partitions by day normally.
//!
//! # Cross-references
//!
//! * Rule §5 / §6 / §23 / §24 / §25 / §27 — `daily-universe-scope-expansion-2026-05-27.md`
//! * `security-id-uniqueness.md` (I-P1-11) — composite `(security_id, exchange_segment)`
//! * `gap-enforcement.md` (I-P1-08) — constant designated timestamp for UPSERT-one-row tables

#![cfg(feature = "daily_universe_fetcher")]

/// Wire-format table name for the current-state lifecycle table.
/// Stable across releases — operators, the reconciler, and the
/// `partition_manager` S3 archive job depend on the exact string.
pub const QUESTDB_TABLE_INSTRUMENT_LIFECYCLE: &str = "instrument_lifecycle";

/// Wire-format table name for the forensic transition-chain table.
pub const QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT: &str = "instrument_lifecycle_audit";

/// DEDUP key for `instrument_lifecycle` — UPSERT one row per instrument.
///
/// `ts` (pinned constant — see module docs) satisfies QuestDB's
/// designated-timestamp-in-DEDUP requirement; `security_id` +
/// `exchange_segment` are the I-P1-11 composite business key (Dhan
/// reuses `security_id` across segments, so segment is mandatory).
pub const DEDUP_KEY_INSTRUMENT_LIFECYCLE: &str = "ts, security_id, exchange_segment";

/// DEDUP key for `instrument_lifecycle_audit` — append one row per
/// transition.
///
/// `ts` is the real per-transition wall-clock designated timestamp.
/// `trading_date_ist` is the partition/business date. `security_id` +
/// `exchange_segment` is the I-P1-11 composite key. `transition_kind`
/// ensures two distinct transitions for the same instrument on the same
/// day (e.g. `updated` then `expired`) BOTH survive rather than the
/// terminal one overwriting the trigger one (§6).
// Kept on one line so the `dedup_segment_meta_guard.rs` source scanner
// (which matches `const … : &str = "…"` on a single line) detects it and
// verifies both the I-P1-11 segment-pairing and the designated-`ts` token.
#[rustfmt::skip]
pub const DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT: &str = "ts, trading_date_ist, security_id, exchange_segment, transition_kind";

/// The pinned constant designated timestamp (epoch 0) for the
/// UPSERT-in-place `instrument_lifecycle` table. See module docs
/// ("Designated timestamp") for the I-P1-08 rationale. The DDL
/// follow-up stamps every lifecycle row's designated `ts` with this
/// value so the DEDUP fires on the business key alone.
#[must_use]
pub const fn lifecycle_designated_ts_nanos() -> i64 {
    0
}

/// Stable wire-format strings for the `instrument_lifecycle.lifecycle_state`
/// SYMBOL column (§5). Operators query this column by exact string match;
/// bumping any label is a breaking change.
///
/// The reconciler (follow-up) flips `Active` → one of the `Expired*`
/// states when an instrument disappears from the daily CSV, and back to
/// `Active` on reappearance. `Delisted` is operator-set (manual). Rows
/// are NEVER deleted (§0 operator lock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    /// Present in today's CSV and tradable.
    Active,
    /// Stock dropped out of the F&O list (its derivatives expired off it).
    ExpiredFromFno,
    /// A specific derivative contract reached its expiry date.
    ExpiredContract,
    /// An index that is no longer published.
    ExpiredIndex,
    /// Operator-set terminal state (manual override) — instrument removed
    /// from the exchange entirely.
    Delisted,
}

impl LifecycleState {
    /// Returns the wire-format SYMBOL label. ALWAYS lowercase snake_case.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ExpiredFromFno => "expired_from_fno",
            Self::ExpiredContract => "expired_contract",
            Self::ExpiredIndex => "expired_index",
            Self::Delisted => "delisted",
        }
    }

    /// True only for `Active`. Dashboards + the subscription dispatcher
    /// MUST filter `WHERE lifecycle_state = 'active'`.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// True for any of the three `Expired*` states (NOT `Delisted`,
    /// which is a manual terminal state, and NOT `Active`).
    #[must_use]
    pub const fn is_expired(self) -> bool {
        matches!(
            self,
            Self::ExpiredFromFno | Self::ExpiredContract | Self::ExpiredIndex
        )
    }

    /// All variants, for exhaustive ratchet tests.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Active,
            Self::ExpiredFromFno,
            Self::ExpiredContract,
            Self::ExpiredIndex,
            Self::Delisted,
        ]
    }
}

/// Stable wire-format strings for the
/// `instrument_lifecycle_audit.transition_kind` SYMBOL column (§6 + §23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionKind {
    /// First time this `(security_id, exchange_segment)` was ever seen.
    Appeared,
    /// A mutable field (lot_size, tick_size, symbol, …) changed.
    Updated,
    /// `lifecycle_state` flipped to one of the `Expired*` states.
    Expired,
    /// An expired instrument reappeared in the CSV and flipped back to
    /// `Active`.
    Reactivated,
    /// Operator manually set `Delisted`.
    DelistedManual,
    /// Operator set `lifecycle_state_locked` (§5 override).
    Locked,
    /// Stock split detected — lot/tick size halved or less (§23).
    Split,
    /// Same `security_id` moved to a different `exchange_segment` (§23).
    SegmentMoved,
}

impl TransitionKind {
    /// Returns the wire-format SYMBOL label. ALWAYS lowercase snake_case.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Appeared => "appeared",
            Self::Updated => "updated",
            Self::Expired => "expired",
            Self::Reactivated => "reactivated",
            Self::DelistedManual => "delisted_manual",
            Self::Locked => "locked",
            Self::Split => "split",
            Self::SegmentMoved => "segment_moved",
        }
    }

    /// All variants, for exhaustive ratchet tests.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::Appeared,
            Self::Updated,
            Self::Expired,
            Self::Reactivated,
            Self::DelistedManual,
            Self::Locked,
            Self::Split,
            Self::SegmentMoved,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_name_constants() {
        assert_eq!(QUESTDB_TABLE_INSTRUMENT_LIFECYCLE, "instrument_lifecycle");
        assert_eq!(
            QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT,
            "instrument_lifecycle_audit"
        );
    }

    #[test]
    fn test_lifecycle_dedup_key_includes_ts_and_segment() {
        // QuestDB designated-timestamp requirement + I-P1-11 segment.
        let has_ts = DEDUP_KEY_INSTRUMENT_LIFECYCLE
            .split([',', ' '])
            .map(str::trim)
            .any(|t| t == "ts");
        assert!(
            has_ts,
            "lifecycle DEDUP key must include the bare `ts` token"
        );
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE.contains("security_id"));
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE.contains("exchange_segment"),
            "I-P1-11: security_id must be paired with exchange_segment"
        );
        assert_eq!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE.matches(',').count() + 1,
            3,
            "lifecycle DEDUP key has exactly 3 columns"
        );
    }

    #[test]
    fn test_lifecycle_audit_dedup_key_includes_ts_segment_and_transition_kind() {
        let has_ts = DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT
            .split([',', ' '])
            .map(str::trim)
            .any(|t| t == "ts");
        assert!(has_ts, "audit DEDUP key must include the bare `ts` token");
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("trading_date_ist"));
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("exchange_segment"));
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("transition_kind"),
            "without transition_kind two same-day transitions for one \
             instrument would collapse to a single row (§6)"
        );
        assert_eq!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.matches(',').count() + 1,
            5,
            "audit DEDUP key has exactly 5 columns"
        );
    }

    #[test]
    fn test_lifecycle_designated_ts_is_constant_zero() {
        // I-P1-08: pinned constant so business-key DEDUP works for the
        // UPSERT-in-place lifecycle table.
        assert_eq!(lifecycle_designated_ts_nanos(), 0);
        assert_eq!(
            lifecycle_designated_ts_nanos(),
            lifecycle_designated_ts_nanos(),
            "must be constant across calls"
        );
    }

    #[test]
    fn test_lifecycle_state_has_five_distinct_lowercase_labels() {
        let variants = LifecycleState::all();
        assert_eq!(variants.len(), 5);
        let mut seen = Vec::new();
        for v in variants {
            let label = v.as_str();
            assert!(!seen.contains(&label), "duplicate label {label}");
            for ch in label.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_',
                    "non-snake_case char `{ch}` in `{label}`"
                );
            }
            seen.push(label);
        }
    }

    #[test]
    fn test_lifecycle_state_is_active_only_for_active() {
        assert!(LifecycleState::Active.is_active());
        for v in LifecycleState::all() {
            if v != LifecycleState::Active {
                assert!(!v.is_active(), "{v:?} must not be active");
            }
        }
        let active_count = LifecycleState::all()
            .iter()
            .filter(|v| v.is_active())
            .count();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn test_lifecycle_state_is_expired_covers_three_expired_states_only() {
        assert!(LifecycleState::ExpiredFromFno.is_expired());
        assert!(LifecycleState::ExpiredContract.is_expired());
        assert!(LifecycleState::ExpiredIndex.is_expired());
        assert!(!LifecycleState::Active.is_expired());
        assert!(
            !LifecycleState::Delisted.is_expired(),
            "Delisted is a manual terminal state, distinct from Expired*"
        );
        let expired_count = LifecycleState::all()
            .iter()
            .filter(|v| v.is_expired())
            .count();
        assert_eq!(expired_count, 3);
    }

    #[test]
    fn test_lifecycle_state_active_and_expired_are_disjoint() {
        for v in LifecycleState::all() {
            assert!(
                !(v.is_active() && v.is_expired()),
                "{v:?} cannot be both active and expired"
            );
        }
    }

    #[test]
    fn test_transition_kind_has_eight_distinct_lowercase_labels() {
        let variants = TransitionKind::all();
        assert_eq!(variants.len(), 8);
        let mut seen = Vec::new();
        for v in variants {
            let label = v.as_str();
            assert!(!seen.contains(&label), "duplicate label {label}");
            assert!(!label.is_empty());
            for ch in label.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_',
                    "non-snake_case char `{ch}` in `{label}`"
                );
            }
            seen.push(label);
        }
    }

    #[test]
    fn test_transition_kind_wire_labels_are_stable() {
        // Pin the exact strings — operators + the §25 point-in-time
        // query depend on them.
        assert_eq!(TransitionKind::Appeared.as_str(), "appeared");
        assert_eq!(TransitionKind::Updated.as_str(), "updated");
        assert_eq!(TransitionKind::Expired.as_str(), "expired");
        assert_eq!(TransitionKind::Reactivated.as_str(), "reactivated");
        assert_eq!(TransitionKind::DelistedManual.as_str(), "delisted_manual");
        assert_eq!(TransitionKind::Locked.as_str(), "locked");
        assert_eq!(TransitionKind::Split.as_str(), "split");
        assert_eq!(TransitionKind::SegmentMoved.as_str(), "segment_moved");
    }

    #[test]
    fn test_lifecycle_state_wire_labels_are_stable() {
        assert_eq!(LifecycleState::Active.as_str(), "active");
        assert_eq!(LifecycleState::ExpiredFromFno.as_str(), "expired_from_fno");
        assert_eq!(LifecycleState::ExpiredContract.as_str(), "expired_contract");
        assert_eq!(LifecycleState::ExpiredIndex.as_str(), "expired_index");
        assert_eq!(LifecycleState::Delisted.as_str(), "delisted");
    }

    #[test]
    fn test_enums_derive_eq_and_hash() {
        use std::collections::HashSet;
        let mut s: HashSet<LifecycleState> = HashSet::new();
        for v in LifecycleState::all() {
            assert!(s.insert(v));
        }
        let mut t: HashSet<TransitionKind> = HashSet::new();
        for v in TransitionKind::all() {
            assert!(t.insert(v));
        }
    }
}
