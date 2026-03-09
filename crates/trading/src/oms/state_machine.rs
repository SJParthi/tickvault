//! Order lifecycle state machine — validates transitions from WebSocket updates.
//!
//! Uses a simple match-based approach rather than `statig` for the state machine,
//! since the order lifecycle is a straightforward DAG with no entry/exit actions.
//! `statig` is available if the FSM grows more complex in Phase 2.
//!
//! # Valid Transitions (from Phase 1 spec §10)
//! ```text
//! Transit  → Pending     (reached exchange)
//! Transit  → Rejected    (exchange rejected immediately)
//! Pending  → Confirmed   (exchange accepted)
//! Confirmed → Traded     (fully filled)
//! Confirmed → Cancelled  (user cancelled)
//! Confirmed → Expired    (end of validity)
//! Pending  → Traded      (immediate fill, skips Confirmed)
//! Pending  → Cancelled   (cancelled before confirmation)
//! ```

use dhan_live_trader_common::order_types::OrderStatus;

/// Validates whether a state transition is valid according to the order lifecycle DAG.
///
/// # Returns
/// `true` if the transition `from → to` is valid.
///
/// # Performance
/// O(1) — two-level match with no allocation.
pub fn is_valid_transition(from: OrderStatus, to: OrderStatus) -> bool {
    matches!(
        (from, to),
        // Transit can go to Pending or Rejected
        (OrderStatus::Transit, OrderStatus::Pending)
            | (OrderStatus::Transit, OrderStatus::Rejected)
            // Pending can go to Confirmed, Traded, or Cancelled
            | (OrderStatus::Pending, OrderStatus::Confirmed)
            | (OrderStatus::Pending, OrderStatus::Traded)
            | (OrderStatus::Pending, OrderStatus::Cancelled)
            // Confirmed can go to Traded, Cancelled, or Expired
            | (OrderStatus::Confirmed, OrderStatus::Traded)
            | (OrderStatus::Confirmed, OrderStatus::Cancelled)
            | (OrderStatus::Confirmed, OrderStatus::Expired)
    )
}

/// Parses a Dhan status string (from WebSocket or REST) into an `OrderStatus`.
///
/// Dhan sends uppercase status strings. Returns `None` for unknown values.
///
/// # Performance
/// O(1) — match on string slices.
pub fn parse_order_status(status_str: &str) -> Option<OrderStatus> {
    match status_str {
        "TRANSIT" => Some(OrderStatus::Transit),
        "PENDING" => Some(OrderStatus::Pending),
        "CONFIRMED" => Some(OrderStatus::Confirmed),
        "TRADED" => Some(OrderStatus::Traded),
        "CANCELLED" | "Cancelled" => Some(OrderStatus::Cancelled),
        "REJECTED" => Some(OrderStatus::Rejected),
        "EXPIRED" => Some(OrderStatus::Expired),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Valid transitions ---

    #[test]
    fn transit_to_pending_valid() {
        assert!(is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Pending
        ));
    }

    #[test]
    fn transit_to_rejected_valid() {
        assert!(is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Rejected
        ));
    }

    #[test]
    fn pending_to_confirmed_valid() {
        assert!(is_valid_transition(
            OrderStatus::Pending,
            OrderStatus::Confirmed
        ));
    }

    #[test]
    fn pending_to_traded_valid() {
        assert!(is_valid_transition(
            OrderStatus::Pending,
            OrderStatus::Traded
        ));
    }

    #[test]
    fn pending_to_cancelled_valid() {
        assert!(is_valid_transition(
            OrderStatus::Pending,
            OrderStatus::Cancelled
        ));
    }

    #[test]
    fn confirmed_to_traded_valid() {
        assert!(is_valid_transition(
            OrderStatus::Confirmed,
            OrderStatus::Traded
        ));
    }

    #[test]
    fn confirmed_to_cancelled_valid() {
        assert!(is_valid_transition(
            OrderStatus::Confirmed,
            OrderStatus::Cancelled
        ));
    }

    #[test]
    fn confirmed_to_expired_valid() {
        assert!(is_valid_transition(
            OrderStatus::Confirmed,
            OrderStatus::Expired
        ));
    }

    // --- Invalid transitions ---

    #[test]
    fn traded_to_anything_invalid() {
        assert!(!is_valid_transition(
            OrderStatus::Traded,
            OrderStatus::Pending
        ));
        assert!(!is_valid_transition(
            OrderStatus::Traded,
            OrderStatus::Transit
        ));
        assert!(!is_valid_transition(
            OrderStatus::Traded,
            OrderStatus::Cancelled
        ));
    }

    #[test]
    fn rejected_to_anything_invalid() {
        assert!(!is_valid_transition(
            OrderStatus::Rejected,
            OrderStatus::Pending
        ));
        assert!(!is_valid_transition(
            OrderStatus::Rejected,
            OrderStatus::Transit
        ));
    }

    #[test]
    fn cancelled_to_anything_invalid() {
        assert!(!is_valid_transition(
            OrderStatus::Cancelled,
            OrderStatus::Pending
        ));
        assert!(!is_valid_transition(
            OrderStatus::Cancelled,
            OrderStatus::Confirmed
        ));
    }

    #[test]
    fn expired_to_anything_invalid() {
        assert!(!is_valid_transition(
            OrderStatus::Expired,
            OrderStatus::Pending
        ));
    }

    #[test]
    fn transit_to_traded_invalid() {
        // Cannot skip directly from Transit to Traded
        assert!(!is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Traded
        ));
    }

    #[test]
    fn transit_to_confirmed_invalid() {
        // Must go through Pending first
        assert!(!is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Confirmed
        ));
    }

    #[test]
    fn same_state_transition_invalid() {
        assert!(!is_valid_transition(
            OrderStatus::Pending,
            OrderStatus::Pending
        ));
        assert!(!is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Transit
        ));
    }

    // --- parse_order_status ---

    #[test]
    fn parse_all_known_statuses() {
        assert_eq!(parse_order_status("TRANSIT"), Some(OrderStatus::Transit));
        assert_eq!(parse_order_status("PENDING"), Some(OrderStatus::Pending));
        assert_eq!(
            parse_order_status("CONFIRMED"),
            Some(OrderStatus::Confirmed)
        );
        assert_eq!(parse_order_status("TRADED"), Some(OrderStatus::Traded));
        assert_eq!(
            parse_order_status("CANCELLED"),
            Some(OrderStatus::Cancelled)
        );
        assert_eq!(
            parse_order_status("Cancelled"),
            Some(OrderStatus::Cancelled)
        );
        assert_eq!(parse_order_status("REJECTED"), Some(OrderStatus::Rejected));
        assert_eq!(parse_order_status("EXPIRED"), Some(OrderStatus::Expired));
    }

    #[test]
    fn parse_unknown_status_returns_none() {
        assert_eq!(parse_order_status("UNKNOWN"), None);
        assert_eq!(parse_order_status(""), None);
        assert_eq!(parse_order_status("traded"), None);
    }
}
