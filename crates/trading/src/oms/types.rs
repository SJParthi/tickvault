//! OMS types — managed order, request/response structs, and error types.
//!
//! These types are OMS-internal. The external Dhan types live in `common::order_types`.
//! REST API request/response types use camelCase serde to match Dhan's REST format.

use serde::{Deserialize, Serialize};

use dhan_live_trader_common::order_types::{
    OrderStatus, OrderType, OrderValidity, ProductType, TransactionType,
};

// ---------------------------------------------------------------------------
// Managed Order — internal OMS representation
// ---------------------------------------------------------------------------

/// Internal order record tracked by the OMS throughout its lifecycle.
///
/// One `ManagedOrder` per order placed through the system. Updated by
/// WebSocket order updates and REST reconciliation.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedOrder {
    /// Dhan order ID (returned from place response).
    pub order_id: String,
    /// Our UUID v4 correlation ID (idempotency key).
    pub correlation_id: String,
    /// Dhan security identifier.
    pub security_id: u32,
    /// Buy or sell.
    pub transaction_type: TransactionType,
    /// Order execution type (LIMIT, MARKET, etc.).
    pub order_type: OrderType,
    /// Product type (INTRADAY, CNC, etc.).
    pub product_type: ProductType,
    /// Order validity (DAY, IOC).
    pub validity: OrderValidity,
    /// Total order quantity in lots.
    pub quantity: i64,
    /// Limit price (0.0 for market orders).
    pub price: f64,
    /// Trigger price for stop-loss orders.
    pub trigger_price: f64,
    /// Current order status.
    pub status: OrderStatus,
    /// Quantity filled so far.
    pub traded_qty: i64,
    /// Volume-weighted average fill price.
    pub avg_traded_price: f64,
    /// Lot size for this instrument (for risk engine integration).
    pub lot_size: u32,
    /// Creation timestamp (epoch microseconds).
    pub created_at_us: i64,
    /// Last update timestamp (epoch microseconds).
    pub updated_at_us: i64,
    /// Whether this order needs reconciliation with Dhan REST API.
    pub needs_reconciliation: bool,
}

impl ManagedOrder {
    /// Returns true if the order is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Traded
                | OrderStatus::Cancelled
                | OrderStatus::Rejected
                | OrderStatus::Expired
        )
    }
}

// ---------------------------------------------------------------------------
// Place Order Request
// ---------------------------------------------------------------------------

/// Request to place a new order through the OMS.
///
/// Contains all fields needed to submit to the Dhan REST API.
#[derive(Debug, Clone)]
pub struct PlaceOrderRequest {
    /// Dhan security identifier.
    pub security_id: u32,
    /// Buy or sell.
    pub transaction_type: TransactionType,
    /// Order execution type.
    pub order_type: OrderType,
    /// Product type.
    pub product_type: ProductType,
    /// Order validity.
    pub validity: OrderValidity,
    /// Total order quantity in lots.
    pub quantity: i64,
    /// Limit price (0.0 for market orders).
    pub price: f64,
    /// Trigger price for stop-loss orders (0.0 if not applicable).
    pub trigger_price: f64,
    /// Lot size for risk engine integration.
    pub lot_size: u32,
}

// ---------------------------------------------------------------------------
// Modify Order Request
// ---------------------------------------------------------------------------

/// Request to modify an existing open order.
#[derive(Debug, Clone)]
pub struct ModifyOrderRequest {
    /// New order type (may differ from original).
    pub order_type: OrderType,
    /// New quantity.
    pub quantity: i64,
    /// New price.
    pub price: f64,
    /// New trigger price.
    pub trigger_price: f64,
    /// New validity.
    pub validity: OrderValidity,
    /// Disclosed quantity (0 = fully disclosed).
    pub disclosed_quantity: i64,
}

// ---------------------------------------------------------------------------
// Dhan REST API request/response types (camelCase JSON)
// ---------------------------------------------------------------------------

/// Dhan REST API place order request body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanPlaceOrderRequest {
    pub dhan_client_id: String,
    pub transaction_type: String,
    pub exchange_segment: String,
    pub product_type: String,
    pub order_type: String,
    pub validity: String,
    pub security_id: String,
    pub quantity: i64,
    pub price: f64,
    pub trigger_price: f64,
    pub disclosed_quantity: i64,
    pub after_market_order: bool,
    pub correlation_id: String,
}

/// Dhan REST API place order response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanPlaceOrderResponse {
    pub order_id: String,
    pub order_status: String,
    #[serde(default)]
    pub correlation_id: String,
}

/// Dhan REST API modify order request body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanModifyOrderRequest {
    pub dhan_client_id: String,
    pub order_id: String,
    pub order_type: String,
    /// Leg name — empty string for regular orders, required by Dhan API.
    pub leg_name: String,
    pub quantity: i64,
    pub price: f64,
    pub trigger_price: f64,
    pub validity: String,
    pub disclosed_quantity: i64,
}

/// Dhan REST API order response (GET /orders/{id} and list items).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanOrderResponse {
    #[serde(default)]
    pub order_id: String,
    #[serde(default)]
    pub correlation_id: String,
    #[serde(default)]
    pub order_status: String,
    #[serde(default)]
    pub transaction_type: String,
    #[serde(default)]
    pub exchange_segment: String,
    #[serde(default)]
    pub product_type: String,
    #[serde(default)]
    pub order_type: String,
    #[serde(default)]
    pub validity: String,
    #[serde(default)]
    pub security_id: String,
    #[serde(default)]
    pub quantity: i64,
    #[serde(default)]
    pub price: f64,
    #[serde(default)]
    pub trigger_price: f64,
    #[serde(default)]
    pub traded_quantity: i64,
    #[serde(default)]
    pub traded_price: f64,
    #[serde(default)]
    pub remaining_quantity: i64,
    #[serde(default)]
    pub filled_qty: i64,
    #[serde(default)]
    pub average_traded_price: f64,
    #[serde(default)]
    pub exchange_order_id: String,
    #[serde(default)]
    pub exchange_time: String,
    #[serde(default)]
    pub create_time: String,
    #[serde(default)]
    pub update_time: String,
    #[serde(default)]
    pub rejection_reason: String,
    #[serde(default)]
    pub tag: String,
    /// Dhan OMS error code (e.g., "DH-906" for rejection diagnostics).
    #[serde(default)]
    pub oms_error_code: String,
    /// Dhan OMS error description (human-readable rejection reason).
    #[serde(default)]
    pub oms_error_description: String,
    /// Trading symbol (e.g., "NIFTY-Mar2026-24500-CE").
    #[serde(default)]
    pub trading_symbol: String,
    /// Derivative expiry date.
    #[serde(default)]
    pub drv_expiry_date: String,
    /// Derivative option type ("CALL", "PUT").
    #[serde(default)]
    pub drv_option_type: String,
    /// Derivative strike price.
    #[serde(default)]
    pub drv_strike_price: f64,
}

/// Dhan REST API positions response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanPositionResponse {
    #[serde(default)]
    pub dhan_client_id: String,
    #[serde(default)]
    pub security_id: String,
    #[serde(default)]
    pub exchange_segment: String,
    #[serde(default)]
    pub product_type: String,
    #[serde(default)]
    pub position_type: String,
    #[serde(default)]
    pub buy_qty: i64,
    #[serde(default)]
    pub sell_qty: i64,
    #[serde(default)]
    pub net_qty: i64,
    #[serde(default)]
    pub buy_avg: f64,
    #[serde(default)]
    pub sell_avg: f64,
    #[serde(default)]
    pub realized_profit: f64,
    #[serde(default)]
    pub unrealized_profit: f64,
    /// Trading symbol (e.g., "NIFTY-Mar2026-24500-CE").
    #[serde(default)]
    pub trading_symbol: String,
    /// Cost price.
    #[serde(default)]
    pub cost_price: f64,
    /// Lot multiplier.
    #[serde(default)]
    pub multiplier: i64,
    /// Derivative expiry date.
    #[serde(default)]
    pub drv_expiry_date: String,
    /// Derivative option type ("CALL", "PUT").
    #[serde(default)]
    pub drv_option_type: String,
    /// Derivative strike price.
    #[serde(default)]
    pub drv_strike_price: f64,
}

// ---------------------------------------------------------------------------
// OMS Error
// ---------------------------------------------------------------------------

/// Errors from OMS operations.
#[derive(Debug, thiserror::Error)]
pub enum OmsError {
    /// Risk engine rejected the order.
    #[error("risk check rejected: {reason}")]
    RiskRejected { reason: String },

    /// Rate limiter throttled the order.
    #[error("rate limited: SEBI order rate exceeded")]
    RateLimited,

    /// Circuit breaker is open (Dhan API unreachable).
    #[error("circuit breaker open: Dhan API temporarily unavailable")]
    CircuitBreakerOpen,

    /// Order not found in OMS state.
    #[error("order not found: {order_id}")]
    OrderNotFound { order_id: String },

    /// Order is in a terminal state and cannot be modified/cancelled.
    #[error("order {order_id} is in terminal state {status}")]
    OrderTerminal { order_id: String, status: String },

    /// Invalid state transition detected.
    #[error("invalid transition for order {order_id}: {from} -> {to}")]
    InvalidTransition {
        order_id: String,
        from: String,
        to: String,
    },

    /// Dhan REST API returned an error.
    #[error("dhan API error: {status_code} {message}")]
    DhanApiError { status_code: u16, message: String },

    /// Dhan returned HTTP 429 (rate limited at broker).
    #[error("dhan rate limited (HTTP 429, DH-300): back off, do NOT retry")]
    DhanRateLimited,

    /// No authentication token available.
    #[error("no authentication token available")]
    NoToken,

    /// Authentication token expired.
    #[error("authentication token expired")]
    TokenExpired,

    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    HttpError(String),

    /// JSON serialization/deserialization failed.
    #[error("JSON error: {0}")]
    JsonError(String),
}

// ---------------------------------------------------------------------------
// Reconciliation Report
// ---------------------------------------------------------------------------

/// Summary of a reconciliation pass.
#[derive(Debug, Default)]
pub struct ReconciliationReport {
    /// Number of orders checked.
    pub total_checked: u32,
    /// Number of orders with status mismatches (corrected).
    pub mismatches_found: u32,
    /// Number of orders found on Dhan but missing from OMS.
    pub missing_from_oms: u32,
    /// Number of non-terminal OMS orders not found on Dhan (ghost orders).
    pub missing_from_dhan: u32,
    /// Order IDs that had mismatches.
    pub mismatched_order_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Exchange Segment constant
// ---------------------------------------------------------------------------

/// Exchange segment for NSE F&O (the only segment this system trades).
pub const EXCHANGE_SEGMENT_NSE_FNO: &str = "NSE_FNO";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_order_terminal_states() {
        let mut order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "c1".to_owned(),
            security_id: 100,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            status: OrderStatus::Transit,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
        };

        assert!(!order.is_terminal());

        order.status = OrderStatus::Pending;
        assert!(!order.is_terminal());

        order.status = OrderStatus::Confirmed;
        assert!(!order.is_terminal());

        order.status = OrderStatus::Traded;
        assert!(order.is_terminal());

        order.status = OrderStatus::Cancelled;
        assert!(order.is_terminal());

        order.status = OrderStatus::Rejected;
        assert!(order.is_terminal());

        order.status = OrderStatus::Expired;
        assert!(order.is_terminal());
    }

    #[test]
    fn oms_error_display_variants() {
        let err = OmsError::RiskRejected {
            reason: "max loss".to_owned(),
        };
        assert!(err.to_string().contains("risk check rejected"));

        let err = OmsError::RateLimited;
        assert!(err.to_string().contains("rate limited"));

        let err = OmsError::CircuitBreakerOpen;
        assert!(err.to_string().contains("circuit breaker"));

        let err = OmsError::OrderNotFound {
            order_id: "123".to_owned(),
        };
        assert!(err.to_string().contains("123"));

        let err = OmsError::InvalidTransition {
            order_id: "1".to_owned(),
            from: "TRADED".to_owned(),
            to: "PENDING".to_owned(),
        };
        assert!(err.to_string().contains("TRADED"));
        assert!(err.to_string().contains("PENDING"));

        let err = OmsError::DhanRateLimited;
        assert!(err.to_string().contains("429"));
    }

    #[test]
    fn reconciliation_report_default() {
        let report = ReconciliationReport::default();
        assert_eq!(report.total_checked, 0);
        assert_eq!(report.mismatches_found, 0);
        assert_eq!(report.missing_from_oms, 0);
        assert_eq!(report.missing_from_dhan, 0);
        assert!(report.mismatched_order_ids.is_empty());
    }

    #[test]
    fn dhan_place_order_request_serializes_camel_case() {
        let req = DhanPlaceOrderRequest {
            dhan_client_id: "100".to_owned(),
            transaction_type: "BUY".to_owned(),
            exchange_segment: EXCHANGE_SEGMENT_NSE_FNO.to_owned(),
            product_type: "INTRADAY".to_owned(),
            order_type: "LIMIT".to_owned(),
            validity: "DAY".to_owned(),
            security_id: "52432".to_owned(),
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            disclosed_quantity: 0,
            after_market_order: false,
            correlation_id: "uuid-1".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("dhanClientId"));
        assert!(json.contains("transactionType"));
        assert!(json.contains("exchangeSegment"));
        assert!(json.contains("correlationId"));
        assert!(!json.contains("dhan_client_id"));
    }

    #[test]
    fn dhan_place_order_response_deserializes() {
        let json = r#"{"orderId":"123","orderStatus":"TRANSIT","correlationId":"uuid-1"}"#;
        let resp: DhanPlaceOrderResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.order_id, "123");
        assert_eq!(resp.order_status, "TRANSIT");
        assert_eq!(resp.correlation_id, "uuid-1");
    }

    #[test]
    fn dhan_order_response_deserializes_with_defaults() {
        let json = r#"{"orderId":"999","orderStatus":"TRADED"}"#;
        let resp: DhanOrderResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.order_id, "999");
        assert_eq!(resp.order_status, "TRADED");
        assert_eq!(resp.quantity, 0);
        assert_eq!(resp.price, 0.0);
    }

    #[test]
    fn exchange_segment_constant() {
        assert_eq!(EXCHANGE_SEGMENT_NSE_FNO, "NSE_FNO");
    }
}
