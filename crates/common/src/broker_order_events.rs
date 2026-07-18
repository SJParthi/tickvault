//! Broker-agnostic order-lifecycle event — the shared seam every broker's
//! order-side maps INTO, so the OMS, audit tables, and reconciliation speak
//! ONE order-event shape regardless of which broker produced it.
//!
//! ## Why this lives in `common`
//! `common` sits below `core`/`trading`/`storage`/`api`/`app` in the dependency
//! flow (`common ← core ← trading ← storage ← api ← app`), so a type defined
//! here is importable everywhere — the same reasoning that moved [`crate::feed::Feed`]
//! down to `common` (SP1 of the common-feed-engine convergence). The
//! broker id REUSES [`crate::feed::Feed`] — there is NO second broker enum
//! (Dhan = `Feed::Dhan`, Groww = `Feed::Groww`).
//!
//! ## Scope (Groww order-side build, operator authorization 2026-07-14)
//! This is the neutral order-event contract for the GATED Groww order-side
//! build (`.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §39;
//! `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` §10). It is
//! pure data + total no-panic mappers — it fires NO request and holds NO
//! live-fire gate. Actual order placement remains hard-locked behind the
//! 4-gate live-fire lattice (config default-OFF + the non-default
//! `groww_orders` cargo feature + the `GROWW_ORDER_LIVE_FIRE = false`
//! constant + the rule lock) until the operator's explicit live-orders
//! enable.
//!
//! ## Price + integer discipline
//! Prices are integer PAISE (`i64`) — never `f64` — mirroring the storage
//! integer-paise discipline (`data-integrity.md`). Quantities are `i64`.
//! Missing/absent fields are `Option`, never a sentinel.

use crate::feed::Feed;
use crate::order_types::OrderUpdate;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;

/// A single broker order-lifecycle observation, normalized across brokers.
///
/// Produced by a broker's order-update parser (Dhan order-update WebSocket,
/// Groww order-update stream / REST status poll) and consumed by the OMS,
/// the order-audit table, and reconciliation. All fields are plain public
/// data — this type carries no behaviour beyond construction.
///
/// Deliberately NOT `serde`-derived: [`Feed`] is not `Serialize`, and adding
/// that derive would be an out-of-scope change to `feed.rs`. Persisters map
/// the fields explicitly (the storage-writer house pattern), using
/// [`Feed::as_str`] / [`BrokerOrderStatus::as_str`] for the wire labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerOrderEvent {
    /// Which broker produced this event (`Feed::Dhan` / `Feed::Groww`).
    pub broker: Feed,
    /// The broker-assigned order id (Dhan `orderNo` / Groww `groww_order_id`).
    pub broker_order_id: String,
    /// The user-supplied reference/correlation id, if this broker echoes one
    /// (Dhan `correlationId` / Groww `order_reference_id`). `None` when the
    /// broker did not carry it on this event.
    pub reference_id: Option<String>,
    /// The normalized lifecycle status.
    pub status: BrokerOrderStatus,
    /// The verbatim broker status string this event carried (audit / forensic;
    /// preserved even when [`status`](Self::status) is
    /// [`BrokerOrderStatus::Unknown`], so an un-mapped wire value is never lost).
    pub raw_status: String,
    /// Cumulative filled quantity for this order.
    pub filled_qty: i64,
    /// Remaining unfilled quantity, when the broker reports it (`None` when
    /// the event omits it — never a `-1`/`0` sentinel).
    pub remaining_qty: Option<i64>,
    /// Average fill price in integer PAISE (`Some` once at least one fill has
    /// priced; `None` for a not-yet-filled order). Integer paise, never `f64`.
    pub avg_fill_price_paise: Option<i64>,
    /// The broker segment label as received (`"CASH"`/`"FNO"`/`"COMMODITY"` for
    /// Groww; `"E"`/`"D"`/… for Dhan) — kept verbatim; canonicalization is a
    /// consumer concern.
    pub segment: String,
    /// The exchange event timestamp in epoch MILLISECONDS, when parseable from
    /// the broker payload (`None` when absent or unparseable — never fabricated).
    pub exchange_ts_ms: Option<i64>,
    /// Local receive time in epoch MILLISECONDS — stamped when this event was
    /// decoded (always present; the freshness anchor).
    pub received_at_ms: i64,
}

/// Normalized order-lifecycle status, broker-agnostic.
///
/// Every broker's status vocabulary maps INTO these variants via the total,
/// no-panic mappers below. An unrecognized wire value maps to
/// [`BrokerOrderStatus::Unknown`] (never a panic) while
/// [`BrokerOrderEvent::raw_status`] preserves the original string.
///
/// Mirrors the Dhan [`crate::order_types::OrderStatus`] vocabulary where they
/// overlap; adds the broker-neutral `Failed` (Groww `FAILED`) and `New`
/// (Groww `NEW`) that Dhan does not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrokerOrderStatus {
    /// Newly created, not yet acknowledged (Groww `NEW`).
    New,
    /// Acknowledged / in transit / a cancel-or-modify request is in flight —
    /// a non-terminal working-toward state.
    Pending,
    /// Resting/open in the exchange book (Dhan `CONFIRMED`/`TRIGGERED`;
    /// Groww `APPROVED` and the undocumented `OPEN`).
    Open,
    /// Some quantity filled, remainder still open (Dhan `PART_TRADED` /
    /// `PARTIALLY_FILLED`). Groww has no distinct partial status on the wire —
    /// a partial is inferred from `filled_qty` vs total by the consumer.
    PartiallyFilled,
    /// Fully executed / completed (Dhan `TRADED`/`CLOSED`; Groww `EXECUTED`/
    /// `DELIVERY_AWAITED`/`COMPLETED`).
    Filled,
    /// Cancelled (terminal).
    Cancelled,
    /// Rejected by exchange/RMS (terminal).
    Rejected,
    /// Execution failed (terminal — Groww `FAILED`).
    Failed,
    /// Expired at end of validity (terminal — Dhan `EXPIRED`).
    Expired,
    /// The broker reported a status string outside the known vocabulary.
    /// [`BrokerOrderEvent::raw_status`] preserves the original.
    Unknown,
}

/// How a broker order event reached us — the transport provenance.
///
/// The Orders area design distinguishes a REST status **poll** (we asked)
/// from an order-update stream **push** (the broker told us) so consumers can
/// weigh freshness + ordering accordingly (a push is event-time; a poll is a
/// point-in-time snapshot). Carried alongside [`BrokerOrderEvent`] by the
/// §39.3 area PRs' producers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventSource {
    /// A REST status poll produced this observation (point-in-time snapshot).
    Poll,
    /// An order-update stream push produced this observation (event-driven).
    Push,
}

/// Default for the [`push_active`](new_push_active_channel) flag: `false` — no
/// order-update PUSH feed is live at boot, so the REST status poller runs its
/// full adaptive cadence.
pub const PUSH_ACTIVE_DEFAULT: bool = false;

/// Creates the Session-3 order-update **PUSH-active** flag channel — a
/// [`tokio::sync::watch`] pair seeded to [`PUSH_ACTIVE_DEFAULT`] (`false`).
///
/// This is the additive PR-A0 seam (spec §4.11): the flag lives in `common`
/// so a FUTURE core/app producer (Session 3's order-update feed) and the
/// trading-side REST poller (consumer) can share it WITHOUT either depending
/// on `tickvault-trading` (dependency flow `common ← core ← trading`).
///
/// - **Producer (Session 3):** when the order-update push feed goes live it
///   `send(true)`; when it drops it `send(false)`.
/// - **Consumer (the poller):** reads the receiver. While `false` the poller
///   runs full cadence; when a producer flips it `true`, the poller MAY later
///   (a ratchet-gated change) degrade per-order cadence toward the reconcile
///   floor. A hint NEVER transitions state — it only schedules a targeted
///   status poll, so lossiness under lag is safe.
///
/// PR-A0 ships the seam only — there is NO production producer yet (Session 3
/// owns the flip). Returns `(Sender, Receiver)`; the caller wires ownership.
#[must_use]
// WIRING-EXEMPT: PR-A0 seam — the production producer (Session 3 order-update feed) and consumer (Orders poller) land in later serial §39.3 area PRs; tested inline.
pub fn new_push_active_channel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(PUSH_ACTIVE_DEFAULT)
}

impl BrokerOrderStatus {
    /// The stable label for this status (audit / metric-label use).
    #[must_use]
    // WIRING-EXEMPT: PR-0 seam — production call sites land in the serial §39.3 area PRs (Orders/Smart Orders/Portfolio/Margin/User); tested inline.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "NEW",
            Self::Pending => "PENDING",
            Self::Open => "OPEN",
            Self::PartiallyFilled => "PARTIALLY_FILLED",
            Self::Filled => "FILLED",
            Self::Cancelled => "CANCELLED",
            Self::Rejected => "REJECTED",
            Self::Failed => "FAILED",
            Self::Expired => "EXPIRED",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Whether this is a terminal state — no further lifecycle transition is
    /// expected. `Unknown` is NOT terminal (we cannot assert the order is
    /// done from a string we did not understand).
    #[must_use]
    // WIRING-EXEMPT: PR-0 seam — production call sites land in the serial §39.3 area PRs (Orders/Smart Orders/Portfolio/Margin/User); tested inline.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Rejected | Self::Failed | Self::Expired
        )
    }

    /// Map a Groww order-status string to the neutral status. Total + no-panic:
    /// any unrecognized value → [`BrokerOrderStatus::Unknown`].
    ///
    /// Source: the 12-value Groww Order Status annexure
    /// (`docs/groww-ref/16-orders-margins-portfolio.md` §4.1) PLUS the
    /// doc-observed-but-un-annexed `OPEN` (the orders page's own examples show
    /// `order_status: "OPEN"`; §4.1 flags it Unknown-provenance — we map it to
    /// `Open` because that is what a resting order means, and preserve the raw
    /// string regardless). Comparison is case-insensitive + whitespace-trimmed.
    #[must_use]
    // WIRING-EXEMPT: PR-0 seam — production call sites land in the serial §39.3 area PRs (Orders/Smart Orders/Portfolio/Margin/User); tested inline.
    pub fn from_groww_status(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "NEW" => Self::New,
            // ACKED = acknowledged by the system; TRIGGER_PENDING = waiting on a
            // trigger; *_REQUESTED = a cancel/modify request is in flight — all
            // non-terminal "in progress toward" states.
            "ACKED" | "TRIGGER_PENDING" | "CANCELLATION_REQUESTED" | "MODIFICATION_REQUESTED" => {
                Self::Pending
            }
            // APPROVED = approved and ready for execution (resting/open);
            // OPEN = the undocumented resting value from the doc examples.
            "APPROVED" | "OPEN" => Self::Open,
            // EXECUTED / DELIVERY_AWAITED / COMPLETED all mean the order is done.
            "EXECUTED" | "DELIVERY_AWAITED" | "COMPLETED" => Self::Filled,
            "CANCELLED" => Self::Cancelled,
            "REJECTED" => Self::Rejected,
            "FAILED" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    /// Map a Dhan order-status string to the neutral status. Total + no-panic:
    /// any unrecognized value → [`BrokerOrderStatus::Unknown`].
    ///
    /// The recognized set mirrors [`crate::order_types::OrderStatus`]
    /// (`docs/dhan-ref/08-annexure-enums.md` §6) plus the `PARTIALLY_FILLED`
    /// alias the OMS state machine already tolerates (OMS-GAP-01). Comparison
    /// is case-insensitive + whitespace-trimmed.
    #[must_use]
    // WIRING-EXEMPT: PR-0 seam — production call sites land in the serial §39.3 area PRs (Orders/Smart Orders/Portfolio/Margin/User); tested inline.
    pub fn from_dhan_status(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            // In-transit / pending → the working-toward state.
            "TRANSIT" | "PENDING" => Self::Pending,
            // CONFIRMED = open in the book; TRIGGERED = a Super/Forever
            // condition fired and the order is now working.
            "CONFIRMED" | "TRIGGERED" => Self::Open,
            "PART_TRADED" | "PARTIALLY_FILLED" => Self::PartiallyFilled,
            // TRADED = fully executed; CLOSED = Super Order all legs complete.
            "TRADED" | "CLOSED" => Self::Filled,
            "CANCELLED" => Self::Cancelled,
            "REJECTED" => Self::Rejected,
            "EXPIRED" => Self::Expired,
            _ => Self::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// Full-fidelity push-event capture records (order_update_events /
// position_update_events forensic tables — design 2026-07-18).
//
// These are AUDIT-FIDELITY records: they mirror the QuestDB table columns
// one-to-one so that EVERY field the broker push channels deliver survives
// into a queryable row. They deliberately diverge from the integer-paise
// discipline of [`BrokerOrderEvent`]: prices here are RUPEES `f64` because
// the table columns are DOUBLE and the Dhan wire already delivers f64
// rupees (Groww paise are converted to rupees AT RECORD CONSTRUCTION; the
// storage writer clamps non-finite values + counts them). The 11-field
// [`BrokerOrderEvent`] decision seam is UNTOUCHED by these records.
// ---------------------------------------------------------------------------

/// Bounded capacity of the two capture-record mpsc channels
/// (`order_update_events` / `position_update_events` — producer `try_send`,
/// consumer drains into the storage writers). 256 is ~minutes of buffer at
/// realistic order-event rates (the SEBI order cap is 10/sec and each order
/// produces a handful of lifecycle pushes); a full channel drops the ROW
/// (counted + coded `ORDER-EVT-01 stage="sink_drop"`), never blocks a push
/// read loop. Lives in `common` because BOTH producers feed it — the Dhan
/// consumer (app, compiled WITHOUT `groww_orders`) and the Groww push runner
/// (trading, behind `groww_orders`).
pub const ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY: usize = 256;

/// Process-global receipt sequence for push-event capture rows.
///
/// Starts at 1 (0 is never issued — a 0 in a row would be indistinguishable
/// from a forgotten stamp). Shared across BOTH capture tables so `event_seq`
/// is a monotone receipt order across the whole process, distinct within any
/// same-microsecond burst — which is exactly what makes the DEDUP keys
/// (`…, feed, order_id, event_seq` / `…, feed, symbol_isin, event_seq`)
/// burst-safe while an ILP retry of the SAME row (same seq) still collapses
/// idempotently.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Returns the next process-global push-event receipt sequence number.
///
/// Monotone + unique per process (relaxed `fetch_add` — ordering across
/// threads is by ticket issuance, which is all the DEDUP key needs). The
/// value is returned as `i64` because QuestDB `LONG` is signed; wrap-around
/// would need 2^63 events (never reachable in a process lifetime).
#[must_use]
pub fn next_event_seq() -> i64 {
    // Cast is lossless for any realistic process lifetime (see doc above).
    #[allow(clippy::cast_possible_wrap)] // APPROVED: 2^63 events unreachable; documented above
    {
        EVENT_SEQ.fetch_add(1, Ordering::Relaxed) as i64
    }
}

/// One full-fidelity order push event — every field either broker's order
/// push channel delivers, mapped one-to-one onto the `order_update_events`
/// QuestDB columns.
///
/// Producers fill broker-absent fields with the documented sentinels
/// (`-1` for numeric absents, empty `String` for textual absents — the
/// storage writer maps empty SYMBOL columns to the `"n/a"` sentinel and
/// omits empty STRING columns). Strings are carried RAW here; the storage
/// writer is the single sanitize + bound choke point
/// (`reject_reason` ≤300 chars, `detail_raw`/`stage_trail` ≤2000 chars).
#[derive(Debug, Clone, PartialEq)]
pub struct OrderUpdateEventRecord {
    /// Receipt time in IST epoch NANOS (the designated `ts`).
    pub ts_ist_nanos: i64,
    /// Which broker produced this event.
    pub feed: Feed,
    /// Process-global receipt sequence (from [`next_event_seq`]).
    pub event_seq: i64,
    /// Broker order id (Dhan `OrderNo` / Groww `growwOrderId`).
    pub order_id: String,
    /// Exchange order id (Dhan `ExchOrderNo` / Groww `exchangeOrderId`).
    pub exch_order_id: String,
    /// User correlation id (Dhan `CorrelationId`; empty for Groww push —
    /// the wire carries no reference).
    pub correlation_id: String,
    /// Canonical normalized status label ([`BrokerOrderStatus::as_str`]).
    pub status: String,
    /// Verbatim wire status (Dhan status string / Groww enum name or raw int).
    pub raw_status: String,
    /// Dhan `LegNo` (1/2/3); `-1` for Groww.
    pub leg_no: i32,
    /// Product label (Dhan single-char expanded / Groww enum label).
    pub product: String,
    /// Order type (Dhan LMT/MKT/SL/SLM; Groww MKT/L/SL/SL_M).
    pub order_type: String,
    /// BUY/SELL (Dhan B/S expanded; Groww buySell enum).
    pub transaction_type: String,
    /// Validity (Dhan DAY/IOC; Groww IOC/DAY/GTD/GTC/EOS).
    pub validity: String,
    /// Raw exchange label (NSE/BSE/…).
    pub exchange: String,
    /// Mapped segment slug (NSE_EQ/NSE_FNO/…/CASH/FNO/…).
    pub exchange_segment: String,
    /// Resolved security id; `-1` ONLY when unresolvable.
    pub security_id: i64,
    /// Trading symbol / contract symbol.
    pub symbol: String,
    /// ORDER quantity (Dhan `Quantity` / Groww `qty`).
    pub quantity: i64,
    /// Dhan `DiscQuantity`; `-1` for Groww.
    pub disclosed_qty: i64,
    /// Remaining quantity; `-1` when absent.
    pub remaining_qty: i64,
    /// Cumulative filled quantity (Dhan `TradedQty` / Groww `filledQty`).
    pub traded_qty: i64,
    /// Order price in RUPEES (Groww paise converted at construction).
    pub price: f64,
    /// Trigger price in RUPEES.
    pub trigger_price: f64,
    /// Average traded/fill price in RUPEES.
    pub avg_traded_price: f64,
    /// Last fill price in RUPEES (Dhan `TradedPrice`); `0.0` for Groww.
    pub last_traded_price: f64,
    /// Reject reason (Dhan `ReasonDescription`; Groww `remark` on a
    /// reject-class status — Assumed carrier, UNVERIFIED-LIVE).
    pub reject_reason: String,
    /// Free-form remarks (Dhan `Remarks` / Groww `remark`).
    pub remarks: String,
    /// Dhan `Source` (P/N); `"push"` for Groww.
    pub source: String,
    /// Dhan `OffMktFlag` ("1" = AMO); empty elsewhere.
    pub off_mkt_flag: String,
    /// Dhan `OptType` (CE/PE/XX); empty elsewhere.
    pub opt_type: String,
    /// Dhan `AlgoOrdNo` (Number→String coerced upstream); empty elsewhere.
    pub algo_ord_no: String,
    /// Dhan `AlgoId`; empty elsewhere.
    pub algo_id: String,
    /// Dhan `MktType` (NL/AU/A1/A2); empty elsewhere.
    pub mkt_type: String,
    /// Dhan `Series`; empty elsewhere.
    pub series: String,
    /// Dhan `GoodTillDaysDate` (Forever-Order validity); empty elsewhere.
    pub good_till_days_date: String,
    /// Dhan `RefLtp`; `0.0` elsewhere.
    pub ref_ltp: f64,
    /// Dhan `TickSize`; `0.0` elsewhere.
    pub tick_size: f64,
    /// Dhan `Multiplier`; `-1` elsewhere.
    pub multiplier: i32,
    /// Dhan `Instrument` (EQUITY/FUTIDX/…); empty elsewhere.
    pub instrument: String,
    /// Dhan `OrderDateTime` (IST string, verbatim); empty elsewhere.
    pub broker_create_time: String,
    /// Dhan `LastUpdatedTime` (IST string, verbatim); empty elsewhere.
    pub broker_update_time: String,
    /// Dhan `ExchOrderTime` (IST string, verbatim); empty elsewhere.
    pub exchange_time: String,
    /// Groww exchange epoch ms (today always absent → `-1`; reserved).
    pub exchange_ts_ms: i64,
    /// Groww `contractId` (ISIN / contract symbol); empty elsewhere.
    pub contract_id: String,
    /// Groww `guiOrderId`; empty elsewhere.
    pub gui_order_id: String,
    /// Groww stage trail (`stageName@timeStampFromMidNight` pairs); empty
    /// elsewhere. Bounded + sanitized by the storage writer.
    pub stage_trail: String,
    /// Sanitized full-fidelity remainder (Dhan ClientId/ProductName/
    /// DisplayName/Isin/LotSize/StrikePrice/ExpiryDate/DiscQtyRem etc.).
    /// Bounded by the storage writer (~2000 chars).
    pub detail_raw: String,
}

/// One full-fidelity Groww position push event — every decoded
/// `PositionDetailProto` field, mapped one-to-one onto the
/// `position_update_events` QuestDB columns. Carries a `feed` so a future
/// broker's position push reuses the same table.
///
/// The 8 per-exchange credit/debit legs are `Option<f64>`: an ABSENT leg
/// submessage stays `None` (the writer omits the columns → NULL), never a
/// fabricated `0.0` — a served zero and an absent leg remain distinguishable.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionUpdateEventRecord {
    /// Receipt time in IST epoch NANOS (the designated `ts`).
    pub ts_ist_nanos: i64,
    /// Which broker produced this event.
    pub feed: Feed,
    /// Process-global receipt sequence (from [`next_event_seq`]).
    pub event_seq: i64,
    /// `PositionInfo.symbolIsin` — the wire identity (DEDUP key member).
    pub symbol_isin: String,
    /// Resolved security id; `-1` when unresolvable (honest, never fabricated).
    pub security_id: i64,
    /// Mapped segment slug; raw-preserving fallback label when unknown.
    pub exchange_segment: String,
    /// Display/contract symbol fallback identity.
    pub symbol: String,
    /// `SymbolInfo.exchange` enum label (position-file mapping — the
    /// value-5 GLOBAL/US collision with the order file means per-channel
    /// mappers are mandatory upstream).
    pub exchange: String,
    /// `SymbolInfo.trTimeStamp` (units UNVERIFIED-LIVE; persisted verbatim).
    pub tr_time_stamp: i64,
    /// `SymbolInfo.searchId`.
    pub search_id: String,
    /// `SymbolInfo.stocksProduct` enum label (raw int in `detail_raw` on
    /// unknown).
    pub product: String,
    /// `SymbolInfo.contractId`.
    pub contract_id: String,
    /// `SymbolInfo.equityType` enum label.
    pub equity_type: String,
    /// `SymbolInfo.displayName`.
    pub display_name: String,
    /// `SymbolInfo.underlyingId`.
    pub underlying_id: String,
    /// `SymbolInfo.nseMarketLot`.
    pub nse_market_lot: i64,
    /// `SymbolInfo.bseMarketLot`.
    pub bse_market_lot: i64,
    /// `SymbolInfo.underlyingAssetType` enum label.
    pub underlying_asset_type: String,
    /// `SymbolInfo.freezeQty`.
    pub freeze_qty: i64,
    /// NSE leg credit quantity (`None` = leg absent on the wire).
    pub nse_credit_qty: Option<f64>,
    /// NSE leg credit price.
    pub nse_credit_price: Option<f64>,
    /// NSE leg debit quantity.
    pub nse_debit_qty: Option<f64>,
    /// NSE leg debit price.
    pub nse_debit_price: Option<f64>,
    /// BSE leg credit quantity (`None` = leg absent on the wire).
    pub bse_credit_qty: Option<f64>,
    /// BSE leg credit price.
    pub bse_credit_price: Option<f64>,
    /// BSE leg debit quantity.
    pub bse_debit_qty: Option<f64>,
    /// BSE leg debit price.
    pub bse_debit_price: Option<f64>,
    /// Sanitized remainder (raw enum ints on unknown labels etc.).
    /// Bounded by the storage writer (~2000 chars).
    pub detail_raw: String,
}

// ---------------------------------------------------------------------------
// Dhan producer half: OrderUpdate (the order-update WS wire struct) → the
// full-fidelity capture record. Lives HERE (not in the app crate) because
// both the wire struct and the record are `common` types and the mapping is
// pure — the app-side consumer/publisher stays a thin channel shim.
// ---------------------------------------------------------------------------

/// Expand the Dhan single-char product code when `ProductName` is absent
/// (`live-order-update.md` rule 6: C=CNC, I=INTRADAY, M=MARGIN, F=MTF,
/// V=CO, B=BO). An unknown code is kept VERBATIM (never guessed; the raw
/// code also rides `detail_raw`).
fn dhan_product_label(product_code: &str, product_name: &str) -> String {
    if !product_name.trim().is_empty() {
        return product_name.to_owned();
    }
    match product_code.trim().to_ascii_uppercase().as_str() {
        "C" => "CNC".to_owned(),
        "I" => "INTRADAY".to_owned(),
        "M" => "MARGIN".to_owned(),
        "F" => "MTF".to_owned(),
        "V" => "CO".to_owned(),
        "B" => "BO".to_owned(),
        _ => product_code.to_owned(),
    }
}

/// Expand the Dhan `TxnType` single char (`B`/`S`); anything else is kept
/// verbatim (honest — never guessed).
fn dhan_txn_label(txn_type: &str) -> String {
    if txn_type.eq_ignore_ascii_case("B") {
        "BUY".to_owned()
    } else if txn_type.eq_ignore_ascii_case("S") {
        "SELL".to_owned()
    } else {
        txn_type.to_owned()
    }
}

/// `(exchange, single-char segment)` → the canonical segment slug for the
/// NSE/BSE equity+derivative grid; empty string outside it (the writer maps
/// an empty SYMBOL column to `"n/a"`; the raw segment char always rides
/// `detail_raw`, so nothing is lost).
fn dhan_segment_slug(exchange: &str, segment: &str) -> &'static str {
    match (
        exchange.eq_ignore_ascii_case("NSE"),
        exchange.eq_ignore_ascii_case("BSE"),
        segment.eq_ignore_ascii_case("E"),
        segment.eq_ignore_ascii_case("D"),
    ) {
        (true, _, true, _) => "NSE_EQ",
        (true, _, _, true) => "NSE_FNO",
        (_, true, true, _) => "BSE_EQ",
        (_, true, _, true) => "BSE_FNO",
        _ => "",
    }
}

/// Build the full-fidelity capture record for one Dhan order-update WS
/// message. Pure + total: every one of the wire struct's fields lands in a
/// first-class column or the `detail_raw` remainder (completeness is pinned
/// by `test_dhan_order_update_record_covers_every_field`); nothing is ever
/// fabricated — Groww-only columns carry the documented sentinels.
///
/// `ts_ist_nanos` is the RECEIPT stamp (IST epoch nanos), injected so the
/// mapping stays deterministic under test.
#[must_use]
pub fn build_dhan_order_event_record(
    update: &OrderUpdate,
    ts_ist_nanos: i64,
) -> OrderUpdateEventRecord {
    OrderUpdateEventRecord {
        ts_ist_nanos,
        feed: Feed::Dhan,
        event_seq: next_event_seq(),
        order_id: update.order_no.clone(),
        exch_order_id: update.exch_order_no.clone(),
        correlation_id: update.correlation_id.clone(),
        status: BrokerOrderStatus::from_dhan_status(&update.status)
            .as_str()
            .to_owned(),
        raw_status: update.status.clone(),
        // Wire LegNo is 1/2/3 (0 when the field was absent — persisted
        // verbatim); an out-of-i32-range value is the -1 absent sentinel.
        leg_no: i32::try_from(update.leg_no).unwrap_or(-1),
        product: dhan_product_label(&update.product, &update.product_name),
        order_type: update.order_type.clone(),
        transaction_type: dhan_txn_label(&update.txn_type),
        validity: update.validity.clone(),
        exchange: update.exchange.clone(),
        exchange_segment: dhan_segment_slug(&update.exchange, &update.segment).to_owned(),
        security_id: update.security_id.trim().parse::<i64>().unwrap_or(-1),
        symbol: update.symbol.clone(),
        quantity: update.quantity,
        disclosed_qty: update.disc_quantity,
        remaining_qty: update.remaining_quantity,
        traded_qty: update.traded_qty,
        // Dhan wire prices are already RUPEES f64 — verbatim.
        price: update.price,
        trigger_price: update.trigger_price,
        avg_traded_price: update.avg_traded_price,
        last_traded_price: update.traded_price,
        // ReasonDescription carries the reject reason on reject-class
        // statuses and confirmation text otherwise — persisted VERBATIM
        // (the writer bounds it); classification is a consumer concern.
        reject_reason: update.reason_description.clone(),
        remarks: update.remarks.clone(),
        source: update.source.clone(),
        off_mkt_flag: update.off_mkt_flag.clone(),
        opt_type: update.opt_type.clone(),
        algo_ord_no: update.algo_ord_no.clone(),
        algo_id: update.algo_id.clone(),
        mkt_type: update.mkt_type.clone(),
        series: update.series.clone(),
        good_till_days_date: update.good_till_days_date.clone(),
        ref_ltp: update.ref_ltp,
        tick_size: update.tick_size,
        multiplier: i32::try_from(update.multiplier).unwrap_or(-1),
        instrument: update.instrument.clone(),
        broker_create_time: update.order_date_time.clone(),
        broker_update_time: update.last_updated_time.clone(),
        exchange_time: update.exch_order_time.clone(),
        // Groww-only columns: the documented absent sentinels.
        exchange_ts_ms: -1,
        contract_id: String::new(),
        gui_order_id: String::new(),
        stage_trail: String::new(),
        // The full-fidelity remainder — every wire field WITHOUT a
        // first-class column (the storage writer sanitizes + bounds it).
        detail_raw: format!(
            "client_id={} product_code={} segment={} display_name={} isin={} lot_size={} strike_price={} expiry_date={} disc_qty_rem={}",
            update.client_id,
            update.product,
            update.segment,
            update.display_name,
            update.isin,
            update.lot_size,
            update.strike_price,
            update.expiry_date,
            update.disc_qty_rem,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- as_str: every variant has a stable, non-empty label ---

    #[test]
    fn test_broker_order_status_as_str_all_variants() {
        assert_eq!(BrokerOrderStatus::New.as_str(), "NEW");
        assert_eq!(BrokerOrderStatus::Pending.as_str(), "PENDING");
        assert_eq!(BrokerOrderStatus::Open.as_str(), "OPEN");
        assert_eq!(
            BrokerOrderStatus::PartiallyFilled.as_str(),
            "PARTIALLY_FILLED"
        );
        assert_eq!(BrokerOrderStatus::Filled.as_str(), "FILLED");
        assert_eq!(BrokerOrderStatus::Cancelled.as_str(), "CANCELLED");
        assert_eq!(BrokerOrderStatus::Rejected.as_str(), "REJECTED");
        assert_eq!(BrokerOrderStatus::Failed.as_str(), "FAILED");
        assert_eq!(BrokerOrderStatus::Expired.as_str(), "EXPIRED");
        assert_eq!(BrokerOrderStatus::Unknown.as_str(), "UNKNOWN");
    }

    // --- is_terminal ---

    #[test]
    fn test_is_terminal_classification() {
        for s in [
            BrokerOrderStatus::Filled,
            BrokerOrderStatus::Cancelled,
            BrokerOrderStatus::Rejected,
            BrokerOrderStatus::Failed,
            BrokerOrderStatus::Expired,
        ] {
            assert!(s.is_terminal(), "{} must be terminal", s.as_str());
        }
        for s in [
            BrokerOrderStatus::New,
            BrokerOrderStatus::Pending,
            BrokerOrderStatus::Open,
            BrokerOrderStatus::PartiallyFilled,
            BrokerOrderStatus::Unknown,
        ] {
            assert!(!s.is_terminal(), "{} must not be terminal", s.as_str());
        }
    }

    // --- from_groww_status: every documented value maps non-Unknown ---

    #[test]
    fn test_from_groww_every_documented_value_maps_non_unknown() {
        // The 12 annexure values (§4.1) + the observed OPEN.
        let documented = [
            "NEW",
            "ACKED",
            "TRIGGER_PENDING",
            "APPROVED",
            "REJECTED",
            "FAILED",
            "EXECUTED",
            "DELIVERY_AWAITED",
            "CANCELLED",
            "CANCELLATION_REQUESTED",
            "MODIFICATION_REQUESTED",
            "COMPLETED",
            "OPEN", // undocumented-but-observed
        ];
        for v in documented {
            assert_ne!(
                BrokerOrderStatus::from_groww_status(v),
                BrokerOrderStatus::Unknown,
                "documented Groww status `{v}` must not map to Unknown"
            );
        }
    }

    #[test]
    fn test_from_groww_status_specific_mappings() {
        assert_eq!(
            BrokerOrderStatus::from_groww_status("NEW"),
            BrokerOrderStatus::New
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("ACKED"),
            BrokerOrderStatus::Pending
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("APPROVED"),
            BrokerOrderStatus::Open
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("OPEN"),
            BrokerOrderStatus::Open
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("EXECUTED"),
            BrokerOrderStatus::Filled
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("COMPLETED"),
            BrokerOrderStatus::Filled
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("FAILED"),
            BrokerOrderStatus::Failed
        );
    }

    #[test]
    fn test_from_groww_garbage_and_empty_map_unknown_no_panic() {
        assert_eq!(
            BrokerOrderStatus::from_groww_status(""),
            BrokerOrderStatus::Unknown
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("\u{0}\u{7}garbage\u{feff}"),
            BrokerOrderStatus::Unknown
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("SOMETHING_NEW_2027"),
            BrokerOrderStatus::Unknown
        );
    }

    #[test]
    fn test_from_groww_is_case_and_whitespace_insensitive() {
        assert_eq!(
            BrokerOrderStatus::from_groww_status("  executed  "),
            BrokerOrderStatus::Filled
        );
        assert_eq!(
            BrokerOrderStatus::from_groww_status("Approved"),
            BrokerOrderStatus::Open
        );
    }

    // --- from_dhan_status: every documented value maps non-Unknown ---

    #[test]
    fn test_from_dhan_every_order_status_value_maps_non_unknown() {
        // Every crate::order_types::OrderStatus wire label, via as_str().
        use crate::order_types::OrderStatus;
        let all = [
            OrderStatus::Transit,
            OrderStatus::Pending,
            OrderStatus::Confirmed,
            OrderStatus::PartTraded,
            OrderStatus::Traded,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Expired,
            OrderStatus::Closed,
            OrderStatus::Triggered,
        ];
        for s in all {
            assert_ne!(
                BrokerOrderStatus::from_dhan_status(s.as_str()),
                BrokerOrderStatus::Unknown,
                "documented Dhan status `{}` must not map to Unknown",
                s.as_str()
            );
        }
        // The alias the OMS state machine tolerates (OMS-GAP-01).
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("PARTIALLY_FILLED"),
            BrokerOrderStatus::PartiallyFilled
        );
    }

    #[test]
    fn test_from_dhan_specific_mappings() {
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("TRADED"),
            BrokerOrderStatus::Filled
        );
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("PART_TRADED"),
            BrokerOrderStatus::PartiallyFilled
        );
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("CONFIRMED"),
            BrokerOrderStatus::Open
        );
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("CLOSED"),
            BrokerOrderStatus::Filled
        );
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("EXPIRED"),
            BrokerOrderStatus::Expired
        );
    }

    #[test]
    fn test_from_dhan_garbage_and_empty_map_unknown_no_panic() {
        assert_eq!(
            BrokerOrderStatus::from_dhan_status(""),
            BrokerOrderStatus::Unknown
        );
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("not-a-status"),
            BrokerOrderStatus::Unknown
        );
    }

    #[test]
    fn test_from_dhan_is_case_and_whitespace_insensitive() {
        assert_eq!(
            BrokerOrderStatus::from_dhan_status(" traded "),
            BrokerOrderStatus::Filled
        );
        assert_eq!(
            BrokerOrderStatus::from_dhan_status("rejected"),
            BrokerOrderStatus::Rejected
        );
    }

    // --- BrokerOrderEvent: construction + raw-status preservation on Unknown ---

    #[test]
    fn test_broker_order_event_preserves_raw_status_when_unknown() {
        let raw = "SOME_FUTURE_STATE";
        let ev = BrokerOrderEvent {
            broker: Feed::Groww,
            broker_order_id: "GRW-123".to_owned(),
            reference_id: Some("ref-0001".to_owned()),
            status: BrokerOrderStatus::from_groww_status(raw),
            raw_status: raw.to_owned(),
            filled_qty: 0,
            remaining_qty: Some(50),
            avg_fill_price_paise: None,
            segment: "FNO".to_owned(),
            exchange_ts_ms: None,
            received_at_ms: 1_760_000_000_000,
        };
        // Un-mapped wire value is Unknown, but never lost.
        assert_eq!(ev.status, BrokerOrderStatus::Unknown);
        assert_eq!(ev.raw_status, "SOME_FUTURE_STATE");
        assert_eq!(ev.broker, Feed::Groww);
        assert_eq!(ev.avg_fill_price_paise, None);
    }

    #[test]
    fn test_broker_order_event_filled_integer_paise() {
        let ev = BrokerOrderEvent {
            broker: Feed::Dhan,
            broker_order_id: "1234567890".to_owned(),
            reference_id: None,
            status: BrokerOrderStatus::from_dhan_status("TRADED"),
            raw_status: "TRADED".to_owned(),
            filled_qty: 50,
            remaining_qty: Some(0),
            avg_fill_price_paise: Some(24_550), // 245.50 rupees, integer paise
            segment: "D".to_owned(),
            exchange_ts_ms: Some(1_760_000_045_000),
            received_at_ms: 1_760_000_045_120,
        };
        assert_eq!(ev.status, BrokerOrderStatus::Filled);
        assert!(ev.status.is_terminal());
        assert_eq!(ev.avg_fill_price_paise, Some(24_550));
    }
    // --- EventSource: the two transport provenances stay distinct + copyable ---

    #[test]
    fn test_event_source_variants_distinct() {
        assert_ne!(EventSource::Poll, EventSource::Push);
        // Copy semantics: the same value compares equal after an implicit copy.
        let s = EventSource::Push;
        let t = s;
        assert_eq!(s, t);
        assert_eq!(format!("{:?}", EventSource::Poll), "Poll");
        assert_eq!(format!("{:?}", EventSource::Push), "Push");
    }

    // --- push_active seam (Session-3 order-update PUSH flag) ---

    #[test]
    fn test_push_active_channel_defaults_false_and_flips() {
        // Seeded to the default: no push feed live at boot.
        assert!(!PUSH_ACTIVE_DEFAULT);
        let (tx, rx) = new_push_active_channel();
        assert!(
            !*rx.borrow(),
            "push_active must start false (poller full cadence)"
        );
        // A producer (Session 3) flips it true; the consumer sees it.
        tx.send(true).expect("receiver alive"); // APPROVED: test
        assert!(
            *rx.borrow(),
            "consumer must observe the producer's flip to true"
        );
        tx.send(false).expect("receiver alive"); // APPROVED: test
        assert!(
            !*rx.borrow(),
            "consumer must observe the drop back to false"
        );
    }

    // --- full-fidelity capture records (order_update_events design 2026-07-18) ---

    #[test]
    fn test_next_event_seq_is_monotonic_and_unique() {
        // Single-thread monotonicity: strictly increasing, never 0.
        let a = next_event_seq();
        let b = next_event_seq();
        let c = next_event_seq();
        assert!(a > 0, "seq must never issue 0 (0 = forgotten stamp)");
        assert!(b > a && c > b, "seq must be strictly increasing");

        // Cross-thread uniqueness: N threads x M draws all distinct.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| (0..100).map(|_| next_event_seq()).collect::<Vec<i64>>())
            })
            .collect();
        let mut all: Vec<i64> = Vec::new();
        for h in handles {
            all.extend(h.join().expect("seq thread must not panic")); // APPROVED: test
        }
        let mut deduped = all.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            all.len(),
            "every issued event_seq must be unique across threads"
        );
    }

    #[test]
    fn test_order_update_event_record_construction() {
        let rec = OrderUpdateEventRecord {
            ts_ist_nanos: 1_760_000_000_000_000_000,
            feed: Feed::Dhan,
            event_seq: next_event_seq(),
            order_id: "5225022912912".to_owned(),
            exch_order_id: "1300000012345".to_owned(),
            correlation_id: "TV-abc123".to_owned(),
            status: BrokerOrderStatus::Filled.as_str().to_owned(),
            raw_status: "TRADED".to_owned(),
            leg_no: 1,
            product: "INTRADAY".to_owned(),
            order_type: "LMT".to_owned(),
            transaction_type: "BUY".to_owned(),
            validity: "DAY".to_owned(),
            exchange: "NSE".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            security_id: 49_081,
            symbol: "NIFTY-Jul2026-24500-CE".to_owned(),
            quantity: 75,
            disclosed_qty: 0,
            remaining_qty: 0,
            traded_qty: 75,
            price: 122.50,
            trigger_price: 0.0,
            avg_traded_price: 122.45,
            last_traded_price: 122.45,
            reject_reason: String::new(),
            remarks: String::new(),
            source: "P".to_owned(),
            off_mkt_flag: "0".to_owned(),
            opt_type: "CE".to_owned(),
            algo_ord_no: String::new(),
            algo_id: String::new(),
            mkt_type: "NL".to_owned(),
            series: "EQ".to_owned(),
            good_till_days_date: String::new(),
            ref_ltp: 122.40,
            tick_size: 0.05,
            multiplier: 1,
            instrument: "OPTIDX".to_owned(),
            broker_create_time: "2026-07-18 09:20:01".to_owned(),
            broker_update_time: "2026-07-18 09:20:02".to_owned(),
            exchange_time: "2026-07-18 09:20:02".to_owned(),
            exchange_ts_ms: -1,
            contract_id: String::new(),
            gui_order_id: String::new(),
            stage_trail: String::new(),
            detail_raw: "client_id=1106656882".to_owned(),
        };
        assert_eq!(rec.feed, Feed::Dhan);
        assert_eq!(rec.status, "FILLED");
        assert_eq!(rec.raw_status, "TRADED");
        assert!(rec.event_seq > 0);
        // Groww-only fields honestly empty / sentinel on a Dhan record.
        assert_eq!(rec.exchange_ts_ms, -1);
        assert!(rec.contract_id.is_empty());
        // Clone + PartialEq round-trip (plain-data contract).
        let dup = rec.clone();
        assert_eq!(dup, rec);
    }

    #[test]
    fn test_position_update_event_record_construction() {
        let rec = PositionUpdateEventRecord {
            ts_ist_nanos: 1_760_000_000_000_000_000,
            feed: Feed::Groww,
            event_seq: next_event_seq(),
            symbol_isin: "NIFTY26JUL24500CE".to_owned(),
            security_id: -1,
            exchange_segment: "FNO".to_owned(),
            symbol: "NIFTY 24500 CE".to_owned(),
            exchange: "NSE".to_owned(),
            tr_time_stamp: 1_760_000_000,
            search_id: "nifty-24500-ce".to_owned(),
            product: "MIS".to_owned(),
            contract_id: "NIFTY26JUL24500CE".to_owned(),
            equity_type: "OPTION".to_owned(),
            display_name: "NIFTY 24500 CE".to_owned(),
            underlying_id: "nifty".to_owned(),
            nse_market_lot: 75,
            bse_market_lot: 0,
            underlying_asset_type: "INDICES".to_owned(),
            freeze_qty: 1_800,
            nse_credit_qty: Some(75.0),
            nse_credit_price: Some(120.0),
            nse_debit_qty: Some(0.0),
            nse_debit_price: Some(0.0),
            bse_credit_qty: None,
            bse_credit_price: None,
            bse_debit_qty: None,
            bse_debit_price: None,
            detail_raw: String::new(),
        };
        assert_eq!(rec.feed, Feed::Groww);
        // Absent BSE leg stays None (never a fabricated 0.0) while the
        // served NSE zero stays Some(0.0) — the two are distinguishable.
        assert!(rec.bse_credit_qty.is_none());
        assert_eq!(rec.nse_debit_qty, Some(0.0));
        assert_eq!(rec.security_id, -1, "unresolvable id is the -1 sentinel");
        let dup = rec.clone();
        assert_eq!(dup, rec);
    }

    // --- Dhan producer builder (build_dhan_order_event_record) ---

    #[test]
    fn test_order_update_events_channel_capacity_is_bounded_nonzero() {
        assert!(ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY > 0);
        assert!(
            ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY <= 4096,
            "bounded, never unbounded"
        );
    }

    #[test]
    fn test_dhan_product_label_expansion_and_verbatim_fallback() {
        // ProductName wins when present.
        assert_eq!(dhan_product_label("I", "INTRADAY"), "INTRADAY");
        // Single-char expansion per live-order-update.md rule 6.
        assert_eq!(dhan_product_label("C", ""), "CNC");
        assert_eq!(dhan_product_label("I", ""), "INTRADAY");
        assert_eq!(dhan_product_label("M", ""), "MARGIN");
        assert_eq!(dhan_product_label("F", ""), "MTF");
        assert_eq!(dhan_product_label("V", ""), "CO");
        assert_eq!(dhan_product_label("B", ""), "BO");
        // Unknown code kept verbatim — never guessed.
        assert_eq!(dhan_product_label("Z", ""), "Z");
        assert_eq!(dhan_product_label("", ""), "");
    }

    #[test]
    fn test_dhan_txn_label_and_segment_slug() {
        assert_eq!(dhan_txn_label("B"), "BUY");
        assert_eq!(dhan_txn_label("s"), "SELL");
        assert_eq!(dhan_txn_label("X"), "X"); // verbatim, never guessed
        assert_eq!(dhan_txn_label(""), "");
        assert_eq!(dhan_segment_slug("NSE", "E"), "NSE_EQ");
        assert_eq!(dhan_segment_slug("NSE", "D"), "NSE_FNO");
        assert_eq!(dhan_segment_slug("BSE", "E"), "BSE_EQ");
        assert_eq!(dhan_segment_slug("BSE", "D"), "BSE_FNO");
        assert_eq!(dhan_segment_slug("MCX", "M"), "");
        assert_eq!(dhan_segment_slug("", ""), "");
    }

    /// Wire-shaped OrderUpdate via serde defaults (every field is
    /// `#[serde(default)]`; the struct has no `Default` impl).
    fn dhan_update_all_fields_distinct() -> OrderUpdate {
        let mut u: OrderUpdate =
            serde_json::from_str("{}").expect("OrderUpdate fields are serde-default"); // APPROVED: test
        u.exchange = "NSE".to_owned();
        u.segment = "D".to_owned();
        u.security_id = "424242".to_owned();
        u.client_id = "CID-9001".to_owned();
        u.order_no = "ORD-1001".to_owned();
        u.exch_order_no = "EXCH-1002".to_owned();
        u.product = "V".to_owned();
        u.txn_type = "B".to_owned();
        u.order_type = "SLM-1003".to_owned();
        u.validity = "IOC-1004".to_owned();
        u.quantity = 1_005;
        u.traded_qty = 1_006;
        u.remaining_quantity = 1_007;
        u.price = 1_008.25;
        u.trigger_price = 1_009.25;
        u.traded_price = 1_010.25;
        u.avg_traded_price = 1_011.25;
        u.status = "TRADED".to_owned();
        u.symbol = "SYM-1012".to_owned();
        u.display_name = "DISP-1013".to_owned();
        u.correlation_id = "CORR-1014".to_owned();
        u.remarks = "REM-1015".to_owned();
        u.reason_description = "REASON-1016".to_owned();
        u.order_date_time = "CREATE-1017".to_owned();
        u.exch_order_time = "EXCHT-1018".to_owned();
        u.last_updated_time = "UPD-1019".to_owned();
        u.instrument = "INSTR-1020".to_owned();
        u.lot_size = 1_021;
        u.strike_price = 1_022.5;
        u.expiry_date = "EXP-1023".to_owned();
        u.opt_type = "OPT-1024".to_owned();
        u.isin = "ISIN-1025".to_owned();
        u.disc_quantity = 1_026;
        u.disc_qty_rem = 1_027;
        u.leg_no = 2;
        u.product_name = String::new(); // exercise the single-char expansion
        u.ref_ltp = 1_028.75;
        u.tick_size = 0.05;
        u.source = "SRC-1029".to_owned();
        u.off_mkt_flag = "OMF-1030".to_owned();
        u.algo_ord_no = "AON-1031".to_owned();
        u.mkt_type = "MKT-1032".to_owned();
        u.series = "SER-1033".to_owned();
        u.good_till_days_date = "GTD-1034".to_owned();
        u.algo_id = "AID-1035".to_owned();
        u.multiplier = 1_036;
        u
    }

    /// COMPLETENESS: every one of the 46 OrderUpdate wire fields must land
    /// in a first-class record column or the detail_raw remainder — a new
    /// wire field added without a record mapping fails this test's
    /// exhaustive destructure below.
    #[test]
    #[allow(clippy::too_many_lines)] // APPROVED: exhaustive 46-field completeness pin
    fn test_dhan_order_update_record_covers_every_field() {
        let u = dhan_update_all_fields_distinct();
        let rec = build_dhan_order_event_record(&u, 555);

        // Exhaustive destructure: adding a field to OrderUpdate without
        // extending this test is a COMPILE error (no `..` rest pattern).
        let OrderUpdate {
            exchange,
            segment,
            security_id,
            client_id,
            order_no,
            exch_order_no,
            product,
            txn_type,
            order_type,
            validity,
            quantity,
            traded_qty,
            remaining_quantity,
            price,
            trigger_price,
            traded_price,
            avg_traded_price,
            status,
            symbol,
            display_name,
            correlation_id,
            remarks,
            reason_description,
            order_date_time,
            exch_order_time,
            last_updated_time,
            instrument,
            lot_size,
            strike_price,
            expiry_date,
            opt_type,
            isin,
            disc_quantity,
            disc_qty_rem,
            leg_no,
            product_name,
            ref_ltp,
            tick_size,
            source,
            off_mkt_flag,
            algo_ord_no,
            mkt_type,
            series,
            good_till_days_date,
            algo_id,
            multiplier,
        } = u;

        // Receipt stamp + identity.
        assert_eq!(rec.ts_ist_nanos, 555);
        assert_eq!(rec.feed, Feed::Dhan);
        assert!(rec.event_seq > 0);

        // First-class column mappings (one assert per wire field).
        assert_eq!(rec.exchange, exchange);
        assert_eq!(rec.security_id, 424_242, "parsed from {security_id}");
        assert_eq!(rec.order_id, order_no);
        assert_eq!(rec.exch_order_id, exch_order_no);
        assert_eq!(rec.transaction_type, "BUY", "expanded from {txn_type}");
        assert_eq!(rec.order_type, order_type);
        assert_eq!(rec.validity, validity);
        assert_eq!(rec.quantity, quantity);
        assert_eq!(rec.traded_qty, traded_qty);
        assert_eq!(rec.remaining_qty, remaining_quantity);
        assert!((rec.price - price).abs() < f64::EPSILON);
        assert!((rec.trigger_price - trigger_price).abs() < f64::EPSILON);
        assert!((rec.last_traded_price - traded_price).abs() < f64::EPSILON);
        assert!((rec.avg_traded_price - avg_traded_price).abs() < f64::EPSILON);
        assert_eq!(rec.raw_status, status);
        assert_eq!(rec.status, "FILLED", "normalized from {status}");
        assert_eq!(rec.symbol, symbol);
        assert_eq!(rec.correlation_id, correlation_id);
        assert_eq!(rec.remarks, remarks);
        assert_eq!(rec.reject_reason, reason_description);
        assert_eq!(rec.broker_create_time, order_date_time);
        assert_eq!(rec.exchange_time, exch_order_time);
        assert_eq!(rec.broker_update_time, last_updated_time);
        assert_eq!(rec.instrument, instrument);
        assert_eq!(rec.opt_type, opt_type);
        assert_eq!(rec.disclosed_qty, disc_quantity);
        assert_eq!(i64::from(rec.leg_no), leg_no);
        assert_eq!(
            rec.product, "CO",
            "expanded from code {product} (name `{product_name}` empty)"
        );
        assert!((rec.ref_ltp - ref_ltp).abs() < f64::EPSILON);
        assert!((rec.tick_size - tick_size).abs() < f64::EPSILON);
        assert_eq!(rec.source, source);
        assert_eq!(rec.off_mkt_flag, off_mkt_flag);
        assert_eq!(rec.algo_ord_no, algo_ord_no);
        assert_eq!(rec.mkt_type, mkt_type);
        assert_eq!(rec.series, series);
        assert_eq!(rec.good_till_days_date, good_till_days_date);
        assert_eq!(rec.algo_id, algo_id);
        assert_eq!(i64::from(rec.multiplier), multiplier);
        assert_eq!(
            rec.exchange_segment, "NSE_FNO",
            "mapped from ({exchange:?}, {segment:?})"
        );

        // detail_raw remainder — the fields without first-class columns.
        for needle in [
            client_id.as_str(),
            "product_code=V",
            "segment=D",
            display_name.as_str(),
            isin.as_str(),
            &lot_size.to_string(),
            &strike_price.to_string(),
            expiry_date.as_str(),
            &disc_qty_rem.to_string(),
        ] {
            assert!(
                rec.detail_raw.contains(needle),
                "detail_raw must carry `{needle}`: {}",
                rec.detail_raw
            );
        }

        // Groww-only columns stay the documented sentinels on a Dhan record.
        assert_eq!(rec.exchange_ts_ms, -1);
        assert!(rec.contract_id.is_empty());
        assert!(rec.gui_order_id.is_empty());
        assert!(rec.stage_trail.is_empty());
    }

    #[test]
    fn test_dhan_builder_sentinels_on_absent_wire_fields() {
        // A wire-default update (serde `{}`) — every field absent.
        let u: OrderUpdate =
            serde_json::from_str("{}").expect("OrderUpdate fields are serde-default"); // APPROVED: test
        let rec = build_dhan_order_event_record(&u, 1);
        assert_eq!(rec.security_id, -1, "unparseable SID is the -1 sentinel");
        assert_eq!(rec.status, "UNKNOWN", "empty status normalizes to UNKNOWN");
        assert!(rec.raw_status.is_empty(), "raw preserved verbatim (empty)");
        assert_eq!(rec.exchange_segment, "", "outside the grid stays empty");
        assert_eq!(rec.leg_no, 0, "wire-default 0 persisted verbatim");
        assert_eq!(rec.multiplier, 0, "wire-default 0 persisted verbatim");
    }

    #[test]
    fn test_dhan_builder_out_of_range_leg_and_multiplier_are_sentinel() {
        let mut u: OrderUpdate =
            serde_json::from_str("{}").expect("OrderUpdate fields are serde-default"); // APPROVED: test
        u.leg_no = i64::MAX;
        u.multiplier = i64::MIN;
        let rec = build_dhan_order_event_record(&u, 1);
        assert_eq!(rec.leg_no, -1, "out-of-i32-range leg_no → -1 sentinel");
        assert_eq!(
            rec.multiplier, -1,
            "out-of-i32-range multiplier → -1 sentinel"
        );
    }
}
