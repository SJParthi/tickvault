//! Indicator types: per-instrument state, snapshots, and configuration.
//!
//! All types are `Copy` for zero-allocation on the hot path.
//! Every indicator updates in O(1) per tick — no lookback recomputation.

use dhan_live_trader_common::constants::{
    INDICATOR_RING_BUFFER_CAPACITY, MAX_INDICATOR_WARMUP_TICKS,
};

// ---------------------------------------------------------------------------
// Ring Buffer — compile-time-sized, zero-allocation
// ---------------------------------------------------------------------------

/// Fixed-size ring buffer for windowed indicators (SMA, Stochastic, MFI).
///
/// Power-of-two capacity enables bitwise masking instead of modulo.
/// Embedded directly in `IndicatorState` — no heap allocation.
///
/// # Performance
/// - O(1) push (one write, one mask op)
/// - O(1) oldest retrieval (one read, one mask op)
#[derive(Clone, Copy)]
pub struct RingBuffer {
    /// Pre-allocated data array.
    data: [f64; INDICATOR_RING_BUFFER_CAPACITY],
    /// Next write position (wraps via bitmask).
    head: u16,
    /// Number of elements filled (capped at capacity).
    count: u16,
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl RingBuffer {
    /// Bitmask for O(1) modulo: `capacity - 1`.
    const MASK: usize = INDICATOR_RING_BUFFER_CAPACITY - 1;

    /// Creates a new zeroed ring buffer.
    pub const fn new() -> Self {
        Self {
            data: [0.0; INDICATOR_RING_BUFFER_CAPACITY],
            head: 0,
            count: 0,
        }
    }

    /// Pushes a value, returning the evicted (oldest) value.
    ///
    /// # Performance
    /// O(1) — one array write, one bitmask increment.
    #[inline(always)]
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: bitmask guarantees bounds; count capped at capacity
    pub fn push(&mut self, value: f64) -> f64 {
        let idx = self.head as usize & Self::MASK;
        let old = self.data[idx];
        self.data[idx] = value;
        self.head = ((self.head as usize).wrapping_add(1) & Self::MASK) as u16;
        if (self.count as usize) < INDICATOR_RING_BUFFER_CAPACITY {
            self.count = self.count.wrapping_add(1);
        }
        old
    }

    /// Returns the number of elements currently stored.
    #[inline(always)]
    pub const fn len(&self) -> u16 {
        self.count
    }

    /// Returns true if no elements have been pushed.
    #[inline(always)]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the oldest value in the buffer.
    ///
    /// # Safety (logical)
    /// Only valid when `count == INDICATOR_RING_BUFFER_CAPACITY` (buffer is full).
    #[inline(always)]
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: bitmask guarantees bounds
    pub fn oldest(&self) -> f64 {
        let idx = self.head as usize & Self::MASK;
        self.data[idx]
    }
}

// ---------------------------------------------------------------------------
// Indicator Snapshot — output of indicator engine per tick
// ---------------------------------------------------------------------------

/// Snapshot of all indicator values for a single instrument at a single tick.
///
/// This is the read-only output passed to the strategy evaluator.
/// `Copy` for zero-allocation fanout.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndicatorSnapshot {
    /// Security identifier (for routing to correct strategy).
    pub security_id: u32,

    // --- Moving Averages ---
    /// Exponential Moving Average (fast period, typically 12).
    pub ema_fast: f64,
    /// Exponential Moving Average (slow period, typically 26).
    pub ema_slow: f64,
    /// Simple Moving Average (configurable period).
    pub sma: f64,
    /// Volume-Weighted Average Price (resets daily).
    pub vwap: f64,

    // --- Momentum / Oscillators ---
    /// Relative Strength Index (0-100).
    pub rsi: f64,
    /// MACD line (EMA_fast - EMA_slow).
    pub macd_line: f64,
    /// MACD signal line (EMA of MACD line).
    pub macd_signal: f64,
    /// MACD histogram (MACD line - signal).
    pub macd_histogram: f64,

    // --- Volatility / Trend ---
    /// Bollinger Band upper (mean + k * stddev).
    pub bollinger_upper: f64,
    /// Bollinger Band middle (running mean).
    pub bollinger_middle: f64,
    /// Bollinger Band lower (mean - k * stddev).
    pub bollinger_lower: f64,
    /// Average True Range.
    pub atr: f64,
    /// Supertrend value.
    pub supertrend: f64,
    /// Supertrend direction: true = bullish (price above), false = bearish.
    pub supertrend_bullish: bool,
    /// Average Directional Index (0-100).
    pub adx: f64,

    // --- Volume ---
    /// On-Balance Volume.
    pub obv: f64,

    // --- Price context ---
    /// Last traded price (from tick).
    pub last_traded_price: f64,
    /// Previous close price (from tick).
    pub previous_close: f64,
    /// Day high.
    pub day_high: f64,
    /// Day low.
    pub day_low: f64,
    /// Cumulative volume.
    pub volume: f64,

    /// Whether the indicator state has completed warmup.
    pub is_warm: bool,
}

// ---------------------------------------------------------------------------
// Indicator State — mutable per-instrument state for O(1) updates
// ---------------------------------------------------------------------------

/// Per-instrument indicator state. Updated incrementally on each tick.
///
/// All fields are stack-allocated. The struct is `Copy` to allow
/// flat `Vec<IndicatorState>` storage indexed by security_id.
///
/// # O(1) Guarantee
/// Every indicator uses a recursive/incremental algorithm:
/// - EMA, RSI, ATR, ADX, Supertrend: Wilder's smoothing
/// - SMA: Ring buffer + running sum
/// - MACD: Cascaded EMAs
/// - Bollinger Bands: Welford's online algorithm
/// - VWAP: Two running sums (cumulative PV / cumulative Vol)
/// - OBV: Running sum with direction
#[derive(Clone, Copy)]
pub struct IndicatorState {
    // --- EMA state ---
    /// EMA fast (e.g., 12-period).
    pub ema_fast: f64,
    /// EMA slow (e.g., 26-period).
    pub ema_slow: f64,

    // --- MACD state (cascaded EMAs) ---
    /// MACD signal line EMA (e.g., 9-period EMA of MACD line).
    pub macd_signal_ema: f64,

    // --- RSI state (Wilder's smoothing) ---
    /// Wilder's average gain.
    pub rsi_avg_gain: f64,
    /// Wilder's average loss.
    pub rsi_avg_loss: f64,

    // --- ATR state (Wilder's smoothing) ---
    /// Average True Range value.
    pub atr: f64,

    // --- Supertrend state ---
    /// Upper band.
    pub supertrend_upper: f64,
    /// Lower band.
    pub supertrend_lower: f64,
    /// Direction: true = bullish.
    pub supertrend_direction: bool,

    // --- ADX state (chain of Wilder's smoothing) ---
    /// Smoothed +DM.
    pub adx_plus_dm_smooth: f64,
    /// Smoothed -DM.
    pub adx_minus_dm_smooth: f64,
    /// Smoothed True Range for ADX.
    pub adx_tr_smooth: f64,
    /// ADX value (smoothed DX).
    pub adx_value: f64,

    // --- OBV state ---
    /// On-Balance Volume running sum.
    pub obv: f64,

    // --- VWAP state (daily reset) ---
    /// Cumulative (price × volume).
    pub vwap_cumulative_pv: f64,
    /// Cumulative volume.
    pub vwap_cumulative_vol: f64,

    // --- Bollinger Bands state (Welford's online algorithm) ---
    /// Running mean.
    pub bb_mean: f64,
    /// Running M2 (sum of squared deviations).
    pub bb_m2: f64,
    /// Count for Welford's.
    pub bb_count: u32,

    // --- SMA state (ring buffer + running sum) ---
    /// Ring buffer for SMA window.
    pub sma_ring: RingBuffer,
    /// Running sum of values in the SMA ring buffer.
    pub sma_running_sum: f64,

    // --- Previous tick values ---
    /// Previous close price (for RSI gain/loss, OBV direction, ATR true range).
    pub prev_close: f64,
    /// Previous high (for ATR true range).
    pub prev_high: f64,
    /// Previous low (for ATR true range).
    pub prev_low: f64,

    // --- Warmup tracking ---
    /// Number of ticks processed so far (capped at MAX_INDICATOR_WARMUP_TICKS).
    pub warmup_count: u16,
}

impl Default for IndicatorState {
    fn default() -> Self {
        Self::new()
    }
}

impl IndicatorState {
    /// Creates a new zeroed indicator state.
    pub const fn new() -> Self {
        Self {
            ema_fast: 0.0,
            ema_slow: 0.0,
            macd_signal_ema: 0.0,
            rsi_avg_gain: 0.0,
            rsi_avg_loss: 0.0,
            atr: 0.0,
            supertrend_upper: 0.0,
            supertrend_lower: 0.0,
            supertrend_direction: true,
            adx_plus_dm_smooth: 0.0,
            adx_minus_dm_smooth: 0.0,
            adx_tr_smooth: 0.0,
            adx_value: 0.0,
            obv: 0.0,
            vwap_cumulative_pv: 0.0,
            vwap_cumulative_vol: 0.0,
            bb_mean: 0.0,
            bb_m2: 0.0,
            bb_count: 0,
            sma_ring: RingBuffer::new(),
            sma_running_sum: 0.0,
            prev_close: 0.0,
            prev_high: 0.0,
            prev_low: 0.0,
            warmup_count: 0,
        }
    }

    /// Returns true if this instrument has received enough ticks to produce
    /// reliable indicator values.
    #[inline(always)]
    pub const fn is_warm(&self) -> bool {
        self.warmup_count >= MAX_INDICATOR_WARMUP_TICKS
    }
}

// ---------------------------------------------------------------------------
// Indicator Parameters — configurable per strategy/instrument
// ---------------------------------------------------------------------------

/// Configuration parameters for the indicator engine.
///
/// Loaded from TOML at startup. All periods are expressed as tick counts.
#[derive(Debug, Clone, Copy)]
pub struct IndicatorParams {
    /// EMA fast period (default: 12).
    pub ema_fast_period: u16,
    /// EMA slow period (default: 26).
    pub ema_slow_period: u16,
    /// MACD signal period (default: 9).
    pub macd_signal_period: u16,
    /// RSI period (default: 14).
    pub rsi_period: u16,
    /// SMA period (must be <= INDICATOR_RING_BUFFER_CAPACITY).
    pub sma_period: u16,
    /// ATR period (default: 14).
    pub atr_period: u16,
    /// ADX period (default: 14).
    pub adx_period: u16,
    /// Supertrend multiplier (default: 3.0).
    pub supertrend_multiplier: f64,
    /// Bollinger Band standard deviation multiplier (default: 2.0).
    pub bollinger_multiplier: f64,
}

impl Default for IndicatorParams {
    fn default() -> Self {
        Self {
            ema_fast_period: 12,
            ema_slow_period: 26,
            macd_signal_period: 9,
            rsi_period: 14,
            sma_period: 20,
            atr_period: 14,
            adx_period: 14,
            supertrend_multiplier: 3.0,
            bollinger_multiplier: 2.0,
        }
    }
}

impl IndicatorParams {
    /// Computes EMA smoothing factor: `2 / (period + 1)`.
    #[inline(always)]
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: period is >= 1 by construction, division is safe
    pub fn ema_alpha(period: u16) -> f64 {
        2.0 / (f64::from(period) + 1.0)
    }

    /// Computes Wilder's smoothing factor: `1 / period`.
    #[inline(always)]
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: period is >= 1 by construction
    pub fn wilder_factor(period: u16) -> f64 {
        1.0 / f64::from(period)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- RingBuffer ---

    #[test]
    fn test_ring_buffer_new_is_empty() {
        let rb = RingBuffer::new();
        assert!(rb.is_empty());
        assert_eq!(rb.len(), 0);
    }

    #[test]
    fn test_ring_buffer_push_increments_count() {
        let mut rb = RingBuffer::new();
        rb.push(1.0);
        assert_eq!(rb.len(), 1);
        assert!(!rb.is_empty());
    }

    #[test]
    fn test_ring_buffer_push_returns_evicted_zero_when_not_full() {
        let mut rb = RingBuffer::new();
        let evicted = rb.push(42.0);
        assert_eq!(evicted, 0.0);
    }

    #[test]
    fn test_ring_buffer_wraps_at_capacity() {
        let mut rb = RingBuffer::new();
        // Fill to capacity
        for i in 0..INDICATOR_RING_BUFFER_CAPACITY {
            rb.push(i as f64);
        }
        assert_eq!(rb.len() as usize, INDICATOR_RING_BUFFER_CAPACITY);

        // Push one more — should evict the oldest (0.0)
        let evicted = rb.push(999.0);
        assert_eq!(evicted, 0.0);
        assert_eq!(rb.len() as usize, INDICATOR_RING_BUFFER_CAPACITY);
    }

    #[test]
    fn test_ring_buffer_oldest_when_full() {
        let mut rb = RingBuffer::new();
        for i in 0..INDICATOR_RING_BUFFER_CAPACITY {
            rb.push(i as f64 + 1.0);
        }
        // Oldest should be the first pushed value (1.0)
        assert_eq!(rb.oldest(), 1.0);
    }

    #[test]
    fn test_ring_buffer_eviction_order() {
        let mut rb = RingBuffer::new();
        // Fill entirely
        for i in 0..INDICATOR_RING_BUFFER_CAPACITY {
            rb.push(i as f64);
        }
        // Evictions should come out in FIFO order
        for i in 0..INDICATOR_RING_BUFFER_CAPACITY {
            let evicted = rb.push(1000.0 + i as f64);
            assert_eq!(evicted, i as f64, "eviction {} mismatch", i);
        }
    }

    #[test]
    fn test_ring_buffer_default_same_as_new() {
        let d = RingBuffer::default();
        let n = RingBuffer::new();
        assert_eq!(d.len(), n.len());
        assert_eq!(d.is_empty(), n.is_empty());
    }

    // --- IndicatorState ---

    #[test]
    fn test_indicator_state_new_all_zeros() {
        let state = IndicatorState::new();
        assert_eq!(state.ema_fast, 0.0);
        assert_eq!(state.ema_slow, 0.0);
        assert_eq!(state.rsi_avg_gain, 0.0);
        assert_eq!(state.rsi_avg_loss, 0.0);
        assert_eq!(state.atr, 0.0);
        assert_eq!(state.obv, 0.0);
        assert_eq!(state.warmup_count, 0);
        assert!(state.supertrend_direction); // starts bullish
    }

    #[test]
    fn test_indicator_state_not_warm_initially() {
        let state = IndicatorState::new();
        assert!(!state.is_warm());
    }

    #[test]
    fn test_indicator_state_warm_at_threshold() {
        let mut state = IndicatorState::new();
        state.warmup_count = MAX_INDICATOR_WARMUP_TICKS;
        assert!(state.is_warm());
    }

    #[test]
    fn test_indicator_state_not_warm_below_threshold() {
        let mut state = IndicatorState::new();
        state.warmup_count = MAX_INDICATOR_WARMUP_TICKS - 1;
        assert!(!state.is_warm());
    }

    #[test]
    fn test_indicator_state_default_same_as_new() {
        let d = IndicatorState::default();
        let n = IndicatorState::new();
        assert_eq!(d.warmup_count, n.warmup_count);
        assert_eq!(d.ema_fast, n.ema_fast);
    }

    // --- IndicatorParams ---

    #[test]
    fn test_indicator_params_default_values() {
        let params = IndicatorParams::default();
        assert_eq!(params.ema_fast_period, 12);
        assert_eq!(params.ema_slow_period, 26);
        assert_eq!(params.macd_signal_period, 9);
        assert_eq!(params.rsi_period, 14);
        assert_eq!(params.sma_period, 20);
        assert_eq!(params.atr_period, 14);
        assert_eq!(params.adx_period, 14);
        assert!((params.supertrend_multiplier - 3.0).abs() < f64::EPSILON);
        assert!((params.bollinger_multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ema_alpha_period_1() {
        // alpha = 2 / (1 + 1) = 1.0
        let alpha = IndicatorParams::ema_alpha(1);
        assert!((alpha - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ema_alpha_period_12() {
        // alpha = 2 / (12 + 1) ≈ 0.153846
        let alpha = IndicatorParams::ema_alpha(12);
        assert!((alpha - 2.0 / 13.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_wilder_factor_period_14() {
        // factor = 1 / 14 ≈ 0.071428
        let factor = IndicatorParams::wilder_factor(14);
        assert!((factor - 1.0 / 14.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_wilder_factor_period_1() {
        let factor = IndicatorParams::wilder_factor(1);
        assert!((factor - 1.0).abs() < f64::EPSILON);
    }

    // --- IndicatorSnapshot ---

    #[test]
    fn test_indicator_snapshot_default_all_zeros() {
        let snap = IndicatorSnapshot::default();
        assert_eq!(snap.security_id, 0);
        assert_eq!(snap.ema_fast, 0.0);
        assert_eq!(snap.rsi, 0.0);
        assert_eq!(snap.vwap, 0.0);
        assert!(!snap.is_warm);
        assert!(!snap.supertrend_bullish);
    }

    #[test]
    fn test_indicator_snapshot_is_copy() {
        let snap = IndicatorSnapshot {
            security_id: 42,
            ema_fast: 100.0,
            ..Default::default()
        };
        let copy = snap;
        assert_eq!(copy.security_id, snap.security_id);
        assert_eq!(copy.ema_fast, snap.ema_fast);
    }

    #[test]
    fn test_indicator_state_is_copy() {
        let state = IndicatorState::new();
        let copy = state;
        assert_eq!(copy.warmup_count, state.warmup_count);
    }
}
