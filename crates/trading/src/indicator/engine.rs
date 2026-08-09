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

use std::collections::HashMap;

use tickvault_common::constants::MAX_INDICATOR_INSTRUMENTS;
use tickvault_common::tick_types::ParsedTick;

use super::types::{IndicatorParams, IndicatorSnapshot, IndicatorState};

// ---------------------------------------------------------------------------
// Indicator Engine — flat Vec indexed by security_id
// ---------------------------------------------------------------------------

/// The indicator engine: pre-allocated flat array of per-instrument state.
///
/// Indexed by `security_id` for O(1) lookup. Pre-allocated at startup
/// for the full instrument universe — zero allocation on the hot path.
pub struct IndicatorEngine {
    /// Per-instrument indicator state. Index = the DENSE SLOT for a
    /// `security_id`, allocated by [`IndicatorEngine::slot_for`] — **not** the
    /// raw id (see the struct-level note above and §28.2).
    states: Vec<IndicatorState>,
    /// Dense-slot allocator: banded `security_id` -> index into `states`.
    ///
    /// Repaired 2026-08-07 under daily-universe §28.2. The previous code did
    /// `security_id as usize` directly. That worked while ids were small u32
    /// exchange tokens, but the id space is now NAMESPACE-BANDED (Groww index
    /// `[2^62, 2^63)`, GDF `[2^60, 2^62)`, TrueData `[2^59, 2^60)`), so every
    /// live id was astronomically past `states.len()` and EVERY lookup fell
    /// through the bounds check into a default snapshot — a total, silent,
    /// permanent no-op across indicators and strategy.
    slot_of: HashMap<u64, u32>,
    /// Next dense slot to hand out. Monotonic; slots are never reclaimed
    /// (an instrument seen once keeps its state for the process lifetime,
    /// which is exactly the warmup semantics the engine needs).
    next_slot: u32,
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
    /// Count of ticks REFUSED at ingest by [`tick_prices_are_sane`].
    ///
    /// Drives the throttled `warn!` in [`IndicatorEngine::refuse_tick`] — a
    /// line is emitted only on powers of two (1, 2, 4, 8 …), so even a
    /// permanently-poisoned upstream is bounded to ≤64 log lines for the
    /// process lifetime while `tv_indicator_tick_rejected_total` still counts
    /// every single one.
    rejected_ticks: u64,
}

/// Validates the widened price triple BEFORE any indicator state is touched.
///
/// # Why this exists (the bug it closes, 2026-08-09)
///
/// [`tickvault_common::price_precision::f32_to_f64_clean`] deliberately passes
/// `NaN` / `±Inf` straight through, and the Dhan quote parser is PROVEN to emit
/// `NaN` OHLC (`crates/core/src/parser/quote.rs`
/// `test_parse_quote_nan_ohlc_parses_without_panic`) and negative LTP
/// (`test_parse_quote_negative_ltp_parses_without_panic`). `NaN` is ABSORBING
/// under arithmetic, so ONE such packet permanently poisoned the
/// Wilder-smoothed accumulators — `atr`, `adx_tr_smooth`, `adx_plus_dm_smooth`,
/// `adx_minus_dm_smooth`, `adx_value`, `vwap_cumulative_pv`,
/// `vwap_cumulative_vol`, `bb_mean`, `bb_m2`, `sma_running_sum`, `ema_*` — for
/// the process lifetime. `IndicatorSnapshot::sanitize_nan_inf` then masked the
/// SNAPSHOT to `0.0`, so the strategy read plausible zeros instead of an error,
/// and neither `reset_vwap_daily` nor `reset_bollinger_daily` clears
/// `atr` / `adx_*` / `supertrend_*` / `prev_*` — so the daily reset never healed
/// it. Fail closed at ingest is the only place that cannot be poisoned.
///
/// # The rule
///
/// | field | accepted |
/// |---|---|
/// | `price` (LTP) | finite AND `> 0.0` |
/// | `high` / `low` | finite AND `>= 0.0` |
///
/// # Why `high`/`low` allow exactly `0.0` (deliberate, not an oversight)
///
/// A **Ticker** packet carries only LTP + LTT: `parse_ticker_packet` builds its
/// `ParsedTick` with `..Default::default()`, so `day_high`/`day_low` are `0.0`
/// by construction, and `ParsedTick`'s own field docs say "0.0 for Ticker".
/// `0.0` is therefore the documented ABSENT sentinel, not a corrupt price.
/// Rejecting it would silently drop every Ticker-mode instrument — precisely
/// the total-silent-no-op class that §28.2 was written to eliminate. A NEGATIVE
/// high/low has no legitimate producer and IS rejected.
///
/// `is_finite()` is load-bearing and NOT redundant with the comparisons:
/// `NaN > 0.0` is already `false`, but `f64::INFINITY > 0.0` is `true`.
///
/// # Performance
/// O(1), zero allocation — six register compares, no branch on heap data.
#[inline(always)]
#[must_use]
fn tick_prices_are_sane(price: f64, high: f64, low: f64) -> bool {
    price.is_finite()
        && price > 0.0
        && high.is_finite()
        && high >= 0.0
        && low.is_finite()
        && low >= 0.0
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
            slot_of: HashMap::with_capacity(MAX_INDICATOR_INSTRUMENTS),
            next_slot: 0,
            alpha_fast: IndicatorParams::ema_alpha(params.ema_fast_period),
            alpha_slow: IndicatorParams::ema_alpha(params.ema_slow_period),
            alpha_signal: IndicatorParams::ema_alpha(params.macd_signal_period),
            wilder_factor: IndicatorParams::wilder_factor(params.rsi_period),
            adx_wilder_factor: IndicatorParams::wilder_factor(params.adx_period),
            rejected_ticks: 0,
            params,
        }
    }

    /// Cold path: record + report a tick refused by [`tick_prices_are_sane`].
    ///
    /// Kept `#[cold]` + `#[inline(never)]` so the refusal branch contributes
    /// nothing but a not-taken jump to the hot path's instruction footprint.
    ///
    /// Emits NO `ErrorCode` — there is no variant covering indicator-ingest
    /// validation, and inventing one would require the `error_code_*` cross-ref
    /// rule-file edits that are out of this change's scope. The counter is the
    /// machine signal; the throttled `warn!` is the human one.
    ///
    /// The returned snapshot is `is_warm: false`, which the strategy evaluator
    /// treats as an unconditional `Hold` — fail closed, never a trade on
    /// garbage. `slot` carries the instrument's REAL dense slot when one is
    /// already allocated, so nothing downstream mis-indexes slot 0.
    #[cold]
    #[inline(never)]
    fn refuse_tick(
        &mut self,
        tick: &ParsedTick,
        price: f64,
        high: f64,
        low: f64,
    ) -> IndicatorSnapshot {
        metrics::counter!("tv_indicator_tick_rejected_total").increment(1);
        self.rejected_ticks = self.rejected_ticks.saturating_add(1);
        // Powers of two only (1, 2, 4, 8 …): a permanently-broken upstream can
        // never amplify into a log flood, yet the first occurrence is always
        // reported. `is_power_of_two()` is already true for 1.
        if self.rejected_ticks.is_power_of_two() {
            tracing::warn!(
                security_id = tick.security_id,
                last_traded_price = price,
                day_high = high,
                day_low = low,
                rejected_total = self.rejected_ticks,
                "indicator engine REFUSED a tick carrying a non-finite or \
                 non-positive price — indicator state was left UNTOUCHED (a NaN \
                 here would permanently poison the Wilder-smoothed ATR/ADX/VWAP \
                 accumulators, and the daily reset does not clear them). This \
                 instrument produces no signal until clean ticks resume; watch \
                 tv_indicator_tick_rejected_total"
            );
        }
        // APPROVED: a dense slot is always < MAX_INDICATOR_INSTRUMENTS (25_000).
        #[allow(clippy::cast_possible_truncation)]
        let slot = self.slot_lookup(tick.security_id).unwrap_or(0) as u32;
        IndicatorSnapshot {
            security_id: tick.security_id,
            slot,
            ..Default::default()
        }
    }

    /// Resolves a `security_id` to its dense `states` slot WITHOUT allocating.
    ///
    /// Read-only companion to [`Self::slot_for`]. Returns `None` for an id that
    /// has never been seen, so a pure query can never consume capacity.
    ///
    /// # Performance
    /// O(1) average — one hash lookup.
    #[inline(always)]
    fn slot_lookup(&self, security_id: u64) -> Option<usize> {
        self.slot_of.get(&security_id).map(|&s| s as usize)
    }

    /// Resolves a `security_id` to its dense `states` slot, allocating one on
    /// first sight.
    ///
    /// Returns `None` ONLY when the engine is at capacity
    /// (`MAX_INDICATOR_INSTRUMENTS` distinct instruments). That case is
    /// FAIL-CLOSED and LOUD — a coded `error!` plus
    /// `tv_indicator_slot_exhausted_total` — never a silent default.
    ///
    /// # Why a map and not `security_id as usize` (repaired 2026-08-07, §28.2)
    ///
    /// Ids are NAMESPACE-BANDED (Groww index `[2^62, 2^63)`, GDF
    /// `[2^60, 2^62)`, TrueData `[2^59, 2^60)`), so the old direct cast put
    /// every real instrument astronomically past `states.len()`. The bounds
    /// check then returned a default snapshot for EVERY tick — indicators and
    /// strategy were a permanent, silent no-op. The dense mapping restores the
    /// flat-Vec design's intent: the Vec is still one startup allocation, still
    /// index-addressed, still O(1) — only the index is now correct.
    ///
    /// # Performance
    /// O(1) average — one hash lookup on the steady-state path (every tick
    /// after an instrument's first). No heap allocation once the id is known;
    /// the map is pre-sized at construction.
    #[inline(always)]
    fn slot_for(&mut self, security_id: u64) -> Option<usize> {
        if let Some(&slot) = self.slot_of.get(&security_id) {
            return Some(slot as usize);
        }
        let slot = self.next_slot as usize;
        if slot >= self.states.len() {
            // Fail closed + loud. Silence here is what made the pre-2026-08-07
            // bug invisible for as long as it existed.
            metrics::counter!("tv_indicator_slot_exhausted_total").increment(1);
            tracing::error!(
                security_id,
                capacity = self.states.len(),
                "indicator engine slot capacity exhausted — this instrument has \
                 NO indicator state and its strategy will never fire; raise \
                 MAX_INDICATOR_INSTRUMENTS"
            );
            return None;
        }
        self.slot_of.insert(security_id, self.next_slot);
        self.next_slot = self.next_slot.saturating_add(1);
        Some(slot)
    }

    /// Number of dense slots currently allocated.
    ///
    /// Exposed for the wiring/observability tests and for capacity monitoring.
    #[must_use]
    pub fn allocated_slots(&self) -> usize {
        self.next_slot as usize
    }

    /// Test-only: resolve (allocating if needed) and hand back a mutable state.
    ///
    /// Tests used to index `engine.states[security_id as usize]` directly. That
    /// is exactly the raw-id assumption §28.2 removed, so they must go through
    /// the slot allocator like production code does.
    #[cfg(test)]
    fn state_mut_for_test(&mut self, security_id: u64) -> &mut IndicatorState {
        let slot = self
            .slot_for(security_id)
            .expect("test engine must have slot capacity");
        &mut self.states[slot]
    }

    /// Updates all indicators for the given tick and returns a snapshot.
    ///
    /// # Performance
    /// O(1) — every indicator uses a recursive/incremental update, plus one
    /// O(1)-average hash lookup to resolve the instrument's dense slot.
    /// Total: ~200ns on t4g.medium (20 indicators, all O(1)).
    ///
    /// # Safety (bounds)
    /// If the engine is at capacity and this `security_id` has no slot,
    /// returns a default snapshot — after emitting a coded error and bumping
    /// `tv_indicator_slot_exhausted_total`.
    #[inline(always)]
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: all arithmetic is bounded f64 operations with finite checks
    pub fn update(&mut self, tick: &ParsedTick) -> IndicatorSnapshot {
        // Operator-spotted 2026-05-25: f64::from(f32) on price fields
        // widens IEEE-754 (e.g. 23925.65_f32 → 23925.650390625_f64).
        // Every SMA/EMA/RSI/MACD/BB derived from these inputs inherits
        // the widening, and every strategy decision built on those
        // indicators is therefore built on corrupted numbers.
        // data-integrity.md "Price Precision Preservation" mandates
        // f32_to_f64_clean on all f32→f64 price conversions.
        let price = tickvault_common::price_precision::f32_to_f64_clean(tick.last_traded_price);
        let high = tickvault_common::price_precision::f32_to_f64_clean(tick.day_high);
        let low = tickvault_common::price_precision::f32_to_f64_clean(tick.day_low);

        // ---- INGEST GATE (fail closed, BEFORE any state mutation) ----
        // `f32_to_f64_clean` passes NaN/±Inf straight through by design, and the
        // Dhan quote parser provably emits both NaN OHLC and negative LTP. NaN
        // is ABSORBING, so a single bad packet permanently poisons every
        // Wilder-smoothed accumulator below and the daily resets do not clear
        // them. Refuse the tick outright — see `tick_prices_are_sane`.
        //
        // This runs BEFORE `slot_for` on purpose: an instrument that only ever
        // emits garbage must not burn one of the MAX_INDICATOR_INSTRUMENTS
        // dense slots.
        if !tick_prices_are_sane(price, high, low) {
            return self.refuse_tick(tick, price, high, low);
        }

        let Some(sid) = self.slot_for(tick.security_id) else {
            return IndicatorSnapshot {
                security_id: tick.security_id,
                ..Default::default()
            };
        };

        let state = &mut self.states[sid];
        // u32→f64 is exact (no IEEE-754 widening); kept as f64::from — and it
        // can never be non-finite or negative, so it needs no ingest check.
        let volume = f64::from(tick.volume);
        // `close` is snapshot-only (`previous_close`) and never mutates state,
        // so it is deliberately NOT part of the gate: `ParsedTick` documents
        // `day_close = 0.0` for Ticker packets, and `warmup_from_candles`
        // hard-codes `day_close: 0.0`. A non-finite close is still clamped by
        // `sanitize_nan_inf` at the snapshot boundary below.
        let close = tickvault_common::price_precision::f32_to_f64_clean(tick.day_close);

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

        let mut snapshot = IndicatorSnapshot {
            security_id: tick.security_id,
            // §28.3: carry the ALREADY-RESOLVED dense slot downstream. The
            // strategy evaluator indexes by this; deriving it again from
            // security_id is what made it a silent no-op.
            // APPROVED: sid is a dense slot < MAX_INDICATOR_INSTRUMENTS (25_000).
            #[allow(clippy::cast_possible_truncation)]
            slot: sid as u32,
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
        };
        // Phase 0 Item 21 (2026-05-15): final-stage NaN / Inf guard.
        // Strategies silently break when fed NaN — `close < NaN` is
        // false, so a poisoned indicator stops firing. Clamp every
        // non-finite field to 0.0 in-place; emit a counter so the
        // operator sees the issue immediately if it ever fires.
        let nan_fields_cleared = snapshot.sanitize_nan_inf();
        if nan_fields_cleared > 0 {
            // Zero-alloc hot-path counter — static label, no
            // formatting at emission. The operator watches
            // `tv_indicator_nan_guard_fired_total` rate; a non-zero
            // rate is a bug elsewhere in the engine math.
            metrics::counter!("tv_indicator_nan_guard_fired_total")
                .increment(u64::from(nan_fields_cleared));
        }
        snapshot
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

    /// Warms up indicators from historical candle data.
    ///
    /// Feed OHLCV candles in chronological order (oldest first). Each candle
    /// updates all indicators using the close price as LTP. After processing
    /// enough candles (>= warmup threshold), `is_warm` becomes true.
    ///
    /// # Usage
    /// Call before market open with 90 days of daily or 1-minute candles.
    /// After warmup, the first live tick produces meaningful indicator values
    /// instead of starting cold.
    ///
    /// # Performance
    /// O(N × M) where N = number of candles and M = number of instruments.
    /// Cold path — runs once at startup, not during live trading.
    // O(1) EXEMPT: begin — cold path startup warmup (runs once before tick processing)
    pub fn warmup_from_candles(
        &mut self,
        security_id: u64,
        candles: &[(f64, f64, f64, f64, f64)], // (open, high, low, close, volume)
    ) -> usize {
        if candles.is_empty() {
            return 0;
        }
        // Reserve the slot up-front so a capacity refusal short-circuits here
        // rather than being re-discovered (and re-logged) per synthetic tick.
        // The body drives `update()`, which resolves the same slot itself.
        if self.slot_for(security_id).is_none() {
            return 0;
        }

        let mut processed = 0_usize;
        let mut rejected = 0_u64;

        for &(open, high, low, close, volume) in candles {
            // ---- CANDLE GATE (fail closed) ----
            // The pre-2026-08-09 check validated `close` ONLY, in f64, so:
            //   * a NaN `high`/`low`/`open` sailed through and poisoned the
            //     Wilder accumulators exactly like a NaN live tick, and
            //   * a large-but-finite f64 (e.g. 1e300) passed `close.is_finite()`
            //     and then OVERFLOWED to `+inf` in the `close as f32` cast
            //     below — non-finite state from an input that "looked" valid.
            // Validating the CONVERTED f32 closes both holes at once: `f64 as
            // f32` maps NaN→NaN and any out-of-range magnitude→±inf, so a
            // single `is_finite()` on the result covers non-finite inputs AND
            // cast overflow.
            let close_f32 = close as f32;
            let high_f32 = high as f32;
            let low_f32 = low as f32;
            let open_f32 = open as f32;
            let candle_ok = close_f32.is_finite()
                && close_f32 > 0.0
                && high_f32.is_finite()
                && high_f32 >= 0.0
                && low_f32.is_finite()
                && low_f32 >= 0.0
                && open_f32.is_finite()
                && open_f32 >= 0.0
                // `volume as u32` saturates: NaN→0 and a negative→0 would both
                // be silently WRONG rather than loud, so gate them here.
                && volume.is_finite()
                && volume >= 0.0;
            if !candle_ok {
                rejected = rejected.saturating_add(1);
                continue;
            }

            // Create a synthetic tick from candle data
            let tick = ParsedTick {
                security_id,
                exchange_segment_code: 0,
                last_traded_price: close_f32,
                day_high: high_f32,
                day_low: low_f32,
                day_open: open_f32,
                day_close: 0.0, // Not used for indicator calculation
                volume: volume as u32,
                average_traded_price: close_f32,
                ..ParsedTick::default()
            };

            // Feed through the normal update path
            let _ = self.update(&tick);
            processed = processed.saturating_add(1);
        }
        // O(1) EXEMPT: end

        if rejected > 0 {
            // Cold path (startup), fired at most once per warmup call — one
            // aggregate line, never per-candle, so a wholly-corrupt REST
            // response cannot flood the boot log.
            metrics::counter!("tv_indicator_warmup_candle_rejected_total").increment(rejected);
            tracing::warn!(
                security_id,
                rejected,
                processed,
                total = candles.len(),
                "indicator warmup REFUSED candles carrying a non-finite or \
                 non-positive value (or one that overflows the f32 tick wire \
                 format) — they were skipped, not fed into indicator state; if \
                 `processed` is far below `total` this instrument may never \
                 reach warmup and its strategy will never fire"
            );
        }

        processed
    }

    /// Returns the warmup count for a given security.
    pub fn warmup_count(&self, security_id: u64) -> u16 {
        // Read-only: must NOT allocate a slot for an unseen id, or merely
        // asking "is this warm?" would consume capacity.
        match self.slot_lookup(security_id) {
            Some(sid) => self.states[sid].warmup_count,
            None => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::MAX_INDICATOR_WARMUP_TICKS;

    fn make_tick(security_id: u64, ltp: f32, high: f32, low: f32, volume: u32) -> ParsedTick {
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
    fn test_large_security_id_is_processed_not_silently_dropped() {
        // REGRESSION 2026-08-07 (daily-universe §28.2): this test previously
        // asserted that a large `security_id` returns a DEFAULT snapshot, i.e.
        // it encoded the bug as the contract. Ids are namespace-banded now
        // (Groww index [2^62, 2^63), GDF [2^60, 2^62), TrueData [2^59, 2^60)),
        // so "large" describes EVERY live instrument — the old assertion meant
        // indicators were a permanent silent no-op for the entire universe.
        let mut engine = default_engine();
        let tick = ParsedTick {
            security_id: u64::from(u32::MAX),
            last_traded_price: 100.0,
            ..Default::default()
        };
        let snap = engine.update(&tick);
        assert_eq!(snap.security_id, u64::from(u32::MAX));
        assert_eq!(
            snap.ema_fast, 100.0,
            "a large security_id must get a dense slot and be PROCESSED; \
             returning defaults here is the pre-§28.2 silent no-op"
        );
        assert_eq!(engine.warmup_count(u64::from(u32::MAX)), 1);
    }

    #[test]
    fn test_namespace_banded_ids_are_processed() {
        // The real shapes: Groww index bit 62, GDF token bit 61, TrueData bit 59.
        let mut engine = default_engine();
        for (label, id) in [
            ("groww_index", 1_u64 << 62),
            ("gdf_token", 1_u64 << 61),
            ("truedata", 1_u64 << 59),
        ] {
            let tick = ParsedTick {
                security_id: id,
                last_traded_price: 250.0,
                ..Default::default()
            };
            let snap = engine.update(&tick);
            assert_eq!(
                snap.ema_fast, 250.0,
                "{label} id {id} must be processed, not silently defaulted"
            );
        }
        assert_eq!(
            engine.allocated_slots(),
            3,
            "one dense slot per distinct id"
        );
    }

    #[test]
    fn test_distinct_banded_ids_do_not_share_state() {
        let mut engine = default_engine();
        let a = 1_u64 << 62;
        let b = (1_u64 << 62) + 1;
        engine.update(&ParsedTick {
            security_id: a,
            last_traded_price: 100.0,
            ..Default::default()
        });
        engine.update(&ParsedTick {
            security_id: b,
            last_traded_price: 900.0,
            ..Default::default()
        });
        let snap_a = engine.update(&ParsedTick {
            security_id: a,
            last_traded_price: 100.0,
            ..Default::default()
        });
        assert!(
            (snap_a.ema_fast - 900.0).abs() > 1.0,
            "two distinct ids must not collapse into one slot"
        );
        assert_eq!(engine.warmup_count(a), 2);
        assert_eq!(engine.warmup_count(b), 1);
    }

    #[test]
    fn test_allocated_slots_not_incremented_by_readonly_warmup_count() {
        // A read-only query must never consume capacity.
        let mut engine = default_engine();
        assert_eq!(engine.warmup_count(1_u64 << 62), 0);
        assert_eq!(
            engine.allocated_slots(),
            0,
            "warmup_count() must not allocate — otherwise merely asking \
             'is this warm?' burns a slot"
        );
        engine.update(&ParsedTick {
            security_id: 1_u64 << 62,
            last_traded_price: 10.0,
            ..Default::default()
        });
        assert_eq!(engine.allocated_slots(), 1);
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
    fn test_large_security_id_preserves_id_and_is_processed() {
        // REGRESSION 2026-08-07 (§28.2): id preservation was always correct;
        // the surrounding "and everything is default" expectation was the bug.
        let mut engine = default_engine();
        let large_id = MAX_INDICATOR_INSTRUMENTS as u64;
        let tick = ParsedTick {
            security_id: large_id,
            last_traded_price: 500.0,
            day_high: 510.0,
            day_low: 490.0,
            volume: 10000,
            ..Default::default()
        };
        let snap = engine.update(&tick);
        assert_eq!(snap.security_id, large_id, "id must round-trip");
        assert_eq!(
            snap.ema_fast, 500.0,
            "and the tick must actually be processed"
        );
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
        let max_valid = (MAX_INDICATOR_INSTRUMENTS - 1) as u64;
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
        let sid = 100_u64;

        // Manually set warmup_count close to u16::MAX
        engine.state_mut_for_test(sid).warmup_count = u16::MAX - 1;

        // One more tick should bring it to u16::MAX
        let tick = make_tick(100, 100.0, 105.0, 95.0, 1000);
        engine.update(&tick);
        assert_eq!(engine.state_mut_for_test(sid).warmup_count, u16::MAX);

        // Another tick should remain at u16::MAX (saturating)
        engine.update(&tick);
        assert_eq!(
            engine.state_mut_for_test(sid).warmup_count,
            u16::MAX,
            "warmup_count must saturate at u16::MAX, not overflow"
        );
    }

    #[test]
    fn test_sma_period_zero_returns_zero_sma() {
        // Covers the sma_count == 0 branch (line 281) where sma_period = 0
        // causes min(ring_len, 0) = 0 regardless of ring contents.
        let params = IndicatorParams {
            sma_period: 0,
            ..Default::default()
        };
        let mut engine = IndicatorEngine::new(params);
        let tick = make_tick(100, 250.0, 260.0, 240.0, 1000);
        let snap = engine.update(&tick);
        assert_eq!(snap.sma, 0.0, "sma_period=0 must yield sma=0.0");
    }

    // -----------------------------------------------------------------------
    // Additional coverage: out-of-bounds edge, warmup saturation, zero-volume VWAP
    // -----------------------------------------------------------------------

    #[test]
    fn test_id_numerically_above_capacity_is_still_processed() {
        // REGRESSION 2026-08-07 (§28.2): the id's NUMERIC value is irrelevant
        // now — only the COUNT of distinct instruments is bounded. This test
        // previously asserted a default snapshot, which was the silent no-op.
        let mut engine = default_engine();
        let big_id = MAX_INDICATOR_INSTRUMENTS as u64;
        let tick = make_tick(big_id, 500.0, 510.0, 490.0, 10000);
        let snap = engine.update(&tick);
        assert_eq!(snap.security_id, big_id);
        assert_eq!(
            snap.ema_fast, 500.0,
            "an id numerically above capacity must still get slot 0"
        );
        assert_eq!(engine.allocated_slots(), 1);
    }

    #[test]
    fn test_slot_capacity_exhaustion_is_fail_closed_and_counted() {
        // The ONLY case that may return a default snapshot: genuinely running
        // out of distinct slots. It must be loud (counter + coded error), not
        // silent.
        let mut engine = default_engine();
        for i in 0..MAX_INDICATOR_INSTRUMENTS {
            let id = (1_u64 << 62) + i as u64;
            let snap = engine.update(&make_tick(id, 100.0, 101.0, 99.0, 1));
            assert_eq!(snap.ema_fast, 100.0, "instrument {i} must fit");
        }
        assert_eq!(engine.allocated_slots(), MAX_INDICATOR_INSTRUMENTS);

        // One past capacity -> default snapshot, id preserved.
        let overflow_id = (1_u64 << 62) + MAX_INDICATOR_INSTRUMENTS as u64;
        let snap = engine.update(&make_tick(overflow_id, 777.0, 778.0, 776.0, 1));
        assert_eq!(snap.security_id, overflow_id);
        assert_eq!(snap.ema_fast, 0.0);
        assert!(!snap.is_warm);
        // Capacity must NOT have moved.
        assert_eq!(engine.allocated_slots(), MAX_INDICATOR_INSTRUMENTS);
        // An already-known id still works after the refusal.
        let known = engine.update(&make_tick(1_u64 << 62, 100.0, 101.0, 99.0, 1));
        assert!(known.ema_fast > 0.0, "refusal must not poison known slots");
    }

    #[test]
    fn test_zero_volume_vwap_stays_unchanged_across_ticks() {
        let mut engine = default_engine();
        // First tick with volume → VWAP set
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let snap1 = engine.update(&make_tick(100, 110.0, 115.0, 105.0, 1000));
        let vwap_after_volume = snap1.vwap;
        assert!(
            vwap_after_volume > 0.0,
            "VWAP should be positive with volume"
        );

        // Subsequent tick with zero volume → VWAP unchanged
        let snap2 = engine.update(&make_tick(100, 120.0, 125.0, 115.0, 0));
        assert!(
            (snap2.vwap - vwap_after_volume).abs() < f64::EPSILON,
            "zero volume must not change VWAP accumulator"
        );
    }

    #[test]
    fn test_warmup_count_starts_at_one_after_first_tick() {
        let mut engine = default_engine();
        let sid = 50_u64;
        let tick = make_tick(50, 100.0, 105.0, 95.0, 1000);
        engine.update(&tick);
        assert_eq!(engine.state_mut_for_test(sid).warmup_count, 1);
    }

    #[test]
    fn test_warmup_saturation_does_not_affect_indicator_computation() {
        let mut engine = default_engine();
        let sid = 100_u64;

        // Set warmup_count to u16::MAX
        engine.state_mut_for_test(sid).warmup_count = u16::MAX;

        // Feed two ticks at different prices
        let tick1 = make_tick(100, 100.0, 105.0, 95.0, 1000);
        let snap1 = engine.update(&tick1);
        assert!(snap1.is_warm);

        let tick2 = make_tick(100, 110.0, 115.0, 105.0, 1000);
        let snap2 = engine.update(&tick2);

        // EMA should have moved toward 110
        assert!(
            snap2.ema_fast > snap1.ema_fast,
            "EMA must still update at saturated warmup"
        );
        // warmup_count stays at MAX
        assert_eq!(engine.state_mut_for_test(sid).warmup_count, u16::MAX);
    }

    #[test]
    fn test_multiple_zero_volume_ticks_vwap_remains_zero() {
        let mut engine = default_engine();
        // All zero-volume ticks → VWAP stays at 0
        for i in 0..10 {
            let snap = engine.update(&make_tick(100, 100.0 + i as f32, 110.0, 90.0, 0));
            assert_eq!(
                snap.vwap, 0.0,
                "zero volume across all ticks must keep VWAP at 0"
            );
        }
    }

    #[test]
    fn test_bollinger_single_tick_stddev_is_zero() {
        let mut engine = default_engine();
        let snap = engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        // With only one data point, variance = 0, so upper == middle == lower
        assert!(
            (snap.bollinger_upper - snap.bollinger_lower).abs() < f64::EPSILON,
            "single tick: bollinger bands must collapse"
        );
    }

    #[test]
    fn test_warmup_counter_at_max_still_produces_valid_indicators() {
        let mut engine = default_engine();
        let sid = 100_u64;

        // Set warmup to max — is_warm should be true
        engine.state_mut_for_test(sid).warmup_count = u16::MAX;

        let tick = make_tick(100, 100.0, 105.0, 95.0, 1000);
        let snap = engine.update(&tick);
        assert!(snap.is_warm, "u16::MAX warmup_count must be warm");
        // EMA should still update correctly
        assert!(snap.ema_fast > 0.0);
    }

    // -----------------------------------------------------------------------
    // Warmup from Historical Candles
    // -----------------------------------------------------------------------

    #[test]
    fn test_warmup_from_candles_empty() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        let result = engine.warmup_from_candles(100, &[]);
        assert_eq!(result, 0);
    }

    #[test]
    fn test_warmup_from_candles_processes_all() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        let candles: Vec<(f64, f64, f64, f64, f64)> = (0..50)
            .map(|i| {
                let price = 100.0 + i as f64;
                (price - 1.0, price + 2.0, price - 2.0, price, 1000.0)
            })
            .collect();
        let processed = engine.warmup_from_candles(100, &candles);
        assert_eq!(processed, 50);
    }

    #[test]
    fn test_warmup_makes_engine_warm() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        // Feed 40 candles (above warmup threshold of 30)
        let candles: Vec<(f64, f64, f64, f64, f64)> = (0..40)
            .map(|i| {
                let price = 100.0 + i as f64;
                (price, price + 1.0, price - 1.0, price, 1000.0)
            })
            .collect();
        engine.warmup_from_candles(100, &candles);
        assert!(engine.warmup_count(100) >= 30);
    }

    #[test]
    fn test_warmup_indicators_have_values() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        let candles: Vec<(f64, f64, f64, f64, f64)> = (0..50)
            .map(|i| {
                let price = 100.0 + (i as f64 * 0.5);
                (price, price + 2.0, price - 1.0, price, 5000.0)
            })
            .collect();
        engine.warmup_from_candles(200, &candles);

        // After warmup, feed one more tick to get a snapshot
        let tick = ParsedTick {
            security_id: 200,
            last_traded_price: 125.0,
            day_high: 126.0,
            day_low: 124.0,
            day_open: 124.5,
            volume: 5000,
            average_traded_price: 125.0,
            ..ParsedTick::default()
        };
        let snap = engine.update(&tick);
        assert!(snap.is_warm);
        assert!(snap.ema_fast > 0.0);
        assert!(snap.ema_slow > 0.0);
        assert!(snap.sma > 0.0);
        assert!(snap.rsi > 0.0 && snap.rsi <= 100.0);
        assert!(snap.atr >= 0.0);
    }

    #[test]
    fn test_warmup_skips_invalid_candles() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        let candles = vec![
            (100.0, 101.0, 99.0, 100.0, 1000.0),           // valid
            (0.0, 0.0, 0.0, 0.0, 0.0),                     // invalid (close=0)
            (f64::NAN, f64::NAN, f64::NAN, f64::NAN, 0.0), // invalid (NaN)
            (102.0, 103.0, 101.0, 102.0, 2000.0),          // valid
        ];
        let processed = engine.warmup_from_candles(300, &candles);
        assert_eq!(processed, 2); // Only 2 valid candles
    }

    #[test]
    fn test_warmup_accepts_large_security_id() {
        // REGRESSION 2026-08-07 (§28.2): warmup used the same raw-id cast, so
        // REST warmup silently processed 0 candles for every banded id — the
        // strategies would never have become warm.
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        let candles = vec![(100.0, 101.0, 99.0, 100.0, 1000.0)];
        let processed = engine.warmup_from_candles(u64::from(u32::MAX), &candles);
        assert_eq!(processed, 1, "a large id must warm up, not silently skip");
        assert_eq!(engine.warmup_count(u64::from(u32::MAX)), 1);
    }

    #[test]
    fn test_warmup_count_method() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        assert_eq!(engine.warmup_count(100), 0); // Initially zero
        let candles: Vec<(f64, f64, f64, f64, f64)> = (0..10)
            .map(|i| (100.0 + i as f64, 101.0, 99.0, 100.0 + i as f64, 1000.0))
            .collect();
        engine.warmup_from_candles(100, &candles);
        assert_eq!(engine.warmup_count(100), 10);
    }

    #[test]
    fn test_warmup_count_oob() {
        let params = IndicatorParams::default();
        let engine = IndicatorEngine::new(params);
        assert_eq!(engine.warmup_count(u64::from(u32::MAX)), 0);
    }

    // -----------------------------------------------------------------------
    // Exact-Value Indicator Tests (mutation-targeted)
    // -----------------------------------------------------------------------

    #[test]
    fn test_ema_exact_value_after_two_ticks() {
        // EMA(12) alpha = 2/(12+1) = 0.153846...
        // Tick 1: EMA = 100.0 (first tick initializes to price)
        // Tick 2: EMA = alpha * 105.0 + (1-alpha) * 100.0
        //       = 0.153846 * 105.0 + 0.846154 * 100.0
        //       = 16.1538 + 84.6154 = 100.7692...
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let tick1 = ParsedTick {
            security_id: 1,
            last_traded_price: 100.0,
            day_high: 101.0,
            day_low: 99.0,
            volume: 1000,
            average_traded_price: 100.0,
            ..ParsedTick::default()
        };
        let snap1 = engine.update(&tick1);
        assert!(
            (snap1.ema_fast - 100.0).abs() < 0.001,
            "First tick EMA must equal price"
        );

        let tick2 = ParsedTick {
            security_id: 1,
            last_traded_price: 105.0,
            day_high: 106.0,
            day_low: 104.0,
            volume: 2000,
            average_traded_price: 105.0,
            ..ParsedTick::default()
        };
        let snap2 = engine.update(&tick2);
        // If mutation changes + to - in EMA: result would be very different
        // If mutation changes * to / in EMA: result would be very different
        assert!(snap2.ema_fast > 100.0, "EMA must increase when price rises");
        assert!(snap2.ema_fast < 105.0, "EMA must lag behind price");
        // Exact check: EMA should be approximately 100.769
        assert!(
            (snap2.ema_fast - 100.769).abs() < 0.1,
            "EMA fast after 2 ticks must be ~100.77, got {}",
            snap2.ema_fast
        );
    }

    #[test]
    fn test_rsi_after_known_sequence() {
        // Feed: 100, 102, 101, 103, 100, 104
        // Alternating gains and losses → RSI should be near 50
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let prices = [
            100.0_f32, 102.0, 101.0, 103.0, 100.0, 104.0, 99.0, 105.0, 98.0, 106.0,
        ];
        let mut last_snap = IndicatorSnapshot::default();
        for &price in &prices {
            let tick = ParsedTick {
                security_id: 2,
                last_traded_price: price,
                day_high: price + 1.0,
                day_low: price - 1.0,
                volume: 1000,
                average_traded_price: price,
                ..ParsedTick::default()
            };
            last_snap = engine.update(&tick);
        }
        // RSI must be in valid range
        assert!(
            last_snap.rsi >= 0.0 && last_snap.rsi <= 100.0,
            "RSI out of range: {}",
            last_snap.rsi
        );
        // With alternating up/down, RSI should be roughly balanced (30-70)
        assert!(
            last_snap.rsi > 20.0 && last_snap.rsi < 80.0,
            "RSI with alternating prices should be near 50, got {}",
            last_snap.rsi
        );
    }

    #[test]
    fn test_macd_signal_divergence() {
        // Feed consistently rising prices → MACD line should be positive
        // (fast EMA > slow EMA when prices rise)
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let mut last_snap = IndicatorSnapshot::default();
        for i in 0..40_u32 {
            let price = 100.0 + i as f32 * 2.0; // Steadily rising
            let tick = ParsedTick {
                security_id: 3,
                last_traded_price: price,
                day_high: price + 0.5,
                day_low: price - 0.5,
                volume: 1000,
                average_traded_price: price,
                ..ParsedTick::default()
            };
            last_snap = engine.update(&tick);
        }
        // MACD line = EMA_fast - EMA_slow → positive for rising prices
        assert!(
            last_snap.macd_line > 0.0,
            "MACD line must be positive for rising prices, got {}",
            last_snap.macd_line
        );
        // Signal should also be positive but lag
        assert!(
            last_snap.macd_signal > 0.0,
            "MACD signal must be positive, got {}",
            last_snap.macd_signal
        );
        // Histogram = line - signal
        let expected_hist = last_snap.macd_line - last_snap.macd_signal;
        assert!(
            (last_snap.macd_histogram - expected_hist).abs() < 0.001,
            "MACD histogram must equal line - signal"
        );
    }

    #[test]
    fn test_atr_positive_after_volatile_sequence() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let mut last_snap = IndicatorSnapshot::default();
        for i in 0..20_u32 {
            // Alternating high volatility
            let base = 100.0 + if i % 2 == 0 { 10.0 } else { -10.0 };
            let tick = ParsedTick {
                security_id: 4,
                last_traded_price: base as f32,
                day_high: (base + 5.0) as f32,
                day_low: (base - 5.0) as f32,
                volume: 1000,
                average_traded_price: base as f32,
                ..ParsedTick::default()
            };
            last_snap = engine.update(&tick);
        }
        // ATR must be positive for volatile data
        assert!(
            last_snap.atr > 0.0,
            "ATR must be positive, got {}",
            last_snap.atr
        );
        // ATR should reflect the ~10-20 point swings
        assert!(
            last_snap.atr > 1.0,
            "ATR too small for volatile data: {}",
            last_snap.atr
        );
    }

    #[test]
    fn test_bollinger_bands_relationship() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let mut last_snap = IndicatorSnapshot::default();
        for i in 0..30_u32 {
            let price = 100.0 + (i as f32 * 0.1);
            let tick = ParsedTick {
                security_id: 5,
                last_traded_price: price,
                day_high: price + 0.5,
                day_low: price - 0.5,
                volume: 1000,
                average_traded_price: price,
                ..ParsedTick::default()
            };
            last_snap = engine.update(&tick);
        }
        // Bollinger: upper > middle > lower (always)
        assert!(
            last_snap.bollinger_upper >= last_snap.bollinger_middle,
            "Upper band must be >= middle"
        );
        assert!(
            last_snap.bollinger_middle >= last_snap.bollinger_lower,
            "Middle band must be >= lower"
        );
        // Middle band is Bollinger mean (Welford's online — separate from SMA ring buffer)
        assert!(
            last_snap.bollinger_middle > 0.0,
            "Bollinger middle must be positive"
        );
    }

    #[test]
    fn test_obv_increases_on_up_tick() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // First tick
        let tick1 = ParsedTick {
            security_id: 6,
            last_traded_price: 100.0,
            day_high: 101.0,
            day_low: 99.0,
            volume: 1000,
            average_traded_price: 100.0,
            ..ParsedTick::default()
        };
        engine.update(&tick1);

        // Up tick → OBV should increase by volume
        let tick2 = ParsedTick {
            security_id: 6,
            last_traded_price: 105.0,
            day_high: 106.0,
            day_low: 104.0,
            volume: 2000,
            average_traded_price: 105.0,
            ..ParsedTick::default()
        };
        let snap2 = engine.update(&tick2);
        assert!(
            snap2.obv > 0.0,
            "OBV must increase on up tick, got {}",
            snap2.obv
        );

        // Down tick → OBV should decrease
        let tick3 = ParsedTick {
            security_id: 6,
            last_traded_price: 95.0,
            day_high: 96.0,
            day_low: 94.0,
            volume: 3000,
            average_traded_price: 95.0,
            ..ParsedTick::default()
        };
        let snap3 = engine.update(&tick3);
        assert!(snap3.obv < snap2.obv, "OBV must decrease on down tick");
    }

    #[test]
    fn test_supertrend_direction_for_trending_market() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        let mut last_snap = IndicatorSnapshot::default();
        // Feed 30 consistently rising ticks
        for i in 0..30_u32 {
            let price = 100.0 + i as f32 * 3.0;
            let tick = ParsedTick {
                security_id: 7,
                last_traded_price: price,
                day_high: price + 1.0,
                day_low: price - 1.0,
                volume: 1000,
                average_traded_price: price,
                ..ParsedTick::default()
            };
            last_snap = engine.update(&tick);
        }
        // Strong uptrend → supertrend should be bullish
        assert!(
            last_snap.supertrend_bullish,
            "Supertrend must be bullish in uptrend"
        );
        assert!(
            last_snap.supertrend > 0.0,
            "Supertrend value must be positive"
        );
    }

    // -----------------------------------------------------------------------
    // NaN / Inf / non-positive INGEST GATE  (bug fixed 2026-08-09)
    //
    // `f32_to_f64_clean` passes NaN/Inf through by design and the Dhan quote
    // parser provably emits NaN OHLC + negative LTP. NaN is ABSORBING, so one
    // bad packet used to poison atr / adx_* / vwap_* / bb_* forever, while
    // `sanitize_nan_inf` masked the SNAPSHOT to plausible 0.0 and the daily
    // resets never cleared the poisoned fields.
    // -----------------------------------------------------------------------

    /// Every state field the Wilder/cumulative math can permanently absorb a
    /// NaN into. `sanitize_nan_inf` masks the SNAPSHOT, so the only honest
    /// assertion is against the STATE itself.
    fn assert_state_finite(engine: &mut IndicatorEngine, sid: u64, ctx: &str) {
        let s = engine.state_mut_for_test(sid);
        for (name, v) in [
            ("ema_fast", s.ema_fast),
            ("ema_slow", s.ema_slow),
            ("macd_signal_ema", s.macd_signal_ema),
            ("rsi_avg_gain", s.rsi_avg_gain),
            ("rsi_avg_loss", s.rsi_avg_loss),
            ("atr", s.atr),
            ("adx_plus_dm_smooth", s.adx_plus_dm_smooth),
            ("adx_minus_dm_smooth", s.adx_minus_dm_smooth),
            ("adx_tr_smooth", s.adx_tr_smooth),
            ("adx_value", s.adx_value),
            ("vwap_cumulative_pv", s.vwap_cumulative_pv),
            ("vwap_cumulative_vol", s.vwap_cumulative_vol),
            ("bb_mean", s.bb_mean),
            ("bb_m2", s.bb_m2),
            ("sma_running_sum", s.sma_running_sum),
            ("supertrend_upper", s.supertrend_upper),
            ("supertrend_lower", s.supertrend_lower),
            ("prev_close", s.prev_close),
            ("prev_high", s.prev_high),
            ("prev_low", s.prev_low),
        ] {
            assert!(v.is_finite(), "{ctx}: state.{name} became non-finite ({v})");
        }
    }

    // --- the pure gate ---

    #[test]
    fn test_tick_prices_are_sane_accepts_ordinary_prices() {
        assert!(tick_prices_are_sane(100.0, 105.0, 95.0));
        // Ticker packets carry high/low = 0.0 (the documented ABSENT sentinel).
        assert!(tick_prices_are_sane(100.0, 0.0, 0.0));
    }

    #[test]
    fn test_tick_prices_are_sane_rejects_every_poison_shape() {
        // NaN in any of the three.
        assert!(!tick_prices_are_sane(f64::NAN, 105.0, 95.0));
        assert!(!tick_prices_are_sane(100.0, f64::NAN, 95.0));
        assert!(!tick_prices_are_sane(100.0, 105.0, f64::NAN));
        // ±Inf in any of the three — `is_finite()` is load-bearing here
        // because `f64::INFINITY > 0.0` is TRUE.
        assert!(!tick_prices_are_sane(f64::INFINITY, 105.0, 95.0));
        assert!(!tick_prices_are_sane(f64::NEG_INFINITY, 105.0, 95.0));
        assert!(!tick_prices_are_sane(100.0, f64::INFINITY, 95.0));
        assert!(!tick_prices_are_sane(100.0, 105.0, f64::NEG_INFINITY));
        // Non-positive LTP (the parser proves -500.0 and 0.0 reach us).
        assert!(!tick_prices_are_sane(0.0, 105.0, 95.0));
        assert!(!tick_prices_are_sane(-0.0, 105.0, 95.0));
        assert!(!tick_prices_are_sane(-500.0, 105.0, 95.0));
        // Negative high/low has no legitimate producer.
        assert!(!tick_prices_are_sane(100.0, -1.0, 95.0));
        assert!(!tick_prices_are_sane(100.0, 105.0, -1.0));
    }

    // --- update(): rejection is silent-safe, state-preserving, slot-preserving ---

    #[test]
    fn test_nan_day_high_is_rejected_and_state_untouched() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        engine.update(&make_tick(100, 102.0, 108.0, 96.0, 1000));
        let good_atr = engine.state_mut_for_test(100).atr;
        assert!(good_atr > 0.0, "precondition: ATR warmed up");

        let snap = engine.update(&make_tick(100, 103.0, f32::NAN, 97.0, 1000));

        assert!(!snap.is_warm, "a refused tick must be fail-closed (Hold)");
        assert_eq!(snap.security_id, 100, "id must still round-trip");
        assert_eq!(
            engine.state_mut_for_test(100).atr,
            good_atr,
            "NaN day_high must leave ATR byte-identical — this is the poison \
             the pre-2026-08-09 engine absorbed permanently"
        );
        assert_state_finite(&mut engine, 100, "after NaN day_high");
    }

    #[test]
    fn test_nan_day_low_is_rejected_and_state_untouched() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        engine.update(&make_tick(100, 102.0, 108.0, 96.0, 1000));
        let before = engine.state_mut_for_test(100).adx_tr_smooth;

        let snap = engine.update(&make_tick(100, 103.0, 109.0, f32::NAN, 1000));

        assert!(!snap.is_warm);
        assert_eq!(engine.state_mut_for_test(100).adx_tr_smooth, before);
        assert_state_finite(&mut engine, 100, "after NaN day_low");
    }

    #[test]
    fn test_nan_ltp_is_rejected_and_state_untouched() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let before = engine.state_mut_for_test(100).ema_fast;

        let snap = engine.update(&make_tick(100, f32::NAN, 105.0, 95.0, 1000));

        assert!(!snap.is_warm);
        assert_eq!(engine.state_mut_for_test(100).ema_fast, before);
        assert_state_finite(&mut engine, 100, "after NaN ltp");
    }

    #[test]
    fn test_infinite_prices_are_rejected() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        engine.update(&make_tick(100, 101.0, 106.0, 96.0, 1000));
        let before_vwap_pv = engine.state_mut_for_test(100).vwap_cumulative_pv;

        for bad in [
            make_tick(100, f32::INFINITY, 105.0, 95.0, 1000),
            make_tick(100, f32::NEG_INFINITY, 105.0, 95.0, 1000),
            make_tick(100, 102.0, f32::INFINITY, 95.0, 1000),
            make_tick(100, 102.0, 105.0, f32::NEG_INFINITY, 1000),
        ] {
            let snap = engine.update(&bad);
            assert!(!snap.is_warm, "infinite price must be refused");
        }
        assert_eq!(
            engine.state_mut_for_test(100).vwap_cumulative_pv,
            before_vwap_pv,
            "VWAP accumulator is cumulative — an Inf would never wash out"
        );
        assert_state_finite(&mut engine, 100, "after Inf burst");
    }

    #[test]
    fn test_negative_ltp_is_rejected() {
        // `parse_quote_packet` accepts -500.0 (proven by its own parser test),
        // so the engine is the layer that must refuse it.
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let before = engine.state_mut_for_test(100).ema_fast;

        let snap = engine.update(&make_tick(100, -500.0, 105.0, 95.0, 1000));

        assert!(!snap.is_warm);
        assert_eq!(engine.state_mut_for_test(100).ema_fast, before);
        assert!(before > 0.0, "EMA must not have been dragged negative");
    }

    #[test]
    fn test_zero_and_negative_zero_ltp_are_rejected() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let before = engine.state_mut_for_test(100).ema_fast;

        assert!(
            !engine
                .update(&make_tick(100, 0.0, 105.0, 95.0, 1000))
                .is_warm
        );
        assert!(
            !engine
                .update(&make_tick(100, -0.0, 105.0, 95.0, 1000))
                .is_warm
        );

        assert_eq!(engine.state_mut_for_test(100).ema_fast, before);
        assert_eq!(
            engine.warmup_count(100),
            1,
            "a refused tick must not advance warmup"
        );
    }

    #[test]
    fn test_negative_high_or_low_is_rejected() {
        let mut engine = default_engine();
        engine.update(&make_tick(100, 100.0, 105.0, 95.0, 1000));
        let (high_before, low_before) = {
            let s = engine.state_mut_for_test(100);
            (s.prev_high, s.prev_low)
        };
        assert_eq!((high_before, low_before), (105.0, 95.0), "precondition");

        // Asserted after EACH bad tick, and on BOTH fields: checking only
        // `prev_high` after both ticks would pass VACUOUSLY, because a later
        // tick with a valid high restores it to the original value.
        engine.update(&make_tick(100, 100.0, -105.0, 95.0, 1000));
        {
            let s = engine.state_mut_for_test(100);
            assert_eq!(s.prev_high, high_before, "negative high must be refused");
            assert_eq!(s.prev_low, low_before);
        }

        engine.update(&make_tick(100, 100.0, 105.0, -95.0, 1000));
        {
            let s = engine.state_mut_for_test(100);
            assert_eq!(s.prev_high, high_before);
            assert_eq!(s.prev_low, low_before, "negative low must be refused");
        }

        assert_eq!(
            engine.warmup_count(100),
            1,
            "neither refused tick may advance warmup"
        );
    }

    #[test]
    fn test_ticker_mode_zero_high_low_is_still_accepted() {
        // REGRESSION GUARD for the gate's deliberate carve-out. A Ticker packet
        // is built with `..Default::default()`, so day_high/day_low are 0.0 —
        // the documented ABSENT sentinel, not a corrupt price. Rejecting it
        // would silently drop every Ticker-mode instrument, which is exactly
        // the total-silent-no-op class §28.2 was written to eliminate.
        let mut engine = default_engine();
        let ticker_like = ParsedTick {
            security_id: 1_u64 << 62,
            last_traded_price: 250.0,
            ..Default::default()
        };
        let snap = engine.update(&ticker_like);
        assert_eq!(
            snap.ema_fast, 250.0,
            "Ticker-mode tick (high=low=0.0) MUST be processed, not refused"
        );
        assert_eq!(engine.allocated_slots(), 1);
    }

    #[test]
    fn test_rejected_tick_for_unknown_id_does_not_burn_a_slot() {
        // The gate runs BEFORE slot_for on purpose: an instrument that only
        // ever emits garbage must not consume finite slot capacity.
        let mut engine = default_engine();
        let snap = engine.update(&make_tick(1_u64 << 62, f32::NAN, 105.0, 95.0, 1000));
        assert_eq!(snap.security_id, 1_u64 << 62, "id still round-trips");
        assert_eq!(
            engine.allocated_slots(),
            0,
            "a poison-only instrument must not consume a dense slot"
        );
    }

    #[test]
    fn test_rejected_tick_preserves_the_real_slot_for_a_known_instrument() {
        // §28.3: the evaluator indexes `states` by snapshot.slot. A refusal
        // must not hand back slot 0 for an instrument that owns slot 1.
        let mut engine = default_engine();
        engine.update(&make_tick(11, 100.0, 105.0, 95.0, 1000));
        let second = engine.update(&make_tick(22, 100.0, 105.0, 95.0, 1000));
        assert_eq!(second.slot, 1, "precondition: id 22 owns dense slot 1");

        let snap = engine.update(&make_tick(22, f32::NAN, 105.0, 95.0, 1000));
        assert_eq!(
            snap.slot, 1,
            "refusal must carry the instrument's real slot"
        );
        assert!(!snap.is_warm, "and still be fail-closed");
    }

    // --- the headline regression: survive poison, keep computing ---

    #[test]
    fn test_prior_good_state_survives_poison_and_next_good_tick_computes() {
        // The exact scenario the bug produced: a warmed-up instrument takes one
        // NaN packet mid-session. Before the fix, atr/adx/vwap were absorbed to
        // NaN forever and every later snapshot was masked to plausible 0.0.
        let mut engine = default_engine();
        for i in 0..40_u32 {
            let p = 100.0 + i as f32;
            engine.update(&make_tick(7, p, p + 5.0, p - 5.0, 1000));
        }
        let clean = engine.update(&make_tick(7, 141.0, 146.0, 136.0, 1000));
        assert!(clean.is_warm, "precondition: warm");
        assert!(clean.atr > 0.0 && clean.adx > 0.0 && clean.vwap > 0.0);

        let (atr_before, adx_before, vwap_before) = (clean.atr, clean.adx, clean.vwap);

        // The poison burst: every shape at once.
        for bad in [
            make_tick(7, 142.0, f32::NAN, 137.0, 1000),
            make_tick(7, 142.0, 147.0, f32::NAN, 1000),
            make_tick(7, f32::NAN, 147.0, 137.0, 1000),
            make_tick(7, f32::INFINITY, 147.0, 137.0, 1000),
            make_tick(7, -1.0, 147.0, 137.0, 1000),
            make_tick(7, 0.0, 147.0, 137.0, 1000),
        ] {
            let snap = engine.update(&bad);
            assert!(!snap.is_warm, "every poison shape must be refused");
        }

        // State must be byte-identical to the last good tick.
        assert_state_finite(&mut engine, 7, "after poison burst");
        assert_eq!(engine.state_mut_for_test(7).atr, atr_before);
        assert_eq!(engine.state_mut_for_test(7).adx_value, adx_before);
        assert_eq!(
            engine.warmup_count(7),
            41,
            "refused ticks must not advance warmup"
        );

        // And the very next GOOD tick must keep computing normally.
        let resumed = engine.update(&make_tick(7, 143.0, 148.0, 138.0, 1000));
        assert!(resumed.is_warm, "instrument must still be warm");
        assert!(
            resumed.atr.is_finite() && resumed.atr > 0.0,
            "ATR must resume finite+positive, got {}",
            resumed.atr
        );
        assert!(resumed.adx.is_finite() && resumed.adx > 0.0);
        assert!(resumed.vwap.is_finite() && resumed.vwap > 0.0);
        assert!(resumed.ema_fast.is_finite() && resumed.ema_fast > 0.0);
        assert!(resumed.rsi.is_finite() && (0.0..=100.0).contains(&resumed.rsi));
        assert!(
            (resumed.atr - atr_before).abs() < atr_before,
            "ATR must evolve continuously from the pre-poison value, not restart"
        );
        assert!(
            resumed.vwap > vwap_before * 0.5 && resumed.vwap < vwap_before * 2.0,
            "VWAP accumulator must be continuous across the refusal"
        );
    }

    #[test]
    fn test_poison_on_the_very_first_tick_leaves_instrument_pristine() {
        // A refusal must not half-initialise an instrument (e.g. bump
        // warmup_count and then bail), which would make the NEXT tick look like
        // a second tick and skip first-tick EMA seeding.
        let mut engine = default_engine();
        assert!(
            !engine
                .update(&make_tick(9, 100.0, f32::NAN, 95.0, 1000))
                .is_warm
        );
        assert_eq!(engine.warmup_count(9), 0);

        let first_good = engine.update(&make_tick(9, 250.0, 255.0, 245.0, 1000));
        assert_eq!(
            first_good.ema_fast, 250.0,
            "the first GOOD tick must still seed EMA to price"
        );
        assert_eq!(engine.warmup_count(9), 1);
    }

    // --- warmup_from_candles gate ---

    #[test]
    fn test_warmup_rejects_nan_high_low_and_open() {
        // Pre-fix, only `close` was validated — a NaN high/low/open flowed
        // straight into the same Wilder accumulators.
        let mut engine = default_engine();
        let candles = vec![
            (100.0, 101.0, 99.0, 100.0, 1000.0),         // valid
            (100.0, f64::NAN, 99.0, 100.0, 1000.0),      // NaN high
            (100.0, 101.0, f64::NAN, 100.0, 1000.0),     // NaN low
            (f64::NAN, 101.0, 99.0, 100.0, 1000.0),      // NaN open
            (100.0, f64::INFINITY, 99.0, 100.0, 1000.0), // Inf high
            (102.0, 103.0, 101.0, 102.0, 2000.0),        // valid
        ];
        let processed = engine.warmup_from_candles(300, &candles);
        assert_eq!(processed, 2, "only the 2 clean candles may be processed");
        assert_state_finite(&mut engine, 300, "after warmup with NaN OHLC");
    }

    #[test]
    fn test_warmup_rejects_f64_to_f32_overflow_candle() {
        // 1e300 is a perfectly finite f64 — it passed the old
        // `close.is_finite()` check and then became +inf in `close as f32`.
        let mut engine = default_engine();
        assert!(1e300_f64.is_finite(), "premise: the f64 IS finite");
        assert!(
            !(1e300_f64 as f32).is_finite(),
            "premise: but it overflows f32"
        );

        let candles = vec![
            (100.0, 101.0, 99.0, 100.0, 1000.0),  // valid
            (1e300, 1e300, 1e300, 1e300, 1000.0), // finite f64, +inf as f32
            (100.0, 1e300, 99.0, 100.0, 1000.0),  // overflow in high only
            (102.0, 103.0, 101.0, 102.0, 2000.0), // valid
        ];
        let processed = engine.warmup_from_candles(301, &candles);
        assert_eq!(processed, 2, "f32-overflow candles must be refused");
        assert_state_finite(&mut engine, 301, "after f32-overflow warmup");
    }

    #[test]
    fn test_warmup_rejects_non_finite_and_negative_volume() {
        // `volume as u32` saturates NaN→0 and negative→0: silently wrong
        // rather than loud, so the gate refuses them instead.
        let mut engine = default_engine();
        let candles = vec![
            (100.0, 101.0, 99.0, 100.0, f64::NAN),
            (100.0, 101.0, 99.0, 100.0, -5.0),
            (100.0, 101.0, 99.0, 100.0, f64::INFINITY),
            (102.0, 103.0, 101.0, 102.0, 2000.0), // valid
        ];
        assert_eq!(engine.warmup_from_candles(302, &candles), 1);
    }

    #[test]
    fn test_warmup_rejects_negative_ohlc_and_keeps_legacy_close_rule() {
        let mut engine = default_engine();
        let candles = vec![
            (100.0, 101.0, 99.0, 0.0, 1000.0),    // close = 0 (legacy rule)
            (100.0, 101.0, 99.0, -100.0, 1000.0), // negative close
            (100.0, -101.0, 99.0, 100.0, 1000.0), // negative high
            (100.0, 101.0, -99.0, 100.0, 1000.0), // negative low
            (-100.0, 101.0, 99.0, 100.0, 1000.0), // negative open
            (102.0, 103.0, 101.0, 102.0, 2000.0), // valid
        ];
        assert_eq!(engine.warmup_from_candles(303, &candles), 1);
    }

    #[test]
    fn test_warmup_zero_high_low_candle_is_still_accepted() {
        // Same Ticker-sentinel carve-out as the live path.
        let mut engine = default_engine();
        let candles = vec![(0.0, 0.0, 0.0, 100.0, 0.0)];
        assert_eq!(
            engine.warmup_from_candles(304, &candles),
            1,
            "close>0 with absent high/low must still warm up"
        );
    }

    #[test]
    fn test_warmup_all_poison_processes_nothing_and_stays_cold() {
        let mut engine = default_engine();
        let candles = vec![
            (f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN),
            (1e300, 1e300, 1e300, 1e300, 1e300),
            (0.0, 0.0, 0.0, 0.0, 0.0),
        ];
        assert_eq!(engine.warmup_from_candles(305, &candles), 0);
        assert_eq!(engine.warmup_count(305), 0);
    }
}
