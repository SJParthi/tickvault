//! Canonical broker-portfolio snapshot types + the process-global fail-closed
//! snapshot handle every consumer reads. Lives in `common` (below every other
//! crate) so the shape is importable everywhere — the
//! [`crate::broker_order_events`] reasoning; the broker id REUSES
//! [`crate::feed::Feed`].
//!
//! ## Scope (Groww Portfolio area 6c.1 contract stubs, operator §39 2026-07-14)
//! Pure data + total no-panic helpers: NO request, NO live-fire gate, NO
//! table write. Fetch/tier code lands gated in
//! `crates/trading/src/oms/groww/portfolio.rs` (feature `groww_orders`,
//! Gate-1 `groww_orders.portfolio_read` / `margin_read`); persistence in
//! `crates/storage/src/broker_portfolio_persistence.rs`. Runbook:
//! `.claude/rules/project/groww-portfolio-error-codes.md`.
//!
//! ## Discipline + the fail-closed consumer contract (§38.8)
//! All money is integer PAISE (`i64`) — never `f64` (`data-integrity.md`);
//! quantities are `i64`; absent wire fields are `Option`, never a sentinel.
//! The ONE sentinel is the `product` DEDUP SYMBOL label (`"UNKNOWN"`, see
//! [`product_dedup_symbol`]) — a nullable DEDUP-key column is
//! UNVERIFIED-LIVE on QuestDB, so the key label is never NULL.
//! `None` ∨ not-[`SnapshotStatus::Complete`] ∨ age > `staleness_max_secs` ⇒
//! FAIL CLOSED: no decision, no flatten, risk-halt posture at live time. A
//! failed cycle NEVER republishes with a fresh stamp. Consumers key by
//! `(security_id, exchange_segment)` per I-P1-11 — never `security_id` alone.

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwapOption;

use crate::feed::Feed;
use crate::types::ExchangeSegment;

/// How a derived (non-wire) value was established. `net_qty = Σ credit −
/// debit` is OUR derivation, flagged [`Derivation::Assumed`] until verified
/// opportunistically (margin-usage consistency, carry-forward arithmetic, or
/// a future fill/stream equality). CASH rows have NO stream cross-check even
/// in the future (the FNO position-update WS is derivatives-only) and may
/// stay `Assumed` indefinitely — the flag is always visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Derivation {
    /// Derived by our arithmetic; NOT yet independently corroborated.
    Assumed,
    /// Corroborated by at least one independent signal.
    Verified,
}

/// Groww product vocabulary (Verified enum). Per-row presence on the wire is
/// a probe item (Unknown) — hence `Option<GrowwProduct>` + the sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrowwProduct {
    Cnc,  // cash-and-carry delivery
    Mis,  // intraday
    Nrml, // normal (carry-forward F&O)
}

impl GrowwProduct {
    /// Stable wire/storage label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cnc => "CNC",
            Self::Mis => "MIS",
            Self::Nrml => "NRML",
        }
    }
}

/// DEDUP-key SYMBOL label: `"UNKNOWN"` when absent — NEVER NULL in the key.
#[must_use]
pub const fn product_dedup_symbol(product: Option<GrowwProduct>) -> &'static str {
    match product {
        Some(p) => p.as_str(),
        None => "UNKNOWN",
    }
}

/// Which scheduler tier produced a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotTier {
    Probe,         // one-shot boot field-inventory probe
    T1Intraday,    // every `cadence_secs_*` in [09:15, 15:15) IST
    T2PreClose,    // 15:20 IST pre-close sweep
    T3OrphanCheck, // 15:25 IST orphan-check (the Groww Dhan-watchdog parallel)
    T4PostClose,   // 15:35 IST authoritative daily book (+COMMODITY +holdings)
    Poke,          // rate-floored on-demand poke (`Arc<Notify>`, ≥60s)
}

/// Snapshot health verdict — the fail-closed classification. STALENESS is
/// deliberately NOT a stored status: age is computed at READ time from
/// `fetched_at_epoch_secs` ([`PortfolioSnapshot::is_fresh`]), so a snapshot
/// can never claim freshness it no longer has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotStatus {
    /// Every leg ok, zero unresolved rows — the ONE decision-eligible state.
    Complete,
    /// A leg FAILED or legs came from different cycles. Honest bound: `Torn`
    /// does NOT detect a fill landing between two SUCCESSFUL legs (no broker
    /// snapshot version exists) — undetected ≤ one cycle budget.
    Torn,
    /// Legs answered but ≥1 row unresolved / suspect (e.g. the margin
    /// false-flat contradiction) — visibility only, never decisions.
    Incomplete,
    /// Cold-boot first cycle: empty book AND margin-quiet — provisionally
    /// flat, BLIND on transition detection. Never "confirmed flat".
    OkEmptyColdBoot,
}

/// One broker position row, normalized. Identity is the resolved
/// `(security_id, exchange_segment)` composite per I-P1-11 — `security_id`
/// is the Groww `exchange_token` resolved via the Groww master, NEVER a wire
/// field trusted for identity. NO documented unrealized-P&L wire field —
/// client-side MTM only, deliberately ABSENT here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerPosition {
    /// Resolved instrument id (Groww exchange_token namespace).
    pub security_id: u64,
    /// Canonical segment — the I-P1-11 composite-key partner.
    pub exchange_segment: ExchangeSegment,
    pub trading_symbol: String, // verbatim wire symbol (audit; resolution input)
    pub isin: Option<String>,   // when the wire carried it
    /// Product, when present per row (probe item; `"UNKNOWN"` DEDUP sentinel).
    pub product: Option<GrowwProduct>,
    // Credit/debit legs — wire field names Assumed pending probe; paise.
    pub credit_qty: Option<i64>,
    pub credit_price_paise: Option<i64>,
    pub debit_qty: Option<i64>,
    pub debit_price_paise: Option<i64>,
    // Carry-forward (T-1) legs, when present.
    pub carry_forward_credit_qty: Option<i64>,
    pub carry_forward_debit_qty: Option<i64>,
    /// Realised P&L, integer paise (Verified present on the wire).
    pub realised_pnl_paise: Option<i64>,
    /// DERIVED net = Σ credit − debit; read with [`Self::derivation`].
    pub net_qty: i64,
    pub derivation: Derivation,
    /// Wire fields received but not mapped (schema-drift signal vs probe).
    pub unparsed_field_count: u16,
}

/// One broker holding row (T4/probe only; in-session polling deferred).
/// ISIN is the identity key (Verified); `available_qty` is NEVER derived
/// pre-probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerHolding {
    /// ISIN — the Verified holding identity key.
    pub isin: String,
    pub trading_symbol: Option<String>,
    /// Total quantity (total-vs-free Unknown pending probe).
    pub quantity: Option<i64>,
    pub average_price_paise: Option<i64>, // field name Verified
    // Splits, when named by the probe.
    pub pledge_qty: Option<i64>,
    pub demat_qty: Option<i64>,
    pub t1_qty: Option<i64>,
    pub unparsed_field_count: u16,
}

/// Account margin/funds — the consumed subset of the 25-field LIVE-DOC table.
/// ACCOUNT-POOLED with the co-tenant program — every figure is ACCOUNT-level
/// truth shared with the co-tenant; `balance_available` is never "our funds".
/// A live consumer must treat margin rejects as expected under co-tenancy.
/// Non-commodity field names Assumed pending probe; commodity_* Verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerMargin {
    pub balance_available_paise: Option<i64>, // POOLED — see the struct stamp
    /// Net margin in use, paise. Nonzero with an empty queried book = the
    /// false-flat contradiction tripwire.
    pub net_margin_used_paise: Option<i64>,
    pub span_margin_used_paise: Option<i64>,
    pub exposure_margin_used_paise: Option<i64>,
    pub commodity_balance_available_paise: Option<i64>,
    /// Nonzero with zero commodity position coverage = the
    /// `unqueried_segment_active` tripwire (GROWW-PORT-04). Paise.
    pub commodity_net_margin_used_paise: Option<i64>,
    pub commodity_span_margin_used_paise: Option<i64>,
    pub commodity_exposure_margin_used_paise: Option<i64>,
    /// Wire fields received but not mapped (of the 25-field table).
    pub unparsed_field_count: u16,
}

impl BrokerMargin {
    /// True when ANY usage figure is positive — the same-cycle false-flat
    /// cross-check input (needs no prior cycle; works on cold boot).
    #[must_use]
    pub fn any_usage_nonzero(&self) -> bool {
        self.net_margin_used_paise.is_some_and(|v| v > 0)
            || self.span_margin_used_paise.is_some_and(|v| v > 0)
            || self.exposure_margin_used_paise.is_some_and(|v| v > 0)
            || self.any_commodity_usage_nonzero()
    }

    /// True when any COMMODITY usage figure is positive — the N1
    /// unqueried-segment tripwire input (zero-request, every cycle).
    #[must_use]
    pub fn any_commodity_usage_nonzero(&self) -> bool {
        self.commodity_net_margin_used_paise.is_some_and(|v| v > 0)
            || self.commodity_span_margin_used_paise.is_some_and(|v| v > 0)
            || self
                .commodity_exposure_margin_used_paise
                .is_some_and(|v| v > 0)
    }
}

/// One published broker-account snapshot (replaced WHOLESALE per cycle).
#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioSnapshot {
    /// Which broker produced it (`Feed::Groww` today).
    pub feed: Feed,
    /// When the cycle's legs finished, epoch SECONDS — the freshness anchor.
    ///
    /// DECLARED DELTA (2026-07-15, hostile review #3 finding 7) vs design
    /// §3.3 "per-leg fetched_at stamps": 6c.1 ships ONE cycle-level stamp;
    /// optional per-leg stamps are an ADDITIVE 6c.3 follow-up (new `Option`
    /// fields — never a breaking reshape of this shared surface). Torn-ness
    /// is asserted via [`SnapshotStatus::Torn`], not evidenced per-leg yet.
    pub fetched_at_epoch_secs: i64,
    /// WALL-CLOCK-ALIGNED tier cycle timestamp, nanos IST — duplicate
    /// pollers' rows DEDUP-collapse on it.
    pub cycle_ts_nanos_ist: i64,
    pub tier: SnapshotTier,
    /// Health verdict (staleness is read-time, not stored — see the enum).
    pub status: SnapshotStatus,
    /// Position rows (resolved; duplicates keep-FIRST, dropped ones counted).
    pub positions: Vec<BrokerPosition>,
    pub holdings: Vec<BrokerHolding>, // T4/probe cycles; empty otherwise
    /// Account margin, when the leg ran and parsed (ACCOUNT-POOLED stamp).
    pub margin: Option<BrokerMargin>,
}

impl PortfolioSnapshot {
    /// The one fail-closed bit, DERIVED from `status` — it can structurally
    /// never disagree (2026-07-15 hostile-review fix: the previously STORED
    /// redundant bool had no enforcing constructor, so a producer could
    /// drift `complete=true` + `Torn`). The §38.8 consumer contract is
    /// unchanged: consumers gate on [`Self::is_fresh`], which reads this.
    #[must_use]
    pub fn complete(&self) -> bool {
        matches!(self.status, SnapshotStatus::Complete)
    }

    /// Fail-closed freshness gate (§38.8): true ONLY when
    /// [`SnapshotStatus::Complete`] and age at `now_epoch_secs` ≤
    /// `staleness_max_secs`. Torn / incomplete / cold-boot-provisional /
    /// future-dated stamp (clock skew) / overflow all return `false`.
    #[must_use]
    pub fn is_fresh(&self, now_epoch_secs: i64, staleness_max_secs: u64) -> bool {
        if !self.complete() {
            return false;
        }
        let Some(age) = now_epoch_secs.checked_sub(self.fetched_at_epoch_secs) else {
            return false;
        };
        (0..=i64::try_from(staleness_max_secs).unwrap_or(i64::MAX)).contains(&age)
    }

    /// [`Self::is_fresh`] against the system clock. A pre-1970 or broken
    /// clock yields `now = 0` ⇒ fail closed (never a panic, no unwrap).
    #[must_use]
    pub fn is_fresh_now(&self, staleness_max_secs: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        self.is_fresh(now, staleness_max_secs)
    }
}

/// Process-global snapshot cell. Starts EMPTY at boot — no stale
/// rehydration; consumers fail closed until the first live cycle.
static PORTFOLIO_SNAPSHOT: OnceLock<ArcSwapOption<PortfolioSnapshot>> = OnceLock::new();

fn portfolio_snapshot_cell() -> &'static ArcSwapOption<PortfolioSnapshot> {
    PORTFOLIO_SNAPSHOT.get_or_init(ArcSwapOption::empty)
}

/// Publish a snapshot WHOLESALE (lock-free swap). Called only by the tier
/// scheduler after a SUCCESSFUL cycle — a failed cycle must NOT call this.
// WIRING-EXEMPT: the 6c.3 poller/persistence PR wires the publish/read pair (the tier scheduler publishes; consumers read) — dormant-by-design until then.
pub fn publish_portfolio_snapshot(snapshot: Arc<PortfolioSnapshot>) {
    portfolio_snapshot_cell().store(Some(snapshot));
}

/// The current snapshot, if any cycle has published this process lifetime.
/// `None` ⇒ FAIL CLOSED at every consumer (§38.8).
#[must_use]
// WIRING-EXEMPT: the 6c.3 poller/persistence PR wires the publish/read pair (the tier scheduler publishes; consumers read) — dormant-by-design until then.
pub fn current_portfolio_snapshot() -> Option<Arc<PortfolioSnapshot>> {
    portfolio_snapshot_cell().load_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_snapshot(fetched_at: i64) -> PortfolioSnapshot {
        PortfolioSnapshot {
            feed: Feed::Groww,
            fetched_at_epoch_secs: fetched_at,
            cycle_ts_nanos_ist: fetched_at.saturating_mul(1_000_000_000),
            tier: SnapshotTier::T1Intraday,
            status: SnapshotStatus::Complete,
            positions: Vec::new(),
            holdings: Vec::new(),
            margin: None,
        }
    }

    #[test]
    fn test_is_fresh_fail_closed_on_stale_torn_incomplete() {
        let now = 1_800_000_000_i64;
        // Fresh + Complete is the ONLY true case; stale / future-dated fail
        // closed; every non-Complete status fails closed regardless of age.
        assert!(complete_snapshot(now - 10).is_fresh(now, 900));
        assert!(!complete_snapshot(now - 901).is_fresh(now, 900));
        assert!(!complete_snapshot(now + 5).is_fresh(now, 900));
        for status in [
            SnapshotStatus::Torn,
            SnapshotStatus::Incomplete,
            SnapshotStatus::OkEmptyColdBoot,
        ] {
            let mut snap = complete_snapshot(now - 10);
            snap.status = status;
            assert!(!snap.is_fresh(now, 900), "{status:?} must fail closed");
            // `complete()` is DERIVED from status — the stored-bit drift
            // class (`complete=true` + `Torn`) is structurally impossible.
            assert!(!snap.complete(), "{status:?} must derive complete=false");
        }
        assert!(complete_snapshot(now - 10).complete());
    }

    #[test]
    fn test_snapshot_starts_none_at_boot() {
        // ONLY this test touches the process-global cell (order-safety).
        assert!(current_portfolio_snapshot().is_none(), "must start None");
        publish_portfolio_snapshot(Arc::new(complete_snapshot(1_800_000_000)));
        assert!(current_portfolio_snapshot().is_some_and(|s| s.complete()));
    }

    #[test]
    fn test_is_fresh_now_fail_closed_on_stale_and_true_when_fresh() {
        // Ancient stamp fails closed against the real clock regardless of a
        // huge staleness budget being *almost* large enough.
        let ancient = complete_snapshot(1_000_000);
        assert!(!ancient.is_fresh_now(900));
        // A snapshot stamped "now" with a generous budget reads fresh (the
        // real-clock happy path); a future-dated stamp fails closed.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        assert!(complete_snapshot(now).is_fresh_now(86_400));
        assert!(!complete_snapshot(now + 3_600).is_fresh_now(86_400));
    }

    #[test]
    fn test_broker_margin_doc_stamp_account_pooled() {
        // Source-scan ratchet: the ACCOUNT-POOLED co-tenancy stamp (N5) must
        // stay on the BrokerMargin doc — never presented as "our funds".
        // Scan the PRODUCTION region ONLY (split at the test-module gate) —
        // a whole-file scan self-matches the assertion literals below
        // (vacuous false-OK; the 2026-07-06 AGGREGATOR-SEAL-01 §(c) class).
        let src = include_str!("portfolio_types.rs");
        let (prod, _) = src.split_once("#[cfg(test)]").unwrap_or((src, ""));
        assert!(prod.contains("ACCOUNT-POOLED with the co-tenant program"));
        assert!(prod.contains("never \"our funds\""));
        let margin = BrokerMargin {
            balance_available_paise: Some(1_00_000),
            net_margin_used_paise: Some(50_000),
            span_margin_used_paise: None,
            exposure_margin_used_paise: None,
            commodity_balance_available_paise: None,
            commodity_net_margin_used_paise: Some(1),
            commodity_span_margin_used_paise: None,
            commodity_exposure_margin_used_paise: None,
            unparsed_field_count: 0,
        };
        assert!(margin.any_usage_nonzero());
        assert!(margin.any_commodity_usage_nonzero());
        assert_eq!(product_dedup_symbol(None), "UNKNOWN");
        assert_eq!(product_dedup_symbol(Some(GrowwProduct::Mis)), "MIS");
    }
}
