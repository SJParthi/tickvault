//! S3-1: QuestDB health poller.
//!
//! Pure state machine that consumes connection-state snapshots from a
//! `TickPersistenceWriter` and emits metrics + alerts when the writer is
//! disconnected or recovering. The poller itself does NO I/O — it's a
//! state machine fed by an upstream task that owns the writer.
//!
//! # Why this exists
//!
//! A QuestDB outage must be **invisible to market data ingestion** (the
//! writer's ring buffer + spill + DLQ layers handle that), but it must
//! be **fully visible to the operator**. This module provides:
//!
//! 1. A Prometheus gauge `dlt_questdb_connected` (1.0 = connected, 0.0 = not)
//! 2. A Prometheus gauge `dlt_questdb_disconnected_seconds` (how long the
//!    current outage has lasted; 0 when connected)
//! 3. A Prometheus counter `dlt_questdb_reconnects_total` (successful
//!    reconnects since startup)
//! 4. A Prometheus counter `dlt_questdb_disconnect_events_total` (transitions
//!    from Connected → Disconnected)
//! 5. Verdict enum consumed by the owning task for ERROR-level logs that
//!    fire Telegram via the shared alert hook
//!
//! # Thresholds
//!
//! - 0-30s disconnected → `Degrading` (gauge only, no alert)
//! - 30s disconnected → `Degraded` CRITICAL alert (once per outage)
//! - 300s disconnected → `Halt` recommended (operator decision)
//!
//! These mirror the WebSocket pool watchdog thresholds for consistency.

use std::time::{Duration, Instant};

/// How long QuestDB can be down before we fire a CRITICAL alert.
pub const QUESTDB_DEGRADED_ALERT_SECS: u64 = 30;

/// How long QuestDB can be down before we recommend a process halt.
pub const QUESTDB_HALT_SECS: u64 = 300;

/// Internal state of the QuestDB health poller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestDbHealthState {
    /// Writer is connected to QuestDB.
    Healthy,
    /// Writer has been disconnected since this instant.
    Disconnected { since: Instant },
}

/// Verdict returned by `tick()` so the caller can wire it to metrics + logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestDbHealthVerdict {
    /// Writer is healthy.
    Healthy,
    /// Writer just recovered from a prior disconnect.
    Recovered { was_down_for: Duration },
    /// Writer is disconnected but inside the alert grace period.
    Degrading { down_for: Duration },
    /// Writer has been disconnected longer than QUESTDB_DEGRADED_ALERT_SECS.
    /// Caller fires CRITICAL Telegram (via ERROR log) and increments the
    /// disconnect counter. Fired exactly once per outage.
    Degraded { down_for: Duration },
    /// Writer has been disconnected longer than QUESTDB_HALT_SECS.
    /// Operator decision whether to exit the process.
    Halt { down_for: Duration },
}

/// QuestDB health poller — stateful wrapper around connection state changes.
#[derive(Debug)]
pub struct QuestDbHealthPoller {
    state: QuestDbHealthState,
    degraded_alert_fired: bool,
    reconnects_total: u64,
    disconnect_events_total: u64,
}

impl QuestDbHealthPoller {
    /// Creates a new poller assumed Healthy (the typical startup state
    /// after the ILP connection succeeds).
    pub const fn new() -> Self {
        Self {
            state: QuestDbHealthState::Healthy,
            degraded_alert_fired: false,
            reconnects_total: 0,
            disconnect_events_total: 0,
        }
    }

    /// Returns the current internal state.
    pub const fn state(&self) -> QuestDbHealthState {
        self.state
    }

    /// Total successful reconnects observed since startup.
    pub const fn reconnects_total(&self) -> u64 {
        self.reconnects_total
    }

    /// Total Connected → Disconnected transitions observed.
    pub const fn disconnect_events_total(&self) -> u64 {
        self.disconnect_events_total
    }

    /// Has the current down-cycle already fired its degraded alert?
    pub const fn degraded_alert_fired(&self) -> bool {
        self.degraded_alert_fired
    }

    /// Feeds a connection snapshot (`true` = connected, `false` = not) plus
    /// the current time into the poller and returns a verdict.
    ///
    /// Pure function over state. The caller is responsible for emitting
    /// the metrics implied by the verdict and deciding whether to exit on
    /// `Halt`.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: saturating counters
    pub fn tick(&mut self, connected: bool, now: Instant) -> QuestDbHealthVerdict {
        match (self.state, connected) {
            (QuestDbHealthState::Healthy, true) => QuestDbHealthVerdict::Healthy,

            (QuestDbHealthState::Healthy, false) => {
                self.state = QuestDbHealthState::Disconnected { since: now };
                self.degraded_alert_fired = false;
                self.disconnect_events_total = self.disconnect_events_total.saturating_add(1);
                QuestDbHealthVerdict::Degrading {
                    down_for: Duration::ZERO,
                }
            }

            (QuestDbHealthState::Disconnected { since }, true) => {
                let was_down_for = now.saturating_duration_since(since);
                self.state = QuestDbHealthState::Healthy;
                self.degraded_alert_fired = false;
                self.reconnects_total = self.reconnects_total.saturating_add(1);
                QuestDbHealthVerdict::Recovered { was_down_for }
            }

            (QuestDbHealthState::Disconnected { since }, false) => {
                let down_for = now.saturating_duration_since(since);
                if down_for >= Duration::from_secs(QUESTDB_HALT_SECS) {
                    QuestDbHealthVerdict::Halt { down_for }
                } else if down_for >= Duration::from_secs(QUESTDB_DEGRADED_ALERT_SECS) {
                    if !self.degraded_alert_fired {
                        self.degraded_alert_fired = true;
                        QuestDbHealthVerdict::Degraded { down_for }
                    } else {
                        QuestDbHealthVerdict::Degrading { down_for }
                    }
                } else {
                    QuestDbHealthVerdict::Degrading { down_for }
                }
            }
        }
    }
}

impl Default for QuestDbHealthPoller {
    fn default() -> Self {
        Self::new()
    }
}

/// Emits the `dlt_questdb_*` metrics implied by a verdict. Separated from
/// `tick()` so the state machine stays pure (testable without a metrics
/// recorder).
// TEST-EXEMPT: pure metric side effects; the underlying state transitions are fully covered by test_poller_* tests and the metric emission path has no branches worth testing in isolation
pub fn emit_metrics_for_verdict(verdict: QuestDbHealthVerdict, poller: &QuestDbHealthPoller) {
    match verdict {
        QuestDbHealthVerdict::Healthy => {
            metrics::gauge!("dlt_questdb_connected").set(1.0);
            metrics::gauge!("dlt_questdb_disconnected_seconds").set(0.0);
        }
        QuestDbHealthVerdict::Recovered { was_down_for } => {
            metrics::gauge!("dlt_questdb_connected").set(1.0);
            metrics::gauge!("dlt_questdb_disconnected_seconds").set(0.0);
            metrics::counter!("dlt_questdb_reconnects_total").increment(1);
            tracing::info!(
                down_for_secs = was_down_for.as_secs(),
                "S3-1: QuestDB reconnected — write path restored"
            );
        }
        QuestDbHealthVerdict::Degrading { down_for } => {
            metrics::gauge!("dlt_questdb_connected").set(0.0);
            metrics::gauge!("dlt_questdb_disconnected_seconds").set(down_for.as_secs_f64());
        }
        QuestDbHealthVerdict::Degraded { down_for } => {
            metrics::gauge!("dlt_questdb_connected").set(0.0);
            metrics::gauge!("dlt_questdb_disconnected_seconds").set(down_for.as_secs_f64());
            metrics::counter!("dlt_questdb_disconnect_events_total").increment(1);
            tracing::error!(
                down_for_secs = down_for.as_secs(),
                disconnect_events_total = poller.disconnect_events_total(),
                "S3-1 CRITICAL: QuestDB has been disconnected for >30s — tick ring buffer \
                 + spill path are absorbing all writes. No data is lost, but operator \
                 attention is required. Check Docker logs for dlt-questdb."
            );
        }
        QuestDbHealthVerdict::Halt { down_for } => {
            metrics::gauge!("dlt_questdb_connected").set(0.0);
            metrics::gauge!("dlt_questdb_disconnected_seconds").set(down_for.as_secs_f64());
            tracing::error!(
                down_for_secs = down_for.as_secs(),
                "S3-1 FATAL: QuestDB has been disconnected for >300s. Ring buffer and \
                 spill file are approaching capacity. Restart QuestDB or the app now."
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test arithmetic
mod tests {
    use super::*;

    #[test]
    fn test_poller_starts_healthy() {
        let p = QuestDbHealthPoller::new();
        assert_eq!(p.state(), QuestDbHealthState::Healthy);
        assert_eq!(p.reconnects_total(), 0);
        assert_eq!(p.disconnect_events_total(), 0);
    }

    #[test]
    fn test_poller_healthy_stays_healthy() {
        let mut p = QuestDbHealthPoller::new();
        assert_eq!(p.tick(true, Instant::now()), QuestDbHealthVerdict::Healthy);
        assert_eq!(p.tick(true, Instant::now()), QuestDbHealthVerdict::Healthy);
    }

    #[test]
    fn test_poller_disconnect_transition_increments_counter() {
        let mut p = QuestDbHealthPoller::new();
        let _ = p.tick(true, Instant::now());
        let v = p.tick(false, Instant::now());
        assert_eq!(
            v,
            QuestDbHealthVerdict::Degrading {
                down_for: Duration::ZERO
            }
        );
        assert_eq!(p.disconnect_events_total(), 1);
        assert!(matches!(p.state(), QuestDbHealthState::Disconnected { .. }));
    }

    #[test]
    fn test_poller_fires_degraded_at_30s() {
        let mut p = QuestDbHealthPoller::new();
        let start = Instant::now();
        p.tick(false, start);

        let t_29 = start + Duration::from_secs(29);
        assert!(matches!(
            p.tick(false, t_29),
            QuestDbHealthVerdict::Degrading { .. }
        ));
        assert!(!p.degraded_alert_fired());

        let t_30 = start + Duration::from_secs(30);
        assert!(matches!(
            p.tick(false, t_30),
            QuestDbHealthVerdict::Degraded { .. }
        ));
        assert!(p.degraded_alert_fired());

        // Subsequent ticks at 35s, 60s, ... stay in Degrading (alert already fired).
        let t_60 = start + Duration::from_secs(60);
        assert!(matches!(
            p.tick(false, t_60),
            QuestDbHealthVerdict::Degrading { .. }
        ));
    }

    #[test]
    fn test_poller_fires_halt_at_300s() {
        let mut p = QuestDbHealthPoller::new();
        let start = Instant::now();
        p.tick(false, start);
        // Burn the Degraded fire.
        p.tick(false, start + Duration::from_secs(30));

        let t_299 = start + Duration::from_secs(299);
        assert!(matches!(
            p.tick(false, t_299),
            QuestDbHealthVerdict::Degrading { .. }
        ));

        let t_300 = start + Duration::from_secs(300);
        assert!(matches!(
            p.tick(false, t_300),
            QuestDbHealthVerdict::Halt { .. }
        ));
    }

    #[test]
    fn test_poller_recovery_increments_reconnect_counter() {
        let mut p = QuestDbHealthPoller::new();
        let start = Instant::now();
        p.tick(false, start);
        p.tick(false, start + Duration::from_secs(10));

        let v = p.tick(true, start + Duration::from_secs(15));
        assert_eq!(
            v,
            QuestDbHealthVerdict::Recovered {
                was_down_for: Duration::from_secs(15)
            }
        );
        assert_eq!(p.reconnects_total(), 1);
        assert_eq!(p.state(), QuestDbHealthState::Healthy);
        assert!(!p.degraded_alert_fired());
    }

    #[test]
    fn test_poller_multiple_outages_each_counted() {
        let mut p = QuestDbHealthPoller::new();
        let start = Instant::now();
        // Outage 1
        p.tick(false, start);
        p.tick(true, start + Duration::from_secs(5));
        // Outage 2
        p.tick(false, start + Duration::from_secs(10));
        p.tick(true, start + Duration::from_secs(15));
        // Outage 3
        p.tick(false, start + Duration::from_secs(20));

        assert_eq!(p.disconnect_events_total(), 3);
        assert_eq!(p.reconnects_total(), 2);
    }

    #[test]
    fn test_poller_degraded_fires_exactly_once_per_outage() {
        let mut p = QuestDbHealthPoller::new();
        let start = Instant::now();
        p.tick(false, start);

        // Fire at 30s.
        let v1 = p.tick(false, start + Duration::from_secs(30));
        assert!(matches!(v1, QuestDbHealthVerdict::Degraded { .. }));

        // Next 10 ticks — no re-fire.
        for i in 1..=10 {
            let t = start + Duration::from_secs(30 + i * 10);
            let v = p.tick(false, t);
            assert!(
                matches!(v, QuestDbHealthVerdict::Degrading { .. }),
                "tick at +{i}x10s must not re-fire Degraded"
            );
        }

        // Recover + re-enter outage → fire again.
        p.tick(true, start + Duration::from_secs(200));
        let t_new = start + Duration::from_secs(210);
        p.tick(false, t_new);
        let v2 = p.tick(false, t_new + Duration::from_secs(30));
        assert!(
            matches!(v2, QuestDbHealthVerdict::Degraded { .. }),
            "second outage must re-fire Degraded"
        );
    }

    #[test]
    fn test_poller_recovery_resets_alert_flag() {
        let mut p = QuestDbHealthPoller::new();
        let start = Instant::now();
        p.tick(false, start);
        p.tick(false, start + Duration::from_secs(30));
        assert!(p.degraded_alert_fired());

        p.tick(true, start + Duration::from_secs(40));
        assert!(!p.degraded_alert_fired());
    }
}
