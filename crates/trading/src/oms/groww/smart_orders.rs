//! Groww Smart Orders (GTT / OCO) — the §39.3 Smart Orders AREA module
//! (`GROWW-OCO-*`), implemented per the operator's dated 2026-07-16 directive
//! ("why stubs gaps — i clearly told you to implement and integrate
//! everything").
//!
//! # Doc fidelity (the truth source)
//! Every field / enum / status / modifiable-field rule below is
//! field-for-field from `docs/groww-ref/18-smart-orders-schemas.md`
//! (Verified-capture, 2026-07-14 recovery of the 2026-07-03 lossless
//! official-docs capture) + `docs/groww-ref/16-orders-margins-portfolio.md`
//! §5 (GA envelope). Where the stub rule file disagreed with the docs, the
//! DOCS win — the corrections are recorded in
//! `.claude/rules/project/groww-oco-error-codes.md` (2026-07-16 note).
//!
//! # What lives here vs `api_client.rs`
//! This module holds TYPES, VALIDATION, the per-type modify matrix, the
//! 6-value open-set status FSM, the sibling-verify + reconcile pure logic,
//! the coded `GROWW-OCO-*` emit helpers, and the spawnable reconcile loop.
//! ALL HTTP + the `/v1/order-advance/*` endpoint PATH strings live ONLY in
//! `api_client.rs` (Gate 5 confinement — `groww_order_lattice_guard.rs` +
//! the transport guard's zero-HTTP scan, which covers THIS file too).
//!
//! # Money — integer paise at rest, DECIMAL STRINGS on this wire
//! Unlike the regular-order endpoints (JSON numbers), every smart-order
//! price crosses the wire as a decimal STRING (`"3985.00"`) — the doc's
//! verbatim "All prices should be passed as decimal strings" (`ltp` is the
//! only float, response-side). Conversion is pure integer math
//! ([`paise_to_decimal_string`] / [`decimal_string_to_paise`]) — no float
//! ever touches a smart-order price.
//!
//! # Gate lattice
//! Mutations are quadruple-gated: `[groww_orders].smart_orders_write` +
//! `live_fire_requested` (Gate 1) · the non-default `groww_orders` cargo
//! feature (Gate 2) · [`tickvault_common::constants::GROWW_ORDER_LIVE_FIRE`]
//! (Gate 3, `false`) · the §39/§10 rule lock (Gate 4).
//! [`smart_live_send_permitted`] is the boot layer's pure triple-check
//! (Gate 1 knobs + Gate 3 const) before any LIVE spawn; per-send the
//! executor re-checks the Gate-1 knobs, and `ExecutionMode::Live` itself is
//! unreachable outside the const-gated constructor (the house engine.rs
//! precedent). Paper mode simulates ACTIVE smart orders with
//! `PAPER-` ids and ZERO HTTP. Write paths have NO transport retry (the
//! double-send class — the `reference_id` idempotency window is unproven,
//! probe P10).

use std::collections::HashMap;
use std::sync::Arc;

use secrecy::SecretString;
use serde::{Serialize, Serializer};
use thiserror::Error;
use tickvault_common::config::GrowwOrdersConfig;
use tickvault_common::constants::GROWW_ORDER_LIVE_FIRE;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;
use tracing::{error, info};

use super::api_client::{AmbiguityReason, SmartOrderTransport, TransportOutcome};
use super::types::{
    GrowwExchange, GrowwOrderType, GrowwProduct, GrowwSegment, GrowwTransactionType, GrowwValidity,
};

// ---------------------------------------------------------------------------
// Money — integer paise ⇄ decimal wire strings (pure integer math)
// ---------------------------------------------------------------------------

/// Largest |paise| admitted on the smart-order wire — the same ₹1e10 band as
/// the regular-order converter in `types.rs` (NSE-plausible; every value in
/// the band round-trips exactly).
pub const MAX_ABS_PAISE_FOR_DECIMAL_STRING: i64 = 1_000_000_000_000;

/// Render integer paise as the decimal string the smart-order wire expects
/// (`3985.00`). Total — pure integer math, no float, negative kept (callers
/// validate positivity separately; the renderer never lies).
#[must_use]
pub fn paise_to_decimal_string(paise: i64) -> String {
    let sign = if paise < 0 { "-" } else { "" };
    let abs = paise.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// Parse a decimal wire string into integer paise. Total, no-panic:
/// - checked integer math (overflow → `None`);
/// - at most 2 SIGNIFICANT fraction digits (extra digits admitted only when
///   zero — sub-paise precision is REFUSED, never silently truncated);
/// - |paise| beyond the ₹1e10 band → `None`;
/// - empty / non-digit / multi-dot garbage → `None`.
#[must_use]
pub fn decimal_string_to_paise(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    let (neg, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((i, f)) => (i, f),
        None => (digits, ""),
    };
    if int_part.is_empty()
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut frac_paise: i64 = 0;
    for (i, b) in frac_part.bytes().enumerate() {
        let d = i64::from(b - b'0');
        match i {
            0 => frac_paise += d * 10,
            1 => frac_paise += d,
            // Sub-paise digits: refuse anything non-zero (never truncate).
            _ if d != 0 => return None,
            _ => {}
        }
    }
    let int_val: i64 = int_part.parse().ok()?;
    let paise = int_val.checked_mul(100)?.checked_add(frac_paise)?;
    if paise > MAX_ABS_PAISE_FOR_DECIMAL_STRING {
        return None;
    }
    Some(if neg { -paise } else { paise })
}

/// Serialize a required paise field as a decimal wire string.
fn ser_paise_decimal<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&paise_to_decimal_string(*v))
}

/// Serialize an optional paise field as a decimal wire string or an EXPLICIT
/// `null` (the doc's own SL_M example carries `"price": null` — the field is
/// always present on leg objects, never skipped).
fn ser_opt_paise_decimal<S: Serializer>(v: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(p) => s.serialize_str(&paise_to_decimal_string(*p)),
        None => s.serialize_none(),
    }
}

/// Serialize an optional paise field as a decimal string, SKIPPED when
/// absent (modify bodies send only what changes).
fn ser_opt_paise_decimal_skip<S: Serializer>(v: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
    ser_opt_paise_decimal(v, s)
}

// ---------------------------------------------------------------------------
// Wire enums (CLOSED on the request side)
// ---------------------------------------------------------------------------

/// The two smart-order flows (`18` §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SmartOrderType {
    /// Good-Till-Triggered — one trigger, one order leg.
    #[serde(rename = "GTT")]
    Gtt,
    /// One-Cancels-Other — target + stop-loss exit pair.
    #[serde(rename = "OCO")]
    Oco,
}

impl SmartOrderType {
    /// Stable wire/audit label (also the cancel/get PATH segment).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gtt => "GTT",
            Self::Oco => "OCO",
        }
    }
}

/// GTT trigger direction (`18` §1 / SDK constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum TriggerDirection {
    /// Trigger when price crosses UP through the trigger.
    #[serde(rename = "UP")]
    Up,
    /// Trigger when price crosses DOWN through the trigger.
    #[serde(rename = "DOWN")]
    Down,
}

impl TriggerDirection {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "UP",
            Self::Down => "DOWN",
        }
    }
}

/// OCO TARGET-leg order types (`18` §2: "Examples: LIMIT, MARKET") — CLOSED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum TargetLegOrderType {
    /// Limit target (price required).
    #[serde(rename = "LIMIT")]
    Limit,
    /// Market target (no price).
    #[serde(rename = "MARKET")]
    Market,
}

impl TargetLegOrderType {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "LIMIT",
            Self::Market => "MARKET",
        }
    }
}

/// OCO STOP-LOSS-leg order types (`18` §2: "Examples: SL, SL_M") — CLOSED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum StopLossLegOrderType {
    /// Stop-loss LIMIT (price required).
    #[serde(rename = "SL")]
    Sl,
    /// Stop-loss MARKET (`null` price — the doc's own body example).
    #[serde(rename = "SL_M")]
    SlM,
}

impl StopLossLegOrderType {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sl => "SL",
            Self::SlM => "SL_M",
        }
    }
}

// ---------------------------------------------------------------------------
// Create requests (field-for-field per 18-smart-orders-schemas §2.2/§2.3)
// ---------------------------------------------------------------------------

/// The GTT `order` leg object — also the wire shape a GTT MODIFY sends when
/// the leg is being changed (`order.transaction_type` is "required but not
/// modifiable" per the doc, so the WHOLE leg travels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GttOrderLeg {
    /// Leg order type (LIMIT | MARKET | SL | SL_M — the doc's modify note
    /// names all four: "required for LIMIT/SL types; set to null for
    /// MARKET/SL_M").
    pub order_type: GrowwOrderType,
    /// Leg limit price in integer paise; wire: decimal string or explicit
    /// `null` (LIMIT/SL require it; MARKET/SL_M require null).
    #[serde(serialize_with = "ser_opt_paise_decimal")]
    pub price: Option<i64>,
    /// BUY | SELL — required, NOT modifiable.
    pub transaction_type: GrowwTransactionType,
}

/// `POST /v1/order-advance/create` — the GTT body (`18` §0 + `16` GTT table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GrowwCreateGttReq {
    /// 8–20-alnum ≤2-hyphen idempotency key (the executor overwrites it with
    /// a freshly generated `TV…` reference).
    pub reference_id: String,
    /// Always [`SmartOrderType::Gtt`] (validated).
    pub smart_order_type: SmartOrderType,
    /// CASH | FNO (COMMODITY is unsupported for smart orders — the closed
    /// [`GrowwSegment`] enum has no commodity arm by construction).
    pub segment: GrowwSegment,
    /// Exchange trading symbol.
    pub trading_symbol: String,
    /// Order quantity ("For FNO, must respect lot size" — broker-validated).
    pub quantity: i64,
    /// Trigger price, integer paise (wire: decimal string).
    #[serde(serialize_with = "ser_paise_decimal")]
    pub trigger_price: i64,
    /// UP | DOWN.
    pub trigger_direction: TriggerDirection,
    /// The single order leg fired at trigger.
    pub order: GttOrderLeg,
    /// Optional bracket child legs — structure UNDOCUMENTED (probe P1);
    /// modeled as an opaque JSON value, never interpreted locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_legs: Option<serde_json::Value>,
    /// Product (CNC | MIS | NRML).
    pub product_type: GrowwProduct,
    /// Exchange (NSE | BSE).
    pub exchange: GrowwExchange,
    /// DAY-only validity (wire field name is `duration` on this surface).
    pub duration: GrowwValidity,
}

/// An OCO TARGET leg (create).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OcoTargetLeg {
    /// Take-profit trigger price, integer paise (wire: decimal string).
    #[serde(serialize_with = "ser_paise_decimal")]
    pub trigger_price: i64,
    /// LIMIT | MARKET.
    pub order_type: TargetLegOrderType,
    /// Limit price (required iff LIMIT; explicit `null` otherwise).
    #[serde(serialize_with = "ser_opt_paise_decimal")]
    pub price: Option<i64>,
}

/// An OCO STOP-LOSS leg (create).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OcoStopLossLeg {
    /// Stop-loss trigger price, integer paise (wire: decimal string).
    #[serde(serialize_with = "ser_paise_decimal")]
    pub trigger_price: i64,
    /// SL | SL_M.
    pub order_type: StopLossLegOrderType,
    /// Limit price (required iff SL; explicit `null` for SL_M — the doc's
    /// own example).
    #[serde(serialize_with = "ser_opt_paise_decimal")]
    pub price: Option<i64>,
}

/// `POST /v1/order-advance/create` — the OCO body (`18` §2). OCO is
/// EXIT-ONLY: "OCO orders are meant to exit an existing position."
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GrowwCreateOcoReq {
    /// Idempotency key (executor-generated).
    pub reference_id: String,
    /// Always [`SmartOrderType::Oco`] (validated).
    pub smart_order_type: SmartOrderType,
    /// CASH | FNO. CASH-OCO availability is intra-doc CONTRADICTED (probe
    /// P9); when CASH, the product MUST be MIS (both doc arms agree).
    pub segment: GrowwSegment,
    /// Exchange trading symbol.
    pub trading_symbol: String,
    /// "Total quantity for both legs. Must be ≤ abs(net_position_quantity)."
    pub quantity: i64,
    /// "Your current net position in this symbol." — leg directions derive
    /// from it; exit-only means it can never be 0.
    pub net_position_quantity: i64,
    /// "Direction of protection/exit for your position."
    pub transaction_type: GrowwTransactionType,
    /// The take-profit leg.
    pub target: OcoTargetLeg,
    /// The stop-loss leg.
    pub stop_loss: OcoStopLossLeg,
    /// Product ("For OCO in cash segment, only MIS is supported currently").
    pub product_type: GrowwProduct,
    /// Exchange.
    pub exchange: GrowwExchange,
    /// DAY-only validity (wire name `duration`).
    pub duration: GrowwValidity,
}

/// A create request for either flow — serializes as the inner body verbatim
/// (`smart_order_type` discriminates on the wire, per the doc).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SmartOrderCreate {
    /// A GTT create.
    Gtt(GrowwCreateGttReq),
    /// An OCO create.
    Oco(GrowwCreateOcoReq),
}

impl SmartOrderCreate {
    /// The flow discriminant.
    #[must_use]
    pub fn smart_order_type(&self) -> SmartOrderType {
        match self {
            Self::Gtt(_) => SmartOrderType::Gtt,
            Self::Oco(_) => SmartOrderType::Oco,
        }
    }

    /// The request's segment.
    #[must_use]
    pub fn segment(&self) -> GrowwSegment {
        match self {
            Self::Gtt(r) => r.segment,
            Self::Oco(r) => r.segment,
        }
    }

    /// The request's trading symbol.
    #[must_use]
    pub fn trading_symbol(&self) -> &str {
        match self {
            Self::Gtt(r) => &r.trading_symbol,
            Self::Oco(r) => &r.trading_symbol,
        }
    }

    /// The request's quantity.
    #[must_use]
    pub fn quantity(&self) -> i64 {
        match self {
            Self::Gtt(r) => r.quantity,
            Self::Oco(r) => r.quantity,
        }
    }

    /// The request's reference id.
    #[must_use]
    pub fn reference_id(&self) -> &str {
        match self {
            Self::Gtt(r) => &r.reference_id,
            Self::Oco(r) => &r.reference_id,
        }
    }

    /// Overwrite the reference id (the executor owns idempotency).
    pub fn set_reference_id(&mut self, reference_id: &str) {
        match self {
            Self::Gtt(r) => reference_id.clone_into(&mut r.reference_id),
            Self::Oco(r) => reference_id.clone_into(&mut r.reference_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Modify — the PER-TYPE modifiable-field matrix (the doc-corrected rule)
// ---------------------------------------------------------------------------

/// The caller's desired changes — validated against the PER-TYPE matrix
/// BEFORE any wire body is built (an immutable-field attempt is a typed
/// refusal, GROWW-OCO-04, never an HTTP call).
///
/// | Field | GTT | OCO |
/// |---|---|---|
/// | `quantity` | ✅ | ✅ |
/// | `trigger_price` | ✅ | — (use the per-leg fields) |
/// | `trigger_direction` | ✅ | ❌ (N/A) |
/// | `order_leg` (order_type + price + txn) | ✅ | ❌ (leg type/price NOT modifiable) |
/// | `child_legs` | ✅ | ❌ |
/// | `duration` | ❌ | ✅ |
/// | `product_type` | ❌ | ✅ |
/// | `target_trigger_price` | ❌ | ✅ |
/// | `stop_loss_trigger_price` | ❌ | ✅ |
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SmartModifyFields {
    /// New quantity (both flows).
    pub quantity: Option<i64>,
    /// New GTT trigger price (paise).
    pub trigger_price: Option<i64>,
    /// New GTT trigger direction.
    pub trigger_direction: Option<TriggerDirection>,
    /// New GTT order leg (the whole leg travels — `transaction_type` is
    /// required-but-not-modifiable, price shape follows the leg order type).
    pub order_leg: Option<GttOrderLeg>,
    /// New GTT child legs (opaque — "all child leg fields are modifiable").
    pub child_legs: Option<serde_json::Value>,
    /// New OCO duration.
    pub duration: Option<GrowwValidity>,
    /// New OCO product ("e.g., MIS ↔ NRML for FNO; CASH OCO only supports
    /// MIS").
    pub product_type: Option<GrowwProduct>,
    /// New OCO target trigger price (paise).
    pub target_trigger_price: Option<i64>,
    /// New OCO stop-loss trigger price (paise).
    pub stop_loss_trigger_price: Option<i64>,
}

impl SmartModifyFields {
    /// Whether NO field is set (an empty modify is refused).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quantity.is_none()
            && self.trigger_price.is_none()
            && self.trigger_direction.is_none()
            && self.order_leg.is_none()
            && self.child_legs.is_none()
            && self.duration.is_none()
            && self.product_type.is_none()
            && self.target_trigger_price.is_none()
            && self.stop_loss_trigger_price.is_none()
    }
}

/// `PUT /v1/order-advance/modify/{id}` — GTT wire body (routing fields +
/// only-what-changes).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GttModifyBody {
    /// Routing-required flow discriminant.
    pub smart_order_type: SmartOrderType,
    /// Routing-required segment.
    pub segment: GrowwSegment,
    /// New quantity, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    /// New trigger price, when changing.
    #[serde(
        serialize_with = "ser_opt_paise_decimal_skip",
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger_price: Option<i64>,
    /// New trigger direction, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_direction: Option<TriggerDirection>,
    /// The whole order leg, when changing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<GttOrderLeg>,
    /// New child legs, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_legs: Option<serde_json::Value>,
}

/// A trigger-price-only leg patch (OCO modify legs may be sent PARTIALLY —
/// the doc's own example: `"target": {"trigger_price": "122.00"}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LegTriggerPatch {
    /// The new leg trigger price (paise → decimal string).
    #[serde(serialize_with = "ser_paise_decimal")]
    pub trigger_price: i64,
}

/// `PUT /v1/order-advance/modify/{id}` — OCO wire body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OcoModifyBody {
    /// Routing-required flow discriminant.
    pub smart_order_type: SmartOrderType,
    /// Routing-required segment.
    pub segment: GrowwSegment,
    /// New quantity, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    /// New duration, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<GrowwValidity>,
    /// New product, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_type: Option<GrowwProduct>,
    /// Partial target-leg patch, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<LegTriggerPatch>,
    /// Partial stop-loss-leg patch, when changing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<LegTriggerPatch>,
}

/// A modify wire body for either flow (untagged — serializes the inner).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SmartModifyBody {
    /// GTT modify body.
    Gtt(GttModifyBody),
    /// OCO modify body.
    Oco(OcoModifyBody),
}

/// Build the wire modify body from VALIDATED fields ([`validate_modify_fields`]
/// must have passed — this fn is shape-only).
#[must_use]
pub fn build_modify_body(
    smart_order_type: SmartOrderType,
    segment: GrowwSegment,
    fields: &SmartModifyFields,
) -> SmartModifyBody {
    match smart_order_type {
        SmartOrderType::Gtt => SmartModifyBody::Gtt(GttModifyBody {
            smart_order_type,
            segment,
            quantity: fields.quantity,
            trigger_price: fields.trigger_price,
            trigger_direction: fields.trigger_direction,
            order: fields.order_leg,
            child_legs: fields.child_legs.clone(),
        }),
        SmartOrderType::Oco => SmartModifyBody::Oco(OcoModifyBody {
            smart_order_type,
            segment,
            quantity: fields.quantity,
            duration: fields.duration,
            product_type: fields.product_type,
            target: fields
                .target_trigger_price
                .map(|trigger_price| LegTriggerPatch { trigger_price }),
            stop_loss: fields
                .stop_loss_trigger_price
                .map(|trigger_price| LegTriggerPatch { trigger_price }),
        }),
    }
}

// ---------------------------------------------------------------------------
// List query (explicit params — the OCO/ACTIVE defaults bite)
// ---------------------------------------------------------------------------

/// `GET /v1/order-advance/list` query. `smart_order_type` and `status` are
/// EXPLICIT (the server defaults to `OCO` + `ACTIVE` — a reconcile poller
/// must never rely on them). `page` clamps to 0..=500, `page_size` to
/// 1..=50; the optional date window must span ≤ 1 month (server-validated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartOrderListQuery {
    /// Optional segment filter.
    pub segment: Option<GrowwSegment>,
    /// Explicit flow filter.
    pub smart_order_type: SmartOrderType,
    /// Explicit status filter (wire literal, e.g. `"ACTIVE"`).
    pub status: &'static str,
    /// Page (0-based; server max 500).
    pub page: u32,
    /// Page size (server 1..=50, default 10).
    pub page_size: u32,
}

impl SmartOrderListQuery {
    /// ACTIVE-page-0 query for one flow (the reconcile poller's shape).
    #[must_use]
    pub fn active(smart_order_type: SmartOrderType) -> Self {
        Self {
            segment: None,
            smart_order_type,
            status: "ACTIVE",
            page: 0,
            page_size: 50,
        }
    }

    /// Render the query pairs, clamped to the documented bounds. Pure.
    #[must_use]
    pub fn to_query_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::with_capacity(5);
        if let Some(seg) = self.segment {
            pairs.push(("segment", seg.as_str().to_owned()));
        }
        pairs.push((
            "smart_order_type",
            self.smart_order_type.as_str().to_owned(),
        ));
        pairs.push(("status", self.status.to_owned()));
        pairs.push(("page", self.page.min(500).to_string()));
        pairs.push(("page_size", self.page_size.clamp(1, 50).to_string()));
        pairs
    }
}

// ---------------------------------------------------------------------------
// Response payloads — ALL fields Option; prices RAW strings (tolerant)
// ---------------------------------------------------------------------------

/// A response leg echo (GTT `order` / OCO `target` / `stop_loss`). Prices
/// stay RAW wire strings — nothing downstream decides on them (the reconcile
/// compares status + quantity only), so a drifted price format can never
/// fail the parse.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct SmartOrderLegEcho {
    /// Leg trigger price (raw decimal string).
    #[serde(default)]
    pub trigger_price: Option<String>,
    /// Leg order type (raw string).
    #[serde(default)]
    pub order_type: Option<String>,
    /// Leg limit price (raw decimal string / null).
    #[serde(default)]
    pub price: Option<String>,
    /// Leg transaction type (raw string — GTT `order` only).
    #[serde(default)]
    pub transaction_type: Option<String>,
}

/// The full smart-order response object (`18` §7 — GTT + OCO field union;
/// every field `Option`, enum-valued fields raw strings, timestamps raw
/// ISO-8601-no-timezone strings). NOTE (verified doc absence): NO
/// `reference_id` and NO leg `groww_order_id` appear in either schema —
/// an ambiguous create is therefore UNRESOLVABLE by reference (see the
/// executor's fail-closed handling) and post-trigger leg linkage is
/// undocumented (probe P5).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct SmartOrderPayload {
    /// Broker-assigned smart-order id (`gtt_…` / `oco_…`).
    #[serde(default)]
    pub smart_order_id: Option<String>,
    /// `GTT` / `OCO` (raw).
    #[serde(default)]
    pub smart_order_type: Option<String>,
    /// Lifecycle status (raw — parsed via [`SmartOrderStatus::parse`]).
    #[serde(default)]
    pub status: Option<String>,
    /// Trading symbol.
    #[serde(default)]
    pub trading_symbol: Option<String>,
    /// Exchange (raw).
    #[serde(default)]
    pub exchange: Option<String>,
    /// Quantity.
    #[serde(default)]
    pub quantity: Option<i64>,
    /// Product (raw).
    #[serde(default)]
    pub product_type: Option<String>,
    /// Duration (raw).
    #[serde(default)]
    pub duration: Option<String>,
    /// GTT trigger price (raw decimal string).
    #[serde(default)]
    pub trigger_price: Option<String>,
    /// GTT trigger direction (raw).
    #[serde(default)]
    pub trigger_direction: Option<String>,
    /// GTT order leg echo.
    #[serde(default)]
    pub order: Option<SmartOrderLegEcho>,
    /// OCO target leg echo.
    #[serde(default)]
    pub target: Option<SmartOrderLegEcho>,
    /// OCO stop-loss leg echo.
    #[serde(default)]
    pub stop_loss: Option<SmartOrderLegEcho>,
    /// LTP — the ONLY float price on this surface (GTT get/list only).
    #[serde(default)]
    pub ltp: Option<f64>,
    /// Broker remark / status message.
    #[serde(default)]
    pub remark: Option<String>,
    /// Display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Whether cancellation is currently allowed.
    #[serde(default)]
    pub is_cancellation_allowed: Option<bool>,
    /// Whether modification is currently allowed.
    #[serde(default)]
    pub is_modification_allowed: Option<bool>,
    /// Creation timestamp (raw).
    #[serde(default)]
    pub created_at: Option<String>,
    /// Expiry timestamp (raw; GTT ≈ create+1y example-derived, OCO null).
    #[serde(default)]
    pub expire_at: Option<String>,
    /// Trigger timestamp (raw; null until triggered).
    #[serde(default)]
    pub triggered_at: Option<String>,
    /// Last-update timestamp (raw).
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// `GET /v1/order-advance/list` payload: `{"orders":[…]}`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct SmartOrderListPayload {
    /// The rows (absent tolerated → empty downstream).
    #[serde(default)]
    pub orders: Option<Vec<SmartOrderPayload>>,
}

// ---------------------------------------------------------------------------
// Status — the 6-value OPEN-SET lifecycle enum (distinct from regular orders)
// ---------------------------------------------------------------------------

/// Smart-order lifecycle status (`18` §6 — 6 documented values) + the
/// open-set `Unknown` arm preserving the raw wire string. Parsing NEVER
/// panics; an unknown status PARKS (never transitions, counted).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SmartOrderStatus {
    /// "Order is monitoring trigger conditions".
    Active,
    /// "Trigger condition met, order placed" — arms the OCO sibling-verify.
    Triggered,
    /// "User cancelled the order" (terminal).
    Cancelled,
    /// "Order expired due to time/date expiry" (terminal).
    Expired,
    /// "Order placement or trigger failed" (terminal).
    Failed,
    /// "Order successfully completed" (terminal).
    Completed,
    /// Any other wire value — raw preserved for audit; PARKED.
    Unknown(String),
}

impl SmartOrderStatus {
    /// Parse a wire status — case-insensitive, whitespace-trimmed, total.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        match trimmed.to_ascii_uppercase().as_str() {
            "ACTIVE" => Self::Active,
            "TRIGGERED" => Self::Triggered,
            "CANCELLED" => Self::Cancelled,
            "EXPIRED" => Self::Expired,
            "FAILED" => Self::Failed,
            "COMPLETED" => Self::Completed,
            _ => Self::Unknown(trimmed.to_owned()),
        }
    }

    /// Stable audit/metric label (`Unknown` reports the fixed `"UNKNOWN"` —
    /// the raw string never rides a metric label).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Active => "ACTIVE",
            Self::Triggered => "TRIGGERED",
            Self::Cancelled => "CANCELLED",
            Self::Expired => "EXPIRED",
            Self::Failed => "FAILED",
            Self::Completed => "COMPLETED",
            Self::Unknown(_) => "UNKNOWN",
        }
    }

    /// Whether the status is a settled terminal.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Expired | Self::Failed | Self::Completed
        )
    }

    /// Lifecycle rank for the monotone FSM (`None` = unrankable Unknown).
    fn rank(&self) -> Option<u8> {
        match self {
            Self::Active => Some(1),
            Self::Triggered => Some(2),
            Self::Cancelled | Self::Expired | Self::Failed | Self::Completed => Some(3),
            Self::Unknown(_) => None,
        }
    }
}

/// The FSM verdict for one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartTransitionOutcome {
    /// Legal forward move — adopt the observed status.
    Transition,
    /// Same status — refresh only.
    SameStatusRefresh,
    /// Illegal (backward / out-of-terminal / unknown) — PARK, reconcile owns
    /// it (never silently normalized).
    Park,
}

/// Rank-monotone open-set transition evaluation — TOTAL over every
/// (current × observed) pair:
/// - observed `Unknown` → Park (vocabulary drift; raw preserved upstream);
/// - current `Unknown` (parked) → any KNOWN observation recovers (Transition);
/// - same status → refresh;
/// - current terminal → Park (out-of-terminal is illegal, terminal→terminal
///   included);
/// - forward rank → Transition; backward rank → Park.
#[must_use]
pub fn evaluate_smart_transition(
    current: &SmartOrderStatus,
    observed: &SmartOrderStatus,
) -> SmartTransitionOutcome {
    if matches!(observed, SmartOrderStatus::Unknown(_)) {
        return SmartTransitionOutcome::Park;
    }
    if matches!(current, SmartOrderStatus::Unknown(_)) {
        return SmartTransitionOutcome::Transition;
    }
    if current == observed {
        return SmartTransitionOutcome::SameStatusRefresh;
    }
    if current.is_terminal() {
        return SmartTransitionOutcome::Park;
    }
    match (current.rank(), observed.rank()) {
        (Some(c), Some(o)) if o > c => SmartTransitionOutcome::Transition,
        _ => SmartTransitionOutcome::Park,
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed Smart-Orders area error (pre-HTTP refusals + gate refusals).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SmartOrderError {
    /// A request field failed local validation.
    #[error("invalid smart-order field: {0}")]
    InvalidField(String),
    /// A modify carried a field the flow's API declares immutable
    /// (GROWW-OCO-04 — "Use cancel + create when you need changes outside
    /// of these lists").
    #[error("{smart_order_type} modify refused: `{field}` is not modifiable (cancel + create)")]
    ImmutableField {
        /// The flow (`GTT` / `OCO`).
        smart_order_type: &'static str,
        /// The refused field.
        field: &'static str,
    },
    /// Quantity exceeds the configured `max_order_quantity` gate.
    #[error("smart-order quantity {quantity} refused (max_order_quantity = {max})")]
    QuantityRefused {
        /// Requested quantity.
        quantity: i64,
        /// Configured ceiling (0 = refuse all).
        max: i64,
    },
    /// OCO quantity exceeds |net position| (exit-only semantics).
    #[error("OCO quantity {quantity} exceeds |net position| {net_position_quantity}")]
    QuantityExceedsPosition {
        /// Requested quantity.
        quantity: i64,
        /// The declared net position.
        net_position_quantity: i64,
    },
    /// An invalid reference id (violates the 8–20-alnum-≤2-hyphens contract).
    #[error("invalid smart-order reference_id: {0}")]
    InvalidReferenceId(String),
    /// CASH-segment OCO with a non-MIS product — BOTH doc arms agree CASH
    /// OCO is MIS-only (availability itself is the contradicted P9 probe).
    #[error("CASH-segment OCO requires product MIS (got {product})")]
    CashOcoProductNotMis {
        /// The refused product label.
        product: &'static str,
    },
    /// The targeted smart order is not tracked locally — mutations are
    /// refused fail-closed (never a blind wire call on a guessed id).
    #[error("smart order {smart_order_id} is not tracked; mutation refused")]
    UnknownSmartOrder {
        /// The unknown id.
        smart_order_id: String,
    },
    /// The targeted smart order is already terminal.
    #[error("smart order {smart_order_id} is terminal ({status}); mutation refused")]
    AlreadyTerminal {
        /// The terminal order.
        smart_order_id: String,
        /// Its RAW terminal status label (adversarial round 1, finding 7 —
        /// never a mislabeled catch-all).
        status: String,
    },
    /// The tracked-book cap refused a new smart place.
    #[error("tracked smart-order cap {cap} reached; new place refused")]
    TrackedCapExceeded {
        /// The cap.
        cap: usize,
    },
    /// The per-order modification cap refused a further modify.
    #[error("smart order {smart_order_id} reached the modification cap {cap}")]
    ModificationCapExceeded {
        /// The capped order.
        smart_order_id: String,
        /// The cap.
        cap: u32,
    },
    /// A LIVE smart-order mutation was attempted without the full write
    /// gate (`GROWW_ORDER_LIVE_FIRE` + `live_fire_requested` +
    /// `smart_orders_write`) — defense-in-depth over Gates 2/3.
    #[error("smart-order live write gate is closed (lattice §39.2)")]
    WriteGateClosed,
    /// A smart-order READ was attempted with `smart_orders_read` off or
    /// outside market hours.
    #[error("smart-order read gate is closed (smart_orders_read / market hours)")]
    ReadGateClosed,
    /// A broker-supplied smart-order id failed the path-segment shape check
    /// (defense-in-depth — ids ride URL paths).
    #[error("implausible smart_order_id shape; refused")]
    ImplausibleSmartOrderId,
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

/// The Smart-Orders slice of `[groww_orders]` — passed into the executor
/// entry points (the `ExecutorConfig` struct is the Orders area's and stays
/// untouched).
#[derive(Debug, Clone, Copy)]
pub struct SmartOrderGates {
    /// Read-only get/list surface (market-hours-gated when enabled).
    pub smart_orders_read: bool,
    /// Mutation INTENT (inert without Gates 2+3).
    pub smart_orders_write: bool,
    /// The operator's declared live intent (inert without Gate 3).
    pub live_fire_requested: bool,
    /// Fail-closed per-order quantity cap (0 = refuse all).
    pub max_order_quantity: i64,
    /// GROWW-OCO-02 sibling-cancel verification deadline (seconds).
    pub sibling_cancel_deadline_secs: u64,
    /// Reconcile poller cadence (seconds).
    pub reconcile_poll_secs: u64,
}

impl SmartOrderGates {
    /// The fail-closed default: everything OFF, quantity cap 0 (refuse all),
    /// deadlines at the documented config defaults.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            smart_orders_read: false,
            smart_orders_write: false,
            live_fire_requested: false,
            max_order_quantity: 0,
            sibling_cancel_deadline_secs: 30,
            reconcile_poll_secs: 15,
        }
    }

    /// Build from the boot config (the single source of the knob values).
    #[must_use]
    pub fn from_config(cfg: &GrowwOrdersConfig) -> Self {
        Self {
            smart_orders_read: cfg.smart_orders_read,
            smart_orders_write: cfg.smart_orders_write,
            live_fire_requested: cfg.live_fire_requested,
            max_order_quantity: cfg.max_order_quantity,
            sibling_cancel_deadline_secs: cfg.oco_sibling_cancel_deadline_secs,
            reconcile_poll_secs: cfg.oco_reconcile_poll_secs,
        }
    }
}

/// The smart-order LIVE write permission — Gate 3 (`GROWW_ORDER_LIVE_FIRE`,
/// `false` today) AND the operator's `live_fire_requested` AND the
/// area-specific `smart_orders_write`. `false` unless ALL THREE align.
#[must_use]
pub const fn smart_live_send_permitted(
    live_fire_requested: bool,
    smart_orders_write: bool,
) -> bool {
    GROWW_ORDER_LIVE_FIRE && live_fire_requested && smart_orders_write
}

/// Path-segment plausibility for a broker-supplied smart-order id (ids ride
/// URL paths on cancel/get — a hostile id must never traverse). Pure.
#[must_use]
pub fn is_plausible_smart_order_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ---------------------------------------------------------------------------
// Validation (financial boundaries — pure, pre-HTTP)
// ---------------------------------------------------------------------------

fn require_positive_paise(name: &'static str, v: i64) -> Result<(), SmartOrderError> {
    if v <= 0 {
        return Err(SmartOrderError::InvalidField(format!(
            "{name} must be > 0 paise (got {v})"
        )));
    }
    // Upper band on the REQUEST side (adversarial round 1, finding 12): a
    // price past the decimal-string band must be refused HERE, before it can
    // reach serialization (where it would error at the serde boundary).
    if v > MAX_ABS_PAISE_FOR_DECIMAL_STRING {
        return Err(SmartOrderError::InvalidField(format!(
            "{name} exceeds the {MAX_ABS_PAISE_FOR_DECIMAL_STRING}-paise band (got {v})"
        )));
    }
    Ok(())
}

/// Validate a GTT/OCO leg's (order-type, price) SHAPE: price-carrying types
/// (LIMIT / SL) require a strictly-positive price; market types (MARKET /
/// SL_M) require an explicit-null price ("required for LIMIT/SL types; set
/// to null for MARKET/SL_M").
pub fn validate_leg_price_shape(
    leg_name: &'static str,
    price_required: bool,
    order_type_label: &'static str,
    price: Option<i64>,
) -> Result<(), SmartOrderError> {
    match (price_required, price) {
        // Positive AND inside the decimal-string band (finding 12) — an
        // out-of-band price is refused pre-serialization.
        (true, Some(p)) if p > 0 && p <= MAX_ABS_PAISE_FOR_DECIMAL_STRING => Ok(()),
        (true, Some(p)) => Err(SmartOrderError::InvalidField(format!(
            "{leg_name} price must be > 0 and <= {MAX_ABS_PAISE_FOR_DECIMAL_STRING} paise \
             for {order_type_label} (got {p})"
        ))),
        (true, None) => Err(SmartOrderError::InvalidField(format!(
            "{leg_name} price required for {order_type_label}"
        ))),
        (false, Some(_)) => Err(SmartOrderError::InvalidField(format!(
            "{leg_name} price must be null for {order_type_label}"
        ))),
        (false, None) => Ok(()),
    }
}

const fn gtt_leg_price_required(order_type: GrowwOrderType) -> bool {
    matches!(order_type, GrowwOrderType::Limit | GrowwOrderType::Sl)
}

fn validate_common(
    reference_id: &str,
    trading_symbol: &str,
    quantity: i64,
    max_order_quantity: i64,
) -> Result<(), SmartOrderError> {
    if trading_symbol.trim().is_empty() {
        return Err(SmartOrderError::InvalidField(
            "trading_symbol is empty".to_owned(),
        ));
    }
    if quantity < 1 {
        return Err(SmartOrderError::InvalidField(format!(
            "quantity {quantity} < 1"
        )));
    }
    if quantity > max_order_quantity {
        return Err(SmartOrderError::QuantityRefused {
            quantity,
            max: max_order_quantity,
        });
    }
    if !super::reference_id::is_valid_reference_id(reference_id) {
        return Err(SmartOrderError::InvalidReferenceId(reference_id.to_owned()));
    }
    Ok(())
}

/// Validate a smart-order create against the pure financial boundaries.
/// Deliberately NOT enforced (the docs state none — never invented): any
/// RELATIVE ordering between target/stop-loss/trigger prices.
pub fn validate_create_smart_order(
    req: &SmartOrderCreate,
    max_order_quantity: i64,
) -> Result<(), SmartOrderError> {
    match req {
        SmartOrderCreate::Gtt(g) => {
            if g.smart_order_type != SmartOrderType::Gtt {
                return Err(SmartOrderError::InvalidField(
                    "GTT body must carry smart_order_type GTT".to_owned(),
                ));
            }
            validate_common(
                &g.reference_id,
                &g.trading_symbol,
                g.quantity,
                max_order_quantity,
            )?;
            require_positive_paise("trigger_price", g.trigger_price)?;
            validate_leg_price_shape(
                "order",
                gtt_leg_price_required(g.order.order_type),
                g.order.order_type.as_str(),
                g.order.price,
            )
        }
        SmartOrderCreate::Oco(o) => {
            if o.smart_order_type != SmartOrderType::Oco {
                return Err(SmartOrderError::InvalidField(
                    "OCO body must carry smart_order_type OCO".to_owned(),
                ));
            }
            validate_common(
                &o.reference_id,
                &o.trading_symbol,
                o.quantity,
                max_order_quantity,
            )?;
            // Exit-only: a zero net position has nothing to exit.
            if o.net_position_quantity == 0 {
                return Err(SmartOrderError::InvalidField(
                    "OCO is exit-only: net_position_quantity must be non-zero".to_owned(),
                ));
            }
            // `unsigned_abs`, never `abs` — `i64::MIN.abs()` aborts under
            // overflow-checks (adversarial round 1, finding 4).
            if o.quantity.unsigned_abs() > o.net_position_quantity.unsigned_abs() {
                return Err(SmartOrderError::QuantityExceedsPosition {
                    quantity: o.quantity,
                    net_position_quantity: o.net_position_quantity,
                });
            }
            // CASH OCO: MIS-only (both doc arms agree; availability = P9).
            if o.segment == GrowwSegment::Cash && o.product_type != GrowwProduct::Mis {
                return Err(SmartOrderError::CashOcoProductNotMis {
                    product: o.product_type.as_str(),
                });
            }
            require_positive_paise("target.trigger_price", o.target.trigger_price)?;
            validate_leg_price_shape(
                "target",
                o.target.order_type == TargetLegOrderType::Limit,
                o.target.order_type.as_str(),
                o.target.price,
            )?;
            require_positive_paise("stop_loss.trigger_price", o.stop_loss.trigger_price)?;
            validate_leg_price_shape(
                "stop_loss",
                o.stop_loss.order_type == StopLossLegOrderType::Sl,
                o.stop_loss.order_type.as_str(),
                o.stop_loss.price,
            )
        }
    }
}

/// Validate a modify against the PER-TYPE modifiable-field matrix (the
/// 2026-07-16 doc correction — GTT and OCO differ; see the
/// [`SmartModifyFields`] table). An immutable-field attempt is the typed
/// GROWW-OCO-04 refusal BEFORE any HTTP.
pub fn validate_modify_fields(
    smart_order_type: SmartOrderType,
    segment: GrowwSegment,
    fields: &SmartModifyFields,
    max_order_quantity: i64,
) -> Result<(), SmartOrderError> {
    if fields.is_empty() {
        return Err(SmartOrderError::InvalidField(
            "modify carries no modifiable field".to_owned(),
        ));
    }
    match smart_order_type {
        SmartOrderType::Gtt => {
            // OCO-only fields are IMMUTABLE on GTT.
            for (present, field) in [
                (fields.duration.is_some(), "duration"),
                (fields.product_type.is_some(), "product_type"),
                (
                    fields.target_trigger_price.is_some(),
                    "target.trigger_price",
                ),
                (
                    fields.stop_loss_trigger_price.is_some(),
                    "stop_loss.trigger_price",
                ),
            ] {
                if present {
                    return Err(SmartOrderError::ImmutableField {
                        smart_order_type: "GTT",
                        field,
                    });
                }
            }
            if let Some(t) = fields.trigger_price {
                require_positive_paise("trigger_price", t)?;
            }
            if let Some(leg) = &fields.order_leg {
                validate_leg_price_shape(
                    "order",
                    gtt_leg_price_required(leg.order_type),
                    leg.order_type.as_str(),
                    leg.price,
                )?;
            }
        }
        SmartOrderType::Oco => {
            // GTT-only fields are IMMUTABLE on OCO (leg order_type/price are
            // "(not modifiable)"; trigger_direction is N/A).
            for (present, field) in [
                (fields.trigger_price.is_some(), "trigger_price"),
                (fields.trigger_direction.is_some(), "trigger_direction"),
                (fields.order_leg.is_some(), "order (leg order_type/price)"),
                (fields.child_legs.is_some(), "child_legs"),
            ] {
                if present {
                    return Err(SmartOrderError::ImmutableField {
                        smart_order_type: "OCO",
                        field,
                    });
                }
            }
            if let Some(t) = fields.target_trigger_price {
                require_positive_paise("target.trigger_price", t)?;
            }
            if let Some(t) = fields.stop_loss_trigger_price {
                require_positive_paise("stop_loss.trigger_price", t)?;
            }
            // A CASH OCO product_type change must re-assert MIS-only,
            // mirroring create's CashOcoProductNotMis (adversarial round 2,
            // LOW-2 — modify previously lacked the segment to check).
            if let Some(p) = fields.product_type
                && segment == GrowwSegment::Cash
                && p != GrowwProduct::Mis
            {
                return Err(SmartOrderError::CashOcoProductNotMis {
                    product: p.as_str(),
                });
            }
        }
    }
    if let Some(q) = fields.quantity {
        if q < 1 {
            return Err(SmartOrderError::InvalidField(format!("quantity {q} < 1")));
        }
        if q > max_order_quantity {
            return Err(SmartOrderError::QuantityRefused {
                quantity: q,
                max: max_order_quantity,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sibling-cancel verification (GROWW-OCO-02) — pure decision core
// ---------------------------------------------------------------------------

/// The sibling-verify verdict for one OCO trigger episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingVerifyVerdict {
    /// No trigger episode is armed.
    NotArmed,
    /// Triggered; still inside the verification deadline.
    Pending,
    /// The smart-order OBJECT settled terminal — the strongest observable
    /// proof no live sibling remains (leg-level ids are undocumented, P5).
    Verified,
    /// The deadline elapsed without a settled terminal — the GROWW-OCO-02
    /// double-fill exposure window.
    DeadlineExceeded {
        /// Milliseconds since the trigger was first observed.
        elapsed_ms: i64,
    },
}

/// Evaluate one OCO trigger episode — PURE (paused-clock testable).
///
/// HONEST ENVELOPE: the smart-order schemas expose NO per-leg order ids or
/// per-leg statuses (verified doc absence — probe P5), so "sibling observed
/// CANCELLED" is verifiable only at the smart-order-OBJECT level: a settled
/// terminal (COMPLETED / CANCELLED / EXPIRED / FAILED) within the deadline
/// counts as verified; a TRIGGERED (or unreadable) order past the deadline
/// is UNVERIFIED = the exposure page.
#[must_use]
pub fn evaluate_sibling_verify(
    triggered_at_ms: Option<i64>,
    now_ms: i64,
    deadline_secs: u64,
    latest: &SmartOrderStatus,
) -> SiblingVerifyVerdict {
    if latest.is_terminal() {
        return SiblingVerifyVerdict::Verified;
    }
    let Some(t0) = triggered_at_ms else {
        return SiblingVerifyVerdict::NotArmed;
    };
    let elapsed_ms = now_ms.saturating_sub(t0).max(0);
    let deadline_ms = i64::try_from(deadline_secs.saturating_mul(1_000)).unwrap_or(i64::MAX);
    if elapsed_ms > deadline_ms {
        SiblingVerifyVerdict::DeadlineExceeded { elapsed_ms }
    } else {
        SiblingVerifyVerdict::Pending
    }
}

// ---------------------------------------------------------------------------
// Tracked book
// ---------------------------------------------------------------------------

/// One locally-tracked smart order.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackedSmartOrder {
    /// Broker smart-order id (or the `PAPER-…` synthetic id).
    pub smart_order_id: String,
    /// GTT | OCO.
    pub smart_order_type: SmartOrderType,
    /// Segment (routing for cancel/get paths).
    pub segment: GrowwSegment,
    /// Trading symbol.
    pub trading_symbol: String,
    /// Last adopted lifecycle status.
    pub status: SmartOrderStatus,
    /// Tracked quantity.
    pub quantity: i64,
    /// Our reference id (the write-ahead intent key).
    pub reference_id: String,
    /// When TRIGGERED was first observed (OCO sibling-verify episode start).
    pub triggered_at_ms: Option<i64>,
    /// Once-per-episode GROWW-OCO-02 latch.
    pub sibling_unverified_paged: bool,
    /// Modify count (capped at `GROWW_ORDER_MAX_MODIFICATIONS_PER_ORDER`).
    pub modifications: u32,
    /// Consecutive reconcile sweeps where the broker could not find this
    /// order (ghost-local grace, 2 sweeps).
    pub missing_sweeps: u8,
    /// The last broker quantity a QuantityDrift finding was flagged for —
    /// the per-(order, qty) latch so an UNADOPTED drift is found ONCE, not
    /// re-fired every poll pass (adversarial round 1, finding 11).
    pub drift_flagged_qty: Option<i64>,
}

impl TrackedSmartOrder {
    /// A freshly adopted (ACTIVE) tracked order.
    #[must_use]
    pub fn new(
        smart_order_id: String,
        smart_order_type: SmartOrderType,
        segment: GrowwSegment,
        trading_symbol: String,
        quantity: i64,
        reference_id: String,
    ) -> Self {
        Self {
            smart_order_id,
            smart_order_type,
            segment,
            trading_symbol,
            status: SmartOrderStatus::Active,
            quantity,
            reference_id,
            triggered_at_ms: None,
            sibling_unverified_paged: false,
            modifications: 0,
            missing_sweeps: 0,
            drift_flagged_qty: None,
        }
    }
}

/// The in-memory smart-order book (per-executor; shared with the reconcile
/// loop via `Arc<tokio::sync::Mutex<…>>`).
#[derive(Debug, Default)]
pub struct SmartOrderBook {
    orders: HashMap<String, TrackedSmartOrder>,
}

impl SmartOrderBook {
    /// Insert / replace a tracked order.
    pub fn insert(&mut self, order: TrackedSmartOrder) {
        self.orders.insert(order.smart_order_id.clone(), order);
        metrics::gauge!("tv_groww_smart_orders_open").set(self.open_count() as f64);
    }

    /// Read a tracked order.
    #[must_use]
    pub fn get(&self, smart_order_id: &str) -> Option<&TrackedSmartOrder> {
        self.orders.get(smart_order_id)
    }

    /// Mutate a tracked order.
    pub fn get_mut(&mut self, smart_order_id: &str) -> Option<&mut TrackedSmartOrder> {
        self.orders.get_mut(smart_order_id)
    }

    /// Non-terminal tracked ids (the reconcile poll set).
    #[must_use]
    pub fn non_terminal_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .orders
            .values()
            .filter(|o| !o.status.is_terminal())
            .map(|o| o.smart_order_id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Count of non-terminal tracked orders.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.orders
            .values()
            .filter(|o| !o.status.is_terminal())
            .count()
    }

    /// Whether a broker id is tracked.
    #[must_use]
    pub fn contains(&self, smart_order_id: &str) -> bool {
        self.orders.contains_key(smart_order_id)
    }
}

// ---------------------------------------------------------------------------
// Coded emit helpers (the GROWW-OCO-* live emit sites — log-sink-only per
// the rule file §6; every error! carries `code` + `stage`)
// ---------------------------------------------------------------------------

/// Sanitize an order id for LOG display (adversarial round 1, finding 3):
/// plausible ids pass verbatim; anything else is control-stripped +
/// length-capped through the house sanitize choke point — never raw hostile
/// bytes in a log line.
pub(crate) fn log_safe_id(id: &str) -> std::borrow::Cow<'_, str> {
    if is_plausible_smart_order_id(id) {
        std::borrow::Cow::Borrowed(id)
    } else {
        std::borrow::Cow::Owned(capture_rest_error_body(id))
    }
}

/// GROWW-OCO-01 — a create/cancel leg failed (definitive reject or an
/// unresolvable ambiguity). `detail` passes the sanitize choke point.
pub(crate) fn emit_oco01(leg: &'static str, stage: &'static str, id: Option<&str>, detail: &str) {
    error!(
        target: "groww_oco",
        code = ErrorCode::GrowwOco01PlacementFailed.code_str(),
        stage,
        leg,
        smart_order_id = %log_safe_id(id.unwrap_or("n/a")),
        detail = %capture_rest_error_body(detail),
        "groww smart order: {leg} leg failed"
    );
}

/// GROWW-OCO-02 — sibling-cancel UNVERIFIED past the deadline (Critical;
/// once per trigger episode, latched by the caller).
pub(crate) fn emit_oco02(smart_order_id: &str, elapsed_ms: i64, deadline_secs: u64) {
    error!(
        target: "groww_oco",
        code = ErrorCode::GrowwOco02SiblingCancelUnverified.code_str(),
        stage = "deadline_exceeded",
        smart_order_id = %log_safe_id(smart_order_id),
        elapsed_ms,
        deadline_secs,
        "groww smart order: OCO sibling cancel UNVERIFIED past the deadline — \
         verify the sibling on the broker book NOW (double-fill exposure)"
    );
}

/// GROWW-OCO-03 — reconcile findings (ONE coalesced emit per pass; ≤5 sample
/// lines; each finding also rides the findings counter).
pub(crate) fn emit_oco03(findings: &[SmartReconcileFinding]) {
    let samples: Vec<String> = findings
        .iter()
        .take(5)
        .map(|f| {
            format!(
                "{} {} {}",
                log_safe_id(&f.smart_order_id),
                f.kind.as_str(),
                capture_rest_error_body(&f.detail)
            )
        })
        .collect();
    error!(
        target: "groww_oco",
        code = ErrorCode::GrowwOco03ReconcileMismatch.code_str(),
        stage = "reconcile_pass",
        findings = findings.len(),
        samples = ?samples,
        "groww smart order: reconcile found local-vs-broker divergence — \
         never auto-normalized; operator decides which side is wrong"
    );
}

/// GROWW-OCO-04 — a modify was refused (immutable field, pre-HTTP) or
/// rejected/ambiguous at the broker.
pub(crate) fn emit_oco04(stage: &'static str, id: Option<&str>, detail: &str) {
    error!(
        target: "groww_oco",
        code = ErrorCode::GrowwOco04ModifyRejected.code_str(),
        stage,
        smart_order_id = %log_safe_id(id.unwrap_or("n/a")),
        detail = %capture_rest_error_body(detail),
        "groww smart order: modify refused/rejected"
    );
}

/// GROWW-OCO-05 — the get/list reconcile poller degraded (transport / token
/// / rate-limit / decode).
pub(crate) fn emit_oco05(stage: &'static str, detail: &str) {
    metrics::counter!("tv_groww_oco_poller_errors_total", "stage" => stage).increment(1);
    error!(
        target: "groww_oco",
        code = ErrorCode::GrowwOco05PollerDegraded.code_str(),
        stage,
        detail = %capture_rest_error_body(detail),
        "groww smart order: reconcile poller degraded — next tick retries"
    );
}

/// The mutation-outcome counter (static labels only).
pub(crate) fn count_mutation(leg: &'static str, outcome: &'static str) {
    metrics::counter!(
        "tv_groww_oco_mutations_total",
        "leg" => leg,
        "outcome" => outcome
    )
    .increment(1);
}

// ---------------------------------------------------------------------------
// Reconcile — pure classification + one bounded pass + the spawnable loop
// ---------------------------------------------------------------------------

/// Reconcile finding kinds (static metric labels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartFindingKind {
    /// Observed status is an illegal backward / out-of-terminal move.
    StatusRegression,
    /// Observed quantity differs from the tracked quantity.
    QuantityDrift,
    /// Broker could not find a tracked non-terminal order on 2 consecutive
    /// sweeps.
    GhostLocal,
    /// Tracked OCO quantity exceeds |net position| (when a position map is
    /// supplied by the caller — the future wiring PR's portfolio snapshot).
    QtyExceedsPosition,
    /// The broker served a status outside the documented vocabulary — the
    /// order is PARKED (raw preserved on the finding detail).
    UnknownStatus,
}

impl SmartFindingKind {
    /// Stable metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StatusRegression => "status_regression",
            Self::QuantityDrift => "quantity_drift",
            Self::GhostLocal => "ghost_local",
            Self::QtyExceedsPosition => "qty_exceeds_position",
            Self::UnknownStatus => "unknown_status",
        }
    }
}

/// One reconcile finding.
#[derive(Debug, Clone, PartialEq)]
pub struct SmartReconcileFinding {
    /// The affected smart order.
    pub smart_order_id: String,
    /// The finding class.
    pub kind: SmartFindingKind,
    /// Bounded human detail (sanitized at emit time).
    pub detail: String,
}

/// The report of one reconcile pass.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SmartReconcileReport {
    /// Tracked orders polled this pass.
    pub polled: usize,
    /// Fail-closed findings (GROWW-OCO-03).
    pub findings: Vec<SmartReconcileFinding>,
    /// OCO ids whose sibling verification exceeded the deadline THIS pass
    /// (GROWW-OCO-02, once per episode).
    pub sibling_unverified: Vec<String>,
    /// Poller degrade stages observed (GROWW-OCO-05).
    pub degraded: Vec<&'static str>,
    /// Broker ACTIVE rows not tracked locally — counted, NEVER acted on
    /// (smart-order responses carry no reference id, so attribution is
    /// impossible; foreign = the co-tenant or a pre-boot order).
    pub foreign_untracked: usize,
}

/// Classify one observation of a tracked order — PURE.
#[must_use]
pub fn classify_smart_observation(
    tracked: &TrackedSmartOrder,
    observed_status: &SmartOrderStatus,
    observed_quantity: Option<i64>,
    net_position: Option<i64>,
) -> (SmartTransitionOutcome, Vec<SmartReconcileFinding>) {
    let outcome = evaluate_smart_transition(&tracked.status, observed_status);
    let mut findings = Vec::new();
    if outcome == SmartTransitionOutcome::Park {
        if let SmartOrderStatus::Unknown(raw) = observed_status {
            findings.push(SmartReconcileFinding {
                smart_order_id: tracked.smart_order_id.clone(),
                kind: SmartFindingKind::UnknownStatus,
                detail: format!("unknown status {raw:?} parked"),
            });
        } else {
            findings.push(SmartReconcileFinding {
                smart_order_id: tracked.smart_order_id.clone(),
                kind: SmartFindingKind::StatusRegression,
                detail: format!(
                    "illegal {} -> {}",
                    tracked.status.as_str(),
                    observed_status.as_str()
                ),
            });
        }
    }
    if let Some(q) = observed_quantity
        && q != tracked.quantity
        // Per-(order, qty) latch (finding 11): the SAME unadopted drift is
        // found once, not re-fired every poll pass; a NEW drifted value
        // re-fires.
        && tracked.drift_flagged_qty != Some(q)
    {
        findings.push(SmartReconcileFinding {
            smart_order_id: tracked.smart_order_id.clone(),
            kind: SmartFindingKind::QuantityDrift,
            detail: format!("local qty {} vs broker qty {q}", tracked.quantity),
        });
    }
    if tracked.smart_order_type == SmartOrderType::Oco
        && let Some(net) = net_position
        // `unsigned_abs`, never `abs` — `i64::MIN.abs()` aborts under
        // overflow-checks (adversarial round 1, finding 4).
        && tracked.quantity.unsigned_abs() > net.unsigned_abs()
    {
        findings.push(SmartReconcileFinding {
            smart_order_id: tracked.smart_order_id.clone(),
            kind: SmartFindingKind::QtyExceedsPosition,
            detail: format!(
                "OCO qty {} vs |net position| {}",
                tracked.quantity,
                net.unsigned_abs()
            ),
        });
    }
    (outcome, findings)
}

/// Consecutive ghost-local sweeps before the finding fires (grace — one
/// missing snapshot is not proof of absence).
pub const SMART_GHOST_LOCAL_CONFIRM_SWEEPS: u8 = 2;

/// Apply one observation to a tracked order (transition adoption + sibling
/// episode arming + OCO-02 evaluation). Returns the sibling verdict.
fn apply_observation(
    tracked: &mut TrackedSmartOrder,
    observed_status: &SmartOrderStatus,
    observed_quantity: Option<i64>,
    outcome: SmartTransitionOutcome,
    now_ms: i64,
    deadline_secs: u64,
) -> SiblingVerifyVerdict {
    tracked.missing_sweeps = 0;
    match outcome {
        SmartTransitionOutcome::Transition => {
            tracked.status = observed_status.clone();
            if let Some(q) = observed_quantity {
                tracked.quantity = q;
            }
        }
        SmartTransitionOutcome::SameStatusRefresh | SmartTransitionOutcome::Park => {}
    }
    // Maintain the QuantityDrift latch (finding 11): a still-drifted broker
    // qty latches (found once); an agreeing qty clears the latch. A
    // Transition that ADOPTED the broker qty lands in the agreeing arm.
    if let Some(q) = observed_quantity {
        tracked.drift_flagged_qty = if q != tracked.quantity { Some(q) } else { None };
    }
    // Arm the OCO sibling episode on the first observed TRIGGERED.
    if tracked.smart_order_type == SmartOrderType::Oco
        && tracked.status == SmartOrderStatus::Triggered
        && tracked.triggered_at_ms.is_none()
    {
        tracked.triggered_at_ms = Some(now_ms);
    }
    let verdict = if tracked.smart_order_type == SmartOrderType::Oco {
        evaluate_sibling_verify(
            tracked.triggered_at_ms,
            now_ms,
            deadline_secs,
            &tracked.status,
        )
    } else {
        SiblingVerifyVerdict::NotArmed
    };
    if verdict == SiblingVerifyVerdict::Verified && tracked.sibling_unverified_paged {
        info!(
            target: "groww_oco",
            smart_order_id = %log_safe_id(&tracked.smart_order_id),
            "groww smart order: OCO settled terminal — sibling exposure episode closed"
        );
        tracked.sibling_unverified_paged = false;
        tracked.triggered_at_ms = None;
    }
    verdict
}

/// One bounded reconcile pass over the tracked book — per-tracked GET +
/// one ACTIVE list per flow (foreign counting). Emits GROWW-OCO-02/-03/-05
/// per the report; NEVER mutates broker state.
pub async fn reconcile_pass<T: SmartOrderTransport>(
    transport: &T,
    token: &SecretString,
    book: &mut SmartOrderBook,
    deadline_secs: u64,
    now_ms: i64,
    positions: &HashMap<String, i64>,
) -> SmartReconcileReport {
    let mut report = SmartReconcileReport::default();
    let ids = book.non_terminal_ids();
    'poll: for id in &ids {
        let Some(snapshot) = book.get(id).cloned() else {
            continue;
        };
        if !is_plausible_smart_order_id(id) {
            // Never build a URL path from an implausible id.
            report.degraded.push("implausible_id");
            continue;
        }
        report.polled += 1;
        let obs = transport
            .get_smart_order(snapshot.segment, snapshot.smart_order_type, id, token)
            .await;
        match obs {
            TransportOutcome::Success(p) => {
                // Identity guard (adversarial round 2, HIGH): the ONLY
                // unguarded broker echo. A GET response carrying a DIFFERENT
                // `smart_order_id` must NEVER be classified/applied under
                // `id` — a foreign echo could silently terminalize a live
                // OCO (dropping it from the non-terminal set → the OCO-02
                // double-fill guard is silenced) or false-arm an episode.
                if let Some(other) = p.smart_order_id.as_deref()
                    && other != id.as_str()
                {
                    report.degraded.push("id_mismatch");
                    emit_oco05(
                        "id_mismatch",
                        &format!("requested {} got {}", log_safe_id(id), log_safe_id(other)),
                    );
                    continue;
                }
                let observed_status = p
                    .status
                    .as_deref()
                    .map_or(SmartOrderStatus::Unknown(String::new()), |s| {
                        SmartOrderStatus::parse(s)
                    });
                let net = positions.get(&snapshot.trading_symbol).copied();
                let (outcome, findings) =
                    classify_smart_observation(&snapshot, &observed_status, p.quantity, net);
                for f in &findings {
                    metrics::counter!(
                        "tv_groww_oco_reconcile_findings_total",
                        "kind" => f.kind.as_str()
                    )
                    .increment(1);
                }
                report.findings.extend(findings);
                if let Some(tracked) = book.get_mut(id) {
                    let verdict = apply_observation(
                        tracked,
                        &observed_status,
                        p.quantity,
                        outcome,
                        now_ms,
                        deadline_secs,
                    );
                    if let SiblingVerifyVerdict::DeadlineExceeded { elapsed_ms } = verdict
                        && !tracked.sibling_unverified_paged
                    {
                        tracked.sibling_unverified_paged = true;
                        emit_oco02(id, elapsed_ms, deadline_secs);
                        metrics::counter!(
                            "tv_groww_oco_reconcile_findings_total",
                            "kind" => "sibling_unverified"
                        )
                        .increment(1);
                        report.sibling_unverified.push(id.clone());
                    }
                }
            }
            TransportOutcome::Rejected {
                http_status,
                ga_code,
                ..
            } if http_status == 404 || ga_code.as_deref() == Some("GA004") => {
                if let Some(tracked) = book.get_mut(id) {
                    tracked.missing_sweeps = tracked.missing_sweeps.saturating_add(1);
                    if tracked.missing_sweeps >= SMART_GHOST_LOCAL_CONFIRM_SWEEPS {
                        let finding = SmartReconcileFinding {
                            smart_order_id: id.clone(),
                            kind: SmartFindingKind::GhostLocal,
                            detail: format!(
                                "broker cannot find it ({} consecutive sweeps)",
                                tracked.missing_sweeps
                            ),
                        };
                        metrics::counter!(
                            "tv_groww_oco_reconcile_findings_total",
                            "kind" => SmartFindingKind::GhostLocal.as_str()
                        )
                        .increment(1);
                        report.findings.push(finding);
                    }
                }
            }
            TransportOutcome::Rejected { ga_code, .. } => {
                report.degraded.push("rejected_read");
                emit_oco05("rejected_read", ga_code.as_deref().unwrap_or("no-ga"));
            }
            TransportOutcome::AuthStale { http_status } => {
                // The token is dead for EVERY read — abort the pass.
                report.degraded.push("token");
                emit_oco05("token", &format!("auth stale {http_status}"));
                break 'poll;
            }
            TransportOutcome::RateLimited { .. } => {
                // Never out-polled — stop the pass; the next tick retries.
                report.degraded.push("rate_limited");
                emit_oco05("rate_limited", "429 on smart-order read");
                break 'poll;
            }
            TransportOutcome::Ambiguous(reason) => {
                report.degraded.push("transport");
                emit_oco05("transport", reason.as_str());
            }
        }
    }

    // Deadline sweep over UNREADABLE triggered OCOs (a GET failure must not
    // silence the exposure page — TRIGGERED-or-unreadable past the deadline
    // pages).
    for id in &ids {
        let Some(tracked) = book.get_mut(id) else {
            continue;
        };
        if tracked.smart_order_type != SmartOrderType::Oco || tracked.sibling_unverified_paged {
            continue;
        }
        if let SiblingVerifyVerdict::DeadlineExceeded { elapsed_ms } = evaluate_sibling_verify(
            tracked.triggered_at_ms,
            now_ms,
            deadline_secs,
            &tracked.status,
        ) {
            tracked.sibling_unverified_paged = true;
            emit_oco02(id, elapsed_ms, deadline_secs);
            metrics::counter!(
                "tv_groww_oco_reconcile_findings_total",
                "kind" => "sibling_unverified"
            )
            .increment(1);
            if !report.sibling_unverified.contains(id) {
                report.sibling_unverified.push(id.clone());
            }
        }
    }

    // Foreign counting: one ACTIVE list per flow. Untracked rows are counted
    // and NEVER acted on (no reference id exists on smart-order responses —
    // attribution is impossible; co-tenant discipline).
    if !report.degraded.contains(&"token") && !report.degraded.contains(&"rate_limited") {
        for flow in [SmartOrderType::Gtt, SmartOrderType::Oco] {
            let q = SmartOrderListQuery::active(flow);
            match transport.list_smart_orders(&q, token).await {
                TransportOutcome::Success(list) => {
                    for row in list.orders.unwrap_or_default() {
                        if let Some(id) = row.smart_order_id
                            && !book.contains(&id)
                        {
                            report.foreign_untracked += 1;
                        }
                    }
                }
                TransportOutcome::AuthStale { http_status } => {
                    report.degraded.push("token");
                    emit_oco05("token", &format!("auth stale {http_status} on list"));
                    break;
                }
                TransportOutcome::RateLimited { .. } => {
                    report.degraded.push("rate_limited");
                    emit_oco05("rate_limited", "429 on smart-order list");
                    break;
                }
                TransportOutcome::Rejected { ga_code, .. } => {
                    report.degraded.push("rejected_read");
                    emit_oco05("rejected_read", ga_code.as_deref().unwrap_or("no-ga"));
                }
                TransportOutcome::Ambiguous(reason) => {
                    report.degraded.push("transport");
                    emit_oco05("transport", reason.as_str());
                }
            }
        }
    }

    if !report.findings.is_empty() {
        emit_oco03(&report.findings);
    }
    report
}

/// The spawnable OCO reconcile poller (the margin-loop house pattern:
/// interval = `reconcile_poll_secs`, shutdown watch). HONESTY (adversarial
/// round 1, finding 9): `gates` is moved BY VALUE at spawn time — the
/// per-turn check re-reads the CAPTURED copy, so a runtime config flip
/// needs a loop respawn to take effect (only `in_market_hours` and the
/// token are genuinely re-read each turn). The spawn site is the FUTURE
/// wiring PR — no crates/app code in
/// this area PR. `token_provider` returns the CURRENT shared-minter token
/// (SSM-read upstream; NEVER minted here).
// WIRING-EXEMPT: the spawn site is the deferred app-integration PR (§39
// live-flip scope); the pass it drives is exercised through the executor +
// unit tests.
pub async fn run_smart_order_reconcile_loop<T, P, M>(
    transport: T,
    token_provider: P,
    book: Arc<tokio::sync::Mutex<SmartOrderBook>>,
    gates: SmartOrderGates,
    in_market_hours: M,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    T: SmartOrderTransport,
    P: Fn() -> Option<SecretString>,
    M: Fn() -> bool,
{
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        gates.reconcile_poll_secs.max(1),
    ));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    info!(target: "groww_oco", "smart-order reconcile loop: shutdown observed — exiting");
                    return;
                }
                continue;
            }
        }
        if *shutdown.borrow() {
            info!(target: "groww_oco", "smart-order reconcile loop: shutdown observed — exiting");
            return;
        }
        // Read gate: the CAPTURED gates copy + the live market-hours closure
        // (a runtime gate flip needs a respawn — see the fn doc, finding 9).
        if !gates.smart_orders_read || !in_market_hours() {
            continue;
        }
        let Some(token) = token_provider() else {
            emit_oco05("token", "no access token available this turn");
            continue;
        };
        let now_ms = epoch_now_ms();
        let mut guard = book.lock().await;
        let _report = reconcile_pass(
            &transport,
            &token,
            &mut guard,
            gates.sibling_cancel_deadline_secs,
            now_ms,
            &HashMap::new(),
        )
        .await;
    }
}

/// Wall-clock epoch milliseconds (cold path; 0 on a pre-1970 clock).
fn epoch_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Map an ambiguity reason to the OCO-01/-04 stage taxonomy.
pub(crate) const fn ambiguity_stage(reason: &AmbiguityReason) -> &'static str {
    match reason {
        AmbiguityReason::ConnectPhase => "connect_phase",
        AmbiguityReason::Timeout => "timeout",
        AmbiguityReason::ServerError(_) => "server_error",
        AmbiguityReason::Decode => "decode",
        AmbiguityReason::GaOnNonRejectStatus(_) => "ga_on_non_reject_status",
        AmbiguityReason::MissingPayload => "missing_payload",
        AmbiguityReason::SendFailed => "send_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oms::groww::api_client::AmbiguityReason;
    use crate::oms::groww::intent_ledger::IntentReceipt;
    use proptest::prelude::*;
    use std::sync::Mutex as StdMutex;

    // -- money: paise <-> decimal wire strings (financial boundaries) -------

    #[test]
    fn test_paise_to_decimal_string_renders_doc_examples() {
        assert_eq!(paise_to_decimal_string(398_500), "3985.00");
        assert_eq!(paise_to_decimal_string(12_200), "122.00");
        assert_eq!(paise_to_decimal_string(0), "0.00");
        assert_eq!(paise_to_decimal_string(5), "0.05");
        assert_eq!(paise_to_decimal_string(-5), "-0.05");
        assert_eq!(paise_to_decimal_string(1), "0.01");
        assert_eq!(
            paise_to_decimal_string(MAX_ABS_PAISE_FOR_DECIMAL_STRING),
            "10000000000.00"
        );
    }

    #[test]
    fn test_decimal_string_to_paise_boundaries() {
        assert_eq!(decimal_string_to_paise("3985.00"), Some(398_500));
        assert_eq!(decimal_string_to_paise("3985"), Some(398_500));
        assert_eq!(decimal_string_to_paise("3985.5"), Some(398_550));
        assert_eq!(decimal_string_to_paise(" 3985.50 "), Some(398_550));
        assert_eq!(decimal_string_to_paise("-0.01"), Some(-1));
        assert_eq!(decimal_string_to_paise("0"), Some(0));
        // Sub-paise precision is REFUSED (never truncated) — zeros tolerated.
        assert_eq!(decimal_string_to_paise("3985.505"), None);
        assert_eq!(decimal_string_to_paise("3985.500"), Some(398_550));
        // Band edges.
        assert_eq!(
            decimal_string_to_paise("10000000000.00"),
            Some(MAX_ABS_PAISE_FOR_DECIMAL_STRING)
        );
        assert_eq!(decimal_string_to_paise("10000000000.01"), None);
        // Garbage / overflow.
        for bad in [
            "",
            ".",
            "-",
            "1e3",
            "1.2.3",
            "abc",
            "1 2",
            "--1",
            "99999999999999999999",
            "0x10",
        ] {
            assert_eq!(decimal_string_to_paise(bad), None, "{bad:?}");
        }
    }

    proptest! {
        #[test]
        fn prop_paise_decimal_roundtrip(p in -MAX_ABS_PAISE_FOR_DECIMAL_STRING..=MAX_ABS_PAISE_FOR_DECIMAL_STRING) {
            let s = paise_to_decimal_string(p);
            prop_assert_eq!(decimal_string_to_paise(&s), Some(p));
        }

        #[test]
        fn prop_decimal_parse_never_panics(s in "\\PC*") {
            let _ = decimal_string_to_paise(&s);
        }

        #[test]
        fn prop_status_parse_total_never_panics(s in "\\PC*") {
            let parsed = SmartOrderStatus::parse(&s);
            // The label is always one of the 7 fixed strings.
            let label = parsed.as_str().to_owned();
            prop_assert!([
                "ACTIVE", "TRIGGERED", "CANCELLED", "EXPIRED", "FAILED",
                "COMPLETED", "UNKNOWN"
            ]
            .contains(&label.as_str()));
        }
    }

    // -- serde wire shape ----------------------------------------------------

    fn gtt_req() -> GrowwCreateGttReq {
        GrowwCreateGttReq {
            reference_id: "TV2607160001ABCD".to_owned(),
            smart_order_type: SmartOrderType::Gtt,
            segment: GrowwSegment::Cash,
            trading_symbol: "RELIANCE".to_owned(),
            quantity: 10,
            trigger_price: 398_500,
            trigger_direction: TriggerDirection::Up,
            order: GttOrderLeg {
                order_type: GrowwOrderType::Market,
                price: None,
                transaction_type: GrowwTransactionType::Sell,
            },
            child_legs: None,
            product_type: GrowwProduct::Cnc,
            exchange: GrowwExchange::Nse,
            duration: GrowwValidity::Day,
        }
    }

    fn oco_req() -> GrowwCreateOcoReq {
        GrowwCreateOcoReq {
            reference_id: "TV2607160002ABCD".to_owned(),
            smart_order_type: SmartOrderType::Oco,
            segment: GrowwSegment::Fno,
            trading_symbol: "NIFTY26JUL28500CE".to_owned(),
            quantity: 75,
            net_position_quantity: 75,
            transaction_type: GrowwTransactionType::Sell,
            target: OcoTargetLeg {
                trigger_price: 12_200,
                order_type: TargetLegOrderType::Limit,
                price: Some(12_150),
            },
            stop_loss: OcoStopLossLeg {
                trigger_price: 9_800,
                order_type: StopLossLegOrderType::SlM,
                price: None,
            },
            product_type: GrowwProduct::Nrml,
            exchange: GrowwExchange::Nse,
            duration: GrowwValidity::Day,
        }
    }

    #[test]
    fn test_gtt_create_wire_shape_decimal_strings_and_explicit_null_price() {
        let v = serde_json::to_value(SmartOrderCreate::Gtt(gtt_req())).expect("serialize");
        assert_eq!(v["smart_order_type"], "GTT");
        assert_eq!(v["segment"], "CASH");
        assert_eq!(v["trigger_price"], "3985.00"); // decimal STRING
        assert_eq!(v["trigger_direction"], "UP");
        assert_eq!(v["duration"], "DAY");
        // MARKET leg: `price` PRESENT and explicitly null (never skipped).
        let order = v["order"].as_object().expect("order leg");
        assert!(order.contains_key("price"));
        assert!(order["price"].is_null());
        assert_eq!(order["order_type"], "MARKET");
        assert_eq!(order["transaction_type"], "SELL");
        // Absent child_legs is SKIPPED (untouched-key discipline).
        assert!(!v.as_object().expect("map").contains_key("child_legs"));
    }

    #[test]
    fn test_oco_create_wire_shape_matches_doc_example() {
        let v = serde_json::to_value(SmartOrderCreate::Oco(oco_req())).expect("serialize");
        assert_eq!(v["smart_order_type"], "OCO");
        assert_eq!(v["segment"], "FNO");
        assert_eq!(v["net_position_quantity"], 75);
        assert_eq!(v["target"]["trigger_price"], "122.00");
        assert_eq!(v["target"]["order_type"], "LIMIT");
        assert_eq!(v["target"]["price"], "121.50");
        assert_eq!(v["stop_loss"]["trigger_price"], "98.00");
        assert_eq!(v["stop_loss"]["order_type"], "SL_M");
        assert!(v["stop_loss"]["price"].is_null()); // the doc's own example
    }

    #[test]
    fn test_modify_body_sends_only_what_changes() {
        let gtt = build_modify_body(
            SmartOrderType::Gtt,
            GrowwSegment::Cash,
            &SmartModifyFields {
                quantity: Some(20),
                ..Default::default()
            },
        );
        let v = serde_json::to_value(&gtt).expect("serialize");
        let map = v.as_object().expect("map");
        assert_eq!(v["smart_order_type"], "GTT");
        assert_eq!(v["segment"], "CASH");
        assert_eq!(v["quantity"], 20);
        for absent in ["trigger_price", "trigger_direction", "order", "child_legs"] {
            assert!(!map.contains_key(absent), "{absent} must be skipped");
        }

        let oco = build_modify_body(
            SmartOrderType::Oco,
            GrowwSegment::Fno,
            &SmartModifyFields {
                target_trigger_price: Some(12_200),
                ..Default::default()
            },
        );
        let v = serde_json::to_value(&oco).expect("serialize");
        assert_eq!(v["target"]["trigger_price"], "122.00"); // doc's partial-leg patch
        assert!(!v.as_object().expect("map").contains_key("stop_loss"));
        assert!(!v.as_object().expect("map").contains_key("quantity"));
    }

    #[test]
    fn test_smart_order_payload_tolerates_minimal_and_full_bodies() {
        // The cancel echo (a documented SUBSET of the full object).
        let echo: SmartOrderPayload = serde_json::from_str(
            r#"{"smart_order_id":"gtt_91a7f4","smart_order_type":"GTT","status":"CANCELLED"}"#,
        )
        .expect("subset parses");
        assert_eq!(echo.smart_order_id.as_deref(), Some("gtt_91a7f4"));
        assert_eq!(echo.status.as_deref(), Some("CANCELLED"));
        // A drifted price format NEVER fails the parse (raw strings).
        let full: SmartOrderPayload = serde_json::from_str(
            r#"{"smart_order_id":"oco_1","status":"ACTIVE","quantity":75,
                "target":{"trigger_price":"122.000000","order_type":"LIMIT"},
                "ltp":121.35,"child_legs":null,"unknown_future_field":1}"#,
        )
        .expect("tolerant parse");
        assert_eq!(full.quantity, Some(75));
        assert_eq!(
            full.target.expect("target").trigger_price.as_deref(),
            Some("122.000000")
        );
        // Empty list payload tolerated.
        let list: SmartOrderListPayload = serde_json::from_str("{}").expect("empty list");
        assert!(list.orders.is_none());
    }

    // -- list query -----------------------------------------------------------

    #[test]
    fn test_list_query_is_explicit_and_clamped() {
        let q = SmartOrderListQuery::active(SmartOrderType::Gtt);
        let pairs = q.to_query_pairs();
        // Explicit flow + status (the server defaults are OCO/ACTIVE — never
        // relied on).
        assert!(pairs.contains(&("smart_order_type", "GTT".to_owned())));
        assert!(pairs.contains(&("status", "ACTIVE".to_owned())));
        let q = SmartOrderListQuery {
            segment: Some(GrowwSegment::Fno),
            smart_order_type: SmartOrderType::Oco,
            status: "CANCELLED",
            page: 9_999,
            page_size: 0,
        };
        let pairs = q.to_query_pairs();
        assert!(pairs.contains(&("segment", "FNO".to_owned())));
        assert!(pairs.contains(&("page", "500".to_owned()))); // clamped
        assert!(pairs.contains(&("page_size", "1".to_owned()))); // clamped
        let q = SmartOrderListQuery {
            page_size: 500,
            ..SmartOrderListQuery::active(SmartOrderType::Oco)
        };
        assert!(q.to_query_pairs().contains(&("page_size", "50".to_owned())));
    }

    // -- status parse + FSM totality ------------------------------------------

    #[test]
    fn test_status_parse_case_insensitive_and_open_set() {
        assert_eq!(
            SmartOrderStatus::parse(" active "),
            SmartOrderStatus::Active
        );
        assert_eq!(
            SmartOrderStatus::parse("Triggered"),
            SmartOrderStatus::Triggered
        );
        assert_eq!(
            SmartOrderStatus::parse("CANCELLED"),
            SmartOrderStatus::Cancelled
        );
        assert_eq!(
            SmartOrderStatus::parse("expired"),
            SmartOrderStatus::Expired
        );
        assert_eq!(SmartOrderStatus::parse("FAILED"), SmartOrderStatus::Failed);
        assert_eq!(
            SmartOrderStatus::parse("completed"),
            SmartOrderStatus::Completed
        );
        // Open set: raw preserved, fixed metric label.
        let u = SmartOrderStatus::parse("PARTIALLY_TRIGGERED");
        assert_eq!(
            u,
            SmartOrderStatus::Unknown("PARTIALLY_TRIGGERED".to_owned())
        );
        assert_eq!(u.as_str(), "UNKNOWN");
        assert!(!u.is_terminal());
    }

    fn known_statuses() -> Vec<SmartOrderStatus> {
        vec![
            SmartOrderStatus::Active,
            SmartOrderStatus::Triggered,
            SmartOrderStatus::Cancelled,
            SmartOrderStatus::Expired,
            SmartOrderStatus::Failed,
            SmartOrderStatus::Completed,
        ]
    }

    #[test]
    fn test_fsm_totality_over_every_pair() {
        let unknown = SmartOrderStatus::Unknown("X".to_owned());
        let mut all = known_statuses();
        all.push(unknown.clone());
        for current in &all {
            for observed in &all {
                let out = evaluate_smart_transition(current, observed);
                // Totality: every pair lands on exactly one verdict.
                if matches!(observed, SmartOrderStatus::Unknown(_)) {
                    assert_eq!(
                        out,
                        SmartTransitionOutcome::Park,
                        "{current:?}->{observed:?}"
                    );
                } else if matches!(current, SmartOrderStatus::Unknown(_)) {
                    assert_eq!(out, SmartTransitionOutcome::Transition);
                } else if current == observed {
                    assert_eq!(out, SmartTransitionOutcome::SameStatusRefresh);
                } else if current.is_terminal() {
                    // Out-of-terminal (incl. terminal->terminal) is illegal.
                    assert_eq!(
                        out,
                        SmartTransitionOutcome::Park,
                        "{current:?}->{observed:?}"
                    );
                } else {
                    let fwd = observed.rank() > current.rank();
                    let expected = if fwd {
                        SmartTransitionOutcome::Transition
                    } else {
                        SmartTransitionOutcome::Park
                    };
                    assert_eq!(out, expected, "{current:?}->{observed:?}");
                }
            }
        }
    }

    #[test]
    fn test_fsm_poll_skip_active_to_completed_is_one_legal_edge() {
        // A 15s poller can skip TRIGGERED entirely.
        assert_eq!(
            evaluate_smart_transition(&SmartOrderStatus::Active, &SmartOrderStatus::Completed),
            SmartTransitionOutcome::Transition
        );
        // Backward TRIGGERED -> ACTIVE parks.
        assert_eq!(
            evaluate_smart_transition(&SmartOrderStatus::Triggered, &SmartOrderStatus::Active),
            SmartTransitionOutcome::Park
        );
    }

    // -- gates + id plausibility ----------------------------------------------

    #[test]
    fn test_smart_live_send_permitted_is_false_today_in_every_combination() {
        // Gate 3 (GROWW_ORDER_LIVE_FIRE) is false — no config combination can
        // open the live write gate.
        for a in [false, true] {
            for b in [false, true] {
                assert!(!smart_live_send_permitted(a, b), "({a},{b})");
            }
        }
    }

    #[test]
    fn test_gates_disabled_is_fail_closed() {
        let g = SmartOrderGates::disabled();
        assert!(!g.smart_orders_read);
        assert!(!g.smart_orders_write);
        assert!(!g.live_fire_requested);
        assert_eq!(g.max_order_quantity, 0);
    }

    #[test]
    fn test_smart_order_id_path_plausibility() {
        assert!(is_plausible_smart_order_id("gtt_91a7f4"));
        assert!(is_plausible_smart_order_id("oco-1_A"));
        assert!(is_plausible_smart_order_id("PAPER-TV2607160001ABCD"));
        assert!(!is_plausible_smart_order_id(""));
        assert!(!is_plausible_smart_order_id(&"a".repeat(65)));
        for hostile in ["a/b", "../x", "a?x=1", "a b", "a\u{202E}b", "a%2f"] {
            assert!(!is_plausible_smart_order_id(hostile), "{hostile:?}");
        }
    }

    // -- create validation (financial boundaries) ------------------------------

    #[test]
    fn test_validate_gtt_create_boundaries() {
        let ok = SmartOrderCreate::Gtt(gtt_req());
        assert!(validate_create_smart_order(&ok, 10).is_ok()); // qty == max
        // Quantity boundaries.
        let mut g = gtt_req();
        g.quantity = 0;
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_err());
        let mut g = gtt_req();
        g.quantity = 11;
        assert!(matches!(
            validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10),
            Err(SmartOrderError::QuantityRefused {
                quantity: 11,
                max: 10
            })
        ));
        // max_order_quantity = 0 refuses ALL (the fail-closed default).
        assert!(matches!(
            validate_create_smart_order(&SmartOrderCreate::Gtt(gtt_req()), 0),
            Err(SmartOrderError::QuantityRefused { .. })
        ));
        // Trigger price boundaries.
        for bad in [0, -1] {
            let mut g = gtt_req();
            g.trigger_price = bad;
            assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_err());
        }
        let mut g = gtt_req();
        g.trigger_price = 1;
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_ok());
        // Leg price shape: MARKET must be null; LIMIT must be > 0.
        let mut g = gtt_req();
        g.order.price = Some(100);
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_err());
        let mut g = gtt_req();
        g.order.order_type = GrowwOrderType::Limit;
        g.order.price = None;
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_err());
        let mut g = gtt_req();
        g.order.order_type = GrowwOrderType::Limit;
        g.order.price = Some(0);
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_err());
        let mut g = gtt_req();
        g.order.order_type = GrowwOrderType::Sl;
        g.order.price = Some(1);
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_ok());
        // Wrong discriminant / empty symbol / bad reference id.
        let mut g = gtt_req();
        g.smart_order_type = SmartOrderType::Oco;
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_err());
        let mut g = gtt_req();
        g.trading_symbol = "  ".to_owned();
        assert!(validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10).is_err());
        let mut g = gtt_req();
        g.reference_id = "no".to_owned();
        assert!(matches!(
            validate_create_smart_order(&SmartOrderCreate::Gtt(g), 10),
            Err(SmartOrderError::InvalidReferenceId(_))
        ));
    }

    #[test]
    fn test_validate_oco_create_exit_only_and_cash_mis_boundaries() {
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(oco_req()), 100).is_ok());
        // Exit-only: zero net position refused.
        let mut o = oco_req();
        o.net_position_quantity = 0;
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_err());
        // qty > |net| refused; qty == |net| ok (short position too).
        let mut o = oco_req();
        o.net_position_quantity = -75;
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_ok());
        let mut o = oco_req();
        o.quantity = 76;
        assert!(matches!(
            validate_create_smart_order(&SmartOrderCreate::Oco(o), 100),
            Err(SmartOrderError::QuantityExceedsPosition {
                quantity: 76,
                net_position_quantity: 75
            })
        ));
        // CASH OCO: MIS-only (both doc arms agree; availability = probe P9).
        let mut o = oco_req();
        o.segment = GrowwSegment::Cash;
        assert!(matches!(
            validate_create_smart_order(&SmartOrderCreate::Oco(o), 100),
            Err(SmartOrderError::CashOcoProductNotMis { product: "NRML" })
        ));
        let mut o = oco_req();
        o.segment = GrowwSegment::Cash;
        o.product_type = GrowwProduct::Mis;
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_ok());
        // Leg boundaries: SL requires price; SL_M requires null; LIMIT target
        // requires price; MARKET target requires null.
        let mut o = oco_req();
        o.stop_loss.order_type = StopLossLegOrderType::Sl;
        o.stop_loss.price = None;
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_err());
        let mut o = oco_req();
        o.stop_loss.price = Some(9_700);
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_err());
        let mut o = oco_req();
        o.target.order_type = TargetLegOrderType::Market;
        o.target.price = None;
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_ok());
        // Trigger boundaries.
        let mut o = oco_req();
        o.target.trigger_price = 0;
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_err());
        let mut o = oco_req();
        o.stop_loss.trigger_price = -1;
        assert!(validate_create_smart_order(&SmartOrderCreate::Oco(o), 100).is_err());
    }

    // -- per-type modify matrix TOTALITY ---------------------------------------

    /// Every `SmartModifyFields` field, exercised one-at-a-time against BOTH
    /// flows — the full 9x2 matrix of the doc's modify tables.
    #[test]
    fn test_modify_matrix_totality_gtt_vs_oco() {
        let legal_leg = GttOrderLeg {
            order_type: GrowwOrderType::Limit,
            price: Some(100),
            transaction_type: GrowwTransactionType::Buy,
        };
        // (field-setter, allowed-on-GTT, allowed-on-OCO)
        let cases: Vec<(&str, SmartModifyFields, bool, bool)> = vec![
            (
                "quantity",
                SmartModifyFields {
                    quantity: Some(1),
                    ..Default::default()
                },
                true,
                true,
            ),
            (
                "trigger_price",
                SmartModifyFields {
                    trigger_price: Some(1),
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                "trigger_direction",
                SmartModifyFields {
                    trigger_direction: Some(TriggerDirection::Down),
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                "order_leg",
                SmartModifyFields {
                    order_leg: Some(legal_leg),
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                "child_legs",
                SmartModifyFields {
                    child_legs: Some(serde_json::json!({})),
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                "duration",
                SmartModifyFields {
                    duration: Some(GrowwValidity::Day),
                    ..Default::default()
                },
                false,
                true,
            ),
            (
                "product_type",
                SmartModifyFields {
                    product_type: Some(GrowwProduct::Mis),
                    ..Default::default()
                },
                false,
                true,
            ),
            (
                "target_trigger_price",
                SmartModifyFields {
                    target_trigger_price: Some(1),
                    ..Default::default()
                },
                false,
                true,
            ),
            (
                "stop_loss_trigger_price",
                SmartModifyFields {
                    stop_loss_trigger_price: Some(1),
                    ..Default::default()
                },
                false,
                true,
            ),
        ];
        for (name, fields, gtt_ok, oco_ok) in cases {
            let g = validate_modify_fields(SmartOrderType::Gtt, GrowwSegment::Fno, &fields, 100);
            let o = validate_modify_fields(SmartOrderType::Oco, GrowwSegment::Fno, &fields, 100);
            assert_eq!(g.is_ok(), gtt_ok, "GTT {name}: {g:?}");
            assert_eq!(o.is_ok(), oco_ok, "OCO {name}: {o:?}");
            // A refusal is the TYPED immutable-field error, never a generic.
            if !gtt_ok {
                assert!(
                    matches!(
                        g,
                        Err(SmartOrderError::ImmutableField {
                            smart_order_type: "GTT",
                            ..
                        })
                    ),
                    "GTT {name}"
                );
            }
            if !oco_ok {
                assert!(
                    matches!(
                        o,
                        Err(SmartOrderError::ImmutableField {
                            smart_order_type: "OCO",
                            ..
                        })
                    ),
                    "OCO {name}"
                );
            }
        }
    }

    #[test]
    fn test_modify_validation_boundaries() {
        // Empty modify refused.
        assert!(
            validate_modify_fields(
                SmartOrderType::Gtt,
                GrowwSegment::Fno,
                &SmartModifyFields::default(),
                100
            )
            .is_err()
        );
        // Quantity boundaries (both flows share them).
        for (q, ok) in [(0, false), (1, true), (100, true), (101, false)] {
            let f = SmartModifyFields {
                quantity: Some(q),
                ..Default::default()
            };
            assert_eq!(
                validate_modify_fields(SmartOrderType::Oco, GrowwSegment::Fno, &f, 100).is_ok(),
                ok,
                "quantity {q}"
            );
        }
        // Price boundaries on the flow-legal fields.
        let f = SmartModifyFields {
            trigger_price: Some(0),
            ..Default::default()
        };
        assert!(validate_modify_fields(SmartOrderType::Gtt, GrowwSegment::Fno, &f, 100).is_err());
        let f = SmartModifyFields {
            target_trigger_price: Some(0),
            ..Default::default()
        };
        assert!(validate_modify_fields(SmartOrderType::Oco, GrowwSegment::Fno, &f, 100).is_err());
        let f = SmartModifyFields {
            stop_loss_trigger_price: Some(-1),
            ..Default::default()
        };
        assert!(validate_modify_fields(SmartOrderType::Oco, GrowwSegment::Fno, &f, 100).is_err());
        // A GTT leg patch is shape-validated (MARKET leg with a price refused).
        let f = SmartModifyFields {
            order_leg: Some(GttOrderLeg {
                order_type: GrowwOrderType::Market,
                price: Some(1),
                transaction_type: GrowwTransactionType::Buy,
            }),
            ..Default::default()
        };
        assert!(validate_modify_fields(SmartOrderType::Gtt, GrowwSegment::Fno, &f, 100).is_err());
    }

    // -- adversarial round 2, LOW-2: modify re-asserts CASH-OCO MIS-only -------

    #[test]
    fn test_modify_cash_oco_product_change_must_be_mis() {
        // CASH OCO: a product_type change to a non-MIS value is refused
        // (mirrors create's CashOcoProductNotMis).
        let bad = SmartModifyFields {
            product_type: Some(GrowwProduct::Nrml),
            ..Default::default()
        };
        assert!(matches!(
            validate_modify_fields(SmartOrderType::Oco, GrowwSegment::Cash, &bad, 100),
            Err(SmartOrderError::CashOcoProductNotMis { product: "NRML" })
        ));
        // CASH OCO with MIS is fine.
        let ok = SmartModifyFields {
            product_type: Some(GrowwProduct::Mis),
            ..Default::default()
        };
        assert!(validate_modify_fields(SmartOrderType::Oco, GrowwSegment::Cash, &ok, 100).is_ok());
        // FNO OCO: NRML product change is allowed (the CASH rule is
        // segment-scoped) — unchanged behavior.
        assert!(
            validate_modify_fields(SmartOrderType::Oco, GrowwSegment::Fno, &bad, 100).is_ok(),
            "FNO OCO product change must NOT trip the CASH-only rule"
        );
    }

    // -- sibling verify (paused-clock pure core) --------------------------------

    #[test]
    fn test_sibling_verify_edges() {
        let deadline = 30_u64;
        // Not armed.
        assert_eq!(
            evaluate_sibling_verify(None, 1_000, deadline, &SmartOrderStatus::Active),
            SiblingVerifyVerdict::NotArmed
        );
        // Terminal is Verified even mid-episode.
        assert_eq!(
            evaluate_sibling_verify(Some(0), 999_999, deadline, &SmartOrderStatus::Completed),
            SiblingVerifyVerdict::Verified
        );
        // Exactly AT the deadline is still Pending (strict >).
        assert_eq!(
            evaluate_sibling_verify(Some(0), 30_000, deadline, &SmartOrderStatus::Triggered),
            SiblingVerifyVerdict::Pending
        );
        // One ms past pages.
        assert_eq!(
            evaluate_sibling_verify(Some(0), 30_001, deadline, &SmartOrderStatus::Triggered),
            SiblingVerifyVerdict::DeadlineExceeded { elapsed_ms: 30_001 }
        );
        // A backwards clock never yields a negative elapsed.
        assert_eq!(
            evaluate_sibling_verify(Some(1_000), 500, deadline, &SmartOrderStatus::Triggered),
            SiblingVerifyVerdict::Pending
        );
        // A huge deadline never overflows.
        assert_eq!(
            evaluate_sibling_verify(Some(0), i64::MAX, u64::MAX, &SmartOrderStatus::Triggered),
            SiblingVerifyVerdict::Pending
        );
    }

    // -- classification + observation application -------------------------------

    fn tracked(id: &str, ty: SmartOrderType) -> TrackedSmartOrder {
        TrackedSmartOrder::new(
            id.to_owned(),
            ty,
            GrowwSegment::Fno,
            "NIFTY26JUL28500CE".to_owned(),
            75,
            "TV2607160009ABCD".to_owned(),
        )
    }

    #[test]
    fn test_classify_smart_observation_findings() {
        let t = tracked("oco_1", SmartOrderType::Oco);
        // Quantity drift.
        let (_, f) = classify_smart_observation(&t, &SmartOrderStatus::Active, Some(50), None);
        assert!(f.iter().any(|x| x.kind == SmartFindingKind::QuantityDrift));
        // Unknown status parks + finding.
        let (out, f) = classify_smart_observation(
            &t,
            &SmartOrderStatus::Unknown("WEIRD".to_owned()),
            Some(75),
            None,
        );
        assert_eq!(out, SmartTransitionOutcome::Park);
        assert!(f.iter().any(|x| x.kind == SmartFindingKind::UnknownStatus));
        // Regression finding on a backward move.
        let mut trig = tracked("oco_2", SmartOrderType::Oco);
        trig.status = SmartOrderStatus::Triggered;
        let (out, f) = classify_smart_observation(&trig, &SmartOrderStatus::Active, Some(75), None);
        assert_eq!(out, SmartTransitionOutcome::Park);
        assert!(
            f.iter()
                .any(|x| x.kind == SmartFindingKind::StatusRegression)
        );
        // OCO qty > |net| when a position map is supplied.
        let (_, f) = classify_smart_observation(&t, &SmartOrderStatus::Active, Some(75), Some(-50));
        assert!(
            f.iter()
                .any(|x| x.kind == SmartFindingKind::QtyExceedsPosition)
        );
        // A GTT never raises the position finding.
        let g = tracked("gtt_1", SmartOrderType::Gtt);
        let (_, f) = classify_smart_observation(&g, &SmartOrderStatus::Active, Some(75), Some(0));
        assert!(
            !f.iter()
                .any(|x| x.kind == SmartFindingKind::QtyExceedsPosition)
        );
    }

    // -- adversarial round 1, finding 4: i64::MIN never aborts --------------

    #[test]
    fn test_i64_min_net_position_never_aborts() {
        // Reconcile classify: OCO qty vs an i64::MIN net position — `abs()`
        // would abort under overflow-checks; unsigned_abs must not (the same
        // arithmetic guards the create-side validate).
        let mut t = tracked("oco_min", SmartOrderType::Oco);
        t.quantity = 50;
        let (_, f) =
            classify_smart_observation(&t, &SmartOrderStatus::Active, Some(50), Some(i64::MIN));
        // |i64::MIN| = 2^63 > 50 — no exceed finding, and no abort.
        assert!(
            !f.iter()
                .any(|x| x.kind == SmartFindingKind::QtyExceedsPosition)
        );
    }

    // -- adversarial round 1, finding 11: drift latch ------------------------

    #[test]
    fn test_quantity_drift_latch_fires_once_per_value() {
        let deadline = 30_u64;
        let mut t = tracked("gtt_drift", SmartOrderType::Gtt);
        t.quantity = 100;
        // First SameStatusRefresh pass with a drifted qty: finding fires.
        let (out, f) = classify_smart_observation(&t, &SmartOrderStatus::Active, Some(75), None);
        assert_eq!(out, SmartTransitionOutcome::SameStatusRefresh);
        assert!(f.iter().any(|x| x.kind == SmartFindingKind::QuantityDrift));
        apply_observation(
            &mut t,
            &SmartOrderStatus::Active,
            Some(75),
            out,
            1_000,
            deadline,
        );
        assert_eq!(t.drift_flagged_qty, Some(75));
        // Second pass, SAME drifted qty: latched — no re-finding.
        let (_, f) = classify_smart_observation(&t, &SmartOrderStatus::Active, Some(75), None);
        assert!(!f.iter().any(|x| x.kind == SmartFindingKind::QuantityDrift));
        // A NEW drifted value re-fires.
        let (out, f) = classify_smart_observation(&t, &SmartOrderStatus::Active, Some(60), None);
        assert!(f.iter().any(|x| x.kind == SmartFindingKind::QuantityDrift));
        apply_observation(
            &mut t,
            &SmartOrderStatus::Active,
            Some(60),
            out,
            2_000,
            deadline,
        );
        assert_eq!(t.drift_flagged_qty, Some(60));
        // A Transition ADOPTING the broker qty clears the latch.
        let (out, _) = classify_smart_observation(&t, &SmartOrderStatus::Triggered, Some(60), None);
        assert_eq!(out, SmartTransitionOutcome::Transition);
        apply_observation(
            &mut t,
            &SmartOrderStatus::Triggered,
            Some(60),
            out,
            3_000,
            deadline,
        );
        assert_eq!(t.quantity, 60);
        assert_eq!(t.drift_flagged_qty, None);
    }

    // -- adversarial round 1, finding 12: request-side upper band ------------

    #[test]
    fn test_price_upper_band_refused_before_serialization() {
        // require_positive_paise: i64::MAX and band+1 refused; band edge ok.
        assert!(require_positive_paise("p", i64::MAX).is_err());
        assert!(require_positive_paise("p", MAX_ABS_PAISE_FOR_DECIMAL_STRING + 1).is_err());
        assert!(require_positive_paise("p", MAX_ABS_PAISE_FOR_DECIMAL_STRING).is_ok());
        // validate_leg_price_shape: same band on price-carrying legs.
        assert!(validate_leg_price_shape("leg", true, "LIMIT", Some(i64::MAX)).is_err());
        assert!(
            validate_leg_price_shape(
                "leg",
                true,
                "LIMIT",
                Some(MAX_ABS_PAISE_FOR_DECIMAL_STRING + 1)
            )
            .is_err()
        );
        assert!(
            validate_leg_price_shape("leg", true, "LIMIT", Some(MAX_ABS_PAISE_FOR_DECIMAL_STRING))
                .is_ok()
        );
    }

    // -- adversarial round 1, finding 3: log-safe id --------------------------

    #[test]
    fn test_log_safe_id_passes_plausible_and_sanitizes_hostile() {
        assert_eq!(log_safe_id("GMOCO-1_a"), "GMOCO-1_a");
        let hostile = format!("x\u{0}y{}", "z".repeat(400));
        let safe = log_safe_id(&hostile);
        assert!(!safe.contains('\u{0}'), "control chars stripped");
        assert!(safe.len() < hostile.len(), "length-capped");
    }

    #[test]
    fn test_apply_observation_arms_pages_and_closes_the_oco_episode() {
        let deadline = 30_u64;
        let mut t = tracked("oco_1", SmartOrderType::Oco);
        // TRIGGERED observation arms the episode at now_ms.
        let v = apply_observation(
            &mut t,
            &SmartOrderStatus::Triggered,
            Some(75),
            SmartTransitionOutcome::Transition,
            10_000,
            deadline,
        );
        assert_eq!(t.triggered_at_ms, Some(10_000));
        assert_eq!(v, SiblingVerifyVerdict::Pending);
        // Past the deadline: exceeded (the caller latches + pages once).
        let v = apply_observation(
            &mut t,
            &SmartOrderStatus::Triggered,
            Some(75),
            SmartTransitionOutcome::SameStatusRefresh,
            50_000,
            deadline,
        );
        assert_eq!(
            v,
            SiblingVerifyVerdict::DeadlineExceeded { elapsed_ms: 40_000 }
        );
        t.sibling_unverified_paged = true;
        // Terminal settles: Verified + episode closed (latch re-armed).
        let v = apply_observation(
            &mut t,
            &SmartOrderStatus::Completed,
            Some(75),
            SmartTransitionOutcome::Transition,
            60_000,
            deadline,
        );
        assert_eq!(v, SiblingVerifyVerdict::Verified);
        assert!(!t.sibling_unverified_paged);
        assert_eq!(t.triggered_at_ms, None);
        assert_eq!(t.status, SmartOrderStatus::Completed);
        // A GTT never arms.
        let mut g = tracked("gtt_1", SmartOrderType::Gtt);
        let v = apply_observation(
            &mut g,
            &SmartOrderStatus::Triggered,
            None,
            SmartTransitionOutcome::Transition,
            10_000,
            deadline,
        );
        assert_eq!(v, SiblingVerifyVerdict::NotArmed);
        assert_eq!(g.triggered_at_ms, None);
    }

    // -- reconcile_pass against a scripted transport ------------------------------

    #[derive(Default)]
    struct SmartScript {
        get: StdMutex<Vec<TransportOutcome<SmartOrderPayload>>>,
        list: StdMutex<Vec<TransportOutcome<SmartOrderListPayload>>>,
        get_ids: StdMutex<Vec<String>>,
    }

    impl SmartOrderTransport for SmartScript {
        async fn create_smart_order(
            &self,
            _req: &SmartOrderCreate,
            _token: &SecretString,
            _receipt: &IntentReceipt,
        ) -> TransportOutcome<SmartOrderPayload> {
            TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
        }
        async fn modify_smart_order(
            &self,
            _smart_order_id: &str,
            _body: &SmartModifyBody,
            _token: &SecretString,
            _receipt: &IntentReceipt,
        ) -> TransportOutcome<SmartOrderPayload> {
            TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
        }
        async fn cancel_smart_order(
            &self,
            _segment: GrowwSegment,
            _smart_order_type: SmartOrderType,
            _smart_order_id: &str,
            _token: &SecretString,
            _receipt: &IntentReceipt,
        ) -> TransportOutcome<SmartOrderPayload> {
            TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
        }
        async fn get_smart_order(
            &self,
            _segment: GrowwSegment,
            _smart_order_type: SmartOrderType,
            smart_order_id: &str,
            _token: &SecretString,
        ) -> TransportOutcome<SmartOrderPayload> {
            self.get_ids.lock().unwrap().push(smart_order_id.to_owned());
            let mut q = self.get.lock().unwrap();
            if q.is_empty() {
                TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
            } else {
                q.remove(0)
            }
        }
        async fn list_smart_orders(
            &self,
            _query: &SmartOrderListQuery,
            _token: &SecretString,
        ) -> TransportOutcome<SmartOrderListPayload> {
            let mut q = self.list.lock().unwrap();
            if q.is_empty() {
                TransportOutcome::Success(SmartOrderListPayload { orders: None })
            } else {
                q.remove(0)
            }
        }
    }

    fn payload(id: &str, status: &str, qty: i64) -> TransportOutcome<SmartOrderPayload> {
        TransportOutcome::Success(SmartOrderPayload {
            smart_order_id: Some(id.to_owned()),
            smart_order_type: None,
            status: Some(status.to_owned()),
            trading_symbol: None,
            exchange: None,
            quantity: Some(qty),
            product_type: None,
            duration: None,
            trigger_price: None,
            trigger_direction: None,
            order: None,
            target: None,
            stop_loss: None,
            ltp: None,
            remark: None,
            display_name: None,
            is_cancellation_allowed: None,
            is_modification_allowed: None,
            created_at: None,
            expire_at: None,
            triggered_at: None,
            updated_at: None,
        })
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("rt")
    }

    #[test]
    fn test_reconcile_pass_arms_then_pages_oco02_once_per_episode() {
        let mut book = SmartOrderBook::default();
        book.insert(tracked("oco_1", SmartOrderType::Oco));
        let tok = SecretString::from("t");
        let positions = HashMap::new();
        rt().block_on(async {
            // Pass 1: TRIGGERED arms the episode at now=10_000.
            let t = SmartScript::default();
            t.get
                .lock()
                .unwrap()
                .push(payload("oco_1", "TRIGGERED", 75));
            let r = reconcile_pass(&t, &tok, &mut book, 30, 10_000, &positions).await;
            assert!(r.sibling_unverified.is_empty());
            assert_eq!(r.polled, 1);
            // Pass 2: still TRIGGERED past the deadline — pages ONCE.
            let t = SmartScript::default();
            t.get
                .lock()
                .unwrap()
                .push(payload("oco_1", "TRIGGERED", 75));
            let r = reconcile_pass(&t, &tok, &mut book, 30, 50_000, &positions).await;
            assert_eq!(r.sibling_unverified, vec!["oco_1".to_owned()]);
            // Pass 3: latched — never re-pages the same episode.
            let t = SmartScript::default();
            t.get
                .lock()
                .unwrap()
                .push(payload("oco_1", "TRIGGERED", 75));
            let r = reconcile_pass(&t, &tok, &mut book, 30, 90_000, &positions).await;
            assert!(r.sibling_unverified.is_empty());
            // Pass 4: settled terminal closes the episode (Verified).
            let t = SmartScript::default();
            t.get
                .lock()
                .unwrap()
                .push(payload("oco_1", "COMPLETED", 75));
            let r = reconcile_pass(&t, &tok, &mut book, 30, 95_000, &positions).await;
            assert!(r.findings.is_empty());
            assert!(!book.get("oco_1").unwrap().sibling_unverified_paged);
        });
    }

    #[test]
    fn test_reconcile_pass_deadline_sweep_pages_when_get_is_unreadable() {
        // A GET failure must not silence the exposure page: the tracked order
        // is armed + past deadline, transport ambiguous.
        let mut book = SmartOrderBook::default();
        let mut t0 = tracked("oco_1", SmartOrderType::Oco);
        t0.status = SmartOrderStatus::Triggered;
        t0.triggered_at_ms = Some(0);
        book.insert(t0);
        let tok = SecretString::from("t");
        rt().block_on(async {
            let t = SmartScript::default(); // GET queue empty -> Timeout
            let r = reconcile_pass(&t, &tok, &mut book, 30, 60_000, &HashMap::new()).await;
            assert_eq!(r.sibling_unverified, vec!["oco_1".to_owned()]);
            assert!(r.degraded.contains(&"transport"));
        });
    }

    #[test]
    fn test_reconcile_pass_ghost_local_needs_two_sweeps() {
        let mut book = SmartOrderBook::default();
        book.insert(tracked("gtt_9", SmartOrderType::Gtt));
        let tok = SecretString::from("t");
        let rejected_404 = || TransportOutcome::Rejected {
            http_status: 404,
            ga_code: Some("GA004".to_owned()),
            message: None,
        };
        rt().block_on(async {
            let t = SmartScript::default();
            t.get.lock().unwrap().push(rejected_404());
            let r = reconcile_pass(&t, &tok, &mut book, 30, 1_000, &HashMap::new()).await;
            assert!(r.findings.is_empty(), "one missing sweep is grace");
            let t = SmartScript::default();
            t.get.lock().unwrap().push(rejected_404());
            let r = reconcile_pass(&t, &tok, &mut book, 30, 2_000, &HashMap::new()).await;
            assert!(
                r.findings
                    .iter()
                    .any(|f| f.kind == SmartFindingKind::GhostLocal)
            );
            // A successful read resets the ghost counter.
            let t = SmartScript::default();
            t.get.lock().unwrap().push(payload("gtt_9", "ACTIVE", 75));
            let _ = reconcile_pass(&t, &tok, &mut book, 30, 3_000, &HashMap::new()).await;
            assert_eq!(book.get("gtt_9").unwrap().missing_sweeps, 0);
        });
    }

    #[test]
    fn test_reconcile_pass_auth_stale_aborts_and_rate_limit_never_out_polls() {
        let mut book = SmartOrderBook::default();
        book.insert(tracked("gtt_1", SmartOrderType::Gtt));
        book.insert(tracked("gtt_2", SmartOrderType::Gtt));
        let tok = SecretString::from("t");
        rt().block_on(async {
            let t = SmartScript::default();
            t.get
                .lock()
                .unwrap()
                .push(TransportOutcome::AuthStale { http_status: 401 });
            let r = reconcile_pass(&t, &tok, &mut book, 30, 1_000, &HashMap::new()).await;
            // Aborted after the first poll — the second id was never fetched,
            // and the list leg is skipped.
            assert_eq!(r.polled, 1);
            assert!(r.degraded.contains(&"token"));
            assert_eq!(t.get_ids.lock().unwrap().len(), 1);
            assert_eq!(r.foreign_untracked, 0);

            let t = SmartScript::default();
            t.get.lock().unwrap().push(TransportOutcome::RateLimited {
                http_status: 429,
                retry_after_secs: Some(3),
                body_excerpt: None,
            });
            let r = reconcile_pass(&t, &tok, &mut book, 30, 2_000, &HashMap::new()).await;
            assert!(r.degraded.contains(&"rate_limited"));
            assert_eq!(t.get_ids.lock().unwrap().len(), 1, "never out-polled");
        });
    }

    #[test]
    fn test_reconcile_pass_counts_foreign_untracked_never_acts() {
        let mut book = SmartOrderBook::default();
        book.insert(tracked("oco_mine", SmartOrderType::Oco));
        let tok = SecretString::from("t");
        rt().block_on(async {
            let t = SmartScript::default();
            t.get
                .lock()
                .unwrap()
                .push(payload("oco_mine", "ACTIVE", 75));
            // GTT list: one foreign row; OCO list: our own row (not foreign).
            let foreign = match payload("gtt_cotenant", "ACTIVE", 10) {
                TransportOutcome::Success(p) => p,
                _ => unreachable!(),
            };
            let mine = match payload("oco_mine", "ACTIVE", 75) {
                TransportOutcome::Success(p) => p,
                _ => unreachable!(),
            };
            t.list
                .lock()
                .unwrap()
                .push(TransportOutcome::Success(SmartOrderListPayload {
                    orders: Some(vec![foreign]),
                }));
            t.list
                .lock()
                .unwrap()
                .push(TransportOutcome::Success(SmartOrderListPayload {
                    orders: Some(vec![mine]),
                }));
            let r = reconcile_pass(&t, &tok, &mut book, 30, 1_000, &HashMap::new()).await;
            assert_eq!(r.foreign_untracked, 1);
            // The foreign id was never adopted into the book.
            assert!(!book.contains("gtt_cotenant"));
        });
    }

    #[test]
    fn test_reconcile_pass_refuses_implausible_id_before_any_url() {
        let mut book = SmartOrderBook::default();
        book.insert(tracked("bad/../id", SmartOrderType::Gtt));
        let tok = SecretString::from("t");
        rt().block_on(async {
            let t = SmartScript::default();
            let r = reconcile_pass(&t, &tok, &mut book, 30, 1_000, &HashMap::new()).await;
            assert!(r.degraded.contains(&"implausible_id"));
            assert_eq!(r.polled, 0);
            assert!(t.get_ids.lock().unwrap().is_empty(), "no transport call");
        });
    }

    // -- book + ambiguity stage ---------------------------------------------------

    #[test]
    fn test_smart_order_book_open_count_and_non_terminal_ids() {
        let mut book = SmartOrderBook::default();
        book.insert(tracked("b", SmartOrderType::Gtt));
        book.insert(tracked("a", SmartOrderType::Oco));
        let mut done = tracked("z", SmartOrderType::Gtt);
        done.status = SmartOrderStatus::Completed;
        book.insert(done);
        assert_eq!(book.open_count(), 2);
        assert_eq!(
            book.non_terminal_ids(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(book.contains("z"));
        assert!(!book.contains("missing"));
    }

    #[test]
    fn test_ambiguity_stage_covers_every_reason() {
        for (reason, label) in [
            (AmbiguityReason::ConnectPhase, "connect_phase"),
            (AmbiguityReason::Timeout, "timeout"),
            (AmbiguityReason::ServerError(503), "server_error"),
            (AmbiguityReason::Decode, "decode"),
            (
                AmbiguityReason::GaOnNonRejectStatus("GA003".to_owned()),
                "ga_on_non_reject_status",
            ),
            (AmbiguityReason::MissingPayload, "missing_payload"),
            (AmbiguityReason::SendFailed, "send_failed"),
        ] {
            assert_eq!(ambiguity_stage(&reason), label);
        }
    }

    // -- emit ratchet: every GROWW-OCO code has exactly one live emit site ------

    #[test]
    fn test_ratchet_all_five_oco_codes_have_coded_emit_sites() {
        let src = include_str!("smart_orders.rs");
        let prod = &src[..src.find("mod tests").expect("tests module")];
        for needle in [
            "ErrorCode::GrowwOco01PlacementFailed.code_str()",
            "ErrorCode::GrowwOco02SiblingCancelUnverified.code_str()",
            "ErrorCode::GrowwOco03ReconcileMismatch.code_str()",
            "ErrorCode::GrowwOco04ModifyRejected.code_str()",
            "ErrorCode::GrowwOco05PollerDegraded.code_str()",
        ] {
            assert_eq!(
                prod.matches(needle).count(),
                1,
                "exactly one coded emit site for {needle}"
            );
        }
        // Gate-5 discipline: NO endpoint path string in this module's code.
        assert!(!prod.contains("\"/v1/order"), "paths live in api_client.rs");
    }
}
