//! Groww regular-order wire types — request/response DTOs for the 8 regular
//! order endpoints (ORD-PR-2 pure core; NO transport here — the HTTP client
//! lands in ORD-PR-3).
//!
//! # Doc fidelity (the truth source)
//! Every field/enum/status below is field-for-field from
//! `docs/groww-ref/16-orders-margins-portfolio.md` (LIVE-DOC capture
//! 2026-07-03):
//! - endpoints §1 rows 1–8; detail/list 22-field table §2; status 5-field
//!   subset §2 note; annexure enums §4.1–§4.8; GA error envelope §5.
//! - REQUEST-side enums are CLOSED (`SL_M` underscore exact; NSE|BSE and
//!   CASH|FNO only — MCX/COMMODITY excluded per the doc's own three-way MCX
//!   contradiction, §4.3 note).
//! - RESPONSE-side enum-valued fields deserialize as raw `String` (tolerant —
//!   doc-fidelity F3), and EVERY response field is `Option` (the doc
//!   guarantees none of them on the wire; doc-fidelity F10). A 2xx SUCCESS
//!   with no usable payload stays AMBIGUOUS at the executor (ORD-PR-3).
//! - Money is integer PAISE `i64` end-to-end (the
//!   `tickvault_common::broker_order_events` seam convention +
//!   `data-integrity.md`); the wire speaks decimal rupees, converted at the
//!   serde boundary only.
//!
//! # Dhan-rule divergence (dhan/orders.md auto-loads on OMS paths — H4)
//! These types are GROWW, not Dhan: validity is `DAY` ONLY (no IOC,
//! `16` §4.8); cancel is `POST /v1/order/cancel` (not DELETE); the reference
//! id is 8–20 alnum ≤2 hyphens (not the 30-char Dhan correlationId); modify
//! `quantity` TOTAL-vs-REMAINING semantics are UNKNOWN (O-11) — Dhan
//! orders.md rule 6 does NOT apply.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::reference_id;

// ---------------------------------------------------------------------------
// Money — integer paise at rest, decimal rupees on the wire
// ---------------------------------------------------------------------------

/// Largest |rupee| magnitude admitted for paise conversion (MEDIUM-9:
/// tightened from the raw 2^53 headroom to an NSE-plausible 1e10 rupees =
/// ₹1,000 crore/unit). Inside this band every 2-decimal rupee value maps to
/// |paise| ≤ 1e12 ≪ 2^53, so `paise → rupees → paise` round-trips EXACTLY
/// across the whole admitted band — the old 9e13 bound admitted values
/// (e.g. 4,358,277,003,831,209 paise) whose f64 rupee form is no longer
/// 2-decimal-exact.
const MAX_RUPEES_ABS_FOR_PAISE: f64 = 1.0e10;

/// Convert a wire decimal-rupee value to integer paise (banker's-free
/// round-half-away-from-zero via `f64::round`). Returns `None` for
/// non-finite or out-of-range values — never panics, never wraps.
#[must_use]
pub fn rupees_f64_to_paise(rupees: f64) -> Option<i64> {
    if !rupees.is_finite() || rupees.abs() > MAX_RUPEES_ABS_FOR_PAISE {
        return None;
    }
    Some((rupees * 100.0).round() as i64)
}

/// Convert integer paise to the decimal-rupee value the Groww wire expects.
#[must_use]
pub fn paise_to_rupees_f64(paise: i64) -> f64 {
    paise as f64 / 100.0
}

/// Serialize an `Option<i64>` paise field as a decimal-rupee JSON number
/// (`None` fields are skipped by the callers' `skip_serializing_if`).
fn ser_opt_paise_as_rupees<S: Serializer>(v: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(p) => s.serialize_f64(paise_to_rupees_f64(*p)),
        None => s.serialize_none(),
    }
}

/// Deserialize an optional decimal-rupee JSON number into integer paise.
/// Absent / `null` → `None`. A present-but-unconvertible number (non-finite
/// cannot occur in JSON; out-of-range can) is a hard deserialize error —
/// fail-loud, never a silently wrong price. NEGATIVE values are also a hard
/// error (MEDIUM-9): broker prices (`price` / `trigger_price` /
/// `average_fill_price` / trade `price`) are never negative — a negative
/// wire value is corruption, not data.
fn de_opt_rupees_as_paise<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let raw: Option<f64> = Option::deserialize(d)?;
    match raw {
        None => Ok(None),
        Some(r) if r < 0.0 => Err(D::Error::custom(
            "negative price on a broker response (prices are never negative)",
        )),
        Some(r) => rupees_f64_to_paise(r)
            .map(Some)
            .ok_or_else(|| D::Error::custom("rupee value out of integer-paise range")),
    }
}

// ---------------------------------------------------------------------------
// Request-side CLOSED enums (annexure §4.3–§4.8)
// ---------------------------------------------------------------------------

/// Exchange (`16` §4.3). CLOSED: MCX is excluded per the doc's own unresolved
/// three-way contradiction (changelog vs REST intro vs Smart Orders note).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum GrowwExchange {
    /// National Stock Exchange.
    #[serde(rename = "NSE")]
    Nse,
    /// Bombay Stock Exchange.
    #[serde(rename = "BSE")]
    Bse,
}

impl GrowwExchange {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nse => "NSE",
            Self::Bse => "BSE",
        }
    }
}

/// Segment (`16` §4.4). CLOSED: COMMODITY rides the MCX contradiction and is
/// excluded from requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum GrowwSegment {
    /// Regular equity market.
    #[serde(rename = "CASH")]
    Cash,
    /// Futures and Options.
    #[serde(rename = "FNO")]
    Fno,
}

impl GrowwSegment {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cash => "CASH",
            Self::Fno => "FNO",
        }
    }
}

/// Product (`16` §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum GrowwProduct {
    /// Cash and Carry (delivery).
    #[serde(rename = "CNC")]
    Cnc,
    /// Margin Intraday Square-off.
    #[serde(rename = "MIS")]
    Mis,
    /// Regular margin (overnight).
    #[serde(rename = "NRML")]
    Nrml,
}

impl GrowwProduct {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cnc => "CNC",
            Self::Mis => "MIS",
            Self::Nrml => "NRML",
        }
    }
}

/// Order type (`16` §4.5). `SL_M` underscore EXACT (not `SL-M`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum GrowwOrderType {
    /// Limit order (price required).
    #[serde(rename = "LIMIT")]
    Limit,
    /// Market order (no price).
    #[serde(rename = "MARKET")]
    Market,
    /// Stop-loss LIMIT (price + trigger).
    #[serde(rename = "SL")]
    Sl,
    /// Stop-loss MARKET (trigger only).
    #[serde(rename = "SL_M")]
    SlM,
}

impl GrowwOrderType {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "LIMIT",
            Self::Market => "MARKET",
            Self::Sl => "SL",
            Self::SlM => "SL_M",
        }
    }
}

/// Transaction type (`16` §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum GrowwTransactionType {
    /// Long.
    #[serde(rename = "BUY")]
    Buy,
    /// Short.
    #[serde(rename = "SELL")]
    Sell,
}

impl GrowwTransactionType {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }
}

/// Validity (`16` §4.8) — **DAY ONLY**. Groww documents NO IOC; the enum has
/// exactly one variant so an IOC request is a compile error, not a runtime
/// reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum GrowwValidity {
    /// Valid until market close on the same trading day.
    #[serde(rename = "DAY")]
    Day,
}

impl GrowwValidity {
    /// Stable wire/audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "DAY",
        }
    }
}

// ---------------------------------------------------------------------------
// Order-status enum — the 12 annexure values + observed OPEN + open-set Unknown
// ---------------------------------------------------------------------------

/// Groww order lifecycle status (`16` §4.1, 12 annexure values) plus the
/// doc-observed-but-un-annexed `OPEN` (every captured response example shows
/// it; wire truth Unknown, O-1) plus the open-set [`GrowwOrderStatus::Unknown`]
/// arm preserving any unrecognized wire string verbatim — parsing NEVER
/// panics and never loses the raw value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GrowwOrderStatus {
    /// Newly created, pending further processing.
    New,
    /// Acknowledged by the system.
    Acked,
    /// Waiting for a trigger event.
    TriggerPending,
    /// Approved and ready for execution.
    Approved,
    /// Resting in the book — the undocumented-but-observed value (O-1).
    Open,
    /// A modify request is in flight.
    ModificationRequested,
    /// A cancel request is in flight.
    CancellationRequested,
    /// Successfully executed (fill complete; settlement pending).
    Executed,
    /// Executed, awaiting delivery.
    DeliveryAwaited,
    /// Completed (settled/closed terminal).
    Completed,
    /// Rejected by the system (terminal).
    Rejected,
    /// Execution failed (terminal).
    Failed,
    /// Cancelled (terminal).
    Cancelled,
    /// Any wire value outside the vocabulary above — the raw string is
    /// preserved for audit; the state machine PARKS on it (never transitions).
    Unknown(String),
}

impl GrowwOrderStatus {
    /// Parse a wire status string — case-insensitive, whitespace-trimmed,
    /// total, no-panic. Unrecognized values land in
    /// [`GrowwOrderStatus::Unknown`] carrying the ORIGINAL (untrimmed-case)
    /// trimmed string.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        match trimmed.to_ascii_uppercase().as_str() {
            "NEW" => Self::New,
            "ACKED" => Self::Acked,
            "TRIGGER_PENDING" => Self::TriggerPending,
            "APPROVED" => Self::Approved,
            "OPEN" => Self::Open,
            "MODIFICATION_REQUESTED" => Self::ModificationRequested,
            "CANCELLATION_REQUESTED" => Self::CancellationRequested,
            "EXECUTED" => Self::Executed,
            "DELIVERY_AWAITED" => Self::DeliveryAwaited,
            "COMPLETED" => Self::Completed,
            "REJECTED" => Self::Rejected,
            "FAILED" => Self::Failed,
            "CANCELLED" => Self::Cancelled,
            _ => Self::Unknown(trimmed.to_owned()),
        }
    }

    /// Stable audit/metric label. `Unknown` reports the fixed label
    /// `"UNKNOWN"` (static-label discipline — the raw string travels on the
    /// audit row, never on a metric label).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::New => "NEW",
            Self::Acked => "ACKED",
            Self::TriggerPending => "TRIGGER_PENDING",
            Self::Approved => "APPROVED",
            Self::Open => "OPEN",
            Self::ModificationRequested => "MODIFICATION_REQUESTED",
            Self::CancellationRequested => "CANCELLATION_REQUESTED",
            Self::Executed => "EXECUTED",
            Self::DeliveryAwaited => "DELIVERY_AWAITED",
            Self::Completed => "COMPLETED",
            Self::Rejected => "REJECTED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
            Self::Unknown(_) => "UNKNOWN",
        }
    }
}

// ---------------------------------------------------------------------------
// Requests (the 3 mutating endpoints)
// ---------------------------------------------------------------------------

/// `POST /v1/order/create` request (`16` §1 row 1 + §2.3 of the design).
///
/// Required-ness of the non-price fields is ASSUMED (the doc gives names, not
/// flags — doc-fidelity F11; day-0 probe D-3). `order_reference_id` is
/// optional on the wire but LOCALLY MANDATORY — our idempotency depends on it
/// (design §4.4) — so it is not `Option` here.
///
/// Named `GrowwCreateOrderReq` (non-substring of the Dhan `PlaceOrderRequest`
/// trigger strings — hostile H4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GrowwCreateOrderReq {
    /// Groww trading symbol.
    pub trading_symbol: String,
    /// Order quantity (total units).
    pub quantity: i64,
    /// Limit price in integer paise; serialized as decimal rupees. `None`
    /// for MARKET / SL_M.
    #[serde(
        serialize_with = "ser_opt_paise_as_rupees",
        skip_serializing_if = "Option::is_none"
    )]
    pub price: Option<i64>,
    /// Trigger price in integer paise; serialized as decimal rupees. `None`
    /// unless SL / SL_M.
    #[serde(
        serialize_with = "ser_opt_paise_as_rupees",
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger_price: Option<i64>,
    /// DAY-only validity.
    pub validity: GrowwValidity,
    /// Exchange (NSE | BSE).
    pub exchange: GrowwExchange,
    /// Segment (CASH | FNO).
    pub segment: GrowwSegment,
    /// Product (CNC | MIS | NRML).
    pub product: GrowwProduct,
    /// Order type (LIMIT | MARKET | SL | SL_M).
    pub order_type: GrowwOrderType,
    /// BUY | SELL.
    pub transaction_type: GrowwTransactionType,
    /// Locally-mandatory idempotency key (8–20 alnum, ≤2 hyphens — `16` §2
    /// note verbatim). Generated by [`super::reference_id`].
    pub order_reference_id: String,
}

/// `POST /v1/order/modify` request (`16` §1 row 2: "quantity / price /
/// trigger_price / order_type + segment + groww_order_id").
///
/// The modifiable fields are `Option` — send only what changes (Assumed; the
/// doc does not flag required-ness). ⚠ Modify `quantity` TOTAL-vs-REMAINING
/// semantics are UNKNOWN (O-11): the executor structurally REFUSES modify on
/// any partially-filled order until the day-0 probe answers it (design §4.7).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GrowwModifyOrderReq {
    /// The Groww-assigned order id being modified.
    pub groww_order_id: String,
    /// Segment of the order.
    pub segment: GrowwSegment,
    /// New quantity (O-11 semantics UNKNOWN — see type doc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    /// New limit price in integer paise (wire: decimal rupees).
    #[serde(
        serialize_with = "ser_opt_paise_as_rupees",
        skip_serializing_if = "Option::is_none"
    )]
    pub price: Option<i64>,
    /// New trigger price in integer paise (wire: decimal rupees).
    #[serde(
        serialize_with = "ser_opt_paise_as_rupees",
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger_price: Option<i64>,
    /// New order type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_type: Option<GrowwOrderType>,
}

/// `POST /v1/order/cancel` request (`16` §1 row 3 — POST, NOT DELETE).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrowwCancelOrderReq {
    /// The Groww-assigned order id being cancelled.
    pub groww_order_id: String,
    /// Segment of the order.
    pub segment: GrowwSegment,
}

// ---------------------------------------------------------------------------
// Responses — envelope + payloads (ALL fields Option; enum fields raw String)
// ---------------------------------------------------------------------------

/// The common `{"status": "...", "payload": {...}}` / failure envelope
/// (`16` §1 header + §5). Every field is `Option` — a 2xx body missing any
/// of them is parseable and classifies AMBIGUOUS downstream, never a panic.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GrowwEnvelope<T> {
    /// `"SUCCESS"` / `"FAILURE"` — raw string (tolerant).
    #[serde(default)]
    pub status: Option<String>,
    /// The success payload.
    #[serde(default = "none_payload")]
    pub payload: Option<T>,
    /// The failure body (`16` §5).
    #[serde(default)]
    pub error: Option<GrowwErrorBody>,
}

/// serde `default` helper — `Option<T>` without a `T: Default` bound.
fn none_payload<T>() -> Option<T> {
    None
}

impl<T> GrowwEnvelope<T> {
    /// Whether the envelope's own `status` field says SUCCESS
    /// (case-insensitive). Absent status ⇒ `false` (fail-closed).
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("SUCCESS"))
    }

    /// Whether the envelope is a WELL-SHAPED FAILURE: `status` says FAILURE
    /// AND an error body with a code is present. Only this shape may ever
    /// classify a mutation as definitively rejected (design §4.3 — anything
    /// else is AMBIGUOUS).
    #[must_use]
    pub fn is_well_shaped_failure(&self) -> bool {
        let status_failure = self
            .status
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("FAILURE"));
        status_failure
            && self
                .error
                .as_ref()
                .is_some_and(|e| e.code.as_deref().is_some_and(|c| !c.trim().is_empty()))
    }
}

/// The failure `error` object (`16` §5): `{"code": "GAxxx", "message": "…",
/// "metadata": null}`. All fields tolerant `Option`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GrowwErrorBody {
    /// GA-code (`GA000`/`GA001`/`GA003`–`GA007`; GA002 does not exist).
    #[serde(default)]
    pub code: Option<String>,
    /// Request-specific message (NOT the generic table string).
    #[serde(default)]
    pub message: Option<String>,
}

/// Place/modify/cancel RESPONSE payload — the schemas are NOT tabulated in
/// doc 16 (O-2 Unknown); captured examples show `groww_order_id` +
/// `order_status` (+ the echoed reference). Modeled tolerant: every field
/// `Option`; a 2xx SUCCESS lacking `groww_order_id` classifies AMBIGUOUS at
/// the executor (fail-closed on O-2, design §4.3).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GrowwMutationRespPayload {
    /// Broker-assigned id (place adopts it; modify/cancel echo it).
    #[serde(default)]
    pub groww_order_id: Option<String>,
    /// Raw status string (examples show `"OPEN"` — O-1).
    #[serde(default)]
    pub order_status: Option<String>,
    /// Echoed user reference id.
    #[serde(default)]
    pub order_reference_id: Option<String>,
    /// Broker remark.
    #[serde(default)]
    pub remark: Option<String>,
}

/// `GET /v1/order/status/{id}` + `/status/reference/{ref}` payload — the
/// 5-field SUBSET (`16` §2 note). ALL FIVE fields `Option` (doc-fidelity
/// F10): a 200 SUCCESS with no usable id/status stays AMBIGUOUS downstream.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GrowwOrderStatusPayload {
    /// Broker-assigned order id.
    #[serde(default)]
    pub groww_order_id: Option<String>,
    /// Raw status string (annexure §4.1 + observed `OPEN`).
    #[serde(default)]
    pub order_status: Option<String>,
    /// Broker remark.
    #[serde(default)]
    pub remark: Option<String>,
    /// Cumulative filled quantity.
    #[serde(default)]
    pub filled_quantity: Option<i64>,
    /// Echoed user reference id.
    #[serde(default)]
    pub order_reference_id: Option<String>,
}

/// `GET /v1/order/detail/{id}` payload — the verbatim 22-field table
/// (`16` §2); `GET /v1/order/list` rows are identical. Every field `Option`
/// (nothing is wire-guaranteed); enum-valued fields raw `String`
/// (doc-fidelity F3); money in integer paise (wire: decimal rupees);
/// timestamps raw strings (formats inconsistent in the doc's own example —
/// `created_at` naive-ISO vs `trade_date` Zulu; wire truth Unknown #4).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GrowwOrderDetailPayload {
    /// Broker-assigned order id.
    #[serde(default)]
    pub groww_order_id: Option<String>,
    /// Groww trading symbol.
    #[serde(default)]
    pub trading_symbol: Option<String>,
    /// Raw status string.
    #[serde(default)]
    pub order_status: Option<String>,
    /// Broker remark.
    #[serde(default)]
    pub remark: Option<String>,
    /// Ordered quantity.
    #[serde(default)]
    pub quantity: Option<i64>,
    /// Limit price, integer paise (wire: decimal rupees).
    #[serde(default, deserialize_with = "de_opt_rupees_as_paise")]
    pub price: Option<i64>,
    /// Trigger price, integer paise (wire: decimal rupees).
    #[serde(default, deserialize_with = "de_opt_rupees_as_paise")]
    pub trigger_price: Option<i64>,
    /// Executed quantity.
    #[serde(default)]
    pub filled_quantity: Option<i64>,
    /// Remaining quantity.
    #[serde(default)]
    pub remaining_quantity: Option<i64>,
    /// Average fill price, integer paise (wire: decimal rupees).
    #[serde(default, deserialize_with = "de_opt_rupees_as_paise")]
    pub average_fill_price: Option<i64>,
    /// Deliverable quantity.
    #[serde(default)]
    pub deliverable_quantity: Option<i64>,
    /// After-market-order status — 7-value annexure (§4.2) modeled as a
    /// tolerant raw string.
    #[serde(default)]
    pub amo_status: Option<String>,
    /// Validity — raw string on the response side.
    #[serde(default)]
    pub validity: Option<String>,
    /// Exchange — raw string on the response side.
    #[serde(default)]
    pub exchange: Option<String>,
    /// Order type — raw string on the response side.
    #[serde(default)]
    pub order_type: Option<String>,
    /// Transaction type — raw string on the response side.
    #[serde(default)]
    pub transaction_type: Option<String>,
    /// Segment — raw string on the response side.
    #[serde(default)]
    pub segment: Option<String>,
    /// Product — raw string on the response side.
    #[serde(default)]
    pub product: Option<String>,
    /// Creation timestamp — raw string (format Unknown #4).
    #[serde(default)]
    pub created_at: Option<String>,
    /// Exchange timestamp — raw string (format Unknown #4).
    #[serde(default)]
    pub exchange_time: Option<String>,
    /// Trade date — raw string (format Unknown #4).
    #[serde(default)]
    pub trade_date: Option<String>,
    /// Echoed user reference id.
    #[serde(default)]
    pub order_reference_id: Option<String>,
}

/// `GET /v1/order/trades/{id}` row — the FULL schema is Unknown locally
/// (O-3, `16` §1 row 8 names only the id + settlement fields). Modeled with
/// the known fields, all `Option`, plus tolerant money/quantity guesses
/// flagged Assumed; day-0 probe D-15 completes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GrowwTradeRow {
    /// Groww trade id.
    #[serde(default)]
    pub groww_trade_id: Option<String>,
    /// Exchange trade id.
    #[serde(default)]
    pub exchange_trade_id: Option<String>,
    /// Groww order id.
    #[serde(default)]
    pub groww_order_id: Option<String>,
    /// Exchange order id.
    #[serde(default)]
    pub exchange_order_id: Option<String>,
    /// Settlement number.
    #[serde(default)]
    pub settlement_number: Option<String>,
    /// Trade quantity (Assumed field name — O-3).
    #[serde(default)]
    pub quantity: Option<i64>,
    /// Trade price, integer paise (Assumed field name — O-3).
    #[serde(default, deserialize_with = "de_opt_rupees_as_paise")]
    pub price: Option<i64>,
}

// ---------------------------------------------------------------------------
// Timestamps — dual-format parse (naive-ISO + Zulu), Assumed IST
// ---------------------------------------------------------------------------

/// Parse a Groww response timestamp into epoch MILLISECONDS.
///
/// The doc's own example mixes `"2023-10-01T10:15:30"` (naive ISO) and
/// `"2019-08-24T14:15:22Z"` (Zulu) — wire truth Unknown #4. Dual-format:
/// a Zulu/offset form is taken at face value; a NAIVE form is Assumed IST
/// (the whole doc pack carries no timezone statement). Returns `None` on
/// anything else — never fabricated, never a panic.
#[must_use]
pub fn parse_groww_timestamp_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Offset-carrying (Zulu or explicit offset) form first.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.timestamp_millis());
    }
    // Naive ISO — Assumed IST wall-clock.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        use chrono::TimeZone as _;
        return match chrono_tz::Asia::Kolkata.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => Some(dt.timestamp_millis()),
            // IST has no DST — ambiguous/none are unreachable; fail-soft anyway.
            _ => None,
        };
    }
    None
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed Groww OMS error — the pure-core subset (transport-classification
/// variants extend this in ORD-PR-3).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GrowwOmsError {
    /// The intent ledger could not durably record the intent — the mutation
    /// is REFUSED (write-ahead discipline, design §4.5).
    // GROWW-ORD-06 emit lands in ORD-PR-3 once the variants exist.
    #[error("intent ledger unavailable: {0}")]
    LedgerUnavailable(String),
    /// A non-terminal intent with the same reference id already exists —
    /// refused locally BEFORE any HTTP (design §4.4).
    #[error("duplicate intent for reference id {reference_id}")]
    DuplicateIntent {
        /// The colliding reference id.
        reference_id: String,
    },
    /// A non-terminal mutation intent is already in flight for this order
    /// (serialization invariant, design §4.7 / hostile F-3).
    #[error("mutation already in flight for order {groww_order_id}")]
    MutationInFlight {
        /// The order with the open mutation.
        groww_order_id: String,
    },
    /// Modify refused on a partially-filled order — O-11 TOTAL-vs-REMAINING
    /// semantics are unproven (design §4.7 / hostile F-4).
    #[error("modify refused: order {groww_order_id} is partially filled (O-11 unproven)")]
    ModifyOnPartialFillUnproven {
        /// The partially-filled order.
        groww_order_id: String,
    },
    /// Quantity exceeds the configured `max_order_quantity` gate
    /// (design §4.12; single-slice discipline).
    // GROWW-ORD-09 emit lands in ORD-PR-3 once the variants exist.
    #[error("quantity {quantity} refused (max_order_quantity = {max})")]
    QuantityRefused {
        /// Requested quantity.
        quantity: i64,
        /// Configured ceiling (0 = refuse all).
        max: i64,
    },
    /// A request field failed local validation (pure, pre-HTTP).
    #[error("invalid order field: {0}")]
    InvalidOrderField(String),
    /// An invalid reference id (violates the 8–20-alnum-≤2-hyphens contract).
    #[error("invalid order_reference_id: {0}")]
    InvalidReferenceId(String),
    /// Mutations on an Unknown-parked order are refused until reconcile
    /// clarifies (design §4.7 / P50).
    #[error("order {groww_order_id} is parked on an unknown status; mutation refused")]
    OrderParkedUnknown {
        /// The parked order.
        groww_order_id: String,
    },
    /// A NON-terminal phase append was attempted on an already-settled
    /// intent (MEDIUM-8) — a terminal ledger phase can never regress; the
    /// append is refused fail-loud.
    #[error(
        "intent {intent_id} already settled at {settled_phase}; \
         non-terminal phase {attempted_phase} refused"
    )]
    IntentAlreadySettled {
        /// The settled intent.
        intent_id: String,
        /// Its terminal phase label.
        settled_phase: &'static str,
        /// The refused non-terminal phase label.
        attempted_phase: &'static str,
    },
    /// The live-send arm is disabled at compile time (Gate 2) — constructing
    /// a live send without the feature+const alignment is refused.
    #[error("live order sending is disabled at compile time")]
    LiveDisabledAtCompileTime,
}

// ---------------------------------------------------------------------------
// Pure validation (financial boundary — qty/price/trigger)
// ---------------------------------------------------------------------------

/// Validate a create-order request against the pure financial boundaries.
///
/// - `quantity` ≥ 1 and ≤ `max_order_quantity` (`max == 0` ⇒ refuse ALL —
///   the fail-closed default of design §4.12/§4.13);
/// - LIMIT: price required (> 0), trigger absent;
/// - MARKET: no price, no trigger;
/// - SL: price AND trigger required (> 0);
/// - SL_M: trigger required (> 0), no price;
/// - `trading_symbol` non-empty;
/// - `order_reference_id` valid per the doc contract.
///
/// Price-shape rules are Assumed (doc-fidelity F11 — the doc gives names,
/// not required-flags; day-0 probe D-3 confirms).
pub fn validate_create_order(
    req: &GrowwCreateOrderReq,
    max_order_quantity: i64,
) -> Result<(), GrowwOmsError> {
    if req.trading_symbol.trim().is_empty() {
        return Err(GrowwOmsError::InvalidOrderField(
            "trading_symbol is empty".to_owned(),
        ));
    }
    if req.quantity < 1 {
        return Err(GrowwOmsError::InvalidOrderField(format!(
            "quantity {} < 1",
            req.quantity
        )));
    }
    if req.quantity > max_order_quantity {
        return Err(GrowwOmsError::QuantityRefused {
            quantity: req.quantity,
            max: max_order_quantity,
        });
    }
    if !reference_id::is_valid_reference_id(&req.order_reference_id) {
        return Err(GrowwOmsError::InvalidReferenceId(
            req.order_reference_id.clone(),
        ));
    }
    validate_price_shape(req.order_type, req.price, req.trigger_price)
}

/// Validate the (order_type, price, trigger_price) shape — shared by create
/// and modify validation. Paise values must be strictly positive when
/// present/required.
pub fn validate_price_shape(
    order_type: GrowwOrderType,
    price: Option<i64>,
    trigger_price: Option<i64>,
) -> Result<(), GrowwOmsError> {
    let positive = |name: &str, v: Option<i64>| -> Result<(), GrowwOmsError> {
        match v {
            Some(p) if p > 0 => Ok(()),
            Some(p) => Err(GrowwOmsError::InvalidOrderField(format!(
                "{name} must be > 0 paise (got {p})"
            ))),
            None => Err(GrowwOmsError::InvalidOrderField(format!(
                "{name} required for {}",
                order_type.as_str()
            ))),
        }
    };
    let absent = |name: &str, v: Option<i64>| -> Result<(), GrowwOmsError> {
        if v.is_some() {
            Err(GrowwOmsError::InvalidOrderField(format!(
                "{name} must be absent for {}",
                order_type.as_str()
            )))
        } else {
            Ok(())
        }
    };
    match order_type {
        GrowwOrderType::Limit => {
            positive("price", price)?;
            absent("trigger_price", trigger_price)
        }
        GrowwOrderType::Market => {
            absent("price", price)?;
            absent("trigger_price", trigger_price)
        }
        GrowwOrderType::Sl => {
            positive("price", price)?;
            positive("trigger_price", trigger_price)
        }
        GrowwOrderType::SlM => {
            absent("price", price)?;
            positive("trigger_price", trigger_price)
        }
    }
}

/// Validate a modify request: at least one modifiable field present; any
/// present price/trigger strictly positive; quantity (when present) ≥ 1 and
/// ≤ the gate. The O-11 partial-fill refusal is the EXECUTOR's job (it holds
/// the fill state); this is the pure field-shape half.
pub fn validate_modify_order(
    req: &GrowwModifyOrderReq,
    max_order_quantity: i64,
) -> Result<(), GrowwOmsError> {
    if req.groww_order_id.trim().is_empty() {
        return Err(GrowwOmsError::InvalidOrderField(
            "groww_order_id is empty".to_owned(),
        ));
    }
    if req.quantity.is_none()
        && req.price.is_none()
        && req.trigger_price.is_none()
        && req.order_type.is_none()
    {
        return Err(GrowwOmsError::InvalidOrderField(
            "modify carries no modifiable field".to_owned(),
        ));
    }
    if let Some(q) = req.quantity {
        if q < 1 {
            return Err(GrowwOmsError::InvalidOrderField(format!(
                "quantity {q} < 1"
            )));
        }
        if q > max_order_quantity {
            return Err(GrowwOmsError::QuantityRefused {
                quantity: q,
                max: max_order_quantity,
            });
        }
    }
    for (name, v) in [("price", req.price), ("trigger_price", req.trigger_price)] {
        if let Some(p) = v
            && p <= 0
        {
            return Err(GrowwOmsError::InvalidOrderField(format!(
                "{name} must be > 0 paise (got {p})"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_limit_req() -> GrowwCreateOrderReq {
        GrowwCreateOrderReq {
            trading_symbol: "NIFTY25JUL26000CE".to_owned(),
            quantity: 50,
            price: Some(12_550), // ₹125.50
            trigger_price: None,
            validity: GrowwValidity::Day,
            exchange: GrowwExchange::Nse,
            segment: GrowwSegment::Fno,
            product: GrowwProduct::Nrml,
            order_type: GrowwOrderType::Limit,
            transaction_type: GrowwTransactionType::Buy,
            order_reference_id: "TV26071500010A2B".to_owned(),
        }
    }

    // --- serde renames exact per doc 16 ---

    #[test]
    fn test_create_req_serializes_exact_wire_names_and_rupees() {
        let req = base_limit_req();
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["trading_symbol"], "NIFTY25JUL26000CE");
        assert_eq!(v["quantity"], 50);
        assert_eq!(v["price"], 125.5); // paise → rupees at the boundary
        assert!(v.get("trigger_price").is_none()); // skipped when None
        assert_eq!(v["validity"], "DAY");
        assert_eq!(v["exchange"], "NSE");
        assert_eq!(v["segment"], "FNO");
        assert_eq!(v["product"], "NRML");
        assert_eq!(v["order_type"], "LIMIT");
        assert_eq!(v["transaction_type"], "BUY");
        assert_eq!(v["order_reference_id"], "TV26071500010A2B");
    }

    #[test]
    fn test_sl_m_serializes_with_underscore_exact() {
        assert_eq!(
            serde_json::to_value(GrowwOrderType::SlM).unwrap(),
            serde_json::json!("SL_M")
        );
        assert_eq!(GrowwOrderType::SlM.as_str(), "SL_M");
    }

    #[test]
    fn test_modify_req_skips_absent_fields() {
        let req = GrowwModifyOrderReq {
            groww_order_id: "GRW123".to_owned(),
            segment: GrowwSegment::Cash,
            quantity: None,
            price: Some(10_001), // ₹100.01
            trigger_price: None,
            order_type: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["groww_order_id"], "GRW123");
        assert_eq!(v["segment"], "CASH");
        assert_eq!(v["price"], 100.01);
        assert!(v.get("quantity").is_none());
        assert!(v.get("trigger_price").is_none());
        assert!(v.get("order_type").is_none());
    }

    #[test]
    fn test_cancel_req_shape() {
        let req = GrowwCancelOrderReq {
            groww_order_id: "GRW9".to_owned(),
            segment: GrowwSegment::Fno,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["groww_order_id"], "GRW9");
        assert_eq!(v["segment"], "FNO");
    }

    // --- money conversion ---

    #[test]
    fn test_rupees_to_paise_boundaries() {
        assert_eq!(rupees_f64_to_paise(0.0), Some(0));
        assert_eq!(rupees_f64_to_paise(100.01), Some(10_001));
        assert_eq!(rupees_f64_to_paise(245.5), Some(24_550));
        assert_eq!(rupees_f64_to_paise(-1.25), Some(-125));
        assert_eq!(rupees_f64_to_paise(f64::NAN), None);
        assert_eq!(rupees_f64_to_paise(f64::INFINITY), None);
        assert_eq!(rupees_f64_to_paise(1.0e300), None);
        // MEDIUM-9: the NSE-plausible bound is 1e10 rupees — exactly at the
        // bound is admitted; anything beyond is rejected.
        assert_eq!(rupees_f64_to_paise(1.0e10), Some(1_000_000_000_000));
        assert_eq!(rupees_f64_to_paise(1.0e10 + 1.0), None);
        assert_eq!(rupees_f64_to_paise(-(1.0e10 + 1.0)), None);
    }

    #[test]
    fn test_reviewer_counterexample_paise_rejected_not_silently_rounded() {
        // MEDIUM-9 counterexample: 4,358,277,003,831,209 paise =
        // ₹43,582,770,038,312.09 — inside the OLD 9e13 bound, but its f64
        // rupee form is not 2-decimal-exact, so the round-trip silently
        // drifted. Under the tightened 1e10 bound it MUST be rejected.
        let paise: i64 = 4_358_277_003_831_209;
        let rupees = paise_to_rupees_f64(paise);
        assert_eq!(rupees_f64_to_paise(rupees), None, "must reject, not drift");
        assert_eq!(rupees_f64_to_paise(43_582_770_038_312.09), None);
    }

    #[test]
    fn test_paise_rupees_roundtrip_two_decimals() {
        for paise in [
            0_i64,
            1,
            99,
            100,
            10_001,
            24_550,
            9_999_999,
            999_999_999_999,
            1_000_000_000_000, // the exact band edge (₹1e10)
        ] {
            let rupees = paise_to_rupees_f64(paise);
            assert_eq!(rupees_f64_to_paise(rupees), Some(paise), "paise={paise}");
        }
    }

    #[test]
    fn test_response_negative_price_is_a_hard_deserialize_error() {
        // MEDIUM-9: broker prices are never negative — fail-loud, never a
        // silently wrong (negative) paise value.
        for body in [
            r#"{"price":-2847.5}"#,
            r#"{"trigger_price":-0.05}"#,
            r#"{"average_fill_price":-1.0}"#,
        ] {
            let parsed: Result<GrowwOrderDetailPayload, _> = serde_json::from_str(body);
            assert!(parsed.is_err(), "negative price must fail loud: {body}");
        }
        let trade: Result<GrowwTradeRow, _> = serde_json::from_str(r#"{"price":-1.25}"#);
        assert!(trade.is_err(), "negative trade price must fail loud");
        // Zero stays parseable (untraded/absent-price rows carry 0.0).
        let zero: GrowwOrderDetailPayload = serde_json::from_str(r#"{"price":0.0}"#).unwrap();
        assert_eq!(zero.price, Some(0));
    }

    proptest::proptest! {
        /// MEDIUM-9: paise → rupees → paise is EXACT across the WHOLE
        /// admitted band (|paise| ≤ 1e12 ⇔ |rupees| ≤ 1e10).
        #[test]
        fn prop_paise_roundtrip_exact_across_admitted_band(
            paise in -1_000_000_000_000_i64..=1_000_000_000_000,
        ) {
            let rupees = paise_to_rupees_f64(paise);
            proptest::prop_assert_eq!(rupees_f64_to_paise(rupees), Some(paise));
        }
    }

    // --- envelope / payload tolerance ---

    #[test]
    fn test_envelope_success_with_all_option_status_payload() {
        let body = r#"{"status":"SUCCESS","payload":{}}"#;
        let env: GrowwEnvelope<GrowwOrderStatusPayload> = serde_json::from_str(body).unwrap();
        assert!(env.is_success());
        let p = env.payload.unwrap();
        assert!(p.groww_order_id.is_none());
        assert!(p.order_status.is_none());
        assert!(p.filled_quantity.is_none());
    }

    #[test]
    fn test_envelope_well_shaped_failure_requires_code() {
        let ok = r#"{"status":"FAILURE","error":{"code":"GA001","message":"Bad request","metadata":null}}"#;
        let env: GrowwEnvelope<GrowwMutationRespPayload> = serde_json::from_str(ok).unwrap();
        assert!(env.is_well_shaped_failure());
        assert!(!env.is_success());

        // FAILURE without a code is NOT well-shaped (⇒ ambiguous downstream).
        let missing_code = r#"{"status":"FAILURE","error":{"message":"?"}}"#;
        let env: GrowwEnvelope<GrowwMutationRespPayload> =
            serde_json::from_str(missing_code).unwrap();
        assert!(!env.is_well_shaped_failure());

        // SUCCESS-status with error body is NOT well-shaped failure either.
        let weird = r#"{"status":"SUCCESS","error":{"code":"GA003"}}"#;
        let env: GrowwEnvelope<GrowwMutationRespPayload> = serde_json::from_str(weird).unwrap();
        assert!(!env.is_well_shaped_failure());
        assert!(env.is_success());
    }

    #[test]
    fn test_envelope_empty_object_parses_all_none() {
        let env: GrowwEnvelope<GrowwOrderDetailPayload> = serde_json::from_str("{}").unwrap();
        assert!(!env.is_success());
        assert!(!env.is_well_shaped_failure());
        assert!(env.payload.is_none());
    }

    #[test]
    fn test_detail_payload_full_22_field_row_parses_to_paise() {
        let body = r#"{
            "groww_order_id":"GRW1","trading_symbol":"RELIANCE","order_status":"OPEN",
            "remark":"ok","quantity":10,"price":2847.5,"trigger_price":0.0,
            "filled_quantity":4,"remaining_quantity":6,"average_fill_price":2847.55,
            "deliverable_quantity":0,"amo_status":"NA","validity":"DAY","exchange":"NSE",
            "order_type":"LIMIT","transaction_type":"BUY","segment":"CASH","product":"CNC",
            "created_at":"2023-10-01T10:15:30","exchange_time":"2023-10-01T10:15:31",
            "trade_date":"2019-08-24T14:15:22Z","order_reference_id":"TV26071500010A2B"
        }"#;
        let p: GrowwOrderDetailPayload = serde_json::from_str(body).unwrap();
        assert_eq!(p.price, Some(284_750));
        assert_eq!(p.average_fill_price, Some(284_755));
        assert_eq!(p.trigger_price, Some(0));
        assert_eq!(p.filled_quantity, Some(4));
        assert_eq!(p.order_status.as_deref(), Some("OPEN"));
        assert_eq!(p.segment.as_deref(), Some("CASH"));
    }

    #[test]
    fn test_detail_payload_unknown_extra_fields_tolerated() {
        // A future wire field must never break the parse.
        let body = r#"{"groww_order_id":"G","brand_new_field_2027":true}"#;
        let p: GrowwOrderDetailPayload = serde_json::from_str(body).unwrap();
        assert_eq!(p.groww_order_id.as_deref(), Some("G"));
    }

    #[test]
    fn test_trade_row_all_option() {
        let r: GrowwTradeRow = serde_json::from_str("{}").unwrap();
        assert!(r.groww_trade_id.is_none());
        assert!(r.price.is_none());
    }

    // --- status enum parse: 12 annexure values + OPEN + Unknown ---

    #[test]
    fn test_parse_status_12_annexure_values_plus_undocumented_open() {
        let expected = [
            ("NEW", GrowwOrderStatus::New),
            ("ACKED", GrowwOrderStatus::Acked),
            ("TRIGGER_PENDING", GrowwOrderStatus::TriggerPending),
            ("APPROVED", GrowwOrderStatus::Approved),
            ("REJECTED", GrowwOrderStatus::Rejected),
            ("FAILED", GrowwOrderStatus::Failed),
            ("EXECUTED", GrowwOrderStatus::Executed),
            ("DELIVERY_AWAITED", GrowwOrderStatus::DeliveryAwaited),
            ("CANCELLED", GrowwOrderStatus::Cancelled),
            (
                "CANCELLATION_REQUESTED",
                GrowwOrderStatus::CancellationRequested,
            ),
            (
                "MODIFICATION_REQUESTED",
                GrowwOrderStatus::ModificationRequested,
            ),
            ("COMPLETED", GrowwOrderStatus::Completed),
            // The undocumented-but-observed value (O-1) is FIRST-CLASS:
            ("OPEN", GrowwOrderStatus::Open),
        ];
        for (raw, want) in expected {
            assert_eq!(GrowwOrderStatus::parse(raw), want, "raw={raw}");
            // Case-insensitive + whitespace-trimmed.
            let lower = format!("  {}  ", raw.to_ascii_lowercase());
            assert_eq!(GrowwOrderStatus::parse(&lower), want, "raw={lower}");
        }
    }

    #[test]
    fn test_parse_status_unknown_preserves_raw_no_panic() {
        match GrowwOrderStatus::parse(" Something_New_2027 ") {
            GrowwOrderStatus::Unknown(raw) => assert_eq!(raw, "Something_New_2027"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert_eq!(
            GrowwOrderStatus::parse(""),
            GrowwOrderStatus::Unknown(String::new())
        );
        assert_eq!(GrowwOrderStatus::parse("\u{0}\u{7}").as_str(), "UNKNOWN");
    }

    // --- timestamps ---

    #[test]
    fn test_parse_groww_timestamp_dual_format() {
        // Zulu form: taken at face value.
        let zulu = parse_groww_timestamp_ms("2019-08-24T14:15:22Z").unwrap();
        assert_eq!(zulu, 1_566_656_122_000);
        // Naive ISO: Assumed IST (UTC+5:30).
        let naive = parse_groww_timestamp_ms("2019-08-24T19:45:22").unwrap();
        assert_eq!(naive, zulu, "naive IST 19:45:22 == Zulu 14:15:22");
        // Garbage / empty → None, never panic.
        assert_eq!(parse_groww_timestamp_ms(""), None);
        assert_eq!(parse_groww_timestamp_ms("24-08-2019"), None);
        assert_eq!(parse_groww_timestamp_ms("not a time"), None);
    }

    // --- financial boundary validation ---

    #[test]
    fn test_validate_create_limit_happy_path() {
        assert!(validate_create_order(&base_limit_req(), 100).is_ok());
    }

    #[test]
    fn test_validate_quantity_boundaries() {
        let mut req = base_limit_req();
        req.quantity = 0;
        assert!(matches!(
            validate_create_order(&req, 100),
            Err(GrowwOmsError::InvalidOrderField(_))
        ));
        req.quantity = -5;
        assert!(matches!(
            validate_create_order(&req, 100),
            Err(GrowwOmsError::InvalidOrderField(_))
        ));
        req.quantity = 101;
        assert!(matches!(
            validate_create_order(&req, 100),
            Err(GrowwOmsError::QuantityRefused {
                quantity: 101,
                max: 100
            })
        ));
        // max == 0 ⇒ refuse ALL (the fail-closed default).
        req.quantity = 1;
        assert!(matches!(
            validate_create_order(&req, 0),
            Err(GrowwOmsError::QuantityRefused { .. })
        ));
        // exactly at the cap is allowed.
        req.quantity = 100;
        assert!(validate_create_order(&req, 100).is_ok());
    }

    #[test]
    fn test_validate_price_shape_per_order_type() {
        use GrowwOrderType as T;
        // LIMIT: price required, trigger absent.
        assert!(validate_price_shape(T::Limit, Some(100), None).is_ok());
        assert!(validate_price_shape(T::Limit, None, None).is_err());
        assert!(validate_price_shape(T::Limit, Some(0), None).is_err());
        assert!(validate_price_shape(T::Limit, Some(-10), None).is_err());
        assert!(validate_price_shape(T::Limit, Some(100), Some(90)).is_err());
        // MARKET: neither.
        assert!(validate_price_shape(T::Market, None, None).is_ok());
        assert!(validate_price_shape(T::Market, Some(100), None).is_err());
        assert!(validate_price_shape(T::Market, None, Some(100)).is_err());
        // SL: both, positive.
        assert!(validate_price_shape(T::Sl, Some(100), Some(95)).is_ok());
        assert!(validate_price_shape(T::Sl, Some(100), None).is_err());
        assert!(validate_price_shape(T::Sl, None, Some(95)).is_err());
        assert!(validate_price_shape(T::Sl, Some(100), Some(0)).is_err());
        // SL_M: trigger only.
        assert!(validate_price_shape(T::SlM, None, Some(95)).is_ok());
        assert!(validate_price_shape(T::SlM, Some(100), Some(95)).is_err());
        assert!(validate_price_shape(T::SlM, None, None).is_err());
    }

    #[test]
    fn test_validate_create_rejects_empty_symbol_and_bad_reference() {
        let mut req = base_limit_req();
        req.trading_symbol = "   ".to_owned();
        assert!(matches!(
            validate_create_order(&req, 100),
            Err(GrowwOmsError::InvalidOrderField(_))
        ));
        let mut req = base_limit_req();
        req.order_reference_id = "short".to_owned(); // < 8 chars
        assert!(matches!(
            validate_create_order(&req, 100),
            Err(GrowwOmsError::InvalidReferenceId(_))
        ));
    }

    #[test]
    fn test_validate_modify_shapes() {
        let base = GrowwModifyOrderReq {
            groww_order_id: "GRW1".to_owned(),
            segment: GrowwSegment::Fno,
            quantity: None,
            price: None,
            trigger_price: None,
            order_type: None,
        };
        // No modifiable field ⇒ refused.
        assert!(validate_modify_order(&base, 100).is_err());
        // Empty order id ⇒ refused.
        let mut req = base.clone();
        req.groww_order_id = String::new();
        req.price = Some(100);
        assert!(validate_modify_order(&req, 100).is_err());
        // Happy: one field.
        let mut req = base.clone();
        req.price = Some(100);
        assert!(validate_modify_order(&req, 100).is_ok());
        // Bad quantity / bad price boundaries.
        let mut req = base.clone();
        req.quantity = Some(0);
        assert!(validate_modify_order(&req, 100).is_err());
        let mut req = base.clone();
        req.quantity = Some(101);
        assert!(matches!(
            validate_modify_order(&req, 100),
            Err(GrowwOmsError::QuantityRefused { .. })
        ));
        let mut req = base;
        req.trigger_price = Some(-1);
        assert!(validate_modify_order(&req, 100).is_err());
    }

    #[test]
    fn test_enum_as_str_labels_stable() {
        assert_eq!(GrowwExchange::Nse.as_str(), "NSE");
        assert_eq!(GrowwExchange::Bse.as_str(), "BSE");
        assert_eq!(GrowwSegment::Cash.as_str(), "CASH");
        assert_eq!(GrowwSegment::Fno.as_str(), "FNO");
        assert_eq!(GrowwProduct::Cnc.as_str(), "CNC");
        assert_eq!(GrowwProduct::Mis.as_str(), "MIS");
        assert_eq!(GrowwProduct::Nrml.as_str(), "NRML");
        assert_eq!(GrowwValidity::Day.as_str(), "DAY");
        assert_eq!(GrowwTransactionType::Buy.as_str(), "BUY");
        assert_eq!(GrowwTransactionType::Sell.as_str(), "SELL");
        assert_eq!(GrowwOrderType::Limit.as_str(), "LIMIT");
        assert_eq!(GrowwOrderType::Market.as_str(), "MARKET");
        assert_eq!(GrowwOrderType::Sl.as_str(), "SL");
    }
}
