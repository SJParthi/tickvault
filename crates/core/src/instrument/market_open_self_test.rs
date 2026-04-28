//! Wave 3 Item 12 — Market-open self-test.
//!
//! At 09:16:00 IST on every trading day (after the 09:13 anchor and the
//! 09:15:30 streaming heartbeat have settled), this pure-logic evaluator
//! answers the operator's "is anything broken right now?" question with a
//! single tri-state verdict: **Passed**, **Degraded**, or **Critical**.
//!
//! The evaluator is a pure function so it can be unit-tested without a
//! running QuestDB / WebSocket pool / tokio runtime. The boot scheduler
//! samples the live state and hands a populated [`MarketOpenSelfTestInputs`]
//! to [`evaluate_self_test`].
//!
//! # Outcome routing
//!
//! | Outcome | Severity | ErrorCode | Telegram |
//! |---|---|---|---|
//! | `Passed` | Info | SELFTEST-01 | optional positive ping |
//! | `Degraded` | High | SELFTEST-02 | always |
//! | `Critical` | Critical | SELFTEST-02 | always (also SMS via SNS) |
//!
//! # Critical vs Degraded
//!
//! Per SCOPE §12.2: any of these failing alone yields `Critical` because
//! the system **cannot trade** until they are fixed:
//!
//! * `main_feed_active == 0` — no live ticks at all
//! * `questdb_connected == false` — every persist attempt fails
//! * `token_expiry_headroom < 4h` — JWT will die mid-session
//!
//! Anything else failing yields `Degraded` — trading can continue but
//! the operator should investigate before next market open.
//!
//! # Why no kill-switch coupling here
//!
//! SCOPE §12 does not request the legacy plan's `kill_switch.activate()`
//! call on failure. The operator pages on `[CRITICAL]` and decides; the
//! kill switch stays under operator control. Coupling them would conflate
//! "self-test detected a problem" with "stop trading" which is a
//! risk-engine decision, not a self-test decision.

use tickvault_common::error_code::ErrorCode;

/// All seven inputs the self-test consumes. Populated by the boot
/// scheduler from live counters before each evaluation; the evaluator
/// itself does not touch any global state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarketOpenSelfTestInputs {
    /// Number of main-feed WS connections currently in `Connected` state.
    /// Expected: 5 (the Dhan account cap).
    pub main_feed_active: usize,
    /// Number of depth-20 WS connections currently active.
    /// Expected: 4 today (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY) — but
    /// after the 2026-04-25 universe rebuild may be 2. The check is
    /// "non-zero", not "exactly N".
    pub depth_20_active: usize,
    /// Number of depth-200 WS connections currently active.
    /// Expected: ≥ 1 (NIFTY ATM CE/PE + BANKNIFTY ATM CE/PE = up to 4).
    pub depth_200_active: usize,
    /// True iff the order-update WebSocket is connected.
    pub order_update_active: bool,
    /// True iff the tick-processing pipeline task is alive (heartbeat
    /// gauge `tv_pipeline_active == 1`).
    pub pipeline_active: bool,
    /// Age in seconds of the most recent tick observed by the pipeline.
    /// Stale tick (> 60s during market hours) signals silent socket.
    pub last_tick_age_secs: u64,
    /// True iff the most recent QuestDB health probe succeeded.
    pub questdb_connected: bool,
    /// Seconds remaining until the JWT auth token expires.
    /// Less than 4h (14_400s) is a critical failure.
    pub token_expiry_headroom_secs: u64,
}

/// Tri-state outcome of one self-test evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketOpenSelfTestOutcome {
    /// All seven sub-checks green. Severity::Info.
    Passed {
        /// Always 7 in the current schema; carried for forward
        /// compatibility if the SCOPE ever adds checks.
        checks_passed: usize,
    },
    /// One or more non-critical sub-checks failed. Severity::High.
    Degraded {
        checks_passed: usize,
        checks_failed: usize,
        failed: Vec<&'static str>,
    },
    /// At least one critical sub-check failed (no main feed / QuestDB
    /// down / token expired). Severity::Critical.
    Critical {
        checks_failed: usize,
        failed: Vec<&'static str>,
    },
}

impl MarketOpenSelfTestOutcome {
    /// ErrorCode emitted alongside this outcome for log/triage routing.
    #[must_use]
    pub const fn error_code(&self) -> ErrorCode {
        match self {
            Self::Passed { .. } => ErrorCode::Selftest01Passed,
            Self::Degraded { .. } | Self::Critical { .. } => ErrorCode::Selftest02Failed,
        }
    }

    /// Stable wire-format string used in audit-row `outcome` column.
    #[must_use]
    pub const fn outcome_str(&self) -> &'static str {
        match self {
            Self::Passed { .. } => "passed",
            Self::Degraded { .. } => "degraded",
            Self::Critical { .. } => "critical",
        }
    }
}

/// Total number of sub-checks the evaluator runs. Wire-stable: the
/// audit `detail` column references this count, and the SCOPE §12.5
/// test asserts it.
///
/// SCOPE §12.1 lists 7 ordinal sub-checks but the evaluator runs 8
/// `if` arms (it splits "depth" into `depth_20_active` and
/// `depth_200_active` because those WS pools are independent). This
/// constant matches the implementation, not the SCOPE wording.
/// Adversarial review (hot-path-reviewer, 2026-04-28) caught the
/// off-by-one between `7` and the actual `8` `if` checks.
pub const TOTAL_SUB_CHECKS: usize = 8;

/// Token-expiry headroom threshold below which the self-test fires
/// `Critical`. 4 hours = 14_400 seconds. Matches the existing
/// `force_renewal_if_stale(threshold_secs = 14400)` used by the
/// post-sleep wake-up path (AUTH-GAP-03).
pub const TOKEN_EXPIRY_HEADROOM_CRITICAL_SECS: u64 = 4 * 3600;

/// Recent-tick threshold above which the self-test fires `Degraded`.
/// 60 seconds during market hours is the operator's "silent socket"
/// boundary used by the pool watchdog.
pub const RECENT_TICK_DEGRADED_THRESHOLD_SECS: u64 = 60;

/// Names of the three sub-checks whose failure escalates the outcome
/// to `Critical`. Listed here as a single source of truth so the unit
/// tests can verify the escalation semantics.
pub const CRITICAL_CHECK_NAMES: &[&str] = &[
    "main_feed_active",
    "questdb_connected",
    "token_expiry_headroom",
];

/// Pure evaluator. Hot-path-clean: no allocation when `Passed`; one
/// `Vec<&'static str>` allocation only when at least one check fails
/// (acceptable — fires once per trading day).
#[must_use]
pub fn evaluate_self_test(inputs: &MarketOpenSelfTestInputs) -> MarketOpenSelfTestOutcome {
    // Run all seven checks; collect names of the failed ones. Static
    // strings only — never include user-controllable data.
    let mut failed: Vec<&'static str> = Vec::new();

    if inputs.main_feed_active == 0 {
        failed.push("main_feed_active");
    }
    if inputs.depth_20_active == 0 {
        failed.push("depth_20_active");
    }
    if inputs.depth_200_active == 0 {
        failed.push("depth_200_active");
    }
    if !inputs.order_update_active {
        failed.push("order_update_active");
    }
    if !inputs.pipeline_active {
        failed.push("pipeline_active");
    }
    if inputs.last_tick_age_secs > RECENT_TICK_DEGRADED_THRESHOLD_SECS {
        failed.push("recent_tick");
    }
    if !inputs.questdb_connected {
        failed.push("questdb_connected");
    }
    if inputs.token_expiry_headroom_secs < TOKEN_EXPIRY_HEADROOM_CRITICAL_SECS {
        failed.push("token_expiry_headroom");
    }

    let checks_failed = failed.len();
    let checks_passed = TOTAL_SUB_CHECKS.saturating_sub(checks_failed);

    if failed.is_empty() {
        return MarketOpenSelfTestOutcome::Passed {
            checks_passed: TOTAL_SUB_CHECKS,
        };
    }

    let any_critical = failed
        .iter()
        .any(|name| CRITICAL_CHECK_NAMES.contains(name));
    if any_critical {
        MarketOpenSelfTestOutcome::Critical {
            checks_failed,
            failed,
        }
    } else {
        MarketOpenSelfTestOutcome::Degraded {
            checks_passed,
            checks_failed,
            failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All-green inputs used as a baseline by the failure-injection tests.
    fn green_inputs() -> MarketOpenSelfTestInputs {
        MarketOpenSelfTestInputs {
            main_feed_active: 5,
            depth_20_active: 4,
            depth_200_active: 2,
            order_update_active: true,
            pipeline_active: true,
            last_tick_age_secs: 10,
            questdb_connected: true,
            token_expiry_headroom_secs: 5 * 3600,
        }
    }

    #[test]
    fn test_self_test_passes_when_all_seven_checks_green() {
        let outcome = evaluate_self_test(&green_inputs());
        assert_eq!(
            outcome,
            MarketOpenSelfTestOutcome::Passed { checks_passed: 8 }
        );
        assert_eq!(outcome.error_code(), ErrorCode::Selftest01Passed);
        assert_eq!(outcome.outcome_str(), "passed");
    }

    #[test]
    fn test_self_test_critical_when_main_feed_zero() {
        let mut inputs = green_inputs();
        inputs.main_feed_active = 0;
        match evaluate_self_test(&inputs) {
            MarketOpenSelfTestOutcome::Critical { failed, .. } => {
                assert!(failed.contains(&"main_feed_active"));
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    #[test]
    fn test_self_test_critical_when_questdb_disconnected() {
        let mut inputs = green_inputs();
        inputs.questdb_connected = false;
        match evaluate_self_test(&inputs) {
            MarketOpenSelfTestOutcome::Critical { failed, .. } => {
                assert!(failed.contains(&"questdb_connected"));
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    #[test]
    fn test_self_test_critical_when_token_expired() {
        let mut inputs = green_inputs();
        // 3h headroom is below the 4h critical threshold.
        inputs.token_expiry_headroom_secs = 3 * 3600;
        match evaluate_self_test(&inputs) {
            MarketOpenSelfTestOutcome::Critical { failed, .. } => {
                assert!(failed.contains(&"token_expiry_headroom"));
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    #[test]
    fn test_self_test_degraded_when_pipeline_inactive() {
        let mut inputs = green_inputs();
        inputs.pipeline_active = false;
        match evaluate_self_test(&inputs) {
            MarketOpenSelfTestOutcome::Degraded {
                failed,
                checks_passed,
                checks_failed,
            } => {
                assert!(failed.contains(&"pipeline_active"));
                assert_eq!(checks_failed, 1);
                assert_eq!(checks_passed, 7);
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[test]
    fn test_self_test_degraded_when_no_recent_tick() {
        let mut inputs = green_inputs();
        // 120s > 60s threshold but no critical checks failed.
        inputs.last_tick_age_secs = 120;
        match evaluate_self_test(&inputs) {
            MarketOpenSelfTestOutcome::Degraded { failed, .. } => {
                assert!(failed.contains(&"recent_tick"));
                // Sanity: no critical names slipped in.
                for name in CRITICAL_CHECK_NAMES {
                    assert!(
                        !failed.contains(name),
                        "degraded outcome must not list critical names; got {failed:?}"
                    );
                }
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    /// Mixed failure: one critical + one non-critical → outcome is Critical
    /// (any critical wins). Pinned because the SCOPE wording is ambiguous;
    /// the conservative engineering call is "any critical → Critical".
    #[test]
    fn test_self_test_critical_wins_over_degraded() {
        let mut inputs = green_inputs();
        inputs.main_feed_active = 0; // critical
        inputs.last_tick_age_secs = 200; // degraded-only
        match evaluate_self_test(&inputs) {
            MarketOpenSelfTestOutcome::Critical { failed, .. } => {
                assert!(failed.contains(&"main_feed_active"));
                assert!(failed.contains(&"recent_tick"));
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    /// Constants pinned so a regression in the threshold can't sneak in
    /// without flipping a test red.
    #[test]
    fn test_self_test_constants_pinned() {
        assert_eq!(TOTAL_SUB_CHECKS, 8);
        assert_eq!(TOKEN_EXPIRY_HEADROOM_CRITICAL_SECS, 14_400);
        assert_eq!(RECENT_TICK_DEGRADED_THRESHOLD_SECS, 60);
        assert_eq!(CRITICAL_CHECK_NAMES.len(), 3);
    }

    #[test]
    fn test_outcome_str_is_stable() {
        assert_eq!(
            MarketOpenSelfTestOutcome::Passed { checks_passed: 7 }.outcome_str(),
            "passed"
        );
        assert_eq!(
            MarketOpenSelfTestOutcome::Degraded {
                checks_passed: 6,
                checks_failed: 1,
                failed: vec!["pipeline_active"],
            }
            .outcome_str(),
            "degraded"
        );
        assert_eq!(
            MarketOpenSelfTestOutcome::Critical {
                checks_failed: 1,
                failed: vec!["main_feed_active"],
            }
            .outcome_str(),
            "critical"
        );
    }
}
