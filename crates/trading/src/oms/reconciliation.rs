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

/// A single correction to apply from reconciliation.
///
/// Carries status and fill data so the engine can sync everything in one pass.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconciliationUpdate {
    pub order_id: String,
    pub status: OrderStatus,
    pub traded_qty: i64,
    pub avg_traded_price: f64,
}

/// Reconciles OMS order state against orders fetched from the Dhan REST API.
///
/// # Arguments
/// * `oms_orders` — Current OMS order state (keyed by order_id).
/// * `dhan_orders` — Orders fetched from `GET /v2/orders`.
///
/// # Returns
/// A `ReconciliationReport` with mismatch details, plus a list of
/// corrections to apply (status + fill data).
///
/// # Side Effects
/// Logs ERROR for each mismatch (triggers Telegram alert via tracing).
pub fn reconcile_orders(
    oms_orders: &HashMap<String, ManagedOrder>,
    dhan_orders: &[DhanOrderResponse],
) -> (ReconciliationReport, Vec<ReconciliationUpdate>) {
    let mut report = ReconciliationReport::default();
    let mut updates: Vec<ReconciliationUpdate> = Vec::new();

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
                let status_mismatch = oms_order.status != dhan_status;
                let fill_mismatch = oms_order.traded_qty != dhan_order.traded_quantity
                    || (oms_order.avg_traded_price - dhan_order.average_trade_price).abs()
                        > f64::EPSILON;

                if status_mismatch {
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
                }

                if status_mismatch || fill_mismatch {
                    updates.push(ReconciliationUpdate {
                        order_id: dhan_order.order_id.clone(),
                        status: dhan_status,
                        traded_qty: dhan_order.traded_quantity,
                        avg_traded_price: dhan_order.average_trade_price,
                    });
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

    // Reverse check: find non-terminal OMS orders not present in Dhan's response.
    // These are "ghost orders" — OMS thinks they exist, but Dhan doesn't.
    let dhan_order_ids: std::collections::HashSet<&str> =
        dhan_orders.iter().map(|o| o.order_id.as_str()).collect();

    for (order_id, oms_order) in oms_orders {
        if !oms_order.is_terminal() && !dhan_order_ids.contains(order_id.as_str()) {
            warn!(
                order_id = %order_id,
                oms_status = %oms_order.status.as_str(),
                "GHOST ORDER — OMS has non-terminal order not found on Dhan"
            );
            report.missing_from_dhan = report.missing_from_dhan.saturating_add(1);
        }
    }

    if report.mismatches_found > 0 || report.missing_from_dhan > 0 {
        error!(
            mismatches = report.mismatches_found,
            missing_from_dhan = report.missing_from_dhan,
            total_checked = report.total_checked,
            "RECONCILIATION COMPLETE — issues found (see above for details)"
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
            modification_count: 0,
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
        assert_eq!(updates[0].order_id, "1");
        assert_eq!(updates[0].status, OrderStatus::Traded);
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
    fn fill_data_mismatch_generates_update() {
        let mut oms = HashMap::new();
        let mut order = make_managed_order("1", OrderStatus::Traded);
        order.traded_qty = 25; // OMS thinks 25 filled
        order.avg_traded_price = 100.0;
        oms.insert("1".to_owned(), order);

        let mut dhan = make_dhan_order("1", "TRADED");
        dhan.traded_quantity = 50; // Dhan says 50 filled
        dhan.average_trade_price = 102.5;
        let dhan = vec![dhan];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.mismatches_found, 0); // status matches
        assert_eq!(updates.len(), 1); // fill data mismatch triggers update
        assert_eq!(updates[0].traded_qty, 50);
        assert!((updates[0].avg_traded_price - 102.5).abs() < f64::EPSILON);
    }

    #[test]
    fn ghost_order_detected() {
        let mut oms = HashMap::new();
        // Non-terminal order in OMS but not in Dhan response = ghost
        oms.insert(
            "ghost-1".to_owned(),
            make_managed_order("ghost-1", OrderStatus::Confirmed),
        );
        // Terminal order in OMS but not in Dhan response = OK (already done)
        oms.insert(
            "done-1".to_owned(),
            make_managed_order("done-1", OrderStatus::Traded),
        );
        let dhan: Vec<DhanOrderResponse> = vec![];

        let (report, _updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.missing_from_dhan, 1); // only the non-terminal one
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
        assert_eq!(report.missing_from_dhan, 0); // both OMS orders appear in Dhan
        assert_eq!(updates.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Edge cases: epsilon boundary, combined mismatches, unknown status count
    // -----------------------------------------------------------------------

    #[test]
    fn epsilon_boundary_fill_mismatch() {
        // If the price difference is exactly at f64::EPSILON, it should NOT
        // trigger a mismatch (comparison uses strict >).
        let mut oms = HashMap::new();
        let mut order = make_managed_order("1", OrderStatus::Traded);
        order.avg_traded_price = 100.0;
        oms.insert("1".to_owned(), order);

        let mut dhan = make_dhan_order("1", "TRADED");
        dhan.average_trade_price = 100.0 + f64::EPSILON;
        let dhan = vec![dhan];

        let (_report, updates) = reconcile_orders(&oms, &dhan);
        // Exactly EPSILON → not > EPSILON → no mismatch
        assert!(updates.is_empty());
    }

    #[test]
    fn price_beyond_epsilon_triggers_fill_update() {
        let mut oms = HashMap::new();
        let mut order = make_managed_order("1", OrderStatus::Traded);
        order.avg_traded_price = 100.0;
        oms.insert("1".to_owned(), order);

        let mut dhan = make_dhan_order("1", "TRADED");
        // Use a clearly-different price that exceeds EPSILON tolerance
        dhan.average_trade_price = 100.5;
        let dhan = vec![dhan];

        let (_report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(updates.len(), 1, "price difference should trigger update");
    }

    #[test]
    fn status_and_fill_mismatch_on_same_order() {
        let mut oms = HashMap::new();
        let mut order = make_managed_order("1", OrderStatus::Pending);
        order.traded_qty = 0;
        oms.insert("1".to_owned(), order);

        let mut dhan = make_dhan_order("1", "TRADED");
        dhan.traded_quantity = 50;
        dhan.average_trade_price = 99.5;
        let dhan = vec![dhan];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.mismatches_found, 1);
        // Both status and fill mismatch → single update with both corrections
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, OrderStatus::Traded);
        assert_eq!(updates[0].traded_qty, 50);
    }

    #[test]
    fn unknown_status_not_counted_in_total_checked() {
        // OMS-GAP-02: Unknown Dhan status → skip, NOT counted in total_checked
        let oms: HashMap<String, ManagedOrder> = HashMap::new();
        let dhan = vec![
            make_dhan_order("1", "GIBBERISH"),
            make_dhan_order("2", "TRADED"),
        ];
        let (report, _) = reconcile_orders(&oms, &dhan);
        // Only the TRADED order is counted (GIBBERISH skipped)
        assert_eq!(report.total_checked, 1);
    }

    #[test]
    fn all_unknown_statuses_yields_zero_checked() {
        let oms: HashMap<String, ManagedOrder> = HashMap::new();
        let dhan = vec![
            make_dhan_order("1", "UNKNOWN_1"),
            make_dhan_order("2", "UNKNOWN_2"),
        ];
        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 0);
        assert!(updates.is_empty());
    }

    #[test]
    fn terminal_oms_order_missing_from_dhan_not_ghost() {
        // Terminal (Traded, Rejected, Cancelled, Expired) orders missing from
        // Dhan response should NOT count as ghost orders.
        let mut oms = HashMap::new();
        oms.insert(
            "t1".to_owned(),
            make_managed_order("t1", OrderStatus::Traded),
        );
        oms.insert(
            "t2".to_owned(),
            make_managed_order("t2", OrderStatus::Rejected),
        );
        oms.insert(
            "t3".to_owned(),
            make_managed_order("t3", OrderStatus::Cancelled),
        );
        oms.insert(
            "t4".to_owned(),
            make_managed_order("t4", OrderStatus::Expired),
        );
        let dhan: Vec<DhanOrderResponse> = vec![];

        let (report, _) = reconcile_orders(&oms, &dhan);
        assert_eq!(
            report.missing_from_dhan, 0,
            "terminal orders should not be flagged as ghosts"
        );
    }

    #[test]
    fn reconciliation_report_default_all_zeros() {
        let report = ReconciliationReport::default();
        assert_eq!(report.total_checked, 0);
        assert_eq!(report.mismatches_found, 0);
        assert_eq!(report.missing_from_oms, 0);
        assert_eq!(report.missing_from_dhan, 0);
        assert!(report.mismatched_order_ids.is_empty());
    }
}
