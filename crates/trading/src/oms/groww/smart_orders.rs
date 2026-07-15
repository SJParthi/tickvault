//! Groww Smart Orders (GTT / OCO) client — the `GROWW-OCO-*` area of the
//! §39.3 collision contract (operator authorization 2026-07-14/15).
//!
//! # Ground truth
//! `docs/groww-ref/18-smart-orders-schemas.md` (the FULL recovered schemas —
//! every wire field name below is verbatim from that capture) +
//! `docs/groww-ref/16-orders-margins-portfolio.md` (endpoint inventory + GA
//! error codes). Prices on the smart-order wire are DECIMAL STRINGS
//! ("All prices should be passed as decimal strings" — doc 18 §8.7);
//! internally this module speaks INTEGER PAISE (`i64`) only, converted at
//! the wire boundary by [`paise_to_decimal_string`] /
//! [`decimal_string_to_paise`] — no `f64` anywhere.
//!
//! # The 4-gate live-fire lattice (§39.2 — the safety core)
//! Every WRITE method (create OCO/GTT, modify, cancel) checks, IN ORDER:
//! 1. `settings.write_enabled == false` → [`SmartOrderOutcome::DisabledByConfig`]
//!    (Gate 1 — config default-OFF);
//! 2. validation + serialization of the fully-typed body;
//! 3. [`GROWW_ORDER_LIVE_FIRE`]` == false` → [`SmartOrderOutcome::DryRun`]
//!    carrying the exact endpoint/method/body that WOULD have been sent
//!    (Gate 3 — the hardcoded const).
//!
//! The `reqwest` send sits STATICALLY after both gates — while either gate
//! is closed, no HTTP request is even constructed. Gate 2 is the
//! non-default `groww_orders` cargo feature this whole subtree compiles
//! under; Gate 4 is the rule lock (§39 / §10). READ methods (status get,
//! list) gate on `settings.read_enabled` only — they place no order.
//!
//! # Gate-5 note
//! The Groww order-side path strings (`/v1/order-advance/...`) are PRIVATE
//! consts in THIS file — the lattice guard's workspace scan
//! (`groww_order_lattice_guard::test_gate5_no_ungated_groww_order_side_call_site`)
//! allows them ONLY inside `oms/groww/`.
//!
//! # Error codes (runbook `.claude/rules/project/groww-oco-error-codes.md`)
//! - `GROWW-OCO-01` — placement-class write (create/cancel) failed at the
//!   live HTTP leg (unreachable while Gate 3 is closed).
//! - `GROWW-OCO-02` — sibling-cancel unverified past deadline: the VERDICT
//!   type ([`SiblingVerdict::Violation`]) ships here; the EMIT site lands
//!   with the later reconcile-orchestrator PR.
//! - `GROWW-OCO-03` — OCO-vs-position reconcile mismatch: the VERDICT type
//!   ([`ReconcileVerdict`]) ships here; the EMIT site lands with the later
//!   reconcile-orchestrator PR.
//! - `GROWW-OCO-04` — modify rejected (unreachable while Gate 3 is closed).
//! - `GROWW-OCO-05` — status poller (read GET) degraded.
//!
//! # Token discipline
//! The access token comes from a caller-supplied READ-ONLY provider (the
//! shared-minter SSM read — `groww-shared-token-minter-2026-07-02.md`);
//! this module NEVER mints, never logs the token, and never puts it in a
//! URL. Error bodies are bounded + secret-redacted via the house
//! [`capture_rest_error_body`] choke point.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tickvault_common::broker_order_events::BrokerOrderStatus;
use tickvault_common::config::GrowwOrdersConfig;
use tickvault_common::constants::{
    GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE, GROWW_ORDER_LIVE_FIRE,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;
use tracing::error;

// ---------------------------------------------------------------------------
// Endpoint constants (PRIVATE — Gate-5: these strings may exist ONLY inside
// oms/groww/; see the module doc)
// ---------------------------------------------------------------------------

/// Production Groww REST base (doc 18 §1).
const GROWW_SMART_ORDER_BASE_URL: &str = "https://api.groww.in"; // APPROVED: Gate-5 keeps every Groww order-side URL string inside oms/groww/ (constants.rs precedent)
/// `POST` — create a GTT or OCO smart order (doc 18 §1 [R:L21]).
const SMART_ORDER_CREATE_PATH: &str = "/v1/order-advance/create";
/// `PUT {prefix}/{smart_order_id}` (doc 18 §1 [R:L194]).
const SMART_ORDER_MODIFY_PATH_PREFIX: &str = "/v1/order-advance/modify";
/// `POST {prefix}/{segment}/{smart_order_type}/{smart_order_id}` — path
/// params only, no body (doc 18 §4 [R:L325]).
const SMART_ORDER_CANCEL_PATH_PREFIX: &str = "/v1/order-advance/cancel";
/// `GET {prefix}/{segment}/{smart_order_type}/internal/{smart_order_id}` —
/// note the literal `internal` path segment (doc 18 §4 [R:L370]).
const SMART_ORDER_STATUS_PATH_PREFIX: &str = "/v1/order-advance/status";
/// `GET` — list smart orders with optional filters (doc 18 §5 [R:L405]).
const SMART_ORDER_LIST_PATH: &str = "/v1/order-advance/list";

/// Per-request timeout (seconds) — one bounded round-trip, no internal
/// retry ladder (the §38 legs' pacing discipline; the future reconcile
/// poller owns cadence).
const SMART_ORDER_REQUEST_TIMEOUT_SECS: u64 = 5;
/// Delay before the SINGLE bounded retry on a TRANSPORT-class send error
/// (never on any HTTP status — a 4xx/5xx is a broker verdict, not a blip).
const SMART_ORDER_TRANSPORT_RETRY_DELAY_MS: u64 = 250;

/// Wire literal for the OCO smart-order type (doc 18 §2).
pub const SMART_ORDER_TYPE_OCO: &str = "OCO";
/// Wire literal for the GTT smart-order type (doc 18 §1).
pub const SMART_ORDER_TYPE_GTT: &str = "GTT";
/// The only smart-order segment OCO accepts today (doc 18 §8.1 — the CASH
/// arm is intra-doc CONTRADICTED, so we fail closed on FNO-only).
const OCO_SEGMENT_FNO: &str = "FNO";

/// `reference_id` constraint (doc 18 §2: "alphanumeric string (8-20
/// characters) ... with at most two hyphens (-) allowed").
const REFERENCE_ID_MIN_LEN: usize = 8;
/// See [`REFERENCE_ID_MIN_LEN`].
const REFERENCE_ID_MAX_LEN: usize = 20;
/// See [`REFERENCE_ID_MIN_LEN`].
const REFERENCE_ID_MAX_HYPHENS: usize = 2;

/// List pagination bounds (doc 18 §5: page min 0 / max 500; page_size
/// min 1 / max 50).
const LIST_PAGE_MAX: u32 = 500;
/// See [`LIST_PAGE_MAX`].
const LIST_PAGE_SIZE_MIN: u32 = 1;
/// See [`LIST_PAGE_MAX`].
const LIST_PAGE_SIZE_MAX: u32 = 50;

/// Bound on any raw input echoed into a typed error (forensic, never a
/// full-body dump; the [`capture_rest_error_body`] discipline for
/// NON-broker strings).
const ERROR_ECHO_MAX_CHARS: usize = 48;

// ---------------------------------------------------------------------------
// Settings (decoupled from config.rs — boot wiring maps config → this)
// ---------------------------------------------------------------------------

/// Runtime settings for the smart-order client — a config-DECOUPLED struct
/// so this module never grows a hard dependency on `config.rs` evolution.
/// Boot wiring builds it via the [`From<&GrowwOrdersConfig>`] mapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowwSmartOrderSettings {
    /// Gate for the read-only GETs (status get + list). Default `false`.
    pub read_enabled: bool,
    /// Gate for the write methods (create/modify/cancel). Default `false`.
    /// Even when `true`, writes stop at the [`GROWW_ORDER_LIVE_FIRE`]
    /// dry-run gate (Gate 3).
    pub write_enabled: bool,
    /// Cadence (seconds) for the future OCO reconcile poller. Default 15.
    pub reconcile_poll_secs: u64,
    /// Seconds after one OCO leg executes within which the sibling must be
    /// VERIFIED cancelled (else [`SiblingVerdict::Violation`]). Default 30.
    pub sibling_cancel_deadline_secs: u64,
}

impl Default for GrowwSmartOrderSettings {
    fn default() -> Self {
        Self {
            read_enabled: false,
            write_enabled: false,
            reconcile_poll_secs: 15,
            sibling_cancel_deadline_secs: 30,
        }
    }
}

impl From<&GrowwOrdersConfig> for GrowwSmartOrderSettings {
    // WIRING-EXEMPT: §39.3 area PR — the boot wiring that maps `[groww_orders]`
    // into this settings struct lands with the later orchestrator PR; tested inline.
    fn from(cfg: &GrowwOrdersConfig) -> Self {
        Self {
            read_enabled: cfg.smart_orders_read,
            write_enabled: cfg.smart_orders_write,
            reconcile_poll_secs: cfg.oco_reconcile_poll_secs,
            sibling_cancel_deadline_secs: cfg.oco_sibling_cancel_deadline_secs,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire enums (doc 18 §2/§6 verbatim values)
// ---------------------------------------------------------------------------

/// Order type of a smart-order leg / embedded order. Wire values `LIMIT` /
/// `MARKET` / `SL` / `SL_M` (doc 13 annexure + doc 18 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SmartLegOrderType {
    /// `LIMIT` — requires an explicit `price`.
    Limit,
    /// `MARKET` — no `price`.
    Market,
    /// `SL` (stop-loss limit) — requires an explicit `price`.
    Sl,
    /// `SL_M` (stop-loss market) — `price` MUST be null (doc 18 §2 body
    /// example shows `"price": null` for `SL_M`).
    SlM,
}

/// Trade direction. Wire values `BUY` / `SELL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionType {
    /// `BUY` — for an OCO, the exit direction of a SHORT position.
    Buy,
    /// `SELL` — for an OCO, the exit direction of a LONG position.
    Sell,
}

/// GTT trigger monitor direction. Wire values `UP` / `DOWN` (doc 18 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TriggerDirection {
    /// `UP` — trigger when price crosses ABOVE the trigger price.
    Up,
    /// `DOWN` — trigger when price crosses BELOW the trigger price.
    Down,
}

// ---------------------------------------------------------------------------
// Request wire structs (snake_case field names verbatim from doc 18)
// ---------------------------------------------------------------------------

/// One OCO leg (`target` or `stop_loss`) — doc 18 §2 [R:L140–L159].
/// `price` serializes as `null` when `None` (the doc's own SL_M example
/// carries an explicit `"price": null`), so NO `skip_serializing_if`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcoLeg {
    /// Trigger price as a DECIMAL STRING (e.g. `"120.50"`). Build it from
    /// integer paise via [`paise_to_decimal_string`].
    pub trigger_price: String,
    /// Leg order type. Target legs: `LIMIT`/`MARKET`. Stop-loss legs:
    /// `SL`/`SL_M` (validated by [`validate_oco_create`]).
    pub order_type: SmartLegOrderType,
    /// Leg limit price (decimal string). Required iff the leg order type
    /// needs one (`LIMIT` / `SL`); MUST be `None` for `SL_M`.
    pub price: Option<String>,
}

/// `POST /v1/order-advance/create` body for an OCO — doc 18 §2 verbatim
/// field names. OCO is EXIT-ONLY ("OCO orders are meant to exit an
/// existing position" — doc 18 §2 [P:L178–L182]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OcoCreateRequest {
    /// Idempotency key — see [`validate_reference_id`].
    pub reference_id: String,
    /// Always [`SMART_ORDER_TYPE_OCO`].
    pub smart_order_type: &'static str,
    /// `FNO` only today ([`validate_oco_create`] rejects `CASH` — doc 18
    /// §8.1 records an intra-doc contradiction on CASH OCO; we fail closed).
    pub segment: String,
    /// Exchange trading symbol (e.g. `NIFTY25OCT24000CE`).
    pub trading_symbol: String,
    /// Total quantity for BOTH legs — must be ≤ `abs(net_position_quantity)`
    /// and a lot-size multiple (doc 18 §2).
    pub quantity: i64,
    /// Current net position in this symbol — "Used to derive leg directions
    /// and validate quantity" (doc 18 §2).
    pub net_position_quantity: i64,
    /// "Direction of protection/exit for your position" (doc 18 §2).
    pub transaction_type: TransactionType,
    /// Take-profit leg (`LIMIT`/`MARKET`).
    pub target: OcoLeg,
    /// Stop-loss leg (`SL`/`SL_M`).
    pub stop_loss: OcoLeg,
    /// Product for the OCO (e.g. `MIS`, `NRML`).
    pub product_type: String,
    /// Exchange (e.g. `NSE`).
    pub exchange: String,
    /// Validity for both legs (e.g. `DAY` — the only documented value).
    pub duration: String,
}

/// The GTT post-trigger embedded order — doc 18 / doc 16 GTT schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GttEmbeddedOrder {
    /// Post-trigger execution order type.
    pub order_type: SmartLegOrderType,
    /// Post-trigger limit price — "required if order.order_type is LIMIT
    /// or SL" (doc); `None` (→ `null`) for `MARKET`/`SL_M`.
    pub price: Option<String>,
    /// `BUY` / `SELL`.
    pub transaction_type: TransactionType,
}

/// `POST /v1/order-advance/create` body for a GTT — verbatim field names
/// from the GTT create schema (doc 16 §1 row 14 / doc 18 §0).
///
/// `child_legs` is DELIBERATELY OMITTED: its object structure is UNKNOWN
/// (doc 18 §9 probe **P1** — the schema says only "object … target/stop-loss"
/// and every doc example shows `null`). Add the field ONLY after the live
/// P1 probe resolves the accepted shape — never contract on a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GttCreateRequest {
    /// Idempotency key — see [`validate_reference_id`].
    pub reference_id: String,
    /// Always [`SMART_ORDER_TYPE_GTT`].
    pub smart_order_type: &'static str,
    /// `CASH` or `FNO` (GTT supports both; COMMODITY never).
    pub segment: String,
    /// Exchange trading symbol.
    pub trading_symbol: String,
    /// Quantity for the post-trigger order ("For FNO, must respect lot
    /// size" — doc).
    pub quantity: i64,
    /// Trigger price as a decimal string.
    pub trigger_price: String,
    /// Direction to monitor relative to the trigger price.
    pub trigger_direction: TriggerDirection,
    /// The post-trigger order.
    pub order: GttEmbeddedOrder,
    /// Product for the post-trigger order (`CNC`/`MIS`/`NRML`).
    pub product_type: String,
    /// Exchange (e.g. `NSE`).
    pub exchange: String,
    /// Validity of the POST-TRIGGER order (doc: `DAY`).
    pub duration: String,
}

/// A partial leg inside a modify body — doc 18 §3 shows legs sent
/// partially: `"target": {"trigger_price": "122.00"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModifyLeg {
    /// The new trigger price (decimal string).
    pub trigger_price: String,
}

/// `PUT /v1/order-advance/modify/{smart_order_id}` body — ONLY the
/// OCO-modifiable fields (doc 18 §3: `quantity`, `duration`,
/// `product_type`, `target.trigger_price`, `stop_loss.trigger_price`).
/// Leg `order_type`/`price` are NOT modifiable by the API
/// ([P:L327–L331]) — use cancel + create for those. `smart_order_type` +
/// `segment` are routing-REQUIRED in the body ([R:L243–L244]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmartOrderModifyRequest {
    /// Routing-required: `OCO` / `GTT`.
    pub smart_order_type: &'static str,
    /// Routing-required: `FNO` / `CASH`.
    pub segment: String,
    /// New total quantity (absent = unchanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    /// New duration (absent = unchanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// New product (absent = unchanged; doc: "MIS ↔ NRML for FNO").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_type: Option<String>,
    /// New target trigger price (absent = unchanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<ModifyLeg>,
    /// New stop-loss trigger price (absent = unchanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<ModifyLeg>,
}

// ---------------------------------------------------------------------------
// Response wire structs (#[serde(default)] tolerance everywhere — the
// broker may add/omit fields; unknown fields are ignored by serde)
// ---------------------------------------------------------------------------

/// The GA error object inside a FAILURE envelope (doc 16 §5:
/// `{"status":"FAILURE","error":{"code":"GA001","message":"..."}}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GaError {
    /// GA wire code (`GA000`..`GA007`).
    pub code: String,
    /// Request-specific message (bounded + redacted before any log).
    pub message: String,
}

/// The generic Groww envelope: `{ "status": ..., "payload": ..., "error": ... }`.
/// Tolerant: every field defaults, so a drifted body degrades to a typed
/// parse verdict, never a panic.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GaEnvelope<T> {
    /// `SUCCESS` / `FAILURE` (empty when absent).
    #[serde(default)]
    pub status: String,
    /// The success payload (absent on failure).
    #[serde(default = "Option::default")]
    pub payload: Option<T>,
    /// The failure object (absent on success).
    #[serde(default)]
    pub error: Option<GaError>,
}

/// A deserialized leg on a smart-order RESPONSE (same shape as [`OcoLeg`]
/// but fully tolerant — the broker may omit any field).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ResponseLeg {
    /// Trigger price (decimal string) as echoed by the broker.
    pub trigger_price: String,
    /// Leg order type as a RAW string (kept verbatim — response tolerance;
    /// an unknown future value must never fail the whole parse).
    pub order_type: String,
    /// Leg limit price (`null` for market-class legs).
    pub price: Option<String>,
}

/// The smart-order object of the §7 response schemas (GTT + OCO fields
/// unioned; GTT-only fields like `trigger_price` are `Option`). The
/// response `ltp` (the schema's only non-string price, GTT-only) is
/// deliberately NOT modeled — serde ignores it; nothing here consumes a
/// float price.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SmartOrderPayload {
    /// Broker-assigned id (`gtt_…` / `oco_…`).
    pub smart_order_id: String,
    /// `GTT` / `OCO`.
    pub smart_order_type: String,
    /// RAW lifecycle status string — map via
    /// [`SmartOrderStatus::from_groww_smart_status`].
    pub status: String,
    /// Trading symbol.
    pub trading_symbol: String,
    /// Exchange.
    pub exchange: String,
    /// Segment (GTT responses carry it; OCO's §7 schema does not).
    pub segment: String,
    /// Quantity (absent on the partial modify echo → `None`).
    pub quantity: Option<i64>,
    /// Product type.
    pub product_type: String,
    /// Duration.
    pub duration: String,
    /// GTT-only: trigger price (decimal string).
    pub trigger_price: Option<String>,
    /// GTT-only: `UP`/`DOWN` (raw).
    pub trigger_direction: Option<String>,
    /// GTT-only: the embedded post-trigger order (raw-tolerant).
    pub order: Option<ResponseLeg>,
    /// OCO-only: target leg.
    pub target: Option<ResponseLeg>,
    /// OCO-only: stop-loss leg.
    pub stop_loss: Option<ResponseLeg>,
    /// Per-order capability flag (doc 18 §7).
    pub is_cancellation_allowed: Option<bool>,
    /// Per-order capability flag (doc 18 §7).
    pub is_modification_allowed: Option<bool>,
    /// ISO 8601 string, no timezone suffix (doc 18 §7).
    pub created_at: Option<String>,
    /// ISO 8601 string (`null` on OCO create — doc 18 §2).
    pub expire_at: Option<String>,
    /// ISO 8601 string (`null` until triggered).
    pub triggered_at: Option<String>,
    /// ISO 8601 string.
    pub updated_at: Option<String>,
    /// GTT-only: remark / status message.
    pub remark: Option<String>,
}

/// `GET /v1/order-advance/list` payload: `{"orders":[…]}` (doc 18 §5).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SmartOrderListPayload {
    /// Full smart-order objects, one per row.
    pub orders: Vec<SmartOrderPayload>,
}

// ---------------------------------------------------------------------------
// Smart-order lifecycle status (doc 18 §6 — the 6-value enum, DISTINCT
// from the 12-value regular-order enum)
// ---------------------------------------------------------------------------

/// Smart-order lifecycle status — total no-panic mapping of the doc 18 §6
/// enum. An unrecognized wire value maps to [`SmartOrderStatus::Unknown`]
/// carrying the (trimmed) raw string verbatim, mirroring the
/// [`BrokerOrderStatus::from_groww_status`] raw-preservation discipline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SmartOrderStatus {
    /// `ACTIVE` — monitoring trigger conditions.
    Active,
    /// `TRIGGERED` — trigger condition met, order placed.
    Triggered,
    /// `CANCELLED` — user cancelled (terminal).
    Cancelled,
    /// `EXPIRED` — time/date expiry (terminal).
    Expired,
    /// `FAILED` — placement or trigger failed (terminal).
    Failed,
    /// `COMPLETED` — successfully completed (terminal).
    Completed,
    /// Outside the documented vocabulary — raw string preserved.
    Unknown(String),
}

impl SmartOrderStatus {
    /// Total, no-panic mapper from the raw wire string. Case-insensitive +
    /// whitespace-trimmed (the [`BrokerOrderStatus::from_groww_status`]
    /// style); anything unrecognized → [`Self::Unknown`] with the trimmed
    /// original preserved.
    #[must_use]
    pub fn from_groww_smart_status(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "ACTIVE" => Self::Active,
            "TRIGGERED" => Self::Triggered,
            "CANCELLED" => Self::Cancelled,
            "EXPIRED" => Self::Expired,
            "FAILED" => Self::Failed,
            "COMPLETED" => Self::Completed,
            _ => Self::Unknown(raw.trim().to_owned()),
        }
    }

    /// Stable label (audit / metric-label use). `Unknown` echoes its
    /// preserved raw string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Active => "ACTIVE",
            Self::Triggered => "TRIGGERED",
            Self::Cancelled => "CANCELLED",
            Self::Expired => "EXPIRED",
            Self::Failed => "FAILED",
            Self::Completed => "COMPLETED",
            Self::Unknown(raw) => raw.as_str(),
        }
    }

    /// Terminal = no further lifecycle transition expected. `TRIGGERED` is
    /// NOT terminal (the fired leg may still be working; the
    /// TRIGGERED-vs-COMPLETED boundary prose is undocumented — doc 18 §6).
    /// `Unknown` is NOT terminal (we cannot assert done-ness from a string
    /// we did not understand).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Expired | Self::Failed | Self::Completed
        )
    }
}

// ---------------------------------------------------------------------------
// Money discipline — integer paise ↔ decimal strings, exact, no float
// ---------------------------------------------------------------------------

/// Renders integer paise as the wire's decimal string with EXACTLY two
/// decimals — pure integer arithmetic, no float anywhere.
/// `1_950_050` → `"19500.50"`; `5` → `"0.05"`. Negative paise render with
/// a leading `-` (total function; validation layers reject negatives where
/// prices must be positive).
#[must_use]
pub fn paise_to_decimal_string(paise: i64) -> String {
    // unsigned_abs handles i64::MIN without overflow.
    let abs = paise.unsigned_abs();
    let rupees = abs / 100;
    let frac = abs % 100;
    if paise < 0 {
        format!("-{rupees}.{frac:02}")
    } else {
        format!("{rupees}.{frac:02}")
    }
}

/// Parses a wire decimal string into integer paise — fail-closed:
/// - at most 2 decimal digits (`>2dp` rejected — paise are the atom);
/// - digits + at most one `.` only (so `NaN`/`inf`/exponents/signs are
///   rejected by charset, never parsed via float);
/// - negatives rejected (no negative price exists on this surface);
/// - checked arithmetic (overflow → typed error, never wraparound).
pub fn decimal_string_to_paise(raw: &str) -> Result<i64, SmartOrderValidationError> {
    let trimmed = raw.trim();
    let echo = || bounded_echo(raw);
    if trimmed.is_empty() {
        return Err(SmartOrderValidationError::InvalidPrice {
            raw: echo(),
            reason: "empty",
        });
    }
    if trimmed.starts_with('-') {
        return Err(SmartOrderValidationError::InvalidPrice {
            raw: echo(),
            reason: "negative not allowed",
        });
    }
    let (int_part, frac_part) = match trimmed.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (trimmed, None),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SmartOrderValidationError::InvalidPrice {
            raw: echo(),
            reason: "non-digit integer part",
        });
    }
    let frac_paise: i64 = match frac_part {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 2 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return Err(SmartOrderValidationError::InvalidPrice {
                    raw: echo(),
                    reason: "fraction must be 1-2 digits",
                });
            }
            // "5" means 50 paise; "05" means 5 paise.
            let mut v: i64 = 0;
            for b in f.bytes() {
                v = v * 10 + i64::from(b - b'0');
            }
            if f.len() == 1 { v * 10 } else { v }
        }
    };
    let int_rupees: i64 =
        int_part
            .parse()
            .map_err(|_| SmartOrderValidationError::InvalidPrice {
                raw: echo(),
                reason: "integer part overflow",
            })?;
    int_rupees
        .checked_mul(100)
        .and_then(|p| p.checked_add(frac_paise))
        .ok_or(SmartOrderValidationError::InvalidPrice {
            raw: echo(),
            reason: "paise overflow",
        })
}

/// Bounds a raw input echoed into a typed error (char-boundary safe).
fn bounded_echo(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= ERROR_ECHO_MAX_CHARS {
        trimmed.to_owned()
    } else {
        trimmed.chars().take(ERROR_ECHO_MAX_CHARS).collect()
    }
}

// ---------------------------------------------------------------------------
// Validation (pure, fail-closed)
// ---------------------------------------------------------------------------

/// Typed, total validation errors — every reject names WHY.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SmartOrderValidationError {
    /// `reference_id` violates the doc contract (8-20 chars, `[A-Za-z0-9-]`,
    /// ≤2 hyphens, alphanumeric content).
    #[error("invalid reference_id: {reason}")]
    InvalidReferenceId {
        /// Which constraint failed.
        reason: &'static str,
    },
    /// OCO is exit-only F&O today — CASH rejected fail-closed (doc 18 §8.1).
    #[error("unsupported OCO segment `{segment}` — FNO only")]
    UnsupportedOcoSegment {
        /// The offending (bounded) segment string.
        segment: String,
    },
    /// Quantity must be strictly positive.
    #[error("quantity must be > 0 (got {quantity})")]
    NonPositiveQuantity {
        /// The offending quantity.
        quantity: i64,
    },
    /// OCO quantity may not exceed `abs(net_position_quantity)` (doc 18 §2).
    #[error("quantity {quantity} exceeds abs(net_position_quantity) {abs_net_position}")]
    QuantityExceedsPosition {
        /// The requested OCO quantity.
        quantity: i64,
        /// The absolute net position.
        abs_net_position: i64,
    },
    /// Lot size must be strictly positive (caller-supplied).
    #[error("lot_size must be > 0 (got {lot_size})")]
    NonPositiveLotSize {
        /// The offending lot size.
        lot_size: i64,
    },
    /// FNO quantity must be a lot-size multiple (doc: "must respect lot size").
    #[error("quantity {quantity} is not a multiple of lot_size {lot_size}")]
    QuantityNotLotMultiple {
        /// The requested quantity.
        quantity: i64,
        /// The instrument lot size.
        lot_size: i64,
    },
    /// Target leg order type must be `LIMIT` or `MARKET` (doc 18 §2).
    #[error("target leg order_type must be LIMIT or MARKET")]
    TargetOrderTypeInvalid,
    /// Stop-loss leg order type must be `SL` or `SL_M` (doc 18 §2).
    #[error("stop_loss leg order_type must be SL or SL_M")]
    StopLossOrderTypeInvalid,
    /// A leg/order price is required for this order type but absent —
    /// or present where it must be null.
    #[error("price presence wrong for {leg} ({reason})")]
    PricePresenceInvalid {
        /// Which leg/order.
        leg: &'static str,
        /// Which direction the presence rule failed.
        reason: &'static str,
    },
    /// A decimal price string failed to parse (bounded echo).
    #[error("invalid decimal price `{raw}`: {reason}")]
    InvalidPrice {
        /// Bounded echo of the offending input.
        raw: String,
        /// Which parse rule failed.
        reason: &'static str,
    },
    /// OCO trigger geometry inverted for the exit direction — OUR OWN
    /// fail-closed guard, not doc-mandated (see
    /// [`validate_oco_trigger_geometry`]).
    #[error(
        "OCO trigger geometry invalid for {transaction:?}: target {target_trigger_paise} vs stop-loss {sl_trigger_paise} paise"
    )]
    TriggerGeometryInvalid {
        /// The exit direction.
        transaction: TransactionType,
        /// Target trigger, integer paise.
        target_trigger_paise: i64,
        /// Stop-loss trigger, integer paise.
        sl_trigger_paise: i64,
    },
    /// A modify body with ZERO modifiable fields is a no-op — rejected.
    #[error("modify request carries no modifiable field")]
    EmptyModify,
    /// A path component (segment / smart_order_type / smart_order_id)
    /// failed the fail-closed charset check (URL-path injection defense).
    #[error("invalid {component}: {reason}")]
    InvalidPathComponent {
        /// Which path component.
        component: &'static str,
        /// Which constraint failed.
        reason: &'static str,
    },
    /// A list-pagination bound was violated (doc 18 §5 min/max).
    #[error("list pagination out of bounds: {reason}")]
    ListPaginationOutOfBounds {
        /// Which bound failed.
        reason: &'static str,
    },
}

/// Validates the `reference_id` idempotency key: 8-20 chars, charset
/// `[A-Za-z0-9-]`, at most two hyphens, and at least one alphanumeric
/// character ("alphanumeric string … with at most two hyphens" — doc 18 §2).
pub fn validate_reference_id(reference_id: &str) -> Result<(), SmartOrderValidationError> {
    let len = reference_id.chars().count();
    if !(REFERENCE_ID_MIN_LEN..=REFERENCE_ID_MAX_LEN).contains(&len) {
        return Err(SmartOrderValidationError::InvalidReferenceId {
            reason: "length must be 8-20 characters",
        });
    }
    if !reference_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(SmartOrderValidationError::InvalidReferenceId {
            reason: "only [A-Za-z0-9-] allowed",
        });
    }
    if reference_id.chars().filter(|c| *c == '-').count() > REFERENCE_ID_MAX_HYPHENS {
        return Err(SmartOrderValidationError::InvalidReferenceId {
            reason: "at most two hyphens allowed",
        });
    }
    if !reference_id.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(SmartOrderValidationError::InvalidReferenceId {
            reason: "must contain alphanumeric content",
        });
    }
    Ok(())
}

/// OUR OWN fail-closed sanity guard on OCO trigger geometry — NOT
/// doc-mandated (the doc says only "leg directions are derived from your
/// net position"). For a `SELL` exit of a LONG position the take-profit
/// trigger must sit ABOVE the stop-loss trigger; for a `BUY` exit of a
/// SHORT position, BELOW. Equal triggers are always invalid (the two legs
/// would race).
pub fn validate_oco_trigger_geometry(
    transaction_type: TransactionType,
    target_trigger_paise: i64,
    sl_trigger_paise: i64,
) -> Result<(), SmartOrderValidationError> {
    let ok = match transaction_type {
        TransactionType::Sell => target_trigger_paise > sl_trigger_paise,
        TransactionType::Buy => target_trigger_paise < sl_trigger_paise,
    };
    if ok {
        Ok(())
    } else {
        Err(SmartOrderValidationError::TriggerGeometryInvalid {
            transaction: transaction_type,
            target_trigger_paise,
            sl_trigger_paise,
        })
    }
}

/// Full fail-closed validation of an OCO create body (doc 18 §2 rules +
/// our own geometry guard). `lot_size` is CALLER-SUPPLIED (from the
/// instrument master) and must be > 0.
pub fn validate_oco_create(
    req: &OcoCreateRequest,
    lot_size: i64,
) -> Result<(), SmartOrderValidationError> {
    validate_reference_id(&req.reference_id)?;
    // OCO is exit-only F&O: the CASH arm is intra-doc CONTRADICTED
    // (doc 18 §8.1 — header ban vs "CASH OCO only supports MIS"), so we
    // fail closed on FNO-only until probe P9 resolves it live.
    if req.segment != OCO_SEGMENT_FNO {
        return Err(SmartOrderValidationError::UnsupportedOcoSegment {
            segment: bounded_echo(&req.segment),
        });
    }
    if req.quantity <= 0 {
        return Err(SmartOrderValidationError::NonPositiveQuantity {
            quantity: req.quantity,
        });
    }
    let abs_net_position = req.net_position_quantity.unsigned_abs();
    if req.quantity.unsigned_abs() > abs_net_position {
        return Err(SmartOrderValidationError::QuantityExceedsPosition {
            quantity: req.quantity,
            abs_net_position: i64::try_from(abs_net_position).unwrap_or(i64::MAX),
        });
    }
    if lot_size <= 0 {
        return Err(SmartOrderValidationError::NonPositiveLotSize { lot_size });
    }
    if req.quantity % lot_size != 0 {
        return Err(SmartOrderValidationError::QuantityNotLotMultiple {
            quantity: req.quantity,
            lot_size,
        });
    }
    // Target leg: LIMIT/MARKET; price present iff LIMIT (MARKET-with-price
    // is OUR fail-closed reading — the doc marks price "required if LIMIT"
    // and its own MARKET-class examples carry null).
    match req.target.order_type {
        SmartLegOrderType::Limit => {
            let price = req.target.price.as_deref().ok_or(
                SmartOrderValidationError::PricePresenceInvalid {
                    leg: "target",
                    reason: "price required for LIMIT",
                },
            )?;
            decimal_string_to_paise(price)?;
        }
        SmartLegOrderType::Market => {
            if req.target.price.is_some() {
                return Err(SmartOrderValidationError::PricePresenceInvalid {
                    leg: "target",
                    reason: "price must be absent for MARKET",
                });
            }
        }
        SmartLegOrderType::Sl | SmartLegOrderType::SlM => {
            return Err(SmartOrderValidationError::TargetOrderTypeInvalid);
        }
    }
    // Stop-loss leg: SL (price required) / SL_M (price MUST be None —
    // the doc example carries an explicit null).
    match req.stop_loss.order_type {
        SmartLegOrderType::Sl => {
            let price = req.stop_loss.price.as_deref().ok_or(
                SmartOrderValidationError::PricePresenceInvalid {
                    leg: "stop_loss",
                    reason: "price required for SL",
                },
            )?;
            decimal_string_to_paise(price)?;
        }
        SmartLegOrderType::SlM => {
            if req.stop_loss.price.is_some() {
                return Err(SmartOrderValidationError::PricePresenceInvalid {
                    leg: "stop_loss",
                    reason: "price must be absent for SL_M",
                });
            }
        }
        SmartLegOrderType::Limit | SmartLegOrderType::Market => {
            return Err(SmartOrderValidationError::StopLossOrderTypeInvalid);
        }
    }
    // Trigger prices parse + geometry sanity (our own guard).
    let target_trigger = decimal_string_to_paise(&req.target.trigger_price)?;
    let sl_trigger = decimal_string_to_paise(&req.stop_loss.trigger_price)?;
    validate_oco_trigger_geometry(req.transaction_type, target_trigger, sl_trigger)
}

/// Fail-closed validation of a GTT create body: reference_id, positive
/// quantity + lot-multiple (pass `lot_size = 1` for CASH), parseable
/// trigger price, and the embedded order's price-presence rule
/// (`LIMIT`/`SL` require a price; `MARKET`/`SL_M` must carry none).
pub fn validate_gtt_create(
    req: &GttCreateRequest,
    lot_size: i64,
) -> Result<(), SmartOrderValidationError> {
    validate_reference_id(&req.reference_id)?;
    if req.quantity <= 0 {
        return Err(SmartOrderValidationError::NonPositiveQuantity {
            quantity: req.quantity,
        });
    }
    if lot_size <= 0 {
        return Err(SmartOrderValidationError::NonPositiveLotSize { lot_size });
    }
    if req.quantity % lot_size != 0 {
        return Err(SmartOrderValidationError::QuantityNotLotMultiple {
            quantity: req.quantity,
            lot_size,
        });
    }
    decimal_string_to_paise(&req.trigger_price)?;
    match req.order.order_type {
        SmartLegOrderType::Limit | SmartLegOrderType::Sl => {
            let price = req.order.price.as_deref().ok_or(
                SmartOrderValidationError::PricePresenceInvalid {
                    leg: "order",
                    reason: "price required for LIMIT/SL",
                },
            )?;
            decimal_string_to_paise(price)?;
        }
        SmartLegOrderType::Market | SmartLegOrderType::SlM => {
            if req.order.price.is_some() {
                return Err(SmartOrderValidationError::PricePresenceInvalid {
                    leg: "order",
                    reason: "price must be absent for MARKET/SL_M",
                });
            }
        }
    }
    Ok(())
}

/// Validates a modify body: at least one modifiable field present, and any
/// supplied trigger price / quantity parses/passes its own rule.
pub fn validate_modify(req: &SmartOrderModifyRequest) -> Result<(), SmartOrderValidationError> {
    if req.quantity.is_none()
        && req.duration.is_none()
        && req.product_type.is_none()
        && req.target.is_none()
        && req.stop_loss.is_none()
    {
        return Err(SmartOrderValidationError::EmptyModify);
    }
    if let Some(q) = req.quantity
        && q <= 0
    {
        return Err(SmartOrderValidationError::NonPositiveQuantity { quantity: q });
    }
    if let Some(leg) = &req.target {
        decimal_string_to_paise(&leg.trigger_price)?;
    }
    if let Some(leg) = &req.stop_loss {
        decimal_string_to_paise(&leg.trigger_price)?;
    }
    Ok(())
}

/// URL-path-injection defense: a path component (segment / type / id) must
/// be non-empty ASCII `[A-Za-z0-9_-]` (covers `gtt_91a7f4` / `oco_a12bc3` /
/// `FNO` / `OCO`), bounded ≤ 64 chars.
fn validate_path_component(
    component: &'static str,
    value: &str,
) -> Result<(), SmartOrderValidationError> {
    if value.is_empty() || value.len() > 64 {
        return Err(SmartOrderValidationError::InvalidPathComponent {
            component,
            reason: "must be 1-64 characters",
        });
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(SmartOrderValidationError::InvalidPathComponent {
            component,
            reason: "only [A-Za-z0-9_-] allowed",
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// List query
// ---------------------------------------------------------------------------

/// Optional filters for `GET /v1/order-advance/list` (doc 18 §5). Every
/// field absent = the broker's documented default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartOrderListQuery {
    /// `FNO` / `CASH`.
    pub segment: Option<String>,
    /// `OCO` / `GTT` (broker default: `OCO`).
    pub smart_order_type: Option<String>,
    /// e.g. `ACTIVE` / `CANCELLED` / `COMPLETED` (broker default: `ACTIVE`).
    pub status: Option<String>,
    /// Page, 0-based (doc: min 0, max 500).
    pub page: Option<u32>,
    /// Page size (doc: min 1, max 50; default 10).
    pub page_size: Option<u32>,
    /// Inclusive ISO8601 `YYYY-MM-DDThh:mm:ss` window start.
    pub start_date_time: Option<String>,
    /// Inclusive window end (window must not exceed one month — broker-side
    /// validation; we pass it through).
    pub end_date_time: Option<String>,
}

impl SmartOrderListQuery {
    /// Validates the documented pagination bounds (fail-closed before any
    /// request), then renders the present filters as query pairs.
    pub fn to_query_pairs(&self) -> Result<Vec<(&'static str, String)>, SmartOrderValidationError> {
        if let Some(page) = self.page
            && page > LIST_PAGE_MAX
        {
            return Err(SmartOrderValidationError::ListPaginationOutOfBounds {
                reason: "page must be 0-500",
            });
        }
        if let Some(size) = self.page_size
            && !(LIST_PAGE_SIZE_MIN..=LIST_PAGE_SIZE_MAX).contains(&size)
        {
            return Err(SmartOrderValidationError::ListPaginationOutOfBounds {
                reason: "page_size must be 1-50",
            });
        }
        let mut pairs = Vec::with_capacity(7);
        if let Some(v) = &self.segment {
            pairs.push(("segment", v.clone()));
        }
        if let Some(v) = &self.smart_order_type {
            pairs.push(("smart_order_type", v.clone()));
        }
        if let Some(v) = &self.status {
            pairs.push(("status", v.clone()));
        }
        if let Some(v) = self.page {
            pairs.push(("page", v.to_string()));
        }
        if let Some(v) = self.page_size {
            pairs.push(("page_size", v.to_string()));
        }
        if let Some(v) = &self.start_date_time {
            pairs.push(("start_date_time", v.clone()));
        }
        if let Some(v) = &self.end_date_time {
            pairs.push(("end_date_time", v.clone()));
        }
        Ok(pairs)
    }
}

// ---------------------------------------------------------------------------
// Outcomes + errors (typed — no bools)
// ---------------------------------------------------------------------------

/// Typed outcome of a WRITE method (create/modify/cancel).
#[derive(Debug, Clone, PartialEq)]
pub enum SmartOrderOutcome {
    /// `settings.write_enabled == false` (Gate 1) — nothing validated,
    /// nothing serialized, nothing sent.
    DisabledByConfig,
    /// [`GROWW_ORDER_LIVE_FIRE`]` == false` (Gate 3) — the request was
    /// FULLY validated + serialized, and this carries exactly what WOULD
    /// have been sent. No HTTP was constructed.
    DryRun {
        /// The absolute endpoint URL.
        endpoint: String,
        /// The HTTP method (`POST`/`PUT`).
        method: &'static str,
        /// The serialized JSON body (empty string for the body-less cancel).
        body_json: String,
    },
    /// LIVE path only (unreachable until the dated Gate-3 flip): the broker
    /// accepted the request and returned this payload (boxed — the payload
    /// is large relative to the other variants; cold path, so the one
    /// allocation is fine).
    Accepted(Box<SmartOrderPayload>),
}

/// Typed outcome of a READ method (status get / list).
#[derive(Debug, Clone, PartialEq)]
pub enum SmartOrderReadOutcome<T> {
    /// `settings.read_enabled == false` — nothing sent.
    DisabledByConfig,
    /// The broker returned this payload.
    Fetched(T),
}

/// Internal write-op classifier for error-code selection (create/cancel →
/// `GROWW-OCO-01`; modify → `GROWW-OCO-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOp {
    Create,
    Modify,
    Cancel,
}

impl WriteOp {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Modify => "modify",
            Self::Cancel => "cancel",
        }
    }
}

/// Typed client errors. Every embedded broker/transport string is bounded
/// + secret-redacted via [`capture_rest_error_body`] at capture time; the
/// bearer token can never appear (it travels only in the header).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SmartOrderError {
    /// Fail-closed validation reject (nothing sent).
    #[error("validation: {0}")]
    Validation(#[from] SmartOrderValidationError),
    /// The token provider returned no token (nothing sent; the shared
    /// minter has not populated SSM — never minted here).
    #[error("no Groww access token available (read-only provider returned none)")]
    NoToken,
    /// Serialization of the typed body failed (should be unreachable for
    /// these plain structs; typed for totality).
    #[error("serialize: {0}")]
    Serialize(String),
    /// Transport-class failure after the single bounded retry (bounded +
    /// redacted message).
    #[error("transport: {0}")]
    Transport(String),
    /// HTTP 429 — typed, counted (`tv_groww_smart_order_rate_limited_total`),
    /// NEVER retried here (pacing belongs to the caller/poller).
    #[error("rate limited (HTTP 429, retry-after header present: {retry_after_present})")]
    RateLimited {
        /// Whether the broker sent a `retry-after` header.
        retry_after_present: bool,
    },
    /// The GA FAILURE envelope (any HTTP status incl. 2xx — the G1 lesson:
    /// a 2xx body can carry `status: FAILURE`).
    #[error("groww GA failure (http {http_status}) {code}: {message}")]
    GaFailure {
        /// The real HTTP status the envelope rode on.
        http_status: u16,
        /// GA wire code (`GA000`..`GA007`; empty when absent).
        code: String,
        /// Bounded + redacted broker message.
        message: String,
    },
    /// Non-2xx without a parseable GA envelope (bounded + redacted body).
    #[error("http {status}: {body}")]
    HttpStatus {
        /// The HTTP status.
        status: u16,
        /// Bounded + redacted body capture.
        body: String,
    },
    /// A 2xx body that did not parse into the expected envelope/payload.
    #[error("parse: {0}")]
    Parse(String),
}

// ---------------------------------------------------------------------------
// Pure decision helpers for the FUTURE reconcile orchestrator (§39.3 —
// the GROWW-OCO-02 / GROWW-OCO-03 emit sites land with that PR; only the
// verdict types + total functions ship here)
// ---------------------------------------------------------------------------

/// Verdict on the one-cancels-other invariant after leg execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingVerdict {
    /// Invariant holds: nothing executed yet, or the sibling is verified
    /// cancelled / terminally unable to execute.
    Ok,
    /// One leg executed; the sibling is not yet terminal but the deadline
    /// has not elapsed — keep polling.
    AwaitingCancel,
    /// The invariant is UNVERIFIED-BROKEN: both legs executed, or the
    /// sibling is still live past the deadline (the GROWW-OCO-02
    /// condition — potential double exposure).
    Violation,
}

/// Total, no-panic sibling-cancel verdict ("If a leg executes, the other
/// cancels automatically" — doc 18 §8.2; the cancel LOCUS and partial-fill
/// behavior are UNDOCUMENTED, so a `PartiallyFilled` leg is treated as
/// executed-class FAIL-CLOSED — it starts the deadline clock rather than
/// being ignored).
#[must_use]
// WIRING-EXEMPT: §39.3 area PR — the reconcile-orchestrator consumer (the
// GROWW-OCO-02 emit site) lands in its own later PR; tested inline.
pub fn sibling_cancel_verdict(
    leg_a: &BrokerOrderStatus,
    leg_b: &BrokerOrderStatus,
    secs_since_trigger: u64,
    deadline_secs: u64,
) -> SiblingVerdict {
    let executed = |s: &BrokerOrderStatus| {
        matches!(
            s,
            BrokerOrderStatus::Filled | BrokerOrderStatus::PartiallyFilled
        )
    };
    match (executed(leg_a), executed(leg_b)) {
        // Both legs executed — the OCO invariant is broken outright,
        // regardless of any deadline.
        (true, true) => SiblingVerdict::Violation,
        // Nothing executed yet — nothing to verify.
        (false, false) => SiblingVerdict::Ok,
        (a_exec, _) => {
            let sibling = if a_exec { leg_b } else { leg_a };
            if matches!(sibling, BrokerOrderStatus::Cancelled) {
                // The documented auto-cancel landed — verified.
                SiblingVerdict::Ok
            } else if sibling.is_terminal() {
                // Rejected / Failed / Expired: the sibling can no longer
                // execute, so no double-exposure risk remains.
                SiblingVerdict::Ok
            } else if secs_since_trigger <= deadline_secs {
                SiblingVerdict::AwaitingCancel
            } else {
                SiblingVerdict::Violation
            }
        }
    }
}

/// Verdict of the OCO-quantity-vs-net-position reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileVerdict {
    /// The protective OCO exactly covers the net position.
    Consistent,
    /// The OCO would exit MORE than the position holds (over-exit risk —
    /// the GROWW-OCO-03 condition).
    OcoExceedsPosition {
        /// The active OCO quantity.
        oco_quantity: i64,
        /// The absolute net position.
        abs_net_position: i64,
    },
    /// Part of the position is UNPROTECTED by the OCO (under-coverage —
    /// also a GROWW-OCO-03 condition; the operator judges).
    PositionExceedsOco {
        /// The active OCO quantity.
        oco_quantity: i64,
        /// The absolute net position.
        abs_net_position: i64,
    },
}

/// Total OCO-vs-position quantity reconcile (pure comparison — the
/// GROWW-OCO-03 emit site lands with the later orchestrator PR).
#[must_use]
// WIRING-EXEMPT: §39.3 area PR — the reconcile-orchestrator consumer (the
// GROWW-OCO-03 emit site) lands in its own later PR; tested inline.
pub fn reconcile_verdict(oco_quantity: i64, abs_net_position: i64) -> ReconcileVerdict {
    match oco_quantity.cmp(&abs_net_position) {
        std::cmp::Ordering::Equal => ReconcileVerdict::Consistent,
        std::cmp::Ordering::Greater => ReconcileVerdict::OcoExceedsPosition {
            oco_quantity,
            abs_net_position,
        },
        std::cmp::Ordering::Less => ReconcileVerdict::PositionExceedsOco {
            oco_quantity,
            abs_net_position,
        },
    }
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// READ-ONLY access-token provider — returns the current shared-minter
/// token (or `None` when SSM has not been populated). NEVER mints
/// (`groww-shared-token-minter-2026-07-02.md`).
pub type GrowwAccessTokenProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The Groww Smart Orders (GTT/OCO) REST client — every write hard-gated
/// behind the §39.2 live-fire lattice (see the module doc).
pub struct GrowwSmartOrderClient {
    http: reqwest::Client,
    base_url: String,
    token_provider: GrowwAccessTokenProvider,
    settings: GrowwSmartOrderSettings,
}

impl fmt::Debug for GrowwSmartOrderClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrowwSmartOrderClient")
            .field("base_url", &self.base_url)
            .field("settings", &self.settings)
            .field("token_provider", &"<redacted fn>")
            .finish()
    }
}

impl GrowwSmartOrderClient {
    /// Builds the client. `base_url_override` exists for tests (an
    /// unroutable localhost proves no accidental send); production passes
    /// `None` → [`GROWW_SMART_ORDER_BASE_URL`]. Trailing slashes on the
    /// override are trimmed so path joins stay canonical.
    #[must_use]
    // WIRING-EXEMPT: §39.3 area PR — the boot wiring that constructs this
    // client lands with the later orchestrator PR; tested inline.
    pub fn new(
        http: reqwest::Client,
        base_url_override: Option<String>,
        token_provider: GrowwAccessTokenProvider,
        settings: GrowwSmartOrderSettings,
    ) -> Self {
        let base_url = base_url_override
            .map(|u| u.trim_end_matches('/').to_owned())
            .unwrap_or_else(|| GROWW_SMART_ORDER_BASE_URL.to_owned());
        Self {
            http,
            base_url,
            token_provider,
            settings,
        }
    }

    /// The effective settings (read-only view).
    #[must_use]
    // WIRING-EXEMPT: §39.3 area PR — orchestrator introspection; tested inline.
    pub fn settings(&self) -> &GrowwSmartOrderSettings {
        &self.settings
    }

    // -- WRITE methods (Gate 1 → validate/serialize → Gate 3 → HTTP) -------

    /// Creates an OCO smart order. Gate order per the module doc: config
    /// gate → full validation + serialization → dry-run gate → (live only)
    /// HTTP. `lot_size` comes from the instrument master.
    pub async fn create_oco(
        &self,
        req: &OcoCreateRequest,
        lot_size: i64,
    ) -> Result<SmartOrderOutcome, SmartOrderError> {
        if !self.settings.write_enabled {
            return Ok(SmartOrderOutcome::DisabledByConfig);
        }
        validate_oco_create(req, lot_size)?;
        let body_json =
            serde_json::to_string(req).map_err(|e| SmartOrderError::Serialize(e.to_string()))?;
        let endpoint = format!("{}{}", self.base_url, SMART_ORDER_CREATE_PATH);
        if !GROWW_ORDER_LIVE_FIRE {
            return Ok(SmartOrderOutcome::DryRun {
                endpoint,
                method: "POST",
                body_json,
            });
        }
        // LIVE path — statically after BOTH gates; unreachable until the
        // dated Gate-3 flip.
        self.execute_write("POST", &endpoint, Some(body_json), WriteOp::Create)
            .await
    }

    /// Creates a GTT smart order (same gate order as [`Self::create_oco`]).
    /// Pass `lot_size = 1` for CASH instruments.
    pub async fn create_gtt(
        &self,
        req: &GttCreateRequest,
        lot_size: i64,
    ) -> Result<SmartOrderOutcome, SmartOrderError> {
        if !self.settings.write_enabled {
            return Ok(SmartOrderOutcome::DisabledByConfig);
        }
        validate_gtt_create(req, lot_size)?;
        let body_json =
            serde_json::to_string(req).map_err(|e| SmartOrderError::Serialize(e.to_string()))?;
        let endpoint = format!("{}{}", self.base_url, SMART_ORDER_CREATE_PATH);
        if !GROWW_ORDER_LIVE_FIRE {
            return Ok(SmartOrderOutcome::DryRun {
                endpoint,
                method: "POST",
                body_json,
            });
        }
        self.execute_write("POST", &endpoint, Some(body_json), WriteOp::Create)
            .await
    }

    /// Modifies a resting smart order (`PUT .../modify/{smart_order_id}`;
    /// only the doc-modifiable fields — see [`SmartOrderModifyRequest`]).
    pub async fn modify_smart_order(
        &self,
        smart_order_id: &str,
        req: &SmartOrderModifyRequest,
    ) -> Result<SmartOrderOutcome, SmartOrderError> {
        if !self.settings.write_enabled {
            return Ok(SmartOrderOutcome::DisabledByConfig);
        }
        validate_path_component("smart_order_id", smart_order_id)?;
        validate_modify(req)?;
        let body_json =
            serde_json::to_string(req).map_err(|e| SmartOrderError::Serialize(e.to_string()))?;
        let endpoint = format!(
            "{}{}/{}",
            self.base_url, SMART_ORDER_MODIFY_PATH_PREFIX, smart_order_id
        );
        if !GROWW_ORDER_LIVE_FIRE {
            return Ok(SmartOrderOutcome::DryRun {
                endpoint,
                method: "PUT",
                body_json,
            });
        }
        self.execute_write("PUT", &endpoint, Some(body_json), WriteOp::Modify)
            .await
    }

    /// Cancels a smart order (`POST .../cancel/{segment}/{type}/{id}` —
    /// path params only, no body; the dry-run `body_json` is empty).
    pub async fn cancel_smart_order(
        &self,
        segment: &str,
        smart_order_type: &str,
        smart_order_id: &str,
    ) -> Result<SmartOrderOutcome, SmartOrderError> {
        if !self.settings.write_enabled {
            return Ok(SmartOrderOutcome::DisabledByConfig);
        }
        validate_path_component("segment", segment)?;
        validate_path_component("smart_order_type", smart_order_type)?;
        validate_path_component("smart_order_id", smart_order_id)?;
        let endpoint = format!(
            "{}{}/{}/{}/{}",
            self.base_url,
            SMART_ORDER_CANCEL_PATH_PREFIX,
            segment,
            smart_order_type,
            smart_order_id
        );
        if !GROWW_ORDER_LIVE_FIRE {
            return Ok(SmartOrderOutcome::DryRun {
                endpoint,
                method: "POST",
                body_json: String::new(),
            });
        }
        self.execute_write("POST", &endpoint, None, WriteOp::Cancel)
            .await
    }

    // -- READ methods (gate on read_enabled only) ---------------------------

    /// Fetches one smart order's full object
    /// (`GET .../status/{segment}/{type}/internal/{id}` — note the literal
    /// `internal` path segment, doc 18 §4).
    pub async fn get_smart_order(
        &self,
        segment: &str,
        smart_order_type: &str,
        smart_order_id: &str,
    ) -> Result<SmartOrderReadOutcome<SmartOrderPayload>, SmartOrderError> {
        if !self.settings.read_enabled {
            return Ok(SmartOrderReadOutcome::DisabledByConfig);
        }
        validate_path_component("segment", segment)?;
        validate_path_component("smart_order_type", smart_order_type)?;
        validate_path_component("smart_order_id", smart_order_id)?;
        let endpoint = format!(
            "{}{}/{}/{}/internal/{}",
            self.base_url,
            SMART_ORDER_STATUS_PATH_PREFIX,
            segment,
            smart_order_type,
            smart_order_id
        );
        match self
            .fetch_payload::<SmartOrderPayload>(&endpoint, &[])
            .await
        {
            Ok(payload) => Ok(SmartOrderReadOutcome::Fetched(payload)),
            Err(err) => {
                // GROWW-OCO-05: the reconcile poller's snapshot for this
                // cycle is missing; the next poll re-attempts.
                error!(
                    code = ErrorCode::GrowwOco05PollerDegraded.code_str(),
                    op = "status_get",
                    error = %err,
                    "groww smart-order status poll degraded"
                );
                Err(err)
            }
        }
    }

    /// Lists smart orders with optional filters (doc 18 §5).
    pub async fn list_smart_orders(
        &self,
        query: &SmartOrderListQuery,
    ) -> Result<SmartOrderReadOutcome<SmartOrderListPayload>, SmartOrderError> {
        if !self.settings.read_enabled {
            return Ok(SmartOrderReadOutcome::DisabledByConfig);
        }
        let pairs = query.to_query_pairs()?;
        let endpoint = format!("{}{}", self.base_url, SMART_ORDER_LIST_PATH);
        match self
            .fetch_payload::<SmartOrderListPayload>(&endpoint, &pairs)
            .await
        {
            Ok(payload) => Ok(SmartOrderReadOutcome::Fetched(payload)),
            Err(err) => {
                error!(
                    code = ErrorCode::GrowwOco05PollerDegraded.code_str(),
                    op = "list",
                    error = %err,
                    "groww smart-order list poll degraded"
                );
                Err(err)
            }
        }
    }

    // -- Internals -----------------------------------------------------------

    /// LIVE write leg (unreachable while Gate 3 is closed — every caller
    /// returns [`SmartOrderOutcome::DryRun`] first). On failure emits the
    /// op-appropriate coded error: create/cancel → `GROWW-OCO-01`
    /// (placement-class), modify → `GROWW-OCO-04`.
    async fn execute_write(
        &self,
        method: &'static str,
        endpoint: &str,
        body_json: Option<String>,
        op: WriteOp,
    ) -> Result<SmartOrderOutcome, SmartOrderError> {
        match self
            .send_and_parse::<SmartOrderPayload>(method, endpoint, &[], body_json.as_deref())
            .await
        {
            Ok(payload) => Ok(SmartOrderOutcome::Accepted(Box::new(payload))),
            Err(err) => {
                match op {
                    WriteOp::Create | WriteOp::Cancel => error!(
                        code = ErrorCode::GrowwOco01PlacementFailed.code_str(),
                        op = op.as_str(),
                        error = %err,
                        "groww smart-order write failed"
                    ),
                    WriteOp::Modify => error!(
                        code = ErrorCode::GrowwOco04ModifyRejected.code_str(),
                        op = op.as_str(),
                        error = %err,
                        "groww smart-order modify rejected"
                    ),
                }
                Err(err)
            }
        }
    }

    /// Read-path GET → parsed payload.
    async fn fetch_payload<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        query: &[(&'static str, String)],
    ) -> Result<T, SmartOrderError> {
        let (status, body) = self.send_request("GET", endpoint, query, None).await?;
        parse_success_envelope::<T>(status, &body)
    }

    /// Write-path send → parsed payload.
    async fn send_and_parse<T: DeserializeOwned>(
        &self,
        method: &'static str,
        endpoint: &str,
        query: &[(&'static str, String)],
        body_json: Option<&str>,
    ) -> Result<T, SmartOrderError> {
        let (status, body) = self
            .send_request(method, endpoint, query, body_json)
            .await?;
        parse_success_envelope::<T>(status, &body)
    }

    /// One bounded HTTP round-trip: bearer + `x-api-version: 1.0` +
    /// `Accept: application/json` headers (the `groww_spot_1m_boot` shape),
    /// 5s per-request timeout, ONE bounded retry on TRANSPORT error only
    /// (never on any HTTP status), 429 → typed + counted. Returns
    /// `(status, raw body)` for every non-429 HTTP response.
    async fn send_request(
        &self,
        method: &'static str,
        url: &str,
        query: &[(&'static str, String)],
        body_json: Option<&str>,
    ) -> Result<(u16, String), SmartOrderError> {
        let token = (self.token_provider)().ok_or(SmartOrderError::NoToken)?;
        let mut attempt: u8 = 0;
        loop {
            attempt += 1;
            let mut builder = match method {
                "POST" => self.http.post(url),
                "PUT" => self.http.put(url),
                _ => self.http.get(url),
            };
            builder = builder
                .timeout(Duration::from_secs(SMART_ORDER_REQUEST_TIMEOUT_SECS))
                .bearer_auth(&token)
                .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
                .header(reqwest::header::ACCEPT, "application/json");
            if !query.is_empty() {
                builder = builder.query(query);
            }
            if let Some(body) = body_json {
                builder = builder
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.to_owned());
            }
            match builder.send().await {
                Err(_transport) if attempt == 1 => {
                    // ONE bounded retry on transport error only — a 4xx/5xx
                    // never reaches this arm (it is an Ok(response)).
                    // (The first attempt's error is intentionally unlogged —
                    // the retry either succeeds silently or the second
                    // failure below carries the bounded + redacted capture.)
                    tokio::time::sleep(Duration::from_millis(SMART_ORDER_TRANSPORT_RETRY_DELAY_MS))
                        .await;
                    continue;
                }
                Err(e) => {
                    return Err(SmartOrderError::Transport(capture_rest_error_body(
                        &e.to_string(),
                    )));
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let retry_after_present = resp.headers().contains_key("retry-after");
                        metrics::counter!("tv_groww_smart_order_rate_limited_total").increment(1);
                        return Err(SmartOrderError::RateLimited {
                            retry_after_present,
                        });
                    }
                    let status_u16 = status.as_u16();
                    let body = resp.text().await.map_err(|e| {
                        SmartOrderError::Transport(capture_rest_error_body(&e.to_string()))
                    })?;
                    return Ok((status_u16, body));
                }
            }
        }
    }
}

/// Classifies + parses a `(status, body)` pair into the typed payload:
/// - non-2xx with a parseable GA envelope → [`SmartOrderError::GaFailure`];
/// - non-2xx otherwise → [`SmartOrderError::HttpStatus`] (bounded body);
/// - 2xx carrying `status: FAILURE` / an `error` object → `GaFailure`
///   (the G1 lesson: a 2xx can carry the FAILURE envelope);
/// - 2xx SUCCESS without a payload → [`SmartOrderError::Parse`].
fn parse_success_envelope<T: DeserializeOwned>(
    status: u16,
    body: &str,
) -> Result<T, SmartOrderError> {
    if !(200..300).contains(&status) {
        if let Ok(envelope) = serde_json::from_str::<GaEnvelope<serde_json::Value>>(body)
            && let Some(ga) = envelope.error
        {
            return Err(SmartOrderError::GaFailure {
                http_status: status,
                code: bounded_echo(&ga.code),
                message: capture_rest_error_body(&ga.message),
            });
        }
        return Err(SmartOrderError::HttpStatus {
            status,
            body: capture_rest_error_body(body),
        });
    }
    let envelope: GaEnvelope<T> = serde_json::from_str(body)
        .map_err(|e| SmartOrderError::Parse(capture_rest_error_body(&e.to_string())))?;
    if envelope.status.eq_ignore_ascii_case("FAILURE") || envelope.error.is_some() {
        let ga = envelope.error.unwrap_or_default();
        return Err(SmartOrderError::GaFailure {
            http_status: status,
            code: bounded_echo(&ga.code),
            message: capture_rest_error_body(&ga.message),
        });
    }
    envelope
        .payload
        .ok_or_else(|| SmartOrderError::Parse("2xx SUCCESS envelope without payload".to_owned()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Source-scan ratchet: no unwrap/expect in the production region ----

    #[test]
    fn ratchet_no_unwrap_or_expect_in_production_region() {
        let src = include_str!("smart_orders.rs");
        let marker = "#[cfg(test)]";
        let prod = src.split(marker).next().unwrap_or("");
        assert!(
            !prod.contains(".unwrap("),
            "production region of smart_orders.rs must not call unwrap"
        );
        assert!(
            !prod.contains(".expect("),
            "production region of smart_orders.rs must not call expect"
        );
    }

    #[test]
    fn ratchet_reqwest_send_sits_after_both_gates() {
        // Statically: every write method's `send` lives ONLY inside
        // send_request, which is reachable ONLY past the write_enabled
        // check and the GROWW_ORDER_LIVE_FIRE dry-run return. Pin the
        // shape: exactly ONE `.send().await` call site in the whole file,
        // and every write method carries both gate literals.
        let src = include_str!("smart_orders.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        assert_eq!(
            prod.matches(".send().await").count(),
            1,
            "exactly one reqwest send site (inside send_request)"
        );
        assert_eq!(
            prod.matches("if !self.settings.write_enabled").count(),
            4,
            "all four write methods carry the Gate-1 config check"
        );
        assert_eq!(
            prod.matches("if !GROWW_ORDER_LIVE_FIRE").count(),
            4,
            "all four write methods carry the Gate-3 dry-run check"
        );
    }

    // -- Settings ------------------------------------------------------------

    #[test]
    fn test_settings_default_all_off_with_documented_knobs() {
        let s = GrowwSmartOrderSettings::default();
        assert!(!s.read_enabled);
        assert!(!s.write_enabled);
        assert_eq!(s.reconcile_poll_secs, 15);
        assert_eq!(s.sibling_cancel_deadline_secs, 30);
    }

    #[test]
    fn test_settings_from_config_maps_all_four_fields() {
        let cfg = GrowwOrdersConfig {
            smart_orders_read: true,
            smart_orders_write: true,
            oco_reconcile_poll_secs: 7,
            oco_sibling_cancel_deadline_secs: 99,
            ..GrowwOrdersConfig::default()
        };
        let s = GrowwSmartOrderSettings::from(&cfg);
        assert!(s.read_enabled);
        assert!(s.write_enabled);
        assert_eq!(s.reconcile_poll_secs, 7);
        assert_eq!(s.sibling_cancel_deadline_secs, 99);
        // And the config default maps to the settings default.
        assert_eq!(
            GrowwSmartOrderSettings::from(&GrowwOrdersConfig::default()),
            GrowwSmartOrderSettings::default()
        );
    }

    // -- Money discipline -----------------------------------------------------

    #[test]
    fn test_paise_to_decimal_string_exact() {
        assert_eq!(paise_to_decimal_string(1_950_050), "19500.50");
        assert_eq!(paise_to_decimal_string(5), "0.05");
        assert_eq!(paise_to_decimal_string(0), "0.00");
        assert_eq!(paise_to_decimal_string(100), "1.00");
        assert_eq!(paise_to_decimal_string(12_050), "120.50");
        assert_eq!(paise_to_decimal_string(9_500), "95.00");
        // Negative renders with sign (total function).
        assert_eq!(paise_to_decimal_string(-5), "-0.05");
        // i64::MIN must not panic (unsigned_abs path).
        assert_eq!(paise_to_decimal_string(i64::MIN), "-92233720368547758.08");
        assert_eq!(paise_to_decimal_string(i64::MAX), "92233720368547758.07");
    }

    #[test]
    fn test_decimal_string_to_paise_round_trips() {
        for paise in [0_i64, 5, 100, 9_500, 12_050, 1_950_050, i64::MAX] {
            let s = paise_to_decimal_string(paise);
            assert_eq!(
                decimal_string_to_paise(&s),
                Ok(paise),
                "round trip failed for {paise} (string `{s}`)"
            );
        }
        // Single fractional digit means tenths: "3985.5" == 3985.50.
        assert_eq!(decimal_string_to_paise("3985.5"), Ok(398_550));
        assert_eq!(decimal_string_to_paise("3985"), Ok(398_500));
        assert_eq!(decimal_string_to_paise(" 120.50 "), Ok(12_050));
        assert_eq!(decimal_string_to_paise("0.05"), Ok(5));
    }

    #[test]
    fn test_decimal_string_to_paise_rejects_bad_inputs() {
        // >2dp rejected.
        assert!(matches!(
            decimal_string_to_paise("1.234"),
            Err(SmartOrderValidationError::InvalidPrice { .. })
        ));
        // Negative rejected.
        assert!(matches!(
            decimal_string_to_paise("-1.00"),
            Err(SmartOrderValidationError::InvalidPrice { .. })
        ));
        // NaN / inf / exponent / signs rejected by charset.
        for bad in [
            "NaN", "inf", "1e5", "+1.00", "", "  ", ".", ".5", "1.", "1..2", "1,00",
        ] {
            assert!(
                decimal_string_to_paise(bad).is_err(),
                "`{bad}` must be rejected"
            );
        }
        // Overflow via checked math (i64::MAX rupees * 100 overflows).
        assert!(matches!(
            decimal_string_to_paise("92233720368547758080.00"),
            Err(SmartOrderValidationError::InvalidPrice { .. })
        ));
        assert!(matches!(
            decimal_string_to_paise("92233720368547759.00"),
            Err(SmartOrderValidationError::InvalidPrice { .. })
        ));
    }

    // -- reference_id ----------------------------------------------------------

    #[test]
    fn test_validate_reference_id_edges() {
        // Valid: 8-20 chars, ≤2 hyphens.
        assert!(validate_reference_id("sref-unique-456").is_ok());
        assert!(validate_reference_id("abcd1234").is_ok()); // exactly 8
        assert!(validate_reference_id("a1234567890123456789").is_ok()); // exactly 20
        // Length violations.
        assert!(validate_reference_id("abc1234").is_err()); // 7
        assert!(validate_reference_id("a12345678901234567890").is_err()); // 21
        // Charset violations.
        assert!(validate_reference_id("abcd 1234").is_err());
        assert!(validate_reference_id("abcd_1234").is_err());
        assert!(validate_reference_id("abcd!1234").is_err());
        // Hyphen count: 2 ok, 3 rejected.
        assert!(validate_reference_id("ab-cd-1234").is_ok());
        assert!(validate_reference_id("a-b-c-1234").is_err());
    }

    // -- OCO validation ----------------------------------------------------------

    /// The doc 18 §2 body example, verbatim.
    fn doc_oco_request() -> OcoCreateRequest {
        OcoCreateRequest {
            reference_id: "sref-unique-456".to_owned(),
            smart_order_type: SMART_ORDER_TYPE_OCO,
            segment: "FNO".to_owned(),
            trading_symbol: "NIFTY25OCT24000CE".to_owned(),
            quantity: 50,
            net_position_quantity: 50,
            transaction_type: TransactionType::Sell,
            target: OcoLeg {
                trigger_price: "120.50".to_owned(),
                order_type: SmartLegOrderType::Limit,
                price: Some("121.00".to_owned()),
            },
            stop_loss: OcoLeg {
                trigger_price: "95.00".to_owned(),
                order_type: SmartLegOrderType::SlM,
                price: None,
            },
            product_type: "MIS".to_owned(),
            exchange: "NSE".to_owned(),
            duration: "DAY".to_owned(),
        }
    }

    #[test]
    fn test_validate_oco_create_doc_example_passes() {
        assert_eq!(validate_oco_create(&doc_oco_request(), 25), Ok(()));
    }

    #[test]
    fn test_validate_oco_create_rejects_cash_segment() {
        let mut req = doc_oco_request();
        req.segment = "CASH".to_owned();
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::UnsupportedOcoSegment { .. })
        ));
    }

    #[test]
    fn test_validate_oco_create_quantity_rules() {
        let mut req = doc_oco_request();
        req.quantity = 0;
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::NonPositiveQuantity { .. })
        ));
        let mut req = doc_oco_request();
        req.quantity = 75;
        req.net_position_quantity = 50;
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::QuantityExceedsPosition { .. })
        ));
        // Short position: negative net qty, abs() applies.
        let mut req = doc_oco_request();
        req.quantity = 50;
        req.net_position_quantity = -50;
        req.transaction_type = TransactionType::Buy;
        // Buy exit: target below SL.
        req.target.trigger_price = "95.00".to_owned();
        req.target.price = Some("94.50".to_owned());
        req.stop_loss.trigger_price = "120.50".to_owned();
        assert_eq!(validate_oco_create(&req, 25), Ok(()));
        // Lot-size multiple.
        let mut req = doc_oco_request();
        req.quantity = 30;
        req.net_position_quantity = 50;
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::QuantityNotLotMultiple { .. })
        ));
        // Non-positive lot size fail-closed.
        assert!(matches!(
            validate_oco_create(&doc_oco_request(), 0),
            Err(SmartOrderValidationError::NonPositiveLotSize { .. })
        ));
    }

    #[test]
    fn test_validate_oco_create_leg_rules() {
        // LIMIT target without price.
        let mut req = doc_oco_request();
        req.target.price = None;
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::PricePresenceInvalid { leg: "target", .. })
        ));
        // MARKET target with price.
        let mut req = doc_oco_request();
        req.target.order_type = SmartLegOrderType::Market;
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::PricePresenceInvalid { leg: "target", .. })
        ));
        // SL/SL_M as target rejected.
        let mut req = doc_oco_request();
        req.target.order_type = SmartLegOrderType::Sl;
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::TargetOrderTypeInvalid)
        ));
        // SL_M with a price rejected.
        let mut req = doc_oco_request();
        req.stop_loss.price = Some("95.00".to_owned());
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::PricePresenceInvalid {
                leg: "stop_loss",
                ..
            })
        ));
        // SL without a price rejected.
        let mut req = doc_oco_request();
        req.stop_loss.order_type = SmartLegOrderType::Sl;
        req.stop_loss.price = None;
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::PricePresenceInvalid {
                leg: "stop_loss",
                ..
            })
        ));
        // LIMIT/MARKET as stop-loss rejected.
        let mut req = doc_oco_request();
        req.stop_loss.order_type = SmartLegOrderType::Limit;
        req.stop_loss.price = Some("95.00".to_owned());
        assert!(matches!(
            validate_oco_create(&req, 25),
            Err(SmartOrderValidationError::StopLossOrderTypeInvalid)
        ));
    }

    #[test]
    fn test_validate_oco_trigger_geometry_both_sides() {
        // SELL exit of a long: target above SL — ok.
        assert!(validate_oco_trigger_geometry(TransactionType::Sell, 12_050, 9_500).is_ok());
        // Inverted for SELL — rejected.
        assert!(matches!(
            validate_oco_trigger_geometry(TransactionType::Sell, 9_500, 12_050),
            Err(SmartOrderValidationError::TriggerGeometryInvalid { .. })
        ));
        // BUY exit of a short: target below SL — ok.
        assert!(validate_oco_trigger_geometry(TransactionType::Buy, 9_500, 12_050).is_ok());
        // Inverted for BUY — rejected.
        assert!(matches!(
            validate_oco_trigger_geometry(TransactionType::Buy, 12_050, 9_500),
            Err(SmartOrderValidationError::TriggerGeometryInvalid { .. })
        ));
        // Equal triggers always rejected (both directions).
        assert!(validate_oco_trigger_geometry(TransactionType::Sell, 100, 100).is_err());
        assert!(validate_oco_trigger_geometry(TransactionType::Buy, 100, 100).is_err());
    }

    // -- GTT validation ----------------------------------------------------------

    /// The doc's GTT create example (doc 16/18 GTT schema).
    fn doc_gtt_request() -> GttCreateRequest {
        GttCreateRequest {
            reference_id: "sref-unique-123".to_owned(),
            smart_order_type: SMART_ORDER_TYPE_GTT,
            segment: "CASH".to_owned(),
            trading_symbol: "TCS".to_owned(),
            quantity: 10,
            trigger_price: "3985.00".to_owned(),
            trigger_direction: TriggerDirection::Down,
            order: GttEmbeddedOrder {
                order_type: SmartLegOrderType::Limit,
                price: Some("3990.00".to_owned()),
                transaction_type: TransactionType::Buy,
            },
            product_type: "CNC".to_owned(),
            exchange: "NSE".to_owned(),
            duration: "DAY".to_owned(),
        }
    }

    #[test]
    fn test_validate_gtt_create_rules() {
        assert_eq!(validate_gtt_create(&doc_gtt_request(), 1), Ok(()));
        // LIMIT without price.
        let mut req = doc_gtt_request();
        req.order.price = None;
        assert!(matches!(
            validate_gtt_create(&req, 1),
            Err(SmartOrderValidationError::PricePresenceInvalid { leg: "order", .. })
        ));
        // MARKET with price.
        let mut req = doc_gtt_request();
        req.order.order_type = SmartLegOrderType::Market;
        assert!(matches!(
            validate_gtt_create(&req, 1),
            Err(SmartOrderValidationError::PricePresenceInvalid { leg: "order", .. })
        ));
        // SL_M with price.
        let mut req = doc_gtt_request();
        req.order.order_type = SmartLegOrderType::SlM;
        assert!(validate_gtt_create(&req, 1).is_err());
        // FNO lot multiple.
        let mut req = doc_gtt_request();
        req.segment = "FNO".to_owned();
        req.quantity = 40;
        assert!(matches!(
            validate_gtt_create(&req, 75),
            Err(SmartOrderValidationError::QuantityNotLotMultiple { .. })
        ));
        // Bad trigger price string.
        let mut req = doc_gtt_request();
        req.trigger_price = "3985.123".to_owned();
        assert!(validate_gtt_create(&req, 1).is_err());
    }

    // -- Modify validation --------------------------------------------------------

    #[test]
    fn test_validate_modify_rules() {
        // Empty modify rejected.
        let empty = SmartOrderModifyRequest {
            smart_order_type: SMART_ORDER_TYPE_OCO,
            segment: "FNO".to_owned(),
            quantity: None,
            duration: None,
            product_type: None,
            target: None,
            stop_loss: None,
        };
        assert!(matches!(
            validate_modify(&empty),
            Err(SmartOrderValidationError::EmptyModify)
        ));
        // Partial-leg modify (the doc example): both trigger prices.
        let partial = SmartOrderModifyRequest {
            target: Some(ModifyLeg {
                trigger_price: "122.00".to_owned(),
            }),
            stop_loss: Some(ModifyLeg {
                trigger_price: "97.50".to_owned(),
            }),
            ..empty.clone()
        };
        assert_eq!(validate_modify(&partial), Ok(()));
        // Non-positive quantity rejected.
        let bad_qty = SmartOrderModifyRequest {
            quantity: Some(0),
            ..empty.clone()
        };
        assert!(matches!(
            validate_modify(&bad_qty),
            Err(SmartOrderValidationError::NonPositiveQuantity { .. })
        ));
        // Bad trigger price string rejected.
        let bad_price = SmartOrderModifyRequest {
            target: Some(ModifyLeg {
                trigger_price: "abc".to_owned(),
            }),
            ..empty
        };
        assert!(validate_modify(&bad_price).is_err());
    }

    // -- Wire shape: doc-verbatim field names ---------------------------------

    #[test]
    fn test_oco_create_body_matches_doc_18_verbatim() {
        let body = serde_json::to_value(doc_oco_request()).unwrap();
        let expected = serde_json::json!({
            "reference_id": "sref-unique-456",
            "smart_order_type": "OCO",
            "segment": "FNO",
            "trading_symbol": "NIFTY25OCT24000CE",
            "quantity": 50,
            "net_position_quantity": 50,
            "transaction_type": "SELL",
            "target": {"trigger_price": "120.50", "order_type": "LIMIT", "price": "121.00"},
            "stop_loss": {"trigger_price": "95.00", "order_type": "SL_M", "price": null},
            "product_type": "MIS",
            "exchange": "NSE",
            "duration": "DAY"
        });
        assert_eq!(body, expected);
    }

    #[test]
    fn test_gtt_create_body_matches_doc_verbatim() {
        let body = serde_json::to_value(doc_gtt_request()).unwrap();
        let expected = serde_json::json!({
            "reference_id": "sref-unique-123",
            "smart_order_type": "GTT",
            "segment": "CASH",
            "trading_symbol": "TCS",
            "quantity": 10,
            "trigger_price": "3985.00",
            "trigger_direction": "DOWN",
            "order": {"order_type": "LIMIT", "price": "3990.00", "transaction_type": "BUY"},
            "product_type": "CNC",
            "exchange": "NSE",
            "duration": "DAY"
        });
        assert_eq!(body, expected);
        // child_legs deliberately absent (probe P1 — never a guessed shape).
        assert!(body.get("child_legs").is_none());
    }

    #[test]
    fn test_modify_body_skips_absent_fields() {
        let req = SmartOrderModifyRequest {
            smart_order_type: SMART_ORDER_TYPE_OCO,
            segment: "FNO".to_owned(),
            quantity: Some(40),
            duration: None,
            product_type: None,
            target: Some(ModifyLeg {
                trigger_price: "122.00".to_owned(),
            }),
            stop_loss: None,
        };
        let body = serde_json::to_value(&req).unwrap();
        let expected = serde_json::json!({
            "smart_order_type": "OCO",
            "segment": "FNO",
            "quantity": 40,
            "target": {"trigger_price": "122.00"}
        });
        assert_eq!(body, expected);
    }

    // -- Envelope parsing -------------------------------------------------------

    #[test]
    fn test_parse_success_envelope_gtt_create_response() {
        // The doc 18 §0 / doc 16 GTT 201 response, abridged verbatim.
        let body = r#"{
            "status": "SUCCESS",
            "payload": {
                "smart_order_id": "gtt_91a7f4",
                "smart_order_type": "GTT",
                "status": "ACTIVE",
                "trading_symbol": "TCS",
                "exchange": "NSE",
                "quantity": 10,
                "product_type": "CNC",
                "duration": "DAY",
                "order": {"order_type": "LIMIT", "price": "3990.00", "transaction_type": "BUY"},
                "trigger_direction": "DOWN",
                "trigger_price": "3985.00",
                "is_cancellation_allowed": true,
                "is_modification_allowed": true,
                "created_at": "2025-09-30T07:00:00",
                "expire_at": "2026-09-30T07:00:00",
                "triggered_at": null,
                "updated_at": "2025-09-30T07:00:00"
            }
        }"#;
        let payload = parse_success_envelope::<SmartOrderPayload>(201, body).unwrap();
        assert_eq!(payload.smart_order_id, "gtt_91a7f4");
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status(&payload.status),
            SmartOrderStatus::Active
        );
        assert_eq!(payload.quantity, Some(10));
        assert_eq!(payload.is_cancellation_allowed, Some(true));
        assert_eq!(payload.triggered_at, None);
        assert_eq!(payload.trigger_price.as_deref(), Some("3985.00"));
    }

    #[test]
    fn test_parse_success_envelope_tolerates_partial_modify_echo() {
        // The 202 modify echo is a PARTIAL object (doc 18 §3).
        let body = r#"{"status":"SUCCESS","payload":{"smart_order_id":"oco_a12bc3","smart_order_type":"OCO","status":"ACTIVE","quantity":40}}"#;
        let payload = parse_success_envelope::<SmartOrderPayload>(202, body).unwrap();
        assert_eq!(payload.smart_order_id, "oco_a12bc3");
        assert_eq!(payload.quantity, Some(40));
        assert_eq!(payload.created_at, None);
        assert_eq!(payload.trading_symbol, "");
    }

    #[test]
    fn test_parse_success_envelope_ga_failure_on_4xx() {
        let body = r#"{"status":"FAILURE","error":{"code":"GA007","message":"Duplicate order reference id","metadata":null}}"#;
        let err = parse_success_envelope::<SmartOrderPayload>(400, body).unwrap_err();
        match err {
            SmartOrderError::GaFailure {
                http_status, code, ..
            } => {
                assert_eq!(http_status, 400);
                assert_eq!(code, "GA007");
            }
            other => panic!("expected GaFailure, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_success_envelope_ga_failure_on_2xx_body() {
        // The G1 lesson: a 2xx can carry the FAILURE envelope.
        let body = r#"{"status":"FAILURE","error":{"code":"GA001","message":"Bad request"}}"#;
        let err = parse_success_envelope::<SmartOrderPayload>(200, body).unwrap_err();
        assert!(matches!(
            err,
            SmartOrderError::GaFailure {
                http_status: 200,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_success_envelope_non_2xx_without_ga_envelope() {
        let err =
            parse_success_envelope::<SmartOrderPayload>(502, "<html>gateway</html>").unwrap_err();
        assert!(matches!(
            err,
            SmartOrderError::HttpStatus { status: 502, .. }
        ));
    }

    #[test]
    fn test_parse_success_envelope_unparseable_2xx() {
        let err = parse_success_envelope::<SmartOrderPayload>(200, "not json").unwrap_err();
        assert!(matches!(err, SmartOrderError::Parse(_)));
        // SUCCESS without payload is a typed parse error, never a default
        // payload (no false-OK).
        let err = parse_success_envelope::<SmartOrderPayload>(200, r#"{"status":"SUCCESS"}"#)
            .unwrap_err();
        assert!(matches!(err, SmartOrderError::Parse(_)));
    }

    #[test]
    fn test_parse_list_envelope() {
        let body = r#"{"status":"SUCCESS","payload":{"orders":[
            {"smart_order_id":"oco_a12bc3","smart_order_type":"OCO","status":"ACTIVE"},
            {"smart_order_id":"gtt_91a7f4","smart_order_type":"GTT","status":"WEIRD_2027"}
        ]}}"#;
        let payload = parse_success_envelope::<SmartOrderListPayload>(200, body).unwrap();
        assert_eq!(payload.orders.len(), 2);
        // Unknown-status tolerance: raw preserved, no panic, no parse fail.
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status(&payload.orders[1].status),
            SmartOrderStatus::Unknown("WEIRD_2027".to_owned())
        );
    }

    // -- Status mapper -------------------------------------------------------------

    #[test]
    fn test_smart_order_status_total_mapper() {
        // All six documented values (doc 18 §6), case/whitespace tolerant.
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status("ACTIVE"),
            SmartOrderStatus::Active
        );
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status(" triggered "),
            SmartOrderStatus::Triggered
        );
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status("Cancelled"),
            SmartOrderStatus::Cancelled
        );
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status("EXPIRED"),
            SmartOrderStatus::Expired
        );
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status("FAILED"),
            SmartOrderStatus::Failed
        );
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status("COMPLETED"),
            SmartOrderStatus::Completed
        );
        // Garbage / empty → Unknown with raw preserved; never a panic.
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status(""),
            SmartOrderStatus::Unknown(String::new())
        );
        assert_eq!(
            SmartOrderStatus::from_groww_smart_status("\u{0}garbage\u{feff}"),
            SmartOrderStatus::Unknown("\u{0}garbage\u{feff}".to_owned())
        );
    }

    #[test]
    fn test_smart_order_status_as_str_and_terminal() {
        assert_eq!(SmartOrderStatus::Active.as_str(), "ACTIVE");
        assert_eq!(SmartOrderStatus::Triggered.as_str(), "TRIGGERED");
        assert_eq!(SmartOrderStatus::Cancelled.as_str(), "CANCELLED");
        assert_eq!(SmartOrderStatus::Expired.as_str(), "EXPIRED");
        assert_eq!(SmartOrderStatus::Failed.as_str(), "FAILED");
        assert_eq!(SmartOrderStatus::Completed.as_str(), "COMPLETED");
        assert_eq!(SmartOrderStatus::Unknown("X_9".to_owned()).as_str(), "X_9");
        for s in [
            SmartOrderStatus::Cancelled,
            SmartOrderStatus::Expired,
            SmartOrderStatus::Failed,
            SmartOrderStatus::Completed,
        ] {
            assert!(s.is_terminal(), "{} must be terminal", s.as_str());
        }
        for s in [
            SmartOrderStatus::Active,
            SmartOrderStatus::Triggered,
            SmartOrderStatus::Unknown("NEW_THING".to_owned()),
        ] {
            assert!(!s.is_terminal(), "{} must not be terminal", s.as_str());
        }
    }

    // -- Verdict helpers -----------------------------------------------------------

    #[test]
    fn test_sibling_cancel_verdict_table() {
        use BrokerOrderStatus as S;
        // Nothing executed → Ok regardless of clock.
        assert_eq!(
            sibling_cancel_verdict(&S::Open, &S::Open, 999, 30),
            SiblingVerdict::Ok
        );
        // One filled + sibling cancelled → Ok.
        assert_eq!(
            sibling_cancel_verdict(&S::Filled, &S::Cancelled, 5, 30),
            SiblingVerdict::Ok
        );
        // One filled + sibling terminal-non-fill (Rejected) → Ok (no exposure).
        assert_eq!(
            sibling_cancel_verdict(&S::Filled, &S::Rejected, 5, 30),
            SiblingVerdict::Ok
        );
        // One filled + sibling still open inside deadline → AwaitingCancel.
        assert_eq!(
            sibling_cancel_verdict(&S::Filled, &S::Open, 30, 30),
            SiblingVerdict::AwaitingCancel
        );
        // Same, symmetric legs.
        assert_eq!(
            sibling_cancel_verdict(&S::Open, &S::Filled, 10, 30),
            SiblingVerdict::AwaitingCancel
        );
        // Past the deadline → Violation.
        assert_eq!(
            sibling_cancel_verdict(&S::Filled, &S::Open, 31, 30),
            SiblingVerdict::Violation
        );
        // Both executed → Violation immediately.
        assert_eq!(
            sibling_cancel_verdict(&S::Filled, &S::Filled, 0, 30),
            SiblingVerdict::Violation
        );
        // Partial fill counts as executed-class (fail-closed — partial-fill
        // behavior is UNDOCUMENTED, doc 18 §8.2).
        assert_eq!(
            sibling_cancel_verdict(&S::PartiallyFilled, &S::Open, 31, 30),
            SiblingVerdict::Violation
        );
        // Unknown sibling status is NOT terminal → clock applies.
        assert_eq!(
            sibling_cancel_verdict(&S::Filled, &S::Unknown, 0, 30),
            SiblingVerdict::AwaitingCancel
        );
    }

    #[test]
    fn test_reconcile_verdict_table() {
        assert_eq!(reconcile_verdict(50, 50), ReconcileVerdict::Consistent);
        assert_eq!(
            reconcile_verdict(75, 50),
            ReconcileVerdict::OcoExceedsPosition {
                oco_quantity: 75,
                abs_net_position: 50
            }
        );
        assert_eq!(
            reconcile_verdict(25, 50),
            ReconcileVerdict::PositionExceedsOco {
                oco_quantity: 25,
                abs_net_position: 50
            }
        );
        assert_eq!(reconcile_verdict(0, 0), ReconcileVerdict::Consistent);
    }

    // -- List query -----------------------------------------------------------------

    #[test]
    fn test_list_query_pairs_and_bounds() {
        let q = SmartOrderListQuery {
            segment: Some("FNO".to_owned()),
            smart_order_type: Some("OCO".to_owned()),
            status: Some("ACTIVE".to_owned()),
            page: Some(0),
            page_size: Some(10),
            start_date_time: Some("2025-01-16T09:15:00".to_owned()),
            end_date_time: Some("2025-01-16T15:30:00".to_owned()),
        };
        let pairs = q.to_query_pairs().unwrap();
        assert_eq!(pairs.len(), 7);
        assert_eq!(pairs[0], ("segment", "FNO".to_owned()));
        assert_eq!(pairs[3], ("page", "0".to_owned()));
        // Empty query = empty pairs.
        assert!(
            SmartOrderListQuery::default()
                .to_query_pairs()
                .unwrap()
                .is_empty()
        );
        // Bounds: page > 500 rejected; page_size 0 / 51 rejected.
        assert!(
            SmartOrderListQuery {
                page: Some(501),
                ..SmartOrderListQuery::default()
            }
            .to_query_pairs()
            .is_err()
        );
        assert!(
            SmartOrderListQuery {
                page_size: Some(0),
                ..SmartOrderListQuery::default()
            }
            .to_query_pairs()
            .is_err()
        );
        assert!(
            SmartOrderListQuery {
                page_size: Some(51),
                ..SmartOrderListQuery::default()
            }
            .to_query_pairs()
            .is_err()
        );
    }

    // -- Path-component injection defense ---------------------------------------

    #[test]
    fn test_validate_path_component_rejects_injection() {
        assert!(validate_path_component("smart_order_id", "gtt_91a7f4").is_ok());
        assert!(validate_path_component("segment", "FNO").is_ok());
        for bad in ["", "a/b", "../x", "a b", "a?b", "a#b", "abc%2e"] {
            assert!(
                validate_path_component("smart_order_id", bad).is_err(),
                "`{bad}` must be rejected"
            );
        }
        let too_long = "a".repeat(65);
        assert!(validate_path_component("smart_order_id", &too_long).is_err());
    }

    // -- Client gating (the safety core) ------------------------------------------

    /// A client whose base_url is UNROUTABLE — any accidental send errors
    /// loudly instead of reaching a real host.
    fn unroutable_client(settings: GrowwSmartOrderSettings) -> GrowwSmartOrderClient {
        GrowwSmartOrderClient::new(
            reqwest::Client::new(),
            Some("http://127.0.0.1:1".to_owned()),
            Arc::new(|| Some("test-token-never-logged".to_owned())),
            settings,
        )
    }

    fn write_enabled_settings() -> GrowwSmartOrderSettings {
        GrowwSmartOrderSettings {
            write_enabled: true,
            ..GrowwSmartOrderSettings::default()
        }
    }

    #[tokio::test]
    async fn test_create_oco_disabled_by_config_before_anything() {
        let client = unroutable_client(GrowwSmartOrderSettings::default());
        // Even an INVALID request returns DisabledByConfig — the config
        // gate sits before validation (nothing runs while Gate 1 is shut).
        let mut req = doc_oco_request();
        req.segment = "CASH".to_owned();
        let outcome = client.create_oco(&req, 25).await.unwrap();
        assert_eq!(outcome, SmartOrderOutcome::DisabledByConfig);
    }

    #[tokio::test]
    async fn test_create_oco_dry_run_carries_exact_doc_body() {
        let client = unroutable_client(write_enabled_settings());
        let outcome = client.create_oco(&doc_oco_request(), 25).await.unwrap();
        match outcome {
            SmartOrderOutcome::DryRun {
                endpoint,
                method,
                body_json,
            } => {
                assert_eq!(endpoint, "http://127.0.0.1:1/v1/order-advance/create");
                assert_eq!(method, "POST");
                let body: serde_json::Value = serde_json::from_str(&body_json).unwrap();
                // The serialized field names match doc 18 verbatim.
                let expected = serde_json::json!({
                    "reference_id": "sref-unique-456",
                    "smart_order_type": "OCO",
                    "segment": "FNO",
                    "trading_symbol": "NIFTY25OCT24000CE",
                    "quantity": 50,
                    "net_position_quantity": 50,
                    "transaction_type": "SELL",
                    "target": {"trigger_price": "120.50", "order_type": "LIMIT", "price": "121.00"},
                    "stop_loss": {"trigger_price": "95.00", "order_type": "SL_M", "price": null},
                    "product_type": "MIS",
                    "exchange": "NSE",
                    "duration": "DAY"
                });
                assert_eq!(body, expected);
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_create_oco_validation_fires_before_dry_run() {
        let client = unroutable_client(write_enabled_settings());
        let mut req = doc_oco_request();
        req.segment = "CASH".to_owned();
        let err = client.create_oco(&req, 25).await.unwrap_err();
        assert!(matches!(
            err,
            SmartOrderError::Validation(SmartOrderValidationError::UnsupportedOcoSegment { .. })
        ));
    }

    #[tokio::test]
    async fn test_create_gtt_dry_run() {
        let client = unroutable_client(write_enabled_settings());
        let outcome = client.create_gtt(&doc_gtt_request(), 1).await.unwrap();
        match outcome {
            SmartOrderOutcome::DryRun {
                endpoint, method, ..
            } => {
                assert_eq!(endpoint, "http://127.0.0.1:1/v1/order-advance/create");
                assert_eq!(method, "POST");
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
        // Disabled by default.
        let dark = unroutable_client(GrowwSmartOrderSettings::default());
        assert_eq!(
            dark.create_gtt(&doc_gtt_request(), 1).await.unwrap(),
            SmartOrderOutcome::DisabledByConfig
        );
    }

    #[tokio::test]
    async fn test_modify_dry_run_endpoint_and_body() {
        let client = unroutable_client(write_enabled_settings());
        let req = SmartOrderModifyRequest {
            smart_order_type: SMART_ORDER_TYPE_OCO,
            segment: "FNO".to_owned(),
            quantity: Some(40),
            duration: None,
            product_type: None,
            target: Some(ModifyLeg {
                trigger_price: "122.00".to_owned(),
            }),
            stop_loss: Some(ModifyLeg {
                trigger_price: "97.50".to_owned(),
            }),
        };
        let outcome = client.modify_smart_order("oco_a12bc3", &req).await.unwrap();
        match outcome {
            SmartOrderOutcome::DryRun {
                endpoint,
                method,
                body_json,
            } => {
                assert_eq!(
                    endpoint,
                    "http://127.0.0.1:1/v1/order-advance/modify/oco_a12bc3"
                );
                assert_eq!(method, "PUT");
                let body: serde_json::Value = serde_json::from_str(&body_json).unwrap();
                assert_eq!(body["target"]["trigger_price"], "122.00");
                assert_eq!(body["stop_loss"]["trigger_price"], "97.50");
                assert_eq!(body["quantity"], 40);
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
        // Path injection rejected before anything.
        let err = client
            .modify_smart_order("oco/../evil", &req)
            .await
            .unwrap_err();
        assert!(matches!(err, SmartOrderError::Validation(_)));
    }

    #[tokio::test]
    async fn test_cancel_dry_run_bodyless() {
        let client = unroutable_client(write_enabled_settings());
        let outcome = client
            .cancel_smart_order("FNO", "OCO", "oco_a12bc3")
            .await
            .unwrap();
        assert_eq!(
            outcome,
            SmartOrderOutcome::DryRun {
                endpoint: "http://127.0.0.1:1/v1/order-advance/cancel/FNO/OCO/oco_a12bc3"
                    .to_owned(),
                method: "POST",
                body_json: String::new(),
            }
        );
        // Disabled by default.
        let dark = unroutable_client(GrowwSmartOrderSettings::default());
        assert_eq!(
            dark.cancel_smart_order("FNO", "OCO", "oco_a12bc3")
                .await
                .unwrap(),
            SmartOrderOutcome::DisabledByConfig
        );
    }

    #[tokio::test]
    async fn test_get_and_list_gate_on_read_enabled() {
        // read_enabled = false → DisabledByConfig, nothing sent.
        let dark = unroutable_client(GrowwSmartOrderSettings::default());
        assert_eq!(
            dark.get_smart_order("FNO", "OCO", "oco_a12bc3")
                .await
                .unwrap(),
            SmartOrderReadOutcome::DisabledByConfig
        );
        assert_eq!(
            dark.list_smart_orders(&SmartOrderListQuery::default())
                .await
                .unwrap(),
            SmartOrderReadOutcome::DisabledByConfig
        );
        // read_enabled = true against the unroutable base → a LOUD
        // transport error (proving the read path does attempt the GET,
        // after the single bounded retry).
        let lit = unroutable_client(GrowwSmartOrderSettings {
            read_enabled: true,
            ..GrowwSmartOrderSettings::default()
        });
        let err = lit
            .get_smart_order("FNO", "OCO", "oco_a12bc3")
            .await
            .unwrap_err();
        assert!(matches!(err, SmartOrderError::Transport(_)));
        let err = lit
            .list_smart_orders(&SmartOrderListQuery::default())
            .await
            .unwrap_err();
        assert!(matches!(err, SmartOrderError::Transport(_)));
    }

    #[tokio::test]
    async fn test_no_token_is_typed_and_nothing_sent() {
        let client = GrowwSmartOrderClient::new(
            reqwest::Client::new(),
            Some("http://127.0.0.1:1".to_owned()),
            Arc::new(|| None),
            GrowwSmartOrderSettings {
                read_enabled: true,
                ..GrowwSmartOrderSettings::default()
            },
        );
        let err = client
            .get_smart_order("FNO", "OCO", "oco_a12bc3")
            .await
            .unwrap_err();
        assert!(matches!(err, SmartOrderError::NoToken));
    }

    #[test]
    fn test_client_debug_redacts_token_provider_and_settings_accessor() {
        let client = unroutable_client(GrowwSmartOrderSettings::default());
        let dbg = format!("{client:?}");
        assert!(dbg.contains("<redacted fn>"));
        assert!(!dbg.contains("test-token-never-logged"));
        assert!(!client.settings().write_enabled);
        // Trailing-slash trim on the override.
        let trimmed = GrowwSmartOrderClient::new(
            reqwest::Client::new(),
            Some("http://127.0.0.1:1///".to_owned()),
            Arc::new(|| None),
            GrowwSmartOrderSettings::default(),
        );
        assert_eq!(trimmed.base_url, "http://127.0.0.1:1");
        // No override → the production base.
        let prod = GrowwSmartOrderClient::new(
            reqwest::Client::new(),
            None,
            Arc::new(|| None),
            GrowwSmartOrderSettings::default(),
        );
        assert_eq!(prod.base_url, GROWW_SMART_ORDER_BASE_URL);
    }

    // -- bounded_echo edge (helper coverage) --------------------------------------

    #[test]
    fn test_bounded_echo_truncates_char_safely() {
        let long = "é".repeat(100);
        let echoed = bounded_echo(&long);
        assert_eq!(echoed.chars().count(), ERROR_ECHO_MAX_CHARS);
        assert_eq!(bounded_echo("  ok  "), "ok");
    }

    #[test]
    fn test_write_op_labels() {
        assert_eq!(WriteOp::Create.as_str(), "create");
        assert_eq!(WriteOp::Modify.as_str(), "modify");
        assert_eq!(WriteOp::Cancel.as_str(), "cancel");
    }
}
