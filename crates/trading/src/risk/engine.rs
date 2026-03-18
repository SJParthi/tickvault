//! Risk engine — enforces max daily loss, position limits, and auto-halt.
//!
//! # Performance
//! - O(1) per pre-trade check: HashMap lookup + arithmetic comparison
//! - No allocation on the check path (all state is pre-allocated)
//!
//! # Thread Safety
//! Single-threaded access assumed (owned by the trading pipeline task).
//! For multi-threaded access, wrap in `Arc<Mutex<RiskEngine>>`.

use std::collections::HashMap;

use tracing::{info, warn};

use super::types::{PositionInfo, RiskBreach, RiskCheck};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Initial capacity for the positions HashMap.
const POSITIONS_INITIAL_CAPACITY: usize = 512;

// ---------------------------------------------------------------------------
// RiskEngine
// ---------------------------------------------------------------------------

/// Stateful risk manager tracking daily P&L and positions.
///
/// Enforces:
/// 1. **Max daily loss**: realized + unrealized P&L vs configured threshold
/// 2. **Position size limits**: per-instrument lot count vs configured max
/// 3. **Auto-halt**: once any breach occurs, all subsequent orders are rejected
pub struct RiskEngine {
    /// Maximum daily loss as a fraction (e.g., 0.02 for 2%).
    max_daily_loss_fraction: f64,
    /// Maximum position size in lots for any single instrument.
    max_position_lots: u32,
    /// Trading capital for daily loss calculation (in rupees).
    capital: f64,
    /// Per-instrument positions keyed by security_id.
    positions: HashMap<u32, PositionInfo>,
    /// Latest market prices keyed by security_id (for unrealized P&L).
    market_prices: HashMap<u32, f64>,
    /// Sum of all realized P&L from closed trades today.
    total_realized_pnl: f64,
    /// Whether trading is halted due to a risk breach.
    halted: bool,
    /// The breach that caused the halt (if any).
    halt_reason: Option<RiskBreach>,
    /// Total orders checked since startup.
    total_checks: u64,
    /// Total orders rejected by risk checks.
    total_rejections: u64,
}

impl RiskEngine {
    /// Creates a new risk engine with the given configuration.
    ///
    /// # Arguments
    /// * `max_daily_loss_percent` — maximum daily loss as percentage (e.g., 2.0 for 2%)
    /// * `max_position_lots` — maximum lots for any single instrument
    /// * `capital` — trading capital in rupees (for daily loss threshold calculation)
    pub fn new(max_daily_loss_percent: f64, max_position_lots: u32, capital: f64) -> Self {
        Self {
            max_daily_loss_fraction: max_daily_loss_percent / 100.0,
            max_position_lots,
            capital,
            positions: HashMap::with_capacity(POSITIONS_INITIAL_CAPACITY),
            market_prices: HashMap::with_capacity(POSITIONS_INITIAL_CAPACITY),
            total_realized_pnl: 0.0,
            halted: false,
            halt_reason: None,
            total_checks: 0,
            total_rejections: 0,
        }
    }

    /// Pre-trade risk check. Returns `Approved` or `Rejected` with reason.
    ///
    /// # Arguments
    /// * `security_id` — instrument to trade
    /// * `order_lots` — number of lots in this order (positive = buy, negative = sell)
    ///
    /// # Performance
    /// O(1) — HashMap lookup + arithmetic comparison.
    pub fn check_order(&mut self, security_id: u32, order_lots: i32) -> RiskCheck {
        self.total_checks = self.total_checks.saturating_add(1);

        // Auto-halt: reject all orders once halted
        if self.halted {
            self.total_rejections = self.total_rejections.saturating_add(1);
            return RiskCheck::Rejected {
                breach: self.halt_reason.unwrap_or(RiskBreach::ManualHalt),
                reason: "trading halted due to risk breach".to_string(), // O(1) EXEMPT: halt path, not normal execution
            };
        }

        // Check 1: Daily loss threshold
        let unrealized = self.total_unrealized_pnl();
        let total_pnl = self.total_realized_pnl + unrealized;
        let max_loss = self.capital * self.max_daily_loss_fraction;

        // P&L is negative for losses; compare absolute value against threshold
        if total_pnl < 0.0 && total_pnl.abs() >= max_loss {
            self.trigger_halt(RiskBreach::MaxDailyLossExceeded);
            self.total_rejections = self.total_rejections.saturating_add(1);
            return RiskCheck::Rejected {
                breach: RiskBreach::MaxDailyLossExceeded,
                // O(1) EXEMPT: error path, only reached on daily loss breach
                reason: format!(
                    "daily loss {:.2} exceeds max {:.2} ({:.1}% of {:.0})",
                    total_pnl.abs(),
                    max_loss,
                    self.max_daily_loss_fraction * 100.0,
                    self.capital
                ),
            };
        }

        // Check 2: Position size limit
        let current_pos = self
            .positions
            .get(&security_id)
            .copied()
            .unwrap_or_default();
        let new_net = current_pos.net_lots.saturating_add(order_lots);
        if new_net.unsigned_abs() > self.max_position_lots {
            self.total_rejections = self.total_rejections.saturating_add(1);
            return RiskCheck::Rejected {
                breach: RiskBreach::PositionSizeLimitExceeded,
                // O(1) EXEMPT: error path, only reached on position limit breach
                reason: format!(
                    "resulting position {} lots exceeds max {} for security {}",
                    new_net, self.max_position_lots, security_id
                ),
            };
        }

        RiskCheck::Approved
    }

    /// Records a fill (trade execution) to update position and P&L tracking.
    ///
    /// # Arguments
    /// * `security_id` — instrument traded
    /// * `filled_lots` — lots filled (positive = buy, negative = sell)
    /// * `fill_price` — execution price per lot (in rupees)
    /// * `lot_size` — contract lot size (e.g., 25 for NIFTY options)
    pub fn record_fill(
        &mut self,
        security_id: u32,
        filled_lots: i32,
        fill_price: f64,
        lot_size: u32,
    ) {
        let pos = self.positions.entry(security_id).or_default();

        // Check if this fill reduces or closes the position (generates realized P&L)
        let is_reducing =
            (pos.net_lots > 0 && filled_lots < 0) || (pos.net_lots < 0 && filled_lots > 0);

        if is_reducing && pos.avg_entry_price > 0.0 {
            let closing_lots = filled_lots.unsigned_abs().min(pos.net_lots.unsigned_abs());
            let pnl_per_lot = if pos.net_lots > 0 {
                // Long position being closed by sell
                fill_price - pos.avg_entry_price
            } else {
                // Short position being closed by buy
                pos.avg_entry_price - fill_price
            };
            let realized = pnl_per_lot * f64::from(closing_lots) * f64::from(lot_size);
            pos.realized_pnl += realized;
            self.total_realized_pnl += realized;
        }

        // Update position
        let old_lots = pos.net_lots;
        pos.net_lots = old_lots.saturating_add(filled_lots);

        // Update average entry price
        if pos.net_lots == 0 {
            pos.avg_entry_price = 0.0;
        } else if (old_lots >= 0 && filled_lots > 0) || (old_lots <= 0 && filled_lots < 0) {
            // Adding to position: weighted average
            let old_value = pos.avg_entry_price * f64::from(old_lots.unsigned_abs());
            let new_value = fill_price * f64::from(filled_lots.unsigned_abs());
            pos.avg_entry_price = (old_value + new_value) / f64::from(pos.net_lots.unsigned_abs());
        }
        // If reversing through zero, just set the new entry price
        if (old_lots > 0 && pos.net_lots < 0) || (old_lots < 0 && pos.net_lots > 0) {
            pos.avg_entry_price = fill_price;
        }
    }

    /// Updates the mark-to-market price for an instrument (for unrealized P&L).
    ///
    /// # Performance
    /// O(1) — HashMap lookup + field update.
    pub fn update_market_price(&mut self, security_id: u32, current_price: f64) {
        // RISK-GAP-02: Reject non-positive and non-finite prices.
        if !current_price.is_finite() || current_price <= 0.0 {
            return;
        }
        self.market_prices.insert(security_id, current_price);
    }

    /// Manually halts trading (operator-initiated).
    pub fn manual_halt(&mut self) {
        self.trigger_halt(RiskBreach::ManualHalt);
    }

    /// Resets the halt state (operator-initiated, for next trading day).
    ///
    /// Does NOT reset P&L or positions — call `reset_daily()` for that.
    pub fn reset_halt(&mut self) {
        if self.halted {
            info!(reason = ?self.halt_reason, "risk halt reset by operator");
            self.halted = false;
            self.halt_reason = None;
        }
    }

    /// Resets all daily state (P&L, positions, halt) for a new trading day.
    pub fn reset_daily(&mut self) {
        self.positions.clear();
        self.market_prices.clear();
        self.total_realized_pnl = 0.0;
        self.halted = false;
        self.halt_reason = None;
        self.total_checks = 0;
        self.total_rejections = 0;
        info!("risk engine daily state reset");
    }

    /// Returns true if trading is currently halted.
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Returns the halt reason (if halted).
    pub fn halt_reason(&self) -> Option<RiskBreach> {
        self.halt_reason
    }

    /// Returns the total realized P&L for today.
    pub fn total_realized_pnl(&self) -> f64 {
        self.total_realized_pnl
    }

    /// Returns the total unrealized P&L across all open positions.
    ///
    /// RISK-GAP-02: Conservative — skips securities with no market price.
    /// O(N) where N = open positions (cold path, called during risk checks).
    pub fn total_unrealized_pnl(&self) -> f64 {
        let mut total = 0.0_f64;
        for (security_id, pos) in &self.positions {
            if pos.net_lots == 0 {
                continue;
            }
            if let Some(&market_price) = self.market_prices.get(security_id) {
                let unrealized = (pos.net_lots as f64) * (market_price - pos.avg_entry_price);
                total += unrealized;
            }
            // Conservative: skip securities without a market price
        }
        total
    }

    /// Returns the position info for a specific instrument.
    pub fn position(&self, security_id: u32) -> Option<&PositionInfo> {
        self.positions.get(&security_id)
    }

    /// Returns the number of instruments with open positions.
    pub fn open_position_count(&self) -> usize {
        self.positions.values().filter(|p| p.net_lots != 0).count()
    }

    /// Returns total orders checked since startup/reset.
    pub fn total_checks(&self) -> u64 {
        self.total_checks
    }

    /// Returns total orders rejected by risk checks.
    pub fn total_rejections(&self) -> u64 {
        self.total_rejections
    }

    /// Returns the configured max daily loss amount (in rupees).
    pub fn max_daily_loss_amount(&self) -> f64 {
        self.capital * self.max_daily_loss_fraction
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    fn trigger_halt(&mut self, breach: RiskBreach) {
        if !self.halted {
            warn!(
                breach = ?breach,
                realized_pnl = self.total_realized_pnl,
                "RISK BREACH — trading halted"
            );
            self.halted = true;
            self.halt_reason = Some(breach);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic
mod tests {
    use super::*;
    use crate::risk::types::RiskBreach;

    fn make_engine() -> RiskEngine {
        // 2% max daily loss, 100 lots max per instrument, 10L capital
        RiskEngine::new(2.0, 100, 1_000_000.0)
    }

    #[test]
    fn new_engine_starts_clean() {
        let engine = make_engine();
        assert!(!engine.is_halted());
        assert!(engine.halt_reason().is_none());
        assert_eq!(engine.total_realized_pnl(), 0.0);
        assert_eq!(engine.total_checks(), 0);
        assert_eq!(engine.total_rejections(), 0);
        assert_eq!(engine.open_position_count(), 0);
    }

    #[test]
    fn check_order_approved_within_limits() {
        let mut engine = make_engine();
        let result = engine.check_order(1001, 10);
        assert!(result.is_approved());
        assert_eq!(engine.total_checks(), 1);
    }

    #[test]
    fn check_order_rejected_position_limit() {
        let mut engine = make_engine();
        // Try to buy 101 lots (max is 100)
        let result = engine.check_order(1001, 101);
        assert!(!result.is_approved());
        if let RiskCheck::Rejected { breach, .. } = result {
            assert_eq!(breach, RiskBreach::PositionSizeLimitExceeded);
        }
    }

    #[test]
    fn check_order_with_existing_position() {
        let mut engine = make_engine();
        // Record a fill for 50 lots
        engine.record_fill(1001, 50, 100.0, 25);
        // Try to add 60 more (total would be 110, exceeds 100)
        let result = engine.check_order(1001, 60);
        assert!(!result.is_approved());
    }

    #[test]
    fn check_order_sell_within_limits() {
        let mut engine = make_engine();
        engine.record_fill(1001, 50, 100.0, 25);
        // Selling 30 lots reduces position to 20 — within limits
        let result = engine.check_order(1001, -30);
        assert!(result.is_approved());
    }

    #[test]
    fn manual_halt_rejects_all_orders() {
        let mut engine = make_engine();
        engine.manual_halt();
        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::ManualHalt));

        let result = engine.check_order(1001, 1);
        assert!(!result.is_approved());
    }

    #[test]
    fn reset_halt_allows_trading_again() {
        let mut engine = make_engine();
        engine.manual_halt();
        assert!(engine.is_halted());

        engine.reset_halt();
        assert!(!engine.is_halted());

        let result = engine.check_order(1001, 1);
        assert!(result.is_approved());
    }

    #[test]
    fn record_fill_updates_position() {
        let mut engine = make_engine();
        engine.record_fill(1001, 10, 100.0, 25);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 10);
        assert_eq!(pos.avg_entry_price, 100.0);
    }

    #[test]
    fn record_fill_closing_trade_generates_pnl() {
        let mut engine = make_engine();
        // Buy 10 lots at 100
        engine.record_fill(1001, 10, 100.0, 25);
        // Sell 10 lots at 110 → profit = (110-100) * 10 * 25 = 2500
        engine.record_fill(1001, -10, 110.0, 25);

        assert_eq!(engine.total_realized_pnl(), 2500.0);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 0);
    }

    #[test]
    fn record_fill_partial_close() {
        let mut engine = make_engine();
        // Buy 10 lots at 100
        engine.record_fill(1001, 10, 100.0, 25);
        // Sell 5 lots at 110 → profit = (110-100) * 5 * 25 = 1250
        engine.record_fill(1001, -5, 110.0, 25);

        assert_eq!(engine.total_realized_pnl(), 1250.0);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 5);
        assert_eq!(pos.avg_entry_price, 100.0); // Avg unchanged for remaining
    }

    #[test]
    fn record_fill_adds_to_position() {
        let mut engine = make_engine();
        // Buy 5 lots at 100
        engine.record_fill(1001, 5, 100.0, 25);
        // Buy 5 more lots at 120 → avg = (500 + 600) / 10 = 110
        engine.record_fill(1001, 5, 120.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 10);
        assert!((pos.avg_entry_price - 110.0).abs() < 0.01);
    }

    #[test]
    fn daily_loss_breach_halts_trading() {
        // 2% of 1_000_000 = 20_000 max loss
        let mut engine = make_engine();
        // Buy 100 lots at 100, lot size 25
        engine.record_fill(1001, 100, 100.0, 25);
        // Sell 100 lots at 92 → loss = (100-92) * 100 * 25 = 20_000
        engine.record_fill(1001, -100, 92.0, 25);

        assert_eq!(engine.total_realized_pnl(), -20_000.0);

        // Next order should be rejected due to max daily loss
        let result = engine.check_order(1002, 1);
        assert!(!result.is_approved());
        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::MaxDailyLossExceeded));
    }

    #[test]
    fn daily_loss_within_threshold_allows_trading() {
        let mut engine = make_engine();
        // Small loss: (100-99) * 10 * 25 = 250 (well under 20,000 threshold)
        engine.record_fill(1001, 10, 100.0, 25);
        engine.record_fill(1001, -10, 99.0, 25);

        assert_eq!(engine.total_realized_pnl(), -250.0);

        let result = engine.check_order(1002, 1);
        assert!(result.is_approved());
        assert!(!engine.is_halted());
    }

    #[test]
    fn reset_daily_clears_everything() {
        let mut engine = make_engine();
        engine.record_fill(1001, 10, 100.0, 25);
        engine.manual_halt();

        engine.reset_daily();

        assert!(!engine.is_halted());
        assert_eq!(engine.total_realized_pnl(), 0.0);
        assert_eq!(engine.open_position_count(), 0);
        assert_eq!(engine.total_checks(), 0);
        assert_eq!(engine.total_rejections(), 0);
    }

    #[test]
    fn max_daily_loss_amount_correct() {
        let engine = make_engine();
        assert_eq!(engine.max_daily_loss_amount(), 20_000.0); // 2% of 1M
    }

    #[test]
    fn open_position_count() {
        let mut engine = make_engine();
        engine.record_fill(1001, 10, 100.0, 25);
        engine.record_fill(1002, -5, 200.0, 50);
        assert_eq!(engine.open_position_count(), 2);

        // Close one position
        engine.record_fill(1001, -10, 100.0, 25);
        assert_eq!(engine.open_position_count(), 1);
    }

    #[test]
    fn rejection_counter_increments() {
        let mut engine = make_engine();
        engine.manual_halt();

        let _ = engine.check_order(1001, 1);
        let _ = engine.check_order(1002, 1);
        assert_eq!(engine.total_rejections(), 2);
        assert_eq!(engine.total_checks(), 2);
    }

    #[test]
    fn short_position_pnl() {
        let mut engine = make_engine();
        // Short 10 lots at 100
        engine.record_fill(1001, -10, 100.0, 25);
        // Cover at 90 → profit = (100-90) * 10 * 25 = 2500
        engine.record_fill(1001, 10, 90.0, 25);

        assert_eq!(engine.total_realized_pnl(), 2500.0);
    }

    #[test]
    fn zero_lots_order_approved() {
        let mut engine = make_engine();
        let result = engine.check_order(1001, 0);
        assert!(result.is_approved());
    }

    #[test]
    fn multiple_instruments_tracked_independently() {
        let mut engine = make_engine();
        engine.record_fill(1001, 50, 100.0, 25);
        engine.record_fill(1002, 80, 200.0, 50);

        // 1001 can add 50 more (total 100), but 1002 can only add 20 more
        assert!(engine.check_order(1001, 50).is_approved());
        assert!(!engine.check_order(1002, 21).is_approved());
    }

    #[test]
    fn position_for_unknown_security() {
        let engine = make_engine();
        assert!(engine.position(9999).is_none());
    }

    // -----------------------------------------------------------------------
    // P&L edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_position_reversal_long_to_short_pnl() {
        let mut engine = make_engine();
        // Buy 10 at 100
        engine.record_fill(1001, 10, 100.0, 25);
        // Sell 15 at 110 → closes 10 long, opens 5 short
        engine.record_fill(1001, -15, 110.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, -5);
        // Realized P&L from closing 10 long: (110-100)*10*25 = 2500
        assert!((pos.realized_pnl - 2500.0).abs() < 0.01);
        // New short entry price = 110.0
        assert!((pos.avg_entry_price - 110.0).abs() < 0.01);
    }

    #[test]
    fn test_position_reversal_short_to_long_pnl() {
        let mut engine = make_engine();
        // Sell 10 at 200
        engine.record_fill(1001, -10, 200.0, 50);
        // Buy 15 at 180 → closes 10 short, opens 5 long
        engine.record_fill(1001, 15, 180.0, 50);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 5);
        // Realized P&L from closing 10 short: (200-180)*10*50 = 10000
        assert!((pos.realized_pnl - 10000.0).abs() < 0.01);
        // New long entry price = 180.0
        assert!((pos.avg_entry_price - 180.0).abs() < 0.01);
    }

    #[test]
    fn test_weighted_avg_entry_many_fills() {
        let mut engine = make_engine();
        // 5 incremental buys at different prices
        engine.record_fill(1001, 2, 100.0, 25);
        engine.record_fill(1001, 3, 110.0, 25);
        engine.record_fill(1001, 5, 120.0, 25);
        engine.record_fill(1001, 4, 130.0, 25);
        engine.record_fill(1001, 6, 140.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 20);
        // Weighted avg = (2*100 + 3*110 + 5*120 + 4*130 + 6*140) / 20
        //              = (200 + 330 + 600 + 520 + 840) / 20
        //              = 2490 / 20 = 124.5
        let expected = 124.5;
        assert!((pos.avg_entry_price - expected).abs() < 0.01);
    }

    #[test]
    fn test_close_and_reopen_same_instrument() {
        let mut engine = make_engine();
        // Open long
        engine.record_fill(1001, 10, 100.0, 25);
        // Close long → realized = (120-100)*10*25 = 5000
        engine.record_fill(1001, -10, 120.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 0);
        assert!((pos.realized_pnl - 5000.0).abs() < 0.01);

        // Reopen same instrument at new price
        engine.record_fill(1001, 5, 200.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 5);
        assert!((pos.avg_entry_price - 200.0).abs() < 0.01);
        // Old realized P&L persists on the position
        assert!((pos.realized_pnl - 5000.0).abs() < 0.01);
    }

    #[test]
    fn test_short_loss_calculation() {
        let mut engine = make_engine();
        // Short 10 at 100
        engine.record_fill(1001, -10, 100.0, 25);
        // Cover at 110 → loss = (100-110)*10*25 = -2500
        engine.record_fill(1001, 10, 110.0, 25);

        assert!((engine.total_realized_pnl() - (-2500.0)).abs() < 0.01);
        let pos = engine.position(1001).unwrap();
        assert!((pos.realized_pnl - (-2500.0)).abs() < 0.01);
    }

    #[test]
    fn test_multiple_instruments_pnl_independent() {
        let mut engine = make_engine();
        // Instrument A: buy 10 at 100, sell 10 at 120 → profit 5000
        engine.record_fill(1001, 10, 100.0, 25);
        engine.record_fill(1001, -10, 120.0, 25);
        // Instrument B: sell 5 at 200, cover at 180 → profit 5000
        engine.record_fill(2002, -5, 200.0, 25);
        engine.record_fill(2002, 5, 180.0, 25);

        let pos_a = engine.position(1001).unwrap();
        let pos_b = engine.position(2002).unwrap();

        // Each instrument tracks its own realized P&L
        assert!((pos_a.realized_pnl - 5000.0).abs() < 0.01);
        assert!((pos_b.realized_pnl - 2500.0).abs() < 0.01);

        // Total is the sum
        assert!((engine.total_realized_pnl() - 7500.0).abs() < 0.01);
    }

    #[test]
    fn test_reset_daily_then_new_fills() {
        let mut engine = make_engine();
        // Fill, halt
        engine.record_fill(1001, 10, 100.0, 25);
        engine.manual_halt();
        assert!(engine.is_halted());
        assert_eq!(engine.open_position_count(), 1);

        // Reset daily
        engine.reset_daily();
        assert!(!engine.is_halted());
        assert_eq!(engine.total_realized_pnl(), 0.0);
        assert_eq!(engine.open_position_count(), 0);
        assert_eq!(engine.total_checks(), 0);
        assert_eq!(engine.total_rejections(), 0);

        // Fill again — everything starts fresh
        engine.record_fill(2002, 5, 200.0, 25);
        let pos = engine.position(2002).unwrap();
        assert_eq!(pos.net_lots, 5);
        assert!((pos.avg_entry_price - 200.0).abs() < 0.01);
        assert!((pos.realized_pnl).abs() < 0.01);
        // Old instrument should be gone
        assert!(engine.position(1001).is_none());
    }

    #[test]
    fn test_exact_max_lots_boundary() {
        let mut engine = make_engine();
        // Buy exactly 100 lots (the max)
        engine.record_fill(1001, 100, 100.0, 25);
        // check_order with 0 more → approved (100 + 0 = 100, not exceeding)
        assert!(engine.check_order(1001, 0).is_approved());
        // Buy 1 more → rejected (100 + 1 = 101 > 100)
        assert!(!engine.check_order(1001, 1).is_approved());
    }

    #[test]
    fn test_negative_lots_short_position_limit() {
        let mut engine = make_engine();
        // Short 100 lots (by selling)
        engine.record_fill(1001, -100, 100.0, 25);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, -100);

        // Try to short 1 more → net = -101, abs = 101 > 100 → rejected
        assert!(!engine.check_order(1001, -1).is_approved());
        // Covering (buying) should still be allowed
        assert!(engine.check_order(1001, 1).is_approved());
    }

    #[test]
    fn test_fill_at_zero_price_no_nan() {
        let mut engine = make_engine();
        engine.record_fill(1001, 5, 0.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 5);
        assert!((pos.avg_entry_price - 0.0).abs() < 0.01);
        // Ensure no NaN
        assert!(!pos.avg_entry_price.is_nan());
        assert!(!pos.realized_pnl.is_nan());
    }

    #[test]
    fn test_fill_at_very_large_price() {
        let mut engine = make_engine();
        let large_price = f64::MAX / 2.0;
        engine.record_fill(1001, 1, large_price, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 1);
        // Should not overflow — avg_entry_price should be the large price
        assert!((pos.avg_entry_price - large_price).abs() < 1.0);
        assert!(pos.avg_entry_price.is_finite());
    }

    #[test]
    fn test_partial_close_avg_entry_unchanged() {
        let mut engine = make_engine();
        // Buy 20 at 100
        engine.record_fill(1001, 20, 100.0, 25);
        // Sell 10 at 150 → partial close, remaining 10 still at avg 100.0
        engine.record_fill(1001, -10, 150.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 10);
        assert!((pos.avg_entry_price - 100.0).abs() < 0.01);
        // Realized = (150-100)*10*25 = 12500
        assert!((pos.realized_pnl - 12500.0).abs() < 0.01);
    }

    #[test]
    fn test_halt_persists_across_checks() {
        let mut engine = make_engine();
        engine.manual_halt();

        // 100 consecutive check_order calls all rejected
        for i in 0..100 {
            let result = engine.check_order(1001 + i, 1);
            assert!(!result.is_approved());
        }
        assert!(engine.is_halted());
        assert_eq!(engine.total_rejections(), 100);
    }

    #[test]
    fn test_pnl_accumulates_across_instruments() {
        let mut engine = make_engine();

        // Instrument 1: buy 10 at 100, sell at 110 → (110-100)*10*25 = 2500
        engine.record_fill(1001, 10, 100.0, 25);
        engine.record_fill(1001, -10, 110.0, 25);

        // Instrument 2: sell 5 at 200, cover at 190 → (200-190)*5*25 = 1250
        engine.record_fill(2002, -5, 200.0, 25);
        engine.record_fill(2002, 5, 190.0, 25);

        // Instrument 3: buy 8 at 50, sell at 45 → (45-50)*8*25 = -1000
        engine.record_fill(3003, 8, 50.0, 25);
        engine.record_fill(3003, -8, 45.0, 25);

        // Total = 2500 + 1250 + (-1000) = 2750
        assert!((engine.total_realized_pnl() - 2750.0).abs() < 0.01);

        // Verify individual positions
        let pos1 = engine.position(1001).unwrap();
        assert!((pos1.realized_pnl - 2500.0).abs() < 0.01);
        let pos2 = engine.position(2002).unwrap();
        assert!((pos2.realized_pnl - 1250.0).abs() < 0.01);
        let pos3 = engine.position(3003).unwrap();
        assert!((pos3.realized_pnl - (-1000.0)).abs() < 0.01);
    }
}
