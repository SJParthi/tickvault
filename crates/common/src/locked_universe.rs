//! LOCKED universe of 4 IDX_I SIDs — the indices-only contract for tickvault.
//!
//! ## Why this exists
//!
//! Operator-locked 2026-05-18: tickvault subscribes to and trades EXACTLY
//! 4 instruments — NIFTY (13), BANKNIFTY (25), SENSEX (51), INDIA VIX (21).
//! All four are `IDX_I` exchange-segment (index value). The original
//! universe builder (`crates/core/src/instrument/universe_builder.rs`,
//! ~12K LoC across bhavcopy + CSV + delta detector + binary cache + S3
//! backup) was designed to ingest Dhan's full instrument master CSV
//! (~25,000 rows of stocks + F&O contracts + indices) and produce a
//! `FnoUniverse` runtime object.
//!
//! For the indices-only scope none of that machinery is needed — the
//! universe is 4 hard-coded entries. This module replaces the entire
//! ~12K LoC universe-builder subsystem with the const slice below.
//!
//! ## The 14-PR sequence (where this fits)
//!
//! - **PR #6-prep (THIS FILE)**: cement the `LOCKED_UNIVERSE` const without
//!   touching the existing `universe_builder.rs` / `bhavcopy_*.rs` /
//!   `csv_*.rs` consumers. Pure additive — same pattern as PR #1
//!   (ErrorCode stubs) and PR #2.5 (DayOhlcTracker).
//! - **PR #2**: delete movers pipeline.
//! - **PR #3**: delete greeks inline pipeline.
//! - **PR #4**: delete depth-20 / depth-200 dynamic pipelines.
//! - **PR #5**: delete Phase 2 dispatcher.
//! - **PR #6-real**: migrate remaining consumers of `FnoUniverse` to
//!   read `LOCKED_UNIVERSE` instead, then delete `universe_builder.rs` +
//!   `bhavcopy_*.rs` + CSV / S3 / delta-detector / binary-cache machinery.
//! - **PR #7**: tighten `SubscriptionScope` to `Indices4Only` variant.
//!
//! Until PR #6-real ships, the existing universe builder remains the
//! runtime source of truth. This module is a CONTRACT stub that future
//! PRs anchor against.
//!
//! ## Composite key per I-P1-11
//!
//! Each entry is keyed by `(security_id, exchange_segment)` — composite
//! per `.claude/rules/project/security-id-uniqueness.md`. At the 4-SID
//! scope the composite is degenerate (all four are `IdxI`), but the
//! invariant holds for future scope extension.

use crate::types::ExchangeSegment;

/// The 4 IDX_I instruments tickvault subscribes to via the main-feed
/// WebSocket. Each tuple is `(security_id, display_name, exchange_segment)`.
///
/// Security IDs verified from Dhan's instrument master and pinned per
/// `.claude/rules/project/live-market-feed-subscription.md` Wave 5
/// Update and `.claude/rules/project/websocket-connection-scope-lock.md`.
///
/// `INDIA VIX` (21) is included here for live tick subscription.
pub const LOCKED_UNIVERSE: &[(u32, &str, ExchangeSegment)] = &[
    (13, "NIFTY", ExchangeSegment::IdxI),
    (25, "BANKNIFTY", ExchangeSegment::IdxI),
    (51, "SENSEX", ExchangeSegment::IdxI),
    (21, "INDIA VIX", ExchangeSegment::IdxI),
];

// 2026-06-28: OPTION_CHAIN_UNDERLYINGS, OPTION_CHAIN_FETCH_SEQUENCE, and
// has_option_chain() were REMOVED with the option_chain REST subsystem
// (operator directive 2026-06-28). They served only the deleted REST poll
// loop / expiry-warmup task — not live tick subscription.

/// Convenience: lookup an underlying name by security_id in the locked
/// universe. Returns `None` for any SID not in the 4-entry slice.
///
/// O(1) bounded — at most 4 comparisons. Const-fn compatible for use
/// in compile-time contexts.
#[must_use]
pub const fn name_for_security_id(security_id: u32) -> Option<&'static str> {
    let mut i = 0;
    while i < LOCKED_UNIVERSE.len() {
        let (sid, name, _seg) = LOCKED_UNIVERSE[i];
        if sid == security_id {
            return Some(name);
        }
        i += 1;
    }
    None
}

/// Convenience: lookup the exchange segment for a security_id in the
/// locked universe. Returns `None` for any SID not in the 4-entry slice.
#[must_use]
pub const fn segment_for_security_id(security_id: u32) -> Option<ExchangeSegment> {
    let mut i = 0;
    while i < LOCKED_UNIVERSE.len() {
        let (sid, _name, seg) = LOCKED_UNIVERSE[i];
        if sid == security_id {
            return Some(seg);
        }
        i += 1;
    }
    None
}

/// True iff the given security_id is in the LOCKED_UNIVERSE.
#[must_use]
pub const fn is_locked_universe_sid(security_id: u32) -> bool {
    name_for_security_id(security_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_locked_universe_has_exactly_four_entries() {
        assert_eq!(LOCKED_UNIVERSE.len(), 4);
    }

    #[test]
    fn test_locked_universe_contains_nifty_banknifty_sensex_vix() {
        let sids: Vec<u32> = LOCKED_UNIVERSE.iter().map(|(s, _, _)| *s).collect();
        assert!(sids.contains(&13), "NIFTY = 13 must be in LOCKED_UNIVERSE");
        assert!(
            sids.contains(&25),
            "BANKNIFTY = 25 must be in LOCKED_UNIVERSE"
        );
        assert!(sids.contains(&51), "SENSEX = 51 must be in LOCKED_UNIVERSE");
        assert!(
            sids.contains(&21),
            "INDIA VIX = 21 must be in LOCKED_UNIVERSE"
        );
    }

    #[test]
    fn test_all_locked_universe_entries_are_idx_i() {
        for (sid, name, seg) in LOCKED_UNIVERSE {
            assert_eq!(
                *seg,
                ExchangeSegment::IdxI,
                "{name} (sid={sid}) must be IDX_I — operator-charter §I"
            );
        }
    }

    #[test]
    fn test_name_for_security_id_returns_correct_name_for_each_sid() {
        assert_eq!(name_for_security_id(13), Some("NIFTY"));
        assert_eq!(name_for_security_id(25), Some("BANKNIFTY"));
        assert_eq!(name_for_security_id(51), Some("SENSEX"));
        assert_eq!(name_for_security_id(21), Some("INDIA VIX"));
    }

    #[test]
    fn test_name_for_security_id_returns_none_for_unknown_sid() {
        assert_eq!(name_for_security_id(999), None);
        assert_eq!(name_for_security_id(0), None);
        // SID 27 was FINNIFTY in the old universe — dropped per
        // operator-charter §I 2026-05-18.
        assert_eq!(name_for_security_id(27), None);
    }

    #[test]
    fn test_segment_for_security_id_returns_idx_i_for_locked_entries() {
        assert_eq!(segment_for_security_id(13), Some(ExchangeSegment::IdxI));
        assert_eq!(segment_for_security_id(25), Some(ExchangeSegment::IdxI));
        assert_eq!(segment_for_security_id(51), Some(ExchangeSegment::IdxI));
        assert_eq!(segment_for_security_id(21), Some(ExchangeSegment::IdxI));
    }

    #[test]
    fn test_segment_for_security_id_returns_none_for_unknown() {
        assert_eq!(segment_for_security_id(999), None);
    }

    #[test]
    fn test_is_locked_universe_sid_true_for_four_known_sids() {
        assert!(is_locked_universe_sid(13));
        assert!(is_locked_universe_sid(25));
        assert!(is_locked_universe_sid(51));
        assert!(is_locked_universe_sid(21));
    }

    #[test]
    fn test_is_locked_universe_sid_false_for_unknown() {
        assert!(!is_locked_universe_sid(999));
        assert!(!is_locked_universe_sid(0));
        assert!(!is_locked_universe_sid(27)); // dropped FINNIFTY
    }

    #[test]
    fn test_const_fns_usable_in_const_context() {
        // Compile-time evaluation — proves these are genuinely const fn.
        const NIFTY_NAME: Option<&str> = name_for_security_id(13);
        const VIX_IN_UNIVERSE: bool = is_locked_universe_sid(21);

        assert_eq!(NIFTY_NAME, Some("NIFTY"));
        assert!(VIX_IN_UNIVERSE);
    }
}
