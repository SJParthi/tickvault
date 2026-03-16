//! Graceful degradation tests for trading components.
//!
//! Verifies risk engine and circuit breaker handle cascading failures.

use dhan_live_trader_trading::oms::circuit_breaker::OrderCircuitBreaker;
use dhan_live_trader_trading::risk::engine::RiskEngine;

// ---------------------------------------------------------------------------
// Risk engine: degradation under cascading failures
// ---------------------------------------------------------------------------

#[test]
fn degradation_risk_engine_survives_rapid_halts() {
    let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);

    // Halt → reset → halt rapidly
    for _ in 0..100 {
        engine.manual_halt();
        assert!(engine.is_halted());
        let _ = engine.check_order(1001, 1); // Should be rejected
        engine.reset_halt();
        assert!(!engine.is_halted());
        let result = engine.check_order(1001, 1);
        assert!(result.is_approved());
    }
}

#[test]
fn degradation_risk_engine_daily_reset_clears_all() {
    let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);

    // Build up complex state
    for sec in 0..50_u32 {
        engine.record_fill(sec, 50, 100.0 + sec as f64, 25);
    }
    engine.manual_halt();

    // Daily reset should clear everything
    engine.reset_daily();
    assert!(!engine.is_halted());
    assert_eq!(engine.total_realized_pnl(), 0.0);
    assert_eq!(engine.open_position_count(), 0);
    assert_eq!(engine.total_checks(), 0);
    assert_eq!(engine.total_rejections(), 0);

    // Should accept orders again
    let result = engine.check_order(1001, 1);
    assert!(result.is_approved());
}

// ---------------------------------------------------------------------------
// Circuit breaker: degradation scenarios
// ---------------------------------------------------------------------------

#[test]
fn degradation_circuit_breaker_reset_from_open() {
    let cb = OrderCircuitBreaker::new();

    // Force open
    for _ in 0..10 {
        cb.record_failure();
    }
    assert!(cb.check().is_err());

    // Reset should close
    cb.reset();
    assert!(cb.check().is_ok());
}

#[test]
fn degradation_circuit_breaker_rapid_open_close_cycles() {
    let cb = OrderCircuitBreaker::new();

    for _ in 0..50 {
        // Open
        for _ in 0..10 {
            cb.record_failure();
        }
        assert!(cb.check().is_err());

        // Close via reset
        cb.reset();
        assert!(cb.check().is_ok());
    }
}
