//! Option movers tracker — 7-category ranking for F&O contracts.
//!
//! Tracks OI change, price change, volume, and value for option contracts.
//! Computes periodic snapshots ranked by 7 categories matching Dhan's
//! Options > Price Movers view.
//!
//! # Categories
//! 1. Highest OI — sorted by absolute OI descending
//! 2. OI Gainers — sorted by OI change % descending (positive only)
//! 3. OI Losers — sorted by OI change % ascending (negative only)
//! 4. Top Volume — sorted by volume descending
//! 5. Top Value — sorted by (LTP × volume × lot_size) descending
//! 6. Price Gainers — sorted by change % descending (positive only)
//! 7. Price Losers — sorted by change % ascending (negative only)
//!
//! # Performance
//! - O(1) per tick (HashMap lookup + update)
//! - O(N log N) per snapshot (periodic sort, cold path)

use std::collections::HashMap;

use dhan_live_trader_common::tick_types::ParsedTick;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Top N entries per category in each snapshot.
const TOP_N: usize = 20;

/// Initial HashMap capacity for option state tracking.
/// ~25,000 option contracts typical for NSE F&O.
const OPTION_MAP_INITIAL_CAPACITY: usize = 30_000;

/// Minimum OI to qualify for OI-based rankings.
/// Filters out illiquid contracts with near-zero OI.
const MIN_OI_THRESHOLD: u32 = 100;

/// Exchange segment code for NSE_FNO (derivatives).
const NSE_FNO_SEGMENT: u8 = 2;

// ---------------------------------------------------------------------------
// OptionMoverEntry — public snapshot entry
// ---------------------------------------------------------------------------

/// A single entry in the option movers snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OptionMoverEntry {
    /// Dhan security identifier (option contract).
    pub security_id: u32,
    /// Exchange segment code.
    pub exchange_segment_code: u8,
    /// Last traded price (option premium).
    pub ltp: f32,
    /// Previous close price.
    pub prev_close: f32,
    /// Price change (absolute).
    pub change: f32,
    /// Price change percentage.
    pub change_pct: f32,
    /// Current open interest.
    pub oi: u32,
    /// Previous day open interest (from PrevClose packet).
    pub prev_oi: u32,
    /// OI change (absolute).
    pub oi_change: i64,
    /// OI change percentage.
    pub oi_change_pct: f32,
    /// Day volume.
    pub volume: u32,
    /// Traded value estimate (LTP × volume).
    pub value: f64,
}

// ---------------------------------------------------------------------------
// OptionMoversSnapshot — periodic ranked output
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of option movers across 7 categories.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OptionMoversSnapshot {
    /// Top N by absolute OI descending.
    pub highest_oi: Vec<OptionMoverEntry>,
    /// Top N by OI change % descending (positive only).
    pub oi_gainers: Vec<OptionMoverEntry>,
    /// Top N by OI change % ascending (negative only).
    pub oi_losers: Vec<OptionMoverEntry>,
    /// Top N by volume descending.
    pub top_volume: Vec<OptionMoverEntry>,
    /// Top N by value descending.
    pub top_value: Vec<OptionMoverEntry>,
    /// Top N by price change % descending (positive only).
    pub price_gainers: Vec<OptionMoverEntry>,
    /// Top N by price change % ascending (negative only).
    pub price_losers: Vec<OptionMoverEntry>,
    /// Total option contracts being tracked.
    pub total_tracked: usize,
}

/// Thread-safe handle for sharing option movers snapshot with API handlers.
pub type SharedOptionMoversSnapshot =
    std::sync::Arc<std::sync::RwLock<Option<OptionMoversSnapshot>>>;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Per-option contract mutable state.
struct OptionState {
    ltp: f32,
    prev_close: f32,
    oi: u32,
    prev_oi: u32,
    volume: u32,
    exchange_segment_code: u8,
}

// ---------------------------------------------------------------------------
// OptionMoversTracker
// ---------------------------------------------------------------------------

/// Real-time tracker for option contract movers across 7 categories.
///
/// Updated O(1) per tick. Snapshot computation is O(N log N) but only runs
/// periodically on the cold path (every 60 seconds for persistence).
pub struct OptionMoversTracker {
    /// Per-option state keyed by `(security_id, segment_code)`.
    options: HashMap<(u32, u8), OptionState>,
    /// Total ticks processed.
    ticks_processed: u64,
}

impl Default for OptionMoversTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl OptionMoversTracker {
    /// Creates a new tracker with pre-allocated capacity.
    pub fn new() -> Self {
        Self {
            options: HashMap::with_capacity(OPTION_MAP_INITIAL_CAPACITY),
            ticks_processed: 0,
        }
    }

    /// Updates the tracker with a new tick.
    ///
    /// Only processes F&O ticks (segment code == NSE_FNO).
    /// Requires valid `day_close` (previous close > 0) for change calculation.
    ///
    /// # Performance
    /// O(1) — single HashMap lookup + arithmetic.
    #[inline]
    pub fn update(&mut self, tick: &ParsedTick) {
        // Only track F&O derivatives
        if tick.exchange_segment_code != NSE_FNO_SEGMENT {
            return;
        }

        self.ticks_processed = self.ticks_processed.saturating_add(1);

        let key = (tick.security_id, tick.exchange_segment_code);

        if let Some(state) = self.options.get_mut(&key) {
            state.ltp = tick.last_traded_price;
            state.oi = tick.open_interest;
            state.volume = tick.volume;
        } else {
            self.options.insert(
                key,
                OptionState {
                    ltp: tick.last_traded_price,
                    prev_close: tick.day_close,
                    oi: tick.open_interest,
                    prev_oi: 0, // Set from PrevClose packet
                    volume: tick.volume,
                    exchange_segment_code: tick.exchange_segment_code,
                },
            );
        }
    }

    /// Updates previous day OI from PrevClose packet.
    ///
    /// Called when a PrevClose packet arrives for an F&O instrument.
    /// This sets the baseline for OI change calculations.
    pub fn update_prev_oi(&mut self, security_id: u32, segment_code: u8, prev_oi: u32) {
        if segment_code != NSE_FNO_SEGMENT {
            return;
        }
        let key = (security_id, segment_code);
        if let Some(state) = self.options.get_mut(&key) {
            state.prev_oi = prev_oi;
        }
    }

    /// Computes a ranked snapshot across all 7 categories.
    ///
    /// # Performance
    /// O(N log N) — collects entries, sorts by each criterion.
    /// Cold path only (called every 60 seconds for persistence).
    pub fn compute_snapshot(&self) -> OptionMoversSnapshot {
        // Build entries from all tracked options
        // O(1) EXEMPT: begin — cold path snapshot computation (every 60s)
        let mut entries: Vec<OptionMoverEntry> = self
            .options
            .iter()
            .filter_map(|(&(_sid, _seg), state)| {
                // Require valid LTP and prev_close
                if !state.ltp.is_finite()
                    || state.ltp <= 0.0
                    || !state.prev_close.is_finite()
                    || state.prev_close <= 0.0
                {
                    return None;
                }

                let change = state.ltp - state.prev_close;
                let change_pct = (change / state.prev_close) * 100.0;
                let oi_change = i64::from(state.oi).saturating_sub(i64::from(state.prev_oi));
                let oi_change_pct = if state.prev_oi > 0 {
                    (oi_change as f32 / state.prev_oi as f32) * 100.0
                } else {
                    0.0
                };
                // DATA-INTEGRITY-EXEMPT: value is a derived calculation (LTP × volume), not raw Dhan price storage
                let value = f64::from(state.ltp) * f64::from(state.volume);

                if !change_pct.is_finite() || !oi_change_pct.is_finite() {
                    return None;
                }

                Some(OptionMoverEntry {
                    security_id: _sid,
                    exchange_segment_code: state.exchange_segment_code,
                    ltp: state.ltp,
                    prev_close: state.prev_close,
                    change,
                    change_pct,
                    oi: state.oi,
                    prev_oi: state.prev_oi,
                    oi_change,
                    oi_change_pct,
                    volume: state.volume,
                    value,
                })
            })
            .collect();

        let total_tracked = entries.len();

        // 1. Highest OI — sort by absolute OI descending
        let highest_oi = {
            let mut v: Vec<_> = entries
                .iter()
                .filter(|e| e.oi >= MIN_OI_THRESHOLD)
                .cloned()
                .collect();
            v.sort_unstable_by(|a, b| b.oi.cmp(&a.oi));
            v.truncate(TOP_N);
            v
        };

        // 2. OI Gainers — sort by OI change % descending (positive only)
        let oi_gainers = {
            let mut v: Vec<_> = entries
                .iter()
                .filter(|e| e.oi_change > 0 && e.prev_oi >= MIN_OI_THRESHOLD)
                .cloned()
                .collect();
            v.sort_unstable_by(|a, b| {
                b.oi_change_pct
                    .partial_cmp(&a.oi_change_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            v.truncate(TOP_N);
            v
        };

        // 3. OI Losers — sort by OI change % ascending (negative only)
        let oi_losers = {
            let mut v: Vec<_> = entries
                .iter()
                .filter(|e| e.oi_change < 0 && e.prev_oi >= MIN_OI_THRESHOLD)
                .cloned()
                .collect();
            v.sort_unstable_by(|a, b| {
                a.oi_change_pct
                    .partial_cmp(&b.oi_change_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            v.truncate(TOP_N);
            v
        };

        // 4. Top Volume — sort by volume descending
        let top_volume = {
            let mut v = entries.clone();
            v.sort_unstable_by(|a, b| b.volume.cmp(&a.volume));
            v.truncate(TOP_N);
            v
        };

        // 5. Top Value — sort by value descending
        let top_value = {
            let mut v = entries.clone();
            v.sort_unstable_by(|a, b| {
                b.value
                    .partial_cmp(&a.value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            v.truncate(TOP_N);
            v
        };

        // 6. Price Gainers — sort by change % descending (positive only)
        let price_gainers = {
            let mut v: Vec<_> = entries
                .iter()
                .filter(|e| e.change_pct > 0.0)
                .cloned()
                .collect();
            v.sort_unstable_by(|a, b| {
                b.change_pct
                    .partial_cmp(&a.change_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            v.truncate(TOP_N);
            v
        };

        // 7. Price Losers — sort by change % ascending (negative only)
        let price_losers = {
            let mut v: Vec<_> = entries
                .iter()
                .filter(|e| e.change_pct < 0.0)
                .cloned()
                .collect();
            v.sort_unstable_by(|a, b| {
                a.change_pct
                    .partial_cmp(&b.change_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            v.truncate(TOP_N);
            v
        };
        // O(1) EXEMPT: end

        // Clear the entries vec to free memory
        drop(entries);

        OptionMoversSnapshot {
            highest_oi,
            oi_gainers,
            oi_losers,
            top_volume,
            top_value,
            price_gainers,
            price_losers,
            total_tracked,
        }
    }

    /// Returns the total number of ticks processed.
    pub fn ticks_processed(&self) -> u64 {
        self.ticks_processed
    }

    /// Returns the number of tracked option contracts.
    pub fn tracked_count(&self) -> usize {
        self.options.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn make_option_tick(
        security_id: u32,
        ltp: f32,
        day_close: f32,
        oi: u32,
        volume: u32,
    ) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: NSE_FNO_SEGMENT,
            last_traded_price: ltp,
            day_close,
            open_interest: oi,
            volume,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn test_new_tracker_is_empty() {
        let tracker = OptionMoversTracker::new();
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.ticks_processed(), 0);
    }

    #[test]
    fn test_update_tracks_fno_tick() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 150.0, 140.0, 50000, 1000);
        tracker.update(&tick);
        assert_eq!(tracker.tracked_count(), 1);
        assert_eq!(tracker.ticks_processed(), 1);
    }

    #[test]
    fn test_update_ignores_non_fno() {
        let mut tracker = OptionMoversTracker::new();
        let mut tick = make_option_tick(49001, 150.0, 140.0, 50000, 1000);
        tick.exchange_segment_code = 1; // NSE_EQ — not F&O
        tracker.update(&tick);
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn test_snapshot_empty_tracker() {
        let tracker = OptionMoversTracker::new();
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.total_tracked, 0);
        assert!(snapshot.highest_oi.is_empty());
        assert!(snapshot.oi_gainers.is_empty());
        assert!(snapshot.oi_losers.is_empty());
        assert!(snapshot.top_volume.is_empty());
        assert!(snapshot.top_value.is_empty());
        assert!(snapshot.price_gainers.is_empty());
        assert!(snapshot.price_losers.is_empty());
    }

    #[test]
    fn test_price_change_calculation() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 150.0, 100.0, 50000, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.price_gainers.len(), 1);
        let entry = &snapshot.price_gainers[0];
        assert!((entry.change_pct - 50.0).abs() < 0.01); // (150-100)/100 * 100 = 50%
        assert!((entry.change - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_price_losers_negative_change() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 80.0, 100.0, 50000, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.price_losers.len(), 1);
        let entry = &snapshot.price_losers[0];
        assert!(entry.change_pct < 0.0);
        assert!((entry.change_pct - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn test_oi_change_with_prev_oi() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 150.0, 140.0, 60000, 1000);
        tracker.update(&tick);
        tracker.update_prev_oi(49001, NSE_FNO_SEGMENT, 50000);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.oi_gainers.len(), 1);
        let entry = &snapshot.oi_gainers[0];
        assert_eq!(entry.oi_change, 10000); // 60000 - 50000
        assert!((entry.oi_change_pct - 20.0).abs() < 0.01); // 10000/50000 * 100
    }

    #[test]
    fn test_oi_losers_negative_oi_change() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 150.0, 140.0, 40000, 1000);
        tracker.update(&tick);
        tracker.update_prev_oi(49001, NSE_FNO_SEGMENT, 50000);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.oi_losers.len(), 1);
        let entry = &snapshot.oi_losers[0];
        assert_eq!(entry.oi_change, -10000);
        assert!(entry.oi_change_pct < 0.0);
    }

    #[test]
    fn test_highest_oi_sorted_descending() {
        let mut tracker = OptionMoversTracker::new();
        for i in 0..5_u32 {
            let tick = make_option_tick(49000 + i, 100.0, 90.0, (i + 1) * 10000, 100);
            tracker.update(&tick);
        }
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.highest_oi.len(), 5);
        // Should be sorted by OI descending
        for i in 0..snapshot.highest_oi.len() - 1 {
            assert!(snapshot.highest_oi[i].oi >= snapshot.highest_oi[i + 1].oi);
        }
    }

    #[test]
    fn test_top_volume_sorted_descending() {
        let mut tracker = OptionMoversTracker::new();
        for i in 0..5_u32 {
            let tick = make_option_tick(49000 + i, 100.0, 90.0, 1000, (i + 1) * 500);
            tracker.update(&tick);
        }
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.top_volume.len(), 5);
        for i in 0..snapshot.top_volume.len() - 1 {
            assert!(snapshot.top_volume[i].volume >= snapshot.top_volume[i + 1].volume);
        }
    }

    #[test]
    fn test_top_value_sorted_descending() {
        let mut tracker = OptionMoversTracker::new();
        // Higher LTP × volume = higher value
        tracker.update(&make_option_tick(49001, 500.0, 400.0, 1000, 1000)); // value = 500k
        tracker.update(&make_option_tick(49002, 100.0, 90.0, 1000, 2000)); // value = 200k
        tracker.update(&make_option_tick(49003, 200.0, 180.0, 1000, 5000)); // value = 1M
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.top_value.len(), 3);
        assert_eq!(snapshot.top_value[0].security_id, 49003); // 1M value
    }

    #[test]
    fn test_snapshot_caps_at_top_n() {
        let mut tracker = OptionMoversTracker::new();
        for i in 0..30_u32 {
            let tick = make_option_tick(49000 + i, 100.0 + i as f32, 90.0, 1000 + i * 100, 100);
            tracker.update(&tick);
        }
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.price_gainers.len() <= TOP_N);
        assert!(snapshot.top_volume.len() <= TOP_N);
    }

    #[test]
    fn test_nan_ltp_filtered() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, f32::NAN, 100.0, 50000, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.total_tracked, 0);
    }

    #[test]
    fn test_zero_prev_close_filtered() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 100.0, 0.0, 50000, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.total_tracked, 0);
    }

    #[test]
    fn test_min_oi_threshold_filters_low_oi() {
        let mut tracker = OptionMoversTracker::new();
        // OI below threshold
        let tick = make_option_tick(49001, 100.0, 90.0, 50, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.highest_oi.is_empty()); // Filtered by MIN_OI_THRESHOLD
        assert!(!snapshot.price_gainers.is_empty()); // Price change doesn't need OI
    }

    #[test]
    fn test_update_prev_oi_ignores_non_fno() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 100.0, 90.0, 50000, 1000);
        tracker.update(&tick);
        tracker.update_prev_oi(49001, 1, 40000); // NSE_EQ — should be ignored
        let snapshot = tracker.compute_snapshot();
        // prev_oi should still be 0 (not updated from non-FNO call)
        if let Some(entry) = snapshot.highest_oi.first() {
            assert_eq!(entry.prev_oi, 0);
        }
    }

    #[test]
    fn test_multiple_ticks_same_option_updates_state() {
        let mut tracker = OptionMoversTracker::new();
        tracker.update(&make_option_tick(49001, 100.0, 90.0, 50000, 1000));
        tracker.update(&make_option_tick(49001, 110.0, 90.0, 55000, 1500));
        assert_eq!(tracker.tracked_count(), 1);
        assert_eq!(tracker.ticks_processed(), 2);
        let snapshot = tracker.compute_snapshot();
        let entry = &snapshot.price_gainers[0];
        assert!((entry.ltp - 110.0).abs() < 0.01);
        assert_eq!(entry.volume, 1500);
    }

    #[test]
    fn test_all_7_categories_populated() {
        let mut tracker = OptionMoversTracker::new();
        // Add enough options with diverse data
        for i in 0..25_u32 {
            let ltp = if i < 15 {
                100.0 + i as f32 * 5.0
            } else {
                50.0 - i as f32
            };
            let tick = make_option_tick(49000 + i, ltp, 100.0, 10000 + i * 1000, 500 + i * 100);
            tracker.update(&tick);
            tracker.update_prev_oi(49000 + i, NSE_FNO_SEGMENT, 12000 + i * 500);
        }
        let snapshot = tracker.compute_snapshot();
        assert!(!snapshot.highest_oi.is_empty());
        assert!(!snapshot.oi_gainers.is_empty());
        assert!(!snapshot.oi_losers.is_empty());
        assert!(!snapshot.top_volume.is_empty());
        assert!(!snapshot.top_value.is_empty());
        assert!(!snapshot.price_gainers.is_empty());
        assert!(!snapshot.price_losers.is_empty());
    }

    #[test]
    fn test_default_impl() {
        let tracker = OptionMoversTracker::default();
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn test_value_calculation() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 200.0, 180.0, 50000, 3000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        let entry = &snapshot.top_value[0];
        // value = LTP × volume = 200 × 3000 = 600000
        assert!((entry.value - 600_000.0).abs() < 1.0);
    }

    #[test]
    fn test_price_gainers_only_positive() {
        let mut tracker = OptionMoversTracker::new();
        tracker.update(&make_option_tick(49001, 80.0, 100.0, 50000, 1000)); // -20%
        tracker.update(&make_option_tick(49002, 120.0, 100.0, 50000, 1000)); // +20%
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.price_gainers.len(), 1);
        assert!(snapshot.price_gainers[0].change_pct > 0.0);
    }

    #[test]
    fn test_price_losers_only_negative() {
        let mut tracker = OptionMoversTracker::new();
        tracker.update(&make_option_tick(49001, 80.0, 100.0, 50000, 1000)); // -20%
        tracker.update(&make_option_tick(49002, 120.0, 100.0, 50000, 1000)); // +20%
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.price_losers.len(), 1);
        assert!(snapshot.price_losers[0].change_pct < 0.0);
    }

    #[test]
    fn test_infinity_ltp_filtered() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, f32::INFINITY, 100.0, 50000, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.total_tracked, 0);
    }

    #[test]
    fn test_negative_ltp_filtered() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, -10.0, 100.0, 50000, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.total_tracked, 0);
    }

    #[test]
    fn test_oi_gainers_requires_prev_oi_above_threshold() {
        let mut tracker = OptionMoversTracker::new();
        let tick = make_option_tick(49001, 100.0, 90.0, 60000, 1000);
        tracker.update(&tick);
        // Don't set prev_oi — it stays 0 (below threshold)
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.oi_gainers.is_empty()); // prev_oi = 0 < MIN_OI_THRESHOLD
    }

    #[test]
    fn test_snapshot_serializes_to_json() {
        let snapshot = OptionMoversSnapshot {
            highest_oi: vec![],
            oi_gainers: vec![],
            oi_losers: vec![],
            top_volume: vec![],
            top_value: vec![],
            price_gainers: vec![],
            price_losers: vec![],
            total_tracked: 0,
        };
        let json = serde_json::to_string(&snapshot);
        assert!(json.is_ok());
    }
}
