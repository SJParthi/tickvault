//! Rolling candle state machines — O(1) per-tick updates.
//!
//! Two state machines:
//! - `RollingCandleState` for time-based candles (IST boundary alignment)
//! - `TickCandleState` for tick-count-based candles (N ticks per candle)

use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS_I64;
use dhan_live_trader_common::tick_types::Candle;

// ---------------------------------------------------------------------------
// IST Boundary Computation — O(1) integer math
// ---------------------------------------------------------------------------

/// Computes the IST-aligned candle boundary for a given UTC epoch.
///
/// # Algorithm
/// 1. Shift UTC epoch to IST by adding 5h30m (19800 seconds)
/// 2. Floor-divide by interval to find the boundary in IST
/// 3. Shift back to UTC by subtracting 5h30m
///
/// # Performance
/// O(1) — two additions and one integer division.
pub fn compute_ist_aligned_boundary(epoch_utc: i64, interval_secs: i64) -> i64 {
    let ist_epoch = epoch_utc + IST_UTC_OFFSET_SECONDS_I64;
    let boundary_ist = (ist_epoch / interval_secs) * interval_secs;
    boundary_ist - IST_UTC_OFFSET_SECONDS_I64
}

// ---------------------------------------------------------------------------
// RollingCandleState — time-based candles
// ---------------------------------------------------------------------------

/// O(1) rolling candle state for a single (security_id, time_interval) pair.
///
/// On each tick:
/// - If the tick falls in the same interval → update OHLCV in O(1)
/// - If the tick crosses the boundary → finalize current candle, start new one
///
/// Volume is delta-based: `candle_volume = current_cumulative - start_cumulative`.
#[derive(Debug, Clone)]
pub struct RollingCandleState {
    /// Candle open boundary in UTC epoch seconds.
    interval_start_epoch: i64,
    /// Interval duration in seconds.
    interval_secs: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    /// Cumulative volume at interval start (for delta calculation).
    volume_at_start: u32,
    /// Latest cumulative volume from exchange.
    current_volume: u32,
    /// Open interest at latest tick.
    open_interest: u32,
    /// Number of ticks in this candle.
    tick_count: u32,
    /// Whether this state has received at least one tick.
    initialized: bool,
}

impl RollingCandleState {
    /// Creates a new uninitialized state for a given interval.
    pub fn new(interval_secs: i64) -> Self {
        Self {
            interval_start_epoch: 0,
            interval_secs,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume_at_start: 0,
            current_volume: 0,
            open_interest: 0,
            tick_count: 0,
            initialized: false,
        }
    }

    /// Processes a single tick. Returns `Some(candle)` if the interval rolled over.
    ///
    /// # Performance
    /// O(1) — one boundary check, one or two branch, basic arithmetic.
    pub fn update(
        &mut self,
        ltp: f32,
        volume: u32,
        oi: u32,
        tick_epoch_utc: i64,
        security_id: u32,
    ) -> Option<Candle> {
        let boundary = compute_ist_aligned_boundary(tick_epoch_utc, self.interval_secs);
        let price = f64::from(ltp);

        if !self.initialized {
            // First tick ever — initialize state.
            self.reset(price, volume, oi, boundary);
            return None;
        }

        if boundary != self.interval_start_epoch {
            // Interval rolled → finalize current candle, start new one.
            let finalized = self.finalize(security_id);
            self.reset(price, volume, oi, boundary);
            Some(finalized)
        } else {
            // Same interval → O(1) update.
            self.high = self.high.max(price);
            self.low = self.low.min(price);
            self.close = price;
            self.current_volume = volume;
            self.open_interest = oi;
            self.tick_count += 1;
            None
        }
    }

    /// Finalizes the current candle and returns it.
    fn finalize(&self, security_id: u32) -> Candle {
        Candle {
            timestamp: self.interval_start_epoch,
            security_id,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.current_volume.saturating_sub(self.volume_at_start),
            open_interest: self.open_interest,
            tick_count: self.tick_count,
        }
    }

    /// Resets state for a new interval.
    fn reset(&mut self, price: f64, volume: u32, oi: u32, boundary: i64) {
        self.interval_start_epoch = boundary;
        self.open = price;
        self.high = price;
        self.low = price;
        self.close = price;
        self.volume_at_start = volume;
        self.current_volume = volume;
        self.open_interest = oi;
        self.tick_count = 1;
        self.initialized = true;
    }

    /// Returns true if this state has been initialized with at least one tick.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Forces finalization of the current in-progress candle (e.g., at market close).
    ///
    /// Returns `None` if not initialized or tick_count is zero.
    pub fn force_finalize(&self, security_id: u32) -> Option<Candle> {
        if !self.initialized || self.tick_count == 0 {
            return None;
        }
        Some(self.finalize(security_id))
    }
}

// ---------------------------------------------------------------------------
// TickCandleState — tick-count-based candles
// ---------------------------------------------------------------------------

/// O(1) rolling candle state for tick-count-based intervals.
///
/// The candle finalizes after exactly `target_ticks` ticks have been received.
#[derive(Debug, Clone)]
pub struct TickCandleState {
    /// Number of ticks needed to finalize this candle.
    target_ticks: u32,
    /// Number of ticks received so far.
    tick_count: u32,
    /// Epoch of the first tick in this candle (UTC).
    first_tick_epoch: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume_at_start: u32,
    current_volume: u32,
    open_interest: u32,
    initialized: bool,
}

impl TickCandleState {
    /// Creates a new uninitialized tick candle state.
    pub fn new(target_ticks: u32) -> Self {
        Self {
            target_ticks,
            tick_count: 0,
            first_tick_epoch: 0,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume_at_start: 0,
            current_volume: 0,
            open_interest: 0,
            initialized: false,
        }
    }

    /// Processes a single tick. Returns `Some(candle)` if the tick count target is reached.
    ///
    /// # Performance
    /// O(1) — increment counter, compare, basic arithmetic.
    pub fn update(
        &mut self,
        ltp: f32,
        volume: u32,
        oi: u32,
        tick_epoch_utc: i64,
        security_id: u32,
    ) -> Option<Candle> {
        let price = f64::from(ltp);

        if !self.initialized {
            self.reset(price, volume, oi, tick_epoch_utc);
            // For T1 (target=1), finalize immediately after first tick.
            if self.tick_count >= self.target_ticks {
                let candle = self.finalize(security_id);
                self.initialized = false;
                self.tick_count = 0;
                return Some(candle);
            }
            return None;
        }

        // Update OHLCV
        self.high = self.high.max(price);
        self.low = self.low.min(price);
        self.close = price;
        self.current_volume = volume;
        self.open_interest = oi;
        self.tick_count += 1;

        if self.tick_count >= self.target_ticks {
            let candle = self.finalize(security_id);
            self.initialized = false;
            self.tick_count = 0;
            Some(candle)
        } else {
            None
        }
    }

    /// Finalizes and returns the current candle.
    fn finalize(&self, security_id: u32) -> Candle {
        Candle {
            timestamp: self.first_tick_epoch,
            security_id,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.current_volume.saturating_sub(self.volume_at_start),
            open_interest: self.open_interest,
            tick_count: self.tick_count,
        }
    }

    /// Resets state for a new candle.
    fn reset(&mut self, price: f64, volume: u32, oi: u32, epoch: i64) {
        self.first_tick_epoch = epoch;
        self.open = price;
        self.high = price;
        self.low = price;
        self.close = price;
        self.volume_at_start = volume;
        self.current_volume = volume;
        self.open_interest = oi;
        self.tick_count = 1;
        self.initialized = true;
    }

    /// Returns true if this state has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Forces finalization (e.g., at market close for incomplete candles).
    pub fn force_finalize(&self, security_id: u32) -> Option<Candle> {
        if !self.initialized || self.tick_count == 0 {
            return None;
        }
        Some(self.finalize(security_id))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- IST Boundary Tests ---

    #[test]
    fn test_ist_boundary_1m_at_market_open() {
        // 9:15:00 IST = 3:45:00 UTC
        // UTC epoch for 2026-02-26 03:45:00 UTC
        let utc_epoch = 1772073900; // approximate
        let boundary = compute_ist_aligned_boundary(utc_epoch, 60);
        // Boundary should be exactly at the minute mark
        let ist_boundary = boundary + IST_UTC_OFFSET_SECONDS_I64;
        assert_eq!(
            ist_boundary % 60,
            0,
            "1m boundary must be minute-aligned in IST"
        );
    }

    #[test]
    fn test_ist_boundary_5m_alignment() {
        // A tick at 9:17:30 IST should have boundary at 9:15:00 IST (5m aligned)
        // 9:17:30 IST = 3:47:30 UTC
        let utc_epoch = 1772074050; // approximate: 2026-02-26 03:47:30 UTC
        let boundary = compute_ist_aligned_boundary(utc_epoch, 300);
        let ist_boundary = boundary + IST_UTC_OFFSET_SECONDS_I64;
        assert_eq!(
            ist_boundary % 300,
            0,
            "5m boundary must be 5-min-aligned in IST"
        );
    }

    #[test]
    fn test_ist_boundary_1s_same_second() {
        let epoch = 1772073900;
        let b1 = compute_ist_aligned_boundary(epoch, 1);
        let b2 = compute_ist_aligned_boundary(epoch, 1);
        assert_eq!(b1, b2);
    }

    #[test]
    fn test_ist_boundary_1h_alignment() {
        // Any epoch should produce an hour-aligned IST boundary
        let epoch = 1772074050;
        let boundary = compute_ist_aligned_boundary(epoch, 3600);
        let ist_boundary = boundary + IST_UTC_OFFSET_SECONDS_I64;
        assert_eq!(
            ist_boundary % 3600,
            0,
            "1h boundary must be hour-aligned in IST"
        );
    }

    // --- RollingCandleState Tests ---

    #[test]
    fn test_rolling_candle_first_tick_initializes() {
        let mut state = RollingCandleState::new(60); // 1-minute
        assert!(!state.is_initialized());

        let result = state.update(24500.0, 1000, 50000, 1772073900, 13);
        assert!(result.is_none(), "first tick should not finalize");
        assert!(state.is_initialized());
    }

    #[test]
    fn test_rolling_candle_same_interval_updates_ohlc() {
        let mut state = RollingCandleState::new(60);
        // All ticks in the same minute
        let base_epoch = 1772073900;
        state.update(100.0, 1000, 50000, base_epoch, 13);
        state.update(105.0, 1100, 50000, base_epoch + 10, 13); // new high
        state.update(95.0, 1200, 50000, base_epoch + 20, 13); // new low
        state.update(102.0, 1300, 50000, base_epoch + 30, 13); // close

        // Force finalize to check values
        let candle = state.force_finalize(13).unwrap();
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.high, 105.0);
        assert_eq!(candle.low, 95.0);
        assert_eq!(candle.close, 102.0);
        assert_eq!(candle.tick_count, 4);
    }

    #[test]
    fn test_rolling_candle_volume_delta() {
        let mut state = RollingCandleState::new(60);
        let base_epoch = 1772073900;
        // First tick: cumulative volume = 10000
        state.update(100.0, 10000, 0, base_epoch, 13);
        // Last tick in same interval: cumulative volume = 12000
        state.update(101.0, 12000, 0, base_epoch + 30, 13);

        let candle = state.force_finalize(13).unwrap();
        assert_eq!(candle.volume, 2000, "volume should be delta: 12000 - 10000");
    }

    #[test]
    fn test_rolling_candle_interval_rollover_returns_finalized() {
        let mut state = RollingCandleState::new(60); // 1-minute
        let base_epoch = 1772073900;

        // First tick in interval 1
        state.update(100.0, 1000, 50000, base_epoch, 13);
        state.update(105.0, 1500, 50000, base_epoch + 30, 13);

        // Tick in NEXT interval (60 seconds later)
        let result = state.update(110.0, 2000, 51000, base_epoch + 60, 13);
        assert!(
            result.is_some(),
            "crossing boundary must return finalized candle"
        );

        let candle = result.unwrap();
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.high, 105.0);
        assert_eq!(candle.close, 105.0);
        assert_eq!(candle.tick_count, 2);
        assert_eq!(candle.volume, 500); // 1500 - 1000
    }

    #[test]
    fn test_rolling_candle_skip_interval_still_finalizes() {
        let mut state = RollingCandleState::new(60);
        let base_epoch = 1772073900;

        state.update(100.0, 1000, 50000, base_epoch, 13);

        // Skip 5 minutes ahead
        let result = state.update(200.0, 5000, 60000, base_epoch + 300, 13);
        assert!(result.is_some());
        let candle = result.unwrap();
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.close, 100.0);
        assert_eq!(candle.tick_count, 1);
    }

    #[test]
    fn test_rolling_candle_force_finalize_uninitialized_returns_none() {
        let state = RollingCandleState::new(60);
        assert!(state.force_finalize(13).is_none());
    }

    #[test]
    fn test_rolling_candle_security_id_in_finalized_candle() {
        let mut state = RollingCandleState::new(60);
        let base_epoch = 1772073900;

        state.update(100.0, 1000, 0, base_epoch, 42);
        let result = state.update(110.0, 2000, 0, base_epoch + 60, 42);
        let candle = result.unwrap();
        assert_eq!(candle.security_id, 42);
    }

    #[test]
    fn test_rolling_candle_oi_stored() {
        let mut state = RollingCandleState::new(60);
        let base_epoch = 1772073900;

        state.update(100.0, 1000, 50000, base_epoch, 13);
        state.update(101.0, 1100, 55000, base_epoch + 10, 13); // OI changes

        let candle = state.force_finalize(13).unwrap();
        assert_eq!(candle.open_interest, 55000, "OI should be latest value");
    }

    // --- TickCandleState Tests ---

    #[test]
    fn test_tick_candle_t1_finalizes_every_tick() {
        let mut state = TickCandleState::new(1);

        let result = state.update(100.0, 1000, 50000, 1772073900, 13);
        assert!(result.is_some(), "T1 should finalize on every tick");

        let candle = result.unwrap();
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.close, 100.0);
        assert_eq!(candle.tick_count, 1);
    }

    #[test]
    fn test_tick_candle_t10_finalizes_after_10() {
        let mut state = TickCandleState::new(10);
        let base_epoch = 1772073900;

        // First 9 ticks should not finalize
        for i in 0..9 {
            let result = state.update(
                100.0 + (i as f32),
                1000 + i * 10,
                50000,
                base_epoch + (i as i64),
                13,
            );
            assert!(result.is_none(), "tick {} should not finalize", i + 1);
        }

        // 10th tick finalizes
        let result = state.update(109.0, 1090, 50000, base_epoch + 9, 13);
        assert!(result.is_some(), "10th tick must finalize");

        let candle = result.unwrap();
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.high, 109.0);
        assert_eq!(candle.low, 100.0);
        assert_eq!(candle.close, 109.0);
        assert_eq!(candle.tick_count, 10);
    }

    #[test]
    fn test_tick_candle_custom_50_ticks() {
        let mut state = TickCandleState::new(50);
        let base_epoch = 1772073900;

        for i in 0..49 {
            let result = state.update(100.0, 1000, 50000, base_epoch + i, 13);
            assert!(result.is_none());
        }

        let result = state.update(100.0, 1490, 50000, base_epoch + 49, 13);
        assert!(result.is_some());
        assert_eq!(result.unwrap().tick_count, 50);
    }

    #[test]
    fn test_tick_candle_resets_after_finalization() {
        let mut state = TickCandleState::new(2);
        let base_epoch = 1772073900;

        // First candle: 2 ticks
        state.update(100.0, 1000, 50000, base_epoch, 13);
        let result = state.update(110.0, 1100, 50000, base_epoch + 1, 13);
        assert!(result.is_some());
        let c1 = result.unwrap();
        assert_eq!(c1.open, 100.0);
        assert_eq!(c1.close, 110.0);

        // Second candle: 2 more ticks
        state.update(200.0, 2000, 60000, base_epoch + 2, 13);
        let result = state.update(210.0, 2100, 60000, base_epoch + 3, 13);
        assert!(result.is_some());
        let c2 = result.unwrap();
        assert_eq!(c2.open, 200.0);
        assert_eq!(c2.close, 210.0);
    }

    #[test]
    fn test_tick_candle_force_finalize_partial() {
        let mut state = TickCandleState::new(10);
        state.update(100.0, 1000, 50000, 1772073900, 13);
        state.update(105.0, 1050, 50000, 1772073901, 13);
        state.update(95.0, 1100, 50000, 1772073902, 13);

        let candle = state.force_finalize(13).unwrap();
        assert_eq!(candle.tick_count, 3);
        assert_eq!(candle.high, 105.0);
        assert_eq!(candle.low, 95.0);
    }

    #[test]
    fn test_tick_candle_force_finalize_uninitialized_returns_none() {
        let state = TickCandleState::new(10);
        assert!(state.force_finalize(13).is_none());
    }

    #[test]
    fn test_tick_candle_volume_delta() {
        let mut state = TickCandleState::new(3);
        let base_epoch = 1772073900;

        state.update(100.0, 5000, 0, base_epoch, 13); // vol_start=5000
        state.update(101.0, 5100, 0, base_epoch + 1, 13);
        let result = state.update(102.0, 5300, 0, base_epoch + 2, 13);

        let candle = result.unwrap();
        assert_eq!(candle.volume, 300, "volume delta: 5300 - 5000");
    }
}
