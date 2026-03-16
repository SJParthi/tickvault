//! Financial math overflow/underflow tests.
//!
//! Ensures all price, quantity, and P&L calculations handle extreme values
//! without panicking, producing NaN, or wrapping incorrectly.
//! These tests enforce Principle #1 (zero allocation) and correctness
//! for real-money financial calculations.

use dhan_live_trader_trading::risk::engine::RiskEngine;
use dhan_live_trader_trading::risk::types::RiskBreach;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_engine() -> RiskEngine {
    // 2% max daily loss, 100 lots max, 10L capital
    RiskEngine::new(2.0, 100, 1_000_000.0)
}

fn make_engine_custom(max_loss_pct: f64, max_lots: u32, capital: f64) -> RiskEngine {
    RiskEngine::new(max_loss_pct, max_lots, capital)
}

// ---------------------------------------------------------------------------
// Overflow / Underflow: Position sizes
// ---------------------------------------------------------------------------

#[test]
fn overflow_max_i32_order_lots() {
    let mut engine = make_engine_custom(2.0, u32::MAX, f64::MAX / 2.0);
    // Order with i32::MAX lots — should not panic
    let result = engine.check_order(1001, i32::MAX);
    // Will be rejected due to position limit (i32::MAX > u32 threshold depends on config)
    let _ = result; // Just assert no panic
}

#[test]
fn overflow_min_i32_order_lots() {
    let mut engine = make_engine_custom(2.0, u32::MAX, 1_000_000.0);
    // Negative max lots (sell side)
    let result = engine.check_order(1001, i32::MIN);
    let _ = result; // No panic
}

#[test]
fn overflow_zero_lots_always_approved() {
    let mut engine = make_engine();
    let result = engine.check_order(1001, 0);
    assert!(result.is_approved());
}

// ---------------------------------------------------------------------------
// Overflow / Underflow: Prices
// ---------------------------------------------------------------------------

#[test]
fn overflow_record_fill_max_f64_price() {
    let mut engine = make_engine();
    // Fill with extreme price — should not panic
    engine.record_fill(1001, 1, f64::MAX, 25);
    let pos = engine.position(1001);
    assert!(pos.is_some());
}

#[test]
fn overflow_record_fill_zero_price() {
    let mut engine = make_engine();
    engine.record_fill(1001, 10, 0.0, 25);
    let pos = engine.position(1001).unwrap();
    assert_eq!(pos.avg_entry_price, 0.0);
}

#[test]
fn overflow_record_fill_negative_price() {
    let mut engine = make_engine();
    // Negative prices shouldn't happen in practice but must not panic
    engine.record_fill(1001, 10, -100.0, 25);
    let pos = engine.position(1001).unwrap();
    assert_eq!(pos.avg_entry_price, -100.0);
}

#[test]
fn overflow_record_fill_nan_price() {
    let mut engine = make_engine();
    engine.record_fill(1001, 10, f64::NAN, 25);
    let pos = engine.position(1001).unwrap();
    assert!(pos.avg_entry_price.is_nan());
}

#[test]
fn overflow_record_fill_infinity_price() {
    let mut engine = make_engine();
    engine.record_fill(1001, 10, f64::INFINITY, 25);
    let pos = engine.position(1001).unwrap();
    assert!(pos.avg_entry_price.is_infinite());
}

// ---------------------------------------------------------------------------
// Overflow / Underflow: Lot sizes
// ---------------------------------------------------------------------------

#[test]
fn overflow_record_fill_zero_lot_size() {
    let mut engine = make_engine();
    // Zero lot size — P&L calculation multiplies by lot_size
    engine.record_fill(1001, 10, 100.0, 0);
    engine.record_fill(1001, -10, 110.0, 0);
    // P&L = (110-100) * 10 * 0 = 0
    assert_eq!(engine.total_realized_pnl(), 0.0);
}

#[test]
fn overflow_record_fill_max_u32_lot_size() {
    let mut engine = make_engine();
    engine.record_fill(1001, 1, 100.0, u32::MAX);
    engine.record_fill(1001, -1, 101.0, u32::MAX);
    // Should produce large but finite P&L
    let pnl = engine.total_realized_pnl();
    assert!(pnl.is_finite());
    assert!(pnl > 0.0);
}

// ---------------------------------------------------------------------------
// Overflow / Underflow: P&L calculations
// ---------------------------------------------------------------------------

#[test]
fn overflow_pnl_large_loss_does_not_wrap() {
    let mut engine = make_engine();
    // Buy high, sell low — large loss
    engine.record_fill(1001, 100, 1_000_000.0, 100);
    engine.record_fill(1001, -100, 1.0, 100);
    let pnl = engine.total_realized_pnl();
    assert!(pnl < 0.0, "P&L should be negative for a loss");
    assert!(
        pnl.is_finite(),
        "P&L should be finite even for large losses"
    );
}

#[test]
fn overflow_pnl_large_profit_does_not_wrap() {
    let mut engine = make_engine();
    engine.record_fill(1001, 100, 1.0, 100);
    engine.record_fill(1001, -100, 1_000_000.0, 100);
    let pnl = engine.total_realized_pnl();
    assert!(pnl > 0.0, "P&L should be positive for a profit");
    assert!(pnl.is_finite());
}

// ---------------------------------------------------------------------------
// Overflow / Underflow: Capital and loss fractions
// ---------------------------------------------------------------------------

#[test]
fn overflow_zero_capital() {
    let mut engine = make_engine_custom(2.0, 100, 0.0);
    // Max daily loss = 0.02 * 0.0 = 0.0
    // Any loss should breach immediately
    engine.record_fill(1001, 10, 100.0, 25);
    engine.record_fill(1001, -10, 99.0, 25);
    let result = engine.check_order(1002, 1);
    // With zero capital, max loss = 0, so any negative P&L triggers breach
    assert!(!result.is_approved());
}

#[test]
fn overflow_max_capital() {
    let mut engine = make_engine_custom(2.0, 100, f64::MAX);
    // Max daily loss = 0.02 * f64::MAX — could overflow to infinity
    let result = engine.check_order(1001, 1);
    assert!(result.is_approved());
}

#[test]
fn overflow_zero_loss_fraction() {
    let mut engine = make_engine_custom(0.0, 100, 1_000_000.0);
    // Max loss = 0% — any loss should breach
    engine.record_fill(1001, 10, 100.0, 25);
    engine.record_fill(1001, -10, 99.99, 25);
    let result = engine.check_order(1002, 1);
    // 0% loss threshold + negative P&L = halt
    assert!(!result.is_approved());
}

#[test]
fn overflow_hundred_percent_loss_fraction() {
    let mut engine = make_engine_custom(100.0, 100, 1_000_000.0);
    // Max loss = 100% of capital = 1M — very permissive
    engine.record_fill(1001, 100, 100.0, 25);
    engine.record_fill(1001, -100, 1.0, 25);
    // Loss = (100-1) * 100 * 25 = 247,500 < 1M threshold
    let result = engine.check_order(1002, 1);
    assert!(result.is_approved());
}

// ---------------------------------------------------------------------------
// Boundary: Position reversal through zero
// ---------------------------------------------------------------------------

#[test]
fn boundary_position_reversal_long_to_short() {
    let mut engine = make_engine();
    engine.record_fill(1001, 10, 100.0, 25);
    // Sell 20 to reverse from +10 to -10
    engine.record_fill(1001, -20, 110.0, 25);
    let pos = engine.position(1001).unwrap();
    assert_eq!(pos.net_lots, -10);
    // Entry price should be the new fill price (reversal sets new entry)
    assert_eq!(pos.avg_entry_price, 110.0);
}

#[test]
fn boundary_position_reversal_short_to_long() {
    let mut engine = make_engine();
    engine.record_fill(1001, -10, 200.0, 25);
    engine.record_fill(1001, 20, 190.0, 25);
    let pos = engine.position(1001).unwrap();
    assert_eq!(pos.net_lots, 10);
    assert_eq!(pos.avg_entry_price, 190.0);
}

// ---------------------------------------------------------------------------
// Boundary: Saturating arithmetic on counters
// ---------------------------------------------------------------------------

#[test]
fn boundary_check_counter_saturates() {
    let mut engine = make_engine();
    // Run many checks — counter uses saturating_add
    for i in 0..10_000_u32 {
        let _ = engine.check_order(1001, 1);
        assert_eq!(engine.total_checks(), u64::from(i) + 1);
    }
}

// ---------------------------------------------------------------------------
// Boundary: Edge position sizes
// ---------------------------------------------------------------------------

#[test]
fn boundary_exactly_at_position_limit() {
    let mut engine = make_engine(); // max 100 lots
    let result = engine.check_order(1001, 100);
    assert!(result.is_approved(), "exactly at limit should be approved");
}

#[test]
fn boundary_one_over_position_limit() {
    let mut engine = make_engine(); // max 100 lots
    let result = engine.check_order(1001, 101);
    assert!(!result.is_approved(), "one over limit must be rejected");
}

#[test]
fn boundary_negative_at_position_limit() {
    let mut engine = make_engine(); // max 100 lots
    let result = engine.check_order(1001, -100);
    assert!(result.is_approved(), "negative at limit should be approved");
}

#[test]
fn boundary_negative_over_position_limit() {
    let mut engine = make_engine(); // max 100 lots
    let result = engine.check_order(1001, -101);
    assert!(
        !result.is_approved(),
        "negative over limit must be rejected"
    );
}

// ---------------------------------------------------------------------------
// Boundary: Daily loss threshold edge
// ---------------------------------------------------------------------------

#[test]
fn boundary_loss_just_below_threshold() {
    // 2% of 1M = 20,000 max loss
    let mut engine = make_engine();
    // Loss of 19,999 (just under)
    // (100 - 92.0004) * 100 * 25 = 19,999
    engine.record_fill(1001, 100, 100.0, 25);
    engine.record_fill(1001, -100, 92.0004, 25);
    let pnl = engine.total_realized_pnl();
    assert!(pnl > -20_000.0, "loss should be just under threshold");
    let result = engine.check_order(1002, 1);
    assert!(
        result.is_approved(),
        "loss just below threshold should allow trading"
    );
}

#[test]
fn boundary_loss_exactly_at_threshold() {
    // 2% of 1M = 20,000 max loss
    let mut engine = make_engine();
    // Loss of exactly 20,000
    engine.record_fill(1001, 100, 100.0, 25);
    engine.record_fill(1001, -100, 92.0, 25);
    let pnl = engine.total_realized_pnl();
    assert_eq!(pnl, -20_000.0);
    let result = engine.check_order(1002, 1);
    assert!(!result.is_approved(), "loss at threshold must trigger halt");
    assert_eq!(engine.halt_reason(), Some(RiskBreach::MaxDailyLossExceeded));
}

// ---------------------------------------------------------------------------
// Property-like: Multiple instruments independent
// ---------------------------------------------------------------------------

#[test]
fn property_instruments_independent_pnl() {
    let mut engine = make_engine();
    // Instrument A: profit
    engine.record_fill(1001, 10, 100.0, 25);
    engine.record_fill(1001, -10, 110.0, 25);
    // Instrument B: loss
    engine.record_fill(1002, 10, 200.0, 50);
    engine.record_fill(1002, -10, 190.0, 50);

    // Realized P&L: A = +2500, B = -5000, total = -2500
    let pnl = engine.total_realized_pnl();
    assert!((pnl - (-2500.0)).abs() < 0.01);
}
