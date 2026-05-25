//! Post-market fetch window gate — operator-locked 2026-05-25.
//!
//! Verbatim operator quote 2026-05-25 17:10 IST:
//! > "this cross verification and historical fetch of 90 days and
//! >  current day historical fetch of current day if 1,5,15,60
//! >  should always happen between this timeline alone only dude
//! >  okay which is between 3.30 pm between 11 pm alone only dude
//! >  okay?"
//!
//! ## What this gates
//!
//! | Operation | Allowed window |
//! |---|---|
//! | 90-day historical fetch (post-market) | 15:30 – 23:00 IST |
//! | Current-day intraday fetch (1m/5m/15m/60m) | 15:30 – 23:00 IST |
//! | Cross-verification (historical vs candles tables) | 15:30 – 23:00 IST |
//!
//! ## What this does NOT gate
//!
//! - Pre-market yesterday's-1d fetch at 09:00:30 IST (PR #793 — narrow
//!   exception per `live-feed-purity.md` rule 8). That ~20-call operation
//!   runs OUTSIDE this window by operator directive.
//! - Live WebSocket tick capture (09:15–15:30 IST).
//! - Order-update WebSocket (market hours).
//!
//! ## Behavior at the boundaries
//!
//! - `is_within_post_market_fetch_window` returns `true` for any
//!   `secs_of_day_ist` in `[15:30, 23:00)` — start inclusive, end exclusive.
//! - At 23:00:00 IST the function returns `false` — operations in flight
//!   must abort cleanly and write a `cutoff_reached` outcome to the JSONL.
//!
//! ## O(1) hot-path
//!
//! Single u32 range check. No allocation, no I/O, no syscalls. Safe to
//! call from any hot path; the typical caller is a per-cycle scheduler
//! tick or a per-batch retry loop.

use tickvault_common::constants::{
    POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST, POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST,
};

/// Returns `true` if the given seconds-of-day IST value falls within the
/// post-market fetch window `[15:30, 23:00)`.
///
/// Operator-locked 2026-05-25 — see module-level docs.
///
/// # Performance
///
/// O(1). Single u32 range check. Inlined.
#[inline]
#[must_use]
// TEST-EXEMPT: covered by 8 unit tests in this module (`test_is_within_window_*`) — boundary, midnight, 09:00, 15:30, 17:00, 22:59, 23:00, 23:01.
pub fn is_within_post_market_fetch_window(secs_of_day_ist: u32) -> bool {
    secs_of_day_ist >= POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST
        && secs_of_day_ist < POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST
}

/// Reason a post-market fetch operation cannot proceed at the current
/// wall-clock time. Used to emit a typed cutoff signal in JSONL +
/// Telegram payloads so the operator sees exactly which boundary was
/// hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchWindowGateOutcome {
    /// Current seconds-of-day IST is within `[15:30, 23:00)`. Caller
    /// proceeds with the fetch / cross-verify.
    InWindow,
    /// Current seconds-of-day IST is before 15:30 IST. Caller MUST NOT
    /// fetch pre-market (per operator directive 2026-04-22 +
    /// 2026-05-25). PR #793 yesterday's-1d at 09:00:30 IST is the only
    /// narrow exception (uses a separate scheduler module, not this gate).
    BeforeWindowStart,
    /// Current seconds-of-day IST is at or after 23:00 IST. Caller MUST
    /// abort in-flight retries and write a `cutoff_reached` outcome.
    /// Tomorrow's 15:30 IST scheduler will pick up the work.
    AfterWindowEnd,
}

/// O(1) classifier returning the typed gate outcome.
#[inline]
#[must_use]
// TEST-EXEMPT: covered by 4 unit tests in this module (`test_classify_at_*`) — 0900, 1530, 2300, 2359.
pub fn classify_post_market_fetch_window(secs_of_day_ist: u32) -> FetchWindowGateOutcome {
    if secs_of_day_ist < POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST {
        FetchWindowGateOutcome::BeforeWindowStart
    } else if secs_of_day_ist >= POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST {
        FetchWindowGateOutcome::AfterWindowEnd
    } else {
        FetchWindowGateOutcome::InWindow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_within_window_at_1530_exactly_is_true() {
        assert!(is_within_post_market_fetch_window(15 * 3600 + 30 * 60));
    }

    #[test]
    fn test_is_within_window_just_before_1530_is_false() {
        assert!(!is_within_post_market_fetch_window(15 * 3600 + 30 * 60 - 1));
    }

    #[test]
    fn test_is_within_window_at_1700_is_true() {
        assert!(is_within_post_market_fetch_window(17 * 3600));
    }

    #[test]
    fn test_is_within_window_at_2259_is_true() {
        assert!(is_within_post_market_fetch_window(22 * 3600 + 59 * 60));
    }

    #[test]
    fn test_is_within_window_at_2300_exactly_is_false() {
        assert!(!is_within_post_market_fetch_window(23 * 3600));
    }

    #[test]
    fn test_is_within_window_at_2301_is_false() {
        assert!(!is_within_post_market_fetch_window(23 * 3600 + 1));
    }

    #[test]
    fn test_is_within_window_at_midnight_is_false() {
        assert!(!is_within_post_market_fetch_window(0));
    }

    #[test]
    fn test_is_within_window_at_0900_is_false() {
        assert!(!is_within_post_market_fetch_window(9 * 3600));
    }

    #[test]
    fn test_classify_at_0900_is_before_window_start() {
        assert_eq!(
            classify_post_market_fetch_window(9 * 3600),
            FetchWindowGateOutcome::BeforeWindowStart
        );
    }

    #[test]
    fn test_classify_at_1530_is_in_window() {
        assert_eq!(
            classify_post_market_fetch_window(15 * 3600 + 30 * 60),
            FetchWindowGateOutcome::InWindow
        );
    }

    #[test]
    fn test_classify_at_2300_is_after_window_end() {
        assert_eq!(
            classify_post_market_fetch_window(23 * 3600),
            FetchWindowGateOutcome::AfterWindowEnd
        );
    }

    #[test]
    fn test_classify_at_2359_is_after_window_end() {
        assert_eq!(
            classify_post_market_fetch_window(23 * 3600 + 59 * 60),
            FetchWindowGateOutcome::AfterWindowEnd
        );
    }

    #[test]
    fn test_window_duration_is_seven_and_half_hours() {
        // 23:00 IST - 15:30 IST = 7h 30m = 27_000 seconds.
        let duration_secs = POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST
            - POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST;
        assert_eq!(duration_secs, 7 * 3600 + 30 * 60);
    }
}
