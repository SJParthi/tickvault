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
//!
//! ## Option chain underlyings (3 of the 4)
//!
//! `INDIA VIX` (security_id = 21) has NO option chain — it's a
//! volatility-index value, not an optionable underlying. The
//! `OPTION_CHAIN_UNDERLYINGS` slice drops VIX for the option-chain REST
//! loop while keeping it in `LOCKED_UNIVERSE` for live tick subscription.

use crate::types::ExchangeSegment;

/// The 4 IDX_I instruments tickvault subscribes to via the main-feed
/// WebSocket. Each tuple is `(security_id, display_name, exchange_segment)`.
///
/// Security IDs verified from Dhan's instrument master and pinned per
/// `.claude/rules/project/live-market-feed-subscription.md` Wave 5
/// Update and `.claude/rules/project/websocket-connection-scope-lock.md`.
///
/// `INDIA VIX` (21) is included here for live tick subscription but
/// excluded from `OPTION_CHAIN_UNDERLYINGS` below (VIX has no options).
pub const LOCKED_UNIVERSE: &[(u32, &str, ExchangeSegment)] = &[
    (13, "NIFTY", ExchangeSegment::IdxI),
    (25, "BANKNIFTY", ExchangeSegment::IdxI),
    (51, "SENSEX", ExchangeSegment::IdxI),
    (21, "INDIA VIX", ExchangeSegment::IdxI),
];

/// Option-chain underlyings for the 50-second REST poll loop.
/// 3 of the 4 LOCKED_UNIVERSE entries — INDIA VIX excluded (no options).
///
/// Per `docs/architecture/option-chain-z-plus-heart-piece.md` §2 + §3.
pub const OPTION_CHAIN_UNDERLYINGS: &[(u32, &str)] =
    &[(13, "NIFTY"), (25, "BANKNIFTY"), (51, "SENSEX")];

/// Operator-locked sequential fetch order for the option-chain minute
/// scheduler (2026-05-25). At every `:50` of each market-hours minute,
/// the scheduler fires fetches in THIS order — SENSEX first
/// (slowest-to-update per operator's profiling), then BANKNIFTY, then
/// NIFTY. Sequential (not parallel) to stay inside Dhan's
/// 1-request-per-3-seconds-per-underlying rate limit; total burst
/// completes in ~18s, finishing before the next minute boundary.
///
/// Re-ordering this slice requires a rule-file edit per operator
/// directive and an explicit ratchet test update. The order is
/// pinned by `tests/option_chain_warmup_wiring.rs`.
pub const OPTION_CHAIN_FETCH_SEQUENCE: &[(u32, &str)] =
    &[(51, "SENSEX"), (25, "BANKNIFTY"), (13, "NIFTY")];

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

/// True iff the given security_id has an option chain (= in
/// `OPTION_CHAIN_UNDERLYINGS`). False for INDIA VIX (no options) AND
/// for any SID not in `LOCKED_UNIVERSE`.
#[must_use]
pub const fn has_option_chain(security_id: u32) -> bool {
    let mut i = 0;
    while i < OPTION_CHAIN_UNDERLYINGS.len() {
        let (sid, _name) = OPTION_CHAIN_UNDERLYINGS[i];
        if sid == security_id {
            return true;
        }
        i += 1;
    }
    false
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
    fn test_option_chain_underlyings_has_exactly_three_entries() {
        assert_eq!(OPTION_CHAIN_UNDERLYINGS.len(), 3);
    }

    #[test]
    fn test_option_chain_underlyings_excludes_india_vix() {
        let sids: Vec<u32> = OPTION_CHAIN_UNDERLYINGS.iter().map(|(s, _)| *s).collect();
        assert!(
            !sids.contains(&21),
            "INDIA VIX (21) has no option chain — must NOT be in OPTION_CHAIN_UNDERLYINGS"
        );
        assert!(sids.contains(&13), "NIFTY (13) has options");
        assert!(sids.contains(&25), "BANKNIFTY (25) has options");
        assert!(sids.contains(&51), "SENSEX (51) has options");
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
    fn test_has_option_chain_true_for_three_underlyings() {
        assert!(has_option_chain(13));
        assert!(has_option_chain(25));
        assert!(has_option_chain(51));
    }

    #[test]
    fn test_has_option_chain_false_for_india_vix() {
        assert!(
            !has_option_chain(21),
            "INDIA VIX has no option chain — operator-locked"
        );
    }

    #[test]
    fn test_has_option_chain_false_for_unknown() {
        assert!(!has_option_chain(999));
        assert!(!has_option_chain(0));
    }

    #[test]
    fn test_const_fns_usable_in_const_context() {
        // Compile-time evaluation — proves these are genuinely const fn.
        const NIFTY_NAME: Option<&str> = name_for_security_id(13);
        const VIX_HAS_OC: bool = has_option_chain(21);
        const VIX_IN_UNIVERSE: bool = is_locked_universe_sid(21);

        assert_eq!(NIFTY_NAME, Some("NIFTY"));
        assert!(!VIX_HAS_OC);
        assert!(VIX_IN_UNIVERSE);
    }

    #[test]
    fn test_option_chain_underlyings_is_subset_of_locked_universe() {
        // Every option-chain underlying must also be in LOCKED_UNIVERSE.
        for (oc_sid, _) in OPTION_CHAIN_UNDERLYINGS {
            assert!(
                is_locked_universe_sid(*oc_sid),
                "option-chain SID {oc_sid} must also be in LOCKED_UNIVERSE"
            );
        }
    }
}
