//! Conditional & Multi Order constructors — pure, fail-closed builders for the
//! Dhan `/alerts/*` family.
//!
//! Ground truth: `docs/dhan-ref/07c-conditional-trigger.md` +
//! `.claude/rules/dhan/conditional-trigger.md`. The family support note is
//! binding: **Equities and Indices ONLY** — every other segment is
//! UNREPRESENTABLE in the typed segment enums below (fail-closed; widening
//! requires a dated operator quote + a rule-file edit FIRST, then the enum
//! variant + the `conditional_gate_guard.rs` ratchet edit, same PR).
//!
//! §28 boundary note: [`TriggerIndicatorName`] holds **Dhan WIRE enums**
//! (protocol strings like "SMA_5") — `crates/trading/src/indicator` is NEVER
//! imported here and no indicator math exists in this module.
//!
//! Every function in this module is PURE: no I/O, no clock, no metrics. The
//! HTTP senders live in `oms::api_client` behind the hardcoded alerts gate.

use tickvault_common::order_types::{OrderType, OrderValidity, ProductType, TransactionType};

use super::types::{
    DhanConditionalTriggerRequest, DhanMultiOrderRequest, MultiOrderLeg, TriggerCondition,
    TriggerOperator, TriggerOrder, TriggerTimeFrame,
};

// ---------------------------------------------------------------------------
// Family constants
// ---------------------------------------------------------------------------

/// Max orders per conditional trigger AND per multi-order batch
/// (live portal callout 2026-07-14: "up to 15 orders").
pub const DHAN_CONDITIONAL_MAX_ORDERS_PER_REQUEST: usize = 15;

/// Price plausibility cap in integer paise: 1e7 rupees (₹1,00,00,000.00).
/// Mirrors the GDF parse guard's |price| > 1e7-rupees rejection. Every paise
/// value at or below this cap converts to f64 rupees without integer
/// representation loss (1e9 < 2^53).
const MAX_PRICE_PAISE: i64 = 1_000_000_000;

/// Max `correlationId` length (docs: >= 0 characters, <= 30 characters).
const MAX_CORRELATION_ID_CHARS: usize = 30;

/// Max `securityId` length (docs: <= 20 characters; empty fail-closed refused).
const MAX_SECURITY_ID_CHARS: usize = 20;

/// Max `dhanClientId` length. The docs type it only as "string, Required"
/// (live-docs §3.2/§9.1); real Dhan client IDs are short numeric account
/// identifiers (the `1106656882` class), so 20 is a generous fail-closed
/// bound against runaway/garbage inputs while tolerating any plausible ID.
const MAX_DHAN_CLIENT_ID_CHARS: usize = 20;

// ---------------------------------------------------------------------------
// Segment locks (fail-closed)
// ---------------------------------------------------------------------------

/// Segments legal for a trigger CONDITION — the verbatim live enum for
/// `condition.exchangeSegment` (docs: NSE_EQ, BSE_EQ, IDX_I).
/// FAIL-CLOSED: no other variant exists; F&O / commodity / currency
/// conditions are UNREPRESENTABLE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalSegment {
    /// NSE cash equity condition.
    NseEq,
    /// BSE cash equity condition.
    BseEq,
    /// Index-value condition (NSE + BSE indices).
    IdxI,
}

impl ConditionalSegment {
    /// Exact wire form of the condition segment.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NseEq => "NSE_EQ",
            Self::BseEq => "BSE_EQ",
            Self::IdxI => "IDX_I",
        }
    }
}

/// Segments legal for a conditional/multi ORDER LEG — equities ONLY.
/// `IdxI` is deliberately absent (an index is not a tradable instrument —
/// "Indices" in the family support note is the condition side); the F&O and
/// commodity segments are deliberately UNREPRESENTABLE (the family support
/// note "Equities and Indices only" governs the generic order enum's wider
/// textual listing — fail-closed).
///
/// Widening protocol (mechanical, in order, same PR): dated operator quote →
/// `.claude/rules/dhan/conditional-trigger.md` rule edit → enum variant →
/// `crates/trading/tests/conditional_gate_guard.rs` ratchet edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalLegSegment {
    /// NSE cash equity order leg.
    NseEq,
    /// BSE cash equity order leg.
    BseEq,
}

impl ConditionalLegSegment {
    /// Exact wire form of the order-leg segment.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NseEq => "NSE_EQ",
            Self::BseEq => "BSE_EQ",
        }
    }
}

// ---------------------------------------------------------------------------
// Wire enums (Dhan protocol strings — NOT our indicator engine)
// ---------------------------------------------------------------------------

/// The 21 Dhan WIRE indicator names (docs §3.4). These are Dhan protocol
/// enums — `crates/trading/src/indicator` is NEVER imported (§28 boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // APPROVED: 21 self-describing wire-name variants; the wire strings are in as_str
pub enum TriggerIndicatorName {
    Sma5,
    Sma10,
    Sma20,
    Sma50,
    Sma100,
    Sma200,
    Ema5,
    Ema10,
    Ema20,
    Ema50,
    Ema100,
    Ema200,
    BbUpper,
    BbLower,
    Rsi14,
    Atr14,
    Stochastic,
    StochRsi14,
    Macd26,
    Macd12,
    MacdHist,
}

impl TriggerIndicatorName {
    /// Exact wire form of the indicator name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sma5 => "SMA_5",
            Self::Sma10 => "SMA_10",
            Self::Sma20 => "SMA_20",
            Self::Sma50 => "SMA_50",
            Self::Sma100 => "SMA_100",
            Self::Sma200 => "SMA_200",
            Self::Ema5 => "EMA_5",
            Self::Ema10 => "EMA_10",
            Self::Ema20 => "EMA_20",
            Self::Ema50 => "EMA_50",
            Self::Ema100 => "EMA_100",
            Self::Ema200 => "EMA_200",
            Self::BbUpper => "BB_UPPER",
            Self::BbLower => "BB_LOWER",
            Self::Rsi14 => "RSI_14",
            Self::Atr14 => "ATR_14",
            Self::Stochastic => "STOCHASTIC",
            Self::StochRsi14 => "STOCHRSI_14",
            Self::Macd26 => "MACD_26",
            Self::Macd12 => "MACD_12",
            Self::MacdHist => "MACD_HIST",
        }
    }
}

/// AMO timing (multi-order legs only): PRE_OPEN, OPEN, OPEN_30, OPEN_60.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmoTime {
    /// Pre-open session.
    PreOpen,
    /// Market open.
    Open,
    /// 30 minutes after open.
    Open30,
    /// 60 minutes after open.
    Open60,
}

impl AmoTime {
    /// Exact wire form of the AMO timing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreOpen => "PRE_OPEN",
            Self::Open => "OPEN",
            Self::Open30 => "OPEN_30",
            Self::Open60 => "OPEN_60",
        }
    }
}

/// Wire form of a [`TriggerOperator`] (private helper — the serde enum has no
/// `as_str`; this avoids a serialize-then-trim round trip).
fn trigger_operator_wire(operator: TriggerOperator) -> &'static str {
    match operator {
        TriggerOperator::CrossingUp => "CROSSING_UP",
        TriggerOperator::CrossingDown => "CROSSING_DOWN",
        TriggerOperator::CrossingAnySide => "CROSSING_ANY_SIDE",
        TriggerOperator::GreaterThan => "GREATER_THAN",
        TriggerOperator::LessThan => "LESS_THAN",
        TriggerOperator::GreaterThanEqual => "GREATER_THAN_EQUAL",
        TriggerOperator::LessThanEqual => "LESS_THAN_EQUAL",
        TriggerOperator::Equal => "EQUAL",
        TriggerOperator::NotEqual => "NOT_EQUAL",
    }
}

/// Wire form of a [`TriggerTimeFrame`] (private helper).
fn trigger_time_frame_wire(time_frame: TriggerTimeFrame) -> &'static str {
    match time_frame {
        TriggerTimeFrame::Day => "DAY",
        TriggerTimeFrame::OneMin => "ONE_MIN",
        TriggerTimeFrame::FiveMin => "FIVE_MIN",
        TriggerTimeFrame::FifteenMin => "FIFTEEN_MIN",
    }
}

// ---------------------------------------------------------------------------
// Condition spec (mandatory-field matrix, unrepresentable-wrong)
// ---------------------------------------------------------------------------

/// Comparison spec — each variant carries EXACTLY the docs-mandated fields
/// (07c §comparison-types matrix), so an under-/over-specified condition is
/// UNREPRESENTABLE.
#[derive(Debug, Clone)]
pub enum TriggerConditionSpec {
    /// Market price vs fixed value (`PRICE_WITH_VALUE`).
    PriceWithValue {
        /// Comparison operator.
        operator: TriggerOperator,
        /// Fixed value to compare against (must be finite).
        comparing_value: f64,
    },
    /// Indicator vs fixed number (`TECHNICAL_WITH_VALUE`).
    TechnicalWithValue {
        /// Technical indicator (Dhan wire name).
        indicator: TriggerIndicatorName,
        /// Evaluation timeframe.
        time_frame: TriggerTimeFrame,
        /// Comparison operator.
        operator: TriggerOperator,
        /// Fixed value to compare against (must be finite).
        comparing_value: f64,
    },
    /// Indicator vs another indicator (`TECHNICAL_WITH_INDICATOR`).
    TechnicalWithIndicator {
        /// Technical indicator (Dhan wire name).
        indicator: TriggerIndicatorName,
        /// Evaluation timeframe.
        time_frame: TriggerTimeFrame,
        /// Comparison operator.
        operator: TriggerOperator,
        /// Second indicator to compare against.
        comparing_indicator: TriggerIndicatorName,
    },
    /// Indicator vs closing price (`TECHNICAL_WITH_CLOSE`).
    TechnicalWithClose {
        /// Technical indicator (Dhan wire name).
        indicator: TriggerIndicatorName,
        /// Evaluation timeframe.
        time_frame: TriggerTimeFrame,
        /// Comparison operator.
        operator: TriggerOperator,
    },
}

// ---------------------------------------------------------------------------
// Typed refusals
// ---------------------------------------------------------------------------

/// Typed refusals from the production constructors. Deliberately NOT
/// `OmsError` variants (zero risk to `OmsError` match sites) and NOT
/// `ErrorCode` (2026-07-14 Dhan noise lock — no new coded taxonomy).
#[derive(Debug, thiserror::Error)]
pub enum ConditionalBuildError {
    /// Order-leg segment outside the equities-only fail-closed set.
    #[error("leg segment not allowed: conditional/multi legs are equities-only (NSE_EQ/BSE_EQ)")]
    SegmentNotAllowed,
    /// Leg count outside 1..=15.
    #[error("legs out of bounds: {count} (allowed 1..=15 per live docs callout)")]
    LegCountOutOfBounds {
        /// The offending leg count.
        count: usize,
    },
    /// Security ID empty, overlong, or outside ASCII alphanumerics.
    #[error("security_id invalid: must be 1..=20 ASCII alphanumeric chars")]
    BadSecurityId,
    /// Quantity outside 1..=i32::MAX.
    #[error("quantity must be 1..=i32::MAX, got {quantity}")]
    BadQuantity {
        /// The offending quantity.
        quantity: i64,
    },
    /// Price invalid for the order type (or outside the paise cap).
    #[error("price invalid for order type {order_type}: {reason}")]
    BadPrice {
        /// Wire form of the order type.
        order_type: &'static str,
        /// Refusal reason.
        reason: &'static str,
    },
    /// Trigger price invalid for the order type (or outside the paise cap).
    #[error("trigger price invalid for order type {order_type}: {reason}")]
    BadTriggerPrice {
        /// Wire form of the order type.
        order_type: &'static str,
        /// Refusal reason.
        reason: &'static str,
    },
    /// Disclosed quantity outside the >30%-of-quantity rule.
    #[error("disclosed quantity invalid: {reason}")]
    BadDisclosedQuantity {
        /// Refusal reason.
        reason: &'static str,
    },
    /// Product type outside the family's CNC/INTRADAY/MARGIN/MTF set.
    #[error("product type not supported by this family (CNC/INTRADAY/MARGIN/MTF only)")]
    UnsupportedProductType,
    /// Correlation ID longer than 30 chars or outside `[a-zA-Z0-9 _-]`.
    #[error("correlation id invalid: max 30 chars of [a-zA-Z0-9 _-]")]
    BadCorrelationId,
    /// Dhan client ID empty, overlong, or outside ASCII alphanumerics
    /// (docs mark `dhanClientId` Required on both endpoints — an empty or
    /// whitespace/BiDi-carrying ID is a guaranteed-invalid order body,
    /// refused locally instead of burning an Order-API slot on a DH-905).
    #[error("dhan client id invalid: must be 1..=20 ASCII alphanumeric chars")]
    BadDhanClientId,
    /// Expiry date not in `YYYY-MM-DD` form.
    #[error("expDate must be YYYY-MM-DD")]
    BadExpDate,
    /// Comparing value NaN or infinite.
    #[error("comparing value must be finite")]
    NonFiniteComparingValue,
}

// ---------------------------------------------------------------------------
// Leg specs (typed inputs)
// ---------------------------------------------------------------------------

/// Typed input for one conditional order leg (STRING-price wire schema).
#[derive(Debug, Clone)]
pub struct TriggerOrderSpec {
    /// Order-leg segment (equities-only, fail-closed).
    pub segment: ConditionalLegSegment,
    /// Buy or sell.
    pub transaction_type: TransactionType,
    /// Product type — Co/Bo REFUSED at build.
    pub product_type: ProductType,
    /// Order execution type.
    pub order_type: OrderType,
    /// Order validity.
    pub validity: OrderValidity,
    /// Dhan security ID (1..=20 ASCII alphanumeric chars).
    pub security_id: String,
    /// Quantity (1..=i32::MAX).
    pub quantity: i64,
    /// Price in integer paise → exact "250.00" string; no float formatting.
    pub price_paise: i64,
    /// Trigger price in integer paise.
    pub trigger_price_paise: i64,
    /// Disclosed quantity (0, or >30% of quantity and <= quantity).
    pub disclosed_quantity: i64,
}

/// Typed input for one multi-order leg (FLOAT-price wire schema; `sequence`
/// is NOT settable — assigned by the request constructor).
#[derive(Debug, Clone)]
pub struct MultiOrderLegSpec {
    /// Order-leg segment (equities-only, fail-closed).
    pub segment: ConditionalLegSegment,
    /// Buy or sell.
    pub transaction_type: TransactionType,
    /// Product type — Co/Bo REFUSED at build.
    pub product_type: ProductType,
    /// Order execution type.
    pub order_type: OrderType,
    /// Order validity.
    pub validity: OrderValidity,
    /// Dhan security ID (1..=20 ASCII alphanumeric chars).
    pub security_id: String,
    /// Quantity (1..=i32::MAX).
    pub quantity: i64,
    /// Price in integer paise → f64 rupees (exact: paise plausibility-capped).
    pub price_paise: i64,
    /// Trigger price in integer paise.
    pub trigger_price_paise: i64,
    /// Disclosed quantity (0, or >30% of quantity and <= quantity).
    pub disclosed_quantity: i64,
    /// Optional tracking ID (<= 30 chars of `[a-zA-Z0-9 _-]`).
    pub correlation_id: Option<String>,
    /// `Some` ⇒ `afterMarketOrder=true` + `amoTime`; `None` ⇒ neither
    /// (flag/time mismatch UNREPRESENTABLE).
    pub amo: Option<AmoTime>,
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Formats non-negative integer paise as an exact rupee price string:
/// `25000 → "250.00"`, `5 → "0.05"`. Integer formatting only — no float
/// arithmetic, no rounding.
fn paise_to_price_string(paise: i64) -> String {
    format!("{}.{:02}", paise / 100, paise % 100)
}

/// Converts capped non-negative integer paise to f64 rupees for the
/// multi-order FLOAT wire. Exact integer representation is guaranteed by the
/// [`MAX_PRICE_PAISE`] cap (1e9 < 2^53); the /100 scaling is the wire's own
/// decimal form.
fn paise_to_rupees_f64(paise: i64) -> f64 {
    paise as f64 / 100.0
}

/// Format-only `YYYY-MM-DD` check — NO clock: a past date is Dhan's to
/// reject (pure clockless constructor, documented).
fn is_valid_exp_date_format(exp_date: &str) -> bool {
    let bytes = exp_date.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        4 | 7 => *byte == b'-',
        _ => byte.is_ascii_digit(),
    })
}

/// Validates a Dhan `securityId`: 1..=20 ASCII alphanumeric chars. Real
/// Dhan security IDs are numeric strings ("1333"); ASCII-alphanumeric-only
/// refuses whitespace, control, BiDi and multi-byte garbage fail-closed
/// (round-4 review — the prior length-only check let `" 1333"` /
/// `"13\u{202E}33"` serialize verbatim into a guaranteed-invalid
/// DATA-813-class order body), mirroring [`validate_dhan_client_id`] on the
/// SAME request bodies. Byte length equals char count on the accepted set,
/// so `len()` is the exact 20-char bound.
fn validate_security_id(security_id: &str) -> Result<(), ConditionalBuildError> {
    if security_id.is_empty() || security_id.len() > MAX_SECURITY_ID_CHARS {
        return Err(ConditionalBuildError::BadSecurityId);
    }
    if !security_id
        .chars()
        .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(ConditionalBuildError::BadSecurityId);
    }
    Ok(())
}

/// Validates the fields shared by conditional and multi order legs.
fn validate_common_leg(
    security_id: &str,
    quantity: i64,
    product_type: ProductType,
    order_type: OrderType,
    price_paise: i64,
    trigger_price_paise: i64,
    disclosed_quantity: i64,
) -> Result<(), ConditionalBuildError> {
    validate_security_id(security_id)?;
    if quantity < 1 || quantity > i64::from(i32::MAX) {
        return Err(ConditionalBuildError::BadQuantity { quantity });
    }
    if matches!(product_type, ProductType::Co | ProductType::Bo) {
        return Err(ConditionalBuildError::UnsupportedProductType);
    }

    let order_type_wire = order_type.as_str();
    if price_paise < 0 {
        return Err(ConditionalBuildError::BadPrice {
            order_type: order_type_wire,
            reason: "price paise must not be negative",
        });
    }
    if price_paise > MAX_PRICE_PAISE {
        return Err(ConditionalBuildError::BadPrice {
            order_type: order_type_wire,
            reason: "price paise exceeds the 1e7-rupee plausibility cap",
        });
    }
    if trigger_price_paise < 0 {
        return Err(ConditionalBuildError::BadTriggerPrice {
            order_type: order_type_wire,
            reason: "trigger price paise must not be negative",
        });
    }
    if trigger_price_paise > MAX_PRICE_PAISE {
        return Err(ConditionalBuildError::BadTriggerPrice {
            order_type: order_type_wire,
            reason: "trigger price paise exceeds the 1e7-rupee plausibility cap",
        });
    }

    // Price/trigger coupling by order type (docs/dhan-ref/07-orders.md rules
    // 10/11 applied to this family):
    //   LIMIT:            price > 0, trigger == 0
    //   MARKET:           price == 0, trigger == 0
    //   STOP_LOSS:        price > 0, trigger > 0
    //   STOP_LOSS_MARKET: price == 0, trigger > 0
    match order_type {
        OrderType::Limit => {
            if price_paise == 0 {
                return Err(ConditionalBuildError::BadPrice {
                    order_type: order_type_wire,
                    reason: "LIMIT requires price > 0",
                });
            }
            if trigger_price_paise != 0 {
                return Err(ConditionalBuildError::BadTriggerPrice {
                    order_type: order_type_wire,
                    reason: "LIMIT requires trigger price == 0",
                });
            }
        }
        OrderType::Market => {
            if price_paise != 0 {
                return Err(ConditionalBuildError::BadPrice {
                    order_type: order_type_wire,
                    reason: "MARKET requires price == 0",
                });
            }
            if trigger_price_paise != 0 {
                return Err(ConditionalBuildError::BadTriggerPrice {
                    order_type: order_type_wire,
                    reason: "MARKET requires trigger price == 0",
                });
            }
        }
        OrderType::StopLoss => {
            if price_paise == 0 {
                return Err(ConditionalBuildError::BadPrice {
                    order_type: order_type_wire,
                    reason: "STOP_LOSS requires price > 0",
                });
            }
            if trigger_price_paise == 0 {
                return Err(ConditionalBuildError::BadTriggerPrice {
                    order_type: order_type_wire,
                    reason: "STOP_LOSS requires trigger price > 0",
                });
            }
        }
        OrderType::StopLossMarket => {
            if price_paise != 0 {
                return Err(ConditionalBuildError::BadPrice {
                    order_type: order_type_wire,
                    reason: "STOP_LOSS_MARKET requires price == 0",
                });
            }
            if trigger_price_paise == 0 {
                return Err(ConditionalBuildError::BadTriggerPrice {
                    order_type: order_type_wire,
                    reason: "STOP_LOSS_MARKET requires trigger price > 0",
                });
            }
        }
    }

    // Disclosed quantity: 0, OR strictly more than 30% of quantity AND at
    // most the quantity (docs/dhan-ref/07-orders.md rule 12 borrowed —
    // Assumed for this family, fail-closed direction). Integer math:
    // disclosed*10 > quantity*3 ⇔ disclosed > 0.3*quantity. Bounded by the
    // quantity <= i32::MAX check above, so no overflow in i64.
    if disclosed_quantity != 0 {
        if disclosed_quantity < 0 {
            return Err(ConditionalBuildError::BadDisclosedQuantity {
                reason: "disclosed quantity must not be negative",
            });
        }
        if disclosed_quantity > quantity {
            return Err(ConditionalBuildError::BadDisclosedQuantity {
                reason: "disclosed quantity must not exceed quantity",
            });
        }
        if disclosed_quantity * 10 <= quantity * 3 {
            return Err(ConditionalBuildError::BadDisclosedQuantity {
                reason: "disclosed quantity must exceed 30% of quantity",
            });
        }
    }

    Ok(())
}

/// Validates the mandatory `dhanClientId`: 1..=20 ASCII alphanumeric chars.
/// The docs mark it Required on BOTH family endpoints (live-docs §3.2/§9.1);
/// an empty string would serialize `"dhanClientId":""` — a guaranteed-
/// invalid order body from a module whose contract is fail-closed typed
/// refusals. ASCII-alphanumeric-only also refuses whitespace, control, BiDi
/// and multi-byte garbage fail-closed (real Dhan IDs are numeric strings).
fn validate_dhan_client_id(dhan_client_id: &str) -> Result<(), ConditionalBuildError> {
    if dhan_client_id.is_empty() || dhan_client_id.len() > MAX_DHAN_CLIENT_ID_CHARS {
        return Err(ConditionalBuildError::BadDhanClientId);
    }
    if !dhan_client_id
        .chars()
        .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(ConditionalBuildError::BadDhanClientId);
    }
    Ok(())
}

/// Validates an optional `correlationId`: <= 30 chars of `[a-zA-Z0-9 _-]`.
fn validate_correlation_id(correlation_id: &str) -> Result<(), ConditionalBuildError> {
    if correlation_id.chars().count() > MAX_CORRELATION_ID_CHARS {
        return Err(ConditionalBuildError::BadCorrelationId);
    }
    let charset_ok = correlation_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == ' ' || ch == '_' || ch == '-');
    if !charset_ok {
        return Err(ConditionalBuildError::BadCorrelationId);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Constructors (pure; every refusal typed)
// ---------------------------------------------------------------------------

/// Fail-closed condition builder: segment unrepresentable-wrong;
/// mandatory-field matrix enforced by [`TriggerConditionSpec`] shape;
/// frequency hardcoded "ONCE" (ship ONCE; yaml-only ALWAYS is tolerated on
/// parse because the response field is a String).
///
/// Validates: `security_id` 1..=20 ASCII alphanumeric chars; `exp_date`
/// `YYYY-MM-DD` format-only (no clock — a past date is Dhan's to reject,
/// documented); `comparing_value` finite.
///
/// # Errors
/// - [`ConditionalBuildError::BadSecurityId`] on empty / >20-char /
///   non-ASCII-alphanumeric IDs
/// - [`ConditionalBuildError::BadExpDate`] on malformed dates
/// - [`ConditionalBuildError::NonFiniteComparingValue`] on NaN / ±∞
pub fn build_trigger_condition(
    segment: ConditionalSegment,
    security_id: &str,
    spec: &TriggerConditionSpec,
    exp_date: &str,
    user_note: Option<&str>,
) -> Result<TriggerCondition, ConditionalBuildError> {
    validate_security_id(security_id)?;
    if !is_valid_exp_date_format(exp_date) {
        return Err(ConditionalBuildError::BadExpDate);
    }

    let (
        comparison_type,
        indicator_name,
        time_frame,
        operator,
        comparing_value,
        comparing_indicator_name,
    ) = match spec {
        TriggerConditionSpec::PriceWithValue {
            operator,
            comparing_value,
        } => {
            if !comparing_value.is_finite() {
                return Err(ConditionalBuildError::NonFiniteComparingValue);
            }
            (
                "PRICE_WITH_VALUE",
                None,
                None,
                trigger_operator_wire(*operator),
                Some(*comparing_value),
                None,
            )
        }
        TriggerConditionSpec::TechnicalWithValue {
            indicator,
            time_frame,
            operator,
            comparing_value,
        } => {
            if !comparing_value.is_finite() {
                return Err(ConditionalBuildError::NonFiniteComparingValue);
            }
            (
                "TECHNICAL_WITH_VALUE",
                Some(indicator.as_str()),
                Some(trigger_time_frame_wire(*time_frame)),
                trigger_operator_wire(*operator),
                Some(*comparing_value),
                None,
            )
        }
        TriggerConditionSpec::TechnicalWithIndicator {
            indicator,
            time_frame,
            operator,
            comparing_indicator,
        } => (
            "TECHNICAL_WITH_INDICATOR",
            Some(indicator.as_str()),
            Some(trigger_time_frame_wire(*time_frame)),
            trigger_operator_wire(*operator),
            None,
            Some(comparing_indicator.as_str()),
        ),
        TriggerConditionSpec::TechnicalWithClose {
            indicator,
            time_frame,
            operator,
        } => (
            "TECHNICAL_WITH_CLOSE",
            Some(indicator.as_str()),
            Some(trigger_time_frame_wire(*time_frame)),
            trigger_operator_wire(*operator),
            None,
            None,
        ),
    };

    Ok(TriggerCondition {
        comparison_type: comparison_type.to_string(),
        exchange_segment: segment.as_str().to_string(),
        security_id: security_id.to_string(),
        indicator_name: indicator_name.map(str::to_string),
        time_frame: time_frame.map(str::to_string),
        operator: operator.to_string(),
        comparing_value,
        comparing_indicator_name: comparing_indicator_name.map(str::to_string),
        exp_date: Some(exp_date.to_string()),
        frequency: "ONCE".to_string(),
        user_note: user_note.map(str::to_string),
    })
}

/// Builds an equities-only conditional order leg (STRING prices from integer
/// paise — exact formatting, no float arithmetic on the price path).
///
/// # Errors
/// Every refusal is a typed [`ConditionalBuildError`]: quantity 1..=i32::MAX;
/// product ∉ {CO, BO}; price/trigger coupling by order type; paise within the
/// 1e7-rupee plausibility cap; disclosed == 0 OR (>30% of quantity AND
/// <= quantity).
pub fn build_trigger_order(spec: &TriggerOrderSpec) -> Result<TriggerOrder, ConditionalBuildError> {
    validate_common_leg(
        &spec.security_id,
        spec.quantity,
        spec.product_type,
        spec.order_type,
        spec.price_paise,
        spec.trigger_price_paise,
        spec.disclosed_quantity,
    )?;

    Ok(TriggerOrder {
        transaction_type: spec.transaction_type.as_str().to_string(),
        exchange_segment: spec.segment.as_str().to_string(),
        product_type: spec.product_type.as_str().to_string(),
        order_type: spec.order_type.as_str().to_string(),
        security_id: spec.security_id.clone(),
        quantity: spec.quantity,
        validity: spec.validity.as_str().to_string(),
        price: paise_to_price_string(spec.price_paise),
        disc_quantity: spec.disclosed_quantity.to_string(),
        trigger_price: paise_to_price_string(spec.trigger_price_paise),
    })
}

/// Builds a Place/Modify conditional trigger body. Enforces
/// 1..=[`DHAN_CONDITIONAL_MAX_ORDERS_PER_REQUEST`] legs. `alert_id` is always
/// `None` here (Place semantics) — use [`with_alert_id`] for the Modify echo.
///
/// # Errors
/// - [`ConditionalBuildError::BadDhanClientId`] on an empty / overlong /
///   non-ASCII-alphanumeric `dhan_client_id` (docs: Required)
/// - [`ConditionalBuildError::LegCountOutOfBounds`] on 0 or >15 legs
pub fn build_conditional_trigger_request(
    dhan_client_id: &str,
    condition: TriggerCondition,
    orders: Vec<TriggerOrder>,
) -> Result<DhanConditionalTriggerRequest, ConditionalBuildError> {
    validate_dhan_client_id(dhan_client_id)?;
    let count = orders.len();
    if count == 0 || count > DHAN_CONDITIONAL_MAX_ORDERS_PER_REQUEST {
        return Err(ConditionalBuildError::LegCountOutOfBounds { count });
    }
    Ok(DhanConditionalTriggerRequest {
        dhan_client_id: dhan_client_id.to_string(),
        condition,
        orders,
        alert_id: None,
    })
}

/// Modify variant: same request body + the documented OPTIONAL body `alertId`
/// echo (PORTAL modify page, 2026-07-14).
pub fn with_alert_id(
    request: DhanConditionalTriggerRequest,
    alert_id: &str,
) -> DhanConditionalTriggerRequest {
    DhanConditionalTriggerRequest {
        alert_id: Some(alert_id.to_string()),
        ..request
    }
}

/// Builds a Multi Order request (`POST /alerts/multi/orders`). 1..=15 legs;
/// `sequence` is assigned "1".."N" HERE in slice order (docs: "Start with
/// Order 1 and add additional orders sequentially") — forged / duplicate /
/// skipped sequences are UNREPRESENTABLE. Per-leg checks mirror
/// [`build_trigger_order`] plus `correlation_id` charset/length and AMO
/// coupling; prices convert paise → f64 rupees (exact within the cap).
///
/// # Errors
/// Every refusal is a typed [`ConditionalBuildError`], including
/// [`ConditionalBuildError::BadDhanClientId`] on an empty / overlong /
/// non-ASCII-alphanumeric `dhan_client_id` (docs: Required).
pub fn build_multi_order_request(
    dhan_client_id: &str,
    specs: &[MultiOrderLegSpec],
) -> Result<DhanMultiOrderRequest, ConditionalBuildError> {
    validate_dhan_client_id(dhan_client_id)?;
    let count = specs.len();
    if count == 0 || count > DHAN_CONDITIONAL_MAX_ORDERS_PER_REQUEST {
        return Err(ConditionalBuildError::LegCountOutOfBounds { count });
    }

    let mut orders = Vec::with_capacity(count);
    for (index, spec) in specs.iter().enumerate() {
        validate_common_leg(
            &spec.security_id,
            spec.quantity,
            spec.product_type,
            spec.order_type,
            spec.price_paise,
            spec.trigger_price_paise,
            spec.disclosed_quantity,
        )?;
        if let Some(correlation_id) = &spec.correlation_id {
            validate_correlation_id(correlation_id)?;
        }

        // `Option<AmoTime>` couples the flag and the timing: Some ⇒ both
        // fields, None ⇒ neither — a mismatch is unrepresentable.
        let (after_market_order, amo_time) = match spec.amo {
            Some(timing) => (Some(true), Some(timing.as_str().to_string())),
            None => (None, None),
        };

        orders.push(MultiOrderLeg {
            sequence: (index + 1).to_string(),
            correlation_id: spec.correlation_id.clone(),
            transaction_type: spec.transaction_type.as_str().to_string(),
            exchange_segment: spec.segment.as_str().to_string(),
            product_type: spec.product_type.as_str().to_string(),
            order_type: spec.order_type.as_str().to_string(),
            validity: spec.validity.as_str().to_string(),
            security_id: spec.security_id.clone(),
            quantity: spec.quantity,
            after_market_order,
            amo_time,
            price: paise_to_rupees_f64(spec.price_paise),
            trigger_price: paise_to_rupees_f64(spec.trigger_price_paise),
            disclosed_quantity: spec.disclosed_quantity,
        });
    }

    Ok(DhanMultiOrderRequest {
        dhan_client_id: dhan_client_id.to_string(),
        orders,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn limit_leg_spec() -> TriggerOrderSpec {
        TriggerOrderSpec {
            segment: ConditionalLegSegment::NseEq,
            transaction_type: TransactionType::Buy,
            product_type: ProductType::Cnc,
            order_type: OrderType::Limit,
            validity: OrderValidity::Day,
            security_id: "1333".to_string(),
            quantity: 10,
            price_paise: 25_000,
            trigger_price_paise: 0,
            disclosed_quantity: 0,
        }
    }

    fn multi_leg_spec() -> MultiOrderLegSpec {
        MultiOrderLegSpec {
            segment: ConditionalLegSegment::NseEq,
            transaction_type: TransactionType::Buy,
            product_type: ProductType::Cnc,
            order_type: OrderType::Limit,
            validity: OrderValidity::Day,
            security_id: "1333".to_string(),
            quantity: 10,
            price_paise: 25_000,
            trigger_price_paise: 0,
            disclosed_quantity: 0,
            correlation_id: None,
            amo: None,
        }
    }

    #[test]
    fn test_build_trigger_condition_all_four_comparison_types_serialize_mandatory_fields() {
        // PRICE_WITH_VALUE: operator + comparingValue only.
        let price = build_trigger_condition(
            ConditionalSegment::NseEq,
            "1333",
            &TriggerConditionSpec::PriceWithValue {
                operator: TriggerOperator::GreaterThan,
                comparing_value: 250.0,
            },
            "2026-08-24",
            None,
        )
        .unwrap();
        assert_eq!(price.comparison_type, "PRICE_WITH_VALUE");
        assert!(price.indicator_name.is_none());
        assert!(price.time_frame.is_none());
        assert_eq!(price.comparing_value, Some(250.0));
        assert!(price.comparing_indicator_name.is_none());
        assert_eq!(price.exp_date.as_deref(), Some("2026-08-24"));

        // TECHNICAL_WITH_VALUE: indicator + timeFrame + operator + comparingValue.
        let tech_value = build_trigger_condition(
            ConditionalSegment::IdxI,
            "13",
            &TriggerConditionSpec::TechnicalWithValue {
                indicator: TriggerIndicatorName::Sma5,
                time_frame: TriggerTimeFrame::Day,
                operator: TriggerOperator::CrossingUp,
                comparing_value: 250.0,
            },
            "2026-08-24",
            Some("Price crossing SMA"),
        )
        .unwrap();
        assert_eq!(tech_value.comparison_type, "TECHNICAL_WITH_VALUE");
        assert_eq!(tech_value.exchange_segment, "IDX_I");
        assert_eq!(tech_value.indicator_name.as_deref(), Some("SMA_5"));
        assert_eq!(tech_value.time_frame.as_deref(), Some("DAY"));
        assert_eq!(tech_value.operator, "CROSSING_UP");
        assert_eq!(tech_value.comparing_value, Some(250.0));
        assert_eq!(tech_value.user_note.as_deref(), Some("Price crossing SMA"));

        // TECHNICAL_WITH_INDICATOR: comparingIndicatorName, NO comparingValue.
        let tech_indicator = build_trigger_condition(
            ConditionalSegment::BseEq,
            "500325",
            &TriggerConditionSpec::TechnicalWithIndicator {
                indicator: TriggerIndicatorName::Ema20,
                time_frame: TriggerTimeFrame::FifteenMin,
                operator: TriggerOperator::CrossingDown,
                comparing_indicator: TriggerIndicatorName::Ema50,
            },
            "2026-08-24",
            None,
        )
        .unwrap();
        assert_eq!(tech_indicator.comparison_type, "TECHNICAL_WITH_INDICATOR");
        assert!(tech_indicator.comparing_value.is_none());
        assert_eq!(
            tech_indicator.comparing_indicator_name.as_deref(),
            Some("EMA_50")
        );

        // TECHNICAL_WITH_CLOSE: indicator + timeFrame + operator only —
        // a stray comparingValue is UNREPRESENTABLE by spec shape.
        let tech_close = build_trigger_condition(
            ConditionalSegment::NseEq,
            "1333",
            &TriggerConditionSpec::TechnicalWithClose {
                indicator: TriggerIndicatorName::Rsi14,
                time_frame: TriggerTimeFrame::OneMin,
                operator: TriggerOperator::LessThan,
            },
            "2026-08-24",
            None,
        )
        .unwrap();
        assert_eq!(tech_close.comparison_type, "TECHNICAL_WITH_CLOSE");
        assert!(tech_close.comparing_value.is_none());
        assert!(tech_close.comparing_indicator_name.is_none());
    }

    #[test]
    fn test_build_trigger_condition_frequency_ships_once() {
        let condition = build_trigger_condition(
            ConditionalSegment::NseEq,
            "1333",
            &TriggerConditionSpec::PriceWithValue {
                operator: TriggerOperator::GreaterThan,
                comparing_value: 250.0,
            },
            "2026-08-24",
            None,
        )
        .unwrap();
        // Ship ONCE always; yaml-only ALWAYS is tolerated on PARSE only
        // (the response field is a String), never emitted.
        assert_eq!(condition.frequency, "ONCE");
    }

    #[test]
    fn test_build_trigger_condition_rejects_bad_security_id_boundary_zero_and_21_chars() {
        let spec = TriggerConditionSpec::PriceWithValue {
            operator: TriggerOperator::GreaterThan,
            comparing_value: 250.0,
        };
        // Empty (0 chars) refused.
        assert!(matches!(
            build_trigger_condition(ConditionalSegment::NseEq, "", &spec, "2026-08-24", None),
            Err(ConditionalBuildError::BadSecurityId)
        ));
        // 21 chars refused; 20 chars accepted (inclusive boundary).
        let twenty_one = "1".repeat(21);
        assert!(matches!(
            build_trigger_condition(
                ConditionalSegment::NseEq,
                &twenty_one,
                &spec,
                "2026-08-24",
                None
            ),
            Err(ConditionalBuildError::BadSecurityId)
        ));
        let twenty = "1".repeat(20);
        assert!(
            build_trigger_condition(
                ConditionalSegment::NseEq,
                &twenty,
                &spec,
                "2026-08-24",
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn test_build_trigger_condition_rejects_non_alphanumeric_security_id_content() {
        // Round-4 review: length-only validation let whitespace/control/BiDi
        // garbage serialize verbatim into the wire body (DATA-813 class).
        let spec = TriggerConditionSpec::PriceWithValue {
            operator: TriggerOperator::GreaterThan,
            comparing_value: 250.0,
        };
        for garbage in [
            " 1333",            // leading space
            "13\u{202E}33",     // BiDi override
            "\n\n\n",           // control chars
            "13 33",            // interior space
            "13-33",            // punctuation
            "\u{202E}\u{202E}", // BiDi-only
        ] {
            assert!(
                matches!(
                    build_trigger_condition(
                        ConditionalSegment::NseEq,
                        garbage,
                        &spec,
                        "2026-08-24",
                        None
                    ),
                    Err(ConditionalBuildError::BadSecurityId)
                ),
                "garbage security_id {garbage:?} must refuse fail-closed"
            );
        }
        // Alphanumeric stays accepted (real Dhan IDs are numeric strings).
        assert!(
            build_trigger_condition(ConditionalSegment::NseEq, "1333", &spec, "2026-08-24", None)
                .is_ok()
        );
    }

    #[test]
    fn test_build_trigger_condition_rejects_malformed_exp_date() {
        let spec = TriggerConditionSpec::PriceWithValue {
            operator: TriggerOperator::GreaterThan,
            comparing_value: 250.0,
        };
        for bad in ["24-08-2019", "2026/08/24", "garbage", "", "2026-8-24"] {
            assert!(
                matches!(
                    build_trigger_condition(ConditionalSegment::NseEq, "1333", &spec, bad, None),
                    Err(ConditionalBuildError::BadExpDate)
                ),
                "expDate '{bad}' must be refused"
            );
        }
        // Format-only: a past date passes the pure clockless constructor —
        // rejecting it is Dhan's job (documented).
        assert!(
            build_trigger_condition(ConditionalSegment::NseEq, "1333", &spec, "2019-08-24", None)
                .is_ok()
        );
    }

    #[test]
    fn test_build_trigger_condition_comparing_value_boundary_nan_negative_infinite_rejected() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let spec = TriggerConditionSpec::PriceWithValue {
                operator: TriggerOperator::GreaterThan,
                comparing_value: bad,
            };
            assert!(matches!(
                build_trigger_condition(
                    ConditionalSegment::NseEq,
                    "1333",
                    &spec,
                    "2026-08-24",
                    None
                ),
                Err(ConditionalBuildError::NonFiniteComparingValue)
            ));
            let tech = TriggerConditionSpec::TechnicalWithValue {
                indicator: TriggerIndicatorName::MacdHist,
                time_frame: TriggerTimeFrame::Day,
                operator: TriggerOperator::LessThan,
                comparing_value: bad,
            };
            assert!(matches!(
                build_trigger_condition(
                    ConditionalSegment::NseEq,
                    "1333",
                    &tech,
                    "2026-08-24",
                    None
                ),
                Err(ConditionalBuildError::NonFiniteComparingValue)
            ));
        }
        // Negative FINITE values are legal (MACD histogram can be negative).
        let negative_finite = TriggerConditionSpec::TechnicalWithValue {
            indicator: TriggerIndicatorName::MacdHist,
            time_frame: TriggerTimeFrame::Day,
            operator: TriggerOperator::LessThan,
            comparing_value: -5.0,
        };
        assert!(
            build_trigger_condition(
                ConditionalSegment::NseEq,
                "1333",
                &negative_finite,
                "2026-08-24",
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn test_build_trigger_order_price_boundary_zero_market_ok_limit_zero_rejected() {
        // MARKET with price 0 + trigger 0 is legal.
        let mut market = limit_leg_spec();
        market.order_type = OrderType::Market;
        market.price_paise = 0;
        assert!(build_trigger_order(&market).is_ok());

        // LIMIT with price 0 refused.
        let mut limit_zero = limit_leg_spec();
        limit_zero.price_paise = 0;
        assert!(matches!(
            build_trigger_order(&limit_zero),
            Err(ConditionalBuildError::BadPrice { .. })
        ));

        // MARKET with a price refused.
        let mut market_priced = limit_leg_spec();
        market_priced.order_type = OrderType::Market;
        assert!(matches!(
            build_trigger_order(&market_priced),
            Err(ConditionalBuildError::BadPrice { .. })
        ));

        // Negative paise refused.
        let mut negative = limit_leg_spec();
        negative.price_paise = -1;
        assert!(matches!(
            build_trigger_order(&negative),
            Err(ConditionalBuildError::BadPrice { .. })
        ));

        // STOP_LOSS needs price > 0 AND trigger > 0.
        let mut stop_loss = limit_leg_spec();
        stop_loss.order_type = OrderType::StopLoss;
        assert!(matches!(
            build_trigger_order(&stop_loss),
            Err(ConditionalBuildError::BadTriggerPrice { .. })
        ));
        stop_loss.trigger_price_paise = 24_900;
        assert!(build_trigger_order(&stop_loss).is_ok());

        // STOP_LOSS_MARKET needs price == 0 AND trigger > 0.
        let mut slm = limit_leg_spec();
        slm.order_type = OrderType::StopLossMarket;
        slm.price_paise = 0;
        slm.trigger_price_paise = 24_900;
        assert!(build_trigger_order(&slm).is_ok());

        // LIMIT with a stray trigger refused.
        let mut limit_triggered = limit_leg_spec();
        limit_triggered.trigger_price_paise = 100;
        assert!(matches!(
            build_trigger_order(&limit_triggered),
            Err(ConditionalBuildError::BadTriggerPrice { .. })
        ));
    }

    #[test]
    fn test_build_trigger_order_quantity_boundary_zero_negative_i32_max_edge() {
        let mut zero = limit_leg_spec();
        zero.quantity = 0;
        assert!(matches!(
            build_trigger_order(&zero),
            Err(ConditionalBuildError::BadQuantity { quantity: 0 })
        ));

        let mut negative = limit_leg_spec();
        negative.quantity = -5;
        assert!(matches!(
            build_trigger_order(&negative),
            Err(ConditionalBuildError::BadQuantity { quantity: -5 })
        ));

        // i32::MAX accepted (inclusive edge); i32::MAX + 1 refused.
        let mut at_max = limit_leg_spec();
        at_max.quantity = i64::from(i32::MAX);
        assert!(build_trigger_order(&at_max).is_ok());

        let mut over_max = limit_leg_spec();
        over_max.quantity = i64::from(i32::MAX) + 1;
        assert!(matches!(
            build_trigger_order(&over_max),
            Err(ConditionalBuildError::BadQuantity { .. })
        ));
    }

    #[test]
    fn test_build_trigger_order_disclosed_quantity_boundary_30pct_edge() {
        // quantity 100: 30 (exactly 30%) refused, 31 accepted, 101 (> qty)
        // refused, 0 accepted, negative refused.
        let mut spec = limit_leg_spec();
        spec.quantity = 100;

        spec.disclosed_quantity = 30;
        assert!(matches!(
            build_trigger_order(&spec),
            Err(ConditionalBuildError::BadDisclosedQuantity { .. })
        ));

        spec.disclosed_quantity = 31;
        assert!(build_trigger_order(&spec).is_ok());

        spec.disclosed_quantity = 101;
        assert!(matches!(
            build_trigger_order(&spec),
            Err(ConditionalBuildError::BadDisclosedQuantity { .. })
        ));

        spec.disclosed_quantity = 0;
        assert!(build_trigger_order(&spec).is_ok());

        spec.disclosed_quantity = -1;
        assert!(matches!(
            build_trigger_order(&spec),
            Err(ConditionalBuildError::BadDisclosedQuantity { .. })
        ));
    }

    #[test]
    fn test_build_trigger_order_price_string_formatting_exact_paise_no_float() {
        let mut spec = limit_leg_spec();
        spec.price_paise = 25_000;
        let order = build_trigger_order(&spec).unwrap();
        assert_eq!(order.price, "250.00");
        assert_eq!(order.trigger_price, "0.00");

        spec.price_paise = 5;
        let order = build_trigger_order(&spec).unwrap();
        assert_eq!(order.price, "0.05");

        spec.price_paise = 142_805;
        let order = build_trigger_order(&spec).unwrap();
        assert_eq!(order.price, "1428.05");

        // Max-cap boundary: exactly the 1e7-rupee cap accepted, +1 refused.
        spec.price_paise = 1_000_000_000;
        let order = build_trigger_order(&spec).unwrap();
        assert_eq!(order.price, "10000000.00");
        spec.price_paise = 1_000_000_001;
        assert!(matches!(
            build_trigger_order(&spec),
            Err(ConditionalBuildError::BadPrice { .. })
        ));
    }

    #[test]
    fn test_build_trigger_order_rejects_co_bo_product_types() {
        for product in [ProductType::Co, ProductType::Bo] {
            let mut spec = limit_leg_spec();
            spec.product_type = product;
            assert!(matches!(
                build_trigger_order(&spec),
                Err(ConditionalBuildError::UnsupportedProductType)
            ));
        }
        // The family's four legal products all pass.
        for product in [
            ProductType::Cnc,
            ProductType::Intraday,
            ProductType::Margin,
            ProductType::Mtf,
        ] {
            let mut spec = limit_leg_spec();
            spec.product_type = product;
            assert!(build_trigger_order(&spec).is_ok());
        }
    }

    #[test]
    fn test_build_trigger_order_rejects_bad_security_id() {
        let mut spec = limit_leg_spec();
        spec.security_id = String::new();
        assert!(matches!(
            build_trigger_order(&spec),
            Err(ConditionalBuildError::BadSecurityId)
        ));
        spec.security_id = "1".repeat(21);
        assert!(matches!(
            build_trigger_order(&spec),
            Err(ConditionalBuildError::BadSecurityId)
        ));
        // Round-4: content garbage (whitespace/BiDi/control) refuses on BOTH
        // leg builders — validate_common_leg is the shared choke point.
        for garbage in [" 1333", "13\u{202E}33", "\n\n\n"] {
            spec.security_id = garbage.to_string();
            assert!(
                matches!(
                    build_trigger_order(&spec),
                    Err(ConditionalBuildError::BadSecurityId)
                ),
                "trigger-leg garbage security_id {garbage:?} must refuse"
            );
            let mut multi_spec = multi_leg_spec();
            multi_spec.security_id = garbage.to_string();
            assert!(
                matches!(
                    build_multi_order_request("1106656882", &[multi_spec]),
                    Err(ConditionalBuildError::BadSecurityId)
                ),
                "multi-leg garbage security_id {garbage:?} must refuse"
            );
        }
    }

    fn valid_condition() -> TriggerCondition {
        build_trigger_condition(
            ConditionalSegment::NseEq,
            "1333",
            &TriggerConditionSpec::PriceWithValue {
                operator: TriggerOperator::GreaterThan,
                comparing_value: 250.0,
            },
            "2026-08-24",
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_build_conditional_trigger_request_leg_count_boundary_0_1_15_16() {
        let leg = build_trigger_order(&limit_leg_spec()).unwrap();

        // 0 legs refused.
        assert!(matches!(
            build_conditional_trigger_request("100", valid_condition(), vec![]),
            Err(ConditionalBuildError::LegCountOutOfBounds { count: 0 })
        ));
        // 1 leg accepted.
        let one =
            build_conditional_trigger_request("100", valid_condition(), vec![leg.clone()]).unwrap();
        assert_eq!(one.orders.len(), 1);
        assert!(one.alert_id.is_none()); // Place semantics
        // 15 legs accepted (inclusive max).
        let fifteen = build_conditional_trigger_request(
            "100",
            valid_condition(),
            vec![leg.clone(); DHAN_CONDITIONAL_MAX_ORDERS_PER_REQUEST],
        )
        .unwrap();
        assert_eq!(fifteen.orders.len(), 15);
        // 16 legs refused.
        assert!(matches!(
            build_conditional_trigger_request("100", valid_condition(), vec![leg; 16]),
            Err(ConditionalBuildError::LegCountOutOfBounds { count: 16 })
        ));
    }

    #[test]
    fn test_build_requests_reject_bad_dhan_client_id_boundary_empty_21_chars_charset() {
        let leg = build_trigger_order(&limit_leg_spec()).unwrap();
        // Empty, whitespace, BiDi, control, and 21-char IDs refused on BOTH
        // builders (docs mark dhanClientId Required — an empty ID is a
        // guaranteed-invalid order body, refused locally).
        let twenty_one = "1".repeat(21);
        for bad in ["", " ", "  100", "110\u{202E}882", "10\n0", &twenty_one] {
            assert!(
                matches!(
                    build_conditional_trigger_request(bad, valid_condition(), vec![leg.clone()]),
                    Err(ConditionalBuildError::BadDhanClientId)
                ),
                "conditional builder must refuse dhanClientId {bad:?}"
            );
            assert!(
                matches!(
                    build_multi_order_request(bad, &[multi_leg_spec()]),
                    Err(ConditionalBuildError::BadDhanClientId)
                ),
                "multi builder must refuse dhanClientId {bad:?}"
            );
        }
        // 20 chars (inclusive boundary) and the real numeric-ID shape pass.
        let twenty = "1".repeat(20);
        for good in [twenty.as_str(), "1106656882", "100"] {
            assert!(
                build_conditional_trigger_request(good, valid_condition(), vec![leg.clone()])
                    .is_ok(),
                "conditional builder must accept dhanClientId {good:?}"
            );
            assert!(
                build_multi_order_request(good, &[multi_leg_spec()]).is_ok(),
                "multi builder must accept dhanClientId {good:?}"
            );
        }
    }

    #[test]
    fn test_with_alert_id_sets_field() {
        let leg = build_trigger_order(&limit_leg_spec()).unwrap();
        let request =
            build_conditional_trigger_request("100", valid_condition(), vec![leg]).unwrap();
        assert!(request.alert_id.is_none());
        let modified = with_alert_id(request, "12345");
        assert_eq!(modified.alert_id.as_deref(), Some("12345"));
    }

    #[test]
    fn test_build_multi_order_request_boundary_15_legs_max_16_rejected_zero_rejected() {
        // 0 legs refused.
        assert!(matches!(
            build_multi_order_request("100", &[]),
            Err(ConditionalBuildError::LegCountOutOfBounds { count: 0 })
        ));
        // 15 legs accepted (inclusive max).
        let fifteen = vec![multi_leg_spec(); DHAN_CONDITIONAL_MAX_ORDERS_PER_REQUEST];
        let request = build_multi_order_request("100", &fifteen).unwrap();
        assert_eq!(request.orders.len(), 15);
        // 16 legs refused.
        let sixteen = vec![multi_leg_spec(); 16];
        assert!(matches!(
            build_multi_order_request("100", &sixteen),
            Err(ConditionalBuildError::LegCountOutOfBounds { count: 16 })
        ));
    }

    #[test]
    fn test_build_multi_order_request_assigns_sequences_one_to_n() {
        let specs = vec![multi_leg_spec(); 3];
        let request = build_multi_order_request("100", &specs).unwrap();
        let sequences: Vec<&str> = request
            .orders
            .iter()
            .map(|leg| leg.sequence.as_str())
            .collect();
        assert_eq!(sequences, ["1", "2", "3"]);
        // Wire numbers, prices as floats, disclosed as int.
        assert!((request.orders[0].price - 250.0).abs() < f64::EPSILON);
        assert_eq!(request.orders[0].disclosed_quantity, 0);
        assert_eq!(request.dhan_client_id, "100");
    }

    #[test]
    fn test_build_multi_order_request_correlation_id_boundary_30_max_31_rejected_and_charset() {
        // 30 chars (inclusive cap) accepted.
        let mut spec = multi_leg_spec();
        spec.correlation_id = Some("a".repeat(30));
        assert!(build_multi_order_request("100", &[spec.clone()]).is_ok());
        // 31 chars refused.
        spec.correlation_id = Some("a".repeat(31));
        assert!(matches!(
            build_multi_order_request("100", &[spec.clone()]),
            Err(ConditionalBuildError::BadCorrelationId)
        ));
        // Legal charset [a-zA-Z0-9 _-] accepted.
        spec.correlation_id = Some("Trade 42_leg-A".to_string());
        assert!(build_multi_order_request("100", &[spec.clone()]).is_ok());
        // Illegal charset refused.
        for bad in ["semi;colon", "new\nline", "uni-∞", "quote\"x"] {
            spec.correlation_id = Some(bad.to_string());
            assert!(matches!(
                build_multi_order_request("100", &[spec.clone()]),
                Err(ConditionalBuildError::BadCorrelationId)
            ));
        }
    }

    #[test]
    fn test_build_multi_order_request_amo_coupling_serializes_flag_and_time_together() {
        // Some(AmoTime) ⇒ flag true + timing string.
        let mut spec = multi_leg_spec();
        spec.amo = Some(AmoTime::Open30);
        let request = build_multi_order_request("100", &[spec]).unwrap();
        assert_eq!(request.orders[0].after_market_order, Some(true));
        assert_eq!(request.orders[0].amo_time.as_deref(), Some("OPEN_30"));
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"afterMarketOrder\":true"));
        assert!(json.contains("\"amoTime\":\"OPEN_30\""));

        // None ⇒ NEITHER field on the wire (mismatch unrepresentable).
        let request = build_multi_order_request("100", &[multi_leg_spec()]).unwrap();
        assert!(request.orders[0].after_market_order.is_none());
        assert!(request.orders[0].amo_time.is_none());
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("afterMarketOrder"));
        assert!(!json.contains("amoTime"));
    }

    #[test]
    fn test_build_multi_order_request_price_paise_overflow_cap_rejected() {
        // Above the 1e7-rupee plausibility cap refused (per-leg BadPrice).
        let mut spec = multi_leg_spec();
        spec.price_paise = 1_000_000_001;
        assert!(matches!(
            build_multi_order_request("100", &[spec]),
            Err(ConditionalBuildError::BadPrice { .. })
        ));
        // Exactly at the cap accepted, converts exactly.
        let mut spec = multi_leg_spec();
        spec.price_paise = 1_000_000_000;
        let request = build_multi_order_request("100", &[spec]).unwrap();
        assert!((request.orders[0].price - 10_000_000.0).abs() < f64::EPSILON);
        // i64::MAX paise refused (never reaches the f64 conversion).
        let mut spec = multi_leg_spec();
        spec.price_paise = i64::MAX;
        assert!(matches!(
            build_multi_order_request("100", &[spec]),
            Err(ConditionalBuildError::BadPrice { .. })
        ));
    }

    #[test]
    fn test_conditional_segments_as_str_exact_wire_forms() {
        // Exhaustive matches — a new variant fails compile without a test edit.
        for segment in [
            ConditionalSegment::NseEq,
            ConditionalSegment::BseEq,
            ConditionalSegment::IdxI,
        ] {
            let wire = match segment {
                ConditionalSegment::NseEq => "NSE_EQ",
                ConditionalSegment::BseEq => "BSE_EQ",
                ConditionalSegment::IdxI => "IDX_I",
            };
            assert_eq!(segment.as_str(), wire);
        }
        for segment in [ConditionalLegSegment::NseEq, ConditionalLegSegment::BseEq] {
            let wire = match segment {
                ConditionalLegSegment::NseEq => "NSE_EQ",
                ConditionalLegSegment::BseEq => "BSE_EQ",
            };
            assert_eq!(segment.as_str(), wire);
        }
        for timing in [
            AmoTime::PreOpen,
            AmoTime::Open,
            AmoTime::Open30,
            AmoTime::Open60,
        ] {
            let wire = match timing {
                AmoTime::PreOpen => "PRE_OPEN",
                AmoTime::Open => "OPEN",
                AmoTime::Open30 => "OPEN_30",
                AmoTime::Open60 => "OPEN_60",
            };
            assert_eq!(timing.as_str(), wire);
        }
    }

    #[test]
    fn test_trigger_indicator_name_as_str_all_21() {
        let all = [
            (TriggerIndicatorName::Sma5, "SMA_5"),
            (TriggerIndicatorName::Sma10, "SMA_10"),
            (TriggerIndicatorName::Sma20, "SMA_20"),
            (TriggerIndicatorName::Sma50, "SMA_50"),
            (TriggerIndicatorName::Sma100, "SMA_100"),
            (TriggerIndicatorName::Sma200, "SMA_200"),
            (TriggerIndicatorName::Ema5, "EMA_5"),
            (TriggerIndicatorName::Ema10, "EMA_10"),
            (TriggerIndicatorName::Ema20, "EMA_20"),
            (TriggerIndicatorName::Ema50, "EMA_50"),
            (TriggerIndicatorName::Ema100, "EMA_100"),
            (TriggerIndicatorName::Ema200, "EMA_200"),
            (TriggerIndicatorName::BbUpper, "BB_UPPER"),
            (TriggerIndicatorName::BbLower, "BB_LOWER"),
            (TriggerIndicatorName::Rsi14, "RSI_14"),
            (TriggerIndicatorName::Atr14, "ATR_14"),
            (TriggerIndicatorName::Stochastic, "STOCHASTIC"),
            (TriggerIndicatorName::StochRsi14, "STOCHRSI_14"),
            (TriggerIndicatorName::Macd26, "MACD_26"),
            (TriggerIndicatorName::Macd12, "MACD_12"),
            (TriggerIndicatorName::MacdHist, "MACD_HIST"),
        ];
        assert_eq!(all.len(), 21);
        for (variant, wire) in all {
            assert_eq!(variant.as_str(), wire);
        }
    }

    // Proptest invariant: arbitrary inputs either build or refuse with a
    // typed error — the constructor NEVER panics.
    proptest::proptest! {
        #[test]
        fn proptest_build_multi_order_request_never_panics_on_arbitrary_spec(
            dhan_client_id in ".{0,24}",
            segment_is_bse in proptest::prelude::any::<bool>(),
            sell in proptest::prelude::any::<bool>(),
            product_idx in 0usize..6,
            order_type_idx in 0usize..4,
            ioc in proptest::prelude::any::<bool>(),
            security_id in ".{0,24}",
            quantity in proptest::prelude::any::<i64>(),
            price_paise in proptest::prelude::any::<i64>(),
            trigger_price_paise in proptest::prelude::any::<i64>(),
            disclosed_quantity in proptest::prelude::any::<i64>(),
            correlation_id in proptest::option::of(".{0,40}"),
            amo_idx in proptest::option::of(0usize..4),
        ) {
            let products = [
                ProductType::Cnc,
                ProductType::Intraday,
                ProductType::Margin,
                ProductType::Mtf,
                ProductType::Co,
                ProductType::Bo,
            ];
            let order_types = [
                OrderType::Limit,
                OrderType::Market,
                OrderType::StopLoss,
                OrderType::StopLossMarket,
            ];
            let amo_times = [AmoTime::PreOpen, AmoTime::Open, AmoTime::Open30, AmoTime::Open60];
            let spec = MultiOrderLegSpec {
                segment: if segment_is_bse {
                    ConditionalLegSegment::BseEq
                } else {
                    ConditionalLegSegment::NseEq
                },
                transaction_type: if sell {
                    TransactionType::Sell
                } else {
                    TransactionType::Buy
                },
                product_type: products[product_idx],
                order_type: order_types[order_type_idx],
                validity: if ioc { OrderValidity::Ioc } else { OrderValidity::Day },
                security_id,
                quantity,
                price_paise,
                trigger_price_paise,
                disclosed_quantity,
                correlation_id,
                amo: amo_idx.map(|idx| amo_times[idx]),
            };
            // Ok or typed Err — never a panic (client id fuzzed too).
            let _ = build_multi_order_request(&dhan_client_id, &[spec]);
        }
    }
}
