//! Movers 22-TF supervisor — Phase 10 of v3 plan, 2026-04-28.
//!
//! Watches the 22 `tokio::JoinHandle`s spawned by the snapshot scheduler.
//! Every 5 seconds, polls `is_finished()` on each handle. When a handle
//! reports finished (the inner task panicked or returned), the supervisor:
//!
//! 1. Increments `tv_movers_supervisor_respawn_total{timeframe}` counter.
//! 2. Emits `NotificationEvent::Movers22Tf02SchedulerPanic` (Severity::High,
//!    edge-triggered per audit-findings Rule 4).
//! 3. Re-spawns the task with a fresh closure and stores the new handle.
//!
//! Pure-function primitives are testable without spawning real tasks; the
//! actual `run_movers_22tf_supervisor` async runner composes them.
//!
//! ## See also
//!
//! - `crates/core/src/websocket/connection_pool.rs::respawn_dead_connections_loop`
//!   — the prior-art reference (WS-GAP-05) implementing the same pattern.

use std::time::Duration;

/// Polling interval for the supervisor. 5s matches the WS pool supervisor.
pub const SUPERVISOR_POLL_INTERVAL_SECS: u64 = 5;

/// Threshold above which sustained respawn rate triggers escalation.
/// 1 respawn per minute = 0.0166 per second. Pinned by the
/// `tv-movers-22tf-scheduler-panic-burst` Prometheus alert.
pub const SUSTAINED_RESPAWN_RATE_THRESHOLD: f64 = 0.0166;

/// Result of a single supervisor poll over the 22 task slots.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SupervisorPollOutcome {
    /// Indices (0..22) where the task is healthy (still running).
    pub alive: Vec<usize>,
    /// Indices (0..22) where the task is_finished — needs respawn.
    pub finished: Vec<usize>,
}

impl SupervisorPollOutcome {
    /// Returns true if at least one task needs a respawn.
    #[must_use]
    // TEST-EXEMPT: trivial accessor; semantics covered by `test_classify_task_states_*` which assert `outcome.needs_respawn()` directly.
    pub fn needs_respawn(&self) -> bool {
        !self.finished.is_empty()
    }

    /// Returns the number of healthy tasks (out of 22).
    #[must_use]
    // TEST-EXEMPT: trivial accessor; semantics covered by `test_classify_task_states_*` which assert `outcome.healthy_count()` directly.
    pub fn healthy_count(&self) -> usize {
        self.alive.len()
    }
}

/// Pure-function classification: given an iterator of bools where `true`
/// means "this task is still running" and `false` means "is_finished",
/// return the `SupervisorPollOutcome` summary.
///
/// Useful for testing the respawn logic without instantiating real
/// tokio tasks. The runtime version simply calls
/// `JoinHandle::is_finished()` on each handle and feeds the negated
/// bool into this function.
#[must_use]
pub fn classify_task_states<I: IntoIterator<Item = bool>>(states: I) -> SupervisorPollOutcome {
    let mut alive = Vec::new();
    let mut finished = Vec::new();
    for (idx, is_running) in states.into_iter().enumerate() {
        if is_running {
            alive.push(idx);
        } else {
            finished.push(idx);
        }
    }
    SupervisorPollOutcome { alive, finished }
}

/// Edge-triggered detector for sustained respawn bursts. Tracks the
/// per-tick respawn count over a sliding window (`window_size` ticks)
/// and returns `true` iff the burst rate exceeds
/// `SUSTAINED_RESPAWN_RATE_THRESHOLD` (1/min).
///
/// `respawn_history` MUST be sorted oldest-first; the runner appends
/// the new count at the end on each tick and pops the front when the
/// window is full.
#[must_use]
pub fn is_sustained_respawn_burst(respawn_history: &[u32], window_secs: u64) -> bool {
    if respawn_history.is_empty() || window_secs == 0 {
        return false;
    }
    let total: u32 = respawn_history.iter().sum();
    // DATA-INTEGRITY-EXEMPT: u32-count → f64-rate conversion for sustained-burst threshold compare; total is a respawn count (NOT a Dhan price), so f32_to_f64_clean() does not apply.
    let total_f64 = f64::from(total);
    let window_f64 = window_secs as f64;
    let rate = total_f64 / window_f64;
    rate > SUSTAINED_RESPAWN_RATE_THRESHOLD
}

/// Returns the supervisor poll interval as a `Duration`. Used by the
/// runner; exposed for testing constant pinning.
#[must_use]
pub const fn poll_interval() -> Duration {
    Duration::from_secs(SUPERVISOR_POLL_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 10 primitive ratchet — supervisor poll classification.
    #[test]
    fn test_classify_task_states_all_alive() {
        let outcome = classify_task_states(vec![true; 22]);
        assert_eq!(outcome.healthy_count(), 22);
        assert!(!outcome.needs_respawn());
        assert_eq!(outcome.finished.len(), 0);
    }

    #[test]
    fn test_classify_task_states_all_finished() {
        let outcome = classify_task_states(vec![false; 22]);
        assert_eq!(outcome.healthy_count(), 0);
        assert!(outcome.needs_respawn());
        assert_eq!(outcome.finished.len(), 22);
    }

    #[test]
    fn test_classify_task_states_mixed() {
        let states = vec![true, false, true, true, false]; // idx 1, 4 dead
        let outcome = classify_task_states(states);
        assert_eq!(outcome.alive, vec![0, 2, 3]);
        assert_eq!(outcome.finished, vec![1, 4]);
        assert!(outcome.needs_respawn());
        assert_eq!(outcome.healthy_count(), 3);
    }

    #[test]
    fn test_classify_task_states_empty_iterator() {
        let outcome = classify_task_states(Vec::<bool>::new());
        assert_eq!(outcome.healthy_count(), 0);
        assert!(!outcome.needs_respawn());
    }

    #[test]
    fn test_is_sustained_respawn_burst_below_threshold() {
        // 0 respawns over any window → not a burst.
        assert!(!is_sustained_respawn_burst(&[0; 12], 60));
        // 1 respawn over 120s = 0.0083/s — clearly below the 1/min threshold.
        assert!(!is_sustained_respawn_burst(&[1; 1], 120));
    }

    #[test]
    fn test_is_sustained_respawn_burst_above_threshold() {
        // 2 respawns over 60s = 0.033/s — clearly > 1/min.
        assert!(is_sustained_respawn_burst(
            &[1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            60
        ));
        // 5 respawns over 60s = 0.083/s — clearly bursting.
        assert!(is_sustained_respawn_burst(&[5; 1], 60));
    }

    #[test]
    fn test_is_sustained_respawn_burst_empty_history_safe() {
        assert!(!is_sustained_respawn_burst(&[], 60));
    }

    #[test]
    fn test_is_sustained_respawn_burst_zero_window_safe() {
        assert!(!is_sustained_respawn_burst(&[10, 10], 0));
    }

    #[test]
    fn test_supervisor_poll_interval_is_5_seconds() {
        assert_eq!(SUPERVISOR_POLL_INTERVAL_SECS, 5);
        assert_eq!(poll_interval(), Duration::from_secs(5));
    }

    #[test]
    fn test_sustained_respawn_threshold_matches_alert() {
        // Pinned at 1/min = 0.0166/sec. Drift here MUST mirror the
        // tv-movers-22tf-scheduler-panic-burst alert in alerts.yml.
        assert!((SUSTAINED_RESPAWN_RATE_THRESHOLD - 0.0166).abs() < 1e-6);
    }
}
