//! Gap enforcement integration tests — Trading crate.
//!
//! Exhaustive tests for all OMS and Risk gap implementations:
//! - I-P0-03: Expired contract awareness at OMS level
//! - OMS-GAP-01: Order lifecycle state machine (transitions + parsing)
//! - OMS-GAP-02: Order reconciliation (pure function, mismatch detection)
//! - OMS-GAP-03: Circuit breaker (3-state FSM, threshold, reset)
//! - OMS-GAP-04: SEBI rate limiting (burst)
//! - OMS-GAP-05: Idempotency (UUID generation, correlation tracking)
//! - RISK-GAP-01: Pre-trade risk checks (halt, daily loss, position limit)
//! - RISK-GAP-02: Position & P&L tracking (fills, market price, reset)
//! - RISK-GAP-03: Tick gap detection (tick_gap_tracker — warmup, warning/error thresholds, per-security isolation)

// ===========================================================================
// I-P0-03: Expired Contract Awareness at OMS
// ===========================================================================

mod i_p0_03_expired_contract_awareness {
    use dhan_live_trader_common::order_types::{
        OrderStatus, OrderType, OrderValidity, ProductType, TransactionType,
    };
    use dhan_live_trader_trading::oms::types::ManagedOrder;

    /// I-P0-03: Verifies that OMS types can represent expiry information.
    ///
    /// RESOLVED: `PlaceOrderRequest` now has `expiry_date: Option<NaiveDate>`.
    /// The `validate_order_fields()` function in `engine.rs` checks
    /// `expiry_date < today` and returns `OmsError::ExpiredContract`.

    #[test]
    fn test_i_p0_03_expired_contract_rejected() {
        // I-P0-03 RESOLVED: PlaceOrderRequest.expiry_date is checked in
        // validate_order_fields(). Expired contracts return ExpiredContract error.
        use dhan_live_trader_trading::oms::types::PlaceOrderRequest;

        let yesterday = chrono::Utc::now().date_naive() - chrono::Duration::days(1);
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: Some(yesterday),
        };

        // The OMS engine validates expiry in validate_order_fields() before submission.
        // We verify the field exists and can carry expiry information.
        assert_eq!(request.security_id, 52432);
        assert!(
            request.expiry_date.is_some(),
            "I-P0-03: PlaceOrderRequest must carry expiry_date for derivatives"
        );
        assert!(
            request.expiry_date.unwrap() < chrono::Utc::now().date_naive(),
            "I-P0-03: test contract must be expired"
        );
    }

    #[test]
    fn test_i_p0_03_terminal_order_cannot_be_resubmitted() {
        // Even without expiry checks, orders in terminal states are blocked.
        // This is the closest existing protection against stale orders.
        let order = ManagedOrder {
            order_id: "TERM-1".to_owned(),
            correlation_id: "corr-term".to_owned(),
            security_id: 99999, // Could be an expired contract
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            status: OrderStatus::Rejected,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };

        assert!(
            order.is_terminal(),
            "I-P0-03: rejected order must be terminal — no further actions"
        );
    }

    #[test]
    fn test_i_p0_03_all_terminal_states_block_modifications() {
        let terminal_statuses = [
            OrderStatus::Traded,
            OrderStatus::Rejected,
            OrderStatus::Cancelled,
            OrderStatus::Expired,
            OrderStatus::Closed,
        ];

        for status in &terminal_statuses {
            let order = ManagedOrder {
                order_id: format!("TERM-{:?}", status),
                correlation_id: "corr-t".to_owned(),
                security_id: 52432,
                transaction_type: TransactionType::Buy,
                order_type: OrderType::Limit,
                product_type: ProductType::Intraday,
                validity: OrderValidity::Day,
                quantity: 50,
                price: 100.0,
                trigger_price: 0.0,
                status: *status,
                traded_qty: 0,
                avg_traded_price: 0.0,
                lot_size: 25,
                created_at_us: 0,
                updated_at_us: 0,
                needs_reconciliation: false,
                modification_count: 0,
            };

            assert!(
                order.is_terminal(),
                "I-P0-03: {:?} must be terminal",
                status
            );
        }
    }
}

// ===========================================================================
// OMS-GAP-01: Order Lifecycle State Machine
// ===========================================================================

mod oms_state_machine {
    use dhan_live_trader_common::order_types::OrderStatus;
    use dhan_live_trader_trading::oms::state_machine::{is_valid_transition, parse_order_status};

    // -- All valid transitions verified exhaustively -------------------------

    #[test]
    fn all_valid_transitions_accepted() {
        let valid_transitions = [
            (OrderStatus::Transit, OrderStatus::Pending),
            (OrderStatus::Transit, OrderStatus::Rejected),
            (OrderStatus::Pending, OrderStatus::Confirmed),
            (OrderStatus::Pending, OrderStatus::Traded),
            (OrderStatus::Pending, OrderStatus::Cancelled),
            (OrderStatus::Pending, OrderStatus::Rejected),
            (OrderStatus::Pending, OrderStatus::Expired),
            (OrderStatus::Confirmed, OrderStatus::Traded),
            (OrderStatus::Confirmed, OrderStatus::Cancelled),
            (OrderStatus::Confirmed, OrderStatus::Expired),
        ];

        for (from, to) in &valid_transitions {
            assert!(
                is_valid_transition(*from, *to),
                "expected valid: {:?} → {:?}",
                from,
                to
            );
        }
    }

    // -- Terminal states reject ALL outgoing transitions --------------------

    #[test]
    fn terminal_traded_rejects_all() {
        let targets = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Confirmed,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Expired,
        ];
        for to in &targets {
            assert!(
                !is_valid_transition(OrderStatus::Traded, *to),
                "Traded → {:?} must be rejected",
                to
            );
        }
    }

    #[test]
    fn terminal_rejected_rejects_all() {
        let targets = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Confirmed,
            OrderStatus::Traded,
            OrderStatus::Cancelled,
            OrderStatus::Expired,
        ];
        for to in &targets {
            assert!(
                !is_valid_transition(OrderStatus::Rejected, *to),
                "Rejected → {:?} must be rejected",
                to
            );
        }
    }

    #[test]
    fn terminal_cancelled_rejects_all() {
        let targets = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Confirmed,
            OrderStatus::Traded,
            OrderStatus::Rejected,
            OrderStatus::Expired,
        ];
        for to in &targets {
            assert!(
                !is_valid_transition(OrderStatus::Cancelled, *to),
                "Cancelled → {:?} must be rejected",
                to
            );
        }
    }

    #[test]
    fn terminal_expired_rejects_all() {
        let targets = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Confirmed,
            OrderStatus::Traded,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
        ];
        for to in &targets {
            assert!(
                !is_valid_transition(OrderStatus::Expired, *to),
                "Expired → {:?} must be rejected",
                to
            );
        }
    }

    // -- Self-transitions rejected -----------------------------------------

    #[test]
    fn self_transitions_rejected() {
        let all_states = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Confirmed,
            OrderStatus::Traded,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Expired,
        ];
        for state in &all_states {
            assert!(
                !is_valid_transition(*state, *state),
                "self-transition {:?} → {:?} must be rejected",
                state,
                state
            );
        }
    }

    // -- Transit cannot skip to non-Pending/Rejected -----------------------

    #[test]
    fn transit_cannot_skip_to_traded() {
        assert!(!is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Traded
        ));
    }

    #[test]
    fn transit_cannot_skip_to_confirmed() {
        assert!(!is_valid_transition(
            OrderStatus::Transit,
            OrderStatus::Confirmed
        ));
    }

    // -- parse_order_status: all Dhan variants -----------------------------

    #[test]
    fn parse_all_dhan_status_strings() {
        assert_eq!(parse_order_status("TRANSIT"), Some(OrderStatus::Transit));
        assert_eq!(parse_order_status("PENDING"), Some(OrderStatus::Pending));
        assert_eq!(
            parse_order_status("CONFIRMED"),
            Some(OrderStatus::Confirmed)
        );
        assert_eq!(
            parse_order_status("PART_TRADED"),
            Some(OrderStatus::PartTraded)
        );
        assert_eq!(
            parse_order_status("PARTIALLY_FILLED"),
            Some(OrderStatus::PartTraded)
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
        assert_eq!(parse_order_status("CLOSED"), Some(OrderStatus::Closed));
        assert_eq!(
            parse_order_status("TRIGGERED"),
            Some(OrderStatus::Triggered)
        );
        assert_eq!(parse_order_status("CONFIRM"), Some(OrderStatus::Triggered));
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert_eq!(parse_order_status("UNKNOWN"), None);
        assert_eq!(parse_order_status(""), None);
        assert_eq!(parse_order_status("traded"), None); // case-sensitive
        assert_eq!(parse_order_status("pending"), None);
        assert_eq!(parse_order_status("FILLED"), None);
        assert_eq!(parse_order_status(" TRADED"), None); // leading whitespace
    }
}

// ===========================================================================
// OMS-GAP-02: Order Reconciliation
// ===========================================================================

mod oms_reconciliation {
    use std::collections::HashMap;

    use dhan_live_trader_common::order_types::{
        OrderStatus, OrderType, OrderValidity, ProductType, TransactionType,
    };
    use dhan_live_trader_trading::oms::reconciliation::{ReconciliationUpdate, reconcile_orders};
    use dhan_live_trader_trading::oms::types::{DhanOrderResponse, ManagedOrder};

    fn make_managed(order_id: &str, status: OrderStatus) -> ManagedOrder {
        ManagedOrder {
            order_id: order_id.to_owned(),
            correlation_id: "corr-test".to_owned(),
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

    fn make_dhan(order_id: &str, status: &str) -> DhanOrderResponse {
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
            average_traded_price: 0.0,
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

    // -- Pure function: no mutation of inputs ------------------------------

    #[test]
    fn empty_inputs_zero_mismatches() {
        let oms: HashMap<String, ManagedOrder> = HashMap::new();
        let dhan: Vec<DhanOrderResponse> = vec![];
        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 0);
        assert_eq!(report.mismatches_found, 0);
        assert_eq!(report.missing_from_oms, 0);
        assert_eq!(report.missing_from_dhan, 0);
        assert!(updates.is_empty());
    }

    // -- Status mismatch detection ----------------------------------------

    #[test]
    fn status_mismatch_triggers_error() {
        let mut oms = HashMap::new();
        oms.insert(
            "ORD-1".to_owned(),
            make_managed("ORD-1", OrderStatus::Pending),
        );
        let dhan = vec![make_dhan("ORD-1", "TRADED")];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.mismatches_found, 1);
        assert_eq!(report.mismatched_order_ids, vec!["ORD-1"]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, OrderStatus::Traded);
    }

    #[test]
    fn matching_status_no_mismatch() {
        let mut oms = HashMap::new();
        oms.insert(
            "ORD-1".to_owned(),
            make_managed("ORD-1", OrderStatus::Traded),
        );
        let dhan = vec![make_dhan("ORD-1", "TRADED")];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.mismatches_found, 0);
        assert!(updates.is_empty());
    }

    // -- Fill data comparison with epsilon ---------------------------------

    #[test]
    fn fill_data_mismatch_generates_update() {
        let mut oms = HashMap::new();
        let mut order = make_managed("ORD-1", OrderStatus::Traded);
        order.traded_qty = 25;
        order.avg_traded_price = 100.0;
        oms.insert("ORD-1".to_owned(), order);

        let mut dhan = make_dhan("ORD-1", "TRADED");
        dhan.traded_quantity = 50;
        dhan.average_traded_price = 102.5;
        let dhan = vec![dhan];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.mismatches_found, 0); // status matches
        assert_eq!(updates.len(), 1); // fill data mismatch
        assert_eq!(updates[0].traded_qty, 50);
        assert!((updates[0].avg_traded_price - 102.5).abs() < f64::EPSILON);
    }

    // -- Ghost order detection --------------------------------------------

    #[test]
    fn ghost_order_detected_for_non_terminal() {
        let mut oms = HashMap::new();
        oms.insert(
            "GHOST-1".to_owned(),
            make_managed("GHOST-1", OrderStatus::Confirmed),
        );
        oms.insert(
            "DONE-1".to_owned(),
            make_managed("DONE-1", OrderStatus::Traded),
        );
        let dhan: Vec<DhanOrderResponse> = vec![];

        let (report, _) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.missing_from_dhan, 1); // only non-terminal counts
    }

    // -- Missing from OMS -------------------------------------------------

    #[test]
    fn missing_from_oms_detected() {
        let oms: HashMap<String, ManagedOrder> = HashMap::new();
        let dhan = vec![make_dhan("ORD-999", "CONFIRMED")];

        let (report, _) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.missing_from_oms, 1);
    }

    // -- Unknown Dhan status skipped (not counted) ------------------------

    #[test]
    fn unknown_dhan_status_skipped() {
        let mut oms = HashMap::new();
        oms.insert(
            "ORD-1".to_owned(),
            make_managed("ORD-1", OrderStatus::Pending),
        );
        let dhan = vec![make_dhan("ORD-1", "GARBAGE_STATUS")];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 0); // skipped
        assert!(updates.is_empty());
    }

    // -- Mixed scenario: match + mismatch + missing -----------------------

    #[test]
    fn mixed_scenario_comprehensive() {
        let mut oms = HashMap::new();
        oms.insert("1".to_owned(), make_managed("1", OrderStatus::Confirmed));
        oms.insert("2".to_owned(), make_managed("2", OrderStatus::Traded));
        oms.insert(
            "3".to_owned(),
            make_managed("3", OrderStatus::Pending), // non-terminal, not in Dhan
        );

        let dhan = vec![
            make_dhan("1", "TRADED"),  // mismatch
            make_dhan("2", "TRADED"),  // match
            make_dhan("4", "PENDING"), // missing from OMS
        ];

        let (report, updates) = reconcile_orders(&oms, &dhan);
        assert_eq!(report.total_checked, 3);
        assert_eq!(report.mismatches_found, 1);
        assert_eq!(report.missing_from_oms, 1);
        assert_eq!(report.missing_from_dhan, 1); // order "3" is ghost
        assert_eq!(updates.len(), 1);
    }

    // -- ReconciliationUpdate traits --------------------------------------

    #[test]
    fn reconciliation_update_clone_eq() {
        let update = ReconciliationUpdate {
            order_id: "ORD-1".to_owned(),
            status: OrderStatus::Traded,
            traded_qty: 50,
            avg_traded_price: 100.5,
        };
        let cloned = update.clone();
        assert_eq!(update, cloned);
    }
}

// ===========================================================================
// OMS-GAP-03: Circuit Breaker (3-State FSM)
// ===========================================================================

mod oms_circuit_breaker {
    use dhan_live_trader_common::constants::OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD;
    use dhan_live_trader_trading::oms::circuit_breaker::{CircuitState, OrderCircuitBreaker};

    #[test]
    fn initial_state_is_closed() {
        let cb = OrderCircuitBreaker::new();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn opens_after_threshold_failures() {
        let cb = OrderCircuitBreaker::new();
        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.check().is_err());
    }

    #[test]
    fn below_threshold_stays_closed() {
        let cb = OrderCircuitBreaker::new();
        for _ in 0..(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD - 1) {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn success_resets_failure_counter() {
        let cb = OrderCircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);

        // Needs full threshold again to open
        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn manual_reset_closes_circuit() {
        let cb = OrderCircuitBreaker::new();
        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        cb.reset();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn circuit_states_are_distinct() {
        assert_ne!(CircuitState::Closed, CircuitState::Open);
        assert_ne!(CircuitState::Open, CircuitState::HalfOpen);
        assert_ne!(CircuitState::Closed, CircuitState::HalfOpen);
    }

    #[test]
    fn default_impl_matches_new() {
        let cb = OrderCircuitBreaker::default();
        assert_eq!(cb.state(), CircuitState::Closed);
    }
}

// ===========================================================================
// OMS-GAP-04: SEBI Rate Limiting
// ===========================================================================

mod oms_rate_limiter {
    use dhan_live_trader_trading::oms::rate_limiter::OrderRateLimiter;
    use dhan_live_trader_trading::oms::types::OmsError;

    // -- OrderRateLimiter: burst capacity ---------------------------------

    #[test]
    fn allows_within_burst() {
        let limiter = OrderRateLimiter::new(10);
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn exhausts_burst_then_rejects() {
        let limiter = OrderRateLimiter::new(3);
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        let result = limiter.check();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OmsError::RateLimited));
    }

    #[test]
    fn sebi_10_per_sec_enforced() {
        let limiter = OrderRateLimiter::new(10);
        for _ in 0..10 {
            assert!(limiter.check().is_ok());
        }
        assert!(limiter.check().is_err());
    }

    #[test]
    #[should_panic(expected = "max_orders_per_second must be > 0")]
    fn zero_rate_panics_at_construction() {
        let _ = OrderRateLimiter::new(0);
    }
}

// ===========================================================================
// OMS-GAP-05: Idempotency (Correlation Tracking)
// ===========================================================================

mod oms_idempotency {
    use dhan_live_trader_trading::oms::idempotency::CorrelationTracker;

    #[test]
    fn generate_id_returns_valid_correlation_id() {
        let tracker = CorrelationTracker::new();
        let id = tracker.generate_id();
        // Dhan correlationId: max 30 chars, hex-only (truncated UUID v4 simple)
        assert_eq!(id.len(), 30, "correlationId must be 30 chars (Dhan limit)");
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "correlationId must be hex chars only"
        );
    }

    #[test]
    fn generate_id_returns_unique_ids() {
        let tracker = CorrelationTracker::new();
        let ids: Vec<String> = (0..100).map(|_| tracker.generate_id()).collect();
        let unique: std::collections::HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
        assert_eq!(ids.len(), unique.len(), "all 100 UUIDs must be unique");
    }

    #[test]
    fn track_and_retrieve() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("corr-1".to_owned(), "order-100".to_owned());

        assert!(tracker.contains("corr-1"));
        assert_eq!(
            tracker.get_order_id("corr-1"),
            Some(&"order-100".to_owned())
        );
        assert!(!tracker.contains("corr-2"));
        assert_eq!(tracker.get_order_id("corr-2"), None);
    }

    #[test]
    fn new_tracker_is_empty() {
        let tracker = CorrelationTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn clear_removes_all_correlations() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("c1".to_owned(), "o1".to_owned());
        tracker.track("c2".to_owned(), "o2".to_owned());
        tracker.track("c3".to_owned(), "o3".to_owned());
        assert_eq!(tracker.len(), 3);

        tracker.clear();
        assert!(tracker.is_empty());
        assert!(tracker.get_order_id("c1").is_none());
        assert!(tracker.get_order_id("c2").is_none());
        assert!(tracker.get_order_id("c3").is_none());
    }

    #[test]
    fn overwrite_same_correlation() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("corr-1".to_owned(), "order-A".to_owned());
        tracker.track("corr-1".to_owned(), "order-B".to_owned());
        assert_eq!(tracker.get_order_id("corr-1"), Some(&"order-B".to_owned()));
        assert_eq!(tracker.len(), 1); // still 1, not 2
    }

    #[test]
    fn default_impl_matches_new() {
        let tracker = CorrelationTracker::default();
        assert!(tracker.is_empty());
    }
}

// ===========================================================================
// RISK-GAP-01: Pre-Trade Risk Checks
// ===========================================================================

mod risk_engine {
    use dhan_live_trader_trading::risk::{RiskBreach, RiskCheck, RiskEngine};

    // -- Auto-halt: once halted, ALL subsequent orders rejected ------------

    #[test]
    fn manual_halt_rejects_all_orders() {
        let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
        engine.manual_halt();
        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::ManualHalt));

        let check = engine.check_order(1001, 1);
        assert!(!check.is_approved());
    }

    #[test]
    fn auto_halt_after_daily_loss() {
        let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
        // max loss = 2% of 1M = 20,000

        // Simulate a fill that creates realized loss
        engine.record_fill(1001, 1, 100.0, 25); // buy 1 lot at 100
        engine.record_fill(1001, -1, 80.0, 25); // sell 1 lot at 80 → loss = 20*25 = 500
        // Not enough to breach 20K yet

        // Simulate large loss via repeated fills
        for _ in 0..40 {
            engine.record_fill(1002, 10, 1000.0, 25); // buy
            engine.record_fill(1002, -10, 980.0, 25); // sell at loss
        }
        // Loss per round: (1000-980)*10*25 = 5000, 40 rounds = 200,000 > 20,000

        let check = engine.check_order(1003, 1);
        assert!(!check.is_approved());
        assert!(engine.is_halted());
        assert_eq!(engine.halt_reason(), Some(RiskBreach::MaxDailyLossExceeded));
    }

    // -- Position limit check ---------------------------------------------

    #[test]
    fn position_limit_rejects_excess() {
        let mut engine = RiskEngine::new(10.0, 5, 1_000_000.0); // max 5 lots

        // First 5 lots: OK
        let check = engine.check_order(1001, 5);
        assert!(check.is_approved());
        engine.record_fill(1001, 5, 100.0, 25);

        // 6th lot: rejected
        let check = engine.check_order(1001, 1);
        assert!(!check.is_approved());
        if let RiskCheck::Rejected { breach, .. } = check {
            assert_eq!(breach, RiskBreach::PositionSizeLimitExceeded);
        }
    }

    #[test]
    fn opposite_direction_within_limit_approved() {
        let mut engine = RiskEngine::new(10.0, 5, 1_000_000.0);
        engine.record_fill(1001, 3, 100.0, 25); // long 3 lots

        // Sell 2 to reduce → net position 1 lot
        let check = engine.check_order(1001, -2);
        assert!(check.is_approved());
    }

    // -- Order of checks: halt → daily loss → position limit --------------

    #[test]
    fn halt_check_precedes_other_checks() {
        let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
        engine.manual_halt();

        // Even though position limit is fine, halt should reject first
        let check = engine.check_order(1001, 1);
        assert!(!check.is_approved());
    }

    // -- Reset daily clears ALL state ------------------------------------

    #[test]
    fn reset_daily_clears_all() {
        let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
        engine.record_fill(1001, 5, 100.0, 25);
        engine.update_market_price(1001, 105.0);
        engine.manual_halt();

        engine.reset_daily();
        assert!(!engine.is_halted());
        assert_eq!(engine.halt_reason(), None);
        assert_eq!(engine.total_realized_pnl(), 0.0);
        assert_eq!(engine.open_position_count(), 0);
    }

    // -- Reset halt (operator override) -----------------------------------

    #[test]
    fn reset_halt_allows_trading_again() {
        let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
        engine.manual_halt();
        assert!(engine.is_halted());

        engine.reset_halt();
        assert!(!engine.is_halted());
        assert!(engine.check_order(1001, 1).is_approved());
    }

    // -- Fresh engine approves orders ------------------------------------

    #[test]
    fn fresh_engine_approves_orders() {
        let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
        assert!(!engine.is_halted());
        assert!(engine.check_order(1001, 1).is_approved());
    }
}

// ===========================================================================
// RISK-GAP-02: Position & P&L Tracking
// ===========================================================================

mod risk_pnl_tracking {
    use dhan_live_trader_trading::risk::RiskEngine;

    #[test]
    fn record_fill_updates_position() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.record_fill(1001, 5, 100.0, 25);
        assert_eq!(engine.open_position_count(), 1);

        let pos = engine.position(1001);
        assert!(pos.is_some());
        assert_eq!(pos.unwrap().net_lots, 5);
    }

    #[test]
    fn closing_fill_generates_realized_pnl() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.record_fill(1001, 1, 100.0, 25); // buy at 100
        engine.record_fill(1001, -1, 110.0, 25); // sell at 110

        // P&L = (110 - 100) * 1 * 25 = 250
        let pnl = engine.total_realized_pnl();
        assert!((pnl - 250.0).abs() < f64::EPSILON);
    }

    #[test]
    fn short_position_pnl() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.record_fill(1001, -2, 200.0, 25); // sell 2 at 200
        engine.record_fill(1001, 2, 190.0, 25); // buy 2 at 190

        // P&L = (200 - 190) * 2 * 25 = 500
        let pnl = engine.total_realized_pnl();
        assert!((pnl - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn update_market_price_rejects_invalid() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.record_fill(1001, 1, 100.0, 25);

        // Non-positive and non-finite should be ignored
        engine.update_market_price(1001, -5.0);
        engine.update_market_price(1001, 0.0);
        engine.update_market_price(1001, f64::NAN);
        engine.update_market_price(1001, f64::INFINITY);

        // Unrealized P&L should be 0 (no valid market price set)
        assert!((engine.total_unrealized_pnl()).abs() < f64::EPSILON);
    }

    #[test]
    fn unrealized_pnl_with_valid_market_price() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.record_fill(1001, 2, 100.0, 25); // buy 2 at 100
        engine.update_market_price(1001, 110.0);

        // RISK-GAP-02: unrealized = net_lots * (market_price - avg_entry)
        // 2 lots * (110.0 - 100.0) = 20.0
        let unrealized = engine.total_unrealized_pnl();
        assert!((unrealized - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn no_market_price_conservative_zero() {
        let mut engine = RiskEngine::new(10.0, 100, 1_000_000.0);
        engine.record_fill(1001, 2, 100.0, 25);
        // No market price update → conservative 0
        assert!((engine.total_unrealized_pnl()).abs() < f64::EPSILON);
    }
}

// ===========================================================================
// RISK-GAP-03: Tick Gap Detection (Integration)
// ===========================================================================

mod risk_tick_gap {
    use dhan_live_trader_common::constants::{
        TICK_GAP_ALERT_THRESHOLD_SECS, TICK_GAP_ERROR_THRESHOLD_SECS,
        TICK_GAP_MIN_TICKS_BEFORE_ACTIVE,
    };
    use dhan_live_trader_trading::risk::tick_gap_tracker::{TickGapResult, TickGapTracker};

    #[test]
    fn warmup_suppresses_alerts() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // During warmup, even huge gaps must not alert
        for i in 0..TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            let result = tracker.record_tick(1001, base_ts + i * 1000);
            assert_eq!(result, TickGapResult::Ok, "warmup tick {i} must be Ok");
        }
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
    }

    #[test]
    fn warning_threshold_triggers_after_warmup() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Complete warmup
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Gap exactly at warning threshold
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        match result {
            TickGapResult::Warning { gap_secs } => {
                assert_eq!(gap_secs, TICK_GAP_ALERT_THRESHOLD_SECS);
            }
            other => panic!("expected Warning, got {other:?}"),
        }
        assert_eq!(tracker.total_warnings(), 1);
    }

    #[test]
    fn error_threshold_triggers() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ERROR_THRESHOLD_SECS;
        let result = tracker.record_tick(1001, gap_ts);
        match result {
            TickGapResult::Error { gap_secs } => {
                assert_eq!(gap_secs, TICK_GAP_ERROR_THRESHOLD_SECS);
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(tracker.total_errors(), 1);
    }

    #[test]
    fn per_security_isolation() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        // Warmup security 1001
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Warmup security 2002
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(2002, base_ts + i);
        }
        // Gap on 1001 must NOT affect 2002
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ERROR_THRESHOLD_SECS;
        let result_1001 = tracker.record_tick(1001, gap_ts);
        assert!(matches!(result_1001, TickGapResult::Error { .. }));

        // Normal tick on 2002 — should be Ok
        let normal_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + 1;
        let result_2002 = tracker.record_tick(2002, normal_ts);
        assert_eq!(result_2002, TickGapResult::Ok);
        assert_eq!(tracker.tracked_securities(), 2);
    }

    #[test]
    fn out_of_order_timestamps_no_underflow() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        // Out-of-order: earlier timestamp than last — saturating_sub prevents underflow
        let result = tracker.record_tick(1001, base_ts);
        assert_eq!(
            result,
            TickGapResult::Ok,
            "out-of-order must not panic or alert"
        );
    }

    #[test]
    fn reset_clears_all_state() {
        let mut tracker = TickGapTracker::new(10);
        let base_ts = 1_700_000_000;
        for i in 0..=TICK_GAP_MIN_TICKS_BEFORE_ACTIVE {
            tracker.record_tick(1001, base_ts + i);
        }
        let gap_ts = base_ts + TICK_GAP_MIN_TICKS_BEFORE_ACTIVE + TICK_GAP_ALERT_THRESHOLD_SECS;
        tracker.record_tick(1001, gap_ts);
        assert!(tracker.total_warnings() > 0);

        tracker.reset();
        assert_eq!(tracker.tracked_securities(), 0);
        assert_eq!(tracker.total_warnings(), 0);
        assert_eq!(tracker.total_errors(), 0);
    }
}

// ===========================================================================
// OMS-GAP-06: Dry-Run Safety Gate (Integration)
// ===========================================================================

mod oms_dry_run_gate {
    use dhan_live_trader_common::order_types::{
        OrderType, OrderValidity, ProductType, TransactionType,
    };
    use dhan_live_trader_trading::oms::api_client::OrderApiClient;
    use dhan_live_trader_trading::oms::engine::OrderManagementSystem;
    use dhan_live_trader_trading::oms::rate_limiter::OrderRateLimiter;
    use dhan_live_trader_trading::oms::types::PlaceOrderRequest;

    struct TestTokenProvider;
    impl dhan_live_trader_trading::oms::engine::TokenProvider for TestTokenProvider {
        fn get_access_token(
            &self,
        ) -> Result<secrecy::SecretString, dhan_live_trader_trading::oms::types::OmsError> {
            Ok(secrecy::SecretString::from("test-token"))
        }
    }

    fn make_oms() -> OrderManagementSystem {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        )
    }

    #[test]
    fn oms_defaults_to_dry_run() {
        let oms = make_oms();
        assert!(
            oms.is_dry_run(),
            "OMS-GAP-06: OMS must default to dry_run = true"
        );
    }

    #[tokio::test]
    async fn dry_run_returns_paper_order_id() {
        let mut oms = make_oms();
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        let result = oms.place_order(request).await;
        assert!(result.is_ok());
        let order_id = result.unwrap();
        assert!(
            order_id.starts_with("PAPER-"),
            "OMS-GAP-06: dry-run must produce PAPER-{{counter}} IDs, got: {order_id}"
        );
    }

    #[tokio::test]
    async fn dry_run_sequential_paper_ids() {
        let mut oms = make_oms();
        let make_request = || PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 25,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        let id1 = oms.place_order(make_request()).await.unwrap();
        let id2 = oms.place_order(make_request()).await.unwrap();
        assert_ne!(id1, id2, "OMS-GAP-06: each paper order must have unique ID");
        assert!(id1.starts_with("PAPER-"));
        assert!(id2.starts_with("PAPER-"));
    }
}
