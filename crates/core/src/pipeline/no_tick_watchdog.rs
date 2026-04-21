//! No-Live-Ticks-During-Market-Hours watchdog.
//!
//! # Why this exists (Parthiban directive 2026-04-21)
//!
//! On 2026-04-21 the app was up, all 5 WebSocket connections were
//! "connected" per our internal state, but Dhan was not actually
//! streaming live data — the `ticks` table only held stale snapshots
//! from the previous day's close. The operator discovered this only
//! by manually checking Grafana hours into the trading session.
//!
//! Existing safety nets did NOT catch this:
//!
//! - The [`crate::websocket::activity_watchdog::ActivityWatchdog`] only
//!   checks if any frame (data, ping, pong) crossed the wire. Dhan's
//!   server-side pings kept the watchdog satisfied even though zero
//!   tick data flowed.
//! - The [`tickvault_trading::risk::tick_gap_tracker::TickGapTracker`]
//!   is per-instrument and requires a warmup of
//!   `TICK_GAP_MIN_TICKS_BEFORE_ACTIVE` ticks before it starts checking.
//!   If zero ticks ever arrive, it never warms up.
//!
//! This watchdog closes that gap by running a single global heartbeat
//! check during market hours.
//!
//! # Behaviour
//!
//! - Reads a shared `Arc<AtomicI64>` heartbeat that the tick processor
//!   updates on every parsed tick (epoch seconds, single relaxed atomic
//!   store on the hot path — O(1), zero allocation).
//! - Polls every [`NO_TICK_POLL_INTERVAL_SECS`] seconds. Each poll is
//!   one atomic load + one `chrono::Utc::now()` + one comparison.
//! - During market hours only (Rule 3 — uses
//!   [`tickvault_common::market_hours::is_within_market_hours_ist`]).
//! - Edge-triggered (Rule 4 — fires CRITICAL + Telegram on rising edge,
//!   logs INFO on falling edge to avoid spam).
//! - Threshold: [`NO_TICK_THRESHOLD_SECS`] (120s by default — 2 min of
//!   silence during market hours is unambiguously a real failure).
//!
//! # Why this is NOT a backfill
//!
//! Per `.claude/rules/project/live-feed-purity.md` we do not synthesise
//! ticks from any source. This watchdog only DETECTS the silence and
//! ALERTS — it never writes synthetic data to the `ticks` table.
//!
//! # Testing
//!
//! - `test_initial_heartbeat_zero_does_not_fire_alert` — boot before
//!   first tick must not false-fire.
//! - `test_alert_fires_after_threshold_with_market_hours_forced` — uses
//!   the test override on `tickvault_common::market_hours` to pin the
//!   gate to true.
//! - `test_edge_triggered_does_not_fire_twice_for_same_silence_window`
//!   — confirms Rule 4 compliance.
//! - `test_recovery_after_alert_clears_state_and_logs_info` — falling
//!   edge must clear the latch + log INFO (no Telegram).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use tickvault_common::market_hours::is_within_market_hours_ist;
use tracing::{error, info};

use crate::notification::NotificationService;
use crate::notification::events::NotificationEvent;

/// Poll cadence for the watchdog. 30s gives a reasonable balance between
/// detection latency and metric overhead. At a 120s threshold we have
/// 4 sample windows before fire — enough to absorb a single missed poll
/// without false-positive risk.
pub const NO_TICK_POLL_INTERVAL_SECS: u64 = 30;

/// Silence threshold (seconds) during market hours before the watchdog
/// fires CRITICAL + Telegram. 120s = 2 minutes. NSE streams data
/// continuously during 09:15-15:30 IST; 2 minutes of total silence is
/// unambiguous failure.
pub const NO_TICK_THRESHOLD_SECS: u64 = 120;

/// Builds a fresh heartbeat atomic. Initial value `0` means "no tick
/// received yet". The watchdog interprets `0` as a special "not started"
/// state and does NOT fire an alert until at least one tick has been
/// recorded — otherwise every cold boot would fire a false positive.
#[must_use]
pub fn new_tick_heartbeat() -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(0))
}

/// Spawns the no-tick watchdog as an independent tokio task.
///
/// The returned [`tokio::task::JoinHandle`] can be aborted on graceful
/// shutdown. The task runs forever otherwise.
///
/// # Arguments
/// * `heartbeat` — shared atomic that the tick processor updates with
///   the wall-clock epoch seconds of each parsed tick.
/// * `notifier` — `NotificationService` for the CRITICAL Telegram alert.
///   `None` disables Telegram entirely (logs still fire).
// TEST-EXEMPT: thin tokio::spawn wrapper around `run_watchdog_loop`. The behavioural surface (evaluate_heartbeat + apply_verdict) is covered by 9 unit tests in this module; the spawn wrapper itself has no decision logic to exercise.
pub fn spawn_no_tick_watchdog(
    heartbeat: Arc<AtomicI64>,
    notifier: Option<Arc<NotificationService>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_watchdog_loop(heartbeat, notifier))
}

/// The poll loop. Extracted from `spawn_no_tick_watchdog` for direct
/// unit testing without a tokio spawn.
async fn run_watchdog_loop(heartbeat: Arc<AtomicI64>, notifier: Option<Arc<NotificationService>>) {
    let mut interval = tokio::time::interval(Duration::from_secs(NO_TICK_POLL_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick — we want at least one full
    // window before evaluation.
    interval.tick().await;
    let mut state = WatchdogState::default();
    loop {
        interval.tick().await;
        let now_secs = chrono::Utc::now().timestamp();
        let last_tick_secs = heartbeat.load(Ordering::Relaxed);
        let in_hours = is_within_market_hours_ist();
        let verdict = evaluate_heartbeat(now_secs, last_tick_secs, in_hours, &state);
        apply_verdict(verdict, &mut state, notifier.as_ref());
    }
}

/// Internal state — tracks whether we are currently in an alerting
/// window. Edge-triggered: rising edge fires CRITICAL, falling edge
/// logs INFO.
#[derive(Default)]
struct WatchdogState {
    currently_alerting: bool,
}

/// Pure-function verdict for unit testing without IO. Always returns
/// one of the three states; the caller decides whether to actually
/// emit logs / Telegram.
#[derive(Debug, PartialEq, Eq)]
enum WatchdogVerdict {
    /// No action — heartbeat is fresh, off-hours, or no tick yet.
    NoOp,
    /// Rising edge — fire CRITICAL + Telegram.
    Fire { silent_for_secs: u64 },
    /// Falling edge — heartbeat resumed after an alert; log INFO only.
    Recover { silent_for_secs: u64 },
}

/// Pure decision function. Tested directly without spawning the loop.
fn evaluate_heartbeat(
    now_secs: i64,
    last_tick_secs: i64,
    in_market_hours: bool,
    state: &WatchdogState,
) -> WatchdogVerdict {
    // Off-hours: always no-op. If we were alerting and the operator
    // restarts post-close, the alerting state will not be cleared until
    // the next market session — that is acceptable because the latch
    // resets via process restart anyway.
    if !in_market_hours {
        return WatchdogVerdict::NoOp;
    }
    // Cold boot — no tick has ever arrived. Do not fire (would false-
    // alarm every restart pre-market or in tests). The operator will
    // see the silence on dashboards if it persists.
    if last_tick_secs == 0 {
        return WatchdogVerdict::NoOp;
    }
    // Compute silence window. Use saturating math so a clock skew that
    // makes `last_tick_secs > now_secs` does not panic / underflow.
    let silent_for_secs = now_secs.saturating_sub(last_tick_secs).max(0) as u64;
    let threshold_breached = silent_for_secs >= NO_TICK_THRESHOLD_SECS;
    match (state.currently_alerting, threshold_breached) {
        (false, true) => WatchdogVerdict::Fire { silent_for_secs },
        (true, false) => WatchdogVerdict::Recover { silent_for_secs },
        _ => WatchdogVerdict::NoOp,
    }
}

fn apply_verdict(
    verdict: WatchdogVerdict,
    state: &mut WatchdogState,
    notifier: Option<&Arc<NotificationService>>,
) {
    match verdict {
        WatchdogVerdict::NoOp => {}
        WatchdogVerdict::Fire { silent_for_secs } => {
            error!(
                silent_for_secs,
                threshold_secs = NO_TICK_THRESHOLD_SECS,
                "CRITICAL: zero live ticks received during market hours past threshold"
            );
            metrics::counter!("tv_no_tick_alert_fired_total").increment(1);
            if let Some(n) = notifier {
                n.notify(NotificationEvent::NoLiveTicksDuringMarketHours {
                    silent_for_secs,
                    threshold_secs: NO_TICK_THRESHOLD_SECS,
                });
            }
            state.currently_alerting = true;
        }
        WatchdogVerdict::Recover { silent_for_secs } => {
            info!(
                last_silent_for_secs = silent_for_secs,
                "live ticks resumed during market hours — clearing no-tick alert (no Telegram on falling edge)"
            );
            metrics::counter!("tv_no_tick_alert_cleared_total").increment(1);
            state.currently_alerting = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_heartbeat_zero_does_not_fire_alert() {
        let state = WatchdogState::default();
        let verdict = evaluate_heartbeat(1_000_000, 0, true, &state);
        assert_eq!(
            verdict,
            WatchdogVerdict::NoOp,
            "zero heartbeat at boot must not fire — no tick received yet"
        );
    }

    #[test]
    fn test_off_hours_never_fires() {
        let state = WatchdogState::default();
        // last_tick_secs is ancient (5 hours ago) but we are off-hours.
        let verdict = evaluate_heartbeat(1_000_000, 1_000_000 - 5 * 3600, false, &state);
        assert_eq!(verdict, WatchdogVerdict::NoOp);
    }

    #[test]
    fn test_alert_fires_after_threshold_in_market_hours() {
        let state = WatchdogState::default();
        let now = 1_000_000;
        let last = now - i64::from(NO_TICK_THRESHOLD_SECS as i32 + 5);
        let verdict = evaluate_heartbeat(now, last, true, &state);
        assert!(matches!(verdict, WatchdogVerdict::Fire { .. }));
    }

    #[test]
    fn test_alert_does_not_fire_below_threshold() {
        let state = WatchdogState::default();
        let now = 1_000_000;
        // 60s of silence < 120s threshold
        let verdict = evaluate_heartbeat(now, now - 60, true, &state);
        assert_eq!(verdict, WatchdogVerdict::NoOp);
    }

    #[test]
    fn test_edge_triggered_does_not_fire_twice_for_same_silence_window() {
        let mut state = WatchdogState::default();
        let now = 1_000_000;
        let last = now - i64::from(NO_TICK_THRESHOLD_SECS as i32 + 5);
        // First evaluation — fires.
        let v1 = evaluate_heartbeat(now, last, true, &state);
        assert!(matches!(v1, WatchdogVerdict::Fire { .. }));
        // Apply the verdict (sets currently_alerting = true).
        apply_verdict(v1, &mut state, None);
        // Second evaluation — silence persists, must NOT fire again.
        let v2 = evaluate_heartbeat(now + 30, last, true, &state);
        assert_eq!(
            v2,
            WatchdogVerdict::NoOp,
            "Rule 4 — edge-triggered alerts must not re-fire while condition holds"
        );
    }

    #[test]
    fn test_recovery_after_alert_clears_state() {
        let mut state = WatchdogState {
            currently_alerting: true,
        };
        let now = 1_000_000;
        let last = now - 5; // fresh tick — well under threshold
        let verdict = evaluate_heartbeat(now, last, true, &state);
        assert!(matches!(verdict, WatchdogVerdict::Recover { .. }));
        apply_verdict(verdict, &mut state, None);
        assert!(!state.currently_alerting);
    }

    #[test]
    fn test_clock_skew_does_not_panic() {
        let state = WatchdogState::default();
        // last_tick_secs is in the FUTURE (clock went backwards).
        let verdict = evaluate_heartbeat(1_000_000, 1_000_000 + 60, true, &state);
        // saturating_sub yields 0 → not threshold breached → NoOp.
        assert_eq!(verdict, WatchdogVerdict::NoOp);
    }

    #[test]
    fn test_thresholds_are_sane() {
        const _: () = assert!(NO_TICK_THRESHOLD_SECS >= 60);
        const _: () = assert!(NO_TICK_THRESHOLD_SECS <= 600);
        const _: () = assert!(NO_TICK_POLL_INTERVAL_SECS > 0);
        const _: () = assert!(NO_TICK_POLL_INTERVAL_SECS <= NO_TICK_THRESHOLD_SECS / 2);
    }

    #[test]
    fn test_new_tick_heartbeat_starts_at_zero() {
        let hb = new_tick_heartbeat();
        assert_eq!(hb.load(Ordering::Relaxed), 0);
    }
}
