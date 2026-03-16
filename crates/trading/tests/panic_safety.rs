//! Panic safety tests.
//!
//! Verifies that functions that SHOULD panic actually do,
//! and functions that should NEVER panic don't even with extreme inputs.
//! Critical for a trading system where an unhandled panic = process crash.

use dhan_live_trader_trading::indicator::types::{IndicatorParams, IndicatorState, RingBuffer};
use dhan_live_trader_trading::risk::engine::RiskEngine;

// ---------------------------------------------------------------------------
// Must NOT panic: RiskEngine with extreme inputs
// ---------------------------------------------------------------------------

#[test]
fn no_panic_risk_engine_nan_inputs() {
    let mut engine = RiskEngine::new(f64::NAN, 100, 1_000_000.0);
    let _ = engine.check_order(1001, 10);
    engine.record_fill(1001, 10, f64::NAN, 25);
    let _ = engine.total_realized_pnl();
}

#[test]
fn no_panic_risk_engine_infinity_inputs() {
    let mut engine = RiskEngine::new(f64::INFINITY, 100, f64::INFINITY);
    let _ = engine.check_order(1001, 10);
    engine.record_fill(1001, 10, f64::INFINITY, 25);
    engine.record_fill(1001, -10, f64::NEG_INFINITY, 25);
}

#[test]
fn no_panic_risk_engine_zero_everything() {
    let mut engine = RiskEngine::new(0.0, 0, 0.0);
    let _ = engine.check_order(0, 0);
    engine.record_fill(0, 0, 0.0, 0);
    engine.reset_daily();
}

#[test]
fn no_panic_risk_engine_negative_capital() {
    let mut engine = RiskEngine::new(2.0, 100, -1_000_000.0);
    let _ = engine.check_order(1001, 10);
}

// ---------------------------------------------------------------------------
// Must NOT panic: Indicator RingBuffer with extreme inputs
// ---------------------------------------------------------------------------

#[test]
fn no_panic_ring_buffer_push_nan() {
    let mut ring = RingBuffer::new();
    for _ in 0..1000 {
        let _ = ring.push(f64::NAN);
    }
    // Should not panic, just contain NaN values
    assert!(!ring.is_empty());
}

#[test]
fn no_panic_ring_buffer_push_infinity() {
    let mut ring = RingBuffer::new();
    let _ = ring.push(f64::INFINITY);
    let _ = ring.push(f64::NEG_INFINITY);
    assert!(!ring.is_empty());
}

#[test]
fn no_panic_ring_buffer_push_max_f64() {
    let mut ring = RingBuffer::new();
    let _ = ring.push(f64::MAX);
    let _ = ring.push(f64::MIN);
    assert!(!ring.is_empty());
}

#[test]
fn no_panic_ring_buffer_oldest_on_empty() {
    let ring = RingBuffer::new();
    // oldest() on empty ring — should return 0.0 (initialized value), not panic
    let val = ring.oldest();
    assert_eq!(val, 0.0);
}

// ---------------------------------------------------------------------------
// Must NOT panic: IndicatorParams edge cases
// ---------------------------------------------------------------------------

#[test]
fn no_panic_ema_alpha_period_one() {
    let alpha = IndicatorParams::ema_alpha(1);
    assert!(
        (alpha - 1.0).abs() < f64::EPSILON,
        "ema_alpha(1) = 2/(1+1) = 1.0"
    );
}

#[test]
fn no_panic_ema_alpha_max_period() {
    let alpha = IndicatorParams::ema_alpha(u16::MAX);
    assert!(alpha > 0.0 && alpha < 1.0);
}

#[test]
fn no_panic_wilder_factor_period_one() {
    let factor = IndicatorParams::wilder_factor(1);
    assert!((factor - 1.0).abs() < f64::EPSILON);
}

#[test]
fn no_panic_wilder_factor_max_period() {
    let factor = IndicatorParams::wilder_factor(u16::MAX);
    assert!(factor > 0.0 && factor < 1.0);
}

// ---------------------------------------------------------------------------
// Must NOT panic: IndicatorState warmup
// ---------------------------------------------------------------------------

#[test]
fn no_panic_indicator_state_new_is_cold() {
    let state = IndicatorState::new();
    assert!(!state.is_warm());
    assert_eq!(state.warmup_count, 0);
}

// ---------------------------------------------------------------------------
// Must NOT panic: Parser with garbage input
// ---------------------------------------------------------------------------

#[test]
fn no_panic_parser_all_zeros() {
    let packet = vec![0u8; 256];
    let result = dhan_live_trader_core::parser::dispatch_frame(&packet, 0);
    // Unknown response code 0 — should return error, not panic
    assert!(result.is_err());
}

#[test]
fn no_panic_parser_all_ones() {
    let packet = vec![0xFF_u8; 256];
    let result = dhan_live_trader_core::parser::dispatch_frame(&packet, 0);
    assert!(result.is_err());
}

#[test]
fn no_panic_parser_single_byte() {
    for byte in 0..=255_u8 {
        let packet = vec![byte];
        let result = dhan_live_trader_core::parser::dispatch_frame(&packet, 0);
        // Must always return Err, never panic
        assert!(result.is_err());
    }
}

#[test]
fn no_panic_parser_max_size_garbage() {
    // 64KB of random-ish data with a valid-looking header
    let mut packet = vec![0u8; 65536];
    packet[0] = 2; // RESPONSE_CODE_TICKER
    packet[1..3].copy_from_slice(&16_u16.to_le_bytes());
    // Rest is zeros — should parse as ticker with zero values, not panic
    let result = dhan_live_trader_core::parser::dispatch_frame(&packet, 0);
    assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// Must NOT panic: OMS error display for all variants
// ---------------------------------------------------------------------------

#[test]
fn no_panic_oms_error_display_all_variants() {
    use dhan_live_trader_trading::oms::types::OmsError;

    let errors: Vec<OmsError> = vec![
        OmsError::RiskRejected {
            reason: String::new(),
        },
        OmsError::RateLimited,
        OmsError::CircuitBreakerOpen,
        OmsError::OrderNotFound {
            order_id: String::new(),
        },
        OmsError::OrderTerminal {
            order_id: String::new(),
            status: String::new(),
        },
        OmsError::InvalidTransition {
            order_id: String::new(),
            from: String::new(),
            to: String::new(),
        },
        OmsError::DhanApiError {
            status_code: 0,
            message: String::new(),
        },
        OmsError::DhanRateLimited,
        OmsError::NoToken,
        OmsError::TokenExpired,
        OmsError::HttpError(String::new()),
        OmsError::JsonError(String::new()),
    ];

    for err in errors {
        // Display trait must not panic
        let msg = err.to_string();
        assert!(!msg.is_empty());
    }
}
