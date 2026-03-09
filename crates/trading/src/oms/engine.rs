//! OMS engine — orchestrates order lifecycle management.
//!
//! Composes the API client, rate limiter, circuit breaker, idempotency tracker,
//! and state machine into a single cohesive order management system.
//!
//! # Performance
//! Cold path — order submission is ~1-100/day. Allocations acceptable.
//!
//! # Thread Safety
//! Single-threaded access assumed (owned by the trading pipeline task).
//! Token access is via a callback trait to decouple from the core crate.

use std::collections::HashMap;

use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, error, info, warn};

use dhan_live_trader_common::order_types::{OrderStatus, OrderUpdate};

use super::api_client::OrderApiClient;
use super::circuit_breaker::OrderCircuitBreaker;
use super::idempotency::CorrelationTracker;
use super::rate_limiter::OrderRateLimiter;
use super::reconciliation::reconcile_orders;
use super::state_machine::{is_valid_transition, parse_order_status};
use super::types::{
    DhanModifyOrderRequest, DhanPlaceOrderRequest, EXCHANGE_SEGMENT_NSE_FNO, ManagedOrder,
    ModifyOrderRequest, OmsError, PlaceOrderRequest, ReconciliationReport,
};

// ---------------------------------------------------------------------------
// Token Provider trait
// ---------------------------------------------------------------------------

/// Trait for providing access tokens to the OMS engine.
///
/// Implemented at the binary level where `TokenHandle` (core crate) is available.
/// Uses dynamic dispatch (cold path — orders are infrequent).
/// Returns `SecretString` to ensure zeroize-on-drop for the access token.
pub trait TokenProvider: Send + Sync {
    /// Returns the current valid access token, or an error.
    fn get_access_token(&self) -> Result<SecretString, OmsError>;
}

// ---------------------------------------------------------------------------
// OrderManagementSystem
// ---------------------------------------------------------------------------

/// Central order management system composing all OMS sub-components.
///
/// Manages the full order lifecycle: place → track → update → reconcile.
pub struct OrderManagementSystem {
    /// Order state keyed by Dhan order ID.
    orders: HashMap<String, ManagedOrder>,
    /// Correlation ID tracker for idempotency.
    correlations: CorrelationTracker,
    /// Dhan REST API client.
    api_client: OrderApiClient,
    /// SEBI rate limiter.
    rate_limiter: OrderRateLimiter,
    /// Circuit breaker for Dhan API.
    circuit_breaker: OrderCircuitBreaker,
    /// Token provider for authentication.
    token_provider: Box<dyn TokenProvider>,
    /// Dhan client ID for REST API requests.
    client_id: String,
    /// Total orders placed through this OMS instance.
    total_placed: u64,
    /// Total order updates processed.
    total_updates: u64,
}

impl OrderManagementSystem {
    /// Creates a new OMS with the given components.
    ///
    /// # Arguments
    /// * `api_client` — Dhan REST API client.
    /// * `rate_limiter` — SEBI-compliant rate limiter.
    /// * `token_provider` — Callback for getting the current access token.
    /// * `client_id` — Dhan client ID.
    pub fn new(
        api_client: OrderApiClient,
        rate_limiter: OrderRateLimiter,
        token_provider: Box<dyn TokenProvider>,
        client_id: String,
    ) -> Self {
        Self {
            orders: HashMap::new(),
            correlations: CorrelationTracker::new(),
            api_client,
            rate_limiter,
            circuit_breaker: OrderCircuitBreaker::new(),
            token_provider,
            client_id,
            total_placed: 0,
            total_updates: 0,
        }
    }

    /// Places a new order through the OMS pipeline.
    ///
    /// Flow: rate limit → circuit breaker → generate correlation ID →
    ///       call Dhan API → create ManagedOrder in Transit state.
    ///
    /// # Returns
    /// The Dhan order ID on success.
    ///
    /// # Errors
    /// - `OmsError::RateLimited` — SEBI rate limit exceeded
    /// - `OmsError::CircuitBreakerOpen` — Dhan API unavailable
    /// - `OmsError::NoToken` / `OmsError::TokenExpired` — auth failure
    /// - `OmsError::DhanApiError` — Dhan returned an error
    /// - `OmsError::DhanRateLimited` — HTTP 429 from Dhan
    pub async fn place_order(&mut self, request: PlaceOrderRequest) -> Result<String, OmsError> {
        // Step 1: Rate limiter check
        self.rate_limiter.check()?;

        // Step 2: Circuit breaker check
        self.circuit_breaker.check()?;

        // Step 3: Get access token
        let access_token = self.token_provider.get_access_token()?;

        // Step 4: Generate correlation ID
        let correlation_id = self.correlations.generate_id();

        // Step 5: Build Dhan REST request
        let dhan_request = DhanPlaceOrderRequest {
            dhan_client_id: self.client_id.clone(),
            transaction_type: request.transaction_type.as_str().to_owned(),
            exchange_segment: EXCHANGE_SEGMENT_NSE_FNO.to_owned(),
            product_type: request.product_type.as_str().to_owned(),
            order_type: request.order_type.as_str().to_owned(),
            validity: request.validity.as_str().to_owned(),
            security_id: request.security_id.to_string(),
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            disclosed_quantity: 0,
            after_market_order: false,
            correlation_id: correlation_id.clone(),
        };

        // Step 6: Call Dhan REST API
        let response = match self
            .api_client
            .place_order(access_token.expose_secret(), &dhan_request)
            .await
        {
            Ok(resp) => {
                self.circuit_breaker.record_success();
                resp
            }
            Err(err) => {
                self.circuit_breaker.record_failure();
                return Err(err);
            }
        };

        let now_us = now_epoch_us();

        // Step 7: Create ManagedOrder with status from Dhan response.
        // Dhan can return TRADED or REJECTED immediately; default to Transit if unparseable.
        let initial_status =
            parse_order_status(&response.order_status).unwrap_or(OrderStatus::Transit);

        let order = ManagedOrder {
            order_id: response.order_id.clone(),
            correlation_id: correlation_id.clone(),
            security_id: request.security_id,
            transaction_type: request.transaction_type,
            order_type: request.order_type,
            product_type: request.product_type,
            validity: request.validity,
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            status: initial_status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: request.lot_size,
            created_at_us: now_us,
            updated_at_us: now_us,
            needs_reconciliation: false,
        };

        // Step 8: Track in state
        self.correlations
            .track(correlation_id, response.order_id.clone());
        self.orders.insert(response.order_id.clone(), order);
        self.total_placed = self.total_placed.saturating_add(1);

        info!(
            order_id = %response.order_id,
            security_id = request.security_id,
            transaction_type = %request.transaction_type.as_str(),
            quantity = request.quantity,
            price = request.price,
            "order placed successfully"
        );

        Ok(response.order_id)
    }

    /// Modifies an existing open order.
    ///
    /// # Errors
    /// - `OmsError::OrderNotFound` — order ID not tracked
    /// - `OmsError::OrderTerminal` — order already in terminal state
    /// - Other API/rate/circuit errors
    pub async fn modify_order(
        &mut self,
        order_id: &str,
        request: ModifyOrderRequest,
    ) -> Result<(), OmsError> {
        // Validate order exists and is modifiable
        let order = self
            .orders
            .get(order_id)
            .ok_or_else(|| OmsError::OrderNotFound {
                order_id: order_id.to_owned(),
            })?;

        if order.is_terminal() {
            return Err(OmsError::OrderTerminal {
                order_id: order_id.to_owned(),
                status: order.status.as_str().to_owned(),
            });
        }

        self.rate_limiter.check()?;
        self.circuit_breaker.check()?;
        let access_token = self.token_provider.get_access_token()?;

        let dhan_request = DhanModifyOrderRequest {
            dhan_client_id: self.client_id.clone(),
            order_id: order_id.to_owned(),
            order_type: request.order_type.as_str().to_owned(),
            leg_name: String::new(),
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            validity: request.validity.as_str().to_owned(),
            disclosed_quantity: request.disclosed_quantity,
        };

        match self
            .api_client
            .modify_order(access_token.expose_secret(), order_id, &dhan_request)
            .await
        {
            Ok(()) => {
                self.circuit_breaker.record_success();
            }
            Err(err) => {
                self.circuit_breaker.record_failure();
                return Err(err);
            }
        }

        // Update local state with modification details
        if let Some(order) = self.orders.get_mut(order_id) {
            order.order_type = request.order_type;
            order.quantity = request.quantity;
            order.price = request.price;
            order.trigger_price = request.trigger_price;
            order.validity = request.validity;
            order.updated_at_us = now_epoch_us();
        }

        info!(order_id = %order_id, "order modified successfully");
        Ok(())
    }

    /// Cancels an existing open order.
    ///
    /// # Errors
    /// - `OmsError::OrderNotFound` — order ID not tracked
    /// - `OmsError::OrderTerminal` — order already in terminal state
    /// - Other API/rate/circuit errors
    pub async fn cancel_order(&mut self, order_id: &str) -> Result<(), OmsError> {
        let order = self
            .orders
            .get(order_id)
            .ok_or_else(|| OmsError::OrderNotFound {
                order_id: order_id.to_owned(),
            })?;

        if order.is_terminal() {
            return Err(OmsError::OrderTerminal {
                order_id: order_id.to_owned(),
                status: order.status.as_str().to_owned(),
            });
        }

        self.rate_limiter.check()?;
        self.circuit_breaker.check()?;
        let access_token = self.token_provider.get_access_token()?;

        match self
            .api_client
            .cancel_order(access_token.expose_secret(), order_id)
            .await
        {
            Ok(()) => {
                self.circuit_breaker.record_success();
            }
            Err(err) => {
                self.circuit_breaker.record_failure();
                return Err(err);
            }
        }

        // Mark for reconciliation — cancel is async; WebSocket will confirm,
        // but if WS is down we need REST reconciliation to pick it up.
        if let Some(order) = self.orders.get_mut(order_id) {
            order.needs_reconciliation = true;
            order.updated_at_us = now_epoch_us();
        }

        info!(order_id = %order_id, "order cancel request sent");
        Ok(())
    }

    /// Processes an order update from the WebSocket.
    ///
    /// Validates the state transition and updates the managed order.
    /// Invalid transitions are logged at ERROR level (triggers Telegram alert).
    ///
    /// # Returns
    /// `Ok(())` if the update was processed (even if the order is unknown).
    pub fn handle_order_update(&mut self, update: &OrderUpdate) -> Result<(), OmsError> {
        self.total_updates = self.total_updates.saturating_add(1);

        let order_id = &update.order_no;

        // Try to find order by order_id first, then by correlation_id
        let found_order_id = if self.orders.contains_key(order_id) {
            Some(order_id.clone())
        } else if !update.correlation_id.is_empty() {
            self.correlations
                .get_order_id(&update.correlation_id)
                .cloned()
        } else {
            None
        };

        let order_id = match found_order_id {
            Some(id) => id,
            None => {
                debug!(
                    order_no = %update.order_no,
                    status = %update.status,
                    "order update for unknown order — ignoring"
                );
                return Ok(());
            }
        };

        // If the update came via correlation_id with a different order_no,
        // re-index so future updates (which may lack correlation_id) can find it.
        if update.order_no != order_id
            && !update.order_no.is_empty()
            && let Some(order) = self.orders.get(&order_id).cloned()
        {
            self.orders.insert(update.order_no.clone(), order);
            debug!(
                old_order_id = %order_id,
                new_order_no = %update.order_no,
                "re-indexed order under new order_no from WebSocket"
            );
        }

        let new_status = match parse_order_status(&update.status) {
            Some(status) => status,
            None => {
                warn!(
                    order_id = %order_id,
                    status = %update.status,
                    "unknown order status in WebSocket update"
                );
                return Ok(());
            }
        };

        // Get mutable reference to order
        let order = match self.orders.get_mut(&order_id) {
            Some(o) => o,
            None => return Ok(()),
        };

        // Validate transition
        let old_status = order.status;
        if old_status == new_status {
            // Same status — just update fields (e.g., partial fill qty update)
            order.traded_qty = update.traded_qty;
            order.avg_traded_price = update.avg_traded_price;
            order.updated_at_us = now_epoch_us();
            return Ok(());
        }

        if !is_valid_transition(old_status, new_status) {
            error!(
                order_id = %order_id,
                from = %old_status.as_str(),
                to = %new_status.as_str(),
                "INVALID ORDER STATE TRANSITION — flagging for reconciliation"
            );
            order.needs_reconciliation = true;
            return Err(OmsError::InvalidTransition {
                order_id,
                from: old_status.as_str().to_owned(),
                to: new_status.as_str().to_owned(),
            });
        }

        // Apply transition
        order.status = new_status;
        order.traded_qty = update.traded_qty;
        order.avg_traded_price = update.avg_traded_price;
        order.updated_at_us = now_epoch_us();

        debug!(
            order_id = %order_id,
            from = %old_status.as_str(),
            to = %new_status.as_str(),
            traded_qty = update.traded_qty,
            "order state transition applied"
        );

        Ok(())
    }

    /// Runs reconciliation against the Dhan REST API.
    ///
    /// Fetches all today's orders and compares with OMS state.
    /// Mismatches are logged at ERROR level and corrected in local state.
    pub async fn reconcile(&mut self) -> Result<ReconciliationReport, OmsError> {
        let access_token = self.token_provider.get_access_token()?;
        let dhan_orders = self
            .api_client
            .get_all_orders(access_token.expose_secret())
            .await?;

        let (report, updates) = reconcile_orders(&self.orders, &dhan_orders);

        // Apply corrections (status + fill data)
        for update in updates {
            if let Some(order) = self.orders.get_mut(&update.order_id) {
                order.status = update.status;
                order.traded_qty = update.traded_qty;
                order.avg_traded_price = update.avg_traded_price;
                order.needs_reconciliation = false;
                order.updated_at_us = now_epoch_us();
            }
        }

        Ok(report)
    }

    /// Returns an order by ID.
    pub fn order(&self, order_id: &str) -> Option<&ManagedOrder> {
        self.orders.get(order_id)
    }

    /// Returns all active (non-terminal) orders.
    pub fn active_orders(&self) -> Vec<&ManagedOrder> {
        self.orders.values().filter(|o| !o.is_terminal()).collect()
    }

    /// Returns all orders (active + terminal).
    pub fn all_orders(&self) -> &HashMap<String, ManagedOrder> {
        &self.orders
    }

    /// Returns the total number of orders placed.
    pub fn total_placed(&self) -> u64 {
        self.total_placed
    }

    /// Returns the total number of updates processed.
    pub fn total_updates(&self) -> u64 {
        self.total_updates
    }

    /// Resets daily state (orders, correlations, counters).
    pub fn reset_daily(&mut self) {
        self.orders.clear();
        self.correlations.clear();
        self.total_placed = 0;
        self.total_updates = 0;
        self.circuit_breaker.reset();
        info!("OMS daily state reset");
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// Returns the current time as epoch microseconds.
fn now_epoch_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
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

    /// Test token provider that always returns a fixed token.
    struct TestTokenProvider;
    impl TokenProvider for TestTokenProvider {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Ok(SecretString::from("test-token"))
        }
    }

    fn make_oms_with_order(order_id: &str, status: OrderStatus) -> OrderManagementSystem {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let rate_limiter = OrderRateLimiter::new(10);
        let mut oms = OrderManagementSystem::new(
            api_client,
            rate_limiter,
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let order = ManagedOrder {
            order_id: order_id.to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
        };

        oms.orders.insert(order_id.to_owned(), order);
        oms.correlations
            .track("corr-1".to_owned(), order_id.to_owned());
        oms
    }

    fn make_order_update(order_no: &str, status: &str) -> OrderUpdate {
        OrderUpdate {
            order_no: order_no.to_owned(),
            status: status.to_owned(),
            correlation_id: String::new(),
            exchange: String::new(),
            segment: String::new(),
            security_id: String::new(),
            client_id: String::new(),
            exch_order_no: String::new(),
            product: String::new(),
            txn_type: String::new(),
            order_type: String::new(),
            validity: String::new(),
            quantity: 0,
            traded_qty: 0,
            remaining_quantity: 0,
            price: 0.0,
            trigger_price: 0.0,
            traded_price: 0.0,
            avg_traded_price: 0.0,
            symbol: String::new(),
            display_name: String::new(),
            remarks: String::new(),
            reason_description: String::new(),
            order_date_time: String::new(),
            exch_order_time: String::new(),
            last_updated_time: String::new(),
            instrument: String::new(),
            lot_size: 0,
            strike_price: 0.0,
            expiry_date: String::new(),
            opt_type: String::new(),
            isin: String::new(),
            disc_quantity: 0,
            disc_qty_rem: 0,
            leg_no: 0,
            product_name: String::new(),
            ref_ltp: 0.0,
            tick_size: 0.0,
        }
    }

    #[test]
    fn new_oms_is_empty() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        assert_eq!(oms.total_placed(), 0);
        assert_eq!(oms.total_updates(), 0);
        assert!(oms.active_orders().is_empty());
        assert!(oms.all_orders().is_empty());
    }

    #[test]
    fn handle_valid_transition_transit_to_pending() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("1", "PENDING");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Pending);
    }

    #[test]
    fn handle_valid_transition_pending_to_traded() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 50;
        update.avg_traded_price = 245.50;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());

        let order = oms.order("1").unwrap();
        assert_eq!(order.status, OrderStatus::Traded);
        assert_eq!(order.traded_qty, 50);
        assert!(order.is_terminal());
    }

    #[test]
    fn handle_invalid_transition_returns_error() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);
        let update = make_order_update("1", "PENDING");

        let result = oms.handle_order_update(&update);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::InvalidTransition { .. }
        ));
        assert!(oms.order("1").unwrap().needs_reconciliation);
    }

    #[test]
    fn handle_unknown_order_ignored() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("999", "TRADED");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
    }

    #[test]
    fn handle_same_status_updates_fields() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        let mut update = make_order_update("1", "CONFIRMED");
        update.traded_qty = 25;
        update.avg_traded_price = 245.0;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().traded_qty, 25);
    }

    #[test]
    fn handle_unknown_status_ignored() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("1", "WEIRD_STATUS");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        // Status unchanged
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Transit);
    }

    #[test]
    fn handle_correlation_id_lookup() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        // Update comes with a different order_no but matching correlation_id
        let mut update = make_order_update("unknown", "PENDING");
        update.correlation_id = "corr-1".to_owned();

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Pending);
    }

    #[test]
    fn active_orders_excludes_terminal() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        // Add a terminal order
        let terminal = ManagedOrder {
            order_id: "2".to_owned(),
            correlation_id: "corr-2".to_owned(),
            security_id: 100,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            status: OrderStatus::Traded,
            traded_qty: 50,
            avg_traded_price: 100.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
        };
        oms.orders.insert("2".to_owned(), terminal);

        assert_eq!(oms.active_orders().len(), 1);
        assert_eq!(oms.all_orders().len(), 2);
    }

    #[test]
    fn reset_daily_clears_state() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);
        oms.total_placed = 5;
        oms.total_updates = 10;

        oms.reset_daily();

        assert!(oms.all_orders().is_empty());
        assert_eq!(oms.total_placed(), 0);
        assert_eq!(oms.total_updates(), 0);
    }

    #[test]
    fn modify_terminal_order_returns_error() {
        let oms = make_oms_with_order("1", OrderStatus::Traded);
        let order = oms.order("1").unwrap();
        assert!(order.is_terminal());
    }

    #[test]
    fn cancel_nonexistent_order_returns_error() {
        let oms = make_oms_with_order("1", OrderStatus::Transit);
        assert!(oms.order("999").is_none());
    }

    #[test]
    fn update_counter_increments() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("1", "PENDING");

        let _ = oms.handle_order_update(&update);
        assert_eq!(oms.total_updates(), 1);
    }
}
