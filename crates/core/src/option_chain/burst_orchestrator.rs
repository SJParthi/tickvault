//! Pure-function helpers for the minute-aligned option-chain burst
//! scheduler (PR #787, 2026-05-25).
//!
//! ## Operator-locked contract
//!
//! - Every minute at `:50` IST during market hours, fire option-chain
//!   fetches for ALL 3 underlyings (SENSEX/BANKNIFTY/NIFTY) IN
//!   PARALLEL.
//! - Each underlying has its own retry budget — up to N attempts within
//!   the `:50..:60` window, spaced ≥3s apart per Dhan's
//!   1-request-per-3-seconds-per-underlying rate limit.
//! - At the `:59` hard deadline, any in-flight fetch is cancelled and
//!   the slot is recorded as failed. Operator gets a Critical Telegram.
//!
//! ## Why parallel across underlyings works
//!
//! Dhan's option-chain rate limit (per docs):
//!
//! > 1 unique request every 3 seconds. Multiple different underlyings
//! > can be fetched concurrently within the 3s window.
//!
//! So SENSEX + BANKNIFTY + NIFTY fired at the same instant are 3
//! independent rate-limit buckets. Same-underlying retry within 3s
//! would violate the limit, hence the per-underlying retry spacing.
//!
//! ## Burst math
//!
//! | Scenario | Total wall-clock |
//! |---|---|
//! | All 3 succeed first attempt | ~1-3s (done by `:53`) |
//! | 1 retries once, others first-try succeed | ~6s (done by `:56`) |
//! | All 3 retry once | ~6s (parallel, done by `:56`) |
//! | All 3 retry twice | ~9s (parallel, done by `:59`) |
//! | Persistent Dhan failure | Critical Telegram at `:59`; skip minute |
//!
//! ## Authority
//!
//! - operator-charter-forever.md §F — honest envelope: bounded
//!   inside-window guarantee, not magical "never fails".
//! - Dhan rate-limit doc — `dhan/option-chain.md` rule 4.
//! - hot-path.md — these helpers are pure functions, no allocation.

use std::time::Duration;

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS_I64, SECONDS_PER_DAY};

/// Operator-locked minute offset for the burst start. `:50` of every
/// minute. Pinned as a named constant per banned-pattern hook category
/// 3 (no hardcoded magic numbers).
pub const BURST_START_SEC_OF_MINUTE: u32 = 50;

/// Operator-locked HARD deadline for the burst completion. `:59` of
/// the same minute. Any fetch still in flight when wall-clock crosses
/// this is cancelled; the slot records as failed.
///
/// Note: gives 1s buffer to the `:00` minute boundary so the
/// scheduler's bookkeeping (Telegram emit, counter increments, etc.)
/// finishes BEFORE the next burst window opens.
pub const BURST_DEADLINE_SEC_OF_MINUTE: u32 = 59;

/// Per-underlying spacing between retry attempts. Dhan's rate limit is
/// 1 unique request per 3s per underlying. We use 3s exactly so we
/// MAXIMISE retry budget within the 10s `:50..:60` window
/// (10s / 3s = 3 attempts max).
pub const SAME_UNDERLYING_RETRY_SPACING_SECS: u64 = 3;

/// Maximum attempts per underlying inside one burst window. Derived
/// from `(BURST_DEADLINE - BURST_START) / SPACING` rounded down. Pinned
/// as a constant so the limit is operator-visible without algebra.
pub const MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST: u32 = 3;

/// Computes how many seconds to sleep before the NEXT `:50` burst
/// start. Returns the sleep duration in seconds (always >=1, never 0,
/// to ensure we don't fire the same burst twice in the same minute).
///
/// O(1). Pure. Used in `snapshot_scheduler::run_burst_loop`.
#[must_use]
pub fn compute_secs_until_next_burst(now_secs_of_day_ist: u32) -> u32 {
    let sec_in_minute = now_secs_of_day_ist % 60;
    let minute_start = now_secs_of_day_ist - sec_in_minute;

    if sec_in_minute < BURST_START_SEC_OF_MINUTE {
        // Still before this minute's :50 — fire in the gap.
        // Saturating sub is impossible here because we just checked
        // the inequality. Standard sub is fine.
        BURST_START_SEC_OF_MINUTE - sec_in_minute
    } else {
        // Already at or past this minute's :50. Fire at NEXT minute's
        // :50. saturating_add prevents wrap at end-of-day (no-op in
        // practice — 86400 + 60 is many orders below u32::MAX).
        let next_minute_start = minute_start.saturating_add(60);
        let next_target = next_minute_start.saturating_add(BURST_START_SEC_OF_MINUTE);
        next_target.saturating_sub(now_secs_of_day_ist)
    }
}

/// Returns the absolute `secs_of_day_ist` value of the hard deadline
/// for a burst that started at `burst_start_secs_of_day_ist`.
///
/// Always equals `burst_start + (DEADLINE - START)` = `burst_start + 9`.
/// Pure helper kept separate from the per-underlying retry loop so
/// the ratchet tests can pin the offset.
#[must_use]
pub const fn compute_burst_deadline_secs_of_day_ist(burst_start_secs_of_day_ist: u32) -> u32 {
    burst_start_secs_of_day_ist
        .saturating_add(BURST_DEADLINE_SEC_OF_MINUTE - BURST_START_SEC_OF_MINUTE)
}

/// Computes the wall-clock IST `secs_of_day` for the current instant.
/// Used by the scheduler loop. Pure modulo arithmetic over the
/// chrono-supplied UTC second.
#[must_use]
pub fn ist_secs_of_day_now(now_utc_secs: i64) -> u32 {
    let now_ist_secs = now_utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    #[allow(clippy::cast_sign_loss)] // APPROVED: rem_euclid output is always positive
    let secs = now_ist_secs.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
    secs
}

/// Outcome of one per-underlying fetch loop within a burst.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BurstAttemptOutcome {
    /// One of the retries succeeded.
    Succeeded {
        /// 1-based attempt number that finally succeeded.
        attempt: u32,
    },
    /// All retries within the deadline window failed.
    Failed {
        /// Total attempts made before giving up.
        attempts: u32,
        /// Last error message (truncated).
        last_error: String,
    },
    /// The deadline elapsed before even one attempt could be tried.
    DeadlineExpiredBeforeFirstAttempt,
}

/// Computes whether a retry can fit before the deadline, given the
/// current attempt count and the seconds remaining. Pure helper —
/// makes the per-underlying retry loop's decision testable in
/// isolation.
///
/// Returns `Some(sleep_secs_before_next_attempt)` if a retry can fit,
/// `None` if the deadline is too close.
///
/// The math: a retry must respect `SAME_UNDERLYING_RETRY_SPACING_SECS`
/// AND leave room for the call itself (~1-3s budgeted). We use the
/// pessimistic 3s budget so the retry doesn't overshoot the deadline.
#[must_use]
pub fn next_retry_sleep_secs(
    secs_remaining_until_deadline: u32,
    attempts_so_far: u32,
) -> Option<u64> {
    if attempts_so_far >= MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST {
        return None;
    }
    let min_window_needed =
        SAME_UNDERLYING_RETRY_SPACING_SECS.saturating_add(SAME_UNDERLYING_RETRY_SPACING_SECS); // spacing + call budget
    if u64::from(secs_remaining_until_deadline) < min_window_needed {
        return None;
    }
    Some(SAME_UNDERLYING_RETRY_SPACING_SECS)
}

/// Converts a `secs_of_day_ist` deadline target to a `tokio::time::Duration`
/// to wait from the current instant. Returns `Duration::ZERO` if the
/// deadline is already past.
#[must_use]
pub fn duration_until_deadline(current_secs_of_day: u32, deadline_secs_of_day: u32) -> Duration {
    Duration::from_secs(u64::from(
        deadline_secs_of_day.saturating_sub(current_secs_of_day),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_burst_start_is_50_seconds_of_minute() {
        // Operator-locked: every minute at :50.
        assert_eq!(BURST_START_SEC_OF_MINUTE, 50);
    }

    #[test]
    fn test_burst_deadline_is_59_seconds_of_minute() {
        // Operator-locked: hard cutoff at :59 (1s buffer to :00).
        assert_eq!(BURST_DEADLINE_SEC_OF_MINUTE, 59);
    }

    #[test]
    fn test_burst_window_is_10_seconds_wide() {
        // The fetch window from :50 to :60 (exclusive) — 10 seconds.
        // Deadline at :59 gives all in-flight calls 1s grace to
        // settle their bookkeeping before :00.
        assert_eq!(BURST_DEADLINE_SEC_OF_MINUTE - BURST_START_SEC_OF_MINUTE, 9);
    }

    #[test]
    fn test_max_attempts_is_3_per_underlying() {
        // Math: window = 10s, spacing = 3s, call budget = 3s.
        // First attempt at t=0 (3s).
        // Second attempt at t=3s (another 3s).
        // Third attempt at t=6s (another 3s, finishes by t=9s).
        // Fourth attempt would start at t=9s, finish at t=12s ❌.
        assert_eq!(MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST, 3);
    }

    #[test]
    fn test_compute_secs_until_next_burst_before_50() {
        // At 09:15:25 IST (sec_in_minute=25), next :50 is 25s away.
        let now = 9 * 3600 + 15 * 60 + 25;
        assert_eq!(compute_secs_until_next_burst(now), 25);
    }

    #[test]
    fn test_compute_secs_until_next_burst_exactly_at_50() {
        // At 09:15:50 IST exactly — :50 just fired, sleep until next
        // minute's :50 = 60s away.
        let now = 9 * 3600 + 15 * 60 + 50;
        assert_eq!(compute_secs_until_next_burst(now), 60);
    }

    #[test]
    fn test_compute_secs_until_next_burst_after_50() {
        // At 09:15:55 IST — past this minute's :50, fire at 09:16:50.
        // Gap: 60 - 55 + 50 = 55s.
        let now = 9 * 3600 + 15 * 60 + 55;
        assert_eq!(compute_secs_until_next_burst(now), 55);
    }

    #[test]
    fn test_compute_secs_until_next_burst_at_start_of_minute() {
        // At HH:MM:00 — fire at :50 of THIS minute = 50s away.
        let now = 10 * 3600 + 0 * 60 + 0;
        assert_eq!(compute_secs_until_next_burst(now), 50);
    }

    #[test]
    fn test_compute_burst_deadline_secs_of_day_ist_offset_is_9() {
        // burst_start :50 → deadline :59 → offset = 9.
        let burst_start = 9 * 3600 + 15 * 60 + 50;
        let deadline = compute_burst_deadline_secs_of_day_ist(burst_start);
        assert_eq!(deadline - burst_start, 9);
    }

    #[test]
    fn test_next_retry_sleep_secs_alias_for_guard() {
        // Pub-fn-test-guard requires a test name containing the full
        // pub fn identifier `next_retry_sleep_secs`. The canonical
        // scenario tests below cover the actual semantics.
        assert_eq!(next_retry_sleep_secs(9, 1), Some(3));
    }

    #[test]
    fn test_next_retry_sleep_after_first_failure_returns_3s() {
        // Just failed attempt 1; 9s remain → can fit 1 more attempt
        // (sleep 3s + call ~3s = 6s, leaves 3s margin).
        let sleep = next_retry_sleep_secs(9, 1);
        assert_eq!(sleep, Some(3));
    }

    #[test]
    fn test_next_retry_sleep_after_second_failure_returns_3s() {
        // Just failed attempt 2; 6s remain → CAN fit 1 more (3+3=6).
        let sleep = next_retry_sleep_secs(6, 2);
        assert_eq!(sleep, Some(3));
    }

    #[test]
    fn test_next_retry_sleep_after_third_failure_returns_none() {
        // Already at max attempts (3) — no more retries allowed.
        let sleep = next_retry_sleep_secs(3, 3);
        assert_eq!(sleep, None);
    }

    #[test]
    fn test_next_retry_sleep_deadline_too_close_returns_none() {
        // Only 5s remain; need 6s (3 spacing + 3 call budget). Skip.
        let sleep = next_retry_sleep_secs(5, 1);
        assert_eq!(sleep, None);
    }

    #[test]
    fn test_next_retry_sleep_at_attempts_above_max_returns_none() {
        // Defensive — never overshoot even if caller is buggy.
        let sleep = next_retry_sleep_secs(100, 999);
        assert_eq!(sleep, None);
    }

    #[test]
    fn test_ist_secs_of_day_now_handles_negative_utc_offset_safely() {
        // Pre-IST-epoch UTC second — saturating_add ensures no
        // panic even if the system clock wandered backwards.
        let result = ist_secs_of_day_now(-1_000);
        assert!(result < SECONDS_PER_DAY);
    }

    #[test]
    fn test_duration_until_deadline_returns_zero_when_past() {
        // Current time is past the deadline — return Duration::ZERO,
        // not a wrap.
        let now = 9 * 3600 + 15 * 60 + 59;
        let deadline = 9 * 3600 + 15 * 60 + 50; // Already past.
        assert_eq!(duration_until_deadline(now, deadline), Duration::ZERO);
    }

    #[test]
    fn test_duration_until_deadline_correct_normal_case() {
        let now = 9 * 3600 + 15 * 60 + 53;
        let deadline = 9 * 3600 + 15 * 60 + 59;
        assert_eq!(
            duration_until_deadline(now, deadline),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn test_burst_attempt_outcome_serializes_for_logs() {
        // Operator visibility — these variants are emitted as INFO/
        // ERROR with the variant name + payload.
        let succeeded = BurstAttemptOutcome::Succeeded { attempt: 2 };
        let failed = BurstAttemptOutcome::Failed {
            attempts: 3,
            last_error: "DH-904".into(),
        };
        let expired = BurstAttemptOutcome::DeadlineExpiredBeforeFirstAttempt;
        // Just verify Debug works (used by tracing macros).
        assert!(format!("{succeeded:?}").contains("Succeeded"));
        assert!(format!("{failed:?}").contains("Failed"));
        assert!(format!("{expired:?}").contains("DeadlineExpired"));
    }
}
