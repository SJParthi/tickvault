//! Phase 0 Item 22a — DH-904 (rate-limited) exponential-backoff
//! ladder helper.
//!
//! Per `.claude/rules/dhan/api-introduction.md` rule 8: when Dhan
//! returns `DH-904` (rate limit exceeded), retries MUST follow the
//! 10s → 20s → 40s → 80s exponential ladder. After 4 attempts the
//! request gives up and the operator gets a CRITICAL alert.
//!
//! This module is a pure-function helper — the integration into the
//! engine.rs order placement / modify / cancel hot path is a
//! separate sub-PR (Item 22a-wire) to keep this PR's scope focused
//! on the ladder math + tests.

use std::time::Duration;

use tickvault_common::constants::{DH904_BACKOFF_SECS, DH904_MAX_RETRY_ATTEMPTS};

/// Returns the backoff `Duration` to wait BEFORE the next retry,
/// given how many DH-904 responses have already been observed for
/// this request. `Some(d)` means "wait `d` and retry"; `None` means
/// "retry budget exhausted — escalate to CRITICAL".
///
/// Mapping (0-indexed):
///   * `attempts_so_far == 0` → wait 10s (1st retry)
///   * `attempts_so_far == 1` → wait 20s (2nd retry)
///   * `attempts_so_far == 2` → wait 40s (3rd retry)
///   * `attempts_so_far == 3` → wait 80s (4th retry, last)
///   * `attempts_so_far >= 4` → `None` (give up)
///
/// **Why not exponential `pow(2, n) * 10`?** Per the operator
/// charter, the ladder is hard-coded in
/// `DH904_BACKOFF_SECS = [10, 20, 40, 80]` so future tuning (e.g.
/// reducing the 80s tail for low-latency contracts) is a one-line
/// edit in `constants.rs`, not a math change.
///
/// O(1) — bounds check + array index, no allocation.
#[inline]
#[must_use]
pub fn compute_dh904_backoff(attempts_so_far: u32) -> Option<Duration> {
    let idx = attempts_so_far as usize;
    if idx >= DH904_BACKOFF_SECS.len() {
        return None;
    }
    DH904_BACKOFF_SECS
        .get(idx)
        .copied()
        .map(Duration::from_secs)
}

/// Returns `true` when the caller has exhausted the DH-904 retry
/// budget. Equivalent to `compute_dh904_backoff(attempts_so_far).is_none()`
/// but more readable at call sites that decide between
/// `tokio::time::sleep(...)` and `escalate-to-CRITICAL`.
#[inline]
#[must_use]
pub fn is_dh904_budget_exhausted(attempts_so_far: u32) -> bool {
    (attempts_so_far as usize) >= DH904_MAX_RETRY_ATTEMPTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_dh904_backoff_zero_attempts_returns_10s() {
        // First DH-904 response — operator hasn't waited yet.
        assert_eq!(
            compute_dh904_backoff(0),
            Some(Duration::from_secs(10)),
            "1st retry must wait 10s per dhan-api-introduction.md rule 8"
        );
    }

    #[test]
    fn test_compute_dh904_backoff_one_attempt_returns_20s() {
        assert_eq!(compute_dh904_backoff(1), Some(Duration::from_secs(20)));
    }

    #[test]
    fn test_compute_dh904_backoff_two_attempts_returns_40s() {
        assert_eq!(compute_dh904_backoff(2), Some(Duration::from_secs(40)));
    }

    #[test]
    fn test_compute_dh904_backoff_three_attempts_returns_80s() {
        assert_eq!(compute_dh904_backoff(3), Some(Duration::from_secs(80)));
    }

    #[test]
    fn test_compute_dh904_backoff_at_budget_limit_returns_none() {
        // The 5th call (idx 4) is past the end of the ladder — give up.
        assert_eq!(compute_dh904_backoff(4), None);
    }

    #[test]
    fn test_compute_dh904_backoff_far_past_budget_returns_none() {
        // Caller bug-safety: any high `attempts_so_far` returns None
        // without panic / overflow.
        assert_eq!(compute_dh904_backoff(100), None);
        assert_eq!(compute_dh904_backoff(u32::MAX), None);
    }

    #[test]
    fn test_is_dh904_budget_exhausted_zero_attempts_is_false() {
        assert!(!is_dh904_budget_exhausted(0));
    }

    #[test]
    fn test_is_dh904_budget_exhausted_at_budget_limit_is_true() {
        assert!(is_dh904_budget_exhausted(4));
        assert!(is_dh904_budget_exhausted(100));
        assert!(is_dh904_budget_exhausted(u32::MAX));
    }

    #[test]
    fn test_is_dh904_budget_exhausted_below_budget_limit_is_false() {
        for attempts in 0..DH904_MAX_RETRY_ATTEMPTS as u32 {
            assert!(
                !is_dh904_budget_exhausted(attempts),
                "attempt {attempts} must NOT exhaust budget — still {} retries left",
                DH904_MAX_RETRY_ATTEMPTS as u32 - attempts
            );
        }
    }

    /// Ladder invariant: each step must be at least as long as the
    /// previous (monotonic non-decreasing). The 10/20/40/80 ladder
    /// satisfies this — a future edit that accidentally inserts a
    /// short value (e.g. `[10, 5, 40, 80]`) would defeat the
    /// rate-limit-recovery purpose.
    #[test]
    fn test_dh904_backoff_ladder_is_monotonically_non_decreasing() {
        for window in DH904_BACKOFF_SECS.windows(2) {
            assert!(
                window[0] <= window[1],
                "DH904_BACKOFF_SECS must be monotonically non-decreasing: \
                 found {:?} > {:?} in {:?}",
                window[0],
                window[1],
                DH904_BACKOFF_SECS
            );
        }
    }

    /// Total worst-case wait — operator-facing contract. The
    /// dhan-api-introduction.md rule documents this as "10s → 20s
    /// → 40s → 80s → give up", i.e. 150s of cumulative wait. A
    /// future edit that changes the sum should be loudly intentional.
    #[test]
    fn test_dh904_total_worst_case_wait_is_150s() {
        let total: u64 = DH904_BACKOFF_SECS.iter().sum();
        assert_eq!(
            total, 150,
            "DH-904 retry ladder total worst-case wait must be 150s \
             (10+20+40+80) per dhan-api-introduction.md rule 8. \
             Changing this is operator-visible behaviour."
        );
    }

    /// Constants consistency — `DH904_MAX_RETRY_ATTEMPTS` MUST equal
    /// `DH904_BACKOFF_SECS.len()`. Same compile-time pattern as
    /// `GAP_FILL_RETRY_ATTEMPTS` vs `GAP_FILL_RETRY_BACKOFF_SECS`.
    #[test]
    fn test_dh904_max_retry_attempts_matches_ladder_length() {
        assert_eq!(DH904_MAX_RETRY_ATTEMPTS, DH904_BACKOFF_SECS.len());
        assert_eq!(DH904_MAX_RETRY_ATTEMPTS, 4);
    }
}
