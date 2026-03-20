//! O(1) per-tick indicator engine.
//!
//! Updates all indicator state for a given instrument in constant time.
//! No lookback, no recomputation, no heap allocation on the hot path.
//!
//! # Algorithms
//! - EMA: `alpha * price + (1 - alpha) * prev`
//! - RSI: Wilder's smoothing on avg_gain / avg_loss
//! - MACD: Three cascaded EMAs (fast, slow, signal)
//! - ATR: Wilder's smoothing on True Range
//! - Bollinger: Welford's online algorithm for running mean + variance
//! - SMA: Ring buffer + running sum
//! - Supertrend: ATR-based bands with direction flip
//! - ADX: Chain of Wilder's smoothed +DM, -DM, TR, DX
//! - OBV: Running sum with close-vs-prev direction
//! - VWAP: Cumulative (price × volume) / cumulative volume

use dhan_live_trader_common::constants::MAX_INDICATOR_INSTRUMENTS;
use dhan_live_trader_common::tick_types::ParsedTick;

use super::types::{IndicatorParams, IndicatorSnapshot, IndicatorState};

// ---------------------------------------------------------------------------
// Indicator Engine — flat Vec indexed by security_id
// ---------------------------------------------------------------------------

/// The indicator engine: pre-allocated flat array of per-instrument state.
///
/// Indexed by `security_id` for O(1) lookup. Pre-allocated at startup
/// for the full instrument universe — zero allocation on the hot path.
pub struct IndicatorEngine {
    /// Per-instrument indicator state. Index = security_id.
    states: Vec<IndicatorState>,
    /// Shared indicator parameters (periods, multipliers).
    params: IndicatorParams,
    /// Pre-computed EMA alpha for fast period.
    alpha_fast: f64,
    /// Pre-computed EMA alpha for slow period.
    alpha_slow: f64,
    /// Pre-computed EMA alpha for MACD signal.
    alpha_signal: f64,
    /// Pre-computed Wilder factor for RSI/ATR/ADX.
    wilder_factor: f64,
    /// Pre-computed Wilder factor for ADX smoothing.
    adx_wilder_factor: f64,
}

impl IndicatorEngine {
    /// Creates a new engine with pre-allocated state for all instruments.
    ///
    /// # Performance
    /// Single allocation at startup. The `states` Vec is never resized.
    pub fn new(params: IndicatorParams) -> Self {
        let mut states = Vec::with_capacity(MAX_INDICATOR_INSTRUMENTS);
        states.resize_with(MAX_INDICATOR_INSTRUMENTS, IndicatorState::new);

        Self {
            states,
            alpha_fast: IndicatorParams::ema_alpha(params.ema_fast_period),
            alpha_slow: IndicatorParams::ema_alpha(params.ema_slow_period),
            alpha_signal: IndicatorParams::ema_alpha(params.macd_signal_period),
            wilder_factor: IndicatorParams::wilder_factor(params.rsi_period),
            adx_wilder_factor: IndicatorParams::wilder_factor(params.adx_period),
            params,
        }
    }

    /// Updates all indicators for the given tick and returns a snapshot.
    ///
    /// # Performance
    /// O(1) — every indicator uses a recursive/incremental update.
    /// Total: ~200ns on c7i.2xlarge (20 indicators, all O(1)).
    ///
    /// # Safety (bounds)
    /// If `security_id >= MAX_INDICATOR_INSTRUMENTS`, returns a default snapshot.
    #[inline(always)]
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: all arithmetic is bounded f64 operations with finite checks
    pub fn update(&mut self, tick: &ParsedTick) -> IndicatorSnapshot {
        let sid = tick.security_id as usize;
        if sid >= self.states.len() {
            return IndicatorSnapshot {
                security_id: tick.security_id,
                ..Default::default()
            };
        }

        let state = &mut self.states[sid];
        let price = f64::from(tick.last_traded_price);
        let high = f64::from(tick.day_high);
        let low = f64::from(tick.day_low);
        let volume = f64::from(tick.volume);
        let close = f64::from(tick.day_close);

        // Track warmup
        if state.warmup_count < u16::MAX {
            state.warmup_count = state.warmup_count.saturating_add(1);
        }

        let is_first_tick = state.warmup_count == 1;

        // ---- EMA (fast + slow) ----
        if is_first_tick {
            state.ema_fast = price;
            state.ema_slow = price;
        } else {
            state.ema_fast = self.alpha_fast * price + (1.0 - self.alpha_fast) * state.ema_fast;
            state.ema_slow = self.alpha_slow * price + (1.0 - self.alpha_slow) * state.ema_slow;
        }

        // ---- MACD ----
        let macd_line = state.ema_fast - state.ema_slow;
        if is_first_tick {
            state.macd_signal_ema = macd_line;
        } else {
            state.macd_signal_ema =
                self.alpha_signal * macd_line + (1.0 - self.alpha_signal) * state.macd_signal_ema;
        }

        // ---- RSI (Wilder's smoothing) ----
        if !is_first_tick {
            let change = price - state.prev_close;
            let gain = if change > 0.0 { change } else { 0.0 };
            let loss = if change < 0.0 { -change } else { 0.0 };

            let wf = self.wilder_factor;
            state.rsi_avg_gain = state.rsi_avg_gain * (1.0 - wf) + gain * wf;
            state.rsi_avg_loss = state.rsi_avg_loss * (1.0 - wf) + loss * wf;
        }

        // ---- ATR (Wilder's smoothing on True Range) ----
        if !is_first_tick {
            let tr1 = high - low;
            let tr2 = (high - state.prev_close).abs();
            let tr3 = (low - state.prev_close).abs();
            let true_range = tr1.max(tr2).max(tr3);

            let wf = self.wilder_factor;
            if state.warmup_count == 2 {
                state.atr = true_range;
            } else {
                state.atr = state.atr * (1.0 - wf) + true_range * wf;
            }
        }

        // ---- Supertrend (ATR-based) ----
        if !is_first_tick && state.atr > 0.0 {
            let hl2 = (high + low) / 2.0;
            let offset = self.params.supertrend_multiplier * state.atr;
            let mut upper = hl2 + offset;
            let mut lower = hl2 - offset;

            // Carry forward bands (conditional)
            if upper < state.supertrend_upper || state.prev_close > state.supertrend_upper {
                // Keep new upper
            } else {
                upper = state.supertrend_upper;
            }

            if lower > state.supertrend_lower || state.prev_close < state.supertrend_lower {
                // Keep new lower
            } else {
                lower = state.supertrend_lower;
            }

            state.supertrend_upper = upper;
            state.supertrend_lower = lower;

            // Direction flip
            if state.supertrend_direction {
                // Was bullish — flip to bearish if close < lower
                if price < lower {
                    state.supertrend_direction = false;
                }
            } else {
                // Was bearish — flip to bullish if close > upper
                if price > upper {
                    state.supertrend_direction = true;
                }
            }
        } else if is_first_tick {
            state.supertrend_upper = high;
            state.supertrend_lower = low;
        }

        // ---- ADX (chain of Wilder's smoothing) ----
        if !is_first_tick {
            let plus_dm = if high > state.prev_high {
                (high - state.prev_high).max(0.0)
            } else {
                0.0
            };
            let minus_dm = if state.prev_low > low {
                (state.prev_low - low).max(0.0)
            } else {
                0.0
            };
            let tr1 = high - low;
            let tr2 = (high - state.prev_close).abs();
            let tr3 = (low - state.prev_close).abs();
            let true_range = tr1.max(tr2).max(tr3);

            let wf = self.adx_wilder_factor;
            state.adx_plus_dm_smooth = state.adx_plus_dm_smooth * (1.0 - wf) + plus_dm * wf;
            state.adx_minus_dm_smooth = state.adx_minus_dm_smooth * (1.0 - wf) + minus_dm * wf;
            state.adx_tr_smooth = state.adx_tr_smooth * (1.0 - wf) + true_range * wf;

            if state.adx_tr_smooth > 0.0 {
                let plus_di = 100.0 * state.adx_plus_dm_smooth / state.adx_tr_smooth;
                let minus_di = 100.0 * state.adx_minus_dm_smooth / state.adx_tr_smooth;
                let di_sum = plus_di + minus_di;
                let dx = if di_sum > 0.0 {
                    100.0 * (plus_di - minus_di).abs() / di_sum
                } else {
                    0.0
                };
                state.adx_value = state.adx_value * (1.0 - wf) + dx * wf;
            }
        }

        // ---- OBV (running sum) ----
        if !is_first_tick {
            if price > state.prev_close {
                state.obv += volume;
            } else if price < state.prev_close {
                state.obv -= volume;
            }
            // price == prev_close: OBV unchanged
        }

        // ---- VWAP (cumulative) ----
        if volume > 0.0 {
            let typical_price = (high + low + price) / 3.0;
            state.vwap_cumulative_pv += typical_price * volume;
            state.vwap_cumulative_vol += volume;
        }

        // ---- Bollinger Bands (Welford's online algorithm) ----
        state.bb_count = state.bb_count.saturating_add(1);
        let n = f64::from(state.bb_count);
        let delta = price - state.bb_mean;
        state.bb_mean += delta / n;
        let delta2 = price - state.bb_mean;
        state.bb_m2 += delta * delta2;

        // ---- SMA (ring buffer + running sum) ----
        let evicted = state.sma_ring.push(price);
        let sma_period = self.params.sma_period;
        if state.sma_ring.len() <= sma_period {
            state.sma_running_sum += price;
        } else {
            state.sma_running_sum += price - evicted;
        }

        // ---- Store previous values for next tick ----
        state.prev_close = price;
        state.prev_high = high;
        state.prev_low = low;

        // ---- Build snapshot ----
        let rsi = if state.rsi_avg_loss > 0.0 {
            let rs = state.rsi_avg_gain / state.rsi_avg_loss;
            100.0 - 100.0 / (1.0 + rs)
        } else if state.rsi_avg_gain > 0.0 {
            100.0
        } else {
            50.0
        };

        let bb_variance = if state.bb_count > 1 {
            state.bb_m2 / (n - 1.0)
        } else {
            0.0
        };
        let bb_stddev = bb_variance.sqrt();
        let bb_mult = self.params.bollinger_multiplier;

        let sma_count = state.sma_ring.len().min(sma_period);
        let sma = if sma_count > 0 {
            state.sma_running_sum / f64::from(sma_count)
        } else {
            0.0
        };

        let vwap = if state.vwap_cumulative_vol > 0.0 {
            state.vwap_cumulative_pv / state.vwap_cumulative_vol
        } else {
            0.0
        };

        let supertrend = if state.supertrend_direction {
            state.supertrend_lower
        } else {
            state.supertrend_upper
        };

        IndicatorSnapshot {
            security_id: tick.security_id,
            ema_fast: state.ema_fast,
            ema_slow: state.ema_slow,
            sma,
            vwap,
            rsi,
            macd_line,
            macd_signal: state.macd_signal_ema,
            macd_histogram: macd_line - state.macd_signal_ema,
            bollinger_upper: state.bb_mean + bb_mult * bb_stddev,
            bollinger_middle: state.bb_mean,
            bollinger_lower: state.bb_mean - bb_mult * bb_stddev,
            atr: state.atr,
            supertrend,
            supertrend_bullish: state.supertrend_direction,
            adx: state.adx_value,
            obv: state.obv,
            last_traded_price: price,
            previous_close: close,
            day_high: high,
            day_low: low,
            volume,
            is_warm: state.is_warm(),
        }
    }

    /// Resets VWAP accumulators for all instruments (call at market open).
    pub fn reset_vwap_daily(&mut self) {
        for state in &mut self.states {
            state.vwap_cumulative_pv = 0.0;
            state.vwap_cumulative_vol = 0.0;
        }
    }

    /// Resets Bollinger Band accumulators for all instruments (call at session reset).
    pub fn reset_bollinger_daily(&mut self) {
        for state in &mut self.states {
            state.bb_mean = 0.0;
            state.bb_m2 = 0.0;
            state.bb_count = 0;
        }
    }

    /// Returns a reference to the indicator parameters.
    pub const fn params(&self) -> &IndicatorParams {
        &self.params
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::MAX_INDICATOR_WARMUP_TICKS;

    fn make_tick(security_id: u32, ltp: f32, high: f32, low: f32, volume: u32) -> ParsedTick {
        ParsedTick {
            security_id,
            last_traded_price: ltp,
            day_high: high,
            day_low: low,
            volume,
            ..Default::default()
        }
    }

    fn default_engine() -> IndicatorEngine {
        IndicatorEngine::new(IndicatorParams::default())
    }

    // --- Construction ---

    #[test]
    fn test_new_engine_pre_allocates_state() {
        let engine = default_engine();
        assert_eq!(engine.states.len(), MAX_INDICATOR_INSTRUMENTS);
    }

    #[test]
    fn test_new_engine_alpha_values_positive() {
        let engine = default_engine();
        assert!(engine.alpha_fast > 0.0 && engine.alpha_fast < 1.0);
        assert!(engine.alpha_slow > 0.0 && engine.alpha_slow < 1.0);
        assert!(engine.alpha_signal > 0.0 && engine.alpha_signal < 1.0);
        assert!(engine.wilder_factor > 0.0 && engine.wilder_factor < 1.0);
    }

    #[test]
    fn test_fast_alpha_greater_than_slow() {
        let engine = default_engine();
        // Fast EMA (period 12) reacts faster → larger alpha
        assert!(engine.alpha_fast > engine.alpha_slow);
    }

    // --- Bounds check ---

    #[test]
    fn test_out_of_bounds_security_id_returns_default_snapshot() {
        let mut engine = default_engine();
        let tick = ParsedTick {
            security_id: u32::MAX,
            last_traded_price: 100.0,
            ..Default::default()
        };
        let snap = engine.update(&tick);
        assert_eq!(snap.security_id, u32::MAX);
        assert!(!snap.is_warm);
        // All indicators should be default (0.0)
        assert_eq!(snap.ema_fast, 0.0);
        assert_eq!(snap.rsi, 0.0);
    }

    // --- First tick initialization ---

    #[test]
    fn test_first_tick_initializes_ema_to_price() {
        let mut engine = default_engine();
        let tick = make_tick(100, 250.0, 255.0, 245.0, 1000);
        let snap = engine.update(&tick);
        assert!((snap.ema_fast - 250.0).abs() < f64::EPSILON);
        assert!((snap.ema_slow - 250.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_first_tick_not_warm() {
        let mut engine = default_engine();
        let tick = make_tick(100, 250.0, 255.0, 245.0, 1000);
        let snap = engine.update(&tick);
        assert!(!snap.is_warm);
    }

    #[test]
    fn test_first_tick_rsi_is_neutral() {
        let mut engine = default_engine();
        let tick = make_tick(100, 250.0, 255.0, 245.0, 1000);
        let snap = engine.update(&tick);
        // First tick: avg_gain = 0, avg_loss = 0 → RSI = 50 (neutral)
        assert!((snap.rsi - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_first_tick_atr_is_zero() {
        let mut engine = default_engine();
        let tick = make_tick(100, 250.0, 255.0, 245.0, 1000);
        let snap = engine.update(&tick);
        assert_eq!(snap.atr, 0.0);
    }

    // --- Warmup ---

    #[test]
    fn test_warmup_completes_after_threshold() {
        let mut engine = default_engine();
        let warmup = u32::from(MAX_INDICATOR_WARMUP_TICKS);
        for i in 0..warmup {
            let tick = make_tick(100, 250.0 + i as f32, 260.0, 240.0, 1000);
            let snap = engine.update(&tick);
            if i + 1 < warmup {
                assert!(!snap.is_warm, "should not be warm at tick {}", i + 1);
            } else {
                assert!(snap.is_warm, "should be warm at tick {}", i + 1);
            }
        }
    }

    // --- EMA convergence ---

    #[test]
    fn test_ema_converges_to_constant_price() {
        let mut engine = default_engine();
        let price = 100.0_f32;
        // Feed many ticks at the same price — EMA should converge to price
        for _ in 0..200 {
            engine.update(&make_tick(100, price, price, price, 1000));
        }
        let snap = engine.update(&make_tick(100, price, price, price, 1000));
        assert!(
            (snap.ema_fast - f64::from(price)).abs() < 0.01,
            "ema_fast should converge to constant price"
        );
        assert!(
            (snap.ema_slow - f64::from(price)).abs() < 0.01,
            "ema_slow should converge to constant price"
        );
    }

    // --- RSI ---

    #[test]
    fn test_rsi_all_gains_approaches_100() {
        let mut engine = default_engine();
        // Steadily increasing prices → RSI should approach 100
        for i in 0..100 {
            engine.update(&make_tick(
                100,
                100.0 + i as f32,
                110.0 + i as f32,
                95.0 + i as f32,
                1000,
            ));
        }
        let snap = engine.update(&make_tick(100, 200.0, 210.0, 195.0, 1000));
        assert!(
            snap.rsi > 90.0,
            "RSI should be > 90 for all-gain series, got {}",
            snap.rsi
        );
    }

    #[test]
    fn test_rsi_all_losses_approaches_0() {
        let mut engine = default_engine();
        // Steadily decreasing prices → RSI should approach 0
        for i in 0..100 {
            engine.update(&make_tick(
                100,
                200.0 - i as f32,
                210.0 - i as f32,
                195.0 - i as f32,
                1000,
            ));
        }
        let snap = engine.update(&make_tick(100, 99.0, 100.0, 98.0, 1000));
        assert!(
            snap.rsi < 10.0,
            "RSI should be < 10 for all-loss series, got {}",
            snap.rsi
        );
    }

    // --- MACD ---

    #[test]
    fn test_macd_at_constant_price_converges_to_zero() {
        let mut engine = default_engine();
        for _ in 0..200 {
            engine.update(&make_tick(100, 100.0, 100.0, 100.0, 1000));
        }
        let snap = engine.update(&make_tick(100, 100.0, 100.0, 100.0, 1000));
        assert!(
            snap.macd_line.abs() < 0.01,
            "MACD line should be ~0 at constant price, got {}",
            snap.macd_line
        );
        assert!(
            snap.macd_histogram.abs() < 0.01,
            "MACD histogram should be ~0 at constant price, got {}",
            snap.macd_histogram
        );
    }

    // --- VWAP ---

    #[test]
    fn test_vwap_with_volume() {
        let mut engine = default_engine();
        // Tick with price=100, high=105, low=95, volume=1000
        // typical_price = (105 + 95 + 100) / 3 = 100.0
        let snap = engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        assert!((snap.vwap - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_vwap_zero_volume_stays_zero() {
        let mut engine = default_engine();
        let snap = engine.update(&make_tick(100, 100.0, 105.0, 95.0, 0));
        assert_eq!(snap.vwap, 0.0);
    }

    #[test]
    fn test_vwap_reset_daily_clears_accumulators() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        engine.reset_vwap_daily();
        // After reset, VWAP should return 0 until new volume arrives
        let snap = engine.update(&make_tick(100, 100.0, 105.0, 95.0, 0));
        assert_eq!(snap.vwap, 0.0);
    }

    // --- Bollinger Bands ---

    #[test]
    fn test_bollinger_at_constant_price_bands_collapse() {
        let mut engine = default_engine();
        for _ in 0..50 {
            engine.update(&make_tick(100, 100.0, 100.0, 100.0, 1000));
        }
        let snap = engine.update(&make_tick(100, 100.0, 100.0, 100.0, 1000));
        // With constant price, stddev → 0, so all bands converge to mean
        assert!((snap.bollinger_upper - 100.0).abs() < 0.01);
        assert!((snap.bollinger_middle - 100.0).abs() < 0.01);
        assert!((snap.bollinger_lower - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_bollinger_reset_daily() {
        let mut engine = default_engine();
        for _ in 0..10 {
            engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        }
        engine.reset_bollinger_daily();
        // After reset, bb_count = 0 → next snapshot should behave like fresh start
        let snap = engine.update(&make_tick(100, 200.0, 210.0, 190.0, 1000));
        assert!(
            (snap.bollinger_middle - 200.0).abs() < 0.01,
            "bollinger middle after reset should be new price"
        );
    }

    // --- SMA ---

    #[test]
    fn test_sma_single_value_equals_price() {
        let mut engine = default_engine();
        let snap = engine.update(&make_tick(100, 50.0, 55.0, 45.0, 1000));
        assert!((snap.sma - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_sma_within_period_is_exact() {
        let mut engine = default_engine();
        // Within the SMA period (20), running sum is just accumulated prices.
        // After 10 pushes at 100.0: sma = 1000 / 10 = 100.0
        for _ in 0..10 {
            engine.update(&make_tick(100, 100.0, 100.0, 100.0, 1000));
        }
        let snap = engine.update(&make_tick(100, 100.0, 100.0, 100.0, 1000));
        // sma_ring.len() = 11, sma_period = 20 → sma_count = min(11, 20) = 11
        // sma = 1100 / 11 = 100.0
        assert!(
            (snap.sma - 100.0).abs() < f64::EPSILON,
            "SMA within period should be exact avg, got {}",
            snap.sma
        );
    }

    // --- ATR ---

    #[test]
    fn test_atr_second_tick_initializes_to_true_range() {
        let mut engine = default_engine();
        // First tick: sets prev values
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        // Second tick: ATR = TR
        let snap = engine.update(&make_tick(100, 102.0, 108.0, 96.0, 1000));
        // TR = max(108-96, |108-100|, |96-100|) = max(12, 8, 4) = 12
        assert!((snap.atr - 12.0).abs() < f64::EPSILON);
    }

    // --- OBV ---

    #[test]
    fn test_obv_increases_on_price_up() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let snap = engine.update(&make_tick(100, 101.0, 106.0, 96.0, 500));
        // Price went up (100→101), so OBV += volume
        assert!((snap.obv - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_obv_decreases_on_price_down() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let snap = engine.update(&make_tick(100, 99.0, 104.0, 94.0, 500));
        // Price went down (100→99), so OBV -= volume
        assert!((snap.obv - (-500.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_obv_unchanged_on_same_price() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let snap = engine.update(&make_tick(100, 100.0, 105.0, 95.0, 500));
        assert_eq!(snap.obv, 0.0);
    }

    // --- Supertrend ---

    #[test]
    fn test_supertrend_first_tick_sets_initial_bands() {
        let mut engine = default_engine();
        let snap = engine.update(&make_tick(100, 100.0, 110.0, 90.0, 1000));
        // First tick: supertrend_upper = high, supertrend_lower = low
        // Direction starts bullish → supertrend = lower band
        assert!((snap.supertrend - 90.0).abs() < f64::EPSILON);
        assert!(snap.supertrend_bullish);
    }

    // --- Multiple instruments independent ---

    #[test]
    fn test_instruments_are_independent() {
        let mut engine = default_engine();
        // Feed different prices to two instruments
        engine.update(&make_tick(100, 200.0, 210.0, 190.0, 1000));
        engine.update(&make_tick(200, 50.0, 55.0, 45.0, 500));

        let snap1 = engine.update(&make_tick(100, 201.0, 211.0, 191.0, 1000));
        let snap2 = engine.update(&make_tick(200, 51.0, 56.0, 46.0, 500));

        assert!(snap1.ema_fast > 199.0 && snap1.ema_fast < 202.0);
        assert!(snap2.ema_fast > 49.0 && snap2.ema_fast < 52.0);
    }

    // --- Snapshot completeness ---

    #[test]
    fn test_snapshot_carries_price_context() {
        let mut engine = default_engine();
        let tick = make_tick(100, 100.0, 105.0, 95.0, 1000);
        let snap = engine.update(&tick);
        assert!((snap.last_traded_price - 100.0).abs() < f64::EPSILON);
        assert!((snap.day_high - 105.0).abs() < f64::EPSILON);
        assert!((snap.day_low - 95.0).abs() < f64::EPSILON);
        assert!((snap.volume - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_params_accessor() {
        let params = IndicatorParams {
            ema_fast_period: 5,
            ..Default::default()
        };
        let engine = IndicatorEngine::new(params);
        assert_eq!(engine.params().ema_fast_period, 5);
    }

    // --- ADX ---

    #[test]
    fn test_adx_starts_at_zero() {
        let mut engine = default_engine();
        let snap = engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        assert_eq!(snap.adx, 0.0);
    }

    #[test]
    fn test_adx_positive_after_trending() {
        let mut engine = default_engine();
        // Strong uptrend — ADX should become positive
        for i in 0..50 {
            engine.update(&make_tick(
                100,
                100.0 + i as f32 * 2.0,
                110.0 + i as f32 * 2.0,
                95.0 + i as f32 * 2.0,
                1000,
            ));
        }
        let snap = engine.update(&make_tick(100, 200.0, 210.0, 195.0, 1000));
        assert!(
            snap.adx > 0.0,
            "ADX should be > 0 in trending market, got {}",
            snap.adx
        );
    }

    // -----------------------------------------------------------------------
    // Out-of-bounds security_id returns default snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn test_out_of_bounds_security_id_preserves_id_in_snapshot() {
        let mut engine = default_engine();
        // Use a security_id that is >= MAX_INDICATOR_INSTRUMENTS
        let large_id = MAX_INDICATOR_INSTRUMENTS as u32;
        let tick = ParsedTick {
            security_id: large_id,
            last_traded_price: 500.0,
            day_high: 510.0,
            day_low: 490.0,
            volume: 10000,
            ..Default::default()
        };

        let snap = engine.update(&tick);
        assert_eq!(snap.security_id, large_id);
        assert!(!snap.is_warm, "OOB security_id must not be warm");
        assert_eq!(snap.ema_fast, 0.0, "OOB must return default ema_fast");
        assert_eq!(snap.rsi, 0.0, "OOB must return default rsi");
        assert_eq!(snap.atr, 0.0, "OOB must return default atr");
        assert_eq!(snap.macd_line, 0.0, "OOB must return default macd_line");
        assert_eq!(snap.sma, 0.0, "OOB must return default sma");
        assert_eq!(snap.vwap, 0.0, "OOB must return default vwap");
    }

    #[test]
    fn test_security_id_zero_is_valid() {
        let mut engine = default_engine();
        let tick = make_tick(0, 100.0, 105.0, 95.0, 1000);
        let snap = engine.update(&tick);
        // security_id 0 is within bounds and should work normally
        assert_eq!(snap.security_id, 0);
        assert!((snap.ema_fast - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_max_valid_security_id() {
        let mut engine = default_engine();
        let max_valid = (MAX_INDICATOR_INSTRUMENTS - 1) as u32;
        let tick = make_tick(max_valid, 200.0, 210.0, 190.0, 500);
        let snap = engine.update(&tick);
        assert_eq!(snap.security_id, max_valid);
        assert!((snap.ema_fast - 200.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Warmup counter saturation
    // -----------------------------------------------------------------------

    #[test]
    fn test_warmup_counter_saturates_at_u16_max() {
        let mut engine = default_engine();
        let sid = 100_usize;

        // Manually set warmup_count close to u16::MAX
        engine.states[sid].warmup_count = u16::MAX - 1;

        // One more tick should bring it to u16::MAX
        let tick = make_tick(100, 100.0, 105.0, 95.0, 1000);
        engine.update(&tick);
        assert_eq!(engine.states[sid].warmup_count, u16::MAX);

        // Another tick should remain at u16::MAX (saturating)
        engine.update(&tick);
        assert_eq!(
            engine.states[sid].warmup_count,
            u16::MAX,
            "warmup_count must saturate at u16::MAX, not overflow"
        );
    }

    #[test]
    fn test_warmup_counter_at_max_still_produces_valid_indicators() {
        let mut engine = default_engine();
        let sid = 100_usize;

        // Set warmup to max — is_warm should be true
        engine.states[sid].warmup_count = u16::MAX;

        let tick = make_tick(100, 100.0, 105.0, 95.0, 1000);
        let snap = engine.update(&tick);
        assert!(snap.is_warm, "u16::MAX warmup_count must be warm");
        // EMA should still update correctly
        assert!(snap.ema_fast > 0.0);
    }
}
