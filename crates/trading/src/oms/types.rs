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
    /// Number of times this order has been modified (Dhan max: 25 per order).
    pub modification_count: u32,
}

/// Dhan maximum modifications per order.
pub const MAX_MODIFICATIONS_PER_ORDER: u32 = 25;

impl ManagedOrder {
    /// Returns true if the order is in a terminal state.
    ///
    /// Terminal states: Traded, Cancelled, Rejected, Expired, Closed.
    /// Non-terminal: Transit, Pending, Confirmed, PartTraded, Triggered.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Traded
                | OrderStatus::Cancelled
                | OrderStatus::Rejected
                | OrderStatus::Expired
                | OrderStatus::Closed
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
    /// Total order quantity (NOT remaining quantity).
    /// Setting quantity=75 on a 100-qty order with 30 filled → new total = 75.
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
///
/// Source: docs/dhan-ref/12-portfolio-positions.md
/// Endpoint: GET /v2/positions — returns array of position objects.
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
    /// Position type: `LONG`, `SHORT`, or `CLOSED`.
    #[serde(default)]
    pub position_type: String,
    #[serde(default)]
    pub buy_qty: i64,
    #[serde(default)]
    pub sell_qty: i64,
    /// Net quantity = buyQty - sellQty. Can be negative (short position).
    #[serde(default)]
    pub net_qty: i64,
    #[serde(default)]
    pub buy_avg: f64,
    #[serde(default)]
    pub sell_avg: f64,
    /// Booked P&L.
    #[serde(default)]
    pub realized_profit: f64,
    /// Open/mark-to-market P&L.
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
    /// RBI reference rate (for currency derivatives).
    #[serde(default)]
    pub rbi_reference_rate: f64,
    /// Carry-forward buy quantity from previous sessions.
    #[serde(default)]
    pub carry_forward_buy_qty: i64,
    /// Carry-forward sell quantity from previous sessions.
    #[serde(default)]
    pub carry_forward_sell_qty: i64,
    /// Carry-forward buy value from previous sessions.
    #[serde(default)]
    pub carry_forward_buy_value: f64,
    /// Carry-forward sell value from previous sessions.
    #[serde(default)]
    pub carry_forward_sell_value: f64,
    /// Today's intraday buy quantity.
    #[serde(default)]
    pub day_buy_qty: i64,
    /// Today's intraday sell quantity.
    #[serde(default)]
    pub day_sell_qty: i64,
    /// Today's intraday buy value.
    #[serde(default)]
    pub day_buy_value: f64,
    /// Today's intraday sell value.
    #[serde(default)]
    pub day_sell_value: f64,
    /// Whether this is a cross-currency position.
    #[serde(default)]
    pub cross_currency: bool,
}

// ---------------------------------------------------------------------------
// Holdings Response (GET /v2/holdings)
// Source: docs/dhan-ref/12-portfolio-positions.md
// ---------------------------------------------------------------------------

/// Dhan REST API holdings response — a single holding entry.
///
/// Response is an array of these objects (not wrapped in `data`).
/// NOTE: `mtf_tq_qty` and `mtf_qty` use snake_case in Dhan API
/// (inconsistent with camelCase used elsewhere).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanHoldingResponse {
    /// Exchange (e.g., "NSE", "BSE").
    #[serde(default)]
    pub exchange: String,
    /// Trading symbol.
    #[serde(default)]
    pub trading_symbol: String,
    /// Dhan security identifier (string).
    #[serde(default)]
    pub security_id: String,
    /// ISIN (12-character universal identifier).
    #[serde(default)]
    pub isin: String,
    /// Total quantity held.
    #[serde(default)]
    pub total_qty: i64,
    /// Quantity in DP (Depository Participant) — settled shares.
    #[serde(default)]
    pub dp_qty: i64,
    /// T+1 settlement quantity.
    #[serde(default)]
    pub t1_qty: i64,
    /// MTF total quantity — Dhan API uses snake_case here (inconsistent).
    #[serde(default, rename = "mtf_tq_qty")]
    pub mtf_tq_qty: i64,
    /// MTF quantity — Dhan API uses snake_case here (inconsistent).
    #[serde(default, rename = "mtf_qty")]
    pub mtf_qty: i64,
    /// Available quantity for sell/pledge. May differ from totalQty due to
    /// T+1 settlement or collateral.
    #[serde(default)]
    pub available_qty: i64,
    /// Collateral quantity.
    #[serde(default)]
    pub collateral_qty: i64,
    /// Average cost price.
    #[serde(default)]
    pub avg_cost_price: f64,
    /// Last traded price.
    #[serde(default)]
    pub last_traded_price: f64,
}

// ---------------------------------------------------------------------------
// Convert Position Request (POST /v2/positions/convert)
// Source: docs/dhan-ref/12-portfolio-positions.md
// ---------------------------------------------------------------------------

/// Request body for `POST /v2/positions/convert`.
///
/// CRITICAL: `convertQty` is a STRING, not an integer.
/// Response: `202 Accepted`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanConvertPositionRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// Source product type (e.g., "INTRADAY").
    pub from_product_type: String,
    /// Exchange segment (e.g., "NSE_FNO").
    pub exchange_segment: String,
    /// Position type ("LONG", "SHORT").
    pub position_type: String,
    /// Dhan security identifier (STRING).
    pub security_id: String,
    /// Trading symbol.
    pub trading_symbol: String,
    /// Quantity to convert — STRING, NOT integer.
    pub convert_qty: String,
    /// Target product type (e.g., "CNC").
    pub to_product_type: String,
}

// ---------------------------------------------------------------------------
// Exit All Response (DELETE /v2/positions)
// Source: docs/dhan-ref/12-portfolio-positions.md
// ---------------------------------------------------------------------------

/// Response from `DELETE /v2/positions` — exits ALL positions AND cancels ALL pending orders.
///
/// DANGER: This is a nuclear option. Use as emergency stop alongside kill switch.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanExitAllResponse {
    /// Status of the exit-all operation.
    #[serde(default)]
    pub status: String,
    /// Additional message.
    #[serde(default)]
    pub message: String,
}

// ---------------------------------------------------------------------------
// Margin Calculator Request (POST /v2/margincalculator)
// Source: docs/dhan-ref/13-funds-margin.md
// ---------------------------------------------------------------------------

/// Request body for `POST /v2/margincalculator`.
///
/// Uses same fields as order placement.
/// `securityId` is STRING (consistent with order APIs).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginCalculatorRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// Exchange segment (e.g., "NSE_FNO").
    pub exchange_segment: String,
    /// Transaction type ("BUY" or "SELL").
    pub transaction_type: String,
    /// Quantity in lots.
    pub quantity: i64,
    /// Product type (e.g., "INTRADAY", "MARGIN").
    pub product_type: String,
    /// Dhan security identifier (STRING).
    pub security_id: String,
    /// Order price.
    pub price: f64,
    /// Trigger price (for stop-loss).
    pub trigger_price: f64,
}

// ---------------------------------------------------------------------------
// Margin Calculator Response
// Source: docs/dhan-ref/13-funds-margin.md
// ---------------------------------------------------------------------------

/// Response from `POST /v2/margincalculator`.
///
/// NOTE: `leverage` is a STRING (e.g., `"4.00"`), NOT a float.
/// NOTE: `availableBalance` here is spelled correctly (unlike fund limit).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginCalculatorResponse {
    /// Total margin required.
    #[serde(default)]
    pub total_margin: f64,
    /// SPAN margin component.
    #[serde(default)]
    pub span_margin: f64,
    /// Exposure margin component.
    #[serde(default)]
    pub exposure_margin: f64,
    /// Available balance in account (correctly spelled here).
    #[serde(default)]
    pub available_balance: f64,
    /// Variable margin.
    #[serde(default)]
    pub variable_margin: f64,
    /// Shortfall amount (0 if sufficient).
    #[serde(default)]
    pub insufficient_balance: f64,
    /// Brokerage charges.
    #[serde(default)]
    pub brokerage: f64,
    /// Leverage ratio — STRING, NOT float (e.g., "4.00").
    #[serde(default)]
    pub leverage: String,
}

// ---------------------------------------------------------------------------
// Multi-Margin Calculator Types (POST /v2/margincalculator/multi)
// Source: docs/dhan-ref/13-funds-margin.md
// ---------------------------------------------------------------------------

/// A single script entry in the multi-margin calculator request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginScript {
    /// Exchange segment (e.g., "NSE_FNO").
    pub exchange_segment: String,
    /// Transaction type ("BUY" or "SELL").
    pub transaction_type: String,
    /// Quantity in lots.
    pub quantity: i64,
    /// Product type.
    pub product_type: String,
    /// Dhan security identifier (STRING).
    pub security_id: String,
    /// Order price.
    pub price: f64,
    /// Trigger price.
    pub trigger_price: f64,
}

/// Request body for `POST /v2/margincalculator/multi`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiMarginRequest {
    /// Whether to include existing positions in calculation.
    pub include_position: bool,
    /// Whether to include pending orders in calculation.
    pub include_orders: bool,
    /// Array of scripts (instruments) to calculate margin for.
    pub scripts: Vec<MarginScript>,
}

/// Response from `POST /v2/margincalculator/multi`.
///
/// CRITICAL: ALL values are STRINGS, not floats.
/// Uses snake_case (inconsistent with other Dhan APIs).
#[derive(Debug, Clone, Deserialize)]
pub struct MultiMarginResponse {
    /// Total margin required (STRING).
    #[serde(default)]
    pub total_margin: String,
    /// SPAN margin (STRING).
    #[serde(default)]
    pub span_margin: String,
    /// Exposure margin (STRING).
    #[serde(default)]
    pub exposure_margin: String,
    /// Equity margin (STRING).
    #[serde(default)]
    pub equity_margin: String,
    /// F&O margin (STRING).
    #[serde(default)]
    pub fo_margin: String,
    /// Commodity margin (STRING).
    #[serde(default)]
    pub commodity_margin: String,
    /// Currency margin (STRING).
    #[serde(default)]
    pub currency: String,
    /// Hedge benefit from offsetting positions (STRING).
    #[serde(default)]
    pub hedge_benefit: String,
}

// ---------------------------------------------------------------------------
// Fund Limit Response (GET /v2/fundlimit)
// Source: docs/dhan-ref/13-funds-margin.md
// ---------------------------------------------------------------------------

/// Response from `GET /v2/fundlimit`.
///
/// CRITICAL: `availabelBalance` has a TYPO in Dhan's API — missing 'l'.
/// We use `#[serde(rename = "availabelBalance")]` to match the actual API field name.
/// Do NOT "fix" this typo — the API literally sends `availabelBalance`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundLimitResponse {
    /// Dhan client ID.
    #[serde(default)]
    pub dhan_client_id: String,
    /// Available balance — TYPO in Dhan API: `availabelBalance` (missing 'l').
    #[serde(default, rename = "availabelBalance")]
    pub availabel_balance: f64,
    /// Start-of-day limit.
    #[serde(default)]
    pub sod_limit: f64,
    /// Collateral amount.
    #[serde(default)]
    pub collateral_amount: f64,
    /// Receivable amount.
    #[serde(default)]
    pub receiveable_amount: f64,
    /// Utilized amount.
    #[serde(default)]
    pub utilized_amount: f64,
    /// Blocked payout amount.
    #[serde(default)]
    pub blocked_payout_amount: f64,
    /// Withdrawable balance.
    #[serde(default)]
    pub withdrawable_balance: f64,
}

// ---------------------------------------------------------------------------
// Conditional Trigger Types (07c-conditional-trigger.md)
// ---------------------------------------------------------------------------

/// Comparison type for conditional triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComparisonType {
    /// Indicator vs fixed number.
    #[serde(rename = "TECHNICAL_WITH_VALUE")]
    TechnicalWithValue,
    /// Indicator vs another indicator.
    #[serde(rename = "TECHNICAL_WITH_INDICATOR")]
    TechnicalWithIndicator,
    /// Indicator vs closing price.
    #[serde(rename = "TECHNICAL_WITH_CLOSE")]
    TechnicalWithClose,
    /// Market price vs fixed value.
    #[serde(rename = "PRICE_WITH_VALUE")]
    PriceWithValue,
}

/// Operator for conditional trigger comparisons (9 variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TriggerOperator {
    #[serde(rename = "CROSSING_UP")]
    CrossingUp,
    #[serde(rename = "CROSSING_DOWN")]
    CrossingDown,
    #[serde(rename = "CROSSING_ANY_SIDE")]
    CrossingAnySide,
    #[serde(rename = "GREATER_THAN")]
    GreaterThan,
    #[serde(rename = "LESS_THAN")]
    LessThan,
    #[serde(rename = "GREATER_THAN_EQUAL")]
    GreaterThanEqual,
    #[serde(rename = "LESS_THAN_EQUAL")]
    LessThanEqual,
    #[serde(rename = "EQUAL")]
    Equal,
    #[serde(rename = "NOT_EQUAL")]
    NotEqual,
}

/// Timeframe for conditional trigger indicator evaluation (4 variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TriggerTimeFrame {
    #[serde(rename = "DAY")]
    Day,
    #[serde(rename = "ONE_MIN")]
    OneMin,
    #[serde(rename = "FIVE_MIN")]
    FiveMin,
    #[serde(rename = "FIFTEEN_MIN")]
    FifteenMin,
}

/// Condition for a conditional trigger.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerCondition {
    /// Comparison type.
    pub comparison_type: String,
    /// Exchange segment: "NSE_EQ" or index only. F&O NOT supported.
    pub exchange_segment: String,
    /// Dhan security ID (STRING).
    pub security_id: String,
    /// Indicator name (required for TECHNICAL_WITH_* types).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indicator_name: Option<String>,
    /// Timeframe (required for TECHNICAL_WITH_* types).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_frame: Option<String>,
    /// Comparison operator.
    pub operator: String,
    /// Fixed value to compare against (TECHNICAL_WITH_VALUE, PRICE_WITH_VALUE).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparing_value: Option<f64>,
    /// Second indicator to compare against (TECHNICAL_WITH_INDICATOR).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparing_indicator_name: Option<String>,
    /// Expiry date (YYYY-MM-DD). Default: 1 year from creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<String>,
    /// Trigger frequency: "ONCE" (triggers once then deactivates).
    #[serde(default = "default_frequency")]
    pub frequency: String,
    /// User note/description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_note: Option<String>,
}

fn default_frequency() -> String {
    "ONCE".to_string()
}

/// Order attached to a conditional trigger.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerOrder {
    /// "BUY" or "SELL".
    pub transaction_type: String,
    /// Exchange segment.
    pub exchange_segment: String,
    /// Product type.
    pub product_type: String,
    /// Order type.
    pub order_type: String,
    /// Dhan security ID (STRING).
    pub security_id: String,
    /// Quantity.
    pub quantity: i64,
    /// Validity: "DAY" or "IOC".
    pub validity: String,
    /// Price (STRING in Dhan API).
    pub price: String,
    /// Disclosed quantity (STRING in Dhan API).
    #[serde(default, rename = "discQuantity")]
    pub disc_quantity: String,
    /// Trigger price (STRING in Dhan API).
    pub trigger_price: String,
}

/// Create conditional trigger request.
/// Endpoint: `POST /v2/alerts/orders`
///
/// **Equities and Indices ONLY.** F&O and commodities NOT supported.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanConditionalTriggerRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// Trigger condition.
    pub condition: TriggerCondition,
    /// Orders to execute when condition fires (can be multiple).
    pub orders: Vec<TriggerOrder>,
}

/// Conditional trigger response.
/// Endpoint: `POST/PUT/DELETE/GET /v2/alerts/orders`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanConditionalTriggerResponse {
    /// Alert ID.
    #[serde(default)]
    pub alert_id: String,
    /// Alert status: "ACTIVE", "TRIGGERED", "EXPIRED", "CANCELLED".
    #[serde(default)]
    pub alert_status: String,
}

// ---------------------------------------------------------------------------
// EDIS Types (07d-edis.md)
// ---------------------------------------------------------------------------

/// EDIS form request for CDSL T-PIN authorization.
/// Endpoint: `POST /v2/edis/form`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EdisFormRequest {
    /// ISIN of the security to authorize for delivery.
    pub isin: String,
    /// Quantity to authorize.
    pub qty: i64,
    /// Exchange: "NSE" or "BSE".
    pub exchange: String,
    /// Segment: "EQ".
    pub segment: String,
    /// Mark all holdings for sell.
    pub bulk: bool,
}

/// EDIS inquiry response.
/// Endpoint: `GET /v2/edis/inquire/{isin}`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdisInquiryResponse {
    /// Total quantity held.
    #[serde(default)]
    pub total_qty: i64,
    /// Approved quantity for delivery.
    #[serde(default)]
    pub aprvd_qty: i64,
    /// EDIS status.
    #[serde(default)]
    pub status: String,
}

// ---------------------------------------------------------------------------
// Statements Types (14-statements-trade-history.md)
// ---------------------------------------------------------------------------

/// Ledger entry from Dhan.
/// Endpoint: `GET /v2/ledger?from-date=YYYY-MM-DD&to-date=YYYY-MM-DD`
///
/// **CRITICAL:** `debit` and `credit` are STRINGS, not floats.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DhanLedgerEntry {
    /// Dhan client ID.
    #[serde(default, rename = "dhanClientId")]
    pub dhan_client_id: String,
    /// Transaction narration.
    #[serde(default)]
    pub narration: String,
    /// Voucher date (human-readable: "Jun 22, 2022" — NOT ISO format).
    #[serde(default)]
    pub voucherdate: String,
    /// Exchange.
    #[serde(default)]
    pub exchange: String,
    /// Voucher description.
    #[serde(default)]
    pub voucherdesc: String,
    /// Voucher number.
    #[serde(default)]
    pub vouchernumber: String,
    /// Debit amount (STRING, not float). "0.00" when credit.
    #[serde(default)]
    pub debit: String,
    /// Credit amount (STRING, not float). "0.00" when debit.
    #[serde(default)]
    pub credit: String,
    /// Running balance (STRING).
    #[serde(default)]
    pub runbal: String,
}

/// Historical trade entry from Dhan.
/// Endpoint: `GET /v2/trades/{from-date}/{to-date}/{page}`
///
/// **NOTE:** Page is 0-indexed. Path params, NOT query params.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanHistoricalTradeEntry {
    /// Dhan client ID.
    #[serde(default)]
    pub dhan_client_id: String,
    /// Order ID.
    #[serde(default)]
    pub order_id: String,
    /// Exchange order ID.
    #[serde(default)]
    pub exchange_order_id: String,
    /// Exchange trade ID.
    #[serde(default)]
    pub exchange_trade_id: String,
    /// Transaction type.
    #[serde(default)]
    pub transaction_type: String,
    /// Exchange segment.
    #[serde(default)]
    pub exchange_segment: String,
    /// Product type.
    #[serde(default)]
    pub product_type: String,
    /// Order type.
    #[serde(default)]
    pub order_type: String,
    /// Trading symbol.
    #[serde(default)]
    pub trading_symbol: String,
    /// Custom symbol.
    #[serde(default)]
    pub custom_symbol: String,
    /// Security ID.
    #[serde(default)]
    pub security_id: String,
    /// Traded quantity.
    #[serde(default)]
    pub traded_quantity: i64,
    /// Traded price.
    #[serde(default)]
    pub traded_price: f64,
    /// ISIN.
    #[serde(default)]
    pub isin: String,
    /// Instrument type.
    #[serde(default)]
    pub instrument: String,
    /// SEBI tax.
    #[serde(default)]
    pub sebi_tax: f64,
    /// Securities Transaction Tax.
    #[serde(default)]
    pub stt: f64,
    /// Brokerage charges.
    #[serde(default)]
    pub brokerage_charges: f64,
    /// Service tax / GST.
    #[serde(default)]
    pub service_tax: f64,
    /// Exchange transaction charges.
    #[serde(default)]
    pub exchange_transaction_charges: f64,
    /// Stamp duty.
    #[serde(default)]
    pub stamp_duty: f64,
    /// Derivative expiry date ("NA" for non-derivatives).
    #[serde(default)]
    pub drv_expiry_date: String,
    /// Derivative option type.
    #[serde(default)]
    pub drv_option_type: String,
    /// Derivative strike price.
    #[serde(default)]
    pub drv_strike_price: f64,
    /// Exchange time (IST string).
    #[serde(default)]
    pub exchange_time: String,
}

// ---------------------------------------------------------------------------
// Kill Switch Types
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Super Order Types (07a-super-order.md)
// ---------------------------------------------------------------------------

/// Leg type for super orders (bracket orders with entry + target + stop loss).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OrderLeg {
    /// Entry leg — the initial order.
    #[serde(rename = "ENTRY_LEG")]
    EntryLeg,
    /// Target leg — profit booking.
    #[serde(rename = "TARGET_LEG")]
    TargetLeg,
    /// Stop loss leg — risk limit.
    #[serde(rename = "STOP_LOSS_LEG")]
    StopLossLeg,
}

impl OrderLeg {
    /// Returns the Dhan API string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EntryLeg => "ENTRY_LEG",
            Self::TargetLeg => "TARGET_LEG",
            Self::StopLossLeg => "STOP_LOSS_LEG",
        }
    }
}

/// Place super order request (entry + target + stop loss as 3 legs).
/// Endpoint: `POST /v2/super/orders`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanPlaceSuperOrderRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// User-supplied idempotency key (max 30 chars).
    #[serde(default)]
    pub correlation_id: String,
    /// "BUY" or "SELL".
    pub transaction_type: String,
    /// Exchange segment: "NSE_EQ", "NSE_FNO", etc.
    pub exchange_segment: String,
    /// Product type: "CNC", "INTRADAY", "MARGIN", "MTF".
    pub product_type: String,
    /// Order type: "LIMIT", "MARKET", "STOP_LOSS", "STOP_LOSS_MARKET".
    pub order_type: String,
    /// Dhan security ID (STRING, not integer).
    pub security_id: String,
    /// Quantity in lots.
    pub quantity: i64,
    /// Entry price.
    pub price: f64,
    /// Target exit price (profit booking).
    pub target_price: f64,
    /// Stop loss price (risk limit).
    pub stop_loss_price: f64,
    /// Trailing SL price jump (0 = no trailing, 0 explicitly cancels trailing).
    pub trailing_jump: f64,
}

/// Modify super order request (leg-specific restrictions).
/// Endpoint: `PUT /v2/super/orders/{order-id}`
///
/// Modification restrictions by leg:
/// - ENTRY_LEG: all fields modifiable (only when PENDING or PART_TRADED)
/// - TARGET_LEG: only `targetPrice`
/// - STOP_LOSS_LEG: only `stopLossPrice` and `trailingJump`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanModifySuperOrderRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// Which leg to modify.
    pub leg_name: String,
    /// Order type (ENTRY_LEG only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_type: Option<String>,
    /// Quantity (ENTRY_LEG only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    /// Entry price (ENTRY_LEG only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    /// Target price (TARGET_LEG or ENTRY_LEG).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_price: Option<f64>,
    /// Stop loss price (STOP_LOSS_LEG or ENTRY_LEG).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss_price: Option<f64>,
    /// Trailing SL jump (STOP_LOSS_LEG or ENTRY_LEG). 0 = cancel trailing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_jump: Option<f64>,
}

/// Super order leg detail from response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuperOrderLegDetail {
    /// Dhan order ID for this leg.
    #[serde(default)]
    pub order_id: String,
    /// Leg name: "ENTRY_LEG", "TARGET_LEG", "STOP_LOSS_LEG".
    #[serde(default)]
    pub leg_name: String,
    /// Transaction type: "BUY" or "SELL".
    #[serde(default)]
    pub transaction_type: String,
    /// Remaining quantity.
    #[serde(default)]
    pub remaining_quantity: i64,
    /// Triggered quantity.
    #[serde(default)]
    pub triggered_quantity: i64,
    /// Price for this leg.
    #[serde(default)]
    pub price: f64,
    /// Status of this leg.
    #[serde(default)]
    pub order_status: String,
    /// Trailing jump value.
    #[serde(default)]
    pub trailing_jump: f64,
}

/// Super order response (includes leg details).
/// Endpoint: `GET /v2/super/orders` and `POST /v2/super/orders`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanSuperOrderResponse {
    /// Dhan order ID (entry leg).
    #[serde(default)]
    pub order_id: String,
    /// Order status.
    #[serde(default)]
    pub order_status: String,
    /// Correlation ID.
    #[serde(default)]
    pub correlation_id: String,
    /// Leg details array (target + stop loss legs).
    #[serde(default)]
    pub leg_details: Vec<SuperOrderLegDetail>,
}

// ---------------------------------------------------------------------------
// Forever Order Types (07b-forever-order.md)
// ---------------------------------------------------------------------------

/// Order flag for forever orders: SINGLE (one trigger) or OCO (two triggers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ForeverOrderFlag {
    /// Single trigger — one price condition.
    #[serde(rename = "SINGLE")]
    Single,
    /// OCO (One Cancels Other) — two triggers, whichever hits first.
    #[serde(rename = "OCO")]
    Oco,
}

/// Place forever order (GTT) request.
/// Endpoint: `POST /v2/forever/orders`
///
/// Product types: CNC, MTF ONLY (no INTRADAY, no MARGIN).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanForeverOrderRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// User-supplied idempotency key.
    #[serde(default)]
    pub correlation_id: String,
    /// "SINGLE" or "OCO".
    pub order_flag: String,
    /// "BUY" or "SELL".
    pub transaction_type: String,
    /// Exchange segment.
    pub exchange_segment: String,
    /// Product type: CNC or MTF ONLY.
    pub product_type: String,
    /// Order type: "LIMIT" or "MARKET".
    pub order_type: String,
    /// Validity: "DAY" or "IOC".
    pub validity: String,
    /// Dhan security ID (STRING).
    pub security_id: String,
    /// First leg quantity.
    pub quantity: i64,
    /// Disclosed quantity (optional).
    #[serde(default)]
    pub disclosed_quantity: i64,
    /// First leg price.
    pub price: f64,
    /// First leg trigger price.
    pub trigger_price: f64,
    /// Second leg price (OCO ONLY — REQUIRED for OCO, must NOT be sent for SINGLE).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price1: Option<f64>,
    /// Second leg trigger price (OCO ONLY).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_price1: Option<f64>,
    /// Second leg quantity (OCO ONLY).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity1: Option<i64>,
}

/// Forever order response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanForeverOrderResponse {
    /// Dhan order ID.
    #[serde(default)]
    pub order_id: String,
    /// Order status (includes CONFIRM — unique to forever orders).
    #[serde(default)]
    pub order_status: String,
}

// ---------------------------------------------------------------------------
// Trade Book Types
// ---------------------------------------------------------------------------

/// A single trade entry from the trade book.
/// Endpoint: `GET /v2/trades` and `GET /v2/trades/{order-id}`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanTradeEntry {
    /// Dhan client ID.
    #[serde(default)]
    pub dhan_client_id: String,
    /// Dhan order ID.
    #[serde(default)]
    pub order_id: String,
    /// Exchange order ID.
    #[serde(default)]
    pub exchange_order_id: String,
    /// Exchange trade ID (unique per trade).
    #[serde(default)]
    pub exchange_trade_id: String,
    /// Transaction type: "BUY" or "SELL".
    #[serde(default)]
    pub transaction_type: String,
    /// Exchange segment: "NSE_EQ", "NSE_FNO", etc.
    #[serde(default)]
    pub exchange_segment: String,
    /// Product type: "CNC", "INTRADAY", "MARGIN", "MTF".
    #[serde(default)]
    pub product_type: String,
    /// Order type: "LIMIT", "MARKET", "STOP_LOSS", "STOP_LOSS_MARKET".
    #[serde(default)]
    pub order_type: String,
    /// Trading symbol.
    #[serde(default)]
    pub trading_symbol: String,
    /// Custom symbol.
    #[serde(default)]
    pub custom_symbol: String,
    /// Dhan security ID (string in response).
    #[serde(default)]
    pub security_id: String,
    /// Traded quantity.
    #[serde(default)]
    pub traded_quantity: i64,
    /// Traded price.
    #[serde(default)]
    pub traded_price: f64,
    /// ISIN (International Securities Identification Number).
    #[serde(default)]
    pub isin: String,
    /// Instrument type.
    #[serde(default)]
    pub instrument: String,
    /// SEBI tax.
    #[serde(default)]
    pub sebi_tax: f64,
    /// Securities Transaction Tax.
    #[serde(default)]
    pub stt: f64,
    /// Brokerage charges.
    #[serde(default)]
    pub brokerage_charges: f64,
    /// Service tax / GST.
    #[serde(default)]
    pub service_tax: f64,
    /// Exchange transaction charges.
    #[serde(default)]
    pub exchange_transaction_charges: f64,
    /// Stamp duty.
    #[serde(default)]
    pub stamp_duty: f64,
    /// Derivative expiry date ("NA" for non-derivatives, date string for F&O).
    #[serde(default)]
    pub drv_expiry_date: String,
    /// Derivative option type (CE/PE or empty).
    #[serde(default)]
    pub drv_option_type: String,
    /// Derivative strike price.
    #[serde(default)]
    pub drv_strike_price: f64,
    /// Exchange time (IST string: "YYYY-MM-DD HH:MM:SS").
    #[serde(default)]
    pub exchange_time: String,
}

/// Cancel order response from Dhan.
/// Endpoint: `DELETE /v2/orders/{order-id}`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanCancelOrderResponse {
    /// Dhan order ID.
    #[serde(default)]
    pub order_id: String,
    /// Order status after cancellation.
    #[serde(default)]
    pub order_status: String,
}

/// Kill switch status response from Dhan.
/// Endpoint: `GET /v2/killswitch` and `POST /v2/killswitch`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillSwitchResponse {
    /// Dhan client ID.
    #[serde(default)]
    pub dhan_client_id: String,
    /// Kill switch status: "ACTIVATE" or "DEACTIVATE".
    #[serde(default)]
    pub kill_switch_status: String,
}

// ---------------------------------------------------------------------------
// P&L Exit Types
// ---------------------------------------------------------------------------

/// P&L-based exit configuration request.
/// Endpoint: `POST /v2/pnlExit`
///
/// **WARNING:** If `profit_value` < current profit OR `loss_value` < current loss,
/// exit triggers IMMEDIATELY. Always check current P&L before configuring.
///
/// Session-scoped — resets at end of trading session. Must reconfigure daily.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PnlExitRequest {
    /// Profit threshold (STRING, not float) — e.g., "1500.00".
    pub profit_value: String,
    /// Loss threshold (STRING, not float) — e.g., "500.00".
    pub loss_value: String,
    /// Product types to apply exit on — e.g., ["INTRADAY", "DELIVERY"].
    pub product_type: Vec<String>,
    /// Also activate kill switch after P&L exit triggers.
    pub enable_kill_switch: bool,
}

/// P&L exit configure/stop response from Dhan.
/// Endpoint: `POST /v2/pnlExit` and `DELETE /v2/pnlExit`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PnlExitResponse {
    /// P&L exit status: "ACTIVE" or "DISABLED".
    #[serde(default)]
    pub pnl_exit_status: String,
    /// Confirmation message.
    #[serde(default)]
    pub message: String,
}

/// P&L exit status response from Dhan (GET endpoint).
/// **Field names differ from POST request** (Dhan API inconsistency):
/// - POST request: `profitValue`, `lossValue`, `enableKillSwitch` (camelCase)
/// - GET response: `profit`, `loss`, `enable_kill_switch` (shorter names, snake_case mix)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PnlExitStatusResponse {
    /// P&L exit status: "ACTIVE" or "DISABLED".
    #[serde(default, rename = "pnlExitStatus")]
    pub pnl_exit_status: String,
    /// Profit threshold (shorter name than request).
    #[serde(default)]
    pub profit: String,
    /// Loss threshold (shorter name than request).
    #[serde(default)]
    pub loss: String,
    /// Product types.
    #[serde(default, rename = "productType")]
    pub product_type: Vec<String>,
    /// Kill switch enabled flag (snake_case in GET, camelCase in POST — Dhan inconsistency).
    #[serde(default)]
    pub enable_kill_switch: bool,
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
            modification_count: 0,
        };

        // Non-terminal states
        assert!(!order.is_terminal());

        order.status = OrderStatus::Pending;
        assert!(!order.is_terminal());

        order.status = OrderStatus::Confirmed;
        assert!(!order.is_terminal());

        order.status = OrderStatus::PartTraded;
        assert!(
            !order.is_terminal(),
            "PartTraded is NOT terminal — order still active"
        );

        order.status = OrderStatus::Triggered;
        assert!(
            !order.is_terminal(),
            "Triggered is NOT terminal — condition fired, order active"
        );

        // Terminal states
        order.status = OrderStatus::Traded;
        assert!(order.is_terminal());

        order.status = OrderStatus::Cancelled;
        assert!(order.is_terminal());

        order.status = OrderStatus::Rejected;
        assert!(order.is_terminal());

        order.status = OrderStatus::Expired;
        assert!(order.is_terminal());

        order.status = OrderStatus::Closed;
        assert!(
            order.is_terminal(),
            "Closed is terminal — Super Order complete"
        );
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

    // --- DhanHoldingResponse (B1) ---

    #[test]
    fn test_holding_response_deserializes() {
        let json = r#"{
            "exchange": "NSE",
            "tradingSymbol": "RELIANCE",
            "securityId": "2885",
            "isin": "INE002A01018",
            "totalQty": 100,
            "dpQty": 90,
            "t1Qty": 10,
            "availableQty": 90,
            "collateralQty": 0,
            "avgCostPrice": 2450.50,
            "lastTradedPrice": 2500.00
        }"#;

        let holding: DhanHoldingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(holding.exchange, "NSE");
        assert_eq!(holding.security_id, "2885");
        assert_eq!(holding.total_qty, 100);
        assert_eq!(holding.available_qty, 90);
        assert!((holding.avg_cost_price - 2450.50).abs() < f64::EPSILON);
    }

    #[test]
    fn test_holding_response_mtf_snake_case_fields() {
        // mtf_tq_qty and mtf_qty use snake_case in Dhan API (inconsistent)
        let json = r#"{
            "exchange": "NSE",
            "tradingSymbol": "RELIANCE",
            "securityId": "2885",
            "isin": "INE002A01018",
            "totalQty": 100,
            "dpQty": 90,
            "t1Qty": 10,
            "mtf_tq_qty": 50,
            "mtf_qty": 25,
            "availableQty": 90,
            "collateralQty": 0,
            "avgCostPrice": 2450.50,
            "lastTradedPrice": 2500.00
        }"#;

        let holding: DhanHoldingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(holding.mtf_tq_qty, 50);
        assert_eq!(holding.mtf_qty, 25);
    }

    // --- DhanPositionResponse completeness (B2) ---

    #[test]
    fn test_position_response_all_fields_deserialize() {
        let json = r#"{
            "dhanClientId": "1000000001",
            "securityId": "52432",
            "exchangeSegment": "NSE_FNO",
            "productType": "INTRADAY",
            "positionType": "LONG",
            "buyQty": 50,
            "sellQty": 0,
            "netQty": 50,
            "buyAvg": 245.50,
            "sellAvg": 0.0,
            "realizedProfit": 0.0,
            "unrealizedProfit": 1250.00,
            "tradingSymbol": "NIFTY-Mar2026-24500-CE",
            "costPrice": 245.50,
            "multiplier": 25,
            "drvExpiryDate": "2026-03-27",
            "drvOptionType": "CALL",
            "drvStrikePrice": 24500.0,
            "rbiReferenceRate": 0.0,
            "carryForwardBuyQty": 25,
            "carryForwardSellQty": 0,
            "carryForwardBuyValue": 6137.50,
            "carryForwardSellValue": 0.0,
            "dayBuyQty": 25,
            "daySellQty": 0,
            "dayBuyValue": 6137.50,
            "daySellValue": 0.0,
            "crossCurrency": false
        }"#;

        let pos: DhanPositionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(pos.position_type, "LONG");
        assert_eq!(pos.net_qty, 50);
        assert_eq!(pos.carry_forward_buy_qty, 25);
        assert_eq!(pos.day_buy_qty, 25);
        assert!(!pos.cross_currency);
    }

    #[test]
    fn test_position_response_carry_forward_fields() {
        let json = r#"{
            "carryForwardBuyQty": 100,
            "carryForwardSellQty": 50,
            "carryForwardBuyValue": 25000.0,
            "carryForwardSellValue": 12500.0
        }"#;

        let pos: DhanPositionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(pos.carry_forward_buy_qty, 100);
        assert_eq!(pos.carry_forward_sell_qty, 50);
        assert!((pos.carry_forward_buy_value - 25000.0).abs() < f64::EPSILON);
        assert!((pos.carry_forward_sell_value - 12500.0).abs() < f64::EPSILON);
    }

    // --- DhanConvertPositionRequest (B3) ---

    #[test]
    fn test_convert_position_request_serializes() {
        let req = DhanConvertPositionRequest {
            dhan_client_id: "1000000001".to_owned(),
            from_product_type: "INTRADAY".to_owned(),
            exchange_segment: EXCHANGE_SEGMENT_NSE_FNO.to_owned(),
            position_type: "LONG".to_owned(),
            security_id: "52432".to_owned(),
            trading_symbol: "NIFTY-Mar2026-24500-CE".to_owned(),
            convert_qty: "40".to_owned(),
            to_product_type: "CNC".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("dhanClientId"));
        assert!(json.contains("fromProductType"));
        assert!(json.contains("convertQty"));
        assert!(json.contains("toProductType"));
    }

    #[test]
    fn test_convert_position_convert_qty_is_string() {
        let req = DhanConvertPositionRequest {
            dhan_client_id: "100".to_owned(),
            from_product_type: "INTRADAY".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            position_type: "LONG".to_owned(),
            security_id: "52432".to_owned(),
            trading_symbol: "NIFTY".to_owned(),
            convert_qty: "40".to_owned(),
            to_product_type: "CNC".to_owned(),
        };
        let json_value: serde_json::Value = serde_json::to_value(&req).unwrap();
        // convertQty MUST be a string "40", not integer 40
        assert!(
            json_value["convertQty"].is_string(),
            "convertQty must be a STRING, not integer"
        );
        assert_eq!(json_value["convertQty"], "40");
    }

    // --- DhanExitAllResponse (B4) ---

    #[test]
    fn test_exit_all_response_deserializes() {
        let json = r#"{"status": "success", "message": "All positions exited"}"#;
        let resp: DhanExitAllResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "success");
    }

    // --- MarginCalculatorRequest (C1) ---

    #[test]
    fn test_margin_calculator_request_serializes_camel_case() {
        let req = MarginCalculatorRequest {
            dhan_client_id: "100".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            transaction_type: "BUY".to_owned(),
            quantity: 50,
            product_type: "INTRADAY".to_owned(),
            security_id: "52432".to_owned(),
            price: 245.50,
            trigger_price: 0.0,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("dhanClientId"));
        assert!(json.contains("exchangeSegment"));
        assert!(json.contains("transactionType"));
        assert!(json.contains("securityId"));
    }

    #[test]
    fn test_margin_calculator_security_id_is_string() {
        let req = MarginCalculatorRequest {
            dhan_client_id: "100".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            transaction_type: "BUY".to_owned(),
            quantity: 50,
            product_type: "INTRADAY".to_owned(),
            security_id: "52432".to_owned(),
            price: 245.50,
            trigger_price: 0.0,
        };
        let json_value: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert!(json_value["securityId"].is_string());
    }

    // --- MarginCalculatorResponse (C2) ---

    #[test]
    fn test_margin_calculator_response_deserializes() {
        let json = r#"{
            "totalMargin": 12500.00,
            "spanMargin": 10000.00,
            "exposureMargin": 2500.00,
            "availableBalance": 50000.00,
            "variableMargin": 0.0,
            "insufficientBalance": 0.0,
            "brokerage": 20.0,
            "leverage": "4.00"
        }"#;

        let resp: MarginCalculatorResponse = serde_json::from_str(json).unwrap();
        assert!((resp.total_margin - 12500.0).abs() < f64::EPSILON);
        assert!((resp.span_margin - 10000.0).abs() < f64::EPSILON);
        assert_eq!(resp.leverage, "4.00");
    }

    #[test]
    fn test_margin_calculator_leverage_is_string() {
        let json = r#"{"leverage": "4.00"}"#;
        let resp: MarginCalculatorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.leverage, "4.00");
    }

    // --- Multi Margin (C3) ---

    #[test]
    fn test_multi_margin_request_serializes() {
        let req = MultiMarginRequest {
            include_position: true,
            include_orders: false,
            scripts: vec![MarginScript {
                exchange_segment: "NSE_FNO".to_owned(),
                transaction_type: "BUY".to_owned(),
                quantity: 50,
                product_type: "INTRADAY".to_owned(),
                security_id: "52432".to_owned(),
                price: 245.50,
                trigger_price: 0.0,
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("includePosition"));
        assert!(json.contains("includeOrders"));
        assert!(json.contains("scripts"));
    }

    #[test]
    fn test_multi_margin_response_all_strings() {
        let json = r#"{
            "total_margin": "12500.00",
            "span_margin": "10000.00",
            "exposure_margin": "2500.00",
            "equity_margin": "0.00",
            "fo_margin": "12500.00",
            "commodity_margin": "0.00",
            "currency": "0.00",
            "hedge_benefit": "500.00"
        }"#;

        let resp: MultiMarginResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.total_margin, "12500.00");
        assert_eq!(resp.span_margin, "10000.00");
        assert_eq!(resp.hedge_benefit, "500.00");
        // All fields are strings
        assert_eq!(resp.fo_margin, "12500.00");
        assert_eq!(resp.commodity_margin, "0.00");
    }

    // --- FundLimitResponse (C4) ---

    #[test]
    fn test_fund_limit_response_deserializes() {
        let json = r#"{
            "dhanClientId": "1000000001",
            "availabelBalance": 50000.00,
            "sodLimit": 100000.00,
            "collateralAmount": 0.0,
            "receiveableAmount": 5000.00,
            "utilizedAmount": 50000.00,
            "blockedPayoutAmount": 0.0,
            "withdrawableBalance": 45000.00
        }"#;

        let resp: FundLimitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.dhan_client_id, "1000000001");
        assert!((resp.availabel_balance - 50000.0).abs() < f64::EPSILON);
        assert!((resp.sod_limit - 100000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fund_limit_availabel_balance_typo() {
        // The typo "availabelBalance" (missing 'l') is in Dhan's API.
        // Our serde rename maps it correctly.
        let json = r#"{"availabelBalance": 99999.99}"#;
        let resp: FundLimitResponse = serde_json::from_str(json).unwrap();
        assert!((resp.availabel_balance - 99999.99).abs() < f64::EPSILON);

        // Verify that the CORRECT spelling does NOT deserialize
        let json_correct = r#"{"availableBalance": 99999.99}"#;
        let resp_correct: FundLimitResponse = serde_json::from_str(json_correct).unwrap();
        // availabel_balance should default to 0.0 since the correct spelling doesn't match
        assert!((resp_correct.availabel_balance - 0.0).abs() < f64::EPSILON);
    }

    // --- OmsError Display/Debug completeness ---

    #[test]
    fn oms_error_display_all_variants_non_empty() {
        let all_errors: Vec<OmsError> = vec![
            OmsError::RiskRejected {
                reason: "test".to_owned(),
            },
            OmsError::RateLimited,
            OmsError::CircuitBreakerOpen,
            OmsError::OrderNotFound {
                order_id: "O1".to_owned(),
            },
            OmsError::OrderTerminal {
                order_id: "O2".to_owned(),
                status: "TRADED".to_owned(),
            },
            OmsError::InvalidTransition {
                order_id: "O3".to_owned(),
                from: "PENDING".to_owned(),
                to: "TRANSIT".to_owned(),
            },
            OmsError::DhanApiError {
                status_code: 500,
                message: "internal error".to_owned(),
            },
            OmsError::DhanRateLimited,
            OmsError::NoToken,
            OmsError::TokenExpired,
            OmsError::HttpError("connection refused".to_owned()),
            OmsError::JsonError("unexpected token".to_owned()),
        ];

        for err in &all_errors {
            let display = err.to_string();
            assert!(!display.is_empty(), "Display must not be empty for {err:?}");
        }
    }

    #[test]
    fn oms_error_debug_all_variants_contain_variant_name() {
        let test_cases: Vec<(OmsError, &str)> = vec![
            (
                OmsError::RiskRejected {
                    reason: "x".to_owned(),
                },
                "RiskRejected",
            ),
            (OmsError::RateLimited, "RateLimited"),
            (OmsError::CircuitBreakerOpen, "CircuitBreakerOpen"),
            (
                OmsError::OrderNotFound {
                    order_id: "x".to_owned(),
                },
                "OrderNotFound",
            ),
            (
                OmsError::OrderTerminal {
                    order_id: "x".to_owned(),
                    status: "x".to_owned(),
                },
                "OrderTerminal",
            ),
            (
                OmsError::InvalidTransition {
                    order_id: "x".to_owned(),
                    from: "x".to_owned(),
                    to: "x".to_owned(),
                },
                "InvalidTransition",
            ),
            (
                OmsError::DhanApiError {
                    status_code: 0,
                    message: "x".to_owned(),
                },
                "DhanApiError",
            ),
            (OmsError::DhanRateLimited, "DhanRateLimited"),
            (OmsError::NoToken, "NoToken"),
            (OmsError::TokenExpired, "TokenExpired"),
            (OmsError::HttpError("x".to_owned()), "HttpError"),
            (OmsError::JsonError("x".to_owned()), "JsonError"),
        ];

        for (err, expected_name) in &test_cases {
            let debug = format!("{err:?}");
            assert!(
                debug.contains(expected_name),
                "Debug for {expected_name} must contain variant name: got '{debug}'"
            );
        }
    }

    #[test]
    fn oms_error_display_includes_context() {
        let err = OmsError::OrderTerminal {
            order_id: "ORD-42".to_owned(),
            status: "TRADED".to_owned(),
        };
        let display = err.to_string();
        assert!(display.contains("ORD-42"), "must include order_id");
        assert!(display.contains("TRADED"), "must include status");

        let err = OmsError::DhanApiError {
            status_code: 429,
            message: "too many requests".to_owned(),
        };
        let display = err.to_string();
        assert!(display.contains("429"), "must include status code");
        assert!(
            display.contains("too many requests"),
            "must include message"
        );
    }

    // --- ManagedOrder Debug ---

    #[test]
    fn max_modifications_per_order_constant() {
        assert_eq!(
            MAX_MODIFICATIONS_PER_ORDER, 25,
            "Dhan allows max 25 modifications per order"
        );
    }

    #[test]
    fn dhan_modify_order_request_serializes_camel_case() {
        let req = DhanModifyOrderRequest {
            dhan_client_id: "100".to_owned(),
            order_id: "ORD-789".to_owned(),
            order_type: "LIMIT".to_owned(),
            leg_name: "".to_owned(),
            quantity: 75,
            price: 250.00,
            trigger_price: 0.0,
            validity: "DAY".to_owned(),
            disclosed_quantity: 0,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("dhanClientId"));
        assert!(json.contains("orderId"));
        assert!(json.contains("orderType"));
        assert!(json.contains("legName"));
        assert!(json.contains("disclosedQuantity"));
        // Must not contain snake_case field names
        assert!(!json.contains("dhan_client_id"));
        assert!(!json.contains("order_id"));
        assert!(!json.contains("leg_name"));
    }

    #[test]
    fn dhan_order_response_all_fields_deserialize() {
        let json = r#"{
            "orderId": "ORD-100",
            "correlationId": "COR-200",
            "orderStatus": "TRADED",
            "transactionType": "BUY",
            "exchangeSegment": "NSE_FNO",
            "productType": "INTRADAY",
            "orderType": "LIMIT",
            "validity": "DAY",
            "securityId": "52432",
            "quantity": 50,
            "price": 245.50,
            "triggerPrice": 0.0,
            "tradedQuantity": 50,
            "tradedPrice": 246.00,
            "remainingQuantity": 0,
            "filledQty": 50,
            "averageTradedPrice": 246.00,
            "exchangeOrderId": "EX-999",
            "exchangeTime": "2026-03-22 10:30:00",
            "createTime": "2026-03-22 10:29:55",
            "updateTime": "2026-03-22 10:30:00",
            "rejectionReason": "",
            "tag": "strategy-1",
            "omsErrorCode": "",
            "omsErrorDescription": "",
            "tradingSymbol": "NIFTY-Mar2026-24500-CE",
            "drvExpiryDate": "2026-03-27",
            "drvOptionType": "CALL",
            "drvStrikePrice": 24500.0
        }"#;

        let resp: DhanOrderResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.order_id, "ORD-100");
        assert_eq!(resp.traded_quantity, 50);
        assert_eq!(resp.remaining_quantity, 0);
        assert_eq!(resp.filled_qty, 50);
        assert!((resp.average_traded_price - 246.0).abs() < f64::EPSILON);
        assert_eq!(resp.exchange_order_id, "EX-999");
        assert_eq!(resp.trading_symbol, "NIFTY-Mar2026-24500-CE");
        assert_eq!(resp.drv_option_type, "CALL");
        assert!((resp.drv_strike_price - 24500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dhan_order_response_defaults_for_missing_fields() {
        // Empty JSON object — all fields should get their defaults
        let json = r#"{}"#;
        let resp: DhanOrderResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.order_id, "");
        assert_eq!(resp.correlation_id, "");
        assert_eq!(resp.order_status, "");
        assert_eq!(resp.quantity, 0);
        assert_eq!(resp.price, 0.0);
        assert_eq!(resp.traded_quantity, 0);
        assert_eq!(resp.remaining_quantity, 0);
        assert_eq!(resp.oms_error_code, "");
        assert_eq!(resp.oms_error_description, "");
        assert_eq!(resp.drv_strike_price, 0.0);
    }

    #[test]
    fn dhan_position_response_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let pos: DhanPositionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(pos.dhan_client_id, "");
        assert_eq!(pos.security_id, "");
        assert_eq!(pos.position_type, "");
        assert_eq!(pos.net_qty, 0);
        assert_eq!(pos.buy_avg, 0.0);
        assert_eq!(pos.sell_avg, 0.0);
        assert_eq!(pos.realized_profit, 0.0);
        assert_eq!(pos.unrealized_profit, 0.0);
        assert_eq!(pos.multiplier, 0);
        assert!(!pos.cross_currency);
    }

    #[test]
    fn dhan_holding_response_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let holding: DhanHoldingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(holding.exchange, "");
        assert_eq!(holding.trading_symbol, "");
        assert_eq!(holding.security_id, "");
        assert_eq!(holding.isin, "");
        assert_eq!(holding.total_qty, 0);
        assert_eq!(holding.dp_qty, 0);
        assert_eq!(holding.t1_qty, 0);
        assert_eq!(holding.mtf_tq_qty, 0);
        assert_eq!(holding.mtf_qty, 0);
        assert_eq!(holding.available_qty, 0);
        assert_eq!(holding.collateral_qty, 0);
        assert_eq!(holding.avg_cost_price, 0.0);
        assert_eq!(holding.last_traded_price, 0.0);
    }

    #[test]
    fn margin_calculator_response_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let resp: MarginCalculatorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.total_margin, 0.0);
        assert_eq!(resp.span_margin, 0.0);
        assert_eq!(resp.exposure_margin, 0.0);
        assert_eq!(resp.available_balance, 0.0);
        assert_eq!(resp.variable_margin, 0.0);
        assert_eq!(resp.insufficient_balance, 0.0);
        assert_eq!(resp.brokerage, 0.0);
        assert_eq!(resp.leverage, "");
    }

    #[test]
    fn multi_margin_response_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let resp: MultiMarginResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.total_margin, "");
        assert_eq!(resp.span_margin, "");
        assert_eq!(resp.exposure_margin, "");
        assert_eq!(resp.equity_margin, "");
        assert_eq!(resp.fo_margin, "");
        assert_eq!(resp.commodity_margin, "");
        assert_eq!(resp.currency, "");
        assert_eq!(resp.hedge_benefit, "");
    }

    #[test]
    fn fund_limit_response_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let resp: FundLimitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.dhan_client_id, "");
        assert_eq!(resp.availabel_balance, 0.0);
        assert_eq!(resp.sod_limit, 0.0);
        assert_eq!(resp.collateral_amount, 0.0);
        assert_eq!(resp.receiveable_amount, 0.0);
        assert_eq!(resp.utilized_amount, 0.0);
        assert_eq!(resp.blocked_payout_amount, 0.0);
        assert_eq!(resp.withdrawable_balance, 0.0);
    }

    #[test]
    fn exit_all_response_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let resp: DhanExitAllResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "");
        assert_eq!(resp.message, "");
    }

    #[test]
    fn margin_script_serializes_camel_case() {
        let script = MarginScript {
            exchange_segment: "NSE_FNO".to_owned(),
            transaction_type: "BUY".to_owned(),
            quantity: 25,
            product_type: "INTRADAY".to_owned(),
            security_id: "11536".to_owned(),
            price: 100.0,
            trigger_price: 0.0,
        };
        let json = serde_json::to_string(&script).unwrap();
        assert!(json.contains("exchangeSegment"));
        assert!(json.contains("transactionType"));
        assert!(json.contains("productType"));
        assert!(json.contains("securityId"));
        assert!(json.contains("triggerPrice"));
        // Must not contain snake_case
        assert!(!json.contains("exchange_segment"));
        assert!(!json.contains("security_id"));
    }

    #[test]
    fn dhan_place_order_response_missing_correlation_id_defaults() {
        // correlationId has #[serde(default)], so missing field should default to ""
        let json = r#"{"orderId":"456","orderStatus":"PENDING"}"#;
        let resp: DhanPlaceOrderResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.order_id, "456");
        assert_eq!(resp.correlation_id, "");
    }

    #[test]
    fn managed_order_is_terminal_false_for_all_non_terminal() {
        let make_order = |status: OrderStatus| ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "c1".to_owned(),
            security_id: 100,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 0.0,
            trigger_price: 0.0,
            status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };

        // Exhaustive non-terminal check
        let non_terminal = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Confirmed,
            OrderStatus::PartTraded,
            OrderStatus::Triggered,
        ];
        for status in &non_terminal {
            assert!(
                !make_order(*status).is_terminal(),
                "{status:?} must NOT be terminal"
            );
        }

        // Exhaustive terminal check
        let terminal = [
            OrderStatus::Traded,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Expired,
            OrderStatus::Closed,
        ];
        for status in &terminal {
            assert!(
                make_order(*status).is_terminal(),
                "{status:?} must be terminal"
            );
        }
    }

    #[test]
    fn reconciliation_report_fields_can_be_set() {
        let report = ReconciliationReport {
            total_checked: 10,
            mismatches_found: 2,
            missing_from_oms: 1,
            missing_from_dhan: 0,
            mismatched_order_ids: vec!["ORD-1".to_owned(), "ORD-2".to_owned()],
        };
        assert_eq!(report.total_checked, 10);
        assert_eq!(report.mismatches_found, 2);
        assert_eq!(report.mismatched_order_ids.len(), 2);
    }

    #[test]
    fn managed_order_debug_contains_key_fields() {
        let order = ManagedOrder {
            order_id: "ORD-123".to_owned(),
            correlation_id: "COR-456".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Pending,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        let debug = format!("{order:?}");
        assert!(debug.contains("ORD-123"), "Debug must include order_id");
        assert!(debug.contains("52432"), "Debug must include security_id");
        assert!(debug.contains("Pending"), "Debug must include status");
    }
}
