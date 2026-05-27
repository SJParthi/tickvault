//! Sub-PR #10b-β (2026-05-27): pure-function adapter that bridges the
//! [`OrchestratorError`] taxonomy (Sub-PR #10) with the
//! [`instr_fetch_retry_policy`](super::instr_fetch_retry_policy)
//! primitives (Sub-PR #10b-α).
//!
//! Given a failed orchestrator attempt and its 1-based attempt counter,
//! returns a deterministic [`RetryDecision`] carrying:
//!
//! 1. The [`Duration`] to sleep before the next attempt.
//! 2. An optional [`TelegramEmit`] describing the wire-format Telegram
//!    payload the boot orchestrator (Sub-PR #10b integration) MUST
//!    publish — `None` when the §4 cadence gate suppresses emission for
//!    this attempt.
//!
//! The adapter is **pure** — no clock, no I/O, no allocation. The boot
//! orchestrator owns the actual `tokio::time::sleep` + Telegram dispatch
//! side-effects so this module stays unit-testable without async runtime.
//!
//! # Why this layer exists
//!
//! Without this adapter, the integration PR would either:
//! (a) re-derive the severity / cadence / backoff inline at every emit
//!     site (duplication + drift risk), or
//! (b) entangle the pure retry policy with the orchestrator's error
//!     taxonomy (cyclic dependency between modules).
//!
//! Sub-PR #10b-β cleanly separates: policy (10b-α) ← adapter (10b-β) →
//! orchestrator (10) ← integration (10b proper).
//!
//! # Mapping table
//!
//! | Source field | Resolved from |
//! |---|---|
//! | `RetryDecision::backoff` | [`compute_backoff`](super::instr_fetch_retry_policy::compute_backoff) |
//! | `TelegramEmit::severity` | [`classify_severity`](super::instr_fetch_retry_policy::classify_severity) |
//! | `TelegramEmit::error_code` | [`OrchestratorError::error_code`](super::daily_universe_orchestrator::OrchestratorError::error_code) |
//! | `TelegramEmit::stage` | [`OrchestratorError::stage`](super::daily_universe_orchestrator::OrchestratorError::stage) |
//! | emit gate | [`should_emit_telegram`](super::instr_fetch_retry_policy::should_emit_telegram) |
//!
//! Per §4 of `daily-universe-scope-expansion-2026-05-27.md`: boot stays
//! BLOCKED until a fresh validated CSV is in hand. This module is the
//! deterministic decision layer between "fetch failed" and "what does
//! the operator hear about it on the next attempt".

use core::time::Duration;

use tickvault_common::error_code::{ErrorCode, Severity};

use super::daily_universe_orchestrator::OrchestratorError;
use super::instr_fetch_retry_policy::{classify_severity, compute_backoff, should_emit_telegram};

/// Wire-format Telegram payload to emit for a single retry attempt.
///
/// Owned identifiers only — no borrow of the originating
/// [`OrchestratorError`] so the boot orchestrator can drop the error
/// before publishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelegramEmit {
    /// Telegram severity tier per [`classify_severity`].
    pub severity: Severity,
    /// INSTR-FETCH-* code routing dimension per
    /// [`OrchestratorError::error_code`].
    pub error_code: ErrorCode,
    /// Stable wire-format label of the failed orchestrator stage per
    /// [`OrchestratorError::stage`]. Used for CloudWatch dimension
    /// `stage` and audit-row `failed_stage` column.
    pub stage: &'static str,
}

/// Deterministic decision for a single failed retry attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryDecision {
    /// How long to sleep before attempt `N + 1`.
    pub backoff: Duration,
    /// Telegram emission payload — `None` when the cadence gate
    /// suppresses emission for attempt `N`.
    pub telegram: Option<TelegramEmit>,
}

/// Compute the [`RetryDecision`] for a failed orchestrator attempt.
///
/// `attempt` is 1-based — `attempt = 1` means the first fetch+build
/// cycle just failed and the caller is deciding what to do before the
/// second attempt.
///
/// `attempt = 0` is treated identically to `attempt = 1` (silent, 10s
/// backoff) so the adapter is robust to off-by-one callers.
///
/// # Determinism
///
/// Pure function of `(attempt, error)`. Two calls with the same inputs
/// always return the same `RetryDecision`.
#[must_use]
pub fn decide_retry(attempt: u32, error: &OrchestratorError) -> RetryDecision {
    let backoff = compute_backoff(attempt);
    let telegram = match (classify_severity(attempt), should_emit_telegram(attempt)) {
        (Some(severity), true) => Some(TelegramEmit {
            severity,
            error_code: error.error_code(),
            stage: error.stage(),
        }),
        _ => None,
    };
    RetryDecision { backoff, telegram }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::csv_parser::CsvParseError;
    use crate::instrument::daily_universe::BuildError;
    use crate::instrument::fno_underlying_extractor::ExtractError;
    use crate::instrument::index_extractor::IndexExtractError;

    fn parse_err() -> OrchestratorError {
        OrchestratorError::Parse(CsvParseError::NotUtf8)
    }

    fn fno_err() -> OrchestratorError {
        OrchestratorError::FnoExtract(ExtractError::NoDerivativeRowsFound { total_rows: 0 })
    }

    fn index_err() -> OrchestratorError {
        OrchestratorError::IndexExtract(IndexExtractError::BseSensexMissing)
    }

    fn build_err() -> OrchestratorError {
        OrchestratorError::Build(BuildError::UniverseSizeOutOfBounds {
            actual: 50,
            min: 100,
            max: 400,
        })
    }

    #[test]
    fn attempt_zero_treated_as_first_silent_10s() {
        let d = decide_retry(0, &parse_err());
        assert_eq!(d.backoff, Duration::from_secs(10));
        assert!(d.telegram.is_none(), "attempt 0 must be silent");
    }

    #[test]
    fn attempts_one_through_three_are_silent() {
        for attempt in 1..=3 {
            let d = decide_retry(attempt, &parse_err());
            assert!(
                d.telegram.is_none(),
                "attempt {attempt} must be silent per §4 ladder",
            );
        }
    }

    #[test]
    fn silent_backoff_schedule_matches_policy() {
        assert_eq!(
            decide_retry(1, &parse_err()).backoff,
            Duration::from_secs(10)
        );
        assert_eq!(
            decide_retry(2, &parse_err()).backoff,
            Duration::from_secs(20)
        );
        assert_eq!(
            decide_retry(3, &parse_err()).backoff,
            Duration::from_secs(40)
        );
    }

    #[test]
    fn attempt_four_emits_info_every_attempt() {
        let d = decide_retry(4, &parse_err());
        let emit = d.telegram.expect("attempt 4 must emit");
        assert_eq!(emit.severity, Severity::Info);
        assert_eq!(
            emit.error_code,
            ErrorCode::InstrFetch02SchemaValidationFailed
        );
        assert_eq!(emit.stage, "csv_parser");
    }

    #[test]
    fn attempts_four_through_ten_emit_info_on_every_attempt() {
        for attempt in 4..=10 {
            let d = decide_retry(attempt, &parse_err());
            let emit = d
                .telegram
                .unwrap_or_else(|| panic!("attempt {attempt} must emit"));
            assert_eq!(
                emit.severity,
                Severity::Info,
                "attempt {attempt} must be Info-tier",
            );
        }
    }

    #[test]
    fn high_tier_emits_every_fifth_starting_from_eleven() {
        // Per `should_emit_telegram`: offset = attempt - (INFO_THRESHOLD + 1),
        // emit iff offset % ESCALATED_CADENCE == 0. For HIGH band that's
        // attempts 11, 16 (within 11..=20).
        for attempt in 11..=20 {
            let d = decide_retry(attempt, &parse_err());
            let expected = matches!(attempt, 11 | 16);
            assert_eq!(
                d.telegram.is_some(),
                expected,
                "attempt {attempt} emit expectation",
            );
            if let Some(emit) = d.telegram {
                assert_eq!(emit.severity, Severity::High);
            }
        }
    }

    #[test]
    fn critical_tier_emits_every_fifth_in_2126_31_etc() {
        // For CRITICAL band that's attempts 21, 26, 31, 36 within 21..=40.
        for attempt in 21..=40 {
            let d = decide_retry(attempt, &parse_err());
            let expected = matches!(attempt, 21 | 26 | 31 | 36);
            assert_eq!(
                d.telegram.is_some(),
                expected,
                "attempt {attempt} emit expectation",
            );
            if let Some(emit) = d.telegram {
                assert_eq!(emit.severity, Severity::Critical);
            }
        }
    }

    #[test]
    fn backoff_caps_at_300s_for_large_attempts() {
        assert_eq!(
            decide_retry(50, &parse_err()).backoff,
            Duration::from_secs(300),
        );
        assert_eq!(
            decide_retry(u32::MAX, &parse_err()).backoff,
            Duration::from_secs(300),
        );
    }

    #[test]
    fn error_code_mapping_per_orchestrator_taxonomy() {
        let parse_decision = decide_retry(5, &parse_err());
        assert_eq!(
            parse_decision.telegram.unwrap().error_code,
            ErrorCode::InstrFetch02SchemaValidationFailed,
        );

        let fno_decision = decide_retry(5, &fno_err());
        assert_eq!(
            fno_decision.telegram.unwrap().error_code,
            ErrorCode::InstrFetch03DanglingReferences,
        );

        let index_decision = decide_retry(5, &index_err());
        assert_eq!(
            index_decision.telegram.unwrap().error_code,
            ErrorCode::InstrFetch02SchemaValidationFailed,
        );

        let build_decision = decide_retry(5, &build_err());
        assert_eq!(
            build_decision.telegram.unwrap().error_code,
            ErrorCode::InstrFetch04UniverseSizeOutOfBounds,
        );
    }

    #[test]
    fn stage_label_mapping_per_orchestrator_taxonomy() {
        assert_eq!(
            decide_retry(5, &parse_err()).telegram.unwrap().stage,
            "csv_parser",
        );
        assert_eq!(
            decide_retry(5, &fno_err()).telegram.unwrap().stage,
            "fno_underlying_extractor",
        );
        assert_eq!(
            decide_retry(5, &index_err()).telegram.unwrap().stage,
            "index_extractor",
        );
        assert_eq!(
            decide_retry(5, &build_err()).telegram.unwrap().stage,
            "daily_universe_builder",
        );
    }

    #[test]
    fn decision_is_deterministic() {
        let a = decide_retry(7, &fno_err());
        let b = decide_retry(7, &fno_err());
        assert_eq!(a, b, "same inputs MUST yield identical decisions");
    }

    #[test]
    fn severity_never_downgrades_across_attempts() {
        // Walk the ladder; once we have entered a tier we never see a
        // lower-priority severity for a subsequent emission.
        let mut last_seen: Option<Severity> = None;
        for attempt in 1..=50u32 {
            if let Some(emit) = decide_retry(attempt, &parse_err()).telegram {
                if let Some(prev) = last_seen {
                    assert!(
                        emit.severity >= prev,
                        "attempt {attempt}: severity {:?} regressed below previous {:?}",
                        emit.severity,
                        prev,
                    );
                }
                last_seen = Some(emit.severity);
            }
        }
    }

    #[test]
    fn backoff_monotonically_grows_then_caps() {
        let mut prev = decide_retry(1, &parse_err()).backoff;
        for attempt in 2..=10u32 {
            let next = decide_retry(attempt, &parse_err()).backoff;
            assert!(
                next >= prev,
                "attempt {attempt}: backoff {next:?} smaller than previous {prev:?}",
            );
            prev = next;
        }
    }
}
