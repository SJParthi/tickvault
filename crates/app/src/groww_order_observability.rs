//! Groww order/position PUSH — Stage D app consumer (order_audit bridge).
//!
//! Bridges the trading-side supervised push runner's full-fidelity
//! [`BrokerOrderEvent`]s into the SHARED `order_audit` QuestDB table
//! (`feed='groww'`, `mode='paper'` while [`GROWW_ORDER_LIVE_FIRE`] is
//! `false`) via a bounded mpsc sink + a supervised consumer task.
//!
//! Contract points (operator-authorized paper-mode receive-only build,
//! 2026-07-17; `.claude/rules/project/groww-orders-error-codes.md` §8 +
//! `groww-second-feed-scope-2026-06-19.md` §39):
//!
//! - **Receive-only.** Nothing here places, modifies, or cancels an order —
//!   the module consumes events the broker pushed and writes forensic rows.
//! - **Bounded + non-blocking at the runner.** The [`AuditPushSink`] is a
//!   `try_send` over an mpsc([`PUSH_EVENT_CHANNEL_CAPACITY`]) queue; a
//!   full/closed queue is a LOUD coded drop (`GROWW-PUSH-03`,
//!   `stage = "sink_drop"` — the Stage D contract recorded on
//!   [`GrowwPushEventSink`]: the fan-out's own drop accounting is
//!   count + `debug!`, the loud error lives HERE where the sink's
//!   semantics are owned). The runner's read loop is never stalled.
//! - **Best-effort forensics.** A persist failure is `GROWW-ORD-08`
//!   (`GrowwOrd08AuditWriteFailed` — the runbook §8 assignment) with
//!   `stage = "append"` / `"flush"`; the consumer keeps draining
//!   (AUDIT-WS-01 class — the audit row is never on any order path).
//!   A failed flush DISCARDS the pending buffer (the poisoned-buffer
//!   defense, `discard_pending`).
//! - **Supervised.** The consumer task is respawned on panic/cancel
//!   (`GROWW-PUSH-04`, `component = "audit_consumer"` — the house
//!   WS-GAP-05 pattern; release builds abort on panic, so the respawn
//!   arm self-heals in unwind builds only).
//! - **Token discipline.** The runner's access token is a READ-ONLY SSM
//!   read via [`fetch_groww_access_token`] (never minted, never logged —
//!   `groww-shared-token-minter-2026-07-02.md`).
//!
//! Cold path throughout — a handful of events per session; no tick-path
//! involvement.

use std::sync::Arc;

use tickvault_common::broker_order_events::{BrokerOrderEvent, BrokerOrderStatus};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{GROWW_ORDER_LIVE_FIRE, IST_UTC_OFFSET_NANOS};
use tickvault_common::error_code::ErrorCode;
use tickvault_core::auth::secret_manager::fetch_groww_access_token;
use tickvault_storage::order_audit_persistence::{
    ORDER_AUDIT_SYMBOL_NA, OrderAuditEvent, OrderAuditRow, OrderAuditWriter,
    ensure_order_audit_table,
};
use tickvault_trading::oms::groww::events::{
    GrowwPushEventSink, GrowwPushFanOut, PUSH_EVENT_CHANNEL_CAPACITY, PushSinkDelivery,
};
use tickvault_trading::oms::groww::push::order_events::GrowwPushCapture;
use tickvault_trading::oms::groww::push::runner::{
    GrowwAccessTokenProvider, run_groww_push_supervised,
};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Backoff before the supervised audit consumer respawns (the house 5s).
const CONSUMER_RESPAWN_BACKOFF_SECS: u64 = 5;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

// ---------------------------------------------------------------------------
// The bounded sink (runner side — non-blocking, loud coded drop)
// ---------------------------------------------------------------------------

/// The Stage D order-audit sink: a bounded `try_send` bridge from the push
/// runner's fan-out into the app-side consumer. A refused event is a LOUD
/// coded drop here (the runner-side fan-out only counts + `debug!`s).
pub struct AuditPushSink {
    tx: mpsc::Sender<BrokerOrderEvent>,
}

impl AuditPushSink {
    /// A fresh bounded queue at [`PUSH_EVENT_CHANNEL_CAPACITY`] + the sink
    /// over its sender.
    #[must_use]
    pub fn channel() -> (Self, mpsc::Receiver<BrokerOrderEvent>) {
        let (tx, rx) = mpsc::channel(PUSH_EVENT_CHANNEL_CAPACITY);
        (Self { tx }, rx)
    }
}

impl GrowwPushEventSink for AuditPushSink {
    fn name(&self) -> &'static str {
        "order_audit"
    }

    fn on_push_event(&self, event: &BrokerOrderEvent) -> PushSinkDelivery {
        match self.tx.try_send(event.clone()) {
            Ok(()) => PushSinkDelivery::Delivered,
            Err(err) => {
                let reason = match err {
                    mpsc::error::TrySendError::Full(_) => "full",
                    mpsc::error::TrySendError::Closed(_) => "closed",
                };
                // The Stage D loud coded drop (the events.rs contract: the
                // fan-out's own accounting is debug-only; the sink owner —
                // here — emits the coded error). No dedicated runbook code
                // exists for the app-side queue drop, so per the Stage D
                // contract it rides the push channel's decode/loss code.
                error!(
                    code = ErrorCode::GrowwPush03DecodeFailed.code_str(),
                    stage = "sink_drop",
                    reason,
                    broker_order_id = %event.broker_order_id,
                    "groww order push: audit sink refused an event — order_audit row LOST for this event (bounded queue; the reconcile sweep is the floor)"
                );
                metrics::counter!(
                    "tv_groww_order_push_sink_dropped_total",
                    "reason" => reason
                )
                .increment(1);
                PushSinkDelivery::Dropped
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event → order_audit row mapping (pure)
// ---------------------------------------------------------------------------

/// Canonicalize the broker segment label into the audit row's static
/// SYMBOL vocabulary. Groww pushes `"CASH"`/`"FNO"`/`"COMMODITY"`; anything
/// else degrades to the `"n/a"` sentinel (never a fabricated segment).
#[must_use]
pub fn canonical_push_segment(raw: &str) -> &'static str {
    match raw {
        "CASH" => "CASH",
        "FNO" => "FNO",
        "COMMODITY" => "COMMODITY",
        _ => ORDER_AUDIT_SYMBOL_NA,
    }
}

/// Map a normalized push status onto the audit table's typed lifecycle
/// event vocabulary. The push channel observes lifecycle TRANSITIONS, not
/// OMS call sites, so the mapping is deliberately coarse: cancels →
/// `Cancelled`, rejects/failures → `Rejected`, everything else (new /
/// pending / open / fills / expired / unknown) → `Placed` (the "an order
/// exists at the broker" observation) — the row's `order_status` column
/// carries the precise status string.
#[must_use]
pub fn audit_event_for(status: BrokerOrderStatus) -> OrderAuditEvent {
    match status {
        BrokerOrderStatus::Cancelled => OrderAuditEvent::Cancelled,
        BrokerOrderStatus::Rejected | BrokerOrderStatus::Failed => OrderAuditEvent::Rejected,
        BrokerOrderStatus::New
        | BrokerOrderStatus::Pending
        | BrokerOrderStatus::Open
        | BrokerOrderStatus::PartiallyFilled
        | BrokerOrderStatus::Filled
        | BrokerOrderStatus::Expired
        | BrokerOrderStatus::Unknown => OrderAuditEvent::Placed,
    }
}

/// Integer paise → rupees for the audit row's `price` DOUBLE column
/// (`None` — not-yet-priced — is an honest 0.0, matching the row contract
/// "0.0 for market orders").
#[must_use]
pub fn paise_to_rupees(paise: Option<i64>) -> f64 {
    #[allow(clippy::cast_precision_loss)] // APPROVED: audit DOUBLE column; paise magnitudes ≪ 2^52
    match paise {
        Some(p) => p as f64 / 100.0,
        None => 0.0,
    }
}

/// Build the `order_audit` forensic row for one push event at an explicit
/// IST timestamp (pure — the caller stamps the clock).
#[must_use]
pub fn broker_event_to_audit_row_at(
    event: &BrokerOrderEvent,
    paper: bool,
    ts_ist_nanos: i64,
) -> OrderAuditRow {
    let order_id = if event.broker_order_id.is_empty() {
        ORDER_AUDIT_SYMBOL_NA.to_owned()
    } else {
        event.broker_order_id.clone()
    };
    let outcome = match event.status {
        BrokerOrderStatus::Rejected | BrokerOrderStatus::Failed => "failed",
        _ => "ok",
    };
    let remaining = event.remaining_qty.map_or(-1, |q| q);
    OrderAuditRow {
        ts_ist_nanos,
        trading_date_ist_nanos: ist_midnight_nanos(ts_ist_nanos),
        order_id,
        correlation_id: event.reference_id.clone().unwrap_or_default(),
        leg: "single",
        event: audit_event_for(event.status),
        feed: "groww",
        mode: if paper { "paper" } else { "live" },
        security_id: -1,
        exchange_segment: canonical_push_segment(&event.segment),
        transaction_type: ORDER_AUDIT_SYMBOL_NA,
        quantity: event.filled_qty,
        price: paise_to_rupees(event.avg_fill_price_paise),
        order_status: event.status.as_str(),
        outcome,
        detail: format!(
            "push raw_status={} remaining_qty={remaining}",
            event.raw_status
        ),
    }
}

// ---------------------------------------------------------------------------
// The consumer (app side — persist each event, best-effort, loud)
// ---------------------------------------------------------------------------

/// Append + flush one push event's audit row. Failures are LOUD
/// (`GROWW-ORD-08` per the runbook §8 assignment) and best-effort — the
/// consumer keeps draining; a failed flush discards the pending buffer
/// (poisoned-buffer defense).
fn persist_push_row(writer: &mut OrderAuditWriter, event: &BrokerOrderEvent, paper: bool) {
    let row = broker_event_to_audit_row_at(event, paper, now_ist_nanos());
    if let Err(err) = writer.append_order_audit_row(&row) {
        error!(
            code = ErrorCode::GrowwOrd08AuditWriteFailed.code_str(),
            stage = "append",
            error = %err,
            broker_order_id = %event.broker_order_id,
            "groww order push: order_audit append failed — forensic row lost for this event (order path unaffected)"
        );
        metrics::counter!(
            "tv_groww_order_push_persist_errors_total",
            "stage" => "append"
        )
        .increment(1);
        return;
    }
    if let Err(err) = blocking_flush(|| writer.flush()) {
        let discarded = writer.discard_pending();
        error!(
            code = ErrorCode::GrowwOrd08AuditWriteFailed.code_str(),
            stage = "flush",
            error = %err,
            discarded,
            broker_order_id = %event.broker_order_id,
            "groww order push: order_audit flush refused by the server ACK — pending rows discarded (poisoned-buffer defense; re-observations are DEDUP-idempotent)"
        );
        metrics::counter!(
            "tv_groww_order_push_persist_errors_total",
            "stage" => "flush"
        )
        .increment(1);
        return;
    }
    metrics::counter!("tv_groww_order_push_audit_rows_total").increment(1);
}

/// Drain the sink queue until every sender is dropped (queue closed).
/// Returns cleanly on close — the supervisor treats that as shutdown.
async fn consume_until_closed(
    rx: &mut mpsc::Receiver<BrokerOrderEvent>,
    writer: &mut OrderAuditWriter,
    paper: bool,
) {
    while let Some(event) = rx.recv().await {
        persist_push_row(writer, &event, paper);
    }
}

/// The supervised audit consumer: one `ensure_order_audit_table` at start
/// (idempotent DDL — a failure degrades per the storage module's own
/// contract), then drain-forever with respawn on task death.
///
/// The receiver is shared behind a mutex so a respawned incarnation resumes
/// the SAME queue — no event buffered at death instant is structurally
/// orphaned (it is delivered to the next incarnation).
// TEST-EXEMPT: supervision orchestration over the unit-tested pure mapping + persist helpers; the drain loop itself is exercised by test_consume_until_closed_drains_and_returns.
pub async fn run_groww_order_audit_consumer(
    rx: mpsc::Receiver<BrokerOrderEvent>,
    questdb: QuestDbConfig,
    paper: bool,
) {
    ensure_order_audit_table(&questdb).await;
    let shared_rx = Arc::new(tokio::sync::Mutex::new(rx));
    loop {
        let questdb_for_task = questdb.clone();
        let rx_for_task = Arc::clone(&shared_rx);
        let handle = tokio::spawn(async move {
            let mut writer = OrderAuditWriter::new(&questdb_for_task);
            let mut rx_guard = rx_for_task.lock().await;
            consume_until_closed(&mut rx_guard, &mut writer, paper).await;
        });
        let reason: &'static str = match handle.await {
            Ok(()) => {
                // recv() returned None — every sender dropped: shutdown.
                info!("groww order push: audit consumer queue closed — consumer stopped");
                return;
            }
            Err(join_err) if join_err.is_panic() => "panic",
            Err(_cancelled) => "cancelled",
        };
        error!(
            code = ErrorCode::GrowwPush04SupervisorRespawned.code_str(),
            component = "audit_consumer",
            reason,
            backoff_secs = CONSUMER_RESPAWN_BACKOFF_SECS,
            "groww order push: audit consumer died — respawning after backoff (unwind-build self-heal; release panics abort the process)"
        );
        metrics::counter!(
            "tv_groww_order_push_consumer_respawn_total",
            "reason" => reason
        )
        .increment(1);
        tokio::time::sleep(std::time::Duration::from_secs(
            CONSUMER_RESPAWN_BACKOFF_SECS,
        ))
        .await;
    }
}

// ---------------------------------------------------------------------------
// Boot wiring (spawn the runner + the consumer)
// ---------------------------------------------------------------------------

/// The runner's access-token source: a READ-ONLY SSM read of the shared
/// minter Lambda's token parameter (never minted — token-minter lock
/// 2026-07-02). The error string is secret-free (the secret_manager
/// error display never carries parameter values).
struct SsmGrowwTokenProvider;

impl GrowwAccessTokenProvider for SsmGrowwTokenProvider {
    // The trait's `BoxFuture<'_, _>` return type written structurally
    // (`Pin<Box<dyn Future + Send + '_>>`) so the app crate needs no
    // direct futures-util dependency for one type alias.
    fn access_token(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<secrecy::SecretString, String>> + Send + '_>,
    > {
        Box::pin(async {
            fetch_groww_access_token()
                .await
                .map_err(|err| format!("groww access token SSM read failed: {err}"))
        })
    }
}

/// Spawn the Groww order/position PUSH channel: the supervised runner
/// (trading-side) fanning into the bounded [`AuditPushSink`], plus the
/// supervised audit consumer persisting each event to `order_audit`.
///
/// Paper/live mode follows [`GROWW_ORDER_LIVE_FIRE`] (Gate 3 — hardcoded
/// `false` today, so every row is `mode='paper'`). The caller gates this
/// on the runtime `[groww_orders] order_push_enabled` flag (Gate 1).
///
/// `capture` is the ADDITIVE full-fidelity capture lane
/// (`order_update_events` / `position_update_events` — ORDER-EVT-01):
/// pass [`GrowwPushCapture::disabled()`] when `[order_update_events]` is
/// off; the lossy 11-field audit lane above is untouched either way.
pub fn spawn_groww_order_push(questdb: &QuestDbConfig, capture: GrowwPushCapture) {
    let (sink, rx) = AuditPushSink::channel();
    let fan_out = Arc::new(GrowwPushFanOut::new(vec![Arc::new(sink)]));
    let provider: Arc<dyn GrowwAccessTokenProvider> = Arc::new(SsmGrowwTokenProvider);
    // Process-lifetime stop signal: the sender is deliberately leaked so the
    // watch never flips — the push channel runs until process shutdown
    // (config-disable requires a restart today; a runtime toggle is a
    // flagged follow-up). A dropped sender would read as `stop` on some
    // watch impls, so the leak is the fail-safe-run direction.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    std::mem::forget(stop_tx);
    let paper = !GROWW_ORDER_LIVE_FIRE;
    info!(
        paper,
        "groww order push: spawning supervised receive-only order/position channel (audit sink)"
    );
    tokio::spawn(run_groww_push_supervised(
        provider, fan_out, capture, stop_rx,
    ));
    tokio::spawn(run_groww_order_audit_consumer(rx, questdb.clone(), paper));
}

// ---------------------------------------------------------------------------
// Time helpers (mirror order_observability.rs)
// ---------------------------------------------------------------------------

fn now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS)
}

fn ist_midnight_nanos(ist_nanos: i64) -> i64 {
    ist_nanos.saturating_sub(ist_nanos.rem_euclid(NANOS_PER_DAY))
}

/// Run a synchronous blocking ILP flush off the async worker (the
/// order_observability.rs house pattern): `block_in_place` on a
/// multi-thread runtime; direct call on a current-thread (test) runtime
/// where `block_in_place` panics.
fn blocking_flush<T>(flush: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(flush)
    } else {
        flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::feed::Feed;

    fn sample_event() -> BrokerOrderEvent {
        BrokerOrderEvent {
            broker: Feed::Groww,
            broker_order_id: "GW-123".to_owned(),
            reference_id: Some("TV2607150001ABCD".to_owned()),
            status: BrokerOrderStatus::Filled,
            raw_status: "EXECUTED".to_owned(),
            filled_qty: 50,
            remaining_qty: Some(0),
            avg_fill_price_paise: Some(12_345),
            segment: "FNO".to_owned(),
            exchange_ts_ms: Some(1_700_000_000_000),
            received_at_ms: 1_700_000_000_100,
        }
    }

    #[test]
    fn test_broker_event_to_audit_row_at_maps_core_fields() {
        let ev = sample_event();
        let ts = 1_752_700_000_000_000_000_i64;
        let row = broker_event_to_audit_row_at(&ev, true, ts);
        assert_eq!(row.ts_ist_nanos, ts);
        assert_eq!(row.trading_date_ist_nanos, ist_midnight_nanos(ts));
        assert_eq!(row.order_id, "GW-123");
        assert_eq!(row.correlation_id, "TV2607150001ABCD");
        assert_eq!(row.leg, "single");
        assert_eq!(row.event, OrderAuditEvent::Placed);
        assert_eq!(row.feed, "groww");
        assert_eq!(row.mode, "paper");
        assert_eq!(row.security_id, -1);
        assert_eq!(row.exchange_segment, "FNO");
        assert_eq!(row.transaction_type, ORDER_AUDIT_SYMBOL_NA);
        assert_eq!(row.quantity, 50);
        assert!((row.price - 123.45).abs() < 1e-9);
        assert_eq!(row.order_status, "FILLED");
        assert_eq!(row.outcome, "ok");
        assert!(row.detail.contains("raw_status=EXECUTED"));
        assert!(row.detail.contains("remaining_qty=0"));
    }

    #[test]
    fn test_audit_event_for_covers_every_status() {
        let cases = [
            (BrokerOrderStatus::New, OrderAuditEvent::Placed),
            (BrokerOrderStatus::Pending, OrderAuditEvent::Placed),
            (BrokerOrderStatus::Open, OrderAuditEvent::Placed),
            (BrokerOrderStatus::PartiallyFilled, OrderAuditEvent::Placed),
            (BrokerOrderStatus::Filled, OrderAuditEvent::Placed),
            (BrokerOrderStatus::Cancelled, OrderAuditEvent::Cancelled),
            (BrokerOrderStatus::Rejected, OrderAuditEvent::Rejected),
            (BrokerOrderStatus::Failed, OrderAuditEvent::Rejected),
            (BrokerOrderStatus::Expired, OrderAuditEvent::Placed),
            (BrokerOrderStatus::Unknown, OrderAuditEvent::Placed),
        ];
        for (status, expected) in cases {
            assert_eq!(audit_event_for(status), expected, "status {status:?}");
        }
    }

    #[test]
    fn test_broker_event_to_audit_row_at_rejected_is_failed_outcome() {
        let mut ev = sample_event();
        ev.status = BrokerOrderStatus::Rejected;
        ev.raw_status = "REJECTED".to_owned();
        let row = broker_event_to_audit_row_at(&ev, false, 1);
        assert_eq!(row.event, OrderAuditEvent::Rejected);
        assert_eq!(row.outcome, "failed");
        assert_eq!(row.mode, "live");
        assert_eq!(row.order_status, "REJECTED");
    }

    #[test]
    fn test_broker_event_to_audit_row_at_missing_fields_degrade() {
        let mut ev = sample_event();
        ev.broker_order_id = String::new();
        ev.reference_id = None;
        ev.remaining_qty = None;
        ev.avg_fill_price_paise = None;
        ev.segment = "WEIRD".to_owned();
        let row = broker_event_to_audit_row_at(&ev, true, 1);
        assert_eq!(row.order_id, ORDER_AUDIT_SYMBOL_NA);
        assert_eq!(row.correlation_id, "");
        assert_eq!(row.exchange_segment, ORDER_AUDIT_SYMBOL_NA);
        assert!((row.price - 0.0).abs() < f64::EPSILON);
        assert!(row.detail.contains("remaining_qty=-1"));
    }

    #[test]
    fn test_paise_to_rupees_boundaries() {
        assert!((paise_to_rupees(None) - 0.0).abs() < f64::EPSILON);
        assert!((paise_to_rupees(Some(0)) - 0.0).abs() < f64::EPSILON);
        assert!((paise_to_rupees(Some(1)) - 0.01).abs() < 1e-12);
        assert!((paise_to_rupees(Some(-250)) - -2.5).abs() < 1e-12);
        assert!((paise_to_rupees(Some(100_000_000)) - 1_000_000.0).abs() < 1e-6);
    }

    #[test]
    fn test_canonical_push_segment_vocabulary() {
        assert_eq!(canonical_push_segment("CASH"), "CASH");
        assert_eq!(canonical_push_segment("FNO"), "FNO");
        assert_eq!(canonical_push_segment("COMMODITY"), "COMMODITY");
        assert_eq!(canonical_push_segment("E"), ORDER_AUDIT_SYMBOL_NA);
        assert_eq!(canonical_push_segment(""), ORDER_AUDIT_SYMBOL_NA);
    }

    #[test]
    fn test_ist_midnight_nanos_floors_within_day() {
        let midnight = 20_000_i64 * NANOS_PER_DAY;
        assert_eq!(ist_midnight_nanos(midnight), midnight);
        assert_eq!(ist_midnight_nanos(midnight + 1), midnight);
        assert_eq!(ist_midnight_nanos(midnight + NANOS_PER_DAY - 1), midnight);
    }

    #[tokio::test]
    async fn test_on_push_event_full_queue_is_loud_drop() {
        let (tx, _rx_held) = mpsc::channel(1);
        let sink = AuditPushSink { tx };
        // Fill the 1-slot queue, then the second event must drop.
        assert_eq!(
            sink.on_push_event(&sample_event()),
            PushSinkDelivery::Delivered
        );
        assert_eq!(
            sink.on_push_event(&sample_event()),
            PushSinkDelivery::Dropped
        );
    }

    #[tokio::test]
    async fn test_on_push_event_closed_queue_is_loud_drop() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let sink = AuditPushSink { tx };
        assert_eq!(
            sink.on_push_event(&sample_event()),
            PushSinkDelivery::Dropped
        );
    }

    #[tokio::test]
    async fn test_consume_until_closed_drains_and_returns() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(sample_event()).ok();
        tx.try_send(sample_event()).ok();
        drop(tx);
        let mut writer = OrderAuditWriter::for_test();
        consume_until_closed(&mut rx, &mut writer, true).await;
        // for_test writer: appends buffered; flush is a no-op success, so
        // pending() returns to 0 after each event's flush.
        assert_eq!(writer.pending(), 0);
    }

    #[tokio::test]
    async fn test_persist_push_row_appends_into_writer_buffer() {
        let mut writer = OrderAuditWriter::for_test();
        persist_push_row(&mut writer, &sample_event(), true);
        // for_test flush succeeds and clears pending — the row made it
        // through append + flush without a coded failure.
        assert_eq!(writer.pending(), 0);
    }
}
