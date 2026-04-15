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
use std::time::Instant;

use tickvault_common::constants::{
    STALE_LTP_THRESHOLD_SECS, TICK_GAP_ALERT_THRESHOLD_SECS, TICK_GAP_ERROR_THRESHOLD_SECS,
    TICK_GAP_MIN_TICKS_BEFORE_ACTIVE,
};
use tracing::{error, warn};

/// Per-security feed health state.
#[derive(Debug, Clone)]
struct SecurityFeedState {
    /// Last exchange timestamp seen (epoch seconds).
    last_exchange_timestamp: u32,
    /// Total ticks received for this security (used for warmup gate).
    tick_count: u32,
    /// Wall-clock instant of the last tick received (for stale LTP detection).
    last_wall_clock: Instant,
    /// Whether a stale alert has already been emitted for the current gap.
    /// Prevents duplicate alerts until the instrument resumes ticking.
    stale_alerted: bool,
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
    /// Total stale LTP alerts emitted (for metrics/alerting).
    total_stale_alerts: u64,
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
            total_stale_alerts: 0,
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
        let now = Instant::now();
        let state = self.states.entry(security_id).or_insert(SecurityFeedState {
            last_exchange_timestamp: exchange_timestamp,
            tick_count: 0,
            last_wall_clock: now,
            stale_alerted: false,
        });

        state.tick_count = state.tick_count.saturating_add(1);
        state.last_wall_clock = now;
        state.stale_alerted = false; // Reset stale flag on any new tick.

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

    /// Returns the total number of stale LTP alerts emitted.
    pub fn total_stale_alerts(&self) -> u64 {
        self.total_stale_alerts
    }

    /// Scans all tracked instruments for stale LTP (frozen >10 min wall-clock).
    ///
    /// Called periodically from the cold path (watchdog/heartbeat, NOT hot loop).
    /// O(n) where n = tracked securities — acceptable on cold path every 30s.
    ///
    /// Each stale instrument is alerted only once per staleness episode.
    /// When a new tick arrives, `stale_alerted` resets (in `record_tick`).
    ///
    /// Returns the count of newly-stale instruments detected in this scan.
    pub fn detect_stale_instruments(&mut self) -> u32 {
        let now = Instant::now();
        let threshold = std::time::Duration::from_secs(STALE_LTP_THRESHOLD_SECS);
        let mut newly_stale: u32 = 0;

        for (&security_id, state) in &mut self.states {
            // Only check instruments that have passed warmup.
            if state.tick_count <= TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
                continue;
            }

            let elapsed = now.duration_since(state.last_wall_clock);
            if elapsed >= threshold && !state.stale_alerted {
                error!(
                    security_id = security_id,
                    frozen_secs = elapsed.as_secs(),
                    last_exchange_ts = state.last_exchange_timestamp,
                    "stale LTP detected — no tick for >10 minutes"
                );
                state.stale_alerted = true;
                self.total_stale_alerts = self.total_stale_alerts.saturating_add(1);
                newly_stale = newly_stale.saturating_add(1);
            }
        }

        if newly_stale > 0 {
            metrics::gauge!("tv_stale_ltp_instruments").set(f64::from(newly_stale));
        }

        newly_stale
    }

    /// Resets all tracking state (call at daily reset).
    pub fn reset(&mut self) {
        self.states.clear();
        self.total_warnings = 0;
        self.total_errors = 0;
        self.total_stale_alerts = 0;
    }

    /// **P8.1 primitive** — snapshots the set of securities that have
    /// received at least one tick within the last `window_secs` of
    /// wall-clock time, along with each one's last exchange timestamp.
    ///
    /// Intended caller: the WebSocket reconnect handler. On every
    /// reconnect event the handler calls this and, for each returned
    /// `(security_id, last_exchange_ts)` pair, emits a
    /// `GapBackfillRequest` whose `from_ist_secs = last_exchange_ts`
    /// and `to_ist_secs = now_ist_secs`. The backfill worker then
    /// issues a one-shot historical-candle fetch to close the gap.
    ///
    /// Cold path — runs once per reconnect, not per tick. O(n) in
    /// the number of tracked securities (typically ≤ 25,000) — a
    /// full scan is acceptable because reconnects are rare (< 1/hr
    /// in healthy ops).
    ///
    /// # Arguments
    /// * `window_secs` — how far back to look. 300 s (5 min) is the
    ///   plan default: any security that was active in the last 5
    ///   minutes is worth backfilling.
    ///
    /// # Returns
    /// Pairs of `(security_id, last_exchange_timestamp)` suitable for
    /// feeding directly into [`GapBackfillRequest`] construction.
    /// Securities still in warmup are excluded (they have no real
    /// gap history yet).
    pub fn snapshot_active_window(&self, window_secs: u64) -> Vec<(u32, u32)> {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(window_secs);
        let mut out = Vec::with_capacity(self.states.len());
        for (&sid, state) in &self.states {
            if state.tick_count <= TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
                continue;
            }
            let age = now.duration_since(state.last_wall_clock);
            if age <= window {
                out.push((sid, state.last_exchange_timestamp));
            }
        }
        out
    }

    /// **P8.1 accompaniment** — records that a WebSocket reconnect
    /// event was observed. Emits a structured error log with the
    /// recently-active security count (which auto-fires Telegram via
    /// the Loki hook) and increments a Prometheus counter so Grafana
    /// can correlate reconnect frequency against missed-tick volume.
    ///
    /// Returns the list of recently-active securities for the caller
    /// to feed into the backfill pipeline.
    pub fn record_reconnect_event(
        &self,
        connection_label: &str,
        window_secs: u64,
    ) -> Vec<(u32, u32)> {
        let active = self.snapshot_active_window(window_secs);
        metrics::counter!(
            "tv_ws_reconnect_recently_active_securities_total",
            "label" => connection_label.to_string() // O(1) EXEMPT: cold path — fires once per reconnect
        )
        .increment(active.len() as u64);
        tracing::error!(
            connection_label = connection_label,
            recently_active_count = active.len(),
            window_secs = window_secs,
            "WS reconnect detected — backfill window opened for recently-active securities"
        );
        active
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

    // -----------------------------------------------------------------------
    // Stale LTP detection (M3)
    // -----------------------------------------------------------------------

    #[test]
    fn test_stale_ltp_detection() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        // Fill warmup so instruments become active
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
            tracker.record_tick(1002, base_ts + i);
        }

        // Immediately after ticks, nothing should be stale
        let stale = tracker.detect_stale_instruments();
        assert_eq!(stale, 0, "no instruments should be stale right after ticks");
        assert_eq!(tracker.total_stale_alerts(), 0);
    }

    #[test]
    fn stale_ltp_warmup_instruments_skipped() {
        let mut tracker = TickGapTracker::new(10);
        // Only 1 tick — still in warmup phase
        tracker.record_tick(1001, 1_700_000_000);

        // Even if wall-clock time passes, warmup instruments are not checked
        let stale = tracker.detect_stale_instruments();
        assert_eq!(stale, 0, "warmup instruments must be skipped");
    }

    #[test]
    fn stale_ltp_alert_only_once_per_episode() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        // Manually set wall clock to simulate staleness
        tracker.states.get_mut(&1001).unwrap().last_wall_clock =
            Instant::now() - std::time::Duration::from_secs(STALE_LTP_THRESHOLD_SECS + 1);

        let stale1 = tracker.detect_stale_instruments();
        assert_eq!(stale1, 1, "first scan should detect 1 stale instrument");
        assert_eq!(tracker.total_stale_alerts(), 1);

        // Second scan — already alerted, should NOT re-alert
        let stale2 = tracker.detect_stale_instruments();
        assert_eq!(stale2, 0, "second scan should not re-alert");
        assert_eq!(tracker.total_stale_alerts(), 1);
    }

    #[test]
    fn stale_ltp_resets_on_new_tick() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        // Simulate staleness
        tracker.states.get_mut(&1001).unwrap().last_wall_clock =
            Instant::now() - std::time::Duration::from_secs(STALE_LTP_THRESHOLD_SECS + 1);

        let stale = tracker.detect_stale_instruments();
        assert_eq!(stale, 1);
        assert!(tracker.states.get(&1001).unwrap().stale_alerted);

        // New tick arrives — resets stale flag
        tracker.record_tick(1001, base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + 100);
        assert!(
            !tracker.states.get(&1001).unwrap().stale_alerted,
            "stale flag must reset on new tick"
        );

        // Subsequent scan should not find stale (wall clock just refreshed)
        let stale2 = tracker.detect_stale_instruments();
        assert_eq!(stale2, 0);
    }

    #[test]
    fn stale_ltp_multiple_instruments_independent() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
            tracker.record_tick(1002, base_ts + i);
        }

        // Only 1001 goes stale
        tracker.states.get_mut(&1001).unwrap().last_wall_clock =
            Instant::now() - std::time::Duration::from_secs(STALE_LTP_THRESHOLD_SECS + 1);

        let stale = tracker.detect_stale_instruments();
        assert_eq!(stale, 1, "only 1001 should be stale");
        assert!(tracker.states.get(&1001).unwrap().stale_alerted);
        assert!(!tracker.states.get(&1002).unwrap().stale_alerted);
    }

    #[test]
    fn stale_ltp_reset_clears_stale_counter() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;

        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }

        tracker.states.get_mut(&1001).unwrap().last_wall_clock =
            Instant::now() - std::time::Duration::from_secs(STALE_LTP_THRESHOLD_SECS + 1);
        tracker.detect_stale_instruments();
        assert_eq!(tracker.total_stale_alerts(), 1);

        tracker.reset();
        assert_eq!(tracker.total_stale_alerts(), 0);
    }

    #[test]
    fn stale_ltp_threshold_constant_is_600_seconds() {
        assert_eq!(
            STALE_LTP_THRESHOLD_SECS, 600,
            "stale threshold must be 10 minutes"
        );
    }

    #[test]
    fn stale_ltp_counter_saturates_at_u64_max() {
        let mut tracker = TickGapTracker::new(10);
        tracker.total_stale_alerts = u64::MAX;
        let base_ts = 1_700_000_000;

        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        tracker.states.get_mut(&1001).unwrap().last_wall_clock =
            Instant::now() - std::time::Duration::from_secs(STALE_LTP_THRESHOLD_SECS + 1);

        tracker.detect_stale_instruments();
        assert_eq!(tracker.total_stale_alerts(), u64::MAX);
    }

    // ------------------------------------------------------------------
    // P8.1 — snapshot_active_window + record_reconnect_event
    // ------------------------------------------------------------------

    #[test]
    fn test_tick_gap_tracker_snapshot_active_window_empty_tracker_returns_empty() {
        let tracker = TickGapTracker::new(10);
        let snapshot = tracker.snapshot_active_window(300);
        assert!(
            snapshot.is_empty(),
            "empty tracker must produce empty active-window snapshot"
        );
    }

    #[test]
    fn test_tick_gap_tracker_snapshot_active_window_excludes_warmup_securities() {
        let mut tracker = TickGapTracker::new(10);
        // One security, still in warmup (tick_count ≤ TICK_GAP_MIN_TICKS_BEFORE_ACTIVE).
        for i in 0..TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, 1_700_000_000 + i);
        }
        let snapshot = tracker.snapshot_active_window(300);
        assert!(
            snapshot.is_empty(),
            "warmup-only security must not appear in active window"
        );
    }

    #[test]
    fn test_tick_gap_tracker_snapshot_active_window_includes_active_security() {
        let mut tracker = TickGapTracker::new(10);
        // Push past warmup.
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(2002, base_ts + i);
        }
        let snapshot = tracker.snapshot_active_window(300);
        assert_eq!(
            snapshot.len(),
            1,
            "active security must appear exactly once in snapshot"
        );
        assert_eq!(snapshot[0].0, 2002);
        assert_eq!(
            snapshot[0].1,
            base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE,
            "last_exchange_timestamp must match the most recent tick"
        );
    }

    #[test]
    fn test_tick_gap_tracker_snapshot_active_window_excludes_stale_security() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(3003, base_ts + i);
        }
        // Artificially age the last_wall_clock so it falls outside the window.
        tracker.states.get_mut(&3003).unwrap().last_wall_clock =
            Instant::now() - std::time::Duration::from_secs(600);
        let snapshot = tracker.snapshot_active_window(300);
        assert!(
            snapshot.is_empty(),
            "security last seen >5 min ago must not be in 300s window"
        );
    }

    #[test]
    fn test_tick_gap_tracker_record_reconnect_event_returns_active_securities() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Two distinct active securities.
        for sid in [4004, 5005] {
            for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
                tracker.record_tick(sid, base_ts + i);
            }
        }
        let active = tracker.record_reconnect_event("live-feed-0", 300);
        assert_eq!(
            active.len(),
            2,
            "both active securities must be returned on reconnect event"
        );
        let ids: Vec<u32> = active.iter().map(|(s, _)| *s).collect();
        assert!(ids.contains(&4004));
        assert!(ids.contains(&5005));
    }
}
