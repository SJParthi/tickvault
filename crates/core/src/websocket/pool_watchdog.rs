//! A4: Pool-level circuit breaker watchdog.
//!
//! Background health monitor for `WebSocketConnectionPool`. Detects the
//! catastrophic state where ALL connections are simultaneously in
//! `Reconnecting` or `Disconnected` state for a prolonged period — which
//! means no market data is flowing into the system.
//!
//! **State machine:**
//! - `Healthy` — at least one connection is Connected or Connecting
//! - `AllDown(since)` — every connection has been in a non-live state since
//!   a specific instant
//!
//! **Transitions:**
//! - On `tick()`, if any connection is Connected or Connecting → `Healthy`
//! - Otherwise → `AllDown(since=first observation)` OR keep existing since
//!
//! **Alerts emitted by `tick()`:**
//! - At 60s in `AllDown` → fire CRITICAL Telegram + set `tv_pool_degraded_seconds_total`
//! - At 300s in `AllDown` → return `WatchdogVerdict::Halt` so the caller
//!   can exit the process and let the supervisor (systemd / Docker restart
//!   policy) restart it
//! - On recovery (AllDown → Healthy) → INFO log + fire Telegram recovery alert
//!
//! The watchdog itself is pure: `tick()` takes a slice of `ConnectionHealth`
//! and returns a verdict. Side effects (metrics, Telegram) are performed by
//! the caller which owns the `NotificationService`.

use std::time::{Duration, Instant};

use crate::websocket::types::{ConnectionHealth, ConnectionState};

/// How long all connections must be down before we fire a CRITICAL alert.
/// Matches the Dhan WebSocket server ping timeout (40s) plus a margin.
pub const POOL_DEGRADED_ALERT_SECS: u64 = 60;

/// How long all connections must be down before we escalate to process halt.
/// 5 minutes: enough for a single reconnect storm, not enough to miss
/// meaningful market activity.
pub const POOL_HALT_SECS: u64 = 300;

/// Internal state of the watchdog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolHealthState {
    /// At least one connection is Connected or Connecting.
    Healthy,
    /// All connections have been in a non-live state since this instant.
    AllDown { since: Instant },
}

/// Verdict returned by the watchdog on each tick. The caller uses this to
/// decide whether to fire alerts, emit metrics, or exit the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogVerdict {
    /// The pool is healthy.
    Healthy,
    /// The pool just recovered from `AllDown`.
    Recovered { was_down_for: Duration },
    /// All connections are down but we're still inside the alert grace
    /// period. Emit a gauge update but no alert yet.
    Degrading { down_for: Duration },
    /// All connections have been down longer than `POOL_DEGRADED_ALERT_SECS`.
    /// Caller must fire a CRITICAL Telegram alert and update the
    /// `tv_pool_degraded_seconds_total` counter. Fired once per
    /// down-cycle unless `Recovered` resets it.
    Degraded { down_for: Duration },
    /// All connections have been down longer than `POOL_HALT_SECS`. Caller
    /// must exit the process with a non-zero status so the supervisor
    /// restarts it.
    Halt { down_for: Duration },
}

/// Pool-level circuit breaker watchdog.
#[derive(Debug)]
pub struct PoolWatchdog {
    state: PoolHealthState,
    /// Has the `Degraded` alert been fired for the current down-cycle?
    /// Reset to false on recovery.
    degraded_alert_fired: bool,
}

impl PoolWatchdog {
    /// Creates a new watchdog in the `Healthy` state.
    pub const fn new() -> Self {
        Self {
            state: PoolHealthState::Healthy,
            degraded_alert_fired: false,
        }
    }

    /// Returns the current internal state (for metrics + tests).
    pub const fn state(&self) -> PoolHealthState {
        self.state
    }

    /// Returns true if the current down-cycle has already fired its
    /// degraded alert (tests).
    pub const fn degraded_alert_fired(&self) -> bool {
        self.degraded_alert_fired
    }

    /// Evaluates pool health given the current connection snapshots and
    /// the current time. Pure function — the caller decides what to do
    /// with the verdict.
    ///
    /// This is a cold path: called at most once every 5 seconds from the
    /// pool watchdog task, not per tick.
    ///
    /// Side effect: updates observability gauges on every tick —
    /// `tv_websocket_pool_all_dead` (0/1) and
    /// `tv_websocket_failed_connections_count` — so Prometheus has fresh
    /// values regardless of verdict transitions.
    pub fn tick(&mut self, healths: &[ConnectionHealth], now: Instant) -> WatchdogVerdict {
        let any_live = healths.iter().any(|h| {
            matches!(
                h.state,
                ConnectionState::Connected | ConnectionState::Connecting
            )
        });

        // Observability: emit per-tick gauges so Grafana can trend them.
        let failed_count = healths
            .iter()
            .filter(|h| {
                matches!(
                    h.state,
                    ConnectionState::Reconnecting | ConnectionState::Disconnected
                )
            })
            .count();
        // O(1) EXEMPT: 5 connections max; runs every 5s not per tick.
        metrics::gauge!("tv_websocket_pool_all_dead").set(if any_live { 0.0 } else { 1.0 });
        metrics::gauge!("tv_websocket_failed_connections_count").set(failed_count as f64);

        match (self.state, any_live) {
            // Still healthy
            (PoolHealthState::Healthy, true) => WatchdogVerdict::Healthy,

            // Just went down
            (PoolHealthState::Healthy, false) => {
                self.state = PoolHealthState::AllDown { since: now };
                self.degraded_alert_fired = false;
                WatchdogVerdict::Degrading {
                    down_for: Duration::ZERO,
                }
            }

            // Recovered
            (PoolHealthState::AllDown { since }, true) => {
                let was_down_for = now.saturating_duration_since(since);
                self.state = PoolHealthState::Healthy;
                self.degraded_alert_fired = false;
                WatchdogVerdict::Recovered { was_down_for }
            }

            // Still down — check thresholds
            (PoolHealthState::AllDown { since }, false) => {
                let down_for = now.saturating_duration_since(since);
                if down_for >= Duration::from_secs(POOL_HALT_SECS) {
                    WatchdogVerdict::Halt { down_for }
                } else if down_for >= Duration::from_secs(POOL_DEGRADED_ALERT_SECS) {
                    if !self.degraded_alert_fired {
                        self.degraded_alert_fired = true;
                        WatchdogVerdict::Degraded { down_for }
                    } else {
                        // Already alerted — keep reporting as Degrading so
                        // the gauge updates without re-firing Telegram.
                        WatchdogVerdict::Degrading { down_for }
                    }
                } else {
                    WatchdogVerdict::Degrading { down_for }
                }
            }
        }
    }
}

impl Default for PoolWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test arithmetic on time
mod tests {
    use super::*;

    fn health(id: u8, state: ConnectionState) -> ConnectionHealth {
        ConnectionHealth {
            connection_id: id,
            state,
            subscribed_count: 0,
            total_reconnections: 0,
        }
    }

    fn all_reconnecting() -> Vec<ConnectionHealth> {
        (0..5)
            .map(|i| health(i, ConnectionState::Reconnecting))
            .collect()
    }

    fn all_connected() -> Vec<ConnectionHealth> {
        (0..5)
            .map(|i| health(i, ConnectionState::Connected))
            .collect()
    }

    fn mixed_one_alive() -> Vec<ConnectionHealth> {
        let mut v: Vec<_> = (0..5)
            .map(|i| health(i, ConnectionState::Reconnecting))
            .collect();
        v[2].state = ConnectionState::Connected;
        v
    }

    #[test]
    fn test_watchdog_starts_healthy() {
        let wd = PoolWatchdog::new();
        assert_eq!(wd.state(), PoolHealthState::Healthy);
    }

    #[test]
    fn test_watchdog_all_connected_stays_healthy() {
        let mut wd = PoolWatchdog::new();
        let now = Instant::now();
        assert_eq!(wd.tick(&all_connected(), now), WatchdogVerdict::Healthy);
    }

    #[test]
    fn test_watchdog_one_alive_is_healthy() {
        let mut wd = PoolWatchdog::new();
        let now = Instant::now();
        assert_eq!(
            wd.tick(&mixed_one_alive(), now),
            WatchdogVerdict::Healthy,
            "a single Connected is enough"
        );
    }

    #[test]
    fn test_watchdog_detects_all_reconnecting_as_degrading() {
        let mut wd = PoolWatchdog::new();
        let now = Instant::now();
        let verdict = wd.tick(&all_reconnecting(), now);
        assert_eq!(
            verdict,
            WatchdogVerdict::Degrading {
                down_for: Duration::ZERO
            }
        );
        assert!(matches!(wd.state(), PoolHealthState::AllDown { .. }));
    }

    #[test]
    fn test_watchdog_degrades_at_60s() {
        let mut wd = PoolWatchdog::new();
        let start = Instant::now();
        // Enter AllDown.
        wd.tick(&all_reconnecting(), start);

        // 30s later — still Degrading (below threshold).
        let t_30s = start + Duration::from_secs(30);
        assert_eq!(
            wd.tick(&all_reconnecting(), t_30s),
            WatchdogVerdict::Degrading {
                down_for: Duration::from_secs(30)
            }
        );

        // 60s later — Degraded alert fires once.
        let t_60s = start + Duration::from_secs(60);
        assert_eq!(
            wd.tick(&all_reconnecting(), t_60s),
            WatchdogVerdict::Degraded {
                down_for: Duration::from_secs(60)
            }
        );

        // 90s later — Degrading (alert was already fired, no re-fire).
        let t_90s = start + Duration::from_secs(90);
        assert_eq!(
            wd.tick(&all_reconnecting(), t_90s),
            WatchdogVerdict::Degrading {
                down_for: Duration::from_secs(90)
            }
        );
    }

    #[test]
    fn test_watchdog_halts_at_300s() {
        let mut wd = PoolWatchdog::new();
        let start = Instant::now();
        wd.tick(&all_reconnecting(), start);

        // 60s — burn the Degraded fire so subsequent ticks stay Degrading.
        let t_60s = start + Duration::from_secs(60);
        let v1 = wd.tick(&all_reconnecting(), t_60s);
        assert!(matches!(v1, WatchdogVerdict::Degraded { .. }));

        // 299s — still Degrading (alert already fired).
        let t_299s = start + Duration::from_secs(299);
        let v2 = wd.tick(&all_reconnecting(), t_299s);
        assert!(matches!(v2, WatchdogVerdict::Degrading { .. }));

        // 300s — Halt takes precedence over all other states.
        let t_300s = start + Duration::from_secs(300);
        assert_eq!(
            wd.tick(&all_reconnecting(), t_300s),
            WatchdogVerdict::Halt {
                down_for: Duration::from_secs(300)
            }
        );
    }

    #[test]
    fn test_watchdog_recovery_resets_state() {
        let mut wd = PoolWatchdog::new();
        let start = Instant::now();
        wd.tick(&all_reconnecting(), start);

        // 90s later, degraded alert has fired.
        let t_90s = start + Duration::from_secs(90);
        wd.tick(&all_reconnecting(), t_90s);
        assert!(wd.degraded_alert_fired());

        // Recovery at 120s — one connection comes back.
        let t_120s = start + Duration::from_secs(120);
        let verdict = wd.tick(&mixed_one_alive(), t_120s);
        assert_eq!(
            verdict,
            WatchdogVerdict::Recovered {
                was_down_for: Duration::from_secs(120)
            }
        );
        assert_eq!(wd.state(), PoolHealthState::Healthy);
        assert!(!wd.degraded_alert_fired());
    }

    #[test]
    fn test_watchdog_fires_degraded_alert_exactly_once_per_cycle() {
        let mut wd = PoolWatchdog::new();
        let start = Instant::now();
        wd.tick(&all_reconnecting(), start);

        // Fire.
        let t_60 = start + Duration::from_secs(60);
        let v1 = wd.tick(&all_reconnecting(), t_60);
        assert!(matches!(v1, WatchdogVerdict::Degraded { .. }));

        // Continuous ticks — no re-fire.
        for i in 1..=10 {
            let t = start + Duration::from_secs(60 + i * 5);
            let v = wd.tick(&all_reconnecting(), t);
            assert!(
                matches!(v, WatchdogVerdict::Degrading { .. }),
                "tick at +{i}s must not re-fire Degraded"
            );
        }

        // Recover.
        let t_recover = start + Duration::from_secs(200);
        wd.tick(&all_connected(), t_recover);

        // Re-enter AllDown — alert should be re-armed and fire again.
        let t_new_down = start + Duration::from_secs(210);
        wd.tick(&all_reconnecting(), t_new_down);
        let t_new_60 = t_new_down + Duration::from_secs(60);
        let v2 = wd.tick(&all_reconnecting(), t_new_60);
        assert!(
            matches!(v2, WatchdogVerdict::Degraded { .. }),
            "second cycle must re-fire Degraded alert"
        );
    }

    #[test]
    fn test_watchdog_disconnected_counts_as_down() {
        let mut wd = PoolWatchdog::new();
        let disconnected: Vec<_> = (0..5)
            .map(|i| health(i, ConnectionState::Disconnected))
            .collect();
        let v = wd.tick(&disconnected, Instant::now());
        assert!(matches!(v, WatchdogVerdict::Degrading { .. }));
    }

    #[test]
    fn test_watchdog_connecting_counts_as_live() {
        let mut wd = PoolWatchdog::new();
        let connecting: Vec<_> = (0..5)
            .map(|i| health(i, ConnectionState::Connecting))
            .collect();
        assert_eq!(
            wd.tick(&connecting, Instant::now()),
            WatchdogVerdict::Healthy,
            "Connecting means we're actively trying — not yet a failure"
        );
    }

    #[test]
    fn test_watchdog_empty_pool_is_degrading() {
        // Edge case: zero connections → any_live == false → AllDown.
        // This shouldn't happen in production (pool always has 5) but the
        // watchdog must not panic on it.
        let mut wd = PoolWatchdog::new();
        let v = wd.tick(&[], Instant::now());
        assert!(matches!(v, WatchdogVerdict::Degrading { .. }));
    }
}
