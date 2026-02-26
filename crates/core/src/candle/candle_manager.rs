//! Candle manager — orchestrates all active candle states per security.
//!
//! Manages both time-based and tick-based candle states. When a tick arrives,
//! it's fed to all active interval states for that security. Finalized candles
//! are collected into an `ArrayVec` (stack-allocated, no heap allocation).

use std::collections::HashMap;

use arrayvec::ArrayVec;

use dhan_live_trader_common::tick_types::{Candle, ParsedTick, TickInterval, Timeframe};

use super::rolling_candle::{RollingCandleState, TickCandleState};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of time-based intervals active simultaneously.
/// 27 standard + a few custom = 32 is generous.
const MAX_TIME_CANDLES_PER_TICK: usize = 32;

/// Maximum number of tick-based intervals active simultaneously.
const MAX_TICK_CANDLES_PER_TICK: usize = 8;

/// Maximum total candles returned per tick (time + tick).
pub const MAX_CANDLES_PER_TICK: usize = 40;

// ---------------------------------------------------------------------------
// CandleManager
// ---------------------------------------------------------------------------

/// Manages all candle states for all securities and intervals.
///
/// # Usage
/// ```ignore
/// let mut manager = CandleManager::new(
///     &[Timeframe::S1, Timeframe::M1, Timeframe::M5],
///     &[TickInterval::T100],
/// );
///
/// let finalized = manager.process_tick(&tick);
/// for candle in &finalized {
///     // broadcast or persist
/// }
/// ```
pub struct CandleManager {
    /// Active time-based interval durations in seconds.
    active_time_intervals: Vec<i64>,

    /// Active tick-based interval counts.
    active_tick_intervals: Vec<u32>,

    /// Time-based states: (security_id, interval_secs) → RollingCandleState.
    time_states: HashMap<(u32, i64), RollingCandleState>,

    /// Tick-based states: (security_id, tick_count) → TickCandleState.
    tick_states: HashMap<(u32, u32), TickCandleState>,
}

impl CandleManager {
    /// Creates a new CandleManager with the specified active intervals.
    pub fn new(timeframes: &[Timeframe], tick_intervals: &[TickInterval]) -> Self {
        let active_time_intervals: Vec<i64> = timeframes.iter().map(|tf| tf.as_seconds()).collect();
        let active_tick_intervals: Vec<u32> =
            tick_intervals.iter().map(|ti| ti.tick_count()).collect();

        Self {
            active_time_intervals,
            active_tick_intervals,
            time_states: HashMap::new(),
            tick_states: HashMap::new(),
        }
    }

    /// Creates a CandleManager with a custom time interval (in seconds).
    ///
    /// Useful for user-defined intervals like "7m" (420s).
    pub fn add_custom_time_interval(&mut self, interval_secs: i64) {
        if !self.active_time_intervals.contains(&interval_secs) {
            self.active_time_intervals.push(interval_secs);
        }
    }

    /// Adds a custom tick-count interval.
    pub fn add_custom_tick_interval(&mut self, tick_count: u32) {
        if !self.active_tick_intervals.contains(&tick_count) {
            self.active_tick_intervals.push(tick_count);
        }
    }

    /// Processes a tick for all time-based intervals.
    ///
    /// Returns finalized candles for any intervals that rolled over.
    ///
    /// # Performance
    /// O(N) where N = number of active time intervals (typically 27).
    /// Each interval update is O(1). No heap allocation (ArrayVec).
    pub fn process_tick_time(
        &mut self,
        tick: &ParsedTick,
    ) -> ArrayVec<Candle, MAX_TIME_CANDLES_PER_TICK> {
        let mut finalized = ArrayVec::new();
        let tick_epoch = i64::from(tick.exchange_timestamp);

        for &interval_secs in &self.active_time_intervals.clone() {
            let key = (tick.security_id, interval_secs);

            let state = self
                .time_states
                .entry(key)
                .or_insert_with(|| RollingCandleState::new(interval_secs));

            if let Some(candle) = state.update(
                tick.last_traded_price,
                tick.volume,
                tick.open_interest,
                tick_epoch,
                tick.security_id,
            ) && finalized.len() < MAX_TIME_CANDLES_PER_TICK
            {
                finalized.push(candle);
            }
        }

        finalized
    }

    /// Processes a tick for all tick-count-based intervals.
    ///
    /// Returns finalized candles for any intervals that hit their tick target.
    pub fn process_tick_count(
        &mut self,
        tick: &ParsedTick,
    ) -> ArrayVec<Candle, MAX_TICK_CANDLES_PER_TICK> {
        let mut finalized = ArrayVec::new();
        let tick_epoch = i64::from(tick.exchange_timestamp);

        for &target_ticks in &self.active_tick_intervals.clone() {
            let key = (tick.security_id, target_ticks);

            let state = self
                .tick_states
                .entry(key)
                .or_insert_with(|| TickCandleState::new(target_ticks));

            if let Some(candle) = state.update(
                tick.last_traded_price,
                tick.volume,
                tick.open_interest,
                tick_epoch,
                tick.security_id,
            ) && finalized.len() < MAX_TICK_CANDLES_PER_TICK
            {
                finalized.push(candle);
            }
        }

        finalized
    }

    /// Processes a tick for both time-based and tick-based intervals.
    ///
    /// Returns all finalized candles in a single ArrayVec.
    pub fn process_tick(&mut self, tick: &ParsedTick) -> ArrayVec<Candle, MAX_CANDLES_PER_TICK> {
        let mut all = ArrayVec::new();

        let time_candles = self.process_tick_time(tick);
        for c in time_candles {
            if all.len() < MAX_CANDLES_PER_TICK {
                all.push(c);
            }
        }

        let tick_candles = self.process_tick_count(tick);
        for c in tick_candles {
            if all.len() < MAX_CANDLES_PER_TICK {
                all.push(c);
            }
        }

        all
    }

    /// Returns the number of active time-based intervals.
    pub fn time_interval_count(&self) -> usize {
        self.active_time_intervals.len()
    }

    /// Returns the number of active tick-based intervals.
    pub fn tick_interval_count(&self) -> usize {
        self.active_tick_intervals.len()
    }

    /// Returns the total number of tracked candle states (securities × intervals).
    pub fn total_state_count(&self) -> usize {
        self.time_states.len() + self.tick_states.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(security_id: u32, ltp: f32, volume: u32, epoch: u32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: 2,
            last_traded_price: ltp,
            exchange_timestamp: epoch,
            volume,
            open_interest: 50000,
            ..Default::default()
        }
    }

    // --- Construction ---

    #[test]
    fn test_candle_manager_new_counts() {
        let mgr = CandleManager::new(
            &[Timeframe::S1, Timeframe::M1, Timeframe::M5],
            &[TickInterval::T10, TickInterval::T100],
        );
        assert_eq!(mgr.time_interval_count(), 3);
        assert_eq!(mgr.tick_interval_count(), 2);
        assert_eq!(mgr.total_state_count(), 0); // no ticks processed yet
    }

    #[test]
    fn test_candle_manager_add_custom_intervals() {
        let mut mgr = CandleManager::new(&[Timeframe::M1], &[]);
        mgr.add_custom_time_interval(420); // 7m
        mgr.add_custom_tick_interval(50);
        assert_eq!(mgr.time_interval_count(), 2);
        assert_eq!(mgr.tick_interval_count(), 1);
    }

    #[test]
    fn test_candle_manager_add_duplicate_interval_ignored() {
        let mut mgr = CandleManager::new(&[Timeframe::M1], &[TickInterval::T10]);
        mgr.add_custom_time_interval(60); // M1 = 60s, already exists
        mgr.add_custom_tick_interval(10); // T10 = 10, already exists
        assert_eq!(mgr.time_interval_count(), 1);
        assert_eq!(mgr.tick_interval_count(), 1);
    }

    // --- Time-based candle processing ---

    #[test]
    fn test_process_tick_time_first_tick_no_finalization() {
        let mut mgr = CandleManager::new(&[Timeframe::M1], &[]);
        let tick = make_tick(13, 24500.0, 1000, 1772073900);
        let result = mgr.process_tick_time(&tick);
        assert!(
            result.is_empty(),
            "first tick should not finalize any candle"
        );
        assert_eq!(mgr.total_state_count(), 1);
    }

    #[test]
    fn test_process_tick_time_rollover_produces_candle() {
        let mut mgr = CandleManager::new(&[Timeframe::M1], &[]);
        let base = 1772073900_u32;

        // Tick in first minute
        mgr.process_tick_time(&make_tick(13, 100.0, 1000, base));
        mgr.process_tick_time(&make_tick(13, 105.0, 1100, base + 30));

        // Tick in second minute → first minute candle finalized
        let result = mgr.process_tick_time(&make_tick(13, 110.0, 1200, base + 60));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].open, 100.0);
        assert_eq!(result[0].high, 105.0);
        assert_eq!(result[0].close, 105.0);
        assert_eq!(result[0].security_id, 13);
    }

    #[test]
    fn test_process_tick_time_multi_timeframe() {
        // 1m and 5m active. After 5 minutes, both should roll.
        let mut mgr = CandleManager::new(&[Timeframe::M1, Timeframe::M5], &[]);
        let base = 1772073600_u32; // aligned to 5-min boundary

        // Tick at minute 0
        mgr.process_tick_time(&make_tick(13, 100.0, 1000, base));

        // Tick at minute 1 → only M1 rolls
        let result = mgr.process_tick_time(&make_tick(13, 105.0, 1100, base + 60));
        assert_eq!(result.len(), 1, "only M1 should roll at minute 1");

        // Ticks at minutes 2, 3, 4
        mgr.process_tick_time(&make_tick(13, 110.0, 1200, base + 120));
        mgr.process_tick_time(&make_tick(13, 115.0, 1300, base + 180));
        mgr.process_tick_time(&make_tick(13, 120.0, 1400, base + 240));

        // Tick at minute 5 → both M1 and M5 roll
        let result = mgr.process_tick_time(&make_tick(13, 125.0, 1500, base + 300));
        assert_eq!(result.len(), 2, "both M1 and M5 should roll at minute 5");
    }

    #[test]
    fn test_process_tick_time_multiple_securities() {
        let mut mgr = CandleManager::new(&[Timeframe::M1], &[]);
        let base = 1772073900_u32;

        // Two different securities in same minute
        mgr.process_tick_time(&make_tick(13, 100.0, 1000, base));
        mgr.process_tick_time(&make_tick(42, 200.0, 2000, base));

        assert_eq!(
            mgr.total_state_count(),
            2,
            "each security gets its own state"
        );

        // Both roll over at next minute
        let r1 = mgr.process_tick_time(&make_tick(13, 110.0, 1100, base + 60));
        let r2 = mgr.process_tick_time(&make_tick(42, 210.0, 2100, base + 60));
        assert_eq!(r1.len(), 1);
        assert_eq!(r2.len(), 1);
        assert_eq!(r1[0].security_id, 13);
        assert_eq!(r2[0].security_id, 42);
    }

    // --- Tick-based candle processing ---

    #[test]
    fn test_process_tick_count_t10() {
        let mut mgr = CandleManager::new(&[], &[TickInterval::T10]);
        let base = 1772073900_u32;

        for i in 0..9 {
            let result =
                mgr.process_tick_count(&make_tick(13, 100.0 + (i as f32), 1000 + i * 10, base + i));
            assert!(result.is_empty());
        }

        let result = mgr.process_tick_count(&make_tick(13, 109.0, 1090, base + 9));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tick_count, 10);
    }

    #[test]
    fn test_process_tick_count_t1_every_tick() {
        let mut mgr = CandleManager::new(&[], &[TickInterval::T1]);
        let base = 1772073900_u32;

        for i in 0..5_u32 {
            let result =
                mgr.process_tick_count(&make_tick(13, 100.0 + (i as f32), 1000 + i * 10, base + i));
            assert_eq!(result.len(), 1, "T1 should finalize every tick");
        }
    }

    // --- Combined processing ---

    #[test]
    fn test_process_tick_combined() {
        let mut mgr = CandleManager::new(&[Timeframe::M1], &[TickInterval::T1]);
        let tick = make_tick(13, 100.0, 1000, 1772073900);

        let result = mgr.process_tick(&tick);
        // T1 should produce 1 candle, M1 should not (first tick)
        assert_eq!(result.len(), 1, "only T1 candle from first tick");
    }

    #[test]
    fn test_process_tick_combined_both_fire() {
        let mut mgr = CandleManager::new(&[Timeframe::M1], &[TickInterval::T1]);
        let base = 1772073900_u32;

        // First tick
        mgr.process_tick(&make_tick(13, 100.0, 1000, base));

        // Second tick in new minute → M1 rolls + T1 fires
        let result = mgr.process_tick(&make_tick(13, 110.0, 1100, base + 60));
        assert_eq!(result.len(), 2, "M1 rollover + T1 every tick");
    }

    // --- Edge cases ---

    #[test]
    fn test_empty_manager_returns_empty() {
        let mut mgr = CandleManager::new(&[], &[]);
        let tick = make_tick(13, 100.0, 1000, 1772073900);
        let result = mgr.process_tick(&tick);
        assert!(result.is_empty());
    }

    #[test]
    fn test_all_standard_timeframes() {
        let mut mgr = CandleManager::new(Timeframe::all_standard(), &[]);
        assert_eq!(mgr.time_interval_count(), 27);

        let tick = make_tick(13, 100.0, 1000, 1772073900);
        let _ = mgr.process_tick_time(&tick); // no panic, creates 27 states
        assert_eq!(mgr.total_state_count(), 27);
    }

    #[test]
    fn test_custom_time_interval_works() {
        let mut mgr = CandleManager::new(&[], &[]);
        mgr.add_custom_time_interval(420); // 7m
        let base = 1772073600_u32;

        mgr.process_tick_time(&make_tick(13, 100.0, 1000, base));
        // 7 minutes later
        let result = mgr.process_tick_time(&make_tick(13, 110.0, 1100, base + 420));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].open, 100.0);
    }
}
