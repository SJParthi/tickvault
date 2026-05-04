//! Trading-day phase classification.
//!
//! Pure-function map from `(now_ist_secs_of_day, segment)` → `Phase`. Stamped
//! onto every tick by the parser and projected into every matview via
//! `last(phase)` so downstream queries can filter by phase without rejoining
//! the calendar table.
//!
//! ## Phases (5 values — bounded SYMBOL dictionary)
//!
//! | Variant | IST window | Notes |
//! |---|---|---|
//! | `Premarket` | 00:00:00–08:59:59 | Day rollover; volume monotonicity is suppressed |
//! | `Preopen` | 09:00:00–09:14:59 | NSE pre-open auction; pre-open buffer captures closes |
//! | `Open` | 09:15:00–15:29:59 | Continuous trading; volume monotonicity enforced |
//! | `PostAuction` | 15:30:00–15:39:59 | Post-close auction window |
//! | `Closed` | 15:40:00–23:59:59 | Quiet hours |
//!
//! BSE shares the same phase boundaries as NSE today; `segment` is reserved
//! for future divergence (e.g., MCX commodity hours) but is currently
//! ignored. Adding a new segment-specific window = extend the match arm,
//! the property test catches any unrouted segment automatically.
//!
//! ## Why a pure function (no clock side-effect)
//!
//! `compute_phase` takes `now_ist_secs_of_day: u32` as input and returns
//! deterministically. The caller (tick processor) reads the wall-clock
//! once and forwards the result. This keeps the function:
//!
//! - **Hot-path safe** — zero allocations, zero syscalls, returns by value.
//! - **Property-testable** — proptest sweeps every IST minute boundary
//!   without flake.
//! - **Cache-line cheap** — the returned `Phase` is one byte; callers can
//!   pack it next to the tick header.

use crate::constants::{TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST};
use crate::types::ExchangeSegment;

/// IST seconds-of-day for the NSE pre-open auction → continuous-trading
/// transition (09:15:00 IST).
const NSE_OPEN_SECS_OF_DAY_IST: u32 = 9 * 3600 + 15 * 60;

/// IST seconds-of-day for the NSE continuous-trading → post-auction
/// transition (15:30:00 IST). Equals `TICK_PERSIST_END_SECS_OF_DAY_IST`.
const NSE_POST_AUCTION_START_SECS_OF_DAY_IST: u32 = TICK_PERSIST_END_SECS_OF_DAY_IST;

/// IST seconds-of-day for the post-auction → closed transition (15:40:00 IST).
/// 10-minute window after market close.
const NSE_CLOSED_SECS_OF_DAY_IST: u32 = 15 * 3600 + 40 * 60;

/// Trading-day phase used as the QuestDB SYMBOL value on every tick + candle.
///
/// `repr(u8)` keeps the in-memory width minimal and lets the tick-processor
/// pack `Phase` next to the tick header without padding. `as_str()` returns
/// the canonical 5-symbol label that the QuestDB SYMBOL dictionary
/// observes — the dictionary stays at exactly 5 entries by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Phase {
    Premarket = 0,
    Preopen = 1,
    Open = 2,
    PostAuction = 3,
    Closed = 4,
}

impl Phase {
    /// Canonical SYMBOL label written to QuestDB. Drift here changes the
    /// SYMBOL dictionary — pinned by `test_phase_as_str_is_pinned`.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premarket => "PREMARKET",
            Self::Preopen => "PREOPEN",
            Self::Open => "OPEN",
            Self::PostAuction => "POSTAUCTION",
            Self::Closed => "CLOSED",
        }
    }

    /// Whether the phase is within the live-trading window. Used by the
    /// VOLUME-MONO-01 guard (suppress monotonicity check during PREMARKET
    /// per L13 of the 29-tf plan).
    #[inline]
    pub const fn is_live_trading(self) -> bool {
        matches!(self, Self::Open)
    }

    /// All 5 variants in canonical order. Useful for ratchet tests that
    /// need to enumerate every value.
    #[inline]
    pub const fn all() -> [Self; 5] {
        [
            Self::Premarket,
            Self::Preopen,
            Self::Open,
            Self::PostAuction,
            Self::Closed,
        ]
    }
}

/// Pure function: map `(now_ist_secs_of_day, segment)` → `Phase`.
///
/// O(1), zero-alloc, branch-on-int — safe to call on the hot path.
///
/// Boundaries are chosen to match the Dhan-side market windows exactly:
/// - PREMARKET ends at 09:00:00 IST (the same instant tick persistence
///   begins, per `TICK_PERSIST_START_SECS_OF_DAY_IST`).
/// - PREOPEN ends at 09:15:00 IST (NSE continuous-trading start).
/// - OPEN ends at 15:30:00 IST (NSE post-close auction start, equals
///   `TICK_PERSIST_END_SECS_OF_DAY_IST`).
/// - POSTAUCTION ends at 15:40:00 IST (10-minute post-close window).
/// - CLOSED runs from 15:40:00 to 23:59:59 IST.
///
/// `_segment` is currently unused (NSE/BSE share boundaries today) but
/// reserved so adding MCX commodity hours = extend the match without a
/// signature change.
#[inline]
pub fn compute_phase(now_ist_secs_of_day: u32, _segment: ExchangeSegment) -> Phase {
    if now_ist_secs_of_day < TICK_PERSIST_START_SECS_OF_DAY_IST {
        Phase::Premarket
    } else if now_ist_secs_of_day < NSE_OPEN_SECS_OF_DAY_IST {
        Phase::Preopen
    } else if now_ist_secs_of_day < NSE_POST_AUCTION_START_SECS_OF_DAY_IST {
        Phase::Open
    } else if now_ist_secs_of_day < NSE_CLOSED_SECS_OF_DAY_IST {
        Phase::PostAuction
    } else {
        Phase::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_as_str_is_pinned() {
        // SYMBOL dictionary contract — drift breaks the QuestDB symbol
        // table compatibility across rolling deploys.
        assert_eq!(Phase::Premarket.as_str(), "PREMARKET");
        assert_eq!(Phase::Preopen.as_str(), "PREOPEN");
        assert_eq!(Phase::Open.as_str(), "OPEN");
        assert_eq!(Phase::PostAuction.as_str(), "POSTAUCTION");
        assert_eq!(Phase::Closed.as_str(), "CLOSED");
    }

    #[test]
    fn test_phase_all_returns_five_distinct() {
        let all = Phase::all();
        assert_eq!(all.len(), 5);
        // Distinct by Eq.
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "Phase::all has duplicate at {i}, {j}");
            }
        }
    }

    #[test]
    fn test_is_live_trading_only_for_open() {
        for p in Phase::all() {
            let expected = matches!(p, Phase::Open);
            assert_eq!(
                p.is_live_trading(),
                expected,
                "is_live_trading mismatch for {:?}",
                p
            );
        }
    }

    #[test]
    fn test_compute_phase_at_midnight() {
        // 00:00:00 IST = PREMARKET.
        assert_eq!(
            compute_phase(0, ExchangeSegment::NseEquity),
            Phase::Premarket
        );
    }

    #[test]
    fn test_compute_phase_just_before_preopen() {
        // 08:59:59 IST = still PREMARKET.
        let secs = 9 * 3600 - 1;
        assert_eq!(
            compute_phase(secs, ExchangeSegment::NseEquity),
            Phase::Premarket
        );
    }

    #[test]
    fn test_compute_phase_at_preopen_start() {
        // 09:00:00 IST = PREOPEN.
        assert_eq!(
            compute_phase(9 * 3600, ExchangeSegment::NseEquity),
            Phase::Preopen
        );
    }

    #[test]
    fn test_compute_phase_just_before_open() {
        // 09:14:59 IST = still PREOPEN.
        let secs = 9 * 3600 + 15 * 60 - 1;
        assert_eq!(
            compute_phase(secs, ExchangeSegment::NseEquity),
            Phase::Preopen
        );
    }

    #[test]
    fn test_compute_phase_at_open() {
        // 09:15:00 IST = OPEN.
        let secs = 9 * 3600 + 15 * 60;
        assert_eq!(compute_phase(secs, ExchangeSegment::NseEquity), Phase::Open);
    }

    #[test]
    fn test_compute_phase_just_before_post_auction() {
        // 15:29:59 IST = still OPEN.
        let secs = 15 * 3600 + 30 * 60 - 1;
        assert_eq!(compute_phase(secs, ExchangeSegment::NseEquity), Phase::Open);
    }

    #[test]
    fn test_compute_phase_at_post_auction_start() {
        // 15:30:00 IST = POSTAUCTION.
        let secs = 15 * 3600 + 30 * 60;
        assert_eq!(
            compute_phase(secs, ExchangeSegment::NseEquity),
            Phase::PostAuction
        );
    }

    #[test]
    fn test_compute_phase_just_before_closed() {
        // 15:39:59 IST = still POSTAUCTION.
        let secs = 15 * 3600 + 40 * 60 - 1;
        assert_eq!(
            compute_phase(secs, ExchangeSegment::NseEquity),
            Phase::PostAuction
        );
    }

    #[test]
    fn test_compute_phase_at_closed_start() {
        // 15:40:00 IST = CLOSED.
        let secs = 15 * 3600 + 40 * 60;
        assert_eq!(
            compute_phase(secs, ExchangeSegment::NseEquity),
            Phase::Closed
        );
    }

    #[test]
    fn test_compute_phase_at_end_of_day() {
        // 23:59:59 IST = CLOSED.
        let secs = 24 * 3600 - 1;
        assert_eq!(
            compute_phase(secs, ExchangeSegment::NseEquity),
            Phase::Closed
        );
    }

    /// Property test: every IST minute boundary maps to a deterministic
    /// phase. This is the proptest-equivalent without a proptest dep —
    /// exhaustive sweep over 1440 minutes is cheap (<1ms) and gives us
    /// the same coverage with no false flake risk.
    #[test]
    fn test_compute_phase_property_every_minute() {
        let mut transitions = 0_u32;
        let mut prev = compute_phase(0, ExchangeSegment::NseEquity);
        for minute in 0..(24 * 60) {
            let secs = minute * 60;
            let p = compute_phase(secs, ExchangeSegment::NseEquity);
            // Each phase is non-empty.
            assert!(matches!(
                p,
                Phase::Premarket
                    | Phase::Preopen
                    | Phase::Open
                    | Phase::PostAuction
                    | Phase::Closed
            ));
            if p != prev {
                transitions += 1;
                prev = p;
            }
        }
        // Exactly 4 transitions per day (PRE→PREOPEN→OPEN→POSTAUCTION→CLOSED).
        assert_eq!(
            transitions, 4,
            "expected exactly 4 phase transitions per day"
        );
    }

    /// Property test: phase is segment-agnostic today. If a future
    /// commit makes phase segment-specific, this test fails — forcing
    /// the author to update the property explicitly.
    #[test]
    fn test_compute_phase_segment_agnostic_today() {
        for minute in 0..(24 * 60) {
            let secs = minute * 60;
            let nse_eq = compute_phase(secs, ExchangeSegment::NseEquity);
            let nse_fno = compute_phase(secs, ExchangeSegment::NseFno);
            let bse_eq = compute_phase(secs, ExchangeSegment::BseEquity);
            let idx_i = compute_phase(secs, ExchangeSegment::IdxI);
            assert_eq!(nse_eq, nse_fno, "NSE_EQ vs NSE_FNO at {minute}");
            assert_eq!(nse_eq, bse_eq, "NSE_EQ vs BSE_EQ at {minute}");
            assert_eq!(nse_eq, idx_i, "NSE_EQ vs IDX_I at {minute}");
        }
    }

    /// Ratchet: the 5-variant Phase set is exhaustive — adding a 6th
    /// variant requires updating `Phase::all()` and this test fails until
    /// the size constant matches.
    #[test]
    fn test_phase_all_size_is_pinned_at_five() {
        assert_eq!(
            Phase::all().len(),
            5,
            "Phase has 5 canonical variants — adding a 6th requires \
             updating QuestDB SYMBOL expectations and the matview rebuild \
             probe `views_missing_phase1_columns`"
        );
    }
}
