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

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, instrument, warn};

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
    positions: HashMap<u64, PositionInfo>,
    /// Latest market prices keyed by security_id (for unrealized P&L).
    market_prices: HashMap<u64, f64>,
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
    /// Alert callback for immediate Telegram alerts on risk breaches.
    /// Implemented by the app crate to bridge to `NotificationService`.
    // O(1) EXEMPT: cold path — risk halt fires at most once per day, not per-tick
    alert_sink: Option<Box<dyn RiskAlertSink>>,
}

/// Callback trait for Risk Engine → Telegram alerts.
/// Decouples the trading crate from the core notification crate.
pub trait RiskAlertSink: Send + Sync {
    /// Fires a risk halt alert. Best-effort — never blocks.
    // TEST-EXEMPT: trait method implemented by app crate, tested via integration tests
    fn fire_risk_halt(&self, reason: &'static str);
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
            alert_sink: None,
        }
    }

    /// Sets the alert sink for immediate Telegram alerts on risk breaches.
    // TEST-EXEMPT: setter wired at boot, tested indirectly by risk engine integration tests | O(1) EXEMPT: cold path — called once at boot, not per-tick
    pub fn set_alert_sink(&mut self, sink: Box<dyn RiskAlertSink>) {
        // O(1) EXEMPT: cold path
        // O(1) EXEMPT: cold path
        self.alert_sink = Some(sink);
    }

    /// Pre-trade risk check. Returns `Approved` or `Rejected` with reason.
    ///
    /// # Arguments
    /// * `security_id` — instrument to trade
    /// * `order_lots` — number of lots in this order (positive = buy, negative = sell)
    ///
    /// # Performance
    /// O(1) — HashMap lookup + arithmetic comparison.
    #[instrument(skip_all, fields(security_id))]
    pub fn check_order(&mut self, security_id: u64, order_lots: i32) -> RiskCheck {
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
        let (total_pnl, max_loss) = self.daily_loss_state();

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
        security_id: u64,
        filled_lots: i32,
        fill_price: f64,
        lot_size: u32,
    ) {
        // S1 (fix-round 2026-07-14): mirror `update_market_price`'s guard —
        // the fill side was ASYMMETRICALLY unguarded. A non-finite or
        // non-positive fill price poisons realized P&L / avg_entry with
        // NaN/±inf, and a NaN `total_pnl` silently BYPASSES the daily-loss
        // halt comparison (`NaN < 0.0` is false). Reject loudly, never fold.
        if !fill_price.is_finite() || fill_price <= 0.0 {
            error!(
                code = ErrorCode::RiskGapPositionPnl.code_str(),
                security_id,
                filled_lots,
                fill_price,
                "RISK-GAP-02: fill REJECTED — non-finite or non-positive fill \
                 price (would poison P&L and silently bypass the daily-loss halt)"
            );
            metrics::counter!("tv_risk_fill_rejected_total", "reason" => "invalid_price")
                .increment(1);
            return;
        }
        if filled_lots == 0 {
            // Zero-lot no-op guard: nothing to book (defensive — the engine
            // never emits zero-lot FillEvents).
            return;
        }
        let pos = self.positions.entry(security_id).or_default();
        // 2026-07-14 unrealized-P&L fix: store the lot size so mark-to-market
        // P&L multiplies by the SAME contract multiplier realized P&L uses.
        pos.lot_size = lot_size.max(1);

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
            // C12 (fix-round 2026-07-14): multiply by pos.lot_size (already
            // normalized `max(1)` above) — the raw `lot_size` arg could be 0,
            // silently zeroing realized P&L on a close.
            let realized = pnl_per_lot * f64::from(closing_lots) * f64::from(pos.lot_size);
            // S1 overflow guard: an extreme-but-finite price can overflow the
            // product to ±inf; skipping the add (loudly) beats poisoning the
            // accumulator the halt comparison reads.
            if realized.is_finite() {
                pos.realized_pnl += realized;
                self.total_realized_pnl += realized;
            } else {
                error!(
                    code = ErrorCode::RiskGapPositionPnl.code_str(),
                    security_id,
                    fill_price,
                    closing_lots,
                    "RISK-GAP-02: realized P&L product overflowed to non-finite — \
                     SKIPPED (accumulator kept finite; investigate the fill price)"
                );
                metrics::counter!("tv_risk_fill_rejected_total", "reason" => "pnl_overflow")
                    .increment(1);
            }
            metrics::gauge!("tv_realized_pnl").set(self.total_realized_pnl);
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
            let denominator = f64::from(pos.net_lots.unsigned_abs());
            // Safety: denominator is always > 0 here because this branch only runs
            // when adding same-direction lots. Guard against division by zero anyway.
            if denominator > 0.0 {
                pos.avg_entry_price = (old_value + new_value) / denominator;
            }
        }
        // If reversing through zero, just set the new entry price
        if (old_lots > 0 && pos.net_lots < 0) || (old_lots < 0 && pos.net_lots > 0) {
            pos.avg_entry_price = fill_price;
        }
        // S1 repair guard: the weighted average can overflow to non-finite
        // under extreme-but-finite inputs — repair to the (guarded-finite)
        // fill price instead of carrying NaN/inf into unrealized P&L.
        if !pos.avg_entry_price.is_finite() {
            error!(
                code = ErrorCode::RiskGapPositionPnl.code_str(),
                security_id,
                fill_price,
                "RISK-GAP-02: avg_entry_price overflowed to non-finite — repaired \
                 to the fill price (P&L accuracy degraded for this position)"
            );
            pos.avg_entry_price = fill_price;
        }
    }

    /// Updates the mark-to-market price for an instrument (for unrealized P&L).
    ///
    /// # Performance
    /// O(1) — HashMap lookup + field update.
    pub fn update_market_price(&mut self, security_id: u64, current_price: f64) {
        // RISK-GAP-02: Reject non-positive and non-finite prices.
        if !current_price.is_finite() || current_price <= 0.0 {
            return;
        }
        self.market_prices.insert(security_id, current_price);
    }

    /// Mark-to-market daily-loss evaluation OUTSIDE the order path
    /// (order-runtime dry-run PR, 2026-07-14): before this, the daily-loss
    /// threshold was only checked inside `check_order`, so a drawdown with
    /// no new signal never halted. The order runtime calls this ≤1/sec after
    /// mark batches. Returns `true` when trading is (now) halted.
    pub fn evaluate_daily_loss_halt(&mut self) -> bool {
        if self.halted {
            return true;
        }
        let (total_pnl, max_loss) = self.daily_loss_state();
        // S1 fail-closed arm (fix-round 2026-07-14): a non-finite total P&L
        // means the book is POISONED — `NaN < 0.0` is false, so treating it
        // as "no loss" would silently bypass the halt. Fail CLOSED instead
        // (defense-in-depth; the record_fill/update_market_price guards make
        // this structurally unreachable with guarded inputs).
        if !total_pnl.is_finite() {
            error!(
                code = ErrorCode::RiskGapPreTrade.code_str(),
                total_pnl,
                "RISK-GAP-01: total P&L is NON-FINITE — failing CLOSED (halt); \
                 a poisoned book must never trade"
            );
            self.trigger_halt(RiskBreach::MaxDailyLossExceeded);
            return true;
        }
        if total_pnl < 0.0 && total_pnl.abs() >= max_loss {
            self.trigger_halt(RiskBreach::MaxDailyLossExceeded);
            return true;
        }
        false
    }

    /// Shared daily-loss computation: `(realized + unrealized, threshold)` —
    /// used by both `check_order` and `evaluate_daily_loss_halt` so the two
    /// paths can never drift.
    fn daily_loss_state(&self) -> (f64, f64) {
        let total_pnl = self.total_realized_pnl + self.total_unrealized_pnl();
        (total_pnl, self.capital * self.max_daily_loss_fraction)
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

    /// Returns the net lots for a specific security (positive = long, negative = short, 0 = flat).
    pub fn net_lots_for(&self, security_id: u64) -> i32 {
        self.positions.get(&security_id).map_or(0, |p| p.net_lots)
    }

    /// Iterates the security ids of every tracked position row (including
    /// flat rows) — the union-side input of the order runtime's local
    /// reconcile invariant (C7 fix-round 2026-07-14: a risk-side position
    /// absent from the FillEvent mirror must be checkable too).
    pub fn position_security_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.positions.keys().copied()
    }

    /// Test-only accumulator poison — exercises the fail-closed non-finite
    /// arm of `evaluate_daily_loss_halt`, which is structurally unreachable
    /// through the guarded production write paths.
    #[cfg(test)]
    // TEST-EXEMPT: cfg(test)-only accumulator poison helper (exercised by the fail-closed halt test)
    pub(crate) fn poison_realized_pnl_for_test(&mut self, value: f64) {
        self.total_realized_pnl = value;
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
                // 2026-07-14 fix (order-runtime dry-run PR): multiply by the
                // contract lot size — previously omitted, so the daily-loss
                // halt would have been 25-75x understated on options
                // (a Rule-11 false-guarantee class). `max(1)` covers pre-fix
                // Default rows and equities (lot_size 0 → 1).
                let unrealized = (pos.net_lots as f64)
                    * (market_price - pos.avg_entry_price)
                    * f64::from(pos.lot_size.max(1));
                // Defensive finiteness guard: with the lot_size multiplier an
                // ABSURD (finite-but-huge, ~1e307) price could overflow the
                // product to ±inf and poison the total. Conservative-skip,
                // same policy as the missing-market-price arm above —
                // unreachable with real NSE prices (stress-test pinned).
                if unrealized.is_finite() {
                    total += unrealized;
                }
            }
            // Conservative: skip securities without a market price
        }
        metrics::gauge!("tv_unrealized_pnl").set(total);
        total
    }

    /// Returns the position info for a specific instrument.
    pub fn position(&self, security_id: u64) -> Option<&PositionInfo> {
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
            // ERROR level + typed code (2026-07-14: the previously-uncoded
            // halt error gains `code = RISK-GAP-01` per charter rule 5; the
            // stale legacy log-routing claim is retired — the reachable
            // page is the RiskAlertSink → NotificationService Telegram
            // below, wired by the order runtime).
            error!(
                code = ErrorCode::RiskGapPreTrade.code_str(),
                breach = ?breach,
                realized_pnl = self.total_realized_pnl,
                "RISK-GAP-01 CRITICAL: RISK BREACH — trading HALTED. ALL orders \
                 blocked. Investigate immediately."
            );
            self.halted = true;
            self.halt_reason = Some(breach);
            // Fire immediate Telegram notification (not just ERROR log with 2-min delay).
            // O(1) EXEMPT: cold path — risk halt fires at most once per day
            if let Some(ref sink) = self.alert_sink {
                let reason: &'static str = match breach {
                    RiskBreach::MaxDailyLossExceeded => "MaxDailyLossExceeded",
                    RiskBreach::PositionSizeLimitExceeded => "PositionSizeLimitExceeded",
                    RiskBreach::ManualHalt => "ManualHalt",
                };
                sink.fire_risk_halt(reason);
            }
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

    #[test]
    fn test_net_lots_for_long_position() {
        let mut engine = make_engine();
        engine.record_fill(1001, 10, 100.0, 25);
        assert_eq!(engine.net_lots_for(1001), 10);
    }

    #[test]
    fn test_net_lots_for_short_position() {
        let mut engine = make_engine();
        engine.record_fill(1001, -5, 200.0, 50);
        assert_eq!(engine.net_lots_for(1001), -5);
    }

    #[test]
    fn test_net_lots_for_flat_position() {
        let mut engine = make_engine();
        engine.record_fill(1001, 10, 100.0, 25);
        engine.record_fill(1001, -10, 110.0, 25);
        assert_eq!(engine.net_lots_for(1001), 0);
    }

    #[test]
    fn test_net_lots_for_unknown_security() {
        let engine = make_engine();
        assert_eq!(engine.net_lots_for(9999), 0);
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
        // S1 (fix-round 2026-07-14): a 0.0-price fill is now REJECTED
        // outright (mirror of update_market_price) — previously it was
        // folded with avg_entry 0.0. Either way: no NaN anywhere.
        let mut engine = make_engine();
        engine.record_fill(1001, 5, 0.0, 25);
        assert!(
            engine.position(1001).is_none(),
            "zero-price fill must be rejected, never booked"
        );
        assert!(!engine.total_realized_pnl().is_nan());
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

    // -----------------------------------------------------------------------
    // trigger_halt idempotency
    // -----------------------------------------------------------------------

    #[test]
    fn test_trigger_halt_idempotent() {
        let mut engine = make_engine();

        // First halt
        engine.manual_halt();
        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::ManualHalt));

        // Second halt — should not change the reason or state
        engine.trigger_halt(RiskBreach::MaxDailyLossExceeded);
        assert!(engine.is_halted());
        // Reason stays as ManualHalt (first halt wins)
        assert_eq!(engine.halt_reason(), Some(RiskBreach::ManualHalt));
    }

    #[test]
    fn test_daily_loss_halt_then_manual_halt_keeps_first_reason() {
        let mut engine = make_engine();

        // Trigger daily loss breach first
        engine.record_fill(1001, 100, 100.0, 25);
        engine.record_fill(1001, -100, 92.0, 25); // -20,000 loss
        let _ = engine.check_order(1002, 1); // This triggers halt

        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::MaxDailyLossExceeded));

        // Manual halt on top — first reason should persist
        engine.manual_halt();
        assert_eq!(engine.halt_reason(), Some(RiskBreach::MaxDailyLossExceeded));
    }

    // -----------------------------------------------------------------------
    // NaN price rejection
    // -----------------------------------------------------------------------

    #[test]
    fn test_nan_price_rejected() {
        let mut engine = make_engine();
        engine.update_market_price(1001, f64::NAN);
        // NaN should NOT be stored
        assert!(!engine.market_prices.contains_key(&1001));
    }

    #[test]
    fn test_infinity_price_rejected() {
        let mut engine = make_engine();
        engine.update_market_price(1001, f64::INFINITY);
        assert!(!engine.market_prices.contains_key(&1001));
    }

    #[test]
    fn test_negative_infinity_price_rejected() {
        let mut engine = make_engine();
        engine.update_market_price(1001, f64::NEG_INFINITY);
        assert!(!engine.market_prices.contains_key(&1001));
    }

    #[test]
    fn test_zero_price_rejected() {
        let mut engine = make_engine();
        engine.update_market_price(1001, 0.0);
        assert!(!engine.market_prices.contains_key(&1001));
    }

    #[test]
    fn test_negative_price_rejected() {
        let mut engine = make_engine();
        engine.update_market_price(1001, -1.0);
        assert!(!engine.market_prices.contains_key(&1001));
    }

    #[test]
    fn test_valid_price_accepted() {
        let mut engine = make_engine();
        engine.update_market_price(1001, 100.0);
        assert_eq!(*engine.market_prices.get(&1001).unwrap(), 100.0);
    }

    // -----------------------------------------------------------------------
    // Position reversal at exact zero boundary
    // -----------------------------------------------------------------------

    #[test]
    fn test_position_reversal_through_exact_zero() {
        let mut engine = make_engine();

        // Buy 10 lots at 100
        engine.record_fill(1001, 10, 100.0, 25);
        assert_eq!(engine.position(1001).unwrap().net_lots, 10);

        // Sell exactly 10 lots at 110 → position goes to zero
        engine.record_fill(1001, -10, 110.0, 25);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 0);
        assert!((pos.avg_entry_price - 0.0).abs() < f64::EPSILON);
        assert!((pos.realized_pnl - 2500.0).abs() < 0.01); // (110-100)*10*25

        // Immediately re-enter in opposite direction
        engine.record_fill(1001, -5, 115.0, 25);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, -5);
        assert!((pos.avg_entry_price - 115.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_position_reversal_through_zero_short_to_long() {
        let mut engine = make_engine();

        // Short 10 lots at 200
        engine.record_fill(1001, -10, 200.0, 50);

        // Buy 10 to close → net_lots = 0
        engine.record_fill(1001, 10, 190.0, 50);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 0);
        assert!((pos.avg_entry_price - 0.0).abs() < f64::EPSILON);

        // Buy 3 more → new long position
        engine.record_fill(1001, 3, 180.0, 50);
        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 3);
        assert!((pos.avg_entry_price - 180.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Unrealized P&L calculation edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_unrealized_pnl_with_no_market_price() {
        let mut engine = make_engine();
        engine.record_fill(1001, 10, 100.0, 25);
        // No market price set → conservative: unrealized = 0
        assert!((engine.total_unrealized_pnl() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_unrealized_pnl_with_market_price() {
        let mut engine = make_engine();
        engine.record_fill(1001, 10, 100.0, 25);
        engine.update_market_price(1001, 110.0);
        // 2026-07-14 fix (order-runtime dry-run PR): unrealized now multiplies
        // by lot_size, symmetric with realized P&L. Before this fix the pinned
        // value here was 100.0 (per-lot) — a 25x understatement that would
        // have hollowed out the daily-loss halt on options.
        // unrealized = 10 * (110 - 100) * 25 = 2500
        assert!((engine.total_unrealized_pnl() - 2500.0).abs() < f64::EPSILON);
    }

    /// 2026-07-14 asserting test for the lot_size fix: realized and
    /// unrealized P&L must use the SAME contract multiplier.
    #[test]
    fn test_unrealized_pnl_multiplies_lot_size() {
        let mut engine = make_engine();
        // Long 4 lots at 100 with lot_size 75 (BANKNIFTY-class contract).
        engine.record_fill(2001, 4, 100.0, 75);
        engine.update_market_price(2001, 90.0);
        // unrealized = 4 * (90 - 100) * 75 = -3000
        assert!((engine.total_unrealized_pnl() - (-3000.0)).abs() < f64::EPSILON);
        // Close at the same 90 → realized must equal the prior unrealized.
        engine.record_fill(2001, -4, 90.0, 75);
        assert!((engine.total_realized_pnl() - (-3000.0)).abs() < f64::EPSILON);
        assert!((engine.total_unrealized_pnl() - 0.0).abs() < f64::EPSILON);
    }

    /// 2026-07-14: mark-to-market halt evaluation between signals — a
    /// drawdown with NO new order must now halt via
    /// `evaluate_daily_loss_halt` (previously only `check_order` evaluated
    /// the threshold).
    #[test]
    fn test_evaluate_daily_loss_halt_boundary() {
        // 2% of 1_000_000 = 20_000 max loss.
        let mut engine = make_engine();
        // Long 8 lots at 100, lot_size 25.
        engine.record_fill(1001, 8, 100.0, 25);
        // Mark just above the loss boundary: 8 * (0.05 - 100)… use exact
        // arithmetic — loss = 8 * (mark - 100) * 25; boundary at -20_000
        // needs mark = 0. Use a bigger position instead: 100 lots.
        engine.record_fill(1001, 92, 100.0, 25); // now 100 lots @ 100
        // Loss just BELOW threshold: 100 * (92.01 - 100) * 25 = -19_975
        engine.update_market_price(1001, 92.01);
        assert!(
            !engine.evaluate_daily_loss_halt(),
            "below the threshold must not halt"
        );
        assert!(!engine.is_halted());
        // Loss exactly AT threshold: 100 * (92 - 100) * 25 = -20_000
        engine.update_market_price(1001, 92.0);
        assert!(
            engine.evaluate_daily_loss_halt(),
            "at the inclusive boundary the mark-to-market halt must fire"
        );
        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::MaxDailyLossExceeded));
        // Idempotent: once halted, stays halted (returns true, no re-trigger).
        assert!(engine.evaluate_daily_loss_halt());
    }

    // -----------------------------------------------------------------------
    // reset_halt when not halted is a no-op
    // -----------------------------------------------------------------------

    #[test]
    fn test_reset_halt_when_not_halted_is_noop() {
        let mut engine = make_engine();
        assert!(!engine.is_halted());
        engine.reset_halt();
        assert!(!engine.is_halted());
        assert!(engine.halt_reason().is_none());
    }

    // -----------------------------------------------------------------------
    // Halt idempotency — multiple manual_halt calls
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_manual_halt_calls_idempotent() {
        let mut engine = make_engine();
        engine.manual_halt();
        engine.manual_halt();
        engine.manual_halt();

        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::ManualHalt));
    }

    // -----------------------------------------------------------------------
    // NaN/non-finite price rejection — additional cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_subnormal_price_accepted() {
        let mut engine = make_engine();
        // Very small positive subnormal is finite and positive → accepted
        let subnormal = f64::MIN_POSITIVE;
        engine.update_market_price(1001, subnormal);
        assert_eq!(*engine.market_prices.get(&1001).unwrap(), subnormal);
    }

    #[test]
    fn test_update_market_price_overwrites_previous() {
        let mut engine = make_engine();
        engine.update_market_price(1001, 100.0);
        assert_eq!(*engine.market_prices.get(&1001).unwrap(), 100.0);

        // Update with new valid price
        engine.update_market_price(1001, 200.0);
        assert_eq!(*engine.market_prices.get(&1001).unwrap(), 200.0);

        // Attempt NaN → rejected, old price preserved
        engine.update_market_price(1001, f64::NAN);
        assert_eq!(*engine.market_prices.get(&1001).unwrap(), 200.0);
    }

    // -----------------------------------------------------------------------
    // Position reversal boundary — exact sell-to-zero
    // -----------------------------------------------------------------------

    #[test]
    fn test_position_reversal_exact_zero_boundary_avg_price_resets() {
        let mut engine = make_engine();

        // Buy 10 at 100
        engine.record_fill(1001, 10, 100.0, 25);
        // Sell exactly 10 at 100 → net = 0, avg_entry_price → 0.0
        engine.record_fill(1001, -10, 100.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, 0);
        assert!((pos.avg_entry_price - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_position_add_to_short_weighted_avg() {
        let mut engine = make_engine();

        // Short 5 at 200
        engine.record_fill(1001, -5, 200.0, 25);
        // Short 5 more at 220 → avg = (5*200 + 5*220) / 10 = 210
        engine.record_fill(1001, -5, 220.0, 25);

        let pos = engine.position(1001).unwrap();
        assert_eq!(pos.net_lots, -10);
        assert!((pos.avg_entry_price - 210.0).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // Counter saturation at u64::MAX
    // -----------------------------------------------------------------------

    #[test]
    fn test_total_checks_counter_saturates_at_u64_max() {
        let mut engine = make_engine();
        engine.total_checks = u64::MAX - 1;

        let _ = engine.check_order(1001, 1);
        assert_eq!(engine.total_checks(), u64::MAX);

        // Another check → must saturate, not overflow
        let _ = engine.check_order(1001, 1);
        assert_eq!(engine.total_checks(), u64::MAX);
    }

    #[test]
    fn test_total_rejections_counter_saturates_at_u64_max() {
        let mut engine = make_engine();
        engine.manual_halt();
        engine.total_rejections = u64::MAX - 1;

        let _ = engine.check_order(1001, 1);
        assert_eq!(engine.total_rejections(), u64::MAX);

        // Another rejection → must saturate, not overflow
        let _ = engine.check_order(1001, 1);
        assert_eq!(engine.total_rejections(), u64::MAX);
    }

    // -----------------------------------------------------------------------
    // Unrealized P&L with multiple positions and mixed market prices
    // -----------------------------------------------------------------------

    #[test]
    fn test_unrealized_pnl_skips_closed_positions() {
        let mut engine = make_engine();
        // Open and close position for 1001
        engine.record_fill(1001, 10, 100.0, 25);
        engine.record_fill(1001, -10, 110.0, 25);
        // Set market price for 1001 — but position is closed (net_lots = 0)
        engine.update_market_price(1001, 120.0);

        // Unrealized P&L should be 0 — closed position is skipped
        assert!((engine.total_unrealized_pnl() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_unrealized_pnl_short_position() {
        let mut engine = make_engine();
        // Short 10 at 200
        engine.record_fill(1001, -10, 200.0, 25);
        engine.update_market_price(1001, 190.0);
        // unrealized = -10 * (190 - 200) * 25 = 2500 (lot_size fix 2026-07-14)
        assert!((engine.total_unrealized_pnl() - 2500.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Daily loss breach at exact boundary
    // -----------------------------------------------------------------------

    #[test]
    fn test_daily_loss_exact_boundary_halts() {
        // 2% of 1_000_000 = 20_000 max loss
        let mut engine = make_engine();
        // Buy 80 lots at 100, sell at 90 → loss = (100-90)*80*25 = 20_000 (exactly at threshold)
        engine.record_fill(1001, 80, 100.0, 25);
        engine.record_fill(1001, -80, 90.0, 25);

        assert_eq!(engine.total_realized_pnl(), -20_000.0);

        // Next order should be rejected — exactly at threshold means >= max_loss
        let result = engine.check_order(1002, 1);
        assert!(!result.is_approved());
        assert!(engine.is_halted());
    }

    #[test]
    fn test_daily_loss_just_below_boundary_allows() {
        // 2% of 1_000_000 = 20_000 max loss
        let mut engine = make_engine();
        // Loss = (100-90.01)*80*25 = 9.99*80*25 = 19_980 (just below 20_000)
        engine.record_fill(1001, 80, 100.0, 25);
        engine.record_fill(1001, -80, 90.01, 25);

        let pnl = engine.total_realized_pnl();
        assert!(pnl > -20_000.0, "loss must be below threshold");

        let result = engine.check_order(1002, 1);
        assert!(result.is_approved());
        assert!(!engine.is_halted());
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: reset_halt when not halted (no-op path)
    // -----------------------------------------------------------------------

    #[test]
    fn test_reset_halt_when_not_halted_is_safe_noop() {
        let mut engine = make_engine();
        assert!(!engine.is_halted());
        // Calling reset_halt when not halted should be a no-op (no panic, no state change)
        engine.reset_halt();
        assert!(!engine.is_halted());
        assert!(engine.halt_reason().is_none());
    }

    #[test]
    fn test_pnl_metrics_exported() {
        metrics::gauge!("tv_realized_pnl").set(0.0_f64);
        metrics::gauge!("tv_unrealized_pnl").set(0.0_f64);
    }

    // -----------------------------------------------------------------------
    // S1 fix-round (2026-07-14): record_fill finiteness/positivity guards
    // -----------------------------------------------------------------------

    #[test]
    fn test_record_fill_rejects_nonfinite_and_nonpositive_price() {
        let mut engine = make_engine();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -100.0] {
            engine.record_fill(1001, 5, bad, 25);
            assert!(
                engine.position(1001).is_none(),
                "fill at price {bad} must be REJECTED (no position row)"
            );
        }
        // A guarded reject must not disturb an EXISTING position either.
        engine.record_fill(1001, 2, 100.0, 25);
        engine.record_fill(1001, -2, f64::NAN, 25);
        let pos = engine.position(1001).expect("position exists"); // APPROVED: test
        assert_eq!(pos.net_lots, 2, "NaN close must not touch the position");
        assert!(pos.realized_pnl.is_finite());
        assert!(engine.total_realized_pnl().is_finite());
    }

    #[test]
    fn test_record_fill_zero_lots_is_noop() {
        let mut engine = make_engine();
        engine.record_fill(1001, 0, 100.0, 25);
        // A lot_size row may be created-or-not; net effect must be flat + 0 P&L.
        assert_eq!(engine.net_lots_for(1001), 0);
        assert!((engine.total_realized_pnl() - 0.0).abs() < f64::EPSILON);
    }

    /// C12: a raw `lot_size = 0` argument must not zero realized P&L on a
    /// close — the realized product uses the normalized position lot size.
    #[test]
    fn test_record_fill_lot_size_zero_realized_uses_normalized() {
        let mut engine = make_engine();
        engine.record_fill(1001, 4, 100.0, 0);
        engine.record_fill(1001, -4, 110.0, 0);
        // (110-100) × 4 lots × lot_size max(0,1)=1 = 40 — never 0.
        assert!(
            (engine.total_realized_pnl() - 40.0).abs() < f64::EPSILON,
            "realized must use the normalized lot size, got {}",
            engine.total_realized_pnl()
        );
    }

    /// S1 overflow arm: a realized product that overflows to ±inf is skipped
    /// loudly — the accumulator the halt comparison reads stays finite.
    #[test]
    fn test_realized_overflow_skipped_keeps_accumulator_finite() {
        let mut engine = make_engine();
        let huge = f64::MAX / 2.0;
        engine.record_fill(1001, 1, huge, 25);
        // Closing at a tiny price: pnl_per_lot ≈ -huge; × 25 overflows → -inf
        // → the add is SKIPPED (counted + coded error), accumulator finite.
        engine.record_fill(1001, -1, 1.0, 25);
        assert!(
            engine.total_realized_pnl().is_finite(),
            "overflowed realized product must never poison the accumulator"
        );
        assert_eq!(engine.net_lots_for(1001), 0, "position math still applies");
    }

    /// S1 fail-closed arm: a non-finite total P&L HALTS (fail-closed), never
    /// reads as "no loss". Structurally unreachable through guarded writes —
    /// exercised via the test-only poison setter.
    #[test]
    fn test_evaluate_daily_loss_halt_fails_closed_on_nonfinite_pnl() {
        for poison in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut engine = make_engine();
            engine.poison_realized_pnl_for_test(poison);
            assert!(
                engine.evaluate_daily_loss_halt(),
                "non-finite total P&L ({poison}) must fail CLOSED"
            );
            assert!(engine.is_halted());
            assert_eq!(engine.halt_reason(), Some(RiskBreach::MaxDailyLossExceeded));
        }
    }

    // -----------------------------------------------------------------------
    // M4/F13: the halt fires the alert sink exactly once per episode
    // -----------------------------------------------------------------------

    struct CountingSink {
        fires: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        last_reason: std::sync::Arc<std::sync::Mutex<String>>,
    }

    impl RiskAlertSink for CountingSink {
        fn fire_risk_halt(&self, reason: &'static str) {
            self.fires.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self
                .last_reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = reason.to_string();
        }
    }

    #[test]
    fn test_halt_fires_risk_halt_once_per_episode() {
        let fires = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let last_reason = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut engine = make_engine();
        engine.set_alert_sink(Box::new(CountingSink {
            fires: std::sync::Arc::clone(&fires),
            last_reason: std::sync::Arc::clone(&last_reason),
        }));
        // Open 1 lot @400 with lot_size 75, mark to 100 → unrealized
        // -22,500 ≥ the 20,000 threshold (2% of 10L).
        engine.record_fill(1001, 1, 400.0, 75);
        engine.update_market_price(1001, 100.0);
        assert!(engine.evaluate_daily_loss_halt(), "breach must halt");
        assert!(
            engine.evaluate_daily_loss_halt(),
            "second evaluation stays halted"
        );
        assert_eq!(
            fires.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the RiskHalt sink must fire EXACTLY once per halt episode"
        );
        assert_eq!(
            *last_reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "MaxDailyLossExceeded"
        );
    }

    /// C7 companion: the position-sid iterator surfaces every tracked row.
    #[test]
    fn test_position_security_ids_iterates_tracked_rows() {
        let mut engine = make_engine();
        assert_eq!(engine.position_security_ids().count(), 0);
        engine.record_fill(1001, 1, 100.0, 25);
        engine.record_fill(2002, -1, 200.0, 25);
        let mut sids: Vec<u64> = engine.position_security_ids().collect();
        sids.sort_unstable();
        assert_eq!(sids, vec![1001, 2002]);
    }
}
