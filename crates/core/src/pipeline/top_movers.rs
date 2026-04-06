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
use std::sync::{Arc, RwLock};

use tracing::debug;

use dhan_live_trader_common::tick_types::ParsedTick;

/// Shared handle for the latest top movers snapshot.
///
/// Written by the tick processor (cold path, every 5s).
/// Read by the API handler (cold path, on request).
pub type SharedTopMoversSnapshot = Arc<RwLock<Option<TopMoversSnapshot>>>;

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
#[derive(Debug, Clone, Copy, serde::Serialize)]
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
#[derive(Debug, Clone, serde::Serialize)]
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
///
/// Previous close prices are populated from PrevClose WebSocket packets
/// (code 6) which arrive at subscription time. The `day_close` field in
/// `ParsedTick` is NOT used because the exchange only sets it post-market.
pub struct TopMoversTracker {
    /// Per-security state keyed by `(security_id, segment_code)`.
    securities: HashMap<(u32, u8), SecurityState>,
    /// Previous close prices from PrevClose packets, keyed by `(security_id, segment_code)`.
    /// Populated via `update_prev_close()` when PrevClose packets arrive.
    prev_close_prices: HashMap<(u32, u8), f32>,
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
            prev_close_prices: HashMap::with_capacity(TRACKER_MAP_INITIAL_CAPACITY),
            latest_snapshot: None,
            ticks_processed: 0,
        }
    }

    /// Stores previous close price from a PrevClose WebSocket packet (code 6).
    ///
    /// Called by tick_processor when PrevClose packets arrive (typically at boot).
    /// This sets the baseline for change% calculations in `update()`.
    ///
    /// # Performance
    /// O(1) — single HashMap insert.
    pub fn update_prev_close(&mut self, security_id: u32, segment_code: u8, prev_close: f32) {
        if prev_close.is_finite() && prev_close > 0.0 {
            self.prev_close_prices
                .insert((security_id, segment_code), prev_close);
        }
    }

    /// Updates the tracker with a new tick.
    ///
    /// Uses previous close from PrevClose packets (populated via `update_prev_close()`).
    /// Falls back to `tick.day_close` if available (post-market only).
    /// Skips ticks without any valid previous close baseline.
    ///
    /// # Performance
    /// O(1) — single HashMap lookup + one division.
    #[inline]
    pub fn update(&mut self, tick: &ParsedTick) {
        // Reject ticks with non-finite or non-positive LTP (NaN, Inf, 0, negative)
        if !tick.last_traded_price.is_finite() || tick.last_traded_price <= 0.0 {
            return;
        }

        self.ticks_processed = self.ticks_processed.saturating_add(1);

        let key = (tick.security_id, tick.exchange_segment_code);

        // Look up previous close: prefer PrevClose packet, fall back to day_close
        let prev_close = if let Some(&pc) = self.prev_close_prices.get(&key) {
            pc
        } else if tick.day_close.is_finite() && tick.day_close > 0.0 {
            tick.day_close
        } else {
            return; // No baseline — skip
        };

        // Calculate percentage change: ((LTP - prev_close) / prev_close) * 100
        let change_pct = ((tick.last_traded_price - prev_close) / prev_close) * 100.0;

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

        // O(1) EXEMPT: cold path (every 5s). Clone copies ~1KB (3 × 20 Copy entries).
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
        const _: () = assert!(TOP_N >= 5 && TOP_N <= 100);
        const _: () = assert!(MIN_CHANGE_PCT_THRESHOLD > 0.0 && MIN_CHANGE_PCT_THRESHOLD < 1.0);
        const _: () = assert!(TRACKER_MAP_INITIAL_CAPACITY >= 1024);
    }

    #[test]
    fn default_impl_matches_new() {
        let a = TopMoversTracker::new();
        let b = TopMoversTracker::default();
        assert_eq!(a.tracked_count(), b.tracked_count());
        assert_eq!(a.ticks_processed(), b.ticks_processed());
    }

    #[test]
    fn negative_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, -100.0, 1000));
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn nan_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, f32::NAN, 1000));
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn infinity_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, f32::INFINITY, 1000));
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn price_update_replaces_previous() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, 100.0, 1000)); // +10%
        tracker.update(&make_tick(100, 2, 120.0, 100.0, 2000)); // +20%

        assert_eq!(tracker.tracked_count(), 1); // Same security
        let snapshot = tracker.compute_snapshot();
        let pct = snapshot.gainers[0].change_pct;
        assert!((pct - 20.0).abs() < 0.01, "should use latest price");
    }

    #[test]
    fn flat_security_excluded_from_gainers_and_losers() {
        let mut tracker = TopMoversTracker::new();
        // Price == prev_close → 0% change (below threshold)
        tracker.update(&make_tick(100, 2, 100.0, 100.0, 1000));

        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.gainers.is_empty(),
            "flat security should not be a gainer"
        );
        assert!(
            snapshot.losers.is_empty(),
            "flat security should not be a loser"
        );
        assert_eq!(
            snapshot.most_active.len(),
            1,
            "flat security still most active"
        );
    }

    #[test]
    fn mover_entry_is_copy() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.0,
            change_pct: 5.0,
            volume: 1000,
        };
        let copy = entry;
        assert_eq!(entry.security_id, copy.security_id);
        assert_eq!(entry.change_pct, copy.change_pct);
    }

    #[test]
    fn snapshot_total_tracked_matches() {
        let mut tracker = TopMoversTracker::new();
        for i in 1..=10u32 {
            tracker.update(&make_tick(i, 2, 100.0 + i as f32, 100.0, i * 100));
        }

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.total_tracked, 10);
    }

    #[test]
    fn same_security_different_segments() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, 100.0, 1000)); // NSE_EQ
        tracker.update(&make_tick(100, 2, 120.0, 100.0, 2000)); // NSE_FNO

        assert_eq!(
            tracker.tracked_count(),
            2,
            "different segments should be tracked separately"
        );
    }

    #[test]
    fn empty_snapshot_when_no_data() {
        let mut tracker = TopMoversTracker::new();
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.gainers.is_empty());
        assert!(snapshot.losers.is_empty());
        assert!(snapshot.most_active.is_empty());
        assert_eq!(snapshot.total_tracked, 0);
    }

    #[test]
    fn large_negative_change() {
        let mut tracker = TopMoversTracker::new();
        // Circuit breaker scenario: -20% drop
        tracker.update(&make_tick(100, 2, 80.0, 100.0, 5000));

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.losers.len(), 1);
        let pct = snapshot.losers[0].change_pct;
        assert!((pct - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn ticks_processed_counts_all_including_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000)); // Tracked
        tracker.update(&make_tick(2, 2, 110.0, 0.0, 1000)); // Skipped (no close)
        tracker.update(&make_tick(3, 2, 110.0, -1.0, 1000)); // Skipped (negative close)

        assert_eq!(tracker.ticks_processed(), 3);
        assert_eq!(tracker.tracked_count(), 1);
    }

    #[test]
    fn consecutive_snapshots_independent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000));

        let s1 = tracker.compute_snapshot();
        assert_eq!(s1.gainers.len(), 1);

        // Update price to flat
        tracker.update(&make_tick(1, 2, 100.0, 100.0, 1000));
        let s2 = tracker.compute_snapshot();
        assert!(s2.gainers.is_empty(), "should reflect updated price");
    }

    // -----------------------------------------------------------------------
    // NaN change_pct is filtered from snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn nan_change_pct_excluded_from_snapshot() {
        let mut tracker = TopMoversTracker::new();
        // LTP = NaN results in NaN change_pct which should be filtered
        tracker.update(&make_tick(100, 2, f32::NAN, 100.0, 1000));

        // NaN day_close is rejected (tracked_count = 0)
        // Let's instead produce a NaN change_pct via valid inputs:
        // day_close=f32::MIN_POSITIVE → change_pct=(NaN-close)/close but LTP must be NaN
        // Actually, day_close <= 0.0 is already skipped. So we need day_close > 0 and
        // LTP that produces NaN change_pct. But (ltp - close) / close for finite values is finite.
        // So NaN change_pct only comes from NaN LTP. But NaN LTP is finite? No: NaN.is_finite()=false.
        // Actually the update() function does NOT check LTP for finiteness — only day_close.
        // So NaN LTP with valid day_close will produce NaN change_pct.
        let mut tracker2 = TopMoversTracker::new();
        // This tick has NaN LTP but valid close → change_pct = NaN
        let tick = ParsedTick {
            security_id: 200,
            exchange_segment_code: 2,
            last_traded_price: f32::NAN,
            day_close: 100.0,
            volume: 500,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker2.update(&tick);
        assert_eq!(tracker2.tracked_count(), 1, "tick should be tracked");

        let snapshot = tracker2.compute_snapshot();
        // NaN change_pct entries are filtered out of gainers/losers
        assert!(
            snapshot.gainers.is_empty(),
            "NaN change_pct should not appear in gainers"
        );
        assert!(
            snapshot.losers.is_empty(),
            "NaN change_pct should not appear in losers"
        );
    }

    // -----------------------------------------------------------------------
    // Update existing vs new security
    // -----------------------------------------------------------------------

    #[test]
    fn update_existing_security_replaces_state() {
        let mut tracker = TopMoversTracker::new();

        // First update: LTP=110, volume=1000
        tracker.update(&make_tick(42, 2, 110.0, 100.0, 1000));
        assert_eq!(tracker.tracked_count(), 1);

        // Second update same security: LTP=120, volume=2000
        tracker.update(&make_tick(42, 2, 120.0, 100.0, 2000));
        assert_eq!(
            tracker.tracked_count(),
            1,
            "same security — count unchanged"
        );

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.gainers.len(), 1);
        // Should use the latest values
        let entry = &snapshot.gainers[0];
        assert!((entry.change_pct - 20.0).abs() < 0.01);
        assert_eq!(entry.volume, 2000);
        assert!((entry.last_traded_price - 120.0).abs() < 0.01);
    }

    #[test]
    fn new_security_inserted() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000));
        tracker.update(&make_tick(2, 2, 120.0, 100.0, 2000));
        assert_eq!(tracker.tracked_count(), 2);
    }

    // -----------------------------------------------------------------------
    // MoverEntry serialization
    // -----------------------------------------------------------------------

    #[test]
    fn mover_entry_serializes_to_json() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.5,
            change_pct: 5.25,
            volume: 1000,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("42"));
        assert!(json.contains("5.25"));
    }

    // -----------------------------------------------------------------------
    // TopMoversSnapshot serialization
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_serializes_to_json() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000));
        let snapshot = tracker.compute_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("gainers"));
        assert!(json.contains("losers"));
        assert!(json.contains("most_active"));
        assert!(json.contains("total_tracked"));
    }

    // -----------------------------------------------------------------------
    // Additional coverage: invalid day_close values, total_tracked sums,
    // instrument counts
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_day_close_neg_infinity_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, f32::NEG_INFINITY, 1000));
        assert_eq!(
            tracker.tracked_count(),
            0,
            "negative infinity day_close must be skipped"
        );
    }

    #[test]
    fn invalid_day_close_very_small_positive_accepted() {
        // day_close = f32::MIN_POSITIVE (smallest positive finite) is valid
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, f32::MIN_POSITIVE, 1000));
        assert_eq!(
            tracker.tracked_count(),
            1,
            "tiny positive day_close is finite and > 0, should be tracked"
        );
    }

    #[test]
    fn invalid_day_close_negative_zero_skipped() {
        // -0.0 <= 0.0 is true in IEEE 754, so it should be skipped
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 2, 110.0, -0.0, 1000));
        assert_eq!(
            tracker.tracked_count(),
            0,
            "-0.0 day_close must be skipped (equal to 0.0)"
        );
    }

    #[test]
    fn total_tracked_reflects_distinct_instrument_count() {
        let mut tracker = TopMoversTracker::new();
        // 5 distinct securities
        for i in 1..=5u32 {
            tracker.update(&make_tick(i, 2, 100.0 + (i as f32), 100.0, i * 100));
        }
        assert_eq!(tracker.tracked_count(), 5);

        let snapshot = tracker.compute_snapshot();
        assert_eq!(
            snapshot.total_tracked, 5,
            "snapshot total_tracked must equal tracked_count"
        );
    }

    #[test]
    fn total_instruments_sums_gainers_losers_most_active() {
        let mut tracker = TopMoversTracker::new();
        // 3 gainers
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000)); // +10%
        tracker.update(&make_tick(2, 2, 120.0, 100.0, 2000)); // +20%
        tracker.update(&make_tick(3, 2, 105.0, 100.0, 3000)); // +5%
        // 2 losers
        tracker.update(&make_tick(4, 2, 90.0, 100.0, 4000)); // -10%
        tracker.update(&make_tick(5, 2, 80.0, 100.0, 5000)); // -20%
        // 1 flat (excluded from gainers/losers but included in most_active)
        tracker.update(&make_tick(6, 2, 100.0, 100.0, 6000)); // 0%

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.gainers.len(), 3, "3 gainers");
        assert_eq!(snapshot.losers.len(), 2, "2 losers");
        assert_eq!(
            snapshot.most_active.len(),
            6,
            "all 6 securities in most_active"
        );
        assert_eq!(
            snapshot.total_tracked, 6,
            "total_tracked = all distinct instruments"
        );
    }

    #[test]
    fn snapshot_most_active_includes_flat_securities() {
        // Flat securities (0% change) appear in most_active but not
        // in gainers or losers
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 100.0, 100.0, 9999));
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.gainers.is_empty());
        assert!(snapshot.losers.is_empty());
        assert_eq!(snapshot.most_active.len(), 1);
        assert_eq!(snapshot.most_active[0].security_id, 1);
        assert_eq!(snapshot.most_active[0].volume, 9999);
    }

    #[test]
    fn ticks_processed_saturates_at_u64_max() {
        let mut tracker = TopMoversTracker::new();
        // Manually verify saturating_add behavior by sending many ticks
        for _ in 0..10 {
            tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000));
        }
        assert_eq!(tracker.ticks_processed(), 10);
    }

    #[test]
    fn snapshot_with_nan_change_pct_excluded_from_all_lists() {
        // NaN LTP with valid day_close produces NaN change_pct.
        // NaN entries must be filtered from gainers, losers, and most_active.
        let mut tracker = TopMoversTracker::new();
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: f32::NAN,
            day_close: 100.0,
            volume: 5000,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker.update(&tick);
        assert_eq!(tracker.tracked_count(), 1);

        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.gainers.is_empty(), "NaN excluded from gainers");
        assert!(snapshot.losers.is_empty(), "NaN excluded from losers");
        // NaN entries are also filtered from the entries vec before sorting,
        // so most_active won't contain them
        assert!(
            snapshot.most_active.is_empty(),
            "NaN excluded from most_active"
        );
    }

    // -----------------------------------------------------------------------
    // Additional coverage: SharedTopMoversSnapshot, Debug/Clone impls,
    // MoverEntry Debug/Serialize, snapshot after multiple compute cycles,
    // boundary near MIN_CHANGE_PCT_THRESHOLD
    // -----------------------------------------------------------------------

    #[test]
    fn shared_top_movers_snapshot_default_is_none() {
        let shared: SharedTopMoversSnapshot = std::sync::Arc::new(std::sync::RwLock::new(None));
        let guard = shared.read().unwrap();
        assert!(guard.is_none(), "shared snapshot should default to None");
    }

    #[test]
    fn shared_top_movers_snapshot_write_and_read() {
        let shared: SharedTopMoversSnapshot = std::sync::Arc::new(std::sync::RwLock::new(None));

        // Write a snapshot
        let snapshot = TopMoversSnapshot {
            gainers: vec![],
            losers: vec![],
            most_active: vec![],
            total_tracked: 42,
        };
        {
            let mut guard = shared.write().unwrap();
            *guard = Some(snapshot);
        }

        // Read it back
        let guard = shared.read().unwrap();
        assert!(guard.is_some());
        assert_eq!(guard.as_ref().unwrap().total_tracked, 42);
    }

    #[test]
    fn mover_entry_debug_impl() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.5,
            change_pct: 5.25,
            volume: 1000,
        };
        let debug = format!("{entry:?}");
        assert!(debug.contains("MoverEntry"));
        assert!(debug.contains("42"));
        assert!(debug.contains("100.5"));
    }

    #[test]
    fn top_movers_snapshot_debug_impl() {
        let snapshot = TopMoversSnapshot {
            gainers: vec![],
            losers: vec![],
            most_active: vec![],
            total_tracked: 0,
        };
        let debug = format!("{snapshot:?}");
        assert!(debug.contains("TopMoversSnapshot"));
        assert!(debug.contains("total_tracked"));
    }

    #[test]
    fn top_movers_snapshot_clone() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000));
        let snapshot = tracker.compute_snapshot();
        let cloned = snapshot.clone();
        assert_eq!(snapshot.total_tracked, cloned.total_tracked);
        assert_eq!(snapshot.gainers.len(), cloned.gainers.len());
        assert_eq!(snapshot.losers.len(), cloned.losers.len());
        assert_eq!(snapshot.most_active.len(), cloned.most_active.len());
    }

    #[test]
    fn change_pct_just_above_threshold_is_gainer() {
        let mut tracker = TopMoversTracker::new();
        // MIN_CHANGE_PCT_THRESHOLD is 0.01. Need change_pct > 0.01.
        // close=10000, ltp=10001.1 => pct = 0.011 > 0.01
        tracker.update(&make_tick(1, 2, 10001.1, 10000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert_eq!(
            snapshot.gainers.len(),
            1,
            "change just above threshold should be a gainer"
        );
    }

    #[test]
    fn change_pct_just_below_threshold_excluded_from_gainers() {
        let mut tracker = TopMoversTracker::new();
        // Need change_pct > 0 but <= 0.01 to be excluded.
        // close=100000, ltp=100009 => pct = 0.009 < 0.01
        tracker.update(&make_tick(1, 2, 100_009.0, 100_000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.gainers.is_empty(),
            "change just below threshold should be excluded from gainers"
        );
    }

    #[test]
    fn change_pct_just_below_neg_threshold_is_loser() {
        let mut tracker = TopMoversTracker::new();
        // Need change_pct < -0.01.
        // close=10000, ltp=9998.9 => pct = -0.011 < -0.01
        tracker.update(&make_tick(1, 2, 9998.9, 10000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert_eq!(
            snapshot.losers.len(),
            1,
            "change just below neg threshold should be a loser"
        );
    }

    #[test]
    fn change_pct_just_above_neg_threshold_excluded_from_losers() {
        let mut tracker = TopMoversTracker::new();
        // Need change_pct < 0 but >= -0.01 to be excluded.
        // close=100000, ltp=99991 => pct = -0.009 > -0.01
        tracker.update(&make_tick(1, 2, 99_991.0, 100_000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.losers.is_empty(),
            "change just above neg threshold should be excluded from losers"
        );
    }

    #[test]
    fn multiple_compute_snapshots_are_independent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 120.0, 100.0, 1000));

        let s1 = tracker.compute_snapshot();
        let s2 = tracker.compute_snapshot();

        // Both snapshots should have the same data
        assert_eq!(s1.gainers.len(), s2.gainers.len());
        assert_eq!(s1.total_tracked, s2.total_tracked);
    }

    #[test]
    fn latest_snapshot_returns_most_recent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 1000));
        tracker.compute_snapshot();

        // Add more data and compute again
        tracker.update(&make_tick(2, 2, 130.0, 100.0, 2000));
        tracker.compute_snapshot();

        let latest = tracker.latest_snapshot().unwrap();
        assert_eq!(
            latest.total_tracked, 2,
            "latest snapshot should reflect all tracked securities"
        );
    }

    #[test]
    fn volume_zero_still_tracked() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 110.0, 100.0, 0));
        assert_eq!(tracker.tracked_count(), 1);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.most_active.len(), 1);
        assert_eq!(snapshot.most_active[0].volume, 0);
    }

    #[test]
    fn mover_entry_serialization_includes_all_fields() {
        let entry = MoverEntry {
            security_id: 12345,
            exchange_segment_code: 1,
            last_traded_price: 2345.50,
            change_pct: -3.75,
            volume: 99999,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["security_id"], 12345);
        assert_eq!(json["exchange_segment_code"], 1);
        assert_eq!(json["volume"], 99999);
        // Float comparison via JSON
        let ltp = json["last_traded_price"].as_f64().unwrap();
        assert!((ltp - 2345.5).abs() < 0.01);
        let pct = json["change_pct"].as_f64().unwrap();
        assert!((pct - (-3.75)).abs() < 0.01);
    }

    #[test]
    fn snapshot_serialization_includes_all_top_level_fields() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 2, 120.0, 100.0, 5000));
        tracker.update(&make_tick(2, 2, 80.0, 100.0, 3000));
        let snapshot = tracker.compute_snapshot();
        let json = serde_json::to_value(&snapshot).unwrap();
        assert!(json.get("gainers").is_some());
        assert!(json.get("losers").is_some());
        assert!(json.get("most_active").is_some());
        assert_eq!(json["total_tracked"], 2);
    }

    #[test]
    fn inf_ltp_with_valid_close_produces_inf_change_pct_filtered() {
        let mut tracker = TopMoversTracker::new();
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: f32::INFINITY,
            day_close: 100.0,
            volume: 500,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker.update(&tick);
        assert_eq!(tracker.tracked_count(), 1);

        let snapshot = tracker.compute_snapshot();
        // inf change_pct: is_finite() returns false, so filtered from snapshot
        assert!(
            snapshot.gainers.is_empty(),
            "inf change_pct should be filtered"
        );
    }

    #[test]
    fn tracker_with_many_securities_caps_at_top_n_for_all_lists() {
        let mut tracker = TopMoversTracker::new();
        // 50 gainers, 50 losers
        for i in 1..=50u32 {
            let ltp = 100.0 + (i as f32) * 2.0; // gainers
            tracker.update(&make_tick(i, 2, ltp, 100.0, i * 100));
        }
        for i in 51..=100u32 {
            let ltp = 100.0 - ((i - 50) as f32) * 2.0; // losers
            tracker.update(&make_tick(i, 2, ltp, 100.0, i * 100));
        }
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.gainers.len() <= TOP_N);
        assert!(snapshot.losers.len() <= TOP_N);
        assert!(snapshot.most_active.len() <= TOP_N);
        assert_eq!(snapshot.total_tracked, 100);
    }

    // -----------------------------------------------------------------------
    // Additional coverage: ticks_processed saturating behavior
    // -----------------------------------------------------------------------

    #[test]
    fn ticks_processed_uses_saturating_add() {
        let mut tracker = TopMoversTracker::new();
        // Feed 100 ticks: 50 valid, 50 skipped (no close). All should count.
        for i in 0..50u32 {
            tracker.update(&make_tick(i, 2, 110.0, 100.0, 1000));
        }
        for i in 50..100u32 {
            tracker.update(&make_tick(i, 2, 110.0, 0.0, 1000)); // skipped
        }
        assert_eq!(tracker.ticks_processed(), 100);
        assert_eq!(tracker.tracked_count(), 50);
    }

    // -----------------------------------------------------------------------
    // Update: existing vs new key in HashMap (insert vs get_mut paths)
    // -----------------------------------------------------------------------

    #[test]
    fn update_exercises_insert_path_for_new_key() {
        let mut tracker = TopMoversTracker::new();
        assert_eq!(tracker.tracked_count(), 0);
        // First tick for this (security_id, segment) exercises the insert path
        tracker.update(&make_tick(999, 1, 200.0, 100.0, 5000));
        assert_eq!(tracker.tracked_count(), 1);
    }

    #[test]
    fn update_exercises_get_mut_path_for_existing_key() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(999, 1, 200.0, 100.0, 5000));
        // Second tick for same key exercises the get_mut path
        tracker.update(&make_tick(999, 1, 210.0, 100.0, 6000));
        assert_eq!(tracker.tracked_count(), 1);
        let snapshot = tracker.compute_snapshot();
        // Should have updated values from second tick
        assert!((snapshot.gainers[0].last_traded_price - 210.0).abs() < 0.01);
        assert_eq!(snapshot.gainers[0].volume, 6000);
    }

    // -----------------------------------------------------------------------
    // compute_snapshot: gainers filter (change_pct > threshold)
    // -----------------------------------------------------------------------

    #[test]
    fn compute_snapshot_exactly_at_threshold_excluded_from_gainers() {
        // Exactly at threshold should NOT pass the > check
        let mut tracker = TopMoversTracker::new();
        // change_pct = 0.01 exactly: close=10000, ltp=10001 => pct = 0.01
        tracker.update(&make_tick(1, 2, 10001.0, 10000.0, 100));
        let snapshot = tracker.compute_snapshot();
        // 0.01 is NOT > 0.01, so excluded
        assert!(
            snapshot.gainers.is_empty(),
            "change_pct exactly at threshold should be excluded"
        );
    }

    #[test]
    fn compute_snapshot_exactly_at_neg_threshold_excluded_from_losers() {
        let mut tracker = TopMoversTracker::new();
        // change_pct = -0.01 exactly: close=10000, ltp=9999 => pct = -0.01
        tracker.update(&make_tick(1, 2, 9999.0, 10000.0, 100));
        let snapshot = tracker.compute_snapshot();
        // -0.01 is NOT < -0.01, so excluded
        assert!(
            snapshot.losers.is_empty(),
            "change_pct exactly at neg threshold should be excluded"
        );
    }

    // -----------------------------------------------------------------------
    // latest_snapshot: starts None, populated after compute
    // -----------------------------------------------------------------------

    #[test]
    fn latest_snapshot_none_before_first_compute() {
        let tracker = TopMoversTracker::new();
        assert!(tracker.latest_snapshot().is_none());
    }

    #[test]
    fn latest_snapshot_some_after_compute_with_no_data() {
        let mut tracker = TopMoversTracker::new();
        tracker.compute_snapshot();
        assert!(tracker.latest_snapshot().is_some());
        assert_eq!(tracker.latest_snapshot().unwrap().total_tracked, 0);
    }

    // -----------------------------------------------------------------------
    // Default trait produces same state as new()
    // -----------------------------------------------------------------------

    #[test]
    fn default_tracker_no_snapshot_no_ticks() {
        let tracker = TopMoversTracker::default();
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.ticks_processed(), 0);
        assert!(tracker.latest_snapshot().is_none());
    }

    // -----------------------------------------------------------------------
    // update_prev_close — PrevClose packet integration
    // -----------------------------------------------------------------------

    #[test]
    fn prev_close_map_enables_tracking_when_day_close_is_zero() {
        let mut tracker = TopMoversTracker::new();
        // Set prev_close from PrevClose packet
        tracker.update_prev_close(100, 2, 1000.0);
        // Tick with day_close=0 (during market hours) — should now be tracked
        let tick = make_tick(100, 2, 1100.0, 0.0, 5000);
        tracker.update(&tick);
        assert_eq!(
            tracker.tracked_count(),
            1,
            "should track via prev_close map"
        );
        let snapshot = tracker.compute_snapshot();
        let pct = snapshot.gainers[0].change_pct;
        assert!(
            (pct - 10.0).abs() < 0.01,
            "expected ~10% from prev_close map, got {pct}"
        );
    }

    #[test]
    fn prev_close_map_takes_priority_over_day_close() {
        let mut tracker = TopMoversTracker::new();
        // Set prev_close from PrevClose packet
        tracker.update_prev_close(100, 2, 200.0);
        // Tick with day_close=100 (different from PrevClose packet)
        let tick = make_tick(100, 2, 220.0, 100.0, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        let pct = snapshot.gainers[0].change_pct;
        // Should use prev_close map (200.0), not day_close (100.0)
        // (220 - 200) / 200 * 100 = 10%
        assert!(
            (pct - 10.0).abs() < 0.01,
            "should use prev_close map (10%), not day_close (120%), got {pct}"
        );
    }

    #[test]
    fn prev_close_map_rejects_invalid_values() {
        let mut tracker = TopMoversTracker::new();
        tracker.update_prev_close(100, 2, 0.0);
        tracker.update_prev_close(101, 2, -50.0);
        tracker.update_prev_close(102, 2, f32::NAN);
        tracker.update_prev_close(103, 2, f32::INFINITY);
        // None should be stored
        let tick = make_tick(100, 2, 110.0, 0.0, 1000);
        tracker.update(&tick);
        assert_eq!(
            tracker.tracked_count(),
            0,
            "invalid prev_close should be rejected"
        );
    }

    #[test]
    fn prev_close_map_different_segments_independent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update_prev_close(100, 1, 500.0); // NSE_EQ
        tracker.update_prev_close(100, 2, 50.0); // NSE_FNO
        // Tick for NSE_FNO segment
        let tick = ParsedTick {
            security_id: 100,
            exchange_segment_code: 2,
            last_traded_price: 55.0,
            day_close: 0.0,
            volume: 1000,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        // (55 - 50) / 50 * 100 = 10%, NOT (55 - 500) / 500 * 100
        let pct = snapshot.gainers[0].change_pct;
        assert!(
            (pct - 10.0).abs() < 0.01,
            "should use segment-specific prev_close, got {pct}"
        );
    }
}
