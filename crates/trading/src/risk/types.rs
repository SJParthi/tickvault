//! Risk engine types — breach kinds, check results, and position tracking.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Risk Breach — why trading was halted
// ---------------------------------------------------------------------------

/// Reason for a risk-triggered trading halt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RiskBreach {
    /// Realized + unrealized P&L exceeded max daily loss threshold.
    MaxDailyLossExceeded,
    /// Position size for a single instrument exceeded max lots.
    PositionSizeLimitExceeded,
    /// Manual halt triggered by operator.
    ManualHalt,
}

// ---------------------------------------------------------------------------
// Risk Check — pre-trade validation result
// ---------------------------------------------------------------------------

/// Result of a pre-trade risk check.
#[derive(Debug, Clone, PartialEq)]
pub enum RiskCheck {
    /// Order is allowed to proceed.
    Approved,
    /// Order rejected due to risk breach.
    Rejected {
        /// Which risk limit was breached.
        breach: RiskBreach,
        /// Human-readable explanation.
        reason: String,
    },
}

impl RiskCheck {
    /// Returns true if the order was approved.
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }
}

// ---------------------------------------------------------------------------
// Position Info — per-instrument position tracking
// ---------------------------------------------------------------------------

/// Tracks the current position for a single instrument.
#[derive(Debug, Clone, Copy, Default)]
pub struct PositionInfo {
    /// Net quantity in lots (positive = long, negative = short).
    pub net_lots: i32,
    /// Average entry price for the current position.
    pub avg_entry_price: f64,
    /// Realized P&L from closed trades (in rupees).
    pub realized_pnl: f64,
    /// Contract lot size (2026-07-14 unrealized-P&L fix): stored on every
    /// `record_fill` so mark-to-market P&L multiplies by the SAME lot size
    /// realized P&L uses. `0` (the `Default`) is treated as `1` in math —
    /// a pre-fix position or an equity fill is never zeroed out.
    pub lot_size: u32,
}

impl PositionInfo {
    /// Unrealized P&L in rupees for this leg at the given mark price.
    ///
    /// The ONE unrealized formula — the risk-engine total and the
    /// per-leg P&L emission path both delegate here (order-leg-pnl
    /// Item 1). A non-finite mark or entry yields 0.0 (defense in
    /// depth — `update_market_price` rejects non-finite marks
    /// upstream) and a zero `lot_size` is treated as 1, mirroring
    /// the `record_fill` guard.
    #[must_use]
    pub fn unrealized_at(&self, mark: f64) -> f64 {
        if !mark.is_finite() || !self.avg_entry_price.is_finite() {
            return 0.0;
        }
        let lot = if self.lot_size == 0 { 1 } else { self.lot_size };
        (mark - self.avg_entry_price) * f64::from(self.net_lots) * f64::from(lot)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unrealized_at_boundaries() {
        // Long profit: (110 - 100) * 2 * 75 = 1500.
        let long = PositionInfo {
            net_lots: 2,
            avg_entry_price: 100.0,
            realized_pnl: 0.0,
            lot_size: 75,
        };
        assert!((long.unrealized_at(110.0) - 1500.0).abs() < 1e-9);

        // Short profit: (190 - 200) * (-3) * 50 = 1500.
        let short = PositionInfo {
            net_lots: -3,
            avg_entry_price: 200.0,
            realized_pnl: 0.0,
            lot_size: 50,
        };
        assert!((short.unrealized_at(190.0) - 1500.0).abs() < 1e-9);

        // Flat leg: net_lots 0 => 0.0 at any mark.
        let flat = PositionInfo {
            net_lots: 0,
            avg_entry_price: 100.0,
            realized_pnl: 0.0,
            lot_size: 75,
        };
        assert!(flat.unrealized_at(1.0).abs() < 1e-9);
        assert!(flat.unrealized_at(99999.0).abs() < 1e-9);

        // lot_size 0 treated as 1 (record_fill guard mirror):
        // (12.5 - 10.0) * 1 * 1 = 2.5.
        let zero_lot = PositionInfo {
            net_lots: 1,
            avg_entry_price: 10.0,
            realized_pnl: 0.0,
            lot_size: 0,
        };
        assert!((zero_lot.unrealized_at(12.5) - 2.5).abs() < 1e-9);

        // Non-finite marks yield 0.0 (defense in depth).
        assert!(long.unrealized_at(f64::NAN).abs() < 1e-9);
        assert!(long.unrealized_at(f64::INFINITY).abs() < 1e-9);

        // Large-magnitude stays finite.
        let large = PositionInfo {
            net_lots: 1000,
            avg_entry_price: 1.0,
            realized_pnl: 0.0,
            lot_size: 75,
        };
        assert!(large.unrealized_at(1.0e6).is_finite());
    }

    #[test]
    fn risk_check_approved() {
        let check = RiskCheck::Approved;
        assert!(check.is_approved());
    }

    #[test]
    fn risk_check_rejected() {
        let check = RiskCheck::Rejected {
            breach: RiskBreach::MaxDailyLossExceeded,
            reason: "daily loss exceeded 2%".to_string(),
        };
        assert!(!check.is_approved());
    }

    #[test]
    fn position_info_default() {
        let pos = PositionInfo::default();
        assert_eq!(pos.net_lots, 0);
        assert_eq!(pos.avg_entry_price, 0.0);
        assert_eq!(pos.realized_pnl, 0.0);
        assert_eq!(
            pos.lot_size, 0,
            "default lot_size is 0 (treated as 1 in math)"
        );
    }

    #[test]
    fn risk_breach_serialization() {
        let breach = RiskBreach::MaxDailyLossExceeded;
        let json = serde_json::to_string(&breach).unwrap();
        assert!(json.contains("MaxDailyLossExceeded"));
    }

    #[test]
    fn risk_breach_equality() {
        assert_eq!(RiskBreach::ManualHalt, RiskBreach::ManualHalt);
        assert_ne!(RiskBreach::ManualHalt, RiskBreach::MaxDailyLossExceeded);
    }

    #[test]
    fn risk_breach_all_variants_serialize() {
        let variants = [
            RiskBreach::MaxDailyLossExceeded,
            RiskBreach::PositionSizeLimitExceeded,
            RiskBreach::ManualHalt,
        ];
        for breach in &variants {
            let json = serde_json::to_string(breach).unwrap();
            assert!(
                !json.is_empty(),
                "serialization must produce non-empty JSON"
            );
        }
    }

    #[test]
    fn risk_breach_serialization_position_size() {
        let breach = RiskBreach::PositionSizeLimitExceeded;
        let json = serde_json::to_string(&breach).unwrap();
        assert!(json.contains("PositionSizeLimitExceeded"));
    }

    #[test]
    fn risk_breach_serialization_manual_halt() {
        let breach = RiskBreach::ManualHalt;
        let json = serde_json::to_string(&breach).unwrap();
        assert!(json.contains("ManualHalt"));
    }

    #[test]
    fn risk_breach_debug_format() {
        let breach = RiskBreach::MaxDailyLossExceeded;
        let debug = format!("{breach:?}");
        assert!(debug.contains("MaxDailyLossExceeded"));
    }

    #[test]
    #[allow(clippy::clone_on_copy)]
    // APPROVED: this test exists to verify Clone trait is wired alongside Copy
    fn risk_breach_clone_and_copy() {
        let breach = RiskBreach::ManualHalt;
        let cloned = breach.clone();
        let copied = breach;
        assert_eq!(breach, cloned);
        assert_eq!(breach, copied);
    }

    #[test]
    fn risk_breach_all_variants_distinct() {
        let a = RiskBreach::MaxDailyLossExceeded;
        let b = RiskBreach::PositionSizeLimitExceeded;
        let c = RiskBreach::ManualHalt;
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn risk_check_rejected_with_position_limit() {
        let check = RiskCheck::Rejected {
            breach: RiskBreach::PositionSizeLimitExceeded,
            reason: "position limit 10 lots exceeded".to_string(),
        };
        assert!(!check.is_approved());
        if let RiskCheck::Rejected { breach, reason } = &check {
            assert_eq!(*breach, RiskBreach::PositionSizeLimitExceeded);
            assert!(reason.contains("position limit"));
        }
    }

    #[test]
    fn risk_check_rejected_with_manual_halt() {
        let check = RiskCheck::Rejected {
            breach: RiskBreach::ManualHalt,
            reason: "operator initiated halt".to_string(),
        };
        assert!(!check.is_approved());
        if let RiskCheck::Rejected { breach, reason } = &check {
            assert_eq!(*breach, RiskBreach::ManualHalt);
            assert!(reason.contains("operator"));
        }
    }

    #[test]
    fn risk_check_approved_debug_format() {
        let check = RiskCheck::Approved;
        let debug = format!("{check:?}");
        assert!(debug.contains("Approved"));
    }

    #[test]
    fn risk_check_rejected_debug_format() {
        let check = RiskCheck::Rejected {
            breach: RiskBreach::MaxDailyLossExceeded,
            reason: "test".to_string(),
        };
        let debug = format!("{check:?}");
        assert!(debug.contains("Rejected"));
        assert!(debug.contains("MaxDailyLossExceeded"));
    }

    #[test]
    fn risk_check_equality() {
        let a = RiskCheck::Approved;
        let b = RiskCheck::Approved;
        assert_eq!(a, b);

        let c = RiskCheck::Rejected {
            breach: RiskBreach::ManualHalt,
            reason: "halt".to_string(),
        };
        assert_ne!(a, c);
    }

    #[test]
    fn risk_check_rejected_equality_same_breach_different_reason() {
        let a = RiskCheck::Rejected {
            breach: RiskBreach::MaxDailyLossExceeded,
            reason: "reason A".to_string(),
        };
        let b = RiskCheck::Rejected {
            breach: RiskBreach::MaxDailyLossExceeded,
            reason: "reason B".to_string(),
        };
        assert_ne!(a, b, "different reasons must produce non-equal checks");
    }

    #[test]
    fn position_info_with_long_position() {
        let pos = PositionInfo {
            net_lots: 5,
            avg_entry_price: 245.50,
            realized_pnl: 0.0,
            lot_size: 25,
        };
        assert_eq!(pos.net_lots, 5);
        assert!((pos.avg_entry_price - 245.50).abs() < f64::EPSILON);
    }

    #[test]
    fn position_info_with_short_position() {
        let pos = PositionInfo {
            net_lots: -3,
            avg_entry_price: 300.0,
            realized_pnl: 1500.0,
            lot_size: 25,
        };
        assert!(pos.net_lots < 0, "short position has negative lots");
        assert!((pos.realized_pnl - 1500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn position_info_debug_format() {
        let pos = PositionInfo {
            net_lots: 2,
            avg_entry_price: 100.0,
            realized_pnl: 50.0,
            lot_size: 1,
        };
        let debug = format!("{pos:?}");
        assert!(debug.contains("net_lots"));
        assert!(debug.contains("avg_entry_price"));
        assert!(debug.contains("realized_pnl"));
    }

    #[test]
    #[allow(clippy::clone_on_copy)]
    // APPROVED: this test exists to verify Clone trait is wired alongside Copy
    fn position_info_clone_and_copy() {
        let pos = PositionInfo {
            net_lots: 10,
            avg_entry_price: 200.0,
            realized_pnl: -500.0,
            lot_size: 50,
        };
        let cloned = pos.clone();
        let copied = pos;
        assert_eq!(pos.net_lots, cloned.net_lots);
        assert_eq!(pos.net_lots, copied.net_lots);
        assert!((pos.realized_pnl - cloned.realized_pnl).abs() < f64::EPSILON);
    }
}
