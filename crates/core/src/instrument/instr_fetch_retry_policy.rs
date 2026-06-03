//! Sub-PR #10b-α (2026-05-27): pure-function retry policy for the
//! instrument-master CSV fetch chain. Implements the §4 infinite-retry
//! escalation ladder from
//! `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`.
//!
//! Per operator Quote 4 (2026-05-27): "irrespective of any situations it
//! should never ever fail i mean until or unless without the proper fetch
//! it should retry right". Boot stays BLOCKED until a fresh, validated
//! CSV is in hand and the daily universe is built. The system NEVER
//! silently proceeds with stale or partial data.
//!
//! # Escalation ladder (locked)
//!
//! | Attempt | Backoff before next | Telegram severity | Cadence |
//! |---------|---------------------|-------------------|---------|
//! | 1–3     | 10s / 20s / 40s     | none (silent)     | —       |
//! | 4–10    | `min(2^N · 10s, 300s)` | `Info`         | every attempt |
//! | 11–20   | capped 300s         | `High`            | every 5 |
//! | 21+     | capped 300s         | `Critical`        | every 5 |
//!
//! # Behaviour
//!
//! - `compute_backoff(attempt)` — `Duration` to sleep BEFORE attempt N+1.
//! - `classify_severity(attempt)` — Telegram severity tier (None when silent).
//! - `should_emit_telegram(attempt)` — gated by cadence (every-attempt for 4–10,
//!   every-5th for 11+). Combined with `classify_severity` yields the
//!   on-the-wire emission decision.
//!
//! All functions are pure and infallible. No clock, no I/O, no allocation.

use core::time::Duration;
use tickvault_common::error_code::Severity;

/// Initial backoff base — 10 seconds.
pub const RETRY_BASE_SECS: u64 = 10;

/// Backoff cap — 300 seconds (5 minutes).
pub const RETRY_CAP_SECS: u64 = 300;

/// Last silent attempt — attempts 1–3 emit no Telegram.
pub const SILENT_THRESHOLD: u32 = 3;

/// Last Info-tier attempt — attempts 4–10 emit `Severity::Info` per attempt.
pub const INFO_THRESHOLD: u32 = 10;

/// Last High-tier attempt — attempts 11–20 emit `Severity::High` every 5th.
pub const HIGH_THRESHOLD: u32 = 20;

/// Cadence divisor for High/Critical tiers — every 5th attempt fires.
pub const ESCALATED_CADENCE: u32 = 5;

/// Computes the `Duration` to sleep before attempt `attempt + 1`.
///
/// `attempt` is 1-based — `attempt = 1` is the first try, returning the
/// backoff between the first failure and the second try.
///
/// Schedule:
/// - attempt 1 → 10s
/// - attempt 2 → 20s
/// - attempt 3 → 40s
/// - attempt 4 → 80s
/// - attempt 5 → 160s
/// - attempt 6+ → capped at 300s
///
/// `attempt = 0` is treated as the first attempt (10s).
#[must_use]
pub fn compute_backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(31);
    let multiplier: u64 = 1u64 << exponent;
    let raw_secs = RETRY_BASE_SECS.saturating_mul(multiplier);
    let bounded_secs = raw_secs.min(RETRY_CAP_SECS);
    Duration::from_secs(bounded_secs)
}

/// Classifies the Telegram severity tier for the given retry attempt.
///
/// Returns `None` for silent attempts (1–3). Otherwise returns the tier
/// for the bracket the attempt falls into.
///
/// `attempt = 0` is treated as silent.
#[must_use]
pub fn classify_severity(attempt: u32) -> Option<Severity> {
    if attempt == 0 || attempt <= SILENT_THRESHOLD {
        None
    } else if attempt <= INFO_THRESHOLD {
        Some(Severity::Info)
    } else if attempt <= HIGH_THRESHOLD {
        Some(Severity::High)
    } else {
        Some(Severity::Critical)
    }
}

/// Returns `true` if attempt `attempt` should actually emit a Telegram event.
///
/// - Silent (1–3): never.
/// - Info (4–10): every attempt.
/// - High (11–20) / Critical (21+): every 5th attempt counted from the
///   bracket entry (so 11, 16, 21, 26, ... — the (attempt - 1) % 5 == 0
///   pattern relative to the first escalated attempt).
///
/// Combined with `classify_severity` this yields the on-the-wire emission
/// decision: emit iff `should_emit_telegram(attempt) && classify_severity(attempt).is_some()`.
#[must_use]
pub fn should_emit_telegram(attempt: u32) -> bool {
    if attempt == 0 || attempt <= SILENT_THRESHOLD {
        return false;
    }
    if attempt <= INFO_THRESHOLD {
        return true;
    }
    // High / Critical tiers — every 5th attempt counted from attempt = 11.
    let offset = attempt - (INFO_THRESHOLD + 1);
    offset.is_multiple_of(ESCALATED_CADENCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_backoff_attempt_1_is_10s() {
        assert_eq!(compute_backoff(1), Duration::from_secs(10));
    }

    #[test]
    fn compute_backoff_attempt_2_is_20s() {
        assert_eq!(compute_backoff(2), Duration::from_secs(20));
    }

    #[test]
    fn compute_backoff_attempt_3_is_40s() {
        assert_eq!(compute_backoff(3), Duration::from_secs(40));
    }

    #[test]
    fn compute_backoff_attempt_4_is_80s() {
        assert_eq!(compute_backoff(4), Duration::from_secs(80));
    }

    #[test]
    fn compute_backoff_attempt_5_is_160s() {
        assert_eq!(compute_backoff(5), Duration::from_secs(160));
    }

    #[test]
    fn compute_backoff_attempt_6_caps_at_300s() {
        assert_eq!(compute_backoff(6), Duration::from_secs(300));
    }

    #[test]
    fn compute_backoff_high_attempts_remain_capped() {
        for attempt in [10u32, 20, 100, 1_000, 10_000, u32::MAX] {
            assert_eq!(
                compute_backoff(attempt),
                Duration::from_secs(RETRY_CAP_SECS),
                "attempt {attempt} must stay at the cap",
            );
        }
    }

    #[test]
    fn compute_backoff_attempt_0_treated_as_first() {
        assert_eq!(compute_backoff(0), Duration::from_secs(10));
    }

    #[test]
    fn classify_severity_silent_for_1_through_3() {
        for attempt in 1u32..=3 {
            assert_eq!(classify_severity(attempt), None);
        }
    }

    #[test]
    fn classify_severity_zero_is_silent() {
        assert_eq!(classify_severity(0), None);
    }

    #[test]
    fn classify_severity_info_for_4_through_10() {
        for attempt in 4u32..=10 {
            assert_eq!(classify_severity(attempt), Some(Severity::Info));
        }
    }

    #[test]
    fn classify_severity_high_for_11_through_20() {
        for attempt in 11u32..=20 {
            assert_eq!(classify_severity(attempt), Some(Severity::High));
        }
    }

    #[test]
    fn classify_severity_critical_for_21_plus() {
        for attempt in [21u32, 25, 50, 100, 1_000, u32::MAX] {
            assert_eq!(classify_severity(attempt), Some(Severity::Critical));
        }
    }

    #[test]
    fn should_emit_silent_for_first_three_attempts() {
        for attempt in 0u32..=3 {
            assert!(!should_emit_telegram(attempt));
        }
    }

    #[test]
    fn should_emit_every_attempt_in_info_band() {
        for attempt in 4u32..=10 {
            assert!(should_emit_telegram(attempt), "attempt {attempt}");
        }
    }

    #[test]
    fn should_emit_every_5th_in_high_band() {
        assert!(should_emit_telegram(11));
        assert!(!should_emit_telegram(12));
        assert!(!should_emit_telegram(13));
        assert!(!should_emit_telegram(14));
        assert!(!should_emit_telegram(15));
        assert!(should_emit_telegram(16));
        assert!(!should_emit_telegram(17));
        assert!(!should_emit_telegram(20));
    }

    #[test]
    fn should_emit_every_5th_in_critical_band() {
        assert!(should_emit_telegram(21));
        assert!(!should_emit_telegram(22));
        assert!(should_emit_telegram(26));
        assert!(should_emit_telegram(31));
        assert!(!should_emit_telegram(32));
    }

    #[test]
    fn combined_emission_decision_matches_table() {
        let scenarios: &[(u32, bool, Option<Severity>)] = &[
            (1, false, None),
            (3, false, None),
            (4, true, Some(Severity::Info)),
            (10, true, Some(Severity::Info)),
            (11, true, Some(Severity::High)),
            (15, false, Some(Severity::High)),
            (16, true, Some(Severity::High)),
            (20, false, Some(Severity::High)),
            (21, true, Some(Severity::Critical)),
            (26, true, Some(Severity::Critical)),
            (27, false, Some(Severity::Critical)),
        ];
        for &(attempt, expected_emit, expected_sev) in scenarios {
            assert_eq!(
                should_emit_telegram(attempt),
                expected_emit,
                "attempt {attempt} emit gate",
            );
            assert_eq!(
                classify_severity(attempt),
                expected_sev,
                "attempt {attempt} severity",
            );
        }
    }

    #[test]
    fn constants_match_locked_values() {
        assert_eq!(RETRY_BASE_SECS, 10);
        assert_eq!(RETRY_CAP_SECS, 300);
        assert_eq!(SILENT_THRESHOLD, 3);
        assert_eq!(INFO_THRESHOLD, 10);
        assert_eq!(HIGH_THRESHOLD, 20);
        assert_eq!(ESCALATED_CADENCE, 5);
    }

    #[test]
    fn backoff_monotonically_non_decreasing() {
        let mut last = Duration::ZERO;
        for attempt in 1u32..=12 {
            let current = compute_backoff(attempt);
            assert!(
                current >= last,
                "backoff regressed at attempt {attempt}: {last:?} -> {current:?}",
            );
            last = current;
        }
    }

    #[test]
    fn severity_never_downgrades_with_attempt() {
        let order = [
            None,
            Some(Severity::Info),
            Some(Severity::High),
            Some(Severity::Critical),
        ];
        let rank = |s: Option<Severity>| order.iter().position(|x| *x == s).unwrap();
        let mut last_rank = 0usize;
        for attempt in 1u32..=30 {
            let r = rank(classify_severity(attempt));
            assert!(r >= last_rank, "severity regressed at attempt {attempt}",);
            last_rank = r;
        }
    }
}
