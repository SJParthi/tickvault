//! Phase 2 readiness pre-flight check (PR-G, 2026-05-02).
//!
//! Fires 1 second after `Phase2Complete` at 09:13:01 IST. Validates 11
//! forward-looking pre-conditions that predict success of the upcoming
//! market-open milestones:
//!
//! - 09:15:00 IST — NSE opens, first ticks should stream
//! - 09:15:30 IST — `MarketOpenStreamingConfirmation` Telegram fires
//! - 09:16:30 IST — `SELFTEST-01` market-open self-test runs
//!
//! The pre-flight buys the operator ~2 minutes of warning if anything is
//! pre-existing-broken. If a check fails, the operator gets a Critical
//! Telegram BEFORE market open (at 09:13:01) instead of after (at 09:16:30).
//!
//! ## Design — pure-logic classifier
//!
//! The classifier itself takes a [`ReadinessInputs`] struct of already-
//! gathered values and returns a [`ReadinessOutcome`]. No I/O, no async,
//! no concurrency. The boot wiring in `main.rs` is responsible for
//! gathering the inputs from live state (pool health gauges, atomic
//! counters, etc.) and persisting the result via the audit writer.
//!
//! This split makes the classifier:
//! - Trivially testable (no mocks, no fixtures, no tokio runtime)
//! - Cheap to call (~100 ns total)
//! - Easy to reason about — every threshold is a named constant
//!
//! ## The 11 checks — fail predicate + reason
//!
//! | # | Check | Fails when | Predicts |
//! |---|---|---|---|
//! | 1 | `token_expiry_headroom` | `expiry_secs < MIN_TOKEN_HEADROOM_SECS` (4h) | Mid-session DH-901 cascade |
//! | 2 | `main_feed_pool` | `connected_count != EXPECTED_MAIN_FEED_CONNS` (5) | Ticks won't flow at 09:15 |
//! | 3 | `depth_20_pool` | `connected_count != EXPECTED_DEPTH_20_CONNS` (4) | Depth-20 ATM±24 missing |
//! | 4 | `depth_200_pool` | `connected_count != EXPECTED_DEPTH_200_CONNS` (5) | Depth-200 frames missing |
//! | 5 | `order_update_ws` | `!connected \|\| heartbeat_age_secs > MAX_HEARTBEAT_AGE` | Order events not routed |
//! | 6 | `questdb_ilp` | `!tcp_reachable \|\| last_write_age_secs > MAX_WRITE_AGE` | Persistence cascade |
//! | 7 | `preopen_buffer_coverage` | `coverage_pct < MIN_BUFFER_COVERAGE_PCT` (95) | Phase 2 ATM math used stale data |
//! | 8 | `phase2_plan_quality` | `added_count == 0 \|\| skipped_pct > MAX_SKIP_PCT` (5) | Stock F&O subs incomplete |
//! | 9 | `subscribe_ack_rate` | `acked < total` | Dhan partially rejected |
//! | 10 | `rescue_ring_health` | `used_pct > MAX_RING_USED_PCT` (10) | Burst overflow risk |
//! | 11 | `composite_slo_score` | `score < SLO_HEALTHY_THRESHOLD` (0.95) | Hidden upstream issue |

use std::fmt;

/// Minimum token-expiry headroom (seconds) for the readiness check to
/// pass check #1. 4 hours = trading session length + 30 min margin.
pub const MIN_TOKEN_HEADROOM_SECS: i64 = 14_400;

/// Expected number of main-feed WebSocket connections in `Connected` state.
pub const EXPECTED_MAIN_FEED_CONNS: u32 = 5;

/// Expected number of depth-20 WebSocket connections in `Connected` state.
/// Wave 5 narrowed from 4 to 4 dynamic + 1 fixed = 5 total; legacy 4 kept
/// as the readiness threshold until Wave 5 finalizes.
pub const EXPECTED_DEPTH_20_CONNS: u32 = 4;

/// Expected number of depth-200 WebSocket connections in `Connected` state.
/// Per Wave 5 dynamic pool + Dhan 5-conn cap.
pub const EXPECTED_DEPTH_200_CONNS: u32 = 5;

/// Maximum acceptable order-update heartbeat age (seconds) — Dhan's
/// order-update server can go silent on idle accounts; 14400s = 4h
/// matches the in-process activity watchdog.
pub const MAX_ORDER_UPDATE_HEARTBEAT_AGE_SECS: u64 = 14_400;

/// Maximum acceptable QuestDB last-write age (seconds). At 09:13:01 the
/// instrument-build-metadata write should be < 60s old.
pub const MAX_QUESTDB_LAST_WRITE_AGE_SECS: u64 = 60;

/// Minimum pre-open buffer coverage (percent) of F&O stocks with at
/// least one captured 09:00–09:12 IST price slot.
pub const MIN_PREOPEN_BUFFER_COVERAGE_PCT: u8 = 95;

/// Maximum acceptable Phase 2 stocks skipped percentage (out of total
/// universe). > 5% indicates the buffer was missing a meaningful chunk.
pub const MAX_PHASE2_SKIP_PCT: u8 = 5;

/// Maximum acceptable rescue ring used percentage at 09:13. Higher
/// than this means we're already at risk of overflow when the 09:15
/// tick burst arrives.
pub const MAX_RESCUE_RING_USED_PCT: u8 = 10;

/// Composite SLO score threshold for healthy state. Same threshold the
/// 10-second SLO scheduler uses to flip between SLO-01 and SLO-02.
pub const SLO_HEALTHY_THRESHOLD: f64 = 0.95;

/// Minutes between 09:13:01 IST (when the check fires) and 09:15:00 IST
/// (NSE market open). Hard-coded because the check fires only at this
/// exact instant — no need to compute dynamically.
pub const MINUTES_TO_MARKET_OPEN_AT_FIRE_TIME: u32 = 2;

/// Snapshot of every input the readiness classifier needs.
///
/// All fields are simple values to make the classifier purely synchronous
/// and trivially testable. Boot wiring is responsible for gathering these
/// from live state (pool health, atomic counters, ILP probe results).
#[derive(Debug, Clone)]
pub struct ReadinessInputs {
    /// Seconds until the JWT expires. Negative if already expired.
    pub token_expiry_secs: i64,
    /// Number of main-feed WS connections in `Connected` state.
    pub main_feed_connected: u32,
    /// Number of depth-20 WS connections in `Connected` state.
    pub depth_20_connected: u32,
    /// Number of depth-200 WS connections in `Connected` state.
    pub depth_200_connected: u32,
    /// Whether the order-update WebSocket is connected.
    pub order_update_connected: bool,
    /// Seconds since the last order-update heartbeat. `u64::MAX` if
    /// none received yet.
    pub order_update_heartbeat_age_secs: u64,
    /// Whether QuestDB ILP TCP port is reachable.
    pub questdb_ilp_reachable: bool,
    /// Seconds since the last successful QuestDB write.
    pub questdb_last_write_age_secs: u64,
    /// Total F&O stocks in the universe (denominator for buffer coverage).
    pub total_fno_stocks: u32,
    /// F&O stocks that captured at least one 09:00–09:12 price.
    pub preopen_buffer_stocks_with_price: u32,
    /// Phase 2 added_count (from `Phase2Complete` event).
    pub phase2_added_count: u32,
    /// Phase 2 skipped_no_price + skipped_no_expiry sum.
    pub phase2_skipped_count: u32,
    /// Total subscribe batches dispatched at boot + Phase 2.
    pub subscribe_total_batches: u32,
    /// Subscribe batches that received an `OK` ack from Dhan.
    pub subscribe_acked_batches: u32,
    /// Rescue ring fill percentage (0..=100).
    pub rescue_ring_used_pct: u8,
    /// Composite SLO score (0.0..=1.0).
    pub composite_slo_score: f64,
}

/// Result of a single check within the readiness pre-flight.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    /// Stable identifier (matches the `check` Prometheus label and the
    /// `check_name` audit-table column).
    pub name: &'static str,
    /// Whether the check passed.
    pub pass: bool,
    /// Operator-readable detail string of form `"expected=X observed=Y"`.
    pub detail: String,
}

/// Outcome of the entire readiness pre-flight.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadinessOutcome {
    /// All 11 checks passed. Operator gets `Phase2ReadinessPassed` Info.
    Passed {
        /// All 11 results, in fixed order.
        results: Vec<CheckResult>,
        /// Composite SLO score (echoed for the Telegram payload).
        slo_score: f64,
    },
    /// At least 1 check failed. Operator gets `Phase2ReadinessFailed`
    /// Critical with `code = ErrorCode::Phase2Ready01PreflightFailed`.
    Failed {
        /// All 11 results, in fixed order.
        results: Vec<CheckResult>,
        /// Names of failing checks (subset of `results` where pass=false).
        failed_names: Vec<String>,
        /// `expected/observed` strings for each failing check.
        failed_details: Vec<String>,
    },
}

impl ReadinessOutcome {
    /// Returns `true` when every check passed.
    #[must_use]
    pub fn is_passed(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }

    /// Returns the full result list regardless of outcome.
    #[must_use]
    pub fn results(&self) -> &[CheckResult] {
        match self {
            Self::Passed { results, .. } | Self::Failed { results, .. } => results,
        }
    }
}

impl fmt::Display for ReadinessOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passed { results, slo_score } => {
                write!(
                    f,
                    "PASSED ({n}/11 checks, SLO={slo_score:.3})",
                    n = results.len()
                )
            }
            Self::Failed { failed_names, .. } => {
                write!(f, "FAILED [{}]", failed_names.join(", "))
            }
        }
    }
}

/// Pure classifier — given snapshot inputs, return outcome.
///
/// O(1) cost. Fully synchronous. Used by both the boot path and tests.
#[must_use]
pub fn classify_readiness(inputs: &ReadinessInputs) -> ReadinessOutcome {
    let results = vec![
        check_token_expiry_headroom(inputs),
        check_main_feed_pool(inputs),
        check_depth_20_pool(inputs),
        check_depth_200_pool(inputs),
        check_order_update_ws(inputs),
        check_questdb_ilp(inputs),
        check_preopen_buffer_coverage(inputs),
        check_phase2_plan_quality(inputs),
        check_subscribe_ack_rate(inputs),
        check_rescue_ring_health(inputs),
        check_composite_slo_score(inputs),
    ];

    let failed: Vec<&CheckResult> = results.iter().filter(|r| !r.pass).collect();

    if failed.is_empty() {
        ReadinessOutcome::Passed {
            results,
            slo_score: inputs.composite_slo_score,
        }
    } else {
        let failed_names = failed.iter().map(|r| r.name.to_string()).collect();
        let failed_details = failed
            .iter()
            .map(|r| format!("{}: {}", r.name, r.detail))
            .collect();
        ReadinessOutcome::Failed {
            results,
            failed_names,
            failed_details,
        }
    }
}

fn check_token_expiry_headroom(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.token_expiry_secs >= MIN_TOKEN_HEADROOM_SECS;
    CheckResult {
        name: "token_expiry_headroom",
        pass,
        detail: format!(
            "expected>={MIN_TOKEN_HEADROOM_SECS}s observed={}s",
            inputs.token_expiry_secs
        ),
    }
}

fn check_main_feed_pool(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.main_feed_connected == EXPECTED_MAIN_FEED_CONNS;
    CheckResult {
        name: "main_feed_pool",
        pass,
        detail: format!(
            "expected={EXPECTED_MAIN_FEED_CONNS} observed={}",
            inputs.main_feed_connected
        ),
    }
}

fn check_depth_20_pool(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.depth_20_connected == EXPECTED_DEPTH_20_CONNS;
    CheckResult {
        name: "depth_20_pool",
        pass,
        detail: format!(
            "expected={EXPECTED_DEPTH_20_CONNS} observed={}",
            inputs.depth_20_connected
        ),
    }
}

fn check_depth_200_pool(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.depth_200_connected == EXPECTED_DEPTH_200_CONNS;
    CheckResult {
        name: "depth_200_pool",
        pass,
        detail: format!(
            "expected={EXPECTED_DEPTH_200_CONNS} observed={}",
            inputs.depth_200_connected
        ),
    }
}

fn check_order_update_ws(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.order_update_connected
        && inputs.order_update_heartbeat_age_secs <= MAX_ORDER_UPDATE_HEARTBEAT_AGE_SECS;
    CheckResult {
        name: "order_update_ws",
        pass,
        detail: format!(
            "connected={} heartbeat_age={}s max={}s",
            inputs.order_update_connected,
            inputs.order_update_heartbeat_age_secs,
            MAX_ORDER_UPDATE_HEARTBEAT_AGE_SECS
        ),
    }
}

fn check_questdb_ilp(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.questdb_ilp_reachable
        && inputs.questdb_last_write_age_secs <= MAX_QUESTDB_LAST_WRITE_AGE_SECS;
    CheckResult {
        name: "questdb_ilp",
        pass,
        detail: format!(
            "reachable={} last_write_age={}s max={}s",
            inputs.questdb_ilp_reachable,
            inputs.questdb_last_write_age_secs,
            MAX_QUESTDB_LAST_WRITE_AGE_SECS
        ),
    }
}

fn check_preopen_buffer_coverage(inputs: &ReadinessInputs) -> CheckResult {
    let coverage_pct = if inputs.total_fno_stocks == 0 {
        0
    } else {
        let raw = (u64::from(inputs.preopen_buffer_stocks_with_price) * 100)
            / u64::from(inputs.total_fno_stocks);
        u8::try_from(raw.min(100)).unwrap_or(100)
    };
    let pass = coverage_pct >= MIN_PREOPEN_BUFFER_COVERAGE_PCT;
    CheckResult {
        name: "preopen_buffer_coverage",
        pass,
        detail: format!(
            "expected>={MIN_PREOPEN_BUFFER_COVERAGE_PCT}% observed={coverage_pct}% \
             ({}/{})",
            inputs.preopen_buffer_stocks_with_price, inputs.total_fno_stocks
        ),
    }
}

fn check_phase2_plan_quality(inputs: &ReadinessInputs) -> CheckResult {
    let total = inputs
        .phase2_added_count
        .saturating_add(inputs.phase2_skipped_count);
    let skipped_pct = if total == 0 {
        // No additions and no skips means Phase 2 didn't dispatch — fail.
        100
    } else {
        let raw = (u64::from(inputs.phase2_skipped_count) * 100) / u64::from(total);
        u8::try_from(raw.min(100)).unwrap_or(100)
    };
    let pass = inputs.phase2_added_count > 0 && skipped_pct <= MAX_PHASE2_SKIP_PCT;
    CheckResult {
        name: "phase2_plan_quality",
        pass,
        detail: format!(
            "added={} skipped={} skipped_pct={}% max={}%",
            inputs.phase2_added_count,
            inputs.phase2_skipped_count,
            skipped_pct,
            MAX_PHASE2_SKIP_PCT
        ),
    }
}

fn check_subscribe_ack_rate(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.subscribe_total_batches > 0
        && inputs.subscribe_acked_batches == inputs.subscribe_total_batches;
    CheckResult {
        name: "subscribe_ack_rate",
        pass,
        detail: format!(
            "acked={}/{}",
            inputs.subscribe_acked_batches, inputs.subscribe_total_batches
        ),
    }
}

fn check_rescue_ring_health(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.rescue_ring_used_pct <= MAX_RESCUE_RING_USED_PCT;
    CheckResult {
        name: "rescue_ring_health",
        pass,
        detail: format!(
            "used={}% max={}%",
            inputs.rescue_ring_used_pct, MAX_RESCUE_RING_USED_PCT
        ),
    }
}

fn check_composite_slo_score(inputs: &ReadinessInputs) -> CheckResult {
    let pass = inputs.composite_slo_score >= SLO_HEALTHY_THRESHOLD;
    CheckResult {
        name: "composite_slo_score",
        pass,
        detail: format!(
            "score={:.3} threshold={SLO_HEALTHY_THRESHOLD:.3}",
            inputs.composite_slo_score
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a default-healthy inputs struct — every check will pass.
    fn healthy_inputs() -> ReadinessInputs {
        ReadinessInputs {
            token_expiry_secs: 23 * 3600, // 23h headroom
            main_feed_connected: EXPECTED_MAIN_FEED_CONNS,
            depth_20_connected: EXPECTED_DEPTH_20_CONNS,
            depth_200_connected: EXPECTED_DEPTH_200_CONNS,
            order_update_connected: true,
            order_update_heartbeat_age_secs: 30,
            questdb_ilp_reachable: true,
            questdb_last_write_age_secs: 5,
            total_fno_stocks: 209,
            preopen_buffer_stocks_with_price: 209,
            phase2_added_count: 22_000,
            phase2_skipped_count: 0,
            subscribe_total_batches: 230,
            subscribe_acked_batches: 230,
            rescue_ring_used_pct: 0,
            composite_slo_score: 1.0,
        }
    }

    #[test]
    fn classify_readiness_returns_passed_for_healthy_inputs() {
        let outcome = classify_readiness(&healthy_inputs());
        assert!(outcome.is_passed(), "healthy inputs must pass: {outcome}");
        assert_eq!(outcome.results().len(), 11);
        assert!(outcome.results().iter().all(|r| r.pass));
    }

    #[test]
    fn classify_readiness_fails_when_token_expiry_below_4h() {
        let mut inputs = healthy_inputs();
        inputs.token_expiry_secs = 3 * 3600; // 3h, below 4h threshold
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
        if let ReadinessOutcome::Failed { failed_names, .. } = outcome {
            assert!(failed_names.contains(&"token_expiry_headroom".to_string()));
        }
    }

    #[test]
    fn classify_readiness_fails_when_main_feed_count_wrong() {
        let mut inputs = healthy_inputs();
        inputs.main_feed_connected = 3;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
        if let ReadinessOutcome::Failed {
            failed_names,
            failed_details,
            ..
        } = outcome
        {
            assert!(failed_names.contains(&"main_feed_pool".to_string()));
            assert!(failed_details.iter().any(|d| d.contains("observed=3")));
        }
    }

    #[test]
    fn classify_readiness_fails_when_depth_20_count_wrong() {
        let mut inputs = healthy_inputs();
        inputs.depth_20_connected = 2;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_depth_200_count_wrong() {
        let mut inputs = healthy_inputs();
        inputs.depth_200_connected = 0;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_order_update_disconnected() {
        let mut inputs = healthy_inputs();
        inputs.order_update_connected = false;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_order_update_heartbeat_too_old() {
        let mut inputs = healthy_inputs();
        inputs.order_update_heartbeat_age_secs = MAX_ORDER_UPDATE_HEARTBEAT_AGE_SECS + 1;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_questdb_unreachable() {
        let mut inputs = healthy_inputs();
        inputs.questdb_ilp_reachable = false;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_questdb_write_too_old() {
        let mut inputs = healthy_inputs();
        inputs.questdb_last_write_age_secs = MAX_QUESTDB_LAST_WRITE_AGE_SECS + 1;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_preopen_coverage_below_threshold() {
        let mut inputs = healthy_inputs();
        // 90% coverage, below 95% threshold
        inputs.preopen_buffer_stocks_with_price = 188;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_passes_at_exactly_95pct_buffer_coverage() {
        let mut inputs = healthy_inputs();
        // 95% of 209 = 198.55 → 198 stocks → 94.74% (rounds DOWN below)
        // So use 199/209 = 95.21% — at threshold inclusive.
        inputs.preopen_buffer_stocks_with_price = 199;
        inputs.total_fno_stocks = 209;
        let outcome = classify_readiness(&inputs);
        assert!(
            outcome.is_passed(),
            "199/209=95.21% must pass at threshold: {outcome}"
        );
    }

    #[test]
    fn classify_readiness_handles_zero_total_fno_stocks_without_panic() {
        // Defensive: division-by-zero guard for the coverage calculation.
        let mut inputs = healthy_inputs();
        inputs.total_fno_stocks = 0;
        inputs.preopen_buffer_stocks_with_price = 0;
        let outcome = classify_readiness(&inputs);
        // With total=0, coverage is 0 -> fails coverage check
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_phase2_added_zero() {
        let mut inputs = healthy_inputs();
        inputs.phase2_added_count = 0;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_phase2_skip_pct_too_high() {
        let mut inputs = healthy_inputs();
        inputs.phase2_added_count = 80;
        inputs.phase2_skipped_count = 20; // 20% skipped, above 5% threshold
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_subscribe_ack_rate_below_100pct() {
        let mut inputs = healthy_inputs();
        inputs.subscribe_total_batches = 230;
        inputs.subscribe_acked_batches = 229; // one rejection
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_rescue_ring_above_10pct() {
        let mut inputs = healthy_inputs();
        inputs.rescue_ring_used_pct = MAX_RESCUE_RING_USED_PCT + 1;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_fails_when_slo_below_095() {
        let mut inputs = healthy_inputs();
        inputs.composite_slo_score = 0.94;
        let outcome = classify_readiness(&inputs);
        assert!(!outcome.is_passed());
    }

    #[test]
    fn classify_readiness_passes_at_exactly_slo_threshold() {
        let mut inputs = healthy_inputs();
        inputs.composite_slo_score = SLO_HEALTHY_THRESHOLD;
        let outcome = classify_readiness(&inputs);
        assert!(outcome.is_passed(), "score=0.95 must pass at threshold");
    }

    #[test]
    fn classify_readiness_aggregates_multiple_failures() {
        let mut inputs = healthy_inputs();
        inputs.main_feed_connected = 3;
        inputs.depth_200_connected = 2;
        inputs.composite_slo_score = 0.5;
        let outcome = classify_readiness(&inputs);
        if let ReadinessOutcome::Failed { failed_names, .. } = outcome {
            assert!(failed_names.len() >= 3);
            assert!(failed_names.contains(&"main_feed_pool".to_string()));
            assert!(failed_names.contains(&"depth_200_pool".to_string()));
            assert!(failed_names.contains(&"composite_slo_score".to_string()));
        } else {
            panic!("expected Failed outcome");
        }
    }

    #[test]
    fn check_result_has_stable_check_names() {
        // Ratchet: the check `name` strings are part of the operator API
        // (Telegram payload, audit table column, Prometheus label). They
        // MUST NOT silently change.
        let names: Vec<&str> = classify_readiness(&healthy_inputs())
            .results()
            .iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "token_expiry_headroom",
                "main_feed_pool",
                "depth_20_pool",
                "depth_200_pool",
                "order_update_ws",
                "questdb_ilp",
                "preopen_buffer_coverage",
                "phase2_plan_quality",
                "subscribe_ack_rate",
                "rescue_ring_health",
                "composite_slo_score",
            ]
        );
    }

    #[test]
    fn test_is_passed_returns_true_only_for_passed_variant() {
        let pass = classify_readiness(&healthy_inputs());
        assert!(pass.is_passed());

        let mut failed_inputs = healthy_inputs();
        failed_inputs.main_feed_connected = 0;
        let fail = classify_readiness(&failed_inputs);
        assert!(!fail.is_passed());
    }

    #[test]
    fn test_results_returns_all_eleven_check_results() {
        let outcome = classify_readiness(&healthy_inputs());
        let results = outcome.results();
        assert_eq!(results.len(), 11);
        // Failed variant must also expose the same 11 results.
        let mut bad = healthy_inputs();
        bad.main_feed_connected = 0;
        let bad_outcome = classify_readiness(&bad);
        assert_eq!(bad_outcome.results().len(), 11);
    }

    #[test]
    fn outcome_display_passed_includes_count_and_score() {
        let outcome = classify_readiness(&healthy_inputs());
        let s = outcome.to_string();
        assert!(s.contains("PASSED"));
        assert!(s.contains("11/11"));
    }

    #[test]
    fn outcome_display_failed_lists_failed_names() {
        let mut inputs = healthy_inputs();
        inputs.main_feed_connected = 0;
        let outcome = classify_readiness(&inputs);
        let s = outcome.to_string();
        assert!(s.contains("FAILED"));
        assert!(s.contains("main_feed_pool"));
    }

    #[test]
    fn minutes_to_market_open_constant_is_two() {
        // The check fires at 09:13:01 IST. Market opens at 09:15:00 IST.
        // Whole-number minutes = 2.
        assert_eq!(MINUTES_TO_MARKET_OPEN_AT_FIRE_TIME, 2);
    }

    #[test]
    fn slo_threshold_matches_slo_score_evaluator() {
        // The composite SLO evaluator (slo_score.rs) uses 0.95 as the
        // healthy threshold. Pre-flight must use the same value or
        // there's an inconsistency between the two layers.
        assert!((SLO_HEALTHY_THRESHOLD - 0.95).abs() < f64::EPSILON);
    }
}
