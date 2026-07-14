//! Order-side observability — the alert-sink bridge + audit consumer
//! (cluster C, 2026-07-14).
//!
//! Wires the existing-but-dead `OmsAlertSink` / `RiskAlertSink` traits
//! (both `None` since birth — zero call sites of `set_alert_sink` existed)
//! to Telegram, and writes the rebuilt SEBI `order_audit` + `pnl_audit`
//! tables. ONE bounded mpsc(1024) channel feeds ONE consumer task:
//!
//! ```text
//! OMS engine ──OmsAlertBridge──┐
//! Risk engine ─RiskAlertBridge─┤
//! trading_pipeline call sites ─┼─▶ mpsc(1024) ─▶ run_order_side_consumer
//! market-close Notify ─────────┘        │
//!                                       ├─▶ order_audit / pnl_audit rows
//!                                       └─▶ Telegram typed events (paced,
//!                                           session-gated except Critical)
//! ```
//!
//! BEST-EFFORT FORENSICS: a failure anywhere in this module never touches
//! tick capture, candles, the (paper) order path, or feed recovery. Drops
//! are LOUD (coded AUDIT-06 `sink_drop` + counter — the AUDIT-WS-01
//! pattern); the daily OnEod heartbeat + counters-vs-rows reconcile
//! (OMS-GAP-02 on mismatch) make silence detectable (audit Rule 11).
//!
//! HONEST ENVELOPE (2026-07-14 hostile review, C1): this whole subsystem
//! is code-ready and fully wired at BOTH trading-pipeline spawn sites, but
//! it is DORMANT while `feeds.dhan_enabled = false` (today's prod default)
//! — both spawn sites are Dhan-lane-gated, so on a dhan-off boot the
//! consumer, the ensure-DDL, the OnEod heartbeat, and the reconcile never
//! run (and nothing false-pages — nothing runs). The heartbeat/reconcile
//! contract holds whenever the Dhan lane / live trading runs (dhan-ON,
//! in-session, strategy-config-present boots). Additionally the OnEod
//! heartbeat + reconcile fire at most ONCE PER PROCESS (the market-close
//! signal is a one-shot sleep; a multi-day dev process gets no day-2
//! heartbeat and the day tallies are since-process-start — prod's daily
//! 16:30 IST box stop makes once-per-process = once-per-day there).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tracing::{error, info};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{IST_UTC_OFFSET_NANOS, IST_UTC_OFFSET_SECONDS_I64};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::market_hours::is_trading_session_now;
use tickvault_common::sanitize::capture_rest_error_body;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::notification::events::NotificationEvent;
use tickvault_core::notification::service::NotificationService;
use tickvault_storage::order_audit_persistence::{
    OrderAuditEvent, OrderAuditRow, OrderAuditWriter, ensure_order_audit_table,
};
use tickvault_storage::pnl_audit_persistence::{
    PNL_AUDIT_AGGREGATE_SECURITY_ID, PNL_AUDIT_AGGREGATE_SEGMENT, PnlAuditRow, PnlAuditWriter,
    PnlSnapshotKind, ensure_pnl_audit_table,
};
use tickvault_trading::oms::engine::{OmsAlert, OmsAlertSink};
use tickvault_trading::risk::engine::RiskAlertSink;

/// Bounded channel capacity — ~100s of buffer at the 10/sec SEBI order
/// cap; absorbs the 15:30 market-close cancel sweep.
pub const ORDER_SIDE_CHANNEL_CAPACITY: usize = 1024;

/// Repeat-prone High alert kinds page at most once per this many seconds
/// per kind (the `should_page_reject` house precedent); suppressed counts
/// fold into the NEXT page's reason text.
pub const ORDER_ALERT_PAGE_COOLDOWN_SECS: u64 = 300;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;
const ORDER_AUDIT_LEG_SINGLE: &str = "single";
const RECONCILE_QUERY_TIMEOUT_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Messages + wiring
// ---------------------------------------------------------------------------

/// One order-side event, produced by the bridges / call-site captures /
/// market-close arm and consumed by [`run_order_side_consumer`].
#[derive(Debug)]
pub enum OrderSideMsg {
    /// From `OmsAlertBridge` (rejected / circuit transitions / rate-limit).
    Alert(OmsAlert),
    /// From `RiskAlertBridge` — NO `order_audit` row (not an order
    /// lifecycle event); the Critical Telegram + the coded RISK-GAP-01
    /// `error!` in the risk engine are its record.
    RiskHalt {
        /// The trading-crate breach slug (mapped to plain English by
        /// [`plain_risk_reason`] before it reaches the phone).
        reason: &'static str,
    },
    /// `place_order` returned Ok at a trading_pipeline call site.
    Placed {
        /// Paper/live order id (`PAPER-1` today).
        order_id: String,
        /// Caller correlation id (empty when unknown at the call site).
        correlation_id: String,
        /// Instrument id.
        security_id: u64,
        /// Segment slug (`IDX_I` / `NSE_FNO` / …).
        exchange_segment: &'static str,
        /// `BUY` / `SELL`.
        transaction_type: &'static str,
        /// Order quantity.
        quantity: i64,
        /// Order price (0.0 for market orders).
        price: f64,
    },
    /// `place_order` returned Err at a trading_pipeline call site.
    PlaceFailed {
        /// Caller correlation id (empty when unknown).
        correlation_id: String,
        /// Instrument id.
        security_id: u64,
        /// Bounded failure text (sanitized + truncated at append).
        detail: String,
    },
    /// `cancel_order` returned Ok.
    Cancelled {
        /// The cancelled order id.
        order_id: String,
    },
    /// `cancel_order` returned Err.
    CancelFailed {
        /// The order id whose cancel failed.
        order_id: String,
        /// Bounded failure text.
        detail: String,
    },
    /// From the market-close arm — writes the OnEod pnl heartbeat row and
    /// runs the daily reconcile.
    PnlEod {
        /// Session-cumulative realized P&L (rupees).
        realized: f64,
        /// Mark-to-market unrealized P&L (rupees).
        unrealized: f64,
    },
}

impl OrderSideMsg {
    /// Whether this message is expected to produce an `order_audit` row —
    /// the daily-reconcile ledger counts only these (`received` at the
    /// producer, `appended` at the consumer).
    #[must_use]
    pub fn produces_order_audit_row(&self) -> bool {
        !matches!(self, Self::RiskHalt { .. } | Self::PnlEod { .. })
    }
}

/// Everything the consumer task needs, constructed at the two main.rs
/// trading-pipeline spawn sites.
pub struct OrderSideWiring {
    /// Telegram fan-out.
    pub notifier: Arc<NotificationService>,
    /// QuestDB target for the two audit writers.
    pub questdb: QuestDbConfig,
    /// `true` = paper mode (rows stamp `mode = "paper"`).
    pub dry_run: bool,
    /// NSE trading calendar — gates the OnEod heartbeat + reconcile so a
    /// weekend/holiday manual run never writes a bogus trading-day row.
    pub calendar: Arc<TradingCalendar>,
}

/// Process-day ledger: `received` (row-producing messages enqueued),
/// `appended` (rows appended AND flushed), `dropped` (row-producing
/// messages the bounded channel refused). Reconciled at the PnlEod
/// message: `received == appended + dropped` AND `dropped == 0`.
#[derive(Debug, Default)]
pub struct OrderSideDayStats {
    /// Row-producing messages successfully enqueued.
    pub received: AtomicU64,
    /// order_audit rows appended + flushed.
    pub appended: AtomicU64,
    /// Row-producing messages dropped at the channel (Full/Closed).
    pub dropped: AtomicU64,
}

/// Counted, loud, never-blocking send — the ONLY way producers enqueue
/// (trait contract "never blocks": `try_send` only). Full/Closed →
/// coded AUDIT-06 `sink_drop` error + `tv_order_alert_dropped_total`.
pub(crate) fn try_send_order_side(
    tx: &mpsc::Sender<OrderSideMsg>,
    stats: &OrderSideDayStats,
    msg: OrderSideMsg,
) {
    let row_producing = msg.produces_order_audit_row();
    if row_producing {
        stats.received.fetch_add(1, Ordering::Relaxed);
    }
    match tx.try_send(msg) {
        Ok(()) => {}
        Err(err) => {
            let reason = match &err {
                mpsc::error::TrySendError::Full(_) => "full",
                mpsc::error::TrySendError::Closed(_) => "closed",
            };
            if row_producing {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            metrics::counter!("tv_order_alert_dropped_total", "reason" => reason).increment(1);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                stage = "sink_drop",
                reason,
                "AUDIT-06: order-side event dropped at the bounded channel — \
                 the audit row / Telegram page for this event is lost \
                 (best-effort forensics; the order path is unaffected)"
            );
        }
    }
}

/// `OmsAlertSink` impl — bridges OMS alerts into the order-side channel.
pub struct OmsAlertBridge {
    pub(crate) tx: mpsc::Sender<OrderSideMsg>,
    pub(crate) stats: Arc<OrderSideDayStats>,
}

impl OmsAlertSink for OmsAlertBridge {
    fn fire(&self, alert: OmsAlert) {
        try_send_order_side(&self.tx, &self.stats, OrderSideMsg::Alert(alert));
    }
}

/// `RiskAlertSink` impl — bridges risk halts into the order-side channel.
pub struct RiskAlertBridge {
    pub(crate) tx: mpsc::Sender<OrderSideMsg>,
    pub(crate) stats: Arc<OrderSideDayStats>,
}

impl RiskAlertSink for RiskAlertBridge {
    fn fire_risk_halt(&self, reason: &'static str) {
        try_send_order_side(&self.tx, &self.stats, OrderSideMsg::RiskHalt { reason });
    }
}

// ---------------------------------------------------------------------------
// Pure mapping + pacing + reconcile primitives
// ---------------------------------------------------------------------------

/// Pure, exhaustive `OmsAlert` → `NotificationEvent` mapping. The typed
/// Telegram variants have existed since Phase 0 — this is their first
/// production constructor.
///
/// SEC-1 (2026-07-14 security review): the OrderRejected `reason` carries
/// the RAW broker HTTP response body in live mode — it is bounded +
/// secret-redacted through the house `capture_rest_error_body` choke point
/// (≤300 chars, JWT/credential redaction) BEFORE it can reach a Telegram
/// body. The ILP `detail` leg was already bounded at append; this closes
/// the Telegram leg.
#[must_use]
pub fn map_oms_alert(alert: &OmsAlert) -> NotificationEvent {
    match alert {
        OmsAlert::OrderRejected {
            correlation_id,
            reason,
        } => NotificationEvent::OrderRejected {
            correlation_id: correlation_id.clone(),
            reason: capture_rest_error_body(reason),
        },
        OmsAlert::CircuitBreakerOpened {
            consecutive_failures,
        } => NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: *consecutive_failures,
        },
        OmsAlert::CircuitBreakerClosed => NotificationEvent::CircuitBreakerClosed,
        OmsAlert::RateLimitExhausted { limit_type } => NotificationEvent::RateLimitExhausted {
            limit_type: limit_type.clone(),
        },
    }
}

/// Total jargon → plain-English mapping for the RiskHalt Critical page —
/// the trading-crate breach slugs never reach the phone (Telegram
/// commandment 2); trading stays untouched (the mapping lives HERE).
#[must_use]
pub fn plain_risk_reason(reason: &str) -> &'static str {
    match reason {
        "MaxDailyLossExceeded" => "daily loss limit hit",
        "PositionSizeLimitExceeded" => "position size limit hit",
        "ManualHalt" => "manual stop by operator",
        _ => "risk limit hit",
    }
}

/// The repeat-prone High kinds the pacer bounds. NOTE (C2, 2026-07-14
/// hostile review): `CircuitBreakerOpened` is NOT an FSM edge — the OMS
/// engine fires it on EVERY blocked place attempt while the breaker is
/// open (the `check()` err arm), so it is paced here too. Its suppressed
/// count is NOT folded into the next page (the event carries no free-text
/// field to fold into) — the per-attempt `circuit_open` audit rows carry
/// the full truth. `CircuitBreakerClosed` IS a genuine recovery edge
/// (fired only on the open→closed transition) and RiskHalt is latched
/// once/day by the trading crate — both stay unpaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacedAlertKind {
    /// Broker/validation order rejections.
    OrderRejected,
    /// SEBI rate-limit denials.
    RateLimitExhausted,
    /// Circuit-breaker OPEN alerts — fired per blocked attempt while
    /// open, not per transition (see the enum doc above).
    CircuitOpen,
}

/// Pacer decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaceAction {
    /// Page now; `suppressed` = pages swallowed since the last one (fold
    /// the count into this page's reason text).
    Send {
        /// Alerts suppressed within the cooldown since the last page.
        suppressed: u64,
    },
    /// Within the cooldown — swallow (the audit row still lands).
    Suppress,
}

#[derive(Debug, Default)]
struct PaceState {
    last_paged_at: Option<u64>,
    suppressed: u64,
}

/// Pure per-kind pager: at most one Telegram per kind per
/// [`ORDER_ALERT_PAGE_COOLDOWN_SECS`]; suppressed counts fold into the
/// next page. Clock-free (the caller passes `now_secs`).
#[derive(Debug, Default)]
pub struct AlertPacer {
    rejected: PaceState,
    rate_limited: PaceState,
    circuit_open: PaceState,
}

impl AlertPacer {
    fn state_mut(&mut self, kind: PacedAlertKind) -> &mut PaceState {
        match kind {
            PacedAlertKind::OrderRejected => &mut self.rejected,
            PacedAlertKind::RateLimitExhausted => &mut self.rate_limited,
            PacedAlertKind::CircuitOpen => &mut self.circuit_open,
        }
    }

    /// Records one alert of `kind` at `now_secs` and decides Send/Suppress.
    pub fn record(&mut self, kind: PacedAlertKind, now_secs: u64) -> PaceAction {
        let state = self.state_mut(kind);
        let within_cooldown = state
            .last_paged_at
            .is_some_and(|last| now_secs.saturating_sub(last) < ORDER_ALERT_PAGE_COOLDOWN_SECS);
        if within_cooldown {
            state.suppressed = state.suppressed.saturating_add(1);
            PaceAction::Suppress
        } else {
            let suppressed = state.suppressed;
            state.suppressed = 0;
            state.last_paged_at = Some(now_secs);
            PaceAction::Send { suppressed }
        }
    }
}

/// Folds a suppressed-count into the paced event's reason text so the
/// next page says "…and N more since the last page" (nothing silent —
/// the count may span longer than one cooldown when no further page
/// fired, hence "since the last page", never a fixed window claim).
/// `CircuitBreakerOpened` has no free-text field to fold into — its
/// suppressed pages fall through `other => other` (the per-attempt
/// `circuit_open` audit rows carry the truth).
#[must_use]
pub fn fold_suppressed(event: NotificationEvent, suppressed: u64) -> NotificationEvent {
    if suppressed == 0 {
        return event;
    }
    match event {
        NotificationEvent::OrderRejected {
            correlation_id,
            reason,
        } => NotificationEvent::OrderRejected {
            correlation_id,
            reason: format!("{reason} …and {suppressed} more since the last page"),
        },
        NotificationEvent::RateLimitExhausted { limit_type } => {
            NotificationEvent::RateLimitExhausted {
                limit_type: format!("{limit_type} …and {suppressed} more since the last page"),
            }
        }
        other => other,
    }
}

/// Daily reconcile verdict (OMS-GAP-02 on Mismatch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileVerdict {
    /// Ledger balanced, OnEod flushed, DB count readable.
    Reconciled,
    /// Ledger leak, a drop, or a failed OnEod flush — loud, never silent.
    Mismatch,
    /// Ledger balanced but the informational DB count was unreadable
    /// (`db_rows=unknown`) — Rule 11: never a false OK, never a false
    /// mismatch (a mid-day restart legitimately makes DB ≥ process).
    Unverified,
}

/// Pure daily-reconcile classifier. Mismatch iff the process-internal
/// ledger leaks (`received != appended + dropped`), anything was dropped,
/// or the OnEod heartbeat flush failed. The DB count is INFORMATIONAL
/// only — a restart makes DB ≥ process legitimately, so it never drives
/// a mismatch verdict; unreadable DB downgrades Reconciled → Unverified.
#[must_use]
pub fn classify_order_side_reconcile(
    received: u64,
    appended: u64,
    dropped: u64,
    eod_flushed: bool,
    db_count: Option<u64>,
) -> ReconcileVerdict {
    if received != appended.saturating_add(dropped) || dropped > 0 || !eod_flushed {
        ReconcileVerdict::Mismatch
    } else if db_count.is_none() {
        ReconcileVerdict::Unverified
    } else {
        ReconcileVerdict::Reconciled
    }
}

// ---------------------------------------------------------------------------
// The consumer task
// ---------------------------------------------------------------------------

/// Run a SYNCHRONOUS blocking ILP-over-HTTP flush off the async worker
/// (P1, 2026-07-14 hostile review — the `seal_writer_loop::run_cycle`
/// house pattern): the writers' `flush()` is a blocking HTTP call bounded
/// by the conf-pinned `request_timeout=5000`, and on the 2-worker
/// r8g.large runtime it must not pin a tokio worker shared with the WS
/// read loops + tick processor. On a multi-thread runtime the flush runs
/// under `tokio::task::block_in_place` (other tasks migrate); on a
/// current-thread runtime (the `#[tokio::test]` harness) it is called
/// directly, because `block_in_place` panics there.
fn blocking_flush<T>(flush: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(flush)
    } else {
        flush()
    }
}

fn now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS)
}

fn ist_midnight_nanos(ist_nanos: i64) -> i64 {
    ist_nanos.saturating_sub(ist_nanos.rem_euclid(NANOS_PER_DAY))
}

fn today_ist_date_string() -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64))
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// One bounded, informational `/exec` row count for today's `order_audit`
/// rows. Any failure → `None` (verdict Unverified, never a false number).
async fn query_order_audit_db_count(questdb: &QuestDbConfig) -> Option<u64> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(RECONCILE_QUERY_TIMEOUT_SECS))
        .build()
        .ok()?;
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let query = format!(
        "select count(*) from order_audit where trading_date_ist IN '{}'",
        today_ist_date_string()
    );
    let resp = client
        .get(&url)
        .query(&[("query", query.as_str())])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("dataset")?.get(0)?.get(0)?.as_u64()
}

struct AlertRowParts {
    event: OrderAuditEvent,
    correlation_id: String,
    detail: String,
}

fn alert_row_parts(alert: &OmsAlert) -> AlertRowParts {
    match alert {
        OmsAlert::OrderRejected {
            correlation_id,
            reason,
        } => AlertRowParts {
            event: OrderAuditEvent::Rejected,
            correlation_id: correlation_id.clone(),
            detail: reason.clone(),
        },
        OmsAlert::CircuitBreakerOpened {
            consecutive_failures,
        } => AlertRowParts {
            event: OrderAuditEvent::CircuitOpen,
            correlation_id: String::new(),
            detail: format!("consecutive_failures={consecutive_failures}"),
        },
        OmsAlert::CircuitBreakerClosed => AlertRowParts {
            event: OrderAuditEvent::CircuitClosed,
            correlation_id: String::new(),
            detail: "order path recovered".to_string(),
        },
        OmsAlert::RateLimitExhausted { limit_type } => AlertRowParts {
            event: OrderAuditEvent::RateLimited,
            correlation_id: String::new(),
            detail: format!("limit_type={limit_type}"),
        },
    }
}

/// Append + flush one order_audit row; ledger `appended` increments ONLY
/// when both succeed. Failures are coded AUDIT-06 (staged) — the writer's
/// flush already discard-pends + counts.
fn persist_order_row(
    writer: &mut OrderAuditWriter,
    stats: &OrderSideDayStats,
    row: &OrderAuditRow,
) {
    if let Err(err) = writer.append_order_audit_row(row) {
        metrics::counter!("tv_order_audit_persist_errors_total", "stage" => "append").increment(1);
        error!(
            code = ErrorCode::Audit06OrderWriteFailed.code_str(),
            stage = "append",
            event = row.event.as_str(),
            ?err,
            "AUDIT-06: order_audit row append failed (best-effort forensics)"
        );
        return;
    }
    if let Err(err) = blocking_flush(|| writer.flush()) {
        metrics::counter!("tv_order_audit_persist_errors_total", "stage" => "flush").increment(1);
        error!(
            code = ErrorCode::Audit06OrderWriteFailed.code_str(),
            stage = "flush",
            event = row.event.as_str(),
            ?err,
            "AUDIT-06: order_audit flush failed — pending row(s) discarded \
             (poisoned-buffer defense; the daily reconcile will flag the day)"
        );
        return;
    }
    stats.appended.fetch_add(1, Ordering::Relaxed);
}

fn base_row(dry_run: bool) -> OrderAuditRow {
    let ts = now_ist_nanos();
    OrderAuditRow {
        ts_ist_nanos: ts,
        trading_date_ist_nanos: ist_midnight_nanos(ts),
        order_id: String::new(),
        correlation_id: String::new(),
        leg: ORDER_AUDIT_LEG_SINGLE,
        event: OrderAuditEvent::Placed,
        feed: "dhan",
        mode: if dry_run { "paper" } else { "live" },
        security_id: -1,
        exchange_segment: "n/a",
        transaction_type: "n/a",
        quantity: 0,
        price: 0.0,
        order_status: "n/a",
        outcome: "ok",
        detail: String::new(),
    }
}

/// The order-side consumer loop: ensures both audit tables (subsystem-
/// owned lazy ensure — idempotent, never blocks orders), then per message
/// writes the audit row and routes Telegram per the sink behavior table;
/// at `PnlEod` writes the OnEod heartbeat row + runs the daily reconcile.
pub(crate) async fn run_order_side_consumer(
    mut rx: mpsc::Receiver<OrderSideMsg>,
    wiring: OrderSideWiring,
    stats: Arc<OrderSideDayStats>,
) {
    // Subsystem-owned lazy ensure (idempotent; coded failure arms inside).
    ensure_order_audit_table(&wiring.questdb).await;
    ensure_pnl_audit_table(&wiring.questdb).await;

    let mut order_writer = OrderAuditWriter::new(&wiring.questdb);
    let mut pnl_writer = PnlAuditWriter::new(&wiring.questdb);
    let mut pacer = AlertPacer::default();
    // Per-day Telegram tallies for the day-summary line.
    let (mut placed, mut cancelled, mut rejected, mut alerts) = (0u64, 0u64, 0u64, 0u64);

    info!(
        dry_run = wiring.dry_run,
        "order-side observability consumer started (order_audit + pnl_audit + alert bridge)"
    );

    while let Some(msg) = rx.recv().await {
        match msg {
            OrderSideMsg::Alert(alert) => {
                alerts = alerts.saturating_add(1);
                let parts = alert_row_parts(&alert);
                if parts.event == OrderAuditEvent::Rejected {
                    rejected = rejected.saturating_add(1);
                }
                let row = OrderAuditRow {
                    event: parts.event,
                    correlation_id: parts.correlation_id,
                    detail: parts.detail,
                    outcome: match parts.event {
                        OrderAuditEvent::CircuitClosed => "ok",
                        _ => "failed",
                    },
                    ..base_row(wiring.dry_run)
                };
                persist_order_row(&mut order_writer, &stats, &row);

                // Telegram routing per the sink behavior table: session
                // gate for everything except Critical; AlertPacer for the
                // three repeat-prone High kinds.
                if !is_trading_session_now() {
                    info!(
                        event = parts.event.as_str(),
                        "order-side alert outside the trading session — audit row \
                         written, Telegram suppressed (weekend/holiday manual run)"
                    );
                    continue;
                }
                let now_secs = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
                let paced_kind = match alert {
                    OmsAlert::OrderRejected { .. } => Some(PacedAlertKind::OrderRejected),
                    OmsAlert::RateLimitExhausted { .. } => Some(PacedAlertKind::RateLimitExhausted),
                    // C2: the engine fires CircuitBreakerOpened per BLOCKED
                    // ATTEMPT while open (not per transition) — paced.
                    // Closed is a genuine recovery edge — unpaced.
                    OmsAlert::CircuitBreakerOpened { .. } => Some(PacedAlertKind::CircuitOpen),
                    OmsAlert::CircuitBreakerClosed => None,
                };
                let event = map_oms_alert(&alert);
                match paced_kind.map(|k| pacer.record(k, now_secs)) {
                    Some(PaceAction::Suppress) => {
                        info!(
                            topic = event.topic(),
                            "order-side alert paced — within the 300s per-kind \
                             cooldown (audit row written; count folds into the next page)"
                        );
                    }
                    Some(PaceAction::Send { suppressed }) => {
                        wiring.notifier.notify(fold_suppressed(event, suppressed));
                    }
                    None => wiring.notifier.notify(event),
                }
            }
            OrderSideMsg::RiskHalt { reason } => {
                // Critical — NEVER gated, no pacing (trading's !halted
                // latch = once/day), NO order_audit row by design.
                wiring.notifier.notify(NotificationEvent::RiskHalt {
                    reason: plain_risk_reason(reason).to_string(),
                });
            }
            OrderSideMsg::Placed {
                order_id,
                correlation_id,
                security_id,
                exchange_segment,
                transaction_type,
                quantity,
                price,
            } => {
                placed = placed.saturating_add(1);
                let row = OrderAuditRow {
                    order_id,
                    correlation_id,
                    event: OrderAuditEvent::Placed,
                    security_id: i64::try_from(security_id).unwrap_or(-1),
                    exchange_segment,
                    transaction_type,
                    quantity,
                    price,
                    order_status: "pending",
                    outcome: "ok",
                    ..base_row(wiring.dry_run)
                };
                persist_order_row(&mut order_writer, &stats, &row);
            }
            OrderSideMsg::PlaceFailed {
                correlation_id,
                security_id,
                detail,
            } => {
                let row = OrderAuditRow {
                    correlation_id,
                    event: OrderAuditEvent::PlaceFailed,
                    security_id: i64::try_from(security_id).unwrap_or(-1),
                    outcome: "failed",
                    detail,
                    ..base_row(wiring.dry_run)
                };
                persist_order_row(&mut order_writer, &stats, &row);
            }
            OrderSideMsg::Cancelled { order_id } => {
                cancelled = cancelled.saturating_add(1);
                let row = OrderAuditRow {
                    order_id,
                    event: OrderAuditEvent::Cancelled,
                    outcome: "ok",
                    ..base_row(wiring.dry_run)
                };
                persist_order_row(&mut order_writer, &stats, &row);
            }
            OrderSideMsg::CancelFailed { order_id, detail } => {
                let row = OrderAuditRow {
                    order_id,
                    event: OrderAuditEvent::CancelFailed,
                    outcome: "failed",
                    detail,
                    ..base_row(wiring.dry_run)
                };
                persist_order_row(&mut order_writer, &stats, &row);
            }
            OrderSideMsg::PnlEod {
                realized,
                unrealized,
            } => {
                if !wiring.calendar.is_trading_day_today() {
                    info!(
                        "order-side OnEod skipped — not a trading day \
                         (weekend/holiday manual run; no heartbeat row, no reconcile)"
                    );
                    continue;
                }
                // C5 (2026-07-14 hostile review): snapshot the ledger at
                // the TOP of the arm — BEFORE the blocking OnEod flush
                // (~5s window) and the bounded DB query (~10s window). A
                // row-producing message enqueued during those windows
                // (post-close Exit-arm activity increments `received` at
                // the producer) must not inflate `received` past
                // `appended` into a false OMS-GAP-02 Mismatch. FIFO means
                // every pre-PnlEod message is already consumed, so
                // `appended` is settled here; the residual race is the
                // ms-scale PnlEod enqueue→dequeue drain only.
                let received = stats.received.load(Ordering::Relaxed);
                let appended = stats.appended.load(Ordering::Relaxed);
                let dropped = stats.dropped.load(Ordering::Relaxed);
                // The aggregate OnEod heartbeat row: sentinel
                // security_id=0 / segment="ALL" (test-pinned in storage) —
                // ONE row per trading day proving the whole
                // channel→writer→ILP→QuestDB chain end-to-end.
                let ts = now_ist_nanos();
                let eod_row = PnlAuditRow {
                    ts_ist_nanos: ts,
                    trading_date_ist_nanos: ist_midnight_nanos(ts),
                    security_id: PNL_AUDIT_AGGREGATE_SECURITY_ID,
                    exchange_segment: PNL_AUDIT_AGGREGATE_SEGMENT,
                    snapshot_kind: PnlSnapshotKind::OnEod,
                    net_position_qty: 0,
                    avg_entry_price: 0.0,
                    mark_price: 0.0,
                    realized_pnl: realized,
                    unrealized_pnl: unrealized,
                    mode: if wiring.dry_run { "paper" } else { "live" },
                    feed: "dhan",
                };
                let mut eod_flushed = false;
                match pnl_writer.append_pnl_audit_row(&eod_row) {
                    Ok(()) => match blocking_flush(|| pnl_writer.flush()) {
                        Ok(()) => eod_flushed = true,
                        Err(err) => {
                            metrics::counter!("tv_pnl_audit_persist_errors_total", "stage" => "flush")
                                .increment(1);
                            error!(
                                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                                stage = "flush",
                                ?err,
                                "STORAGE-GAP-03: pnl_audit OnEod heartbeat flush failed"
                            );
                        }
                    },
                    Err(err) => {
                        metrics::counter!("tv_pnl_audit_persist_errors_total", "stage" => "append")
                            .increment(1);
                        error!(
                            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                            stage = "append",
                            ?err,
                            "STORAGE-GAP-03: pnl_audit OnEod heartbeat append failed"
                        );
                    }
                }

                // Daily reconcile (ruling 7): process-internal ledger is
                // the verdict (snapshotted at the top of this arm — C5);
                // the DB count is informational only.
                let db_count = query_order_audit_db_count(&wiring.questdb).await;
                let verdict = classify_order_side_reconcile(
                    received,
                    appended,
                    dropped,
                    eod_flushed,
                    db_count,
                );
                let db_rows = db_count.map_or_else(|| "unknown".to_string(), |n| n.to_string());
                info!(
                    placed,
                    cancelled,
                    rejected,
                    alerts,
                    audit_rows = appended,
                    %db_rows,
                    dropped,
                    realized,
                    unrealized,
                    verdict = ?verdict,
                    "order-side day summary"
                );
                if verdict == ReconcileVerdict::Mismatch {
                    error!(
                        code = ErrorCode::OmsGapReconciliation.code_str(),
                        received,
                        appended,
                        dropped,
                        eod_flushed,
                        %db_rows,
                        "OMS-GAP-02: order-side daily reconcile MISMATCH — the \
                         process ledger leaked, an event was dropped, or the OnEod \
                         heartbeat failed (log-sink-only delivery boundary; see \
                         gap-enforcement.md)"
                    );
                }
            }
        }
    }
    info!("order-side observability consumer stopped (channel closed)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every OmsAlert variant maps to its matching typed Telegram event
    /// (exhaustive — a new OmsAlert variant fails this match at compile
    /// time inside `map_oms_alert`).
    #[test]
    fn test_map_oms_alert_exhaustive() {
        let rejected = map_oms_alert(&OmsAlert::OrderRejected {
            correlation_id: "c1".to_string(),
            reason: "DH-906".to_string(),
        });
        assert!(matches!(
            rejected,
            NotificationEvent::OrderRejected { ref correlation_id, ref reason }
                if correlation_id == "c1" && reason == "DH-906"
        ));
        let opened = map_oms_alert(&OmsAlert::CircuitBreakerOpened {
            consecutive_failures: 5,
        });
        assert!(matches!(
            opened,
            NotificationEvent::CircuitBreakerOpened {
                consecutive_failures: 5
            }
        ));
        let closed = map_oms_alert(&OmsAlert::CircuitBreakerClosed);
        assert!(matches!(closed, NotificationEvent::CircuitBreakerClosed));
        let rate = map_oms_alert(&OmsAlert::RateLimitExhausted {
            limit_type: "per_second".to_string(),
        });
        assert!(matches!(
            rate,
            NotificationEvent::RateLimitExhausted { ref limit_type } if limit_type == "per_second"
        ));
    }

    /// The jargon→English mapping is TOTAL — unknown slugs get the
    /// generic phrase, never raw jargon on the phone.
    #[test]
    fn test_plain_risk_reason_total() {
        assert_eq!(
            plain_risk_reason("MaxDailyLossExceeded"),
            "daily loss limit hit"
        );
        assert_eq!(
            plain_risk_reason("PositionSizeLimitExceeded"),
            "position size limit hit"
        );
        assert_eq!(plain_risk_reason("ManualHalt"), "manual stop by operator");
        assert_eq!(plain_risk_reason("SomeFutureBreach"), "risk limit hit");
        assert_eq!(plain_risk_reason(""), "risk limit hit");
    }

    /// Pacer: first alert sends; repeats within the cooldown suppress;
    /// the post-cooldown page folds the suppressed count and re-arms.
    #[test]
    fn test_alert_pacer_suppresses_within_cooldown_and_folds_count() {
        let mut pacer = AlertPacer::default();
        let k = PacedAlertKind::OrderRejected;
        assert_eq!(pacer.record(k, 1_000), PaceAction::Send { suppressed: 0 });
        assert_eq!(pacer.record(k, 1_001), PaceAction::Suppress);
        assert_eq!(pacer.record(k, 1_100), PaceAction::Suppress);
        // Cooldown elapsed — folds the 2 suppressed pages.
        assert_eq!(
            pacer.record(k, 1_000 + ORDER_ALERT_PAGE_COOLDOWN_SECS),
            PaceAction::Send { suppressed: 2 }
        );
        // Re-armed: the counter reset with the fold.
        assert_eq!(
            pacer.record(k, 2_000 + 2 * ORDER_ALERT_PAGE_COOLDOWN_SECS),
            PaceAction::Send { suppressed: 0 }
        );
        // Kinds are independent: the other kinds' first alerts send.
        assert_eq!(
            pacer.record(PacedAlertKind::RateLimitExhausted, 1_001),
            PaceAction::Send { suppressed: 0 }
        );
        // C2: CircuitOpen is paced like the other repeat-prone kinds
        // (the engine fires it per blocked attempt while open).
        assert_eq!(
            pacer.record(PacedAlertKind::CircuitOpen, 1_001),
            PaceAction::Send { suppressed: 0 }
        );
        assert_eq!(
            pacer.record(PacedAlertKind::CircuitOpen, 1_002),
            PaceAction::Suppress
        );
    }

    /// The fold appends the suppressed count to the paced kinds' reason
    /// text and leaves everything else (and zero counts) untouched.
    #[test]
    fn test_fold_suppressed_appends_count_to_reason() {
        let base = NotificationEvent::OrderRejected {
            correlation_id: "c1".to_string(),
            reason: "DH-906".to_string(),
        };
        let folded = fold_suppressed(base, 3);
        assert!(
            folded
                .to_message()
                .contains("and 3 more since the last page")
        );
        let untouched = fold_suppressed(
            NotificationEvent::OrderRejected {
                correlation_id: "c1".to_string(),
                reason: "DH-906".to_string(),
            },
            0,
        );
        assert!(!untouched.to_message().contains("more since the last"));
        let non_paced = fold_suppressed(NotificationEvent::CircuitBreakerClosed, 9);
        assert!(matches!(non_paced, NotificationEvent::CircuitBreakerClosed));
        // CircuitBreakerOpened has no free-text field — the fold falls
        // through unchanged (the audit rows carry the suppressed truth).
        let cb_open = fold_suppressed(
            NotificationEvent::CircuitBreakerOpened {
                consecutive_failures: 5,
            },
            4,
        );
        assert!(matches!(
            cb_open,
            NotificationEvent::CircuitBreakerOpened {
                consecutive_failures: 5
            }
        ));
    }

    /// The verdict matrix (ruling 7): ledger leak / drop / failed OnEod
    /// flush ⇒ Mismatch; balanced + unreadable DB ⇒ Unverified; balanced +
    /// readable DB ⇒ Reconciled. The DB VALUE never drives a mismatch
    /// (restart makes DB ≥ process legitimately).
    #[test]
    fn test_classify_order_side_reconcile_verdict_matrix() {
        use ReconcileVerdict::{Mismatch, Reconciled, Unverified};
        // Balanced, flushed, DB readable (value irrelevant — informational).
        assert_eq!(
            classify_order_side_reconcile(5, 5, 0, true, Some(5)),
            Reconciled
        );
        assert_eq!(
            classify_order_side_reconcile(5, 5, 0, true, Some(999)),
            Reconciled
        );
        // Balanced but DB unreadable → Unverified (never false-OK).
        assert_eq!(
            classify_order_side_reconcile(5, 5, 0, true, None),
            Unverified
        );
        // Ledger leak → Mismatch.
        assert_eq!(
            classify_order_side_reconcile(5, 4, 0, true, Some(4)),
            Mismatch
        );
        // Any drop → Mismatch (even when the ledger sums).
        assert_eq!(
            classify_order_side_reconcile(5, 4, 1, true, Some(4)),
            Mismatch
        );
        // Failed OnEod flush → Mismatch (heartbeat absence is loud).
        assert_eq!(
            classify_order_side_reconcile(5, 5, 0, false, Some(5)),
            Mismatch
        );
        // Zero-activity day still reconciles (0 == 0 + 0).
        assert_eq!(
            classify_order_side_reconcile(0, 0, 0, true, Some(0)),
            Reconciled
        );
    }

    /// Ledger membership: RiskHalt + PnlEod produce NO order_audit row;
    /// everything else does.
    #[test]
    fn test_order_side_msg_row_producing_membership() {
        assert!(!OrderSideMsg::RiskHalt { reason: "x" }.produces_order_audit_row());
        assert!(
            !OrderSideMsg::PnlEod {
                realized: 0.0,
                unrealized: 0.0
            }
            .produces_order_audit_row()
        );
        assert!(
            OrderSideMsg::Cancelled {
                order_id: "PAPER-1".to_string()
            }
            .produces_order_audit_row()
        );
        assert!(OrderSideMsg::Alert(OmsAlert::CircuitBreakerClosed).produces_order_audit_row());
    }

    /// Bridge drop accounting: a FULL channel drops loudly and the ledger
    /// records received == dropped for row-producing messages; a CLOSED
    /// channel behaves the same (the coded error + counter fire in both
    /// arms — asserted via the stats ledger here).
    #[tokio::test]
    async fn test_bridge_full_and_closed_drops_are_counted() {
        // Capacity-1 channel that nobody drains: 1st send lands, 2nd drops.
        let (tx, rx) = mpsc::channel::<OrderSideMsg>(1);
        let stats = Arc::new(OrderSideDayStats::default());
        let bridge = OmsAlertBridge {
            tx: tx.clone(),
            stats: Arc::clone(&stats),
        };
        bridge.fire(OmsAlert::CircuitBreakerClosed);
        bridge.fire(OmsAlert::CircuitBreakerClosed);
        assert_eq!(stats.received.load(Ordering::Relaxed), 2);
        assert_eq!(
            stats.dropped.load(Ordering::Relaxed),
            1,
            "full drop counted"
        );
        // Closed channel: every subsequent fire is a counted drop (and the
        // Alert is still row-producing, so received advances too).
        drop(rx);
        drop(tx);
        bridge.fire(OmsAlert::CircuitBreakerClosed);
        assert_eq!(stats.received.load(Ordering::Relaxed), 3);
        assert_eq!(
            stats.dropped.load(Ordering::Relaxed),
            2,
            "closed drop counted"
        );
        // RiskHalt is NOT ledger-counted (no row expected) even on drop.
        let risk = RiskAlertBridge {
            tx: bridge.tx.clone(),
            stats: Arc::clone(&stats),
        };
        risk.fire_risk_halt("ManualHalt");
        assert_eq!(stats.received.load(Ordering::Relaxed), 3);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 2);
    }

    /// IST midnight derivation is stable across the day and floors to the
    /// same nanos for any instant within one IST day.
    #[test]
    fn test_ist_midnight_nanos_floors_within_day() {
        let midnight = 1_769_990_400_000_000_000_i64; // an IST midnight
        assert_eq!(ist_midnight_nanos(midnight), midnight);
        assert_eq!(ist_midnight_nanos(midnight + 1), midnight);
        assert_eq!(ist_midnight_nanos(midnight + NANOS_PER_DAY - 1), midnight);
        assert_eq!(
            ist_midnight_nanos(midnight + NANOS_PER_DAY),
            midnight + NANOS_PER_DAY
        );
    }

    /// The channel capacity + cooldown constants are pinned (the design's
    /// sizing rationale: ~100s of buffer at the 10/sec SEBI cap; the
    /// should_page_reject 300s house precedent).
    #[test]
    fn test_order_side_constants_pinned() {
        assert_eq!(ORDER_SIDE_CHANNEL_CAPACITY, 1024);
        assert_eq!(ORDER_ALERT_PAGE_COOLDOWN_SECS, 300);
    }

    /// SEC-1: the OrderRejected `reason` (the raw broker body in live
    /// mode) is bounded + redacted through `capture_rest_error_body`
    /// before it can reach a Telegram body — a multi-KB reject page never
    /// ships unbounded.
    #[test]
    fn test_map_oms_alert_bounds_rejected_reason() {
        let huge = format!("dhan API error: 400 {}", "x".repeat(5_000));
        let mapped = map_oms_alert(&OmsAlert::OrderRejected {
            correlation_id: "c1".to_string(),
            reason: huge,
        });
        if let NotificationEvent::OrderRejected { reason, .. } = mapped {
            assert!(
                reason.chars().count() <= 300,
                "reason must be bounded to 300 chars, got {}",
                reason.chars().count()
            );
        } else {
            panic!("mapped to the wrong variant");
        }
    }

    // -----------------------------------------------------------------------
    // Consumer-loop tests (H1/M1, 2026-07-14 hostile review): REAL
    // executions of `run_order_side_consumer` against an UNREACHABLE
    // QuestDB (port 1 — instant connection refusal on every ensure /
    // flush / reconcile leg) + the disabled NotificationService. These
    // exercise every match arm, the ensure error arms, the AUDIT-06
    // append/flush stages, the STORAGE-GAP-03 OnEod arms, the trading-day
    // gate, and the OMS-GAP-02 reconcile emit.
    // -----------------------------------------------------------------------

    use tickvault_common::config::{NseHolidayEntry, TradingConfig};

    fn unreachable_questdb() -> QuestDbConfig {
        // Port 1 is reserved and never listening; guarantees a real,
        // instant transport failure without touching any live service.
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn test_wiring(dry_run: bool, nse_holidays: Vec<NseHolidayEntry>) -> OrderSideWiring {
        let calendar = TradingCalendar::from_config(&TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays,
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        })
        .expect("test calendar builds");
        OrderSideWiring {
            notifier: NotificationService::disabled(),
            questdb: unreachable_questdb(),
            dry_run,
            calendar: Arc::new(calendar),
        }
    }

    /// Every OrderSideMsg variant drains through the real consumer loop:
    /// both ensures' unreachable arms, all match arms, `alert_row_parts`
    /// (4 alert variants), `base_row`, and the AUDIT-06 flush-fail stage
    /// per row-producing message. The ledger proves it: 8 row-producing
    /// messages received, 0 appended (every flush refused), 0 dropped.
    #[tokio::test]
    async fn test_consumer_drains_every_variant_with_unreachable_questdb() {
        let (tx, rx) = mpsc::channel::<OrderSideMsg>(ORDER_SIDE_CHANNEL_CAPACITY);
        let stats = Arc::new(OrderSideDayStats::default());
        let msgs = vec![
            OrderSideMsg::Placed {
                order_id: "PAPER-1".to_string(),
                correlation_id: String::new(),
                security_id: 13,
                exchange_segment: "IDX_I",
                transaction_type: "BUY",
                quantity: 1,
                price: 0.0,
            },
            OrderSideMsg::PlaceFailed {
                correlation_id: String::new(),
                security_id: 13,
                detail: "long entry: DH-906".to_string(),
            },
            OrderSideMsg::Cancelled {
                order_id: "PAPER-1".to_string(),
            },
            OrderSideMsg::CancelFailed {
                order_id: "PAPER-2".to_string(),
                detail: "market-close cancel: DH-906".to_string(),
            },
            OrderSideMsg::Alert(OmsAlert::OrderRejected {
                correlation_id: "c1".to_string(),
                reason: "DH-906".to_string(),
            }),
            OrderSideMsg::Alert(OmsAlert::RateLimitExhausted {
                limit_type: "per_second".to_string(),
            }),
            OrderSideMsg::Alert(OmsAlert::CircuitBreakerOpened {
                consecutive_failures: 5,
            }),
            OrderSideMsg::Alert(OmsAlert::CircuitBreakerClosed),
        ];
        for msg in msgs {
            try_send_order_side(&tx, &stats, msg);
        }
        drop(tx);
        run_order_side_consumer(rx, test_wiring(true, vec![]), Arc::clone(&stats)).await;
        assert_eq!(stats.received.load(Ordering::Relaxed), 8);
        assert_eq!(
            stats.appended.load(Ordering::Relaxed),
            0,
            "unreachable QuestDB — every flush fails, appended never advances"
        );
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    }

    /// The trading-day gate deterministically SKIPS the OnEod heartbeat +
    /// reconcile on a non-trading day: TODAY (IST) is injected as an NSE
    /// holiday when it is a weekday (the calendar validator rejects
    /// weekend holiday entries — on a weekend the calendar is already
    /// non-trading with no injection).
    #[tokio::test]
    async fn test_consumer_pnl_eod_skipped_on_non_trading_day() {
        use chrono::Datelike;
        let today = (chrono::Utc::now() + chrono::TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64))
            .date_naive();
        let holidays = if matches!(today.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
            vec![]
        } else {
            vec![NseHolidayEntry {
                date: today.format("%Y-%m-%d").to_string(),
                name: "Injected test holiday".to_string(),
            }]
        };
        let wiring = test_wiring(true, holidays);
        assert!(
            !wiring.calendar.is_trading_day_today(),
            "calendar must classify today as non-trading"
        );
        let (tx, rx) = mpsc::channel::<OrderSideMsg>(4);
        let stats = Arc::new(OrderSideDayStats::default());
        try_send_order_side(
            &tx,
            &stats,
            OrderSideMsg::PnlEod {
                realized: 0.0,
                unrealized: 0.0,
            },
        );
        drop(tx);
        run_order_side_consumer(rx, wiring, Arc::clone(&stats)).await;
        // PnlEod is not row-producing; the skip branch leaves the ledger
        // untouched and the consumer exits cleanly.
        assert_eq!(stats.received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.appended.load(Ordering::Relaxed), 0);
    }

    /// On a trading day the PnlEod arm runs the full path against the
    /// unreachable QuestDB: OnEod append succeeds locally, the flush
    /// fails (STORAGE-GAP-03), the DB count is None, and the pre-seeded
    /// drop makes the verdict a deterministic Mismatch — executing the
    /// OMS-GAP-02 `error!` emit. Weekend caveat: `is_trading_day_today`
    /// is wall-clock — on a weekend/holiday run the skip branch fires
    /// instead (deterministically covered by the test above), so the
    /// trading-branch expectations are gated on the calendar.
    #[tokio::test]
    async fn test_consumer_pnl_eod_reconcile_with_unreachable_questdb() {
        let wiring = test_wiring(true, vec![]);
        let trading_today = wiring.calendar.is_trading_day_today();
        let (tx, rx) = mpsc::channel::<OrderSideMsg>(4);
        let stats = Arc::new(OrderSideDayStats::default());
        // Pre-seed a counted drop: dropped > 0 forces Mismatch when the
        // trading-day branch runs.
        stats.received.fetch_add(1, Ordering::Relaxed);
        stats.dropped.fetch_add(1, Ordering::Relaxed);
        try_send_order_side(
            &tx,
            &stats,
            OrderSideMsg::PnlEod {
                realized: -10.0,
                unrealized: 2.5,
            },
        );
        drop(tx);
        run_order_side_consumer(rx, wiring, Arc::clone(&stats)).await;
        // No row-producing message went through the channel; the ledger
        // holds only the pre-seeded values and the consumer exits cleanly
        // on both branches.
        assert_eq!(stats.appended.load(Ordering::Relaxed), 0);
        if trading_today {
            assert_eq!(stats.received.load(Ordering::Relaxed), 1);
            assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
        }
    }

    /// Live mode (`dry_run: false`) exercises the two `mode = "live"`
    /// ternaries (base_row + the OnEod row) through the real consumer.
    #[tokio::test]
    async fn test_consumer_live_mode_placed_row() {
        let (tx, rx) = mpsc::channel::<OrderSideMsg>(4);
        let stats = Arc::new(OrderSideDayStats::default());
        try_send_order_side(
            &tx,
            &stats,
            OrderSideMsg::Placed {
                order_id: "112111182198".to_string(),
                correlation_id: "corr-live-1".to_string(),
                security_id: 13,
                exchange_segment: "IDX_I",
                transaction_type: "BUY",
                quantity: 1,
                price: 100.0,
            },
        );
        drop(tx);
        run_order_side_consumer(rx, test_wiring(false, vec![]), Arc::clone(&stats)).await;
        assert_eq!(stats.received.load(Ordering::Relaxed), 1);
        assert_eq!(
            stats.appended.load(Ordering::Relaxed),
            0,
            "unreachable QuestDB — the live-mode row still fails at flush"
        );
    }

    /// RiskHalt routes to the (disabled) notifier WITHOUT an order_audit
    /// row and WITHOUT touching the reconcile ledger.
    #[tokio::test]
    async fn test_consumer_risk_halt_notifies_without_row() {
        let (tx, rx) = mpsc::channel::<OrderSideMsg>(4);
        let stats = Arc::new(OrderSideDayStats::default());
        try_send_order_side(
            &tx,
            &stats,
            OrderSideMsg::RiskHalt {
                reason: "MaxDailyLossExceeded",
            },
        );
        drop(tx);
        run_order_side_consumer(rx, test_wiring(true, vec![]), Arc::clone(&stats)).await;
        assert_eq!(
            stats.received.load(Ordering::Relaxed),
            0,
            "RiskHalt is not row-producing"
        );
        assert_eq!(stats.appended.load(Ordering::Relaxed), 0);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    }
}
