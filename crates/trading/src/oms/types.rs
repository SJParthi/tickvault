//! OMS types — managed order, request/response structs, and error types.
//!
//! These types are OMS-internal. The external Dhan types live in `common::order_types`.
//! REST API request/response types use camelCase serde to match Dhan's REST format.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use tickvault_common::order_types::{
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
    pub security_id: u64,
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
    pub security_id: u64,
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
    /// I-P0-03: Expiry date for derivative contracts. When `Some`, the OMS
    /// validates that the contract has not expired before submitting to Dhan.
    /// `None` for equity orders or when expiry is unknown.
    pub expiry_date: Option<NaiveDate>,
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
    /// Quantity in lots (i32 per Dhan docs, but i64 for safety).
    pub quantity: i64,
    /// Product type (e.g., "INTRADAY", "MARGIN").
    pub product_type: String,
    /// Dhan security identifier (STRING).
    pub security_id: String,
    /// Order price.
    pub price: f64,
    /// Trigger price (for stop-loss orders). 0.0 for non-SL orders.
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
    /// Trigger price (0.0 for non-SL orders).
    pub trigger_price: f64,
}

/// Request body for `POST /v2/margincalculator/multi`.
///
/// 2026-07-14 note: the multi-margin request naming is a Dhan-side
/// THREE-artifact split (classic page vs portal markdown vs portal
/// OpenAPI yaml disagree — see `docs/dhan-ref/13-funds-margin.md`
/// "2026-07-14 Upstream Update"). We send the shape the official SDK
/// 2.3.0rc1 + the classic curl + the portal markdown CONVERGE on:
/// `{dhanClientId, includePosition, includeOrder, scripList}`. This is
/// UNVERIFIED-LIVE — live-probe the endpoint before the first production
/// multi-margin caller trusts either naming.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiMarginRequest {
    /// Dhan client ID (present in the SDK/classic-curl wire body; absent
    /// from the yaml's request schema — part of the unresolved split).
    pub dhan_client_id: String,
    /// Whether to include existing positions in calculation.
    pub include_position: bool,
    /// Whether to include pending orders in calculation. SINGULAR
    /// `includeOrder` on the wire (the SDK-converged shape) — the yaml's
    /// plural `includeOrders` is the LESS-supported reading.
    pub include_order: bool,
    /// Array of scripts (instruments) to calculate margin for.
    /// Serializes as `scripList` (the SDK-converged name; the yaml says
    /// `scripts` — unresolved Dhan-side).
    pub scrip_list: Vec<MarginScript>,
}

/// Response from `POST /v2/margincalculator/multi`.
///
/// 2026-07-14 note: Dhan's own artifacts DISAGREE on the response shape
/// (`docs/dhan-ref/13-funds-margin.md` "2026-07-14 Upstream Update"): the
/// classic page + OpenAPI yaml show snake_case ALL-STRING values
/// (`"total_margin": "150000.00"`), while the portal markdown shows
/// camelCase floats (`"totalMargin": 150000.0`). We tolerant-parse BOTH —
/// each margin field accepts a JSON number OR a numeric string, under
/// either casing (snake_case native name + camelCase `alias`), normalized
/// to `f64`. A NON-numeric string is a hard deserialize error (fail-closed
/// — a garbage margin value must never silently become 0.0). Absent
/// fields default to 0.0.
#[derive(Debug, Clone, Deserialize)]
pub struct MultiMarginResponse {
    /// Total margin required (normalized to f64).
    #[serde(
        default,
        alias = "totalMargin",
        deserialize_with = "de_f64_from_string_or_number"
    )]
    pub total_margin: f64,
    /// SPAN margin (normalized to f64).
    #[serde(
        default,
        alias = "spanMargin",
        deserialize_with = "de_f64_from_string_or_number"
    )]
    pub span_margin: f64,
    /// Exposure margin (normalized to f64).
    #[serde(
        default,
        alias = "exposureMargin",
        deserialize_with = "de_f64_from_string_or_number"
    )]
    pub exposure_margin: f64,
    /// Equity margin (normalized to f64).
    #[serde(
        default,
        alias = "equityMargin",
        deserialize_with = "de_f64_from_string_or_number"
    )]
    pub equity_margin: f64,
    /// F&O margin (normalized to f64).
    #[serde(
        default,
        alias = "foMargin",
        deserialize_with = "de_f64_from_string_or_number"
    )]
    pub fo_margin: f64,
    /// Commodity margin (normalized to f64).
    #[serde(
        default,
        alias = "commodityMargin",
        deserialize_with = "de_f64_from_string_or_number"
    )]
    pub commodity_margin: f64,
    /// Currency field — kept as a RAW string. The classic live example
    /// shows `"INR"` (a currency CODE) while the portal markdown types it
    /// as a float margin amount — semantics Unknown until live-probed
    /// (2026-07-14 split, ground-truth doc item 3). Snake_case and
    /// camelCase are the same word here, so no alias is needed.
    #[serde(default)]
    pub currency: String,
    /// Hedge benefit from offsetting positions (normalized to f64;
    /// absent from the portal markdown's response → defaults to 0.0).
    #[serde(
        default,
        alias = "hedgeBenefit",
        deserialize_with = "de_f64_from_string_or_number"
    )]
    pub hedge_benefit: f64,
}

/// Tolerant f64 deserializer for the multi-margin response's split wire
/// shapes (2026-07-14): accepts a JSON number (the portal-markdown
/// camelCase-float shape, integers included) OR a numeric string (the
/// classic/yaml snake_case-string shape like `"150000.00"`). Fail-closed
/// on garbage: a NON-numeric string is a hard deserialize error, and a
/// NON-FINITE value is too — Rust's `f64` parser accepts `"NaN"` /
/// `"inf"` / `"-inf"`, but a non-finite margin must never round-trip
/// toward a verdict. Any echoed input text in the error message is
/// bounded to ≤ 32 chars so a hostile/oversized wire value can never
/// balloon a log line.
fn de_f64_from_string_or_number<'de, D>(de: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        F64(f64),
        Str(String),
    }
    let value = match NumOrStr::deserialize(de)? {
        NumOrStr::F64(value) => value,
        NumOrStr::Str(text) => text.parse::<f64>().map_err(|_| {
            // Bounded echo — chars (not bytes) so the cut is UTF-8-safe.
            let echoed: String = text.chars().take(32).collect();
            serde::de::Error::custom(format!(
                "non-numeric margin value {echoed:?} — refusing to coerce to 0.0"
            ))
        })?,
    };
    if !value.is_finite() {
        return Err(serde::de::Error::custom(
            "non-finite margin value — refusing to coerce to 0.0",
        ));
    }
    Ok(value)
}

/// Order intent for the pre-trade margin gate (umbrella plan cluster E2).
///
/// `Entry` carries the margin the gate computed for the order (integer
/// paise); `Exit` is never margin-gated — an exit must always be
/// placeable. The future `RiskEngine::check_order` integration takes this
/// type (rebases after cluster A's risk-engine round merges).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderIntent {
    /// Opens or increases exposure — margin-gated.
    Entry {
        /// Margin required for the entry, in integer paise.
        required_paise: i64,
    },
    /// Closes or reduces exposure — NEVER margin-gated.
    Exit,
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
    /// Orders to execute when condition fires (up to 15 per the 2026-07-14
    /// live portal callout).
    pub orders: Vec<TriggerOrder>,
    /// Alert ID echo — documented OPTIONAL body field on Modify only
    /// (PORTAL modify page, 2026-07-14 crawl). Always `None` on Place;
    /// `skip_serializing_if` keeps Place bodies byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_id: Option<String>,
}

/// Conditional trigger response.
/// Endpoint: `POST/PUT/DELETE/GET /v2/alerts/orders`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanConditionalTriggerResponse {
    /// Alert ID (`null` tolerated — the GET echo family emits explicit
    /// `null` for unset fields, e.g. the verbatim `"triggeredTime": null`).
    #[serde(default, deserialize_with = "null_to_default")]
    pub alert_id: String,
    /// Alert status: "ACTIVE", "TRIGGERED", "EXPIRED", "CANCELLED".
    #[serde(default, deserialize_with = "null_to_default")]
    pub alert_status: String,
    /// Creation time — ISO-8601 **UTC `Z`** format ("2019-08-24T14:15:22Z").
    /// The ONLY UTC timestamps in the trading-API family (everything else is
    /// IST strings). Convert; NEVER assume IST. GET responses only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_time: Option<String>,
    /// Trigger time — ISO-8601 UTC `Z`; `null` until the alert triggers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggered_time: Option<String>,
    /// Last traded price — STRING in the doc example ("245.50"), number in
    /// the OpenAPI yaml. Tolerant either way via [`DhanNumeric`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_price: Option<DhanNumeric>,
    /// Condition echo on GET responses (string `comparingValue` safe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<TriggerConditionDetail>,
    /// Order-leg echo on GET responses — the tolerant [`TriggerOrderDetail`]
    /// mirror, NEVER the strict request-side [`TriggerOrder`] (one
    /// non-conforming leg must never brick the whole GET / GET-all parse;
    /// the OpenAPI yaml marks NO order sub-field required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orders: Option<Vec<TriggerOrderDetail>>,
}

// ---------------------------------------------------------------------------
// Multi Order + Conditional Detail Types — 2026-07-14 (07c §multi + portal/yaml)
// ---------------------------------------------------------------------------

/// Tolerant Dhan numeric: bare number OR quoted decimal string on the wire
/// (comparingValue is number-in-request / string-in-response; lastPrice is
/// string-in-example / number-in-yaml). Round-trips verbatim; never panics.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum DhanNumeric {
    /// Bare JSON number (e.g. `245.5`).
    Num(f64),
    /// Quoted decimal string (e.g. `"245.50"`), preserved verbatim.
    Text(String),
}

impl DhanNumeric {
    /// Best-effort f64 view; `None` for unparsable text.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(value) => Some(*value),
            Self::Text(text) => text.trim().parse().ok(),
        }
    }

    /// Best-effort i64 view; `None` for unparsable text. A fractional value
    /// truncates toward zero and an out-of-range value saturates (Rust `as`
    /// casts never panic) — response-echo use only, never a request input.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Num(value) => Some(*value as i64),
            Self::Text(text) => {
                let trimmed = text.trim();
                trimmed
                    .parse::<i64>()
                    .ok()
                    .or_else(|| trimmed.parse::<f64>().ok().map(|value| value as i64))
            }
        }
    }
}

/// Deserializes a possibly-explicit-`null` field into `T::default()`.
///
/// `#[serde(default)]` alone tolerates only a MISSING key; the /alerts GET
/// echo family emits EXPLICIT `null` for unset fields (verbatim doc example:
/// `"triggeredTime": null`), which would otherwise brick the WHOLE GET /
/// GET-all parse over one non-conforming leg. Response-only mirrors use this
/// on every defaulted non-`Option` field.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Deserializes a quantity echo that may arrive as a bare number, a quoted
/// decimal string (the family's documented number/string wobble class —
/// `comparingValue` / `lastPrice`), or explicit `null`. Unparsable text
/// degrades to 0 — response-only mirror, never a request input.
fn tolerant_quantity<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<DhanNumeric>::deserialize(deserializer)?
        .and_then(|value| value.as_i64())
        .unwrap_or_default())
}

/// Response-only mirror of [`TriggerCondition`] — every field defaulted /
/// optional, `comparing_value` tolerant (the GET response carries
/// `"comparingValue": "250.00"` as a STRING; parsing it into the request
/// struct's `Option<f64>` would fail).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerConditionDetail {
    /// Comparison type echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub comparison_type: String,
    /// Exchange segment echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub exchange_segment: String,
    /// Security ID echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub security_id: String,
    /// Indicator name echo (TECHNICAL_WITH_* only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indicator_name: Option<String>,
    /// Timeframe echo (TECHNICAL_WITH_* only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_frame: Option<String>,
    /// Operator echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub operator: String,
    /// Comparing value — number in the request, STRING in the GET response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparing_value: Option<DhanNumeric>,
    /// Second indicator echo (TECHNICAL_WITH_INDICATOR only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparing_indicator_name: Option<String>,
    /// Expiry date echo (YYYY-MM-DD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<String>,
    /// Frequency: "ONCE" | yaml-only "ALWAYS" | anything — String tolerates all.
    #[serde(default, deserialize_with = "null_to_default")]
    pub frequency: String,
    /// User note echo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_note: Option<String>,
}

/// Response-only mirror of [`TriggerOrder`] — every field defaulted and the
/// price-family fields tolerant ([`DhanNumeric`]). The GET / GET-all order
/// echo must be parsed DEFENSIVELY: the OpenAPI yaml marks NO order
/// sub-field required (an alert created via Dhan's web/app UI can omit the
/// request-optional `triggerPrice`/`discQuantity`), and the family's
/// documented number/string wobble (`comparingValue`/`lastPrice`) applies
/// to the leg's `price` fields too. Parsing the echo with the strict
/// request-side struct would fail the ENTIRE response — and the whole
/// account list on GET-all — over ONE such leg.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerOrderDetail {
    /// "BUY" or "SELL" echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub transaction_type: String,
    /// Exchange segment echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub exchange_segment: String,
    /// Product type echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub product_type: String,
    /// Order type echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub order_type: String,
    /// Security ID echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub security_id: String,
    /// Quantity echo — number, quoted decimal string (the family's
    /// documented wobble class), or explicit `null` all tolerated.
    #[serde(default, deserialize_with = "tolerant_quantity")]
    pub quantity: i64,
    /// Validity echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub validity: String,
    /// Price — STRING in the doc example ("250.00"); tolerant to a bare
    /// number (the family's documented wobble class).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<DhanNumeric>,
    /// Disclosed quantity echo (STRING "0" in the doc example).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_quantity: Option<DhanNumeric>,
    /// Trigger price echo — request-optional, so the echo may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_price: Option<DhanNumeric>,
}

/// Multi-order WIRE leg — DISTINCT schema from [`TriggerOrder`] (07c §9.1
/// traps): `disclosedQuantity` INT (vs `discQuantity` STRING), `price` /
/// `triggerPrice` FLOAT (vs STRING), plus `sequence` / `correlationId` /
/// `afterMarketOrder` / `amoTime`. NEVER merge the two structs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiOrderLeg {
    /// Sequence number "1".."15" — constructor-assigned (STRING on the wire).
    pub sequence: String,
    /// User-generated tracking ID (<= 30 chars, `[a-zA-Z0-9 _-]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// "BUY" or "SELL".
    pub transaction_type: String,
    /// Exchange segment — constructor-locked to "NSE_EQ"/"BSE_EQ".
    pub exchange_segment: String,
    /// Product type.
    pub product_type: String,
    /// Order type.
    pub order_type: String,
    /// Validity: "DAY" or "IOC".
    pub validity: String,
    /// Dhan security ID (STRING).
    pub security_id: String,
    /// Quantity.
    pub quantity: i64,
    /// After-market-order flag — coupled with `amo_time` by the builder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_market_order: Option<bool>,
    /// AMO timing (`AmoTime::as_str()`); coupled with the flag by the builder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amo_time: Option<String>,
    /// Price — FLOAT here, vs STRING on conditional legs.
    pub price: f64,
    /// Trigger price — FLOAT here, vs STRING on conditional legs.
    pub trigger_price: f64,
    /// Disclosed quantity — INT `disclosedQuantity`, vs STRING `discQuantity`.
    pub disclosed_quantity: i64,
}
// NOTE: yaml marks only sequence/transactionType/exchangeSegment required; our
// builder always emits full legs, so the remaining fields are concrete (not
// Option). Documented divergence, request-side only.

/// Place Multi Order request. Endpoint: `POST /v2/alerts/multi/orders`
/// (PORTAL-only page). Order legs: Equities ONLY, fail-closed (see
/// `oms::conditional`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanMultiOrderRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// Order legs (1..=15, builder-enforced).
    pub orders: Vec<MultiOrderLeg>,
}

/// Multi-order response — YAML-ONLY schema, UNVERIFIED-LIVE. Parse
/// tolerantly. `Default` (empty `orders`) is the documented-plausible
/// BODYLESS-200 arm: the PORTAL page documents NO response body at all
/// ("200 Successful operation" only), and `place_multi_order` degrades an
/// empty/whitespace 200 body to this default instead of a `JsonError`
/// brick — a parse error for an already-placed batch would push callers
/// toward a double-placing retry.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhanMultiOrderResponse {
    /// Per-leg order results (`null` tolerated → empty — a 200 body means
    /// the legs are ALREADY placed at the broker; a parse brick here would
    /// hide which legs went live).
    #[serde(default, deserialize_with = "null_to_default")]
    pub orders: Vec<MultiOrderLegResult>,
}

/// One per-leg result in the multi-order response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiOrderLegResult {
    /// Dhan order ID for this leg — a REJECTED leg plausibly echoes
    /// `"orderId": null` (yaml-only, UNVERIFIED-LIVE); tolerated as `""`.
    #[serde(default, deserialize_with = "null_to_default")]
    pub order_id: String,
    /// Sequence number echo.
    #[serde(default, deserialize_with = "null_to_default")]
    pub sequence: String,
    /// 10-value yaml enum (TRANSIT PENDING REJECTED CANCELLED PART_TRADED
    /// TRADED EXPIRED MODIFIED TRIGGERED INACTIVE) — UNVERIFIED-LIVE; kept a
    /// plain String so unknown values NEVER panic (annexure rule 15).
    #[serde(default, deserialize_with = "null_to_default")]
    pub order_status: String,
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

    /// Sandbox enforcement: live orders blocked before cutover date.
    #[error("sandbox enforcement: live orders blocked until 2026-07-01")]
    SandboxEnforcement,

    /// I-P0-03: Order rejected because the derivative contract has expired.
    #[error("expired contract: security_id {security_id} expired on {expiry_date}")]
    ExpiredContract {
        security_id: u64,
        expiry_date: String,
    },

    /// Maximum modifications per order exceeded.
    #[error("max modifications ({max}) exceeded for order {order_id}")]
    MaxModificationsExceeded { order_id: String, max: u32 },

    /// The /alerts/* client surface is DISARMED (hardcoded default). No HTTP was
    /// attempted. Arming is #[cfg(test)]-only until a dated operator-quote
    /// activation PR exists.
    #[error("alerts surface disarmed: /alerts request '{operation}' refused (dormant surface)")]
    AlertsSurfaceDisarmed { operation: &'static str },
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
            dhan_client_id: "TEST-100".to_owned(),
            include_position: true,
            include_order: false,
            scrip_list: vec![MarginScript {
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
        // The 2026-07-14 SDK/classic-curl-converged wire shape.
        assert!(json.contains("dhanClientId"));
        assert!(json.contains("includePosition"));
        assert!(json.contains("includeOrder"));
        assert!(json.contains("scripList"));
        // The yaml's LESS-supported reading must NOT be emitted.
        assert!(!json.contains("includeOrders"));
        assert!(!json.contains("\"scripts\""));
    }

    #[test]
    fn test_multi_margin_response_snake_case_strings_normalize() {
        // The classic/yaml shape: snake_case, all-string values.
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
        assert!((resp.total_margin - 12500.00).abs() < 1e-9);
        assert!((resp.span_margin - 10000.00).abs() < 1e-9);
        assert!((resp.exposure_margin - 2500.00).abs() < 1e-9);
        assert!((resp.fo_margin - 12500.00).abs() < 1e-9);
        assert!(resp.commodity_margin.abs() < 1e-9);
        assert!((resp.hedge_benefit - 500.00).abs() < 1e-9);
        // `currency` stays RAW (semantics Unknown — 2026-07-14 split).
        assert_eq!(resp.currency, "0.00");
    }

    #[test]
    fn test_multi_margin_response_camel_case_floats_normalize() {
        // The portal-markdown shape: camelCase, float values, no
        // hedge_benefit field (absent → 0.0 via serde default).
        let json = r#"{
            "totalMargin": 150000.0,
            "spanMargin": 50000.0,
            "exposureMargin": 45000.0
        }"#;

        let resp: MultiMarginResponse = serde_json::from_str(json).unwrap();
        assert!((resp.total_margin - 150000.0).abs() < 1e-9);
        assert!((resp.span_margin - 50000.0).abs() < 1e-9);
        assert!((resp.exposure_margin - 45000.0).abs() < 1e-9);
        assert!(resp.hedge_benefit.abs() < 1e-9);
        assert!(resp.equity_margin.abs() < 1e-9);
    }

    #[test]
    fn test_multi_margin_response_rejects_garbage_string() {
        // Fail-closed: a non-numeric margin string is a hard deserialize
        // error — never silently coerced to 0.0.
        let json = r#"{"total_margin": "abc"}"#;
        let result: Result<MultiMarginResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
        // Bounded echo: a long garbage value must NOT round-trip verbatim
        // into the error text (≤ 32 echoed chars).
        let long_garbage = "x".repeat(500);
        let json = format!(r#"{{"total_margin": "{long_garbage}"}}"#);
        let err = serde_json::from_str::<MultiMarginResponse>(&json).unwrap_err();
        assert!(
            !err.to_string().contains(&long_garbage),
            "error text must not carry the full 500-char garbage value"
        );
    }

    #[test]
    fn test_multi_margin_response_rejects_nan_and_inf_strings() {
        // Rust's f64 parser ACCEPTS these — the deserializer must still
        // hard-fail (a non-finite margin can never reach a verdict).
        for bad in ["NaN", "inf", "-inf", "Infinity", "-infinity"] {
            let json = format!(r#"{{"total_margin": "{bad}"}}"#);
            let result: Result<MultiMarginResponse, _> = serde_json::from_str(&json);
            assert!(result.is_err(), "{bad:?} must be a hard deserialize error");
        }
    }

    #[test]
    fn test_multi_margin_response_integer_json_number_normalizes() {
        // A bare JSON INTEGER (no decimal point) normalizes to f64.
        let json = r#"{"total_margin": 150000}"#;
        let resp: MultiMarginResponse = serde_json::from_str(json).unwrap();
        assert!((resp.total_margin - 150000.0).abs() < 1e-9);
    }

    #[test]
    fn test_multi_margin_response_currency_inr_raw() {
        // The classic live example shows a currency CODE — must parse and
        // survive verbatim (kept raw; semantics Unknown until live-probed).
        let json = r#"{"currency": "INR"}"#;
        let resp: MultiMarginResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.currency, "INR");
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
        assert!(resp.total_margin.abs() < 1e-9);
        assert!(resp.span_margin.abs() < 1e-9);
        assert!(resp.exposure_margin.abs() < 1e-9);
        assert!(resp.equity_margin.abs() < 1e-9);
        assert!(resp.fo_margin.abs() < 1e-9);
        assert!(resp.commodity_margin.abs() < 1e-9);
        assert_eq!(resp.currency, "");
        assert!(resp.hedge_benefit.abs() < 1e-9);
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

    // -----------------------------------------------------------------------
    // Kill Switch Types Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_kill_switch_response_deserialize() {
        let json = r#"{"dhanClientId":"1100003626","killSwitchStatus":"ACTIVATE"}"#;
        let resp: KillSwitchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.kill_switch_status, "ACTIVATE");
        assert_eq!(resp.dhan_client_id, "1100003626");
    }

    #[test]
    fn test_kill_switch_response_deactivate() {
        let json = r#"{"dhanClientId":"1100003626","killSwitchStatus":"DEACTIVATE"}"#;
        let resp: KillSwitchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.kill_switch_status, "DEACTIVATE");
    }

    #[test]
    fn test_kill_switch_response_missing_fields_default() {
        let json = r#"{}"#;
        let resp: KillSwitchResponse = serde_json::from_str(json).unwrap();
        assert!(resp.kill_switch_status.is_empty());
    }

    // -----------------------------------------------------------------------
    // P&L Exit Types Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pnl_exit_request_serialize() {
        let req = PnlExitRequest {
            profit_value: "1500.00".to_string(),
            loss_value: "500.00".to_string(),
            product_type: vec!["INTRADAY".to_string(), "DELIVERY".to_string()],
            enable_kill_switch: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"profitValue\":\"1500.00\""));
        assert!(json.contains("\"lossValue\":\"500.00\""));
        assert!(json.contains("\"enableKillSwitch\":true"));
        assert!(json.contains("DELIVERY"));
    }

    #[test]
    fn test_pnl_exit_request_string_values_not_floats() {
        let req = PnlExitRequest {
            profit_value: "1500.00".to_string(),
            loss_value: "500.00".to_string(),
            product_type: vec!["INTRADAY".to_string()],
            enable_kill_switch: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        // Values must be strings, not numbers
        assert!(json.contains("\"1500.00\""));
        assert!(json.contains("\"500.00\""));
    }

    #[test]
    fn test_pnl_exit_response_deserialize() {
        let json = r#"{"pnlExitStatus":"ACTIVE","message":"P&L based exit configured"}"#;
        let resp: PnlExitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.pnl_exit_status, "ACTIVE");
    }

    #[test]
    fn test_pnl_exit_status_response_different_field_names() {
        // GET response uses different field names from POST request
        let json = r#"{"pnlExitStatus":"ACTIVE","profit":"1500.00","loss":"500.00","productType":["INTRADAY"],"enable_kill_switch":true}"#;
        let resp: PnlExitStatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.profit, "1500.00"); // Short name, not profitValue
        assert_eq!(resp.loss, "500.00"); // Short name, not lossValue
        assert!(resp.enable_kill_switch); // snake_case, not camelCase
    }

    // -----------------------------------------------------------------------
    // Super Order Types Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_order_leg_serialize() {
        assert_eq!(
            serde_json::to_string(&OrderLeg::EntryLeg).unwrap(),
            "\"ENTRY_LEG\""
        );
        assert_eq!(
            serde_json::to_string(&OrderLeg::TargetLeg).unwrap(),
            "\"TARGET_LEG\""
        );
        assert_eq!(
            serde_json::to_string(&OrderLeg::StopLossLeg).unwrap(),
            "\"STOP_LOSS_LEG\""
        );
    }

    #[test]
    fn test_order_leg_deserialize() {
        let entry: OrderLeg = serde_json::from_str("\"ENTRY_LEG\"").unwrap();
        assert_eq!(entry, OrderLeg::EntryLeg);
        let target: OrderLeg = serde_json::from_str("\"TARGET_LEG\"").unwrap();
        assert_eq!(target, OrderLeg::TargetLeg);
        let sl: OrderLeg = serde_json::from_str("\"STOP_LOSS_LEG\"").unwrap();
        assert_eq!(sl, OrderLeg::StopLossLeg);
    }

    #[test]
    fn test_order_leg_as_str() {
        assert_eq!(OrderLeg::EntryLeg.as_str(), "ENTRY_LEG");
        assert_eq!(OrderLeg::TargetLeg.as_str(), "TARGET_LEG");
        assert_eq!(OrderLeg::StopLossLeg.as_str(), "STOP_LOSS_LEG");
    }

    #[test]
    fn test_super_order_request_serialize() {
        let req = DhanPlaceSuperOrderRequest {
            dhan_client_id: "1000000003".to_string(),
            correlation_id: "abc123".to_string(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            security_id: "11536".to_string(),
            quantity: 5,
            price: 1500.0,
            target_price: 1600.0,
            stop_loss_price: 1400.0,
            trailing_jump: 10.0,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"targetPrice\":1600"));
        assert!(json.contains("\"stopLossPrice\":1400"));
        assert!(json.contains("\"trailingJump\":10"));
        assert!(json.contains("\"securityId\":\"11536\"")); // STRING not number
    }

    #[test]
    fn test_super_order_trailing_jump_zero_cancels_trailing() {
        let req = DhanPlaceSuperOrderRequest {
            dhan_client_id: "1".to_string(),
            correlation_id: String::new(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            security_id: "11536".to_string(),
            quantity: 1,
            price: 100.0,
            target_price: 110.0,
            stop_loss_price: 90.0,
            trailing_jump: 0.0, // Explicitly cancels trailing
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"trailingJump\":0")); // Must be 0, not omitted
    }

    #[test]
    fn test_modify_super_order_target_only_price() {
        let req = DhanModifySuperOrderRequest {
            dhan_client_id: "1".to_string(),
            leg_name: "TARGET_LEG".to_string(),
            order_type: None,
            quantity: None,
            price: None,
            target_price: Some(1650.0),
            stop_loss_price: None,
            trailing_jump: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"targetPrice\":1650"));
        assert!(!json.contains("quantity")); // Skipped
        assert!(!json.contains("price\":")); // Skipped (not targetPrice)
    }

    #[test]
    fn test_modify_super_order_sl_only_sl_and_trail() {
        let req = DhanModifySuperOrderRequest {
            dhan_client_id: "1".to_string(),
            leg_name: "STOP_LOSS_LEG".to_string(),
            order_type: None,
            quantity: None,
            price: None,
            target_price: None,
            stop_loss_price: Some(1380.0),
            trailing_jump: Some(5.0),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"stopLossPrice\":1380"));
        assert!(json.contains("\"trailingJump\":5"));
        assert!(!json.contains("\"quantity\"")); // Skipped
    }

    #[test]
    fn test_super_order_leg_detail_deserialize() {
        let json = r#"{"orderId":"ORD-1","legName":"ENTRY_LEG","transactionType":"BUY","remainingQuantity":5,"triggeredQuantity":0,"price":1500.0,"orderStatus":"PENDING","trailingJump":10.0}"#;
        let leg: SuperOrderLegDetail = serde_json::from_str(json).unwrap();
        assert_eq!(leg.order_id, "ORD-1");
        assert_eq!(leg.leg_name, "ENTRY_LEG");
        assert_eq!(leg.remaining_quantity, 5);
    }

    // -----------------------------------------------------------------------
    // Forever Order Types Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_forever_order_flag_serialize() {
        assert_eq!(
            serde_json::to_string(&ForeverOrderFlag::Single).unwrap(),
            "\"SINGLE\""
        );
        assert_eq!(
            serde_json::to_string(&ForeverOrderFlag::Oco).unwrap(),
            "\"OCO\""
        );
    }

    #[test]
    fn test_forever_order_single_no_second_leg() {
        let req = DhanForeverOrderRequest {
            dhan_client_id: "1".to_string(),
            correlation_id: String::new(),
            order_flag: "SINGLE".to_string(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            validity: "DAY".to_string(),
            security_id: "1333".to_string(),
            quantity: 5,
            disclosed_quantity: 0,
            price: 1428.0,
            trigger_price: 1427.0,
            price1: None,
            trigger_price1: None,
            quantity1: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("price1")); // Omitted for SINGLE
        assert!(!json.contains("triggerPrice1")); // Omitted for SINGLE
        assert!(!json.contains("quantity1")); // Omitted for SINGLE
    }

    #[test]
    fn test_forever_order_oco_has_second_leg() {
        let req = DhanForeverOrderRequest {
            dhan_client_id: "1".to_string(),
            correlation_id: String::new(),
            order_flag: "OCO".to_string(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            validity: "DAY".to_string(),
            security_id: "1333".to_string(),
            quantity: 5,
            disclosed_quantity: 0,
            price: 1428.0,
            trigger_price: 1427.0,
            price1: Some(1420.0),
            trigger_price1: Some(1419.0),
            quantity1: Some(10),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"price1\":1420")); // Present for OCO
        assert!(json.contains("\"triggerPrice1\":1419"));
        assert!(json.contains("\"quantity1\":10"));
    }

    // -----------------------------------------------------------------------
    // Conditional Trigger Types Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_comparison_type_serialize() {
        assert_eq!(
            serde_json::to_string(&ComparisonType::TechnicalWithValue).unwrap(),
            "\"TECHNICAL_WITH_VALUE\""
        );
        assert_eq!(
            serde_json::to_string(&ComparisonType::PriceWithValue).unwrap(),
            "\"PRICE_WITH_VALUE\""
        );
    }

    #[test]
    fn test_trigger_operator_all_9_variants() {
        let operators = [
            TriggerOperator::CrossingUp,
            TriggerOperator::CrossingDown,
            TriggerOperator::CrossingAnySide,
            TriggerOperator::GreaterThan,
            TriggerOperator::LessThan,
            TriggerOperator::GreaterThanEqual,
            TriggerOperator::LessThanEqual,
            TriggerOperator::Equal,
            TriggerOperator::NotEqual,
        ];
        assert_eq!(operators.len(), 9);
        for op in &operators {
            let json = serde_json::to_string(op).unwrap();
            assert!(!json.is_empty());
        }
    }

    #[test]
    fn test_trigger_timeframe_all_4_variants() {
        let timeframes = [
            TriggerTimeFrame::Day,
            TriggerTimeFrame::OneMin,
            TriggerTimeFrame::FiveMin,
            TriggerTimeFrame::FifteenMin,
        ];
        assert_eq!(timeframes.len(), 4);
        assert_eq!(
            serde_json::to_string(&TriggerTimeFrame::Day).unwrap(),
            "\"DAY\""
        );
        assert_eq!(
            serde_json::to_string(&TriggerTimeFrame::OneMin).unwrap(),
            "\"ONE_MIN\""
        );
    }

    #[test]
    fn test_trigger_condition_frequency_default() {
        let json = r#"{"comparisonType":"PRICE_WITH_VALUE","exchangeSegment":"NSE_EQ","securityId":"1333","operator":"GREATER_THAN"}"#;
        let cond: TriggerCondition = serde_json::from_str(json).unwrap();
        assert_eq!(cond.frequency, "ONCE"); // Default
    }

    #[test]
    fn test_conditional_trigger_request_serialize() {
        let req = DhanConditionalTriggerRequest {
            dhan_client_id: "1".to_string(),
            condition: TriggerCondition {
                comparison_type: "TECHNICAL_WITH_VALUE".to_string(),
                exchange_segment: "NSE_EQ".to_string(),
                security_id: "12345".to_string(),
                indicator_name: Some("SMA_5".to_string()),
                time_frame: Some("DAY".to_string()),
                operator: "CROSSING_UP".to_string(),
                comparing_value: Some(250.0),
                comparing_indicator_name: None,
                exp_date: None,
                frequency: "ONCE".to_string(),
                user_note: None,
            },
            orders: vec![TriggerOrder {
                transaction_type: "BUY".to_string(),
                exchange_segment: "NSE_EQ".to_string(),
                product_type: "CNC".to_string(),
                order_type: "LIMIT".to_string(),
                security_id: "12345".to_string(),
                quantity: 10,
                validity: "DAY".to_string(),
                price: "250.00".to_string(),
                disc_quantity: "0".to_string(),
                trigger_price: "0".to_string(),
            }],
            alert_id: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("SMA_5"));
        assert!(json.contains("CROSSING_UP"));
        assert!(json.contains("ONCE"));
        // Place semantics: the optional Modify-only alertId echo must be
        // ABSENT — Place bodies stay byte-identical to the pre-2026-07-14 shape.
        assert!(!json.contains("alertId"));
    }

    #[test]
    fn test_modify_request_alert_id_absent_when_none() {
        let req = DhanConditionalTriggerRequest {
            dhan_client_id: "1".to_string(),
            condition: TriggerCondition {
                comparison_type: "PRICE_WITH_VALUE".to_string(),
                exchange_segment: "NSE_EQ".to_string(),
                security_id: "1333".to_string(),
                indicator_name: None,
                time_frame: None,
                operator: "GREATER_THAN".to_string(),
                comparing_value: Some(250.0),
                comparing_indicator_name: None,
                exp_date: None,
                frequency: "ONCE".to_string(),
                user_note: None,
            },
            orders: vec![],
            alert_id: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("alertId"),
            "None alert_id must serialize away"
        );
    }

    #[test]
    fn test_modify_request_alert_id_present_when_set() {
        let req = DhanConditionalTriggerRequest {
            dhan_client_id: "1".to_string(),
            condition: TriggerCondition {
                comparison_type: "PRICE_WITH_VALUE".to_string(),
                exchange_segment: "NSE_EQ".to_string(),
                security_id: "1333".to_string(),
                indicator_name: None,
                time_frame: None,
                operator: "GREATER_THAN".to_string(),
                comparing_value: Some(250.0),
                comparing_indicator_name: None,
                exp_date: None,
                frequency: "ONCE".to_string(),
                user_note: None,
            },
            orders: vec![],
            alert_id: Some("12345".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"alertId\":\"12345\""));
    }

    // -----------------------------------------------------------------------
    // Multi Order + Conditional Detail Types Tests (2026-07-14)
    // -----------------------------------------------------------------------

    #[test]
    fn test_multi_order_request_serializes_camel_case_exact() {
        let req = DhanMultiOrderRequest {
            dhan_client_id: "100".to_string(),
            orders: vec![MultiOrderLeg {
                sequence: "1".to_string(),
                correlation_id: Some("corr-1".to_string()),
                transaction_type: "BUY".to_string(),
                exchange_segment: "NSE_EQ".to_string(),
                product_type: "CNC".to_string(),
                order_type: "LIMIT".to_string(),
                validity: "DAY".to_string(),
                security_id: "1333".to_string(),
                quantity: 10,
                after_market_order: Some(true),
                amo_time: Some("OPEN".to_string()),
                price: 250.0,
                trigger_price: 0.0,
                disclosed_quantity: 0,
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"dhanClientId\":\"100\""));
        assert!(json.contains("\"sequence\":\"1\""));
        assert!(json.contains("\"correlationId\":\"corr-1\""));
        assert!(json.contains("\"afterMarketOrder\":true"));
        assert!(json.contains("\"amoTime\":\"OPEN\""));
        // disclosedQuantity is an INT here (multi wire), never the conditional
        // leg's STRING `discQuantity` — the §9.1 trap pinned both ways.
        assert!(json.contains("\"disclosedQuantity\":0"));
        assert!(!json.contains("discQuantity"));
        assert!(!json.contains("disc_quantity"));
        // Prices are bare FLOATS on the multi wire.
        assert!(json.contains("\"price\":250.0"));
        assert!(json.contains("\"triggerPrice\":0.0"));
    }

    #[test]
    fn test_conditional_vs_multi_price_type_split() {
        // Same leg data: TriggerOrder emits a STRING price, MultiOrderLeg a number.
        let conditional_leg = TriggerOrder {
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            security_id: "1333".to_string(),
            quantity: 10,
            validity: "DAY".to_string(),
            price: "250.00".to_string(),
            disc_quantity: "0".to_string(),
            trigger_price: "0".to_string(),
        };
        let multi_leg = MultiOrderLeg {
            sequence: "1".to_string(),
            correlation_id: None,
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            validity: "DAY".to_string(),
            security_id: "1333".to_string(),
            quantity: 10,
            after_market_order: None,
            amo_time: None,
            price: 250.0,
            trigger_price: 0.0,
            disclosed_quantity: 0,
        };
        let conditional_json = serde_json::to_string(&conditional_leg).unwrap();
        let multi_json = serde_json::to_string(&multi_leg).unwrap();
        assert!(conditional_json.contains("\"price\":\"250.00\"")); // STRING
        assert!(multi_json.contains("\"price\":250.0")); // FLOAT
        // AMO fields absent when None (never fabricated).
        assert!(!multi_json.contains("afterMarketOrder"));
        assert!(!multi_json.contains("amoTime"));
    }

    #[test]
    fn test_multi_order_response_parses_unknown_order_status_modified_inactive_no_panic() {
        // yaml sample statuses beyond the repo's 9-variant OrderStatus, plus
        // a fabricated garbage value — all must parse (annexure rule 15).
        let json = r#"{"orders":[
            {"orderId":"O1","sequence":"1","orderStatus":"MODIFIED"},
            {"orderId":"O2","sequence":"2","orderStatus":"INACTIVE"},
            {"orderId":"O3","sequence":"3","orderStatus":"FROZEN"}
        ]}"#;
        let resp: DhanMultiOrderResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.orders.len(), 3);
        assert_eq!(resp.orders[0].order_status, "MODIFIED");
        assert_eq!(resp.orders[1].order_status, "INACTIVE");
        assert_eq!(resp.orders[2].order_status, "FROZEN");
    }

    #[test]
    fn test_multi_order_response_empty_object_defaults() {
        // An empty JSON OBJECT (`{}`) defaults every field. (A truly EMPTY
        // 200 body — the PORTAL-documented no-body shape — never reaches
        // serde: `place_multi_order` returns `DhanMultiOrderResponse::default()`
        // for empty/whitespace bodies; see the api_client.rs sender tests.)
        let resp: DhanMultiOrderResponse = serde_json::from_str("{}").unwrap();
        assert!(resp.orders.is_empty());
        // The bodyless-200 arm's default is the same empty-orders shape.
        assert!(DhanMultiOrderResponse::default().orders.is_empty());
    }

    #[test]
    fn test_trigger_response_detail_fields_roundtrip() {
        // Full GET-by-ID doc example verbatim (PORTAL export 2026-06-30):
        // createdTime UTC-Z, triggeredTime null, lastPrice STRING,
        // condition with STRING comparingValue, orders echo.
        let json = r#"{
            "alertId": "12345",
            "alertStatus": "ACTIVE",
            "createdTime": "2019-08-24T14:15:22Z",
            "triggeredTime": null,
            "lastPrice": "245.50",
            "condition": { "comparisonType": "TECHNICAL_WITH_VALUE", "exchangeSegment": "NSE_EQ", "securityId": "12345", "indicatorName": "SMA_5", "timeFrame": "DAY", "operator": "CROSSING_UP", "comparingValue": "250.00", "expDate": "2019-08-24", "frequency": "ONCE", "userNote": "Price crossing SMA" },
            "orders": [ { "transactionType": "BUY", "exchangeSegment": "NSE_EQ", "productType": "CNC", "orderType": "LIMIT", "securityId": "12345", "quantity": 10, "validity": "DAY", "price": "250.00", "discQuantity": "0", "triggerPrice": "0" } ]
        }"#;
        let resp: DhanConditionalTriggerResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.alert_id, "12345");
        assert_eq!(
            resp.created_time.as_deref(),
            Some("2019-08-24T14:15:22Z") // UTC-Z — the ONLY UTC in the family
        );
        assert!(resp.triggered_time.is_none()); // null until triggered
        let last_price = resp.last_price.expect("lastPrice present");
        assert_eq!(last_price, DhanNumeric::Text("245.50".to_string()));
        let condition = resp.condition.expect("condition echo present");
        assert_eq!(
            condition.comparing_value,
            Some(DhanNumeric::Text("250.00".to_string())) // STRING in response
        );
        let orders = resp.orders.expect("orders echo present");
        assert_eq!(orders.len(), 1);
        assert_eq!(
            orders[0].price,
            Some(DhanNumeric::Text("250.00".to_string())) // STRING in response
        );
        assert_eq!(
            orders[0].disc_quantity,
            Some(DhanNumeric::Text("0".to_string()))
        );
        assert_eq!(
            orders[0].trigger_price,
            Some(DhanNumeric::Text("0".to_string()))
        );

        // Place/Modify 2-field responses stay byte-identical: the five
        // additive detail fields serialize away when None (golden check).
        let place: DhanConditionalTriggerResponse =
            serde_json::from_str(r#"{"alertId":"12345","alertStatus":"ACTIVE"}"#).unwrap();
        let place_json = serde_json::to_string(&place).unwrap();
        assert_eq!(place_json, r#"{"alertId":"12345","alertStatus":"ACTIVE"}"#);
        assert!(!place_json.contains("createdTime"));
    }

    #[test]
    fn test_trigger_response_order_echo_tolerates_missing_and_numeric_fields() {
        // A leg created via Dhan's web/app UI (or another client) can omit
        // request-optional fields — the OpenAPI yaml marks NO order
        // sub-field required — and the family's documented number/string
        // wobble can echo `price` as a bare number. Neither shape may brick
        // the response parse (tolerance parity with origin/main, where the
        // whole echo was serde-ignored).
        let json = r#"{
            "alertId": "77",
            "alertStatus": "ACTIVE",
            "orders": [ { "transactionType": "BUY", "securityId": "1333", "quantity": 5, "price": 250.5 } ]
        }"#;
        let resp: DhanConditionalTriggerResponse = serde_json::from_str(json).unwrap();
        let orders = resp.orders.expect("orders echo present");
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].price, Some(DhanNumeric::Num(250.5)));
        assert!(orders[0].trigger_price.is_none()); // omitted, never a failure
        assert!(orders[0].disc_quantity.is_none());
        assert_eq!(orders[0].quantity, 5);
        assert!(orders[0].validity.is_empty()); // defaulted, never required

        // GET-all: one UI-created sparse alert must not fail the WHOLE list.
        let list_json = r#"[
            {"alertId":"A1","alertStatus":"ACTIVE",
             "orders":[{"transactionType":"BUY","securityId":"1333","quantity":1}]},
            {"alertId":"A2","alertStatus":"ACTIVE"}
        ]"#;
        let list: Vec<DhanConditionalTriggerResponse> = serde_json::from_str(list_json).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].orders.is_some());
        assert!(list[1].orders.is_none());
    }

    #[test]
    fn test_dhan_numeric_accepts_number_and_string() {
        let num: DhanNumeric = serde_json::from_str("245.5").unwrap();
        assert_eq!(num, DhanNumeric::Num(245.5));
        assert_eq!(num.as_f64(), Some(245.5));
        let text: DhanNumeric = serde_json::from_str("\"245.50\"").unwrap();
        assert_eq!(text, DhanNumeric::Text("245.50".to_string()));
        assert_eq!(text.as_f64(), Some(245.5));
        // Round-trips verbatim (string stays a string).
        assert_eq!(serde_json::to_string(&text).unwrap(), "\"245.50\"");
    }

    #[test]
    fn test_dhan_numeric_as_f64_unparsable_is_none() {
        let garbage = DhanNumeric::Text("not-a-number".to_string());
        assert_eq!(garbage.as_f64(), None);
        let padded = DhanNumeric::Text("  250.00  ".to_string());
        assert_eq!(padded.as_f64(), Some(250.0));
    }

    #[test]
    fn test_dhan_numeric_as_i64_boundaries() {
        // Financial boundary sweep: number, integer string, decimal string,
        // padded, negative, garbage, and saturating extremes — never panics.
        assert_eq!(DhanNumeric::Num(5.0).as_i64(), Some(5));
        assert_eq!(DhanNumeric::Num(5.7).as_i64(), Some(5)); // truncates
        assert_eq!(DhanNumeric::Num(-3.9).as_i64(), Some(-3));
        assert_eq!(DhanNumeric::Num(f64::MAX).as_i64(), Some(i64::MAX)); // saturates
        assert_eq!(DhanNumeric::Num(f64::NAN).as_i64(), Some(0)); // saturating cast
        assert_eq!(DhanNumeric::Text("5".to_string()).as_i64(), Some(5));
        assert_eq!(DhanNumeric::Text("5.0".to_string()).as_i64(), Some(5));
        assert_eq!(DhanNumeric::Text(" 42 ".to_string()).as_i64(), Some(42));
        assert_eq!(DhanNumeric::Text("-7".to_string()).as_i64(), Some(-7));
        assert_eq!(DhanNumeric::Text("abc".to_string()).as_i64(), None);
    }

    #[test]
    fn test_conditional_get_echo_tolerates_explicit_nulls() {
        // Family-proven wire shape: the verbatim GET doc example carries
        // `"triggeredTime": null` — explicit `null` on ANY echo field (not
        // just a missing key, which `#[serde(default)]` alone covers) must
        // never brick the GET-by-id or GET-all parse over one leg.
        let json = r#"{
            "alertId": null,
            "alertStatus": null,
            "createdTime": null,
            "triggeredTime": null,
            "lastPrice": null,
            "condition": {
                "comparisonType": null,
                "exchangeSegment": null,
                "securityId": null,
                "operator": null,
                "comparingValue": null,
                "frequency": null
            },
            "orders": [ {
                "transactionType": null,
                "exchangeSegment": null,
                "productType": null,
                "orderType": null,
                "securityId": null,
                "quantity": null,
                "validity": null,
                "price": null,
                "discQuantity": null,
                "triggerPrice": null
            } ]
        }"#;
        let resp: DhanConditionalTriggerResponse = serde_json::from_str(json).unwrap();
        assert!(resp.alert_id.is_empty());
        assert!(resp.alert_status.is_empty());
        let condition = resp.condition.expect("condition echo present");
        assert!(condition.comparison_type.is_empty());
        assert!(condition.exchange_segment.is_empty());
        assert!(condition.security_id.is_empty());
        assert!(condition.operator.is_empty());
        assert!(condition.frequency.is_empty());
        assert!(condition.comparing_value.is_none());
        let orders = resp.orders.expect("orders echo present");
        assert_eq!(orders.len(), 1);
        assert!(orders[0].transaction_type.is_empty());
        assert_eq!(orders[0].quantity, 0);
        assert!(orders[0].price.is_none());

        // GET-all: one all-null-echo alert must not fail the WHOLE list.
        let list_json = r#"[
            {"alertId":"A1","alertStatus":"ACTIVE"},
            {"alertId":null,"alertStatus":null,
             "orders":[{"transactionType":null,"quantity":null}]}
        ]"#;
        let list: Vec<DhanConditionalTriggerResponse> = serde_json::from_str(list_json).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].alert_id, "A1");
        assert!(list[1].alert_id.is_empty());
    }

    #[test]
    fn test_trigger_order_detail_quantity_tolerates_string_and_null() {
        // The family's documented number/string wobble (`comparingValue` is
        // number-in-request / string-in-response) applied to quantity: a
        // string-echoed quantity must not re-brick GET-all.
        let from_number: TriggerOrderDetail = serde_json::from_str(r#"{"quantity":5}"#).unwrap();
        assert_eq!(from_number.quantity, 5);
        let from_string: TriggerOrderDetail = serde_json::from_str(r#"{"quantity":"5"}"#).unwrap();
        assert_eq!(from_string.quantity, 5);
        let from_decimal: TriggerOrderDetail =
            serde_json::from_str(r#"{"quantity":"5.0"}"#).unwrap();
        assert_eq!(from_decimal.quantity, 5);
        let from_null: TriggerOrderDetail = serde_json::from_str(r#"{"quantity":null}"#).unwrap();
        assert_eq!(from_null.quantity, 0);
        let missing: TriggerOrderDetail = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.quantity, 0);
    }

    #[test]
    fn test_multi_order_response_tolerates_null_order_id_and_null_orders() {
        // YAML-only response, UNVERIFIED-LIVE: a REJECTED leg plausibly
        // echoes `"orderId": null`. A 200 body means the legs are ALREADY
        // placed at the broker — a JSON brick here would hide which legs
        // went live.
        let leg: MultiOrderLegResult =
            serde_json::from_str(r#"{"orderId":null,"sequence":null,"orderStatus":"REJECTED"}"#)
                .unwrap();
        assert!(leg.order_id.is_empty());
        assert!(leg.sequence.is_empty());
        assert_eq!(leg.order_status, "REJECTED");

        let mixed: DhanMultiOrderResponse = serde_json::from_str(
            r#"{"orders":[
                {"orderId":"112111182198","sequence":"1","orderStatus":"TRANSIT"},
                {"orderId":null,"sequence":"2","orderStatus":null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(mixed.orders.len(), 2);
        assert_eq!(mixed.orders[0].order_id, "112111182198");
        assert!(mixed.orders[1].order_id.is_empty());
        assert!(mixed.orders[1].order_status.is_empty());

        let null_orders: DhanMultiOrderResponse =
            serde_json::from_str(r#"{"orders":null}"#).unwrap();
        assert!(null_orders.orders.is_empty());
    }

    // -----------------------------------------------------------------------
    // Trade Book + DhanTradeEntry Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_trade_entry_deserialize_with_tax_breakdown() {
        let json = r#"{"dhanClientId":"1","orderId":"ORD-1","exchangeOrderId":"E1","exchangeTradeId":"T1","transactionType":"BUY","exchangeSegment":"NSE_EQ","productType":"INTRADAY","orderType":"MARKET","tradingSymbol":"NIFTY","customSymbol":"","securityId":"11536","tradedQuantity":50,"tradedPrice":24500.0,"isin":"INE1234","instrument":"OPTIDX","sebiTax":0.5,"stt":12.25,"brokerageCharges":20.0,"serviceTax":3.6,"exchangeTransactionCharges":2.0,"stampDuty":0.1,"drvExpiryDate":"2026-03-27","drvOptionType":"CE","drvStrikePrice":24500.0,"exchangeTime":"2026-03-27 10:30:45"}"#;
        let entry: DhanTradeEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.traded_quantity, 50);
        assert!((entry.stt - 12.25).abs() < f64::EPSILON);
        assert!((entry.stamp_duty - 0.1).abs() < f64::EPSILON);
        assert_eq!(entry.drv_option_type, "CE");
    }

    // -----------------------------------------------------------------------
    // EDIS Types Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_edis_form_request_serialize() {
        let req = EdisFormRequest {
            isin: "INE733E01010".to_string(),
            qty: 100,
            exchange: "NSE".to_string(),
            segment: "EQ".to_string(),
            bulk: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("INE733E01010"));
        assert!(json.contains("\"bulk\":false"));
    }

    #[test]
    fn test_edis_inquiry_response_deserialize() {
        let json = r#"{"totalQty":100,"aprvdQty":50,"status":"APPROVED"}"#;
        let resp: EdisInquiryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.total_qty, 100);
        assert_eq!(resp.aprvd_qty, 50);
        assert_eq!(resp.status, "APPROVED");
    }

    // -----------------------------------------------------------------------
    // Statement Types Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_ledger_entry_string_debit_credit() {
        // CRITICAL: debit and credit are STRINGS, not floats
        let json = r#"{"dhanClientId":"1","narration":"test","voucherdate":"Jun 22, 2022","exchange":"NSE","voucherdesc":"desc","vouchernumber":"V1","debit":"1500.50","credit":"0.00","runbal":"50000.00"}"#;
        let entry: DhanLedgerEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.debit, "1500.50"); // String, not float
        assert_eq!(entry.credit, "0.00"); // String, not float
        assert_eq!(entry.voucherdate, "Jun 22, 2022"); // Human-readable format
    }

    #[test]
    fn test_historical_trade_entry_deserialize() {
        let json = r#"{"dhanClientId":"1","orderId":"O1","exchangeOrderId":"E1","exchangeTradeId":"T1","transactionType":"BUY","exchangeSegment":"NSE_EQ","productType":"CNC","orderType":"LIMIT","tradingSymbol":"RELIANCE","customSymbol":"","securityId":"2885","tradedQuantity":10,"tradedPrice":2500.0,"isin":"INE002A01018","instrument":"EQUITY","sebiTax":0.01,"stt":2.5,"brokerageCharges":0.0,"serviceTax":0.0,"exchangeTransactionCharges":0.5,"stampDuty":0.02,"drvExpiryDate":"NA","drvOptionType":"","drvStrikePrice":0.0,"exchangeTime":"2026-03-25 14:30:00"}"#;
        let entry: DhanHistoricalTradeEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.traded_quantity, 10);
        assert_eq!(entry.drv_expiry_date, "NA"); // "NA" for non-derivatives
        assert_eq!(entry.exchange_time, "2026-03-25 14:30:00"); // IST string
    }

    // -----------------------------------------------------------------------
    // Cancel Order Response Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cancel_order_response_deserialize() {
        let json = r#"{"orderId":"ORD-123","orderStatus":"CANCELLED"}"#;
        let resp: DhanCancelOrderResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.order_id, "ORD-123");
        assert_eq!(resp.order_status, "CANCELLED");
    }
}
