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
use tracing::{error, info};

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
        let is_failing = check_result.is_err();
        let reason = match &check_result {
            Ok(()) => None,
            Err(err) => Some(format!("{err}")),
        };
        let transition = evaluate_transition(&state, is_failing);
        apply_transition(transition, &mut state, reason, notifier.as_ref());
    }
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
}
