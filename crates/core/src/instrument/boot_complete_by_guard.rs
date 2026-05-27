//! Sub-PR #11e of 2026-05-27 daily-universe expansion — boot
//! complete-by deadline (operator-locked 08:45 IST).
//!
//! **Feature-gated.** Compiles only when `daily_universe_fetcher` is
//! enabled (per §21).
//!
//! ## Why a separate complete-by deadline
//!
//! Sub-PR #11's `boot_time_of_day_guard` enforces the START window
//! `[08:30, 08:55]`. Operator clarified 2026-05-27 that all setup work
//! (CSV fetch + parse + universe build + WebSocket subscription
//! dispatch) MUST finish by **08:45 IST** — leaving a 15-minute buffer
//! before market open at 09:00 IST.
//!
//! Without a complete-by deadline the orchestrator might START at 08:55
//! (per §10) but FINISH at 09:01 — past market open, with the first
//! ticks already missed.
//!
//! ## Decision matrix
//!
//! | now_ist | Outcome | Boot orchestrator response |
//! |---|---|---|
//! | < `BOOT_COMPLETE_BY_SECS_OF_DAY_IST` | `OnTime { secs_remaining }` | Continue — log INFO with remaining seconds |
//! | == `BOOT_COMPLETE_BY_SECS_OF_DAY_IST` | `OnTime { secs_remaining: 0 }` | Continue (boundary inclusive) |
//! | > `BOOT_COMPLETE_BY_SECS_OF_DAY_IST` | `OverDeadline { secs_over }` | Log CRITICAL Telegram — operator may need to verify universe state at market open |
//!
//! ## What this module does NOT do
//!
//! - Halt the boot on overdeadline (just reports the outcome — boot
//!   orchestrator decides whether to continue)
//! - Read system clock (caller passes `now_secs_of_day_ist`)
//! - Emit Telegram events (caller does)

#![cfg(feature = "daily_universe_fetcher")]

/// Operator-locked boot complete-by deadline. Equals 08:45:00 IST
/// = 8 × 3600 + 45 × 60 = **31,500 seconds-of-day IST**.
///
/// All setup work (CSV fetch + parse + universe build + WebSocket
/// subscription dispatch) MUST finish by this time so the system has
/// a 15-minute buffer before 09:00 IST market open.
pub const BOOT_COMPLETE_BY_SECS_OF_DAY_IST: u32 = 8 * 3600 + 45 * 60;

/// Outcome of the boot complete-by check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootCompleteOutcome {
    /// Boot finished at or before `BOOT_COMPLETE_BY_SECS_OF_DAY_IST`.
    /// `secs_remaining` = how many seconds remain before the deadline
    /// (0 if exactly at the boundary).
    OnTime { secs_remaining: u32 },
    /// Boot finished AFTER `BOOT_COMPLETE_BY_SECS_OF_DAY_IST`. The
    /// 15-min pre-open buffer is eaten into. Operator should verify
    /// whether the universe is fully subscribed at 09:00 market open.
    OverDeadline { secs_over: u32 },
}

impl BootCompleteOutcome {
    /// Stable wire-format string for the L5 AUDIT layer + CloudWatch.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnTime { .. } => "on_time",
            Self::OverDeadline { .. } => "over_deadline",
        }
    }

    /// True if boot finished past the 08:45 deadline.
    #[must_use]
    pub fn is_over_deadline(self) -> bool {
        matches!(self, Self::OverDeadline { .. })
    }
}

/// Pure function — check whether boot setup completed by the 08:45 IST
/// deadline.
///
/// # Arguments
///
/// * `now_secs_of_day_ist` — current IST seconds-of-day at the moment
///   boot setup finished. Computed by the caller from
///   `chrono::Utc::now().timestamp() + IST_UTC_OFFSET_SECONDS_I64`
///   then `.rem_euclid(SECONDS_PER_DAY)`.
///
/// # Performance
///
/// COLD PATH — called once at boot. Pure integer comparison; ~1ns.
#[must_use]
pub fn check_boot_complete_by(now_secs_of_day_ist: u32) -> BootCompleteOutcome {
    if now_secs_of_day_ist <= BOOT_COMPLETE_BY_SECS_OF_DAY_IST {
        BootCompleteOutcome::OnTime {
            secs_remaining: BOOT_COMPLETE_BY_SECS_OF_DAY_IST - now_secs_of_day_ist,
        }
    } else {
        BootCompleteOutcome::OverDeadline {
            secs_over: now_secs_of_day_ist - BOOT_COMPLETE_BY_SECS_OF_DAY_IST,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn hhmm(hh: u32, mm: u32) -> u32 {
        hh * 3600 + mm * 60
    }

    #[test]
    fn deadline_constant_matches_08_45_ist() {
        assert_eq!(BOOT_COMPLETE_BY_SECS_OF_DAY_IST, 31_500);
        assert_eq!(BOOT_COMPLETE_BY_SECS_OF_DAY_IST, hhmm(8, 45));
    }

    #[test]
    fn complete_at_08_30_is_on_time_with_15_min_remaining() {
        // Boot ran in 0 seconds (impossible but boundary test).
        let result = check_boot_complete_by(hhmm(8, 30));
        match result {
            BootCompleteOutcome::OnTime { secs_remaining } => {
                assert_eq!(secs_remaining, 15 * 60);
            }
            _ => panic!("expected OnTime, got {result:?}"),
        }
    }

    #[test]
    fn complete_at_08_40_is_on_time_with_5_min_remaining() {
        // Typical successful boot — 10 minutes to fetch+parse+subscribe.
        let result = check_boot_complete_by(hhmm(8, 40));
        match result {
            BootCompleteOutcome::OnTime { secs_remaining } => {
                assert_eq!(secs_remaining, 5 * 60);
            }
            _ => panic!("expected OnTime"),
        }
    }

    #[test]
    fn complete_at_08_45_exactly_is_on_time_with_zero_remaining() {
        // Boundary case — exactly at deadline, still counts as OnTime.
        let result = check_boot_complete_by(hhmm(8, 45));
        match result {
            BootCompleteOutcome::OnTime { secs_remaining } => {
                assert_eq!(secs_remaining, 0);
            }
            _ => panic!("expected OnTime at boundary"),
        }
    }

    #[test]
    fn complete_at_08_46_is_over_deadline_by_60s() {
        let result = check_boot_complete_by(hhmm(8, 46));
        match result {
            BootCompleteOutcome::OverDeadline { secs_over } => {
                assert_eq!(secs_over, 60);
            }
            _ => panic!("expected OverDeadline"),
        }
    }

    #[test]
    fn complete_at_09_00_market_open_is_over_deadline_by_15_min() {
        // Boot finished right at market open — entire buffer eaten.
        let result = check_boot_complete_by(hhmm(9, 0));
        match result {
            BootCompleteOutcome::OverDeadline { secs_over } => {
                assert_eq!(secs_over, 15 * 60);
            }
            _ => panic!("expected OverDeadline"),
        }
    }

    #[test]
    fn complete_at_10_00_mid_market_is_far_over_deadline() {
        let result = check_boot_complete_by(hhmm(10, 0));
        match result {
            BootCompleteOutcome::OverDeadline { secs_over } => {
                assert_eq!(secs_over, 75 * 60); // 1h 15min
            }
            _ => panic!("expected OverDeadline"),
        }
    }

    #[test]
    fn pre_08_30_returns_on_time_with_large_remaining() {
        // Edge case — boot completed before 08:30 (clock skew).
        // Pure-function semantics: returns OnTime with a large remaining
        // value. The "too early" defense lives in the START guard
        // (Sub-PR #11), not here.
        let result = check_boot_complete_by(hhmm(7, 0));
        match result {
            BootCompleteOutcome::OnTime { secs_remaining } => {
                assert!(secs_remaining > 60 * 60); // > 1 hour
            }
            _ => panic!("expected OnTime"),
        }
    }

    #[test]
    fn outcome_as_str_returns_stable_wire_format() {
        assert_eq!(
            BootCompleteOutcome::OnTime {
                secs_remaining: 300
            }
            .as_str(),
            "on_time"
        );
        assert_eq!(
            BootCompleteOutcome::OverDeadline { secs_over: 60 }.as_str(),
            "over_deadline"
        );
    }

    #[test]
    fn is_over_deadline_truth_table() {
        assert!(
            !BootCompleteOutcome::OnTime {
                secs_remaining: 300
            }
            .is_over_deadline()
        );
        assert!(!BootCompleteOutcome::OnTime { secs_remaining: 0 }.is_over_deadline());
        assert!(BootCompleteOutcome::OverDeadline { secs_over: 1 }.is_over_deadline());
        assert!(BootCompleteOutcome::OverDeadline { secs_over: 1000 }.is_over_deadline());
    }

    #[test]
    fn buffer_to_market_open_is_15_minutes() {
        // Defensive — pin the buffer at exactly 15 min before 09:00.
        let market_open = hhmm(9, 0);
        assert_eq!(market_open - BOOT_COMPLETE_BY_SECS_OF_DAY_IST, 15 * 60);
    }

    #[test]
    fn deterministic_pure_function() {
        let r1 = check_boot_complete_by(hhmm(8, 40));
        let r2 = check_boot_complete_by(hhmm(8, 40));
        assert_eq!(r1, r2);
    }
}
