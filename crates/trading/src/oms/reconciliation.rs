//! Order reconciliation — syncs OMS state with Dhan REST API.
//!
//! Runs on:
//! - Order update WebSocket reconnect
//! - OMS startup (seed state from today's orders)
//! - Periodic checks (configurable)
//!
//! Mismatches trigger ERROR-level logs (which route to Telegram alerts).

use std::collections::HashMap;

use tracing::{error, info, warn};

use dhan_live_trader_common::order_types::OrderStatus;

use super::state_machine::parse_order_status;
use super::types::{DhanOrderResponse, ManagedOrder, ReconciliationReport};

/// Reconciles OMS order state against orders fetched from the Dhan REST API.
///
/// # Arguments
/// * `oms_orders` — Current OMS order state (keyed by order_id).
/// * `dhan_orders` — Orders fetched from `GET /v2/orders`.
///
/// # Returns
/// A `ReconciliationReport` with mismatch details, plus a list of
/// state updates to apply (order_id, new_status).
///
/// # Side Effects
/// Logs ERROR for each mismatch (triggers Telegram alert via tracing).
pub fn reconcile_orders(
    oms_orders: &HashMap<String, ManagedOrder>,
    dhan_orders: &[DhanOrderResponse],
) -> (ReconciliationReport, Vec<(String, OrderStatus)>) {
    let mut report = ReconciliationReport::default();
    let mut updates: Vec<(String, OrderStatus)> = Vec::new();

    for dhan_order in dhan_orders {
        let dhan_status = match parse_order_status(&dhan_order.order_status) {
            Some(status) => status,
            None => {
                warn!(
                    order_id = %dhan_order.order_id,
                    status = %dhan_order.order_status,
                    "unknown order status from Dhan REST API — skipping"
                );
                continue;
            }
        };

        report.total_checked = report.total_checked.saturating_add(1);

        match oms_orders.get(&dhan_order.order_id) {
            Some(oms_order) => {
                if oms_order.status != dhan_status {
                    error!(
                        order_id = %dhan_order.order_id,
                        oms_status = %oms_order.status.as_str(),
                        dhan_status = %dhan_status.as_str(),
                        "RECONCILIATION MISMATCH — OMS and Dhan disagree on order status"
                    );
                    report.mismatches_found = report.mismatches_found.saturating_add(1);
                    report
                        .mismatched_order_ids
                        .push(dhan_order.order_id.clone());
                    updates.push((dhan_order.order_id.clone(), dhan_status));
                }
            }
            None => {
                // Order exists on Dhan but not in OMS — may have been placed
                // before this OMS instance started, or missed during a gap.
                warn!(
                    order_id = %dhan_order.order_id,
                    status = %dhan_status.as_str(),
                    "order found on Dhan but missing from OMS"
                );
                report.missing_from_oms = report.missing_from_oms.saturating_add(1);
            }
        }
    }

    if report.mismatches_found > 0 {
        error!(
            mismatches = report.mismatches_found,
            total_checked = report.total_checked,
            "RECONCILIATION COMPLETE — mismatches found (see above for details)"
        );
    } else {
        info!(
            total_checked = report.total_checked,
            missing_from_oms = report.missing_from_oms,
            "reconciliation complete — no mismatches"
        );
    }

    (report, updates)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::order_types::{
        OrderType, OrderValidity, ProductType, TransactionType,
    };

    fn make_managed_order(order_id: &str, status: OrderStatus) -> ManagedOrder {
        ManagedOrder {
            order_id: order_id.to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 100,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
        }
    }

    fn make_dhan_order(order_id: &str, status: &str) -> DhanOrderResponse {
        DhanOrderResponse {
            order_id: order_id.to_owned(),
            order_status: status.to_owned(),
            correlation_id: String::new(),
            transaction_type: String::new(),
            exchange_segment: String::new(),
            product_type: String::new(),
            order_type: String::new(),
            validity: String::new(),
            security_id: String::new(),
            quantity: 0,
            price: 0.0,
            trigger_price: 0.0,
            traded_quantity: 0,
            traded_price: 0.0,
            remaining_quantity: 0,
            filled_qty: 0,
            average_trade_price: 0.0,
            exchange_order_id: String::new(),
            exchange_time: String::new(),
            create_time: String::new(),
            update_time: String::new(),
            rejection_reason: String::new(),
            tag: String::new(),
            oms_error_code: String::new(),
            oms_error_description: String::new(),
            trading_symbol: String::new(),
            drv_expiry_date: String::new(),
            drv_option_type: String::new(),
            drv_strike_price: 0.0,
        }
    }

    #[test]
    fn no_orders_no_mismatches() {
        let oms: HashMap<String, ManagedOrder> = HashMap::new();
        let dhan: Vec<DhanOrderResponse> = vec![];
        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 0);
        assert_eq!(report.mismatches_found, 0);
        assert!(updates.is_empty());
    }

    #[test]
    fn matching_status_no_mismatch() {
        let mut oms = HashMap::new();
        oms.insert("1".to_owned(), make_managed_order("1", OrderStatus::Traded));
        let dhan = vec![make_dhan_order("1", "TRADED")];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 1);
        assert_eq!(report.mismatches_found, 0);
        assert!(updates.is_empty());
    }

    #[test]
    fn status_mismatch_detected() {
        let mut oms = HashMap::new();
        oms.insert(
            "1".to_owned(),
            make_managed_order("1", OrderStatus::Pending),
        );
        let dhan = vec![make_dhan_order("1", "TRADED")];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.mismatches_found, 1);
        assert_eq!(report.mismatched_order_ids, vec!["1"]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0], ("1".to_owned(), OrderStatus::Traded));
    }

    #[test]
    fn missing_from_oms_detected() {
        let oms: HashMap<String, ManagedOrder> = HashMap::new();
        let dhan = vec![make_dhan_order("999", "CONFIRMED")];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.missing_from_oms, 1);
        assert_eq!(report.mismatches_found, 0);
        assert!(updates.is_empty());
    }

    #[test]
    fn unknown_dhan_status_skipped() {
        let mut oms = HashMap::new();
        oms.insert(
            "1".to_owned(),
            make_managed_order("1", OrderStatus::Transit),
        );
        let dhan = vec![make_dhan_order("1", "UNKNOWN_STATUS")];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 0); // skipped, not counted
        assert!(updates.is_empty());
    }

    #[test]
    fn multiple_orders_mixed() {
        let mut oms = HashMap::new();
        oms.insert(
            "1".to_owned(),
            make_managed_order("1", OrderStatus::Confirmed),
        );
        oms.insert("2".to_owned(), make_managed_order("2", OrderStatus::Traded));

        let dhan = vec![
            make_dhan_order("1", "TRADED"),  // mismatch
            make_dhan_order("2", "TRADED"),  // match
            make_dhan_order("3", "PENDING"), // missing from OMS
        ];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 3);
        assert_eq!(report.mismatches_found, 1);
        assert_eq!(report.missing_from_oms, 1);
        assert_eq!(updates.len(), 1);
    }
}
