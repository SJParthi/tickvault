//! Tick gap detection — monitors feed health per security.
//!
//! Cold-path component: called from the trading pipeline tick processing loop
//! (NOT the hot-path binary parser). Tracks the last exchange timestamp per
//! security_id and emits warnings when gaps exceed configured thresholds.
//!
//! # Design
//! - HashMap-based O(1) per lookup (cold path, not latency-sensitive)
//! - No allocation after initial warmup (HashMap pre-sizes to known universe)
//! - Thresholds from compile-time constants (TICK_GAP_ALERT_THRESHOLD_SECS, etc.)
//! - Integrates with tracing for structured logging (Telegram alert on ERROR)

use std::collections::HashMap;

use dhan_live_trader_common::constants::{
    TICK_GAP_ALERT_THRESHOLD_SECS, TICK_GAP_ERROR_THRESHOLD_SECS, TICK_GAP_MIN_TICKS_BEFORE_ACTIVE,
};
use tracing::{error, warn};

/// Per-security feed health state.
#[derive(Debug, Clone, Copy)]
struct SecurityFeedState {
    /// Last exchange timestamp seen (epoch seconds).
    last_exchange_timestamp: u32,
    /// Total ticks received for this security (used for warmup gate).
    tick_count: u32,
}

/// Tracks tick gaps per security for feed health monitoring.
///
/// Cold-path component — called once per tick from the trading pipeline.
/// Pre-allocate capacity based on expected universe size.
pub struct TickGapTracker {
    /// Per-security feed state.
    states: HashMap<u32, SecurityFeedState>,
    /// Total gap warnings emitted (for metrics/alerting).
    total_warnings: u64,
    /// Total gap errors emitted (for metrics/alerting).
    total_errors: u64,
}

impl TickGapTracker {
    /// Creates a new tracker with the specified initial capacity.
    ///
    /// # Arguments
    /// * `capacity` — Expected number of unique securities (avoids rehashing).
    pub fn new(capacity: usize) -> Self {
        Self {
            states: HashMap::with_capacity(capacity),
            total_warnings: 0,
            total_errors: 0,
        }
    }

    /// Records a tick and checks for feed gaps.
    ///
    /// # Arguments
    /// * `security_id` — Dhan security identifier.
    /// * `exchange_timestamp` — Exchange timestamp in epoch seconds.
    ///
    /// # Returns
    /// `TickGapResult` indicating whether a gap was detected.
    pub fn record_tick(&mut self, security_id: u32, exchange_timestamp: u32) -> TickGapResult {
        let state = self.states.entry(security_id).or_insert(SecurityFeedState {
            last_exchange_timestamp: exchange_timestamp,
            tick_count: 0,
        });

        state.tick_count = state.tick_count.saturating_add(1);

        // Don't check gaps during warmup phase.
        if state.tick_count <= TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            state.last_exchange_timestamp = exchange_timestamp;
            return TickGapResult::Ok;
        }

        // Compute gap (handle out-of-order timestamps gracefully).
        let gap_secs = exchange_timestamp.saturating_sub(state.last_exchange_timestamp);

        let result = if gap_secs >= TICK_GAP_ERROR_THRESHOLD_SECS {
            error!(
                security_id = security_id,
                gap_secs = gap_secs,
                last_ts = state.last_exchange_timestamp,
                current_ts = exchange_timestamp,
                "tick feed gap — possible disconnection"
            );
            self.total_errors = self.total_errors.saturating_add(1);
            TickGapResult::Error { gap_secs }
        } else if gap_secs >= TICK_GAP_ALERT_THRESHOLD_SECS {
            warn!(
                security_id = security_id,
                gap_secs = gap_secs,
                last_ts = state.last_exchange_timestamp,
                current_ts = exchange_timestamp,
                "tick feed gap detected"
            );
            self.total_warnings = self.total_warnings.saturating_add(1);
            TickGapResult::Warning { gap_secs }
        } else {
            TickGapResult::Ok
        };

        state.last_exchange_timestamp = exchange_timestamp;
        result
    }

    /// Returns the total number of gap warnings emitted.
    pub fn total_warnings(&self) -> u64 {
        self.total_warnings
    }

    /// Returns the total number of gap errors emitted.
    pub fn total_errors(&self) -> u64 {
        self.total_errors
    }

    /// Returns the number of securities currently tracked.
    pub fn tracked_securities(&self) -> usize {
        self.states.len()
    }

    /// Resets all tracking state (call at daily reset).
    pub fn reset(&mut self) {
        self.states.clear();
        self.total_warnings = 0;
        self.total_errors = 0;
    }
}

/// Result of a tick gap check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickGapResult {
    /// No gap detected — tick within normal interval.
    Ok,
    /// Warning-level gap detected (> TICK_GAP_ALERT_THRESHOLD_SECS).
    Warning { gap_secs: u32 },
    /// Error-level gap detected (> TICK_GAP_ERROR_THRESHOLD_SECS).
    Error { gap_secs: u32 },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_empty() {
        let tracker = TickGapTracker::new(100);
        assert_eq!(tracker.tracked_securities(), 0);
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
    }

    #[test]
    fn warmup_phase_no_alerts() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // During warmup, even large gaps should not trigger alerts.
        for i in 0..TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            let result = tracker.record_tick(1001, base_ts + i * 100);
            assert_eq!(result, TickGapResult::Ok);
        }
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
    }

    #[test]
    fn normal_ticks_no_gap() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Fill warmup
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Normal 1-second intervals
        let post_warmup = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + 1;
        let result = tracker.record_tick(1001, post_warmup);
        assert_eq!(result, TickGapResult::Ok);
        assert_eq!(tracker.total_warnings(), 0);
    }

    #[test]
    fn warning_level_gap() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Fill warmup with 1-sec intervals
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Now send a tick with a gap at warning threshold
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        assert!(matches!(result, TickGapResult::Warning { .. }));
        assert_eq!(tracker.total_warnings(), 1);
        assert_eq!(tracker.total_errors(), 0);
    }

    #[test]
    fn error_level_gap() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Fill warmup
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Send a tick with error-level gap
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ERROR_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        assert!(matches!(result, TickGapResult::Error { .. }));
        assert_eq!(tracker.total_errors(), 1);
    }

    #[test]
    fn multiple_securities_independent() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Warm up both securities
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
            tracker.record_tick(1002, base_ts + i);
        }
        // Gap on security 1001 only
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        let r1 = tracker.record_tick(1001, gap_ts);
        assert!(matches!(r1, TickGapResult::Warning { .. }));

        // Normal tick on security 1002
        let normal_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + 1;
        let r2 = tracker.record_tick(1002, normal_ts);
        assert_eq!(r2, TickGapResult::Ok);

        assert_eq!(tracker.tracked_securities(), 2);
    }

    #[test]
    fn reset_clears_all() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        assert_eq!(tracker.tracked_securities(), 1);

        tracker.reset();
        assert_eq!(tracker.tracked_securities(), 0);
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
    }

    // -----------------------------------------------------------------------
    // Edge cases: out-of-order timestamps, gap_secs values, counters
    // -----------------------------------------------------------------------

    #[test]
    fn out_of_order_timestamp_returns_ok() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Fill warmup
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Send an older timestamp (out-of-order) — saturating_sub → gap = 0 → Ok
        let result = tracker.record_tick(1001, base_ts);
        assert_eq!(result, TickGapResult::Ok);
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
    }

    #[test]
    fn warning_result_carries_gap_seconds() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        match result {
            TickGapResult::Warning { gap_secs } => {
                assert_eq!(gap_secs, TICK_GAP_ALERT_THRESHOLD_SECS);
            }
            other => panic!("expected Warning, got {:?}", other),
        }
    }

    #[test]
    fn error_result_carries_gap_seconds() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ERROR_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        match result {
            TickGapResult::Error { gap_secs } => {
                assert_eq!(gap_secs, TICK_GAP_ERROR_THRESHOLD_SECS);
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn reset_mid_warmup_restarts_from_scratch() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Partial warmup
        for i in 0..TICK_GAP_MIN_TICKS_BEFORE_ACTIVE / 2 {
            tracker.record_tick(1001, base_ts + i);
        }
        tracker.reset();

        // After reset, warmup starts fresh — large gaps should still be suppressed
        let result = tracker.record_tick(1001, base_ts + 1_000_000);
        assert_eq!(result, TickGapResult::Ok);
        assert_eq!(tracker.total_warnings(), 0);
    }

    #[test]
    fn gap_just_below_warning_threshold_is_ok() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Gap one second below warning threshold
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS - 1;
        let result = tracker.record_tick(1001, gap_ts);
        assert_eq!(result, TickGapResult::Ok);
    }

    #[test]
    fn cumulative_counters_increment() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        let last = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE;
        // Two consecutive warning-level gaps
        tracker.record_tick(1001, last + TICK_GAP_ALERT_THRESHOLD_SECS);
        tracker.record_tick(
            1001,
            last + TICK_GAP_ALERT_THRESHOLD_SECS + TICK_GAP_ALERT_THRESHOLD_SECS,
        );
        assert_eq!(tracker.total_warnings(), 2);
    }

    #[test]
    fn zero_capacity_tracker_works() {
        let mut tracker = TickGapTracker::new(0);
        let result = tracker.record_tick(1001, 1_700_000_000);
        assert_eq!(result, TickGapResult::Ok);
        assert_eq!(tracker.tracked_securities(), 1);
    }
}
