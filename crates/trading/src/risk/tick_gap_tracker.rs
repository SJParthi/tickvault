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

    // -----------------------------------------------------------------------
    // Warmup boundary: exact threshold tick
    // -----------------------------------------------------------------------

    #[test]
    fn warmup_boundary_exact_threshold_tick_still_suppressed() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        // Feed exactly TICK_GAP_MIN_TICKS_BEFORE_ACTIVE ticks with large gaps.
        // The tick_count goes from 1..=threshold; tick at threshold should still be suppressed.
        for i in 0..TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            let result = tracker.record_tick(1001, base_ts + i * 1000); // 1000s gaps
            assert_eq!(
                result,
                TickGapResult::Ok,
                "warmup tick {} must be Ok",
                i + 1
            );
        }
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
    }

    #[test]
    fn warmup_boundary_first_post_warmup_tick_can_detect_gap() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        // Fill warmup with 1-sec intervals
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        // The next tick (TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + 1) is the first
        // post-warmup tick that CAN detect a gap.
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        assert!(
            matches!(result, TickGapResult::Warning { .. }),
            "first post-warmup tick must detect warning gap"
        );
    }

    // -----------------------------------------------------------------------
    // Counter saturation at u64::MAX
    // -----------------------------------------------------------------------

    #[test]
    fn warning_counter_saturates_at_u64_max() {
        let mut tracker = TickGapTracker::new(10);
        // Pre-set counter to near max
        tracker.total_warnings = u64::MAX - 1;

        let base_ts = 1_700_000_000;
        // Fill warmup
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        // Trigger warning
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        tracker.record_tick(1001, gap_ts);
        assert_eq!(tracker.total_warnings(), u64::MAX);

        // Another warning should saturate at MAX, not overflow
        let gap_ts2 = gap_ts + TICK_GAP_ALERT_THRESHOLD_SECS;
        tracker.record_tick(1001, gap_ts2);
        assert_eq!(tracker.total_warnings(), u64::MAX);
    }

    #[test]
    fn error_counter_saturates_at_u64_max() {
        let mut tracker = TickGapTracker::new(10);
        tracker.total_errors = u64::MAX - 1;

        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ERROR_THRESHOLD_SECS;
        tracker.record_tick(1001, gap_ts);
        assert_eq!(tracker.total_errors(), u64::MAX);

        let gap_ts2 = gap_ts + TICK_GAP_ERROR_THRESHOLD_SECS;
        tracker.record_tick(1001, gap_ts2);
        assert_eq!(tracker.total_errors(), u64::MAX);
    }

    // -----------------------------------------------------------------------
    // Tick count saturation at u32::MAX
    // -----------------------------------------------------------------------

    #[test]
    fn tick_count_saturates_at_u32_max() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        // Insert first tick to create the entry
        tracker.record_tick(1001, base_ts);

        // Set tick_count to near max
        tracker.states.get_mut(&1001).unwrap().tick_count = u32::MAX - 1;

        // Two more ticks should saturate at MAX, not overflow
        tracker.record_tick(1001, base_ts + 1);
        assert_eq!(tracker.states.get(&1001).unwrap().tick_count, u32::MAX);

        tracker.record_tick(1001, base_ts + 2);
        assert_eq!(tracker.states.get(&1001).unwrap().tick_count, u32::MAX);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: TickGapResult variants, gap between warning and error
    // thresholds, many securities tracking, reset after gaps
    // -----------------------------------------------------------------------

    #[test]
    fn gap_between_warning_and_error_threshold_is_warning() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Gap exactly at (alert + error) / 2 — should be Warning not Error
        let mid_gap = (TICK_GAP_ALERT_THRESHOLD_SECS + TICK_GAP_ERROR_THRESHOLD_SECS) / 2;
        if mid_gap >= TICK_GAP_ALERT_THRESHOLD_SECS && mid_gap < TICK_GAP_ERROR_THRESHOLD_SECS {
            let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + mid_gap;
            let result = tracker.record_tick(1001, gap_ts);
            assert!(
                matches!(result, TickGapResult::Warning { .. }),
                "gap between alert and error threshold must be Warning"
            );
        }
    }

    #[test]
    fn gap_one_below_error_threshold_is_warning() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ERROR_THRESHOLD_SECS - 1;
        let result = tracker.record_tick(1001, gap_ts);
        // If error - 1 >= alert, it should be Warning
        if TICK_GAP_ERROR_THRESHOLD_SECS - 1 >= TICK_GAP_ALERT_THRESHOLD_SECS {
            assert!(matches!(result, TickGapResult::Warning { .. }));
        }
    }

    #[test]
    fn many_securities_all_tracked() {
        let mut tracker = TickGapTracker::new(100);
        let base_ts = 1_700_000_000;
        for sid in 1..=50 {
            tracker.record_tick(sid, base_ts);
        }
        assert_eq!(tracker.tracked_securities(), 50);
    }

    #[test]
    fn reset_after_warnings_clears_counters() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Generate a warning
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        tracker.record_tick(1001, gap_ts);
        assert_eq!(tracker.total_warnings(), 1);

        tracker.reset();
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
        assert_eq!(tracker.tracked_securities(), 0);
    }

    #[test]
    fn tick_gap_result_equality() {
        assert_eq!(TickGapResult::Ok, TickGapResult::Ok);
        assert_eq!(
            TickGapResult::Warning { gap_secs: 10 },
            TickGapResult::Warning { gap_secs: 10 }
        );
        assert_eq!(
            TickGapResult::Error { gap_secs: 60 },
            TickGapResult::Error { gap_secs: 60 }
        );
        assert_ne!(TickGapResult::Ok, TickGapResult::Warning { gap_secs: 10 });
        assert_ne!(
            TickGapResult::Warning { gap_secs: 10 },
            TickGapResult::Error { gap_secs: 10 }
        );
    }

    #[test]
    fn tick_gap_result_debug() {
        let ok = format!("{:?}", TickGapResult::Ok);
        assert_eq!(ok, "Ok");
        let warn = format!("{:?}", TickGapResult::Warning { gap_secs: 15 });
        assert!(warn.contains("Warning"));
        assert!(warn.contains("15"));
    }

    // -----------------------------------------------------------------------
    // Additional coverage: warmup boundary exact count, counter saturation,
    // multiple gaps in sequence, reset mid-tracking
    // -----------------------------------------------------------------------

    #[test]
    fn warmup_exact_threshold_tick_transitions_to_active() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        // Feed exactly TICK_GAP_MIN_TICKS_BEFORE_ACTIVE ticks (tick_count goes 1..=threshold)
        for i in 0..TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        // The NEXT tick (threshold+1) is the first active tick
        // Feed it with a huge gap → should detect warning
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE - 1 + TICK_GAP_ALERT_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        assert!(
            matches!(result, TickGapResult::Warning { .. }),
            "first tick after warmup must detect gap"
        );
    }

    #[test]
    fn consecutive_errors_increment_counter() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        // Warmup
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        let mut ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE;
        // Three consecutive error-level gaps
        for _ in 0..3 {
            ts += TICK_GAP_ERROR_THRESHOLD_SECS;
            tracker.record_tick(1001, ts);
        }
        assert_eq!(tracker.total_errors(), 3);
    }

    #[test]
    fn reset_mid_active_tracking_restarts_warmup() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        // Full warmup + active tracking
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + 5 {
            tracker.record_tick(1001, base_ts + i);
        }
        assert_eq!(tracker.tracked_securities(), 1);

        // Reset
        tracker.reset();
        assert_eq!(tracker.tracked_securities(), 0);

        // After reset, warmup suppresses gaps again
        for i in 0..TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            let result = tracker.record_tick(1001, base_ts + 100_000 + i * 1000);
            assert_eq!(
                result,
                TickGapResult::Ok,
                "post-reset warmup must suppress gaps"
            );
        }
    }

    #[test]
    fn same_timestamp_after_warmup_is_ok() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Same timestamp as last → gap_secs = 0 → Ok
        let result = tracker.record_tick(1001, base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE);
        assert_eq!(result, TickGapResult::Ok);
    }
}
