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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
