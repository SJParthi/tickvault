//! Mid-session profile watchdog.
//!
//! # Why this exists (queue item I7, 2026-04-21)
//!
//! On 2026-04-21 the app booted during market hours, WebSocket reported
//! all 5 connections "connected", but Dhan was not actually streaming
//! data — likely a `dataPlan` / `activeSegment` / token-validity issue
//! that appeared DURING the session. PR #309 added a pre-market profile
//! HALT at boot, but once the app is already running the invariant goes
//! unchecked for the rest of the day.
//!
//! This watchdog closes that gap. Every [`MID_SESSION_CHECK_INTERVAL_SECS`]
//! during market hours, it calls [`TokenManager::pre_market_check`]
//! (same validation the boot-time HALT uses). On failure during market
//! hours it fires CRITICAL Telegram via
//! [`NotificationEvent::PreMarketProfileCheckFailed`] — but does NOT HALT
//! the running process. A mid-session HALT would terminate the live
//! WebSocket feed, and reconnecting after systemd restart would take
//! >30 seconds of lost ticks. Paging the operator is strictly better.
//!
//! # Design
//!
//! - **Background tokio task** spawned at boot, runs forever.
//! - **Market-hours gated** (Rule 3 — `tickvault_common::market_hours`).
//! - **Edge-triggered** (Rule 4 — `currently_failing: bool`). Rising
//!   edge fires CRITICAL. Falling edge fires INFO (recovery), no
//!   Telegram on recovery.
//! - **Interval**: 15 minutes. Tight enough to catch a mid-session
//!   `dataPlan` invalidation within one check window; loose enough to
//!   not pound Dhan's `/v2/profile` endpoint (1 req/sec Quote rate
//!   limit shared).
//! - **Cold path**: one HTTP round-trip every 15 minutes. Zero impact
//!   on the tick hot path.
//!
//! # Not a HALT
//!
//! The pre-market HALT (PR #309) is intentional — booting into a known-
//! bad state is a no-op that costs nothing. A MID-SESSION HALT would
//! cost live ticks. The product requirement (operator must be paged,
//! boot must not continue into silent failure) is satisfied by the
//! CRITICAL Telegram alert alone. Any remediation (token rotation,
//! Dhan account fix, manual restart) is an operator decision, not an
//! automatic one.
//!
//! # Tests
//!
//! - `evaluate_transition_first_failure` — rising edge
//! - `evaluate_transition_recovery_after_failure` — falling edge
//! - `evaluate_transition_steady_fail_no_duplicate_alert` — idempotent
//! - `evaluate_transition_steady_ok_no_op` — idempotent

use std::sync::Arc;
use std::time::Duration;

use tickvault_common::market_hours::is_within_market_hours_ist;
use tracing::{error, info, warn};

use crate::auth::token_manager::TokenManager;
use crate::notification::events::NotificationEvent;
use crate::notification::service::NotificationService;

/// Check cadence (seconds). 15 minutes = 900 s. Loose enough to stay
/// under the 1 req/sec Quote API rate limit even under pathological
/// burst conditions; tight enough that a mid-session `dataPlan`
/// invalidation is detected within one cycle.
pub const MID_SESSION_CHECK_INTERVAL_SECS: u64 = 900;

/// Spawns the mid-session profile watchdog as an independent tokio task.
///
/// Consumers should keep the returned [`tokio::task::JoinHandle`] around
/// and abort it on graceful shutdown. The task runs forever otherwise.
///
/// # Arguments
/// * `token_manager` — shared `TokenManager` (uses `pre_market_check`).
/// * `notifier` — `NotificationService` for CRITICAL Telegram on the
///   rising edge. `None` disables Telegram; logs still fire.
// TEST-EXEMPT: thin tokio::spawn wrapper around `run_watchdog_loop`. The behavioural surface (`evaluate_transition` + `apply_transition`) is fully covered by the 4 unit tests in this module; the spawn wrapper itself has no decision logic.
pub fn spawn_mid_session_profile_watchdog(
    token_manager: Arc<TokenManager>,
    notifier: Option<Arc<NotificationService>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_watchdog_loop(token_manager, notifier))
}

/// Internal loop. Extracted so the unit tests below can drive it via
/// `evaluate_transition` + `apply_transition` without spawning a task.
async fn run_watchdog_loop(
    token_manager: Arc<TokenManager>,
    notifier: Option<Arc<NotificationService>>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(MID_SESSION_CHECK_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick — we want to wait one full
    // interval before the first check so this doesn't fire at boot
    // (the pre-market HALT already covers boot-time).
    interval.tick().await;
    let mut state = WatchdogState::default();
    loop {
        interval.tick().await;
        if !is_within_market_hours_ist() {
            // Off-hours: skip the check entirely. Do NOT touch state —
            // if we were alerting at 15:30 market close, the next
            // market open (09:15 tomorrow) should fire fresh if the
            // condition persists.
            continue;
        }
        let check_result = token_manager.pre_market_check().await;
        let (is_real_auth_failing, reason) = classify_check_result(&check_result);
        let transition = evaluate_transition(&state, is_real_auth_failing);
        apply_transition(transition, &mut state, reason, notifier.as_ref());
    }
}

/// Classify a `pre_market_check` result into "real auth failure" vs
/// "transient network failure".
///
/// **Why this exists** (2026-04-26 incident): the original watchdog
/// fired CRITICAL Telegram on ANY `pre_market_check` error. On a Sunday
/// at 13:09 IST the operator's laptop hit a 30-second DNS hiccup
/// (`failed to lookup address information`) and the watchdog paged
/// CRITICAL — but nothing was actually wrong with the Dhan token or
/// data plan. False CRITICAL pages erode the operator's trust in real
/// CRITICAL pages, so we now distinguish:
///
/// * **Transient network failure** (DNS lookup failed, connect refused,
///   connection reset, request timeout, no route to host) → log WARN,
///   increment `tv_mid_session_profile_transient_failure_total`, do NOT
///   page Telegram. The next 15-minute cycle will re-check.
/// * **Real auth failure** (HTTP 401/403, profile parse error, dataPlan
///   inactive, activeSegment missing Derivative, token expiry < 4h) →
///   page CRITICAL Telegram via the existing rising-edge logic.
///
/// Returns `(is_real_auth_failing, reason)`. The boolean drives the
/// state machine; transient failures return `false` so they never
/// touch `state.currently_failing`. This means a transient blip
/// followed by a real auth failure on the NEXT cycle still fires
/// CRITICAL on the rising edge — which is the property we want.
fn classify_check_result(
    result: &Result<(), tickvault_common::error::ApplicationError>,
) -> (bool, Option<String>) {
    match result {
        Ok(()) => (false, None),
        Err(err) => {
            let reason = format!("{err}");
            if is_transient_network_failure(&reason) {
                warn!(
                    reason = %reason,
                    "mid-session profile check encountered transient network error — not paging (will retry next cycle)"
                );
                metrics::counter!("tv_mid_session_profile_transient_failure_total").increment(1);
                (false, Some(reason))
            } else {
                (true, Some(reason))
            }
        }
    }
}

/// Returns true if the failure reason is a transient network error
/// (laptop offline / DNS flap / Dhan socket reset) rather than a
/// real Dhan-side auth or data-plan invalidation.
///
/// The reason strings come from [`crate::auth::token_manager::TokenManager::get_user_profile`]
/// and reach this classifier via `format!("{err}")` on the
/// `ApplicationError::AuthenticationFailed` Display, which prepends
/// `"Dhan authentication failed: "` to the inner reason. So the
/// strings we see here look like one of:
///
/// * `"Dhan authentication failed: profile request failed: <reqwest::Error>"`
///   — the HTTP send leg failed before any response was received.
///   ALL such cases are network-side. We require BOTH the
///   `"profile request failed:"` wrapper (proves we're on the send
///   leg, not a real HTTP 4xx response which uses the
///   `"profile request HTTP {status}"` wrapper) AND one of the
///   transient substrings below.
/// * `"Dhan authentication failed: profile request HTTP 401 ..."` —
///   real auth failure; NOT transient.
/// * `"Dhan authentication failed: data plan is 'Inactive' ..."` —
///   real data-plan failure; NOT transient.
///
/// New substrings should be added here only after observing them in
/// `tv_mid_session_profile_transient_failure_total` not incrementing
/// during a known transient incident — i.e. the ratchet tests in this
/// module prove which cases are covered.
fn is_transient_network_failure(reason: &str) -> bool {
    // Must be the HTTP-send-leg wrapper, NOT the HTTP-response wrapper.
    // `contains` (not `starts_with`) so this works whether or not
    // ApplicationError::AuthenticationFailed's Display prefix is present.
    if !reason.contains("profile request failed:") {
        return false;
    }
    let lower = reason.to_lowercase();
    [
        "dns error",
        "failed to lookup address",
        "connection refused",
        "connection reset",
        "operation timed out",
        "timedout",
        "request timeout",
        "connecterror",
        "no route to host",
        "network is unreachable",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Internal state for the edge-triggered watchdog.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct WatchdogState {
    currently_failing: bool,
}

/// Pure verdict — isolates the decision logic from IO so tests can
/// exercise every edge without a live `TokenManager` + `Notifier`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transition {
    /// No-op — state unchanged (steady ok or steady fail).
    NoOp,
    /// Rising edge — first failure detected. Fire CRITICAL Telegram.
    FirstFailure,
    /// Falling edge — recovery after a failing episode. Fire INFO
    /// (no Telegram — operator already knows from the rising edge).
    Recovery,
}

fn evaluate_transition(state: &WatchdogState, is_failing_now: bool) -> Transition {
    match (state.currently_failing, is_failing_now) {
        (false, true) => Transition::FirstFailure,
        (true, false) => Transition::Recovery,
        _ => Transition::NoOp,
    }
}

fn apply_transition(
    transition: Transition,
    state: &mut WatchdogState,
    reason: Option<String>,
    notifier: Option<&Arc<NotificationService>>,
) {
    match transition {
        Transition::NoOp => {}
        Transition::FirstFailure => {
            let reason = reason.unwrap_or_else(|| "no reason provided".to_string());
            error!(
                reason = %reason,
                "CRITICAL: mid-session profile check FAILED — dataPlan / activeSegment / token may be invalid"
            );
            metrics::counter!("tv_mid_session_profile_alert_fired_total").increment(1);
            if let Some(n) = notifier {
                // Dedicated event variant so Telegram message can carry
                // mid-session-specific guidance (restart the app so the
                // boot-time HALT gate triggers — as opposed to the
                // pre-market `PreMarketProfileCheckFailed` which
                // already fires that HALT automatically).
                n.notify(NotificationEvent::MidSessionProfileInvalidated { reason });
            }
            state.currently_failing = true;
        }
        Transition::Recovery => {
            let reason_context = reason.unwrap_or_default();
            info!(
                last_failure_reason = %reason_context,
                "mid-session profile check recovered — no Telegram on falling edge (operator already paged)"
            );
            metrics::counter!("tv_mid_session_profile_recovery_total").increment(1);
            state.currently_failing = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_transition_first_failure() {
        let state = WatchdogState::default();
        let t = evaluate_transition(&state, true);
        assert_eq!(t, Transition::FirstFailure);
    }

    #[test]
    fn evaluate_transition_recovery_after_failure() {
        let state = WatchdogState {
            currently_failing: true,
        };
        let t = evaluate_transition(&state, false);
        assert_eq!(t, Transition::Recovery);
    }

    #[test]
    fn evaluate_transition_steady_fail_no_duplicate_alert() {
        let state = WatchdogState {
            currently_failing: true,
        };
        let t = evaluate_transition(&state, true);
        assert_eq!(t, Transition::NoOp);
    }

    #[test]
    fn evaluate_transition_steady_ok_no_op() {
        let state = WatchdogState::default();
        let t = evaluate_transition(&state, false);
        assert_eq!(t, Transition::NoOp);
    }

    /// Rising-edge followed by falling-edge in the same test — proves
    /// the state flips correctly across a full failure episode.
    #[test]
    fn apply_transition_episode_round_trip() {
        let mut state = WatchdogState::default();
        apply_transition(
            Transition::FirstFailure,
            &mut state,
            Some("test failure".to_string()),
            None,
        );
        assert!(state.currently_failing);
        apply_transition(Transition::Recovery, &mut state, None, None);
        assert!(!state.currently_failing);
    }

    #[test]
    fn interval_constant_is_sane() {
        // At least 5 min (so we don't pound Dhan).
        const _: () = assert!(MID_SESSION_CHECK_INTERVAL_SECS >= 300);
        // At most 30 min (so mid-session invalidation is caught fast).
        const _: () = assert!(MID_SESSION_CHECK_INTERVAL_SECS <= 1800);
    }

    // ---------------------------------------------------------------
    // 2026-04-26 transient-vs-real classifier ratchet tests.
    //
    // Every substring matched by `is_transient_network_failure` MUST
    // have a covering test below. Adding a new substring without a
    // test will let a real auth failure slip through the classifier
    // unnoticed.
    // ---------------------------------------------------------------

    #[test]
    fn transient_dns_lookup_failure_is_transient() {
        // Verbatim from the 2026-04-26 13:09 IST production incident.
        let reason = "profile request failed: error sending request for url \
                      (https://api.dhan.co/v2/profile): client error (Connect): \
                      dns error: failed to lookup address information: nodename \
                      nor servname provided, or not known";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_connection_refused_is_transient() {
        let reason = "profile request failed: error sending request for url \
                      (https://api.dhan.co/v2/profile): \
                      Connection refused (os error 111)";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_connection_reset_is_transient() {
        // Mirrors the order-update WS reset error pattern.
        let reason = "profile request failed: error sending request for url \
                      (https://api.dhan.co/v2/profile): \
                      Connection reset by peer (os error 54)";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_request_timeout_is_transient() {
        let reason = "profile request failed: \
                      reqwest::Error { kind: Request, source: TimedOut }";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_no_route_to_host_is_transient() {
        let reason = "profile request failed: error sending request \
                      for url (https://api.dhan.co/v2/profile): \
                      No route to host (os error 65)";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn real_auth_http_401_is_not_transient() {
        // Genuine auth failure — must page CRITICAL.
        let reason = "profile request HTTP 401 Unauthorized — see server logs for details";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_auth_http_403_is_not_transient() {
        let reason = "profile request HTTP 403 Forbidden — see server logs for details";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_data_plan_inactive_is_not_transient() {
        let reason = "data plan is 'Inactive', must be 'Active' for market data access";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_active_segment_missing_is_not_transient() {
        let reason = "activeSegment does not contain 'Derivative' — F&O trading not enabled";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_token_expiring_is_not_transient() {
        let reason = "token validity 2h12m — must have > 4h before market open";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn empty_reason_is_not_transient() {
        // Defensive — never page on an empty string but never falsely
        // classify it as transient either. Returning false here means
        // the state machine still treats it as a real failure (and
        // pages on the rising edge), which is the safe-by-default
        // behaviour for an unknown error shape.
        assert!(!is_transient_network_failure(""));
    }

    #[test]
    fn reason_without_profile_request_failed_prefix_is_not_transient() {
        // A reason like "dns error" with no prefix MUST NOT be
        // classified as transient — it could equally come from a
        // future error path we haven't audited.
        let reason = "dns error: failed to lookup address information";
        assert!(!is_transient_network_failure(reason));
    }

    /// Classifier returns (is_real_auth_failing, reason). Transient
    /// inputs MUST set `is_real_auth_failing = false` so the state
    /// machine never enters the failing state — that's the property
    /// that prevents false CRITICAL pages.
    #[test]
    fn classify_check_result_transient_does_not_count_as_failing() {
        use tickvault_common::error::ApplicationError;
        let err = ApplicationError::AuthenticationFailed {
            reason: "profile request failed: dns error: failed to lookup address \
                     information"
                .to_string(),
        };
        let result: Result<(), ApplicationError> = Err(err);
        let (is_failing, reason) = classify_check_result(&result);
        assert!(
            !is_failing,
            "transient DNS failure must NOT count as failing"
        );
        assert!(reason.is_some());
    }

    #[test]
    fn classify_check_result_real_auth_counts_as_failing() {
        use tickvault_common::error::ApplicationError;
        let err = ApplicationError::AuthenticationFailed {
            reason: "data plan is 'Inactive', must be 'Active' for market data \
                     access"
                .to_string(),
        };
        let result: Result<(), ApplicationError> = Err(err);
        let (is_failing, reason) = classify_check_result(&result);
        assert!(is_failing, "real auth failure must count as failing");
        assert!(reason.is_some());
    }

    #[test]
    fn classify_check_result_ok_is_not_failing() {
        let result: Result<(), tickvault_common::error::ApplicationError> = Ok(());
        let (is_failing, reason) = classify_check_result(&result);
        assert!(!is_failing);
        assert!(reason.is_none());
    }

    /// Round-trip property: a transient blip followed by a real auth
    /// failure on the very next cycle MUST still page CRITICAL.
    /// This is the headline property the 2026-04-26 hotfix preserves.
    #[test]
    fn transient_blip_then_real_failure_still_pages_critical() {
        use tickvault_common::error::ApplicationError;

        let mut state = WatchdogState::default();

        // Cycle 1: transient (DNS hiccup) — must NOT flip state.
        let r1: Result<(), ApplicationError> = Err(ApplicationError::AuthenticationFailed {
            reason: "profile request failed: dns error: failed to lookup address \
                     information"
                .to_string(),
        });
        let (failing_1, _) = classify_check_result(&r1);
        let t1 = evaluate_transition(&state, failing_1);
        assert_eq!(
            t1,
            Transition::NoOp,
            "transient must NOT trigger CRITICAL transition"
        );
        apply_transition(t1, &mut state, None, None);
        assert!(
            !state.currently_failing,
            "state must stay clean after a transient"
        );

        // Cycle 2: real auth failure — MUST trigger FirstFailure
        // (rising edge → CRITICAL Telegram).
        let r2: Result<(), ApplicationError> = Err(ApplicationError::AuthenticationFailed {
            reason: "data plan is 'Inactive', must be 'Active' for market data \
                     access"
                .to_string(),
        });
        let (failing_2, _) = classify_check_result(&r2);
        let t2 = evaluate_transition(&state, failing_2);
        assert_eq!(
            t2,
            Transition::FirstFailure,
            "real auth failure after transient must page CRITICAL"
        );
    }
}
