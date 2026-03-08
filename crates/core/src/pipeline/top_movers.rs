//! Top gainers, losers, and most-active tracker for live ticks.
//!
//! Tracks per-security percentage change from previous close (`day_close` in `ParsedTick`)
//! and periodically computes ranked snapshots of the top 20 gainers, losers, and
//! most active by volume.
//!
//! # Performance
//! - O(1) per tick: HashMap lookup + arithmetic update (hot path)
//! - O(N log N) snapshot computation every 5 seconds (cold path, ~2000 instruments)

use std::collections::HashMap;

use tracing::debug;

use dhan_live_trader_common::tick_types::ParsedTick;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Initial capacity for the tracker HashMap.
const TRACKER_MAP_INITIAL_CAPACITY: usize = 4096;

/// Number of top entries to include in each snapshot list.
const TOP_N: usize = 20;

/// Minimum absolute change percentage to qualify as a mover (filters noise).
const MIN_CHANGE_PCT_THRESHOLD: f32 = 0.01;

// ---------------------------------------------------------------------------
// SecurityState — per-security tracking state
// ---------------------------------------------------------------------------

/// Per-security price state for change calculation.
#[derive(Debug, Clone, Copy)]
struct SecurityState {
    /// Latest traded price.
    last_traded_price: f32,
    /// Percentage change from previous close.
    change_pct: f32,
    /// Latest cumulative volume.
    volume: u32,
    /// Exchange segment code.
    exchange_segment_code: u8,
}

// ---------------------------------------------------------------------------
// MoverEntry — public snapshot entry
// ---------------------------------------------------------------------------

/// A single entry in the top movers snapshot.
#[derive(Debug, Clone, Copy)]
pub struct MoverEntry {
    /// Dhan security identifier.
    pub security_id: u32,
    /// Exchange segment code.
    pub exchange_segment_code: u8,
    /// Last traded price.
    pub last_traded_price: f32,
    /// Percentage change from previous close.
    pub change_pct: f32,
    /// Cumulative volume.
    pub volume: u32,
}

// ---------------------------------------------------------------------------
// TopMoversSnapshot — periodic ranked output
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of top gainers, losers, and most active securities.
#[derive(Debug, Clone)]
pub struct TopMoversSnapshot {
    /// Top N gainers sorted by change_pct descending.
    pub gainers: Vec<MoverEntry>,
    /// Top N losers sorted by change_pct ascending.
    pub losers: Vec<MoverEntry>,
    /// Top N most active sorted by volume descending.
    pub most_active: Vec<MoverEntry>,
    /// Total securities being tracked.
    pub total_tracked: usize,
}

// ---------------------------------------------------------------------------
// TopMoversTracker
// ---------------------------------------------------------------------------

/// Real-time tracker for top gainers, losers, and most active securities.
///
/// Updated O(1) per tick. Snapshot computation is O(N log N) but only runs
/// periodically on the cold path (every 5 seconds).
pub struct TopMoversTracker {
    /// Per-security state keyed by `(security_id, segment_code)`.
    securities: HashMap<(u32, u8), SecurityState>,
    /// Latest snapshot (updated periodically).
    latest_snapshot: Option<TopMoversSnapshot>,
    /// Total ticks processed.
    ticks_processed: u64,
}

impl Default for TopMoversTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TopMoversTracker {
    /// Creates a new tracker with pre-allocated capacity.
    pub fn new() -> Self {
        Self {
            securities: HashMap::with_capacity(TRACKER_MAP_INITIAL_CAPACITY),
            latest_snapshot: None,
            ticks_processed: 0,
        }
    }

    /// Updates the tracker with a new tick.
    ///
    /// Only processes ticks with valid `day_close` (previous close > 0).
    ///
    /// # Performance
    /// O(1) — single HashMap lookup + one division.
    #[inline]
    pub fn update(&mut self, tick: &ParsedTick) {
        self.ticks_processed = self.ticks_processed.saturating_add(1);

        // Skip ticks without valid previous close (day_close = 0 for Ticker-only feeds)
        if !tick.day_close.is_finite() || tick.day_close <= 0.0 {
            return;
        }

        let key = (tick.security_id, tick.exchange_segment_code);

        // Calculate percentage change: ((LTP - prev_close) / prev_close) * 100
        let change_pct = ((tick.last_traded_price - tick.day_close) / tick.day_close) * 100.0;

        if let Some(state) = self.securities.get_mut(&key) {
            state.last_traded_price = tick.last_traded_price;
            state.change_pct = change_pct;
            state.volume = tick.volume;
        } else {
            self.securities.insert(
                key,
                SecurityState {
                    last_traded_price: tick.last_traded_price,
                    change_pct,
                    volume: tick.volume,
                    exchange_segment_code: tick.exchange_segment_code,
                },
            );
        }
    }

    /// Computes a ranked snapshot of top gainers, losers, and most active.
    ///
    /// # Performance
    /// O(N log N) where N is the number of tracked securities. Only call periodically.
    pub fn compute_snapshot(&mut self) -> TopMoversSnapshot {
        let total_tracked = self.securities.len();

        // Build entries from all tracked securities
        let mut entries: Vec<MoverEntry> = self
            .securities
            .iter()
            .filter(|(_, state)| state.change_pct.is_finite())
            .map(|(&(security_id, _), state)| MoverEntry {
                security_id,
                exchange_segment_code: state.exchange_segment_code,
                last_traded_price: state.last_traded_price,
                change_pct: state.change_pct,
                volume: state.volume,
            })
            .collect();

        // Gainers: sort by change_pct descending, take top N
        entries.sort_unstable_by(|a, b| {
            b.change_pct
                .partial_cmp(&a.change_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let gainers: Vec<MoverEntry> = entries
            .iter()
            .filter(|e| e.change_pct > MIN_CHANGE_PCT_THRESHOLD)
            .take(TOP_N)
            .copied()
            .collect();

        // Losers: sort by change_pct ascending, take top N
        entries.sort_unstable_by(|a, b| {
            a.change_pct
                .partial_cmp(&b.change_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let losers: Vec<MoverEntry> = entries
            .iter()
            .filter(|e| e.change_pct < -MIN_CHANGE_PCT_THRESHOLD)
            .take(TOP_N)
            .copied()
            .collect();

        // Most active: sort by volume descending, take top N
        entries.sort_unstable_by(|a, b| b.volume.cmp(&a.volume));
        let most_active: Vec<MoverEntry> = entries.iter().take(TOP_N).copied().collect();

        let snapshot = TopMoversSnapshot {
            gainers,
            losers,
            most_active,
            total_tracked,
        };

        debug!(
            total_tracked,
            gainers = snapshot.gainers.len(),
            losers = snapshot.losers.len(),
            most_active = snapshot.most_active.len(),
            "top movers snapshot computed"
        );

        self.latest_snapshot = Some(snapshot.clone());
        snapshot
    }

    /// Returns the latest snapshot (if any has been computed).
    pub fn latest_snapshot(&self) -> Option<&TopMoversSnapshot> {
        self.latest_snapshot.as_ref()
    }

    /// Returns the number of securities being tracked.
    pub fn tracked_count(&self) -> usize {
        self.securities.len()
    }

    /// Returns total ticks processed.
    pub fn ticks_processed(&self) -> u64 {
        self.ticks_processed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic
mod tests {
    use super::*;

    fn make_tick(
        security_id: u32,
        segment: u8,
        ltp: f32,
        prev_close: f32,
        volume: u32,
    ) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment,
            last_traded_price: ltp,
            day_close: prev_close,
            volume,
            exchange_timestamp: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn new_tracker_is_empty() {
        let tracker = TopMoversTracker::new();
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.ticks_processed(), 0);
        assert!(tracker.latest_snapshot().is_none());
    }

    #[test]
    fn tick_with_valid_close_is_tracked() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, 100.0, 1000));
        assert_eq!(tracker.tracked_count(), 1);
        assert_eq!(tracker.ticks_processed(), 1);
    }

    #[test]
    fn tick_without_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, 0.0, 1000)); // No prev close
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.ticks_processed(), 1);
    }

    #[test]
    fn change_pct_calculated_correctly() {
        let mut tracker = TopMoversTracker::new();
        // LTP=110, prev_close=100 → +10%
        tracker.update(&make_tick(100, 2, 110.0, 100.0, 1000));

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.gainers.len(), 1);
        let pct = snapshot.gainers[0].change_pct;
        assert!((pct - 10.0).abs() < 0.01, "expected ~10%, got {pct}");
    }

    #[test]
    fn gainers_sorted_descending() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 105.0, 100.0, 100)); // +5%
        tracker.update(&make_tick(2, 2, 120.0, 100.0, 200)); // +20%
        tracker.update(&make_tick(3, 2, 110.0, 100.0, 300)); // +10%

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.gainers.len(), 3);
        assert_eq!(snapshot.gainers[0].security_id, 2); // +20%
        assert_eq!(snapshot.gainers[1].security_id, 3); // +10%
        assert_eq!(snapshot.gainers[2].security_id, 1); // +5%
    }

    #[test]
    fn losers_sorted_ascending() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 95.0, 100.0, 100)); // -5%
        tracker.update(&make_tick(2, 2, 80.0, 100.0, 200)); // -20%
        tracker.update(&make_tick(3, 2, 90.0, 100.0, 300)); // -10%

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.losers.len(), 3);
        assert_eq!(snapshot.losers[0].security_id, 2); // -20%
        assert_eq!(snapshot.losers[1].security_id, 3); // -10%
        assert_eq!(snapshot.losers[2].security_id, 1); // -5%
    }

    #[test]
    fn most_active_sorted_by_volume() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 100.0, 100.0, 500));
        tracker.update(&make_tick(2, 2, 100.0, 100.0, 5000));
        tracker.update(&make_tick(3, 2, 100.0, 100.0, 1000));

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.most_active.len(), 3);
        assert_eq!(snapshot.most_active[0].security_id, 2); // 5000
        assert_eq!(snapshot.most_active[1].security_id, 3); // 1000
        assert_eq!(snapshot.most_active[2].security_id, 1); // 500
    }

    #[test]
    fn snapshot_caps_at_top_n() {
        let mut tracker = TopMoversTracker::new();
        // Create 30 gainers
        for i in 1..=30u32 {
            let ltp = 100.0 + (i as f32);
            tracker.update(&make_tick(i, 2, ltp, 100.0, i * 100));
        }

        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.gainers.len() <= TOP_N);
        assert!(snapshot.most_active.len() <= TOP_N);
    }

    #[test]
    fn snapshot_computation_updates_latest() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000));

        assert!(tracker.latest_snapshot().is_none());
        tracker.compute_snapshot();
        assert!(tracker.latest_snapshot().is_some());
    }

    #[test]
    fn constants_are_reasonable() {
        assert!(TOP_N >= 5 && TOP_N <= 100);
        assert!(MIN_CHANGE_PCT_THRESHOLD > 0.0 && MIN_CHANGE_PCT_THRESHOLD < 1.0);
        assert!(TRACKER_MAP_INITIAL_CAPACITY >= 1024);
    }
}
