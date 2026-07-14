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
    fn test_from_groww_specific_mappings() {
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
}
