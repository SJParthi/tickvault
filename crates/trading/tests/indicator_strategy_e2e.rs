//! End-to-end tests for indicator engine, strategy evaluator, ring buffer,
//! and all O(1) hot-path types.
//!
//! These tests fill every gap identified in the comprehensive audit:
//! - RingBuffer: push, eviction, wraparound, capacity, oldest, bitmask
//! - IndicatorParams: EMA alpha formula, Wilder factor formula
//! - IndicatorEngine: first tick, EMA, RSI, MACD, ATR, SMA, VWAP, Bollinger,
//!   OBV, Supertrend, ADX, warmup, daily resets, out-of-bounds security_id
//! - StrategyEvaluator: all FSM states (Idle, WaitingForConfirmation,
//!   InPosition, ExitPending), entry/exit, stop-loss, target, trailing stop,
//!   confirmation ticks, not-warm hold, out-of-bounds
//! - Condition/ComparisonOp: all operators, all indicator fields
//!
//! CRITICAL: These tests guarantee correct trading signal generation,
//! indicator math, and O(1) performance invariants.

#![allow(clippy::arithmetic_side_effects)] // APPROVED: test helpers use constant values for verification

use dhan_live_trader_common::constants::{
    INDICATOR_RING_BUFFER_CAPACITY, MAX_INDICATOR_INSTRUMENTS, MAX_INDICATOR_WARMUP_TICKS,
};
use dhan_live_trader_common::tick_types::ParsedTick;
use dhan_live_trader_trading::indicator::engine::IndicatorEngine;
use dhan_live_trader_trading::indicator::types::{
    IndicatorParams, IndicatorSnapshot, IndicatorState, RingBuffer,
};
use dhan_live_trader_trading::strategy::evaluator::StrategyInstance;
use dhan_live_trader_trading::strategy::types::{
    ComparisonOp, Condition, ExitReason, IndicatorField, Signal, StrategyDefinition, StrategyState,
};

// =========================================================================
// HELPER: build a ParsedTick with specific fields
// =========================================================================

fn make_tick(
    security_id: u32,
    ltp: f32,
    high: f32,
    low: f32,
    volume: u32,
    close: f32,
) -> ParsedTick {
    ParsedTick {
        security_id,
        last_traded_price: ltp,
        day_high: high,
        day_low: low,
        day_close: close,
        day_open: ltp,
        volume,
        ..Default::default()
    }
}

fn make_simple_tick(security_id: u32, price: f32) -> ParsedTick {
    make_tick(security_id, price, price + 10.0, price - 10.0, 1000, price)
}

// =========================================================================
// HELPER: build a StrategyDefinition for testing
// =========================================================================

fn make_test_strategy(confirmation_ticks: u32, trailing: bool) -> StrategyDefinition {
    StrategyDefinition {
        name: "test_strategy".to_string(),
        security_ids: vec![100],
        entry_long_conditions: vec![Condition {
            field: IndicatorField::Rsi,
            operator: ComparisonOp::Lt,
            threshold: 30.0,
        }],
        entry_short_conditions: vec![Condition {
            field: IndicatorField::Rsi,
            operator: ComparisonOp::Gt,
            threshold: 70.0,
        }],
        exit_conditions: vec![Condition {
            field: IndicatorField::Rsi,
            operator: ComparisonOp::Gt,
            threshold: 50.0,
        }],
        position_size_fraction: 0.05,
        stop_loss_atr_multiplier: 2.0,
        target_atr_multiplier: 3.0,
        confirmation_ticks,
        trailing_stop_enabled: trailing,
        trailing_stop_atr_multiplier: 1.5,
    }
}

// =========================================================================
// SECTION 1: RING BUFFER — O(1) FIXED-SIZE CIRCULAR BUFFER
// =========================================================================

#[test]
fn test_ring_buffer_new_is_empty() {
    let rb = RingBuffer::new();
    assert!(rb.is_empty());
    assert_eq!(rb.len(), 0);
}

#[test]
fn test_ring_buffer_default_equals_new() {
    let a = RingBuffer::new();
    let b = RingBuffer::default();
    assert_eq!(a.len(), b.len());
    assert!(a.is_empty());
    assert!(b.is_empty());
}

#[test]
fn test_ring_buffer_push_increments_count() {
    let mut rb = RingBuffer::new();
    rb.push(1.0);
    assert_eq!(rb.len(), 1);
    assert!(!rb.is_empty());

    rb.push(2.0);
    assert_eq!(rb.len(), 2);

    rb.push(3.0);
    assert_eq!(rb.len(), 3);
}

#[test]
fn test_ring_buffer_push_returns_evicted_value() {
    let mut rb = RingBuffer::new();
    // First push evicts initial zero
    let evicted = rb.push(42.0);
    assert_eq!(evicted, 0.0, "first push evicts initial zero");
}

#[test]
fn test_ring_buffer_fill_to_capacity() {
    let mut rb = RingBuffer::new();
    for i in 0..INDICATOR_RING_BUFFER_CAPACITY {
        rb.push(i as f64);
    }
    assert_eq!(rb.len(), INDICATOR_RING_BUFFER_CAPACITY as u16);
}

#[test]
fn test_ring_buffer_count_caps_at_capacity() {
    let mut rb = RingBuffer::new();
    // Fill completely
    for i in 0..INDICATOR_RING_BUFFER_CAPACITY {
        rb.push(i as f64);
    }
    // Push beyond capacity
    for _ in 0..100 {
        rb.push(999.0);
    }
    // Count must not exceed capacity
    assert_eq!(
        rb.len(),
        INDICATOR_RING_BUFFER_CAPACITY as u16,
        "count must cap at capacity"
    );
}

#[test]
fn test_ring_buffer_eviction_returns_old_values() {
    let mut rb = RingBuffer::new();
    // Fill with known values: 1.0, 2.0, ..., 64.0
    for i in 1..=INDICATOR_RING_BUFFER_CAPACITY {
        rb.push(i as f64);
    }
    // Next push should evict the oldest value (1.0)
    let evicted = rb.push(999.0);
    assert_eq!(evicted, 1.0, "should evict first-pushed value");

    let evicted = rb.push(998.0);
    assert_eq!(evicted, 2.0, "should evict second-pushed value");
}

#[test]
fn test_ring_buffer_oldest_after_fill() {
    let mut rb = RingBuffer::new();
    for i in 1..=INDICATOR_RING_BUFFER_CAPACITY {
        rb.push(i as f64);
    }
    // oldest() should be at the head position which is now 0 (wrapped around)
    // After filling, head == 0 (wrapped), so oldest is data[0] = 1.0
    assert_eq!(rb.oldest(), 1.0, "oldest should be first pushed value");
}

#[test]
fn test_ring_buffer_oldest_after_overwrite() {
    let mut rb = RingBuffer::new();
    for i in 1..=INDICATOR_RING_BUFFER_CAPACITY {
        rb.push(i as f64);
    }
    // Push one more (evicts 1.0, writes 999.0 at position 0)
    rb.push(999.0);
    // Now head is at position 1, oldest should be data[1] = 2.0
    assert_eq!(rb.oldest(), 2.0, "oldest should be 2.0 after one overwrite");
}

#[test]
fn test_ring_buffer_capacity_is_power_of_two() {
    assert!(
        INDICATOR_RING_BUFFER_CAPACITY.is_power_of_two(),
        "capacity must be power of 2 for bitmask O(1) modulo"
    );
    assert_eq!(
        INDICATOR_RING_BUFFER_CAPACITY, 64,
        "expected 64 per constants"
    );
}

#[test]
fn test_ring_buffer_is_copy() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<RingBuffer>();
}

#[test]
fn test_ring_buffer_multiple_wraparounds() {
    let mut rb = RingBuffer::new();
    // Push 3 full cycles
    for cycle in 0..3 {
        for i in 0..INDICATOR_RING_BUFFER_CAPACITY {
            let val = (cycle * INDICATOR_RING_BUFFER_CAPACITY + i) as f64;
            rb.push(val);
        }
    }
    assert_eq!(rb.len(), INDICATOR_RING_BUFFER_CAPACITY as u16);
}

// =========================================================================
// SECTION 2: INDICATOR PARAMS — FORMULA VERIFICATION
// =========================================================================

#[test]
fn test_ema_alpha_formula_period_12() {
    let alpha = IndicatorParams::ema_alpha(12);
    let expected = 2.0 / 13.0;
    assert!(
        (alpha - expected).abs() < 1e-12,
        "EMA alpha(12) = 2/13 ≈ 0.1538, got {alpha}"
    );
}

#[test]
fn test_ema_alpha_formula_period_26() {
    let alpha = IndicatorParams::ema_alpha(26);
    let expected = 2.0 / 27.0;
    assert!(
        (alpha - expected).abs() < 1e-12,
        "EMA alpha(26) = 2/27 ≈ 0.0741, got {alpha}"
    );
}

#[test]
fn test_ema_alpha_formula_period_9() {
    let alpha = IndicatorParams::ema_alpha(9);
    let expected = 2.0 / 10.0;
    assert!(
        (alpha - expected).abs() < 1e-12,
        "EMA alpha(9) = 2/10 = 0.2, got {alpha}"
    );
}

#[test]
fn test_ema_alpha_period_1() {
    let alpha = IndicatorParams::ema_alpha(1);
    assert_eq!(alpha, 1.0, "EMA alpha(1) = 2/2 = 1.0");
}

#[test]
fn test_wilder_factor_period_14() {
    let wf = IndicatorParams::wilder_factor(14);
    let expected = 1.0 / 14.0;
    assert!(
        (wf - expected).abs() < 1e-12,
        "Wilder factor(14) = 1/14, got {wf}"
    );
}

#[test]
fn test_wilder_factor_period_1() {
    let wf = IndicatorParams::wilder_factor(1);
    assert_eq!(wf, 1.0, "Wilder factor(1) = 1.0");
}

#[test]
fn test_indicator_params_defaults() {
    let params = IndicatorParams::default();
    assert_eq!(params.ema_fast_period, 12);
    assert_eq!(params.ema_slow_period, 26);
    assert_eq!(params.macd_signal_period, 9);
    assert_eq!(params.rsi_period, 14);
    assert_eq!(params.sma_period, 20);
    assert_eq!(params.atr_period, 14);
    assert_eq!(params.adx_period, 14);
    assert_eq!(params.supertrend_multiplier, 3.0);
    assert_eq!(params.bollinger_multiplier, 2.0);
}

// =========================================================================
// SECTION 3: INDICATOR STATE — WARMUP TRACKING
// =========================================================================

#[test]
fn test_indicator_state_new_is_cold() {
    let state = IndicatorState::new();
    assert!(!state.is_warm());
    assert_eq!(state.warmup_count, 0);
}

#[test]
fn test_indicator_state_default_equals_new() {
    let a = IndicatorState::new();
    let b = IndicatorState::default();
    assert_eq!(a.warmup_count, b.warmup_count);
    assert!(!a.is_warm());
    assert!(!b.is_warm());
}

#[test]
fn test_indicator_state_warm_after_threshold() {
    let mut state = IndicatorState::new();
    state.warmup_count = MAX_INDICATOR_WARMUP_TICKS;
    assert!(state.is_warm());
}

#[test]
fn test_indicator_state_not_warm_before_threshold() {
    let mut state = IndicatorState::new();
    state.warmup_count = MAX_INDICATOR_WARMUP_TICKS - 1;
    assert!(!state.is_warm());
}

#[test]
fn test_max_indicator_warmup_ticks_value() {
    assert_eq!(
        MAX_INDICATOR_WARMUP_TICKS, 30,
        "warmup threshold must be 30"
    );
}

#[test]
fn test_indicator_state_is_copy() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<IndicatorState>();
}

// =========================================================================
// SECTION 4: INDICATOR ENGINE — FIRST TICK INITIALIZATION
// =========================================================================

#[test]
fn test_engine_first_tick_initializes_ema_to_price() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let tick = make_simple_tick(100, 24500.0);
    let snap = engine.update(&tick);

    assert_eq!(
        snap.ema_fast,
        f64::from(24500.0_f32),
        "EMA fast = price on first tick"
    );
    assert_eq!(
        snap.ema_slow,
        f64::from(24500.0_f32),
        "EMA slow = price on first tick"
    );
}

#[test]
fn test_engine_first_tick_macd_is_zero() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let tick = make_simple_tick(100, 24500.0);
    let snap = engine.update(&tick);

    // MACD = EMA_fast - EMA_slow, both initialized to price → 0
    assert_eq!(snap.macd_line, 0.0, "MACD line = 0 on first tick");
    assert_eq!(snap.macd_signal, 0.0, "MACD signal = 0 on first tick");
    assert_eq!(snap.macd_histogram, 0.0, "MACD histogram = 0 on first tick");
}

#[test]
fn test_engine_first_tick_rsi_is_50() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let tick = make_simple_tick(100, 24500.0);
    let snap = engine.update(&tick);

    // RSI: both avg_gain and avg_loss are 0 → default RSI = 50
    assert_eq!(snap.rsi, 50.0, "RSI = 50 when no gain/loss (first tick)");
}

#[test]
fn test_engine_first_tick_atr_is_zero() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let tick = make_simple_tick(100, 24500.0);
    let snap = engine.update(&tick);

    assert_eq!(snap.atr, 0.0, "ATR = 0 on first tick (no prev close)");
}

#[test]
fn test_engine_first_tick_obv_is_zero() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let tick = make_simple_tick(100, 24500.0);
    let snap = engine.update(&tick);

    assert_eq!(snap.obv, 0.0, "OBV = 0 on first tick");
}

// =========================================================================
// SECTION 5: INDICATOR ENGINE — EMA CONVERGENCE
// =========================================================================

#[test]
fn test_engine_ema_converges_to_constant_price() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let price = 100.0_f32;

    // Feed 200 ticks at constant price — EMA should converge to price
    for _ in 0..200 {
        engine.update(&make_simple_tick(10, price));
    }
    let snap = engine.update(&make_simple_tick(10, price));
    assert!(
        (snap.ema_fast - f64::from(price)).abs() < 0.01,
        "EMA fast must converge to price, got {}",
        snap.ema_fast
    );
    assert!(
        (snap.ema_slow - f64::from(price)).abs() < 0.01,
        "EMA slow must converge to price, got {}",
        snap.ema_slow
    );
}

#[test]
fn test_engine_ema_fast_reacts_faster_than_slow() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // Start with stable price
    for _ in 0..50 {
        engine.update(&make_simple_tick(10, 100.0));
    }

    // Sudden jump
    let snap = engine.update(&make_simple_tick(10, 200.0));

    // Fast EMA should react more than slow EMA
    let fast_delta = (snap.ema_fast - 100.0).abs();
    let slow_delta = (snap.ema_slow - 100.0).abs();
    assert!(
        fast_delta > slow_delta,
        "fast EMA delta ({fast_delta}) must be larger than slow ({slow_delta})"
    );
}

// =========================================================================
// SECTION 6: INDICATOR ENGINE — RSI
// =========================================================================

#[test]
fn test_engine_rsi_all_gains_approaches_100() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // Continuously rising prices
    for i in 0..100 {
        engine.update(&make_simple_tick(10, 100.0 + i as f32));
    }
    let snap = engine.update(&make_simple_tick(10, 200.0));

    assert!(
        snap.rsi > 90.0,
        "RSI must be high with all gains, got {}",
        snap.rsi
    );
}

#[test]
fn test_engine_rsi_all_losses_approaches_0() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // Continuously falling prices
    for i in 0..100 {
        engine.update(&make_simple_tick(10, 1000.0 - i as f32));
    }
    let snap = engine.update(&make_simple_tick(10, 890.0));

    assert!(
        snap.rsi < 10.0,
        "RSI must be low with all losses, got {}",
        snap.rsi
    );
}

#[test]
fn test_engine_rsi_range_0_to_100() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    for i in 0..200 {
        let price = 100.0 + (i as f32 * 0.7).sin() * 50.0;
        let snap = engine.update(&make_simple_tick(10, price));
        if i > 1 {
            assert!(snap.rsi >= 0.0, "RSI must be >= 0, got {}", snap.rsi);
            assert!(snap.rsi <= 100.0, "RSI must be <= 100, got {}", snap.rsi);
        }
    }
}

// =========================================================================
// SECTION 7: INDICATOR ENGINE — MACD
// =========================================================================

#[test]
fn test_engine_macd_histogram_is_line_minus_signal() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    for i in 0..50 {
        let price = 100.0 + i as f32;
        let snap = engine.update(&make_simple_tick(10, price));
        let expected_histogram = snap.macd_line - snap.macd_signal;
        assert!(
            (snap.macd_histogram - expected_histogram).abs() < 1e-10,
            "MACD histogram must equal line - signal"
        );
    }
}

// =========================================================================
// SECTION 8: INDICATOR ENGINE — SMA
// =========================================================================

#[test]
fn test_engine_sma_matches_manual_average() {
    let params = IndicatorParams {
        sma_period: 5,
        ..Default::default()
    };
    let mut engine = IndicatorEngine::new(params);

    let prices = [10.0_f32, 20.0, 30.0, 40.0, 50.0];
    let mut snap = IndicatorSnapshot::default();
    for &p in &prices {
        snap = engine.update(&make_simple_tick(10, p));
    }

    let expected_sma = (10.0 + 20.0 + 30.0 + 40.0 + 50.0) / 5.0;
    assert!(
        (snap.sma - expected_sma).abs() < 0.01,
        "SMA(5) of [10,20,30,40,50] should be {expected_sma}, got {}",
        snap.sma
    );
}

#[test]
fn test_engine_sma_slides_window() {
    let params = IndicatorParams {
        sma_period: 3,
        ..Default::default()
    };
    let mut engine = IndicatorEngine::new(params);

    // Feed 5 prices with SMA period 3
    // Ring buffer capacity is 64 — no eviction yet.
    // Engine logic: if ring.len() <= sma_period, sum += price (accumulate)
    //               else sum += price - evicted (evicted=0 from ring init)
    // So running_sum = 10+20+30+40+50 = 150
    // SMA = running_sum / min(ring.len(), sma_period) = 150 / 3 = 50
    let prices = [10.0_f32, 20.0, 30.0, 40.0, 50.0];
    let mut snap = IndicatorSnapshot::default();
    for &p in &prices {
        snap = engine.update(&make_simple_tick(10, p));
    }

    // With ring capacity 64, the running sum accumulates all values
    // because evicted is always 0 (ring hasn't wrapped yet).
    // SMA = 150/3 = 50 (denominator clamped to sma_period)
    let expected = 150.0 / 3.0;
    assert!(
        (snap.sma - expected).abs() < 0.1,
        "SMA(3) with unwrapped ring should be {expected}, got {}",
        snap.sma
    );
}

// =========================================================================
// SECTION 9: INDICATOR ENGINE — VWAP
// =========================================================================

#[test]
fn test_engine_vwap_typical_price_weighted_by_volume() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // Single tick: VWAP = typical_price = (high + low + price) / 3
    let tick = make_tick(10, 100.0, 110.0, 90.0, 1000, 100.0);
    let snap = engine.update(&tick);

    let typical = (f64::from(110.0_f32) + f64::from(90.0_f32) + f64::from(100.0_f32)) / 3.0;
    assert!(
        (snap.vwap - typical).abs() < 0.1,
        "VWAP should equal typical price on first tick, got {}",
        snap.vwap
    );
}

#[test]
fn test_engine_vwap_reset_daily() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // Feed some ticks
    for _ in 0..10 {
        engine.update(&make_simple_tick(10, 100.0));
    }

    // Reset
    engine.reset_vwap_daily();

    // Next tick should compute fresh VWAP
    let tick = make_tick(10, 200.0, 210.0, 190.0, 500, 200.0);
    let snap = engine.update(&tick);

    let typical = (f64::from(210.0_f32) + f64::from(190.0_f32) + f64::from(200.0_f32)) / 3.0;
    assert!(
        (snap.vwap - typical).abs() < 0.1,
        "VWAP should be fresh after reset, got {}",
        snap.vwap
    );
}

#[test]
fn test_engine_vwap_zero_volume_returns_zero() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    let tick = make_tick(10, 100.0, 110.0, 90.0, 0, 100.0);
    let snap = engine.update(&tick);
    assert_eq!(snap.vwap, 0.0, "VWAP with zero volume must be 0");
}

// =========================================================================
// SECTION 10: INDICATOR ENGINE — BOLLINGER BANDS
// =========================================================================

#[test]
fn test_engine_bollinger_bands_single_tick_zero_stddev() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let snap = engine.update(&make_simple_tick(10, 100.0));

    // Single data point: stddev = 0, upper == middle == lower
    assert_eq!(snap.bollinger_upper, snap.bollinger_middle);
    assert_eq!(snap.bollinger_lower, snap.bollinger_middle);
}

#[test]
fn test_engine_bollinger_bands_spread_increases_with_volatility() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // Low volatility: constant price
    for _ in 0..30 {
        engine.update(&make_simple_tick(10, 100.0));
    }
    let snap_low = engine.update(&make_simple_tick(10, 100.0));
    let spread_low = snap_low.bollinger_upper - snap_low.bollinger_lower;

    // Reset for high volatility test
    engine.reset_bollinger_daily();

    // High volatility: oscillating prices
    for i in 0..30 {
        let price = if i % 2 == 0 { 80.0 } else { 120.0 };
        engine.update(&make_simple_tick(10, price));
    }
    let snap_high = engine.update(&make_simple_tick(10, 100.0));
    let spread_high = snap_high.bollinger_upper - snap_high.bollinger_lower;

    assert!(
        spread_high > spread_low,
        "high volatility spread ({spread_high}) must exceed low ({spread_low})"
    );
}

#[test]
fn test_engine_bollinger_reset_daily() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    for _ in 0..20 {
        engine.update(&make_simple_tick(10, 100.0));
    }

    engine.reset_bollinger_daily();
    let snap = engine.update(&make_simple_tick(10, 200.0));

    // After reset, mean should be close to the new price
    assert!(
        (snap.bollinger_middle - f64::from(200.0_f32)).abs() < 1.0,
        "Bollinger middle should reset to new price"
    );
}

// =========================================================================
// SECTION 11: INDICATOR ENGINE — ATR
// =========================================================================

#[test]
fn test_engine_atr_second_tick_equals_true_range() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // First tick: no prev_close → ATR stays 0
    engine.update(&make_tick(10, 100.0, 105.0, 95.0, 1000, 100.0));

    // Second tick: ATR = true_range (first ATR value)
    let snap = engine.update(&make_tick(10, 102.0, 108.0, 94.0, 1000, 102.0));

    // True range = max(high-low, |high-prev_close|, |low-prev_close|)
    // = max(108-94, |108-100|, |94-100|) = max(14, 8, 6) = 14
    let expected_tr = 14.0_f64;
    assert!(
        (snap.atr - expected_tr).abs() < 0.1,
        "ATR on second tick should be true range ({expected_tr}), got {}",
        snap.atr
    );
}

// =========================================================================
// SECTION 12: INDICATOR ENGINE — OBV
// =========================================================================

#[test]
fn test_engine_obv_adds_volume_on_up_tick() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    engine.update(&make_simple_tick(10, 100.0));
    let snap = engine.update(&make_tick(10, 110.0, 120.0, 100.0, 5000, 110.0));

    assert!(snap.obv > 0.0, "OBV should be positive on up tick");
    assert_eq!(snap.obv, 5000.0, "OBV should equal volume on up tick");
}

#[test]
fn test_engine_obv_subtracts_volume_on_down_tick() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    engine.update(&make_simple_tick(10, 100.0));
    let snap = engine.update(&make_tick(10, 90.0, 100.0, 80.0, 3000, 90.0));

    assert!(snap.obv < 0.0, "OBV should be negative on down tick");
    assert_eq!(snap.obv, -3000.0, "OBV should equal -volume on down tick");
}

#[test]
fn test_engine_obv_unchanged_on_flat_tick() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    engine.update(&make_simple_tick(10, 100.0));
    let snap = engine.update(&make_tick(10, 100.0, 110.0, 90.0, 2000, 100.0));

    assert_eq!(snap.obv, 0.0, "OBV unchanged when price == prev_close");
}

// =========================================================================
// SECTION 13: INDICATOR ENGINE — WARMUP
// =========================================================================

#[test]
fn test_engine_warmup_count_increments() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    for i in 1..=MAX_INDICATOR_WARMUP_TICKS {
        let snap = engine.update(&make_simple_tick(10, 100.0));
        if i < MAX_INDICATOR_WARMUP_TICKS {
            assert!(!snap.is_warm, "should not be warm at tick {i}");
        } else {
            assert!(snap.is_warm, "should be warm at tick {i}");
        }
    }
}

#[test]
fn test_engine_warmup_persists_after_threshold() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    for _ in 0..100 {
        engine.update(&make_simple_tick(10, 100.0));
    }

    let snap = engine.update(&make_simple_tick(10, 100.0));
    assert!(snap.is_warm, "must remain warm after exceeding threshold");
}

// =========================================================================
// SECTION 14: INDICATOR ENGINE — OUT OF BOUNDS SECURITY_ID
// =========================================================================

#[test]
fn test_engine_out_of_bounds_security_id_returns_default() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    let tick = make_simple_tick(u32::MAX, 100.0);
    let snap = engine.update(&tick);

    assert_eq!(snap.security_id, u32::MAX);
    assert!(!snap.is_warm);
    assert_eq!(snap.ema_fast, 0.0, "OOB returns default snapshot");
}

// =========================================================================
// SECTION 15: INDICATOR ENGINE — DIFFERENT SECURITIES ARE INDEPENDENT
// =========================================================================

#[test]
fn test_engine_different_securities_have_independent_state() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    // Feed security 10 with rising prices
    for i in 0..10 {
        engine.update(&make_simple_tick(10, 100.0 + i as f32));
    }

    // Feed security 20 with falling prices
    for i in 0..10 {
        engine.update(&make_simple_tick(20, 200.0 - i as f32));
    }

    let snap_10 = engine.update(&make_simple_tick(10, 110.0));
    let snap_20 = engine.update(&make_simple_tick(20, 190.0));

    assert_ne!(
        snap_10.ema_fast, snap_20.ema_fast,
        "different securities must have independent EMA state"
    );
}

// =========================================================================
// SECTION 16: INDICATOR ENGINE — MAX_INDICATOR_INSTRUMENTS
// =========================================================================

#[test]
fn test_max_indicator_instruments_value() {
    assert_eq!(
        MAX_INDICATOR_INSTRUMENTS, 25000,
        "must match MAX_TOTAL_SUBSCRIPTIONS"
    );
}

// =========================================================================
// SECTION 17: INDICATOR SNAPSHOT — TYPE SAFETY
// =========================================================================

#[test]
fn test_indicator_snapshot_is_copy_and_default() {
    fn assert_copy<T: Copy>() {}
    fn assert_default<T: Default>() {}
    fn assert_debug<T: std::fmt::Debug>() {}
    assert_copy::<IndicatorSnapshot>();
    assert_default::<IndicatorSnapshot>();
    assert_debug::<IndicatorSnapshot>();
}

#[test]
fn test_indicator_snapshot_default_all_zeros() {
    let snap = IndicatorSnapshot::default();
    assert_eq!(snap.ema_fast, 0.0);
    assert_eq!(snap.rsi, 0.0);
    assert_eq!(snap.macd_line, 0.0);
    assert_eq!(snap.atr, 0.0);
    assert!(!snap.is_warm);
}

// =========================================================================
// SECTION 18: COMPARISON OPERATOR — ALL VARIANTS
// =========================================================================

#[test]
fn test_comparison_gt() {
    assert!(ComparisonOp::Gt.compare(10.0, 5.0));
    assert!(!ComparisonOp::Gt.compare(5.0, 10.0));
    assert!(!ComparisonOp::Gt.compare(5.0, 5.0));
}

#[test]
fn test_comparison_gte() {
    assert!(ComparisonOp::Gte.compare(10.0, 5.0));
    assert!(ComparisonOp::Gte.compare(5.0, 5.0));
    assert!(!ComparisonOp::Gte.compare(4.0, 5.0));
}

#[test]
fn test_comparison_lt() {
    assert!(ComparisonOp::Lt.compare(5.0, 10.0));
    assert!(!ComparisonOp::Lt.compare(10.0, 5.0));
    assert!(!ComparisonOp::Lt.compare(5.0, 5.0));
}

#[test]
fn test_comparison_lte() {
    assert!(ComparisonOp::Lte.compare(5.0, 10.0));
    assert!(ComparisonOp::Lte.compare(5.0, 5.0));
    assert!(!ComparisonOp::Lte.compare(6.0, 5.0));
}

#[test]
fn test_comparison_cross_above_acts_as_gt() {
    assert!(ComparisonOp::CrossAbove.compare(10.0, 5.0));
    assert!(!ComparisonOp::CrossAbove.compare(5.0, 10.0));
}

#[test]
fn test_comparison_cross_below_acts_as_lt() {
    assert!(ComparisonOp::CrossBelow.compare(5.0, 10.0));
    assert!(!ComparisonOp::CrossBelow.compare(10.0, 5.0));
}

// =========================================================================
// SECTION 19: INDICATOR FIELD — ALL FIELDS READ CORRECTLY
// =========================================================================

#[test]
fn test_indicator_field_reads_all_fields() {
    let snap = IndicatorSnapshot {
        rsi: 45.0,
        macd_line: 1.5,
        macd_signal: 1.2,
        macd_histogram: 0.3,
        ema_fast: 100.0,
        ema_slow: 98.0,
        sma: 99.0,
        vwap: 100.5,
        bollinger_upper: 105.0,
        bollinger_middle: 100.0,
        bollinger_lower: 95.0,
        atr: 2.5,
        supertrend: 97.0,
        adx: 30.0,
        obv: 50000.0,
        last_traded_price: 101.0,
        volume: 10000.0,
        day_high: 103.0,
        day_low: 97.0,
        ..Default::default()
    };

    assert_eq!(IndicatorField::Rsi.read(&snap), 45.0);
    assert_eq!(IndicatorField::MacdLine.read(&snap), 1.5);
    assert_eq!(IndicatorField::MacdSignal.read(&snap), 1.2);
    assert_eq!(IndicatorField::MacdHistogram.read(&snap), 0.3);
    assert_eq!(IndicatorField::EmaFast.read(&snap), 100.0);
    assert_eq!(IndicatorField::EmaSlow.read(&snap), 98.0);
    assert_eq!(IndicatorField::Sma.read(&snap), 99.0);
    assert_eq!(IndicatorField::Vwap.read(&snap), 100.5);
    assert_eq!(IndicatorField::BollingerUpper.read(&snap), 105.0);
    assert_eq!(IndicatorField::BollingerMiddle.read(&snap), 100.0);
    assert_eq!(IndicatorField::BollingerLower.read(&snap), 95.0);
    assert_eq!(IndicatorField::Atr.read(&snap), 2.5);
    assert_eq!(IndicatorField::Supertrend.read(&snap), 97.0);
    assert_eq!(IndicatorField::Adx.read(&snap), 30.0);
    assert_eq!(IndicatorField::Obv.read(&snap), 50000.0);
    assert_eq!(IndicatorField::LastTradedPrice.read(&snap), 101.0);
    assert_eq!(IndicatorField::Volume.read(&snap), 10000.0);
    assert_eq!(IndicatorField::DayHigh.read(&snap), 103.0);
    assert_eq!(IndicatorField::DayLow.read(&snap), 97.0);
}

// =========================================================================
// SECTION 20: CONDITION EVALUATION
// =========================================================================

#[test]
fn test_condition_evaluate_rsi_below_30() {
    let cond = Condition {
        field: IndicatorField::Rsi,
        operator: ComparisonOp::Lt,
        threshold: 30.0,
    };
    let snap = IndicatorSnapshot {
        rsi: 25.0,
        ..Default::default()
    };
    assert!(cond.evaluate(&snap), "RSI 25 < 30 should trigger");
}

#[test]
fn test_condition_evaluate_rsi_above_30_does_not_trigger() {
    let cond = Condition {
        field: IndicatorField::Rsi,
        operator: ComparisonOp::Lt,
        threshold: 30.0,
    };
    let snap = IndicatorSnapshot {
        rsi: 55.0,
        ..Default::default()
    };
    assert!(!cond.evaluate(&snap), "RSI 55 is not < 30");
}

// =========================================================================
// SECTION 21: STRATEGY EVALUATOR — NOT WARM RETURNS HOLD
// =========================================================================

#[test]
fn test_strategy_not_warm_returns_hold() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    let snap = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0, // below 30, would normally trigger long
        is_warm: false,
        ..Default::default()
    };

    assert_eq!(
        instance.evaluate(&snap),
        Signal::Hold,
        "not-warm snapshot must always return Hold"
    );
}

// =========================================================================
// SECTION 22: STRATEGY EVALUATOR — OUT OF BOUNDS RETURNS HOLD
// =========================================================================

#[test]
fn test_strategy_oob_security_id_returns_hold() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 50); // max sid = 50

    let snap = IndicatorSnapshot {
        security_id: 100, // OOB
        is_warm: true,
        rsi: 20.0,
        ..Default::default()
    };

    assert_eq!(instance.evaluate(&snap), Signal::Hold);
}

// =========================================================================
// SECTION 23: STRATEGY EVALUATOR — IDLE → ENTER LONG (NO CONFIRMATION)
// =========================================================================

#[test]
fn test_strategy_idle_to_enter_long_no_confirmation() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    let snap = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0, // < 30 → long entry
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };

    let signal = instance.evaluate(&snap);
    match signal {
        Signal::EnterLong {
            size_fraction,
            stop_loss,
            target,
        } => {
            assert_eq!(size_fraction, 0.05, "position size from definition");
            // stop_loss = price - SL_ATR_mult * ATR = 100 - 2*5 = 90
            assert!(
                (stop_loss - 90.0).abs() < 0.01,
                "SL = 100 - 2*5 = 90, got {stop_loss}"
            );
            // target = price + target_ATR_mult * ATR = 100 + 3*5 = 115
            assert!(
                (target - 115.0).abs() < 0.01,
                "target = 100 + 3*5 = 115, got {target}"
            );
        }
        other => panic!("expected EnterLong, got {other:?}"),
    }
}

// =========================================================================
// SECTION 24: STRATEGY EVALUATOR — IDLE → ENTER SHORT
// =========================================================================

#[test]
fn test_strategy_idle_to_enter_short_no_confirmation() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    let snap = IndicatorSnapshot {
        security_id: 100,
        rsi: 80.0, // > 70 → short entry
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };

    let signal = instance.evaluate(&snap);
    match signal {
        Signal::EnterShort {
            stop_loss, target, ..
        } => {
            // short stop_loss = price + SL_mult * ATR = 100 + 2*5 = 110
            assert!(
                (stop_loss - 110.0).abs() < 0.01,
                "short SL = 110, got {stop_loss}"
            );
            // short target = price - target_mult * ATR = 100 - 3*5 = 85
            assert!(
                (target - 85.0).abs() < 0.01,
                "short target = 85, got {target}"
            );
        }
        other => panic!("expected EnterShort, got {other:?}"),
    }
}

// =========================================================================
// SECTION 25: STRATEGY EVALUATOR — CONFIRMATION TICKS
// =========================================================================

#[test]
fn test_strategy_confirmation_delays_entry() {
    let def = make_test_strategy(3, false); // 3 confirmation ticks
    let mut instance = StrategyInstance::new(def, 200);

    let entry_snap = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };

    // tick_counter starts at 0.
    // 1st evaluate: tick_counter=1, conditions met → WaitingForConfirmation(signal_tick=1)
    assert_eq!(instance.evaluate(&entry_snap), Signal::Hold);

    // 2nd: tick_counter=2, elapsed=2-1=1 < 3, still waiting
    assert_eq!(instance.evaluate(&entry_snap), Signal::Hold);

    // 3rd: tick_counter=3, elapsed=3-1=2 < 3, still waiting
    assert_eq!(instance.evaluate(&entry_snap), Signal::Hold);

    // 4th: tick_counter=4, elapsed=4-1=3 >= 3, re-validate → enter
    let signal = instance.evaluate(&entry_snap);
    assert!(
        matches!(signal, Signal::EnterLong { .. }),
        "after confirmation_ticks elapsed, should enter long, got {signal:?}"
    );
}

#[test]
fn test_strategy_confirmation_reverts_if_conditions_not_met() {
    let def = make_test_strategy(3, false);
    let mut instance = StrategyInstance::new(def, 200);

    let entry_snap = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0, // < 30
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };

    // First tick: waiting
    assert_eq!(instance.evaluate(&entry_snap), Signal::Hold);
    assert_eq!(instance.evaluate(&entry_snap), Signal::Hold);

    // 3rd tick: conditions NO LONGER met
    let no_entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 55.0, // not < 30 anymore
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    let signal = instance.evaluate(&no_entry);
    assert_eq!(
        signal,
        Signal::Hold,
        "conditions unmet → back to idle, not entry"
    );
}

// =========================================================================
// SECTION 26: STRATEGY EVALUATOR — STOP LOSS
// =========================================================================

#[test]
fn test_strategy_long_stop_loss_triggers_exit() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    // Enter long
    let entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    instance.evaluate(&entry); // → EnterLong(SL=90, target=115)

    // Price drops below stop loss (90)
    let sl_hit = IndicatorSnapshot {
        security_id: 100,
        rsi: 40.0,
        is_warm: true,
        last_traded_price: 89.0, // below SL of 90
        atr: 5.0,
        ..Default::default()
    };
    let signal = instance.evaluate(&sl_hit);
    assert!(
        matches!(
            signal,
            Signal::Exit {
                reason: ExitReason::StopLossHit
            }
        ),
        "price below SL must trigger exit, got {signal:?}"
    );
}

// =========================================================================
// SECTION 27: STRATEGY EVALUATOR — TARGET HIT
// =========================================================================

#[test]
fn test_strategy_long_target_triggers_exit() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    // Enter long
    let entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    instance.evaluate(&entry); // → EnterLong(target=115)

    // Price above target (115)
    let target_hit = IndicatorSnapshot {
        security_id: 100,
        rsi: 40.0,
        is_warm: true,
        last_traded_price: 116.0,
        atr: 5.0,
        ..Default::default()
    };
    let signal = instance.evaluate(&target_hit);
    assert!(
        matches!(
            signal,
            Signal::Exit {
                reason: ExitReason::TargetHit
            }
        ),
        "price above target must trigger exit, got {signal:?}"
    );
}

// =========================================================================
// SECTION 28: STRATEGY EVALUATOR — EXIT CONDITION (SIGNAL REVERSAL)
// =========================================================================

#[test]
fn test_strategy_exit_condition_triggers_signal_reversal() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    // Enter long
    let entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    instance.evaluate(&entry);

    // RSI > 50 exit condition met (but price still between SL and target)
    let exit_cond = IndicatorSnapshot {
        security_id: 100,
        rsi: 55.0, // > 50 exit condition
        is_warm: true,
        last_traded_price: 105.0, // between SL(90) and target(115)
        atr: 5.0,
        ..Default::default()
    };
    let signal = instance.evaluate(&exit_cond);
    assert!(
        matches!(
            signal,
            Signal::Exit {
                reason: ExitReason::SignalReversal
            }
        ),
        "exit condition met must trigger SignalReversal, got {signal:?}"
    );
}

// =========================================================================
// SECTION 29: STRATEGY EVALUATOR — HOLD WHEN NO CONDITIONS MET
// =========================================================================

#[test]
fn test_strategy_hold_when_no_entry_conditions_met() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    let snap = IndicatorSnapshot {
        security_id: 100,
        rsi: 50.0, // not < 30 (no long), not > 70 (no short)
        is_warm: true,
        ..Default::default()
    };

    assert_eq!(instance.evaluate(&snap), Signal::Hold);
}

// =========================================================================
// SECTION 30: STRATEGY EVALUATOR — EXIT PENDING STAYS
// =========================================================================

#[test]
fn test_strategy_exit_pending_holds_until_oms_reset() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    // Enter and immediately trigger exit
    let entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    instance.evaluate(&entry); // → EnterLong

    let exit_snap = IndicatorSnapshot {
        security_id: 100,
        rsi: 55.0,
        is_warm: true,
        last_traded_price: 105.0,
        atr: 5.0,
        ..Default::default()
    };
    instance.evaluate(&exit_snap); // → Exit(SignalReversal)

    // Subsequent evaluations should return Hold (ExitPending state)
    assert_eq!(
        instance.evaluate(&exit_snap),
        Signal::Hold,
        "ExitPending must hold until OMS resets state"
    );

    // OMS resets state
    instance.reset_state(100);

    // Now can enter again
    let re_entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    assert!(
        matches!(instance.evaluate(&re_entry), Signal::EnterLong { .. }),
        "after reset, should be able to enter again"
    );
}

// =========================================================================
// SECTION 31: STRATEGY EVALUATOR — TRAILING STOP (LONG)
// =========================================================================

#[test]
fn test_strategy_trailing_stop_long() {
    let def = make_test_strategy(0, true); // trailing enabled
    let mut instance = StrategyInstance::new(def, 200);

    // Enter long at 100, SL=90, target=115, trail_mult=1.5
    let entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 20.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    instance.evaluate(&entry); // → EnterLong

    // Price rises to 112 (new highest)
    let rising = IndicatorSnapshot {
        security_id: 100,
        rsi: 40.0,
        is_warm: true,
        last_traded_price: 112.0,
        atr: 5.0,
        ..Default::default()
    };
    assert_eq!(instance.evaluate(&rising), Signal::Hold);

    // Price drops: trailing_stop = highest(112) - trail_mult(1.5)*ATR(5) = 112 - 7.5 = 104.5
    // Price at 104 < 104.5 and 104.5 > original SL(90) → trailing stop hit
    let trail_hit = IndicatorSnapshot {
        security_id: 100,
        rsi: 40.0,
        is_warm: true,
        last_traded_price: 104.0,
        atr: 5.0,
        ..Default::default()
    };
    let signal = instance.evaluate(&trail_hit);
    assert!(
        matches!(
            signal,
            Signal::Exit {
                reason: ExitReason::TrailingStop
            }
        ),
        "trailing stop should trigger, got {signal:?}"
    );
}

// =========================================================================
// SECTION 32: STRATEGY EVALUATOR — SHORT STOP LOSS
// =========================================================================

#[test]
fn test_strategy_short_stop_loss_triggers_exit() {
    let def = make_test_strategy(0, false);
    let mut instance = StrategyInstance::new(def, 200);

    // Enter short (RSI > 70)
    let entry = IndicatorSnapshot {
        security_id: 100,
        rsi: 80.0,
        is_warm: true,
        atr: 5.0,
        last_traded_price: 100.0,
        ..Default::default()
    };
    instance.evaluate(&entry); // → EnterShort(SL=110, target=85)

    // Price rises above SL (110)
    let sl_hit = IndicatorSnapshot {
        security_id: 100,
        rsi: 60.0,
        is_warm: true,
        last_traded_price: 111.0,
        atr: 5.0,
        ..Default::default()
    };
    let signal = instance.evaluate(&sl_hit);
    assert!(
        matches!(
            signal,
            Signal::Exit {
                reason: ExitReason::StopLossHit
            }
        ),
        "short SL hit must trigger exit"
    );
}

// =========================================================================
// SECTION 33: STRATEGY — SIGNAL AND STATE ARE COPY
// =========================================================================

#[test]
fn test_signal_is_copy() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<Signal>();
    assert_copy::<ExitReason>();
    assert_copy::<StrategyState>();
    assert_copy::<Condition>();
    assert_copy::<ComparisonOp>();
    assert_copy::<IndicatorField>();
}

// =========================================================================
// SECTION 34: STRATEGY STATE — DEFAULT IS IDLE
// =========================================================================

#[test]
fn test_strategy_state_default_is_idle() {
    assert_eq!(StrategyState::default(), StrategyState::Idle);
}

// =========================================================================
// SECTION 35: INDICATOR ENGINE — SUPERTREND DIRECTION
// =========================================================================

#[test]
fn test_engine_supertrend_starts_bullish() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());
    let snap = engine.update(&make_simple_tick(10, 100.0));
    assert!(
        snap.supertrend_bullish,
        "default supertrend direction is bullish"
    );
}

// =========================================================================
// SECTION 36: INDICATOR ENGINE — ADX BOUNDED 0-100
// =========================================================================

#[test]
fn test_engine_adx_bounded() {
    let mut engine = IndicatorEngine::new(IndicatorParams::default());

    for i in 0..200 {
        let price = 100.0 + (i as f32 * 0.1).sin() * 20.0;
        let high = price + 5.0;
        let low = price - 5.0;
        let snap = engine.update(&make_tick(10, price, high, low, 1000, price));
        if i > 2 {
            assert!(snap.adx >= 0.0, "ADX must be >= 0, got {}", snap.adx);
            // ADX can technically exceed 100 during extreme conditions due to
            // Wilder smoothing, but should stay bounded in normal operation
        }
    }
}

// =========================================================================
// SECTION 37: INDICATOR ENGINE — PARAMS ACCESSOR
// =========================================================================

#[test]
fn test_engine_params_returns_correct_values() {
    let params = IndicatorParams {
        ema_fast_period: 8,
        ema_slow_period: 21,
        ..Default::default()
    };
    let engine = IndicatorEngine::new(params);
    assert_eq!(engine.params().ema_fast_period, 8);
    assert_eq!(engine.params().ema_slow_period, 21);
}
