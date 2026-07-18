//! 🔷 DHAN order-update PAPER-MODE push consumer (operator directive
//! 2026-07-16; governance authorization on PR #1597 —
//! `.claude/plans/active-plan-dhan-order-update-rewire.md`).
//!
//! RECEIVE-ONLY observability: subscribes the stack-local order-update
//! broadcast (fed by the dormant-module WS spawn in `dhan_rest_stack.rs`
//! Phase 5a, gated on `[dhan_order_push] enabled`) and records each
//! incoming [`OrderUpdate`] as one `order_audit` forensics row
//! `feed='dhan'` / `mode='paper'` via the EXISTING `OrderAuditWriter` —
//! no storage schema change, no new `OrderAuditEvent` variants (best-fit
//! mapping: Cancelled → `Cancelled`, Rejected → `Rejected`, everything
//! else → `Placed`, with the RAW status preserved in the row's
//! `order_status` slug + the bounded `tv_dhan_order_updates_total{status}`
//! counter). NO Telegram on any path (the 2026-07-14 Dhan noise lock —
//! the WS spawn passes `notifier: None` and this consumer holds no
//! notification service); persist failures reuse the coded AUDIT-06
//! stage pattern mirrored from `order_observability.rs` — NO new
//! ErrorCode. No live orders; `dry_run` is untouched.

use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use tickvault_common::broker_order_events::{
    OrderUpdateEventRecord, build_dhan_order_event_record,
};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::order_types::OrderUpdate;
use tickvault_storage::order_audit_persistence::{
    OrderAuditEvent, OrderAuditRow, OrderAuditWriter, ensure_order_audit_table,
};

/// Broadcast capacity for the paper-mode order-update push channel —
/// ~100s of buffer at the 10/sec SEBI order cap (the
/// `ORDER_SIDE_CHANNEL_CAPACITY` sizing rationale).
pub const DHAN_ORDER_PUSH_CHANNEL_CAPACITY: usize = 1024;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;
const ORDER_AUDIT_LEG_SINGLE: &str = "single";
/// Supervisor respawn backoff base (doubles per consecutive abnormal
/// exit, capped) — the order_runtime E8 house ladder.
const DHAN_ORDER_PUSH_RESPAWN_BACKOFF_SECS: u64 = 5;
const DHAN_ORDER_PUSH_RESPAWN_BACKOFF_CAP_SECS: u64 = 300;
/// A run surviving this long resets the backoff ladder (the WS-GAP-10
/// stability-window precedent).
const DHAN_ORDER_PUSH_RESPAWN_STABILITY_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Pure mapping helpers (unit-tested below)
// ---------------------------------------------------------------------------

/// Bounded, STATIC counter-label slug for a raw Dhan order-status string.
/// The known WS vocabulary (annexure rule 6 + the order-update WS subset)
/// maps to fixed labels; anything else is `other` — label cardinality can
/// never grow with hostile/drifted wire strings.
fn order_status_label(status: &str) -> &'static str {
    if status.eq_ignore_ascii_case("TRANSIT") {
        "transit"
    } else if status.eq_ignore_ascii_case("PENDING") {
        "pending"
    } else if status.eq_ignore_ascii_case("TRADED") {
        "traded"
    } else if status.eq_ignore_ascii_case("PART_TRADED")
        || status.eq_ignore_ascii_case("PARTIALLY_FILLED")
    {
        "part_traded"
    } else if status.eq_ignore_ascii_case("REJECTED") {
        "rejected"
    } else if status.eq_ignore_ascii_case("CANCELLED") {
        "cancelled"
    } else if status.eq_ignore_ascii_case("EXPIRED") {
        "expired"
    } else if status.eq_ignore_ascii_case("TRIGGERED") {
        "triggered"
    } else if status.eq_ignore_ascii_case("CLOSED") {
        "closed"
    } else {
        "other"
    }
}

/// Best-fit onto the EXISTING `OrderAuditEvent` vocabulary (a DEDUP-keyed
/// stable wire-string set — never extended by this consumer): Cancelled →
/// `Cancelled`, Rejected → `Rejected`, everything else (incl. unknown
/// strings) → `Placed` (the lifecycle-observation default).
fn map_order_event(status: &str) -> OrderAuditEvent {
    if status.eq_ignore_ascii_case("CANCELLED") {
        OrderAuditEvent::Cancelled
    } else if status.eq_ignore_ascii_case("REJECTED") {
        OrderAuditEvent::Rejected
    } else {
        OrderAuditEvent::Placed
    }
}

/// `B`/`S` (the order-update WS single-char codes) → the audit slugs.
fn transaction_type_slug(txn_type: &str) -> &'static str {
    if txn_type.eq_ignore_ascii_case("B") {
        "BUY"
    } else if txn_type.eq_ignore_ascii_case("S") {
        "SELL"
    } else {
        "n/a"
    }
}

/// `(exchange, segment-code)` → the audit segment slug. The order-update
/// WS carries single-char segment codes (`E` = Equity, `D` = Derivatives);
/// anything outside the NSE/BSE equity+derivative grid is `n/a` (honest —
/// never guessed).
fn segment_slug(exchange: &str, segment: &str) -> &'static str {
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
        _ => "n/a",
    }
}

/// Numeric security id or the `-1` sentinel (the `OrderAuditRow` house
/// convention for not-parseable / not-applicable).
fn parse_security_id(security_id: &str) -> i64 {
    security_id.trim().parse::<i64>().unwrap_or(-1)
}

fn ist_midnight_nanos(ist_nanos: i64) -> i64 {
    ist_nanos.saturating_sub(ist_nanos.rem_euclid(NANOS_PER_DAY))
}

fn now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS)
}

/// One paper-mode `order_audit` row for a received order update. Pure —
/// the timestamp is injected so tests are deterministic. `detail` carries
/// the (source, reason) context; the writer sanitizes + truncates it at
/// append time.
fn order_update_to_audit_row(update: &OrderUpdate, ts_ist_nanos: i64) -> OrderAuditRow {
    OrderAuditRow {
        ts_ist_nanos,
        trading_date_ist_nanos: ist_midnight_nanos(ts_ist_nanos),
        order_id: update.order_no.clone(),
        correlation_id: update.correlation_id.clone(),
        leg: ORDER_AUDIT_LEG_SINGLE,
        event: map_order_event(&update.status),
        feed: "dhan",
        mode: "paper",
        security_id: parse_security_id(&update.security_id),
        exchange_segment: segment_slug(&update.exchange, &update.segment),
        transaction_type: transaction_type_slug(&update.txn_type),
        quantity: update.quantity,
        price: update.price,
        order_status: order_status_label(&update.status),
        outcome: "ok",
        detail: format!("src={} reason={}", update.source, update.reason_description),
    }
}

// ---------------------------------------------------------------------------
// Persist leg (the AUDIT-06 stage pattern, mirrored from
// order_observability.rs — best-effort forensics, never gates anything)
// ---------------------------------------------------------------------------

/// Run a SYNCHRONOUS blocking ILP-over-HTTP flush off the async worker
/// (the `order_observability::blocking_flush` house pattern): under
/// `block_in_place` on a multi-thread runtime, direct on a
/// current-thread runtime (the `#[tokio::test]` harness — `block_in_place`
/// panics there).
fn blocking_flush<T>(flush: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(flush)
    } else {
        flush()
    }
}

/// Append + flush one paper order_audit row. Failures are coded AUDIT-06
/// (staged) — the writer's flush already discard-pends + counts.
fn persist_push_row(writer: &mut OrderAuditWriter, row: &OrderAuditRow) {
    if let Err(err) = writer.append_order_audit_row(row) {
        metrics::counter!("tv_order_audit_persist_errors_total", "stage" => "append").increment(1);
        error!(
            code = ErrorCode::Audit06OrderWriteFailed.code_str(),
            stage = "append",
            event = row.event.as_str(),
            ?err,
            "AUDIT-06: dhan order-push order_audit row append failed (best-effort forensics)"
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
            "AUDIT-06: dhan order-push order_audit flush failed — pending row(s) discarded \
             (poisoned-buffer defense)"
        );
    }
}

// ---------------------------------------------------------------------------
// Full-fidelity capture publish (ORDER-EVT-01 sink_drop semantics — the
// Dhan producer half of the order_update_events capture lane, 2026-07-18)
// ---------------------------------------------------------------------------

/// Best-effort `try_send` of one Dhan full-fidelity capture record into the
/// `order_update_events` consumer channel. `None` sender (capture disabled)
/// is a silent no-op; a FULL or CLOSED channel drops the ROW loudly
/// (counted + coded `ORDER-EVT-01 stage="sink_drop"`) and NEVER blocks or
/// fails the paper-audit path — the capture is additive forensics.
fn publish_dhan_event_record(
    events_tx: Option<&mpsc::Sender<OrderUpdateEventRecord>>,
    record: OrderUpdateEventRecord,
) {
    let Some(tx) = events_tx else {
        return;
    };
    if let Err(e) = tx.try_send(record) {
        let reason = match e {
            mpsc::error::TrySendError::Full(_) => "full",
            mpsc::error::TrySendError::Closed(_) => "closed",
        };
        metrics::counter!("tv_order_update_events_dropped_total", "reason" => reason).increment(1);
        error!(
            code = ErrorCode::OrderEvt01PersistFailed.code_str(),
            stage = "sink_drop",
            feed = "dhan",
            channel = "order",
            reason,
            "ORDER-EVT-01: dhan order push-event capture record dropped — the \
             capture consumer channel is {reason}; the forensic row for this \
             event is lost (best-effort capture; the paper audit path and the \
             feed are unaffected)"
        );
    }
}

// ---------------------------------------------------------------------------
// The consumer loop + supervisor
// ---------------------------------------------------------------------------

/// Inner consumer: ensures the `order_audit` table once (idempotent,
/// subsystem-owned lazy ensure), then drains the broadcast — one paper
/// audit row + one bounded-label counter increment per received update,
/// plus one best-effort full-fidelity capture record (`order_update_events`)
/// when the capture channel is wired.
async fn run_dhan_order_push_consumer(
    questdb: QuestDbConfig,
    mut rx: broadcast::Receiver<OrderUpdate>,
    events_tx: Option<mpsc::Sender<OrderUpdateEventRecord>>,
) {
    ensure_order_audit_table(&questdb).await;
    let mut writer = OrderAuditWriter::new(&questdb);
    info!("dhan order push: paper-mode consumer draining (receive-only; no Telegram)");
    loop {
        match rx.recv().await {
            Ok(update) => {
                let status = order_status_label(&update.status);
                metrics::counter!("tv_dhan_order_updates_total", "status" => status).increment(1);
                let ts_ist_nanos = now_ist_nanos();
                // Full-fidelity capture lane (additive, best-effort) —
                // BEFORE the blocking audit flush so a slow QuestDB leg
                // never delays the receipt-stamped capture publish.
                publish_dhan_event_record(
                    events_tx.as_ref(),
                    build_dhan_order_event_record(&update, ts_ist_nanos),
                );
                let row = order_update_to_audit_row(&update, ts_ist_nanos);
                persist_push_row(&mut writer, &row);
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                metrics::counter!("tv_dhan_order_push_lagged_total").increment(skipped);
                warn!(
                    skipped,
                    "dhan order push: consumer lagged the broadcast — {skipped} update(s) \
                     skipped from the paper audit record (counted, feed unaffected)"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Structurally impossible while the supervisor holds a
                // sender clone — a clean return is classified abnormal
                // there (the order_runtime supervisor precedent).
                return;
            }
        }
    }
}

/// Spawn the SUPERVISED paper-mode order-push consumer. Returns the
/// supervisor handle. The FIRST receiver is created at channel
/// construction and handed in (consumer-before-producer — no update can
/// slip past an unsubscribed channel at boot); the supervisor holds a
/// `Sender` clone, so the broadcast can never close underneath the inner
/// task, and each RESPAWN subscribes fresh (updates during a dead window
/// are lost — counted via the respawn counter, honest). Panic honesty
/// (the TICK-FLUSH-01 envelope): release builds abort on panic
/// (`panic = "abort"`), so the panic-respawn arm self-heals in unwind
/// (dev/test) builds only.
// TEST-EXEMPT: orchestration spawn (supervisor loop around live channels); the pure mapping helpers below are unit-tested, wiring pinned by ws_event_audit_boot_guard.rs
pub fn spawn_dhan_order_push_consumer(
    questdb: QuestDbConfig,
    sender: broadcast::Sender<OrderUpdate>,
    first_rx: broadcast::Receiver<OrderUpdate>,
    events_tx: Option<mpsc::Sender<OrderUpdateEventRecord>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut pending_first_rx = Some(first_rx);
        let mut consecutive_abnormal_exits: u32 = 0;
        loop {
            let rx = match pending_first_rx.take() {
                Some(rx) => rx,
                None => sender.subscribe(),
            };
            let qdb = questdb.clone();
            let capture_tx = events_tx.clone();
            let run_started = std::time::Instant::now();
            let inner =
                tokio::spawn(
                    async move { run_dhan_order_push_consumer(qdb, rx, capture_tx).await },
                );
            let reason = match inner.await {
                // The inner loop is infinite while the supervisor holds a
                // sender clone — a clean return is abnormal.
                Ok(()) => "clean_exit",
                Err(err) if err.is_panic() => "panic",
                // Cancelled = runtime shutdown — exit quietly, never
                // respawn onto a dying runtime.
                Err(_) => {
                    info!("dhan order push: consumer supervisor cancelled — shutting down");
                    return;
                }
            };
            metrics::counter!("tv_dhan_order_push_respawn_total", "reason" => reason).increment(1);
            if run_started.elapsed()
                >= std::time::Duration::from_secs(DHAN_ORDER_PUSH_RESPAWN_STABILITY_SECS)
            {
                consecutive_abnormal_exits = 0;
            }
            consecutive_abnormal_exits = consecutive_abnormal_exits.saturating_add(1);
            let backoff_secs = DHAN_ORDER_PUSH_RESPAWN_BACKOFF_SECS
                .saturating_mul(1_u64 << consecutive_abnormal_exits.saturating_sub(1).min(8))
                .min(DHAN_ORDER_PUSH_RESPAWN_BACKOFF_CAP_SECS);
            error!(
                reason,
                backoff_secs,
                consecutive_abnormal_exits,
                "dhan order push: paper-mode consumer task died — respawning (audit rows \
                 during the dead window are lost; best-effort forensics, the feed and \
                 trading are unaffected)"
            );
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire-shaped OrderUpdate via serde defaults (the order_runtime
    /// `wire_update` house helper — OrderUpdate has no `Default` impl).
    fn update_with(status: &str) -> OrderUpdate {
        let mut update: OrderUpdate =
            serde_json::from_str("{}").expect("OrderUpdate fields are serde-default"); // APPROVED: test
        update.exchange = "NSE".to_string();
        update.segment = "D".to_string();
        update.security_id = "49081".to_string();
        update.order_no = "112111182045".to_string();
        update.txn_type = "B".to_string();
        update.quantity = 50;
        update.price = 21_004.95;
        update.status = status.to_string();
        update.correlation_id = "tv-test-1".to_string();
        update.source = "P".to_string();
        update.reason_description = "CONFIRMED".to_string();
        update
    }

    #[test]
    fn test_order_status_label_known_statuses_and_unknown_to_other() {
        assert_eq!(order_status_label("TRANSIT"), "transit");
        assert_eq!(order_status_label("PENDING"), "pending");
        assert_eq!(order_status_label("TRADED"), "traded");
        assert_eq!(order_status_label("PART_TRADED"), "part_traded");
        assert_eq!(order_status_label("PARTIALLY_FILLED"), "part_traded");
        assert_eq!(order_status_label("REJECTED"), "rejected");
        assert_eq!(order_status_label("CANCELLED"), "cancelled");
        assert_eq!(order_status_label("EXPIRED"), "expired");
        assert_eq!(order_status_label("TRIGGERED"), "triggered");
        assert_eq!(order_status_label("CLOSED"), "closed");
        // Case-insensitive.
        assert_eq!(order_status_label("traded"), "traded");
        // Unknown / drifted / hostile wire strings collapse to the bounded
        // "other" label — cardinality can never grow.
        assert_eq!(order_status_label("SOMETHING_NEW"), "other");
        assert_eq!(order_status_label(""), "other");
        assert_eq!(order_status_label("💣"), "other");
    }

    #[test]
    fn test_map_order_event_best_fit_onto_existing_variants() {
        assert_eq!(map_order_event("CANCELLED"), OrderAuditEvent::Cancelled);
        assert_eq!(map_order_event("cancelled"), OrderAuditEvent::Cancelled);
        assert_eq!(map_order_event("REJECTED"), OrderAuditEvent::Rejected);
        // Everything else — incl. unknown strings — best-fits to Placed
        // (lifecycle observation; NO new OrderAuditEvent variants).
        assert_eq!(map_order_event("TRADED"), OrderAuditEvent::Placed);
        assert_eq!(map_order_event("PENDING"), OrderAuditEvent::Placed);
        assert_eq!(map_order_event("SOMETHING_NEW"), OrderAuditEvent::Placed);
        assert_eq!(map_order_event(""), OrderAuditEvent::Placed);
    }

    #[test]
    fn test_transaction_type_and_segment_slugs() {
        assert_eq!(transaction_type_slug("B"), "BUY");
        assert_eq!(transaction_type_slug("S"), "SELL");
        assert_eq!(transaction_type_slug("b"), "BUY");
        assert_eq!(transaction_type_slug(""), "n/a");
        assert_eq!(transaction_type_slug("X"), "n/a");
        assert_eq!(segment_slug("NSE", "E"), "NSE_EQ");
        assert_eq!(segment_slug("NSE", "D"), "NSE_FNO");
        assert_eq!(segment_slug("BSE", "E"), "BSE_EQ");
        assert_eq!(segment_slug("BSE", "D"), "BSE_FNO");
        assert_eq!(segment_slug("MCX", "M"), "n/a");
        assert_eq!(segment_slug("", ""), "n/a");
    }

    #[test]
    fn test_parse_security_id_numeric_and_sentinel() {
        assert_eq!(parse_security_id("49081"), 49_081);
        assert_eq!(parse_security_id(" 13 "), 13);
        assert_eq!(parse_security_id(""), -1);
        assert_eq!(parse_security_id("not-a-number"), -1);
    }

    #[test]
    fn test_order_update_to_audit_row_paper_dhan_shape() {
        // A fixed IST instant: 2026-07-16 10:00:00 IST-as-nanos.
        let ts = 1_784_100_000_000_000_000_i64;
        let row = order_update_to_audit_row(&update_with("TRADED"), ts);
        assert_eq!(row.ts_ist_nanos, ts);
        assert_eq!(row.trading_date_ist_nanos, ist_midnight_nanos(ts));
        assert_eq!(row.order_id, "112111182045");
        assert_eq!(row.correlation_id, "tv-test-1");
        assert_eq!(row.leg, ORDER_AUDIT_LEG_SINGLE);
        assert_eq!(row.event, OrderAuditEvent::Placed);
        assert_eq!(row.feed, "dhan");
        assert_eq!(row.mode, "paper", "receive-only channel is paper mode");
        assert_eq!(row.security_id, 49_081);
        assert_eq!(row.exchange_segment, "NSE_FNO");
        assert_eq!(row.transaction_type, "BUY");
        assert_eq!(row.quantity, 50);
        assert!((row.price - 21_004.95).abs() < f64::EPSILON);
        assert_eq!(row.order_status, "traded");
        assert_eq!(row.outcome, "ok");
        assert!(row.detail.contains("src=P"));
    }

    #[test]
    fn test_order_update_to_audit_row_unparseable_sid_is_sentinel() {
        let mut update = update_with("CANCELLED");
        update.security_id = "garbage".to_string();
        let row = order_update_to_audit_row(&update, 1);
        assert_eq!(row.security_id, -1);
        assert_eq!(row.event, OrderAuditEvent::Cancelled);
        assert_eq!(row.order_status, "cancelled");
    }

    #[tokio::test]
    async fn test_publish_dhan_event_record_delivers_into_the_capture_channel() {
        let (tx, mut rx) = mpsc::channel(4);
        let update = update_with("TRADED");
        publish_dhan_event_record(Some(&tx), build_dhan_order_event_record(&update, 909));
        let rec = rx
            .recv()
            .await
            .unwrap_or_else(|| panic!("capture record expected"));
        assert_eq!(rec.ts_ist_nanos, 909);
        assert_eq!(rec.order_id, "112111182045");
        assert_eq!(rec.raw_status, "TRADED");
        assert_eq!(rec.feed, tickvault_common::feed::Feed::Dhan);
    }

    #[tokio::test]
    async fn test_publish_dhan_event_record_none_sender_is_a_noop() {
        // Capture disabled: no channel, no panic, no side effect.
        let update = update_with("PENDING");
        publish_dhan_event_record(None, build_dhan_order_event_record(&update, 1));
    }

    #[tokio::test]
    async fn test_publish_dhan_event_record_full_channel_drops_without_blocking() {
        let (tx, _rx) = mpsc::channel(1);
        let update = update_with("PENDING");
        // Fill the single slot, then publish again — must return (drop),
        // never block the consumer loop.
        publish_dhan_event_record(Some(&tx), build_dhan_order_event_record(&update, 1));
        publish_dhan_event_record(Some(&tx), build_dhan_order_event_record(&update, 2));
    }

    #[tokio::test]
    async fn test_publish_dhan_event_record_closed_channel_drops_without_panic() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let update = update_with("CANCELLED");
        publish_dhan_event_record(Some(&tx), build_dhan_order_event_record(&update, 3));
    }

    #[test]
    fn test_ist_midnight_nanos_floors_within_day() {
        let midnight = 1_784_073_600_000_000_000_i64; // some IST midnight-aligned nanos
        let midnight = midnight - midnight.rem_euclid(NANOS_PER_DAY);
        assert_eq!(ist_midnight_nanos(midnight), midnight);
        assert_eq!(ist_midnight_nanos(midnight + 1), midnight);
        assert_eq!(ist_midnight_nanos(midnight + NANOS_PER_DAY - 1), midnight);
        assert_eq!(
            ist_midnight_nanos(midnight + NANOS_PER_DAY),
            midnight + NANOS_PER_DAY
        );
    }
}
