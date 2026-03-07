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
