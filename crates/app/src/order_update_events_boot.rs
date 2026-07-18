//! Full-fidelity order/position PUSH-event capture consumer (ORDER-EVT-01
//! — `.claude/rules/project/order-update-events-error-codes.md`).
//!
//! One supervised cold-path task drains the TWO bounded capture channels —
//! Dhan order-update records (built at the Phase 5a paper consumer) and
//! Groww order/position push records (built at the trading-side push
//! decode sites behind the `groww_orders` feature) — into the NEW
//! `order_update_events` / `position_update_events` QuestDB tables via
//! the storage writers (ILP-over-HTTP per-flush ACK, discard-pending
//! poisoned-buffer defense, DEDUP-idempotent re-appends).
//!
//! Contract points:
//! - **ADDITIVE forensic lane.** The lossy 11-field `order_audit` lane and
//!   the order-runtime hint lane are untouched; a failure here NEVER gates
//!   a mutation, an order path, or the producers' own audit rows.
//! - **Best-effort, loud.** Producer-side drops are coded
//!   `stage = "sink_drop"` at the producer; consumer-side persist failures
//!   are coded ORDER-EVT-01 `stage = "append"` / `"flush"` here — zero
//!   Telegram, zero CloudWatch filter (log-sink-only delivery boundary).
//! - **Supervised.** The consumer is respawned on abnormal exit (the
//!   DISK-WATCHER-01 / rest_candle_fold house pattern) with the RAII
//!   receiver re-park guard so a respawn actually resumes consuming
//!   (unwind builds only — release panics abort the process, the
//!   TICK-FLUSH-01 honesty note).
//! - **Cold path.** A handful of events per session; no tick-path
//!   involvement anywhere.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use metrics::counter;
use tickvault_common::broker_order_events::{
    ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY, OrderUpdateEventRecord, PositionUpdateEventRecord,
};
use tickvault_common::config::{OrderUpdateEventsConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::order_update_events_persistence::{
    OrderUpdateEventsWriter, ensure_order_update_events_table,
};
use tickvault_storage::position_update_events_persistence::{
    PositionUpdateEventsWriter, ensure_position_update_events_table,
};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Supervisor respawn backoff (the house 5s).
const CONSUMER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Shared receiver slot the RAII guard re-parks into (the
/// `rest_candle_fold` HIGH-2 pattern: without the re-park, a respawned
/// incarnation would take `None` forever and every respawn would be a
/// vacuous no-op reading as clean_exit).
type SharedReceiverSlot<T> = Arc<Mutex<Option<mpsc::Receiver<T>>>>;

/// RAII re-park guard: takes a receiver out of the shared slot for one
/// task incarnation and PUTS IT BACK on drop — unwind included.
struct ReceiverGuard<T> {
    slot: SharedReceiverSlot<T>,
    receiver: Option<mpsc::Receiver<T>>,
}

impl<T> ReceiverGuard<T> {
    /// Takes the receiver from the slot; `None` when the slot is empty
    /// (a prior incarnation leaked it — near-unreachable with the RAII
    /// drop, LOUD at the caller).
    fn take(slot: &SharedReceiverSlot<T>) -> Option<Self> {
        let receiver = slot.lock().unwrap_or_else(PoisonError::into_inner).take()?;
        Some(Self {
            slot: Arc::clone(slot),
            receiver: Some(receiver),
        })
    }

    /// Receives the next record (`None` = channel closed: every producer
    /// sender dropped).
    async fn recv(&mut self) -> Option<T> {
        match self.receiver.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }
}

impl<T> Drop for ReceiverGuard<T> {
    fn drop(&mut self) {
        if let Some(rx) = self.receiver.take() {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(rx);
        }
    }
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

/// Append + flush one ORDER capture record (cold path, per-record flush —
/// a handful of events per session). Failures are coded + counted; a
/// failed flush DISCARDS the pending buffer (poisoned-buffer defense —
/// the writer counts the discarded rows itself).
fn persist_order_record(writer: &mut OrderUpdateEventsWriter, record: &OrderUpdateEventRecord) {
    if let Err(err) = writer.append_order_update_event(record) {
        counter!("tv_order_update_events_persist_errors_total", "stage" => "append").increment(1);
        error!(
            code = ErrorCode::OrderEvt01PersistFailed.code_str(),
            stage = "append",
            channel = "order",
            feed = record.feed.as_str(),
            order_id = %record.order_id,
            ?err,
            "order_update_events: ILP append failed — capture row lost for this event \
             (forensic lane only; the order_audit lane and order paths are unaffected)"
        );
        return;
    }
    if let Err(err) = blocking_flush(|| writer.flush()) {
        counter!("tv_order_update_events_persist_errors_total", "stage" => "flush").increment(1);
        let discarded = writer.discard_pending();
        error!(
            code = ErrorCode::OrderEvt01PersistFailed.code_str(),
            stage = "flush",
            channel = "order",
            feed = record.feed.as_str(),
            order_id = %record.order_id,
            discarded,
            ?err,
            "order_update_events: ILP flush refused — pending capture rows discarded \
             (poisoned-buffer defense; DEDUP-idempotent re-appends make later repairs safe)"
        );
    }
}

/// Append + flush one POSITION capture record (same contract as the order
/// leg; the position writer shares the persist-error counter family).
fn persist_position_record(
    writer: &mut PositionUpdateEventsWriter,
    record: &PositionUpdateEventRecord,
) {
    if let Err(err) = writer.append_position_update_event(record) {
        counter!("tv_order_update_events_persist_errors_total", "stage" => "append").increment(1);
        error!(
            code = ErrorCode::OrderEvt01PersistFailed.code_str(),
            stage = "append",
            channel = "position",
            feed = record.feed.as_str(),
            symbol_isin = %record.symbol_isin,
            ?err,
            "position_update_events: ILP append failed — capture row lost for this event \
             (forensic lane only; no order path is affected)"
        );
        return;
    }
    if let Err(err) = blocking_flush(|| writer.flush()) {
        counter!("tv_order_update_events_persist_errors_total", "stage" => "flush").increment(1);
        let discarded = writer.discard_pending();
        error!(
            code = ErrorCode::OrderEvt01PersistFailed.code_str(),
            stage = "flush",
            channel = "position",
            feed = record.feed.as_str(),
            symbol_isin = %record.symbol_isin,
            discarded,
            ?err,
            "position_update_events: ILP flush refused — pending capture rows discarded \
             (poisoned-buffer defense; DEDUP-idempotent re-appends make later repairs safe)"
        );
    }
}

/// One consumer incarnation: ensure both tables (idempotent DDL self-heal),
/// then drain both capture channels until BOTH are closed (every producer
/// sender dropped = shutdown / all producer gates off) — a clean exit the
/// supervisor honors by stopping.
async fn run_order_update_events_consumer(
    questdb: QuestDbConfig,
    order_slot: SharedReceiverSlot<OrderUpdateEventRecord>,
    position_slot: SharedReceiverSlot<PositionUpdateEventRecord>,
) {
    ensure_order_update_events_table(&questdb).await;
    ensure_position_update_events_table(&questdb).await;

    let mut order_guard = ReceiverGuard::take(&order_slot);
    let mut position_guard = ReceiverGuard::take(&position_slot);
    if order_guard.is_none() && position_guard.is_none() {
        // Near-unreachable with the RAII re-park (unwind included) — a
        // leaked slot is a guard bug; exit clean-but-LOUD so the
        // supervisor stops instead of hot-looping on an empty slot.
        error!(
            code = ErrorCode::OrderEvt01PersistFailed.code_str(),
            stage = "receiver_lost",
            "order_update_events: BOTH capture receiver slots empty — a prior incarnation \
             leaked them; consumer exiting (capture lane down until restart)"
        );
        return;
    }

    let mut order_writer = OrderUpdateEventsWriter::new(&questdb);
    let mut position_writer = PositionUpdateEventsWriter::new(&questdb);
    let mut order_open = order_guard.is_some();
    let mut position_open = position_guard.is_some();

    loop {
        tokio::select! {
            record = async {
                match order_guard.as_mut() {
                    Some(guard) => guard.recv().await,
                    None => None,
                }
            }, if order_open => {
                match record {
                    Some(r) => persist_order_record(&mut order_writer, &r),
                    None => order_open = false,
                }
            }
            record = async {
                match position_guard.as_mut() {
                    Some(guard) => guard.recv().await,
                    None => None,
                }
            }, if position_open => {
                match record {
                    Some(r) => persist_position_record(&mut position_writer, &r),
                    None => position_open = false,
                }
            }
            else => break,
        }
    }
    info!(
        "order_update_events: every capture channel closed (all producer senders dropped) — \
         consumer exiting clean"
    );
}

/// Spawns the supervised capture consumer (house respawn pattern —
/// DISK-WATCHER-01 / rest_candle_fold family). Honest panic envelope:
/// release builds run `panic = "abort"`, so the respawn arm self-heals in
/// unwind (dev/test) builds only.
fn spawn_supervised_order_update_events_consumer(
    questdb: QuestDbConfig,
    order_rx: mpsc::Receiver<OrderUpdateEventRecord>,
    position_rx: mpsc::Receiver<PositionUpdateEventRecord>,
) -> tokio::task::JoinHandle<()> {
    let order_slot: SharedReceiverSlot<OrderUpdateEventRecord> =
        Arc::new(Mutex::new(Some(order_rx)));
    let position_slot: SharedReceiverSlot<PositionUpdateEventRecord> =
        Arc::new(Mutex::new(Some(position_rx)));
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_order_update_events_consumer(
                questdb.clone(),
                Arc::clone(&order_slot),
                Arc::clone(&position_slot),
            ));
            let result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
            counter!("tv_order_update_events_task_respawn_total", "reason" => reason).increment(1);
            if reason == "clean_exit" {
                info!("order_update_events: supervisor observed clean exit — stopping");
                return;
            }
            error!(
                code = ErrorCode::OrderEvt01PersistFailed.code_str(),
                stage = "task_respawn",
                reason,
                "order_update_events: capture consumer died — respawning after backoff \
                 (the re-park guard returned the receivers, so the respawn resumes \
                 consuming; unwind builds only — release panics abort the process)"
            );
            tokio::time::sleep(Duration::from_secs(CONSUMER_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// Config-gated entry point (main.rs boot wiring): when
/// `[order_update_events] enabled`, builds the two bounded capture
/// channels, spawns the supervised consumer, and returns the producer
/// senders — the Dhan sender rides into `DhanRestStackParams`
/// (`order_update_events_tx`), the pair feeds `GrowwPushCapture` under
/// the `groww_orders` feature. Disabled ⇒ `(None, None)` and NOTHING is
/// spawned (byte-identical dormant boot).
#[must_use]
pub fn spawn_order_update_events_capture(
    config: &OrderUpdateEventsConfig,
    questdb: &QuestDbConfig,
) -> (
    Option<mpsc::Sender<OrderUpdateEventRecord>>,
    Option<mpsc::Sender<PositionUpdateEventRecord>>,
) {
    if !config.enabled {
        info!(
            "order_update_events disabled (config) — full-fidelity push-event capture lane \
             not spawned"
        );
        return (None, None);
    }
    let (order_tx, order_rx) = mpsc::channel(ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY);
    let (position_tx, position_rx) = mpsc::channel(ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY);
    info!(
        capacity = ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY,
        "order_update_events: spawning supervised full-fidelity capture consumer \
         (order_update_events + position_update_events)"
    );
    let _supervisor =
        spawn_supervised_order_update_events_consumer(questdb.clone(), order_rx, position_rx);
    (Some(order_tx), Some(position_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::broker_order_events::build_dhan_order_event_record;
    use tickvault_common::order_types::OrderUpdate;

    fn test_config(enabled: bool) -> OrderUpdateEventsConfig {
        OrderUpdateEventsConfig { enabled }
    }

    /// Port 1 is reserved and never listening — guarantees a real HTTP
    /// transport failure without touching any live service (the storage
    /// unreachable_cfg pattern).
    fn offline_questdb() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn order_record() -> OrderUpdateEventRecord {
        let update: OrderUpdate =
            serde_json::from_str("{}").expect("OrderUpdate fields are serde-default"); // APPROVED: test
        build_dhan_order_event_record(&update, 1)
    }

    /// Disabled config → `(None, None)` and nothing spawned.
    #[tokio::test]
    async fn test_spawn_capture_disabled_returns_none_pair() {
        let (order_tx, position_tx) =
            spawn_order_update_events_capture(&test_config(false), &offline_questdb());
        assert!(order_tx.is_none());
        assert!(position_tx.is_none());
    }

    /// Enabled config → both producer senders live; a record is accepted
    /// by the bounded channel (the consumer drains it — QuestDB being
    /// offline degrades to coded persist errors, never a panic).
    #[tokio::test]
    async fn test_spawn_capture_enabled_returns_live_senders() {
        let (order_tx, position_tx) =
            spawn_order_update_events_capture(&test_config(true), &offline_questdb());
        let order_tx = order_tx.expect("enabled capture must return an order sender");
        assert!(position_tx.is_some());
        order_tx
            .try_send(order_record())
            .expect("bounded capture channel must accept a record");
    }

    /// The RAII guard re-parks the receiver on drop (unwind-respawn
    /// resume contract — the rest_candle_fold HIGH-2 pattern).
    #[tokio::test]
    async fn test_receiver_guard_reparks_on_drop() {
        let (_tx, rx) = mpsc::channel::<OrderUpdateEventRecord>(4);
        let slot: SharedReceiverSlot<OrderUpdateEventRecord> = Arc::new(Mutex::new(Some(rx)));
        {
            let guard = ReceiverGuard::take(&slot).expect("first take must succeed");
            // While taken, a second take sees an empty slot.
            assert!(ReceiverGuard::take(&slot).is_none());
            drop(guard);
        }
        // Re-parked: a fresh take succeeds again.
        assert!(ReceiverGuard::take(&slot).is_some());
    }

    /// Both channels closing drains the consumer to a clean exit (the
    /// supervisor's stop condition) — exercised through the inner run fn
    /// directly with an offline-QuestDB config (ensure/persist degrade
    /// loudly, never panic).
    #[tokio::test]
    async fn test_consumer_exits_clean_when_both_channels_close() {
        let (order_tx, order_rx) = mpsc::channel::<OrderUpdateEventRecord>(4);
        let (position_tx, position_rx) = mpsc::channel::<PositionUpdateEventRecord>(4);
        let order_slot: SharedReceiverSlot<OrderUpdateEventRecord> =
            Arc::new(Mutex::new(Some(order_rx)));
        let position_slot: SharedReceiverSlot<PositionUpdateEventRecord> =
            Arc::new(Mutex::new(Some(position_rx)));
        let task = tokio::spawn(run_order_update_events_consumer(
            offline_questdb(),
            Arc::clone(&order_slot),
            Arc::clone(&position_slot),
        ));
        drop(order_tx);
        drop(position_tx);
        tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("consumer must exit once both channels close")
            .expect("consumer task must not panic");
    }

    /// `config/base.toml` OPTS IN to the capture lane (the serde default
    /// stays OFF — fail-safe; base.toml is the deliberate enable, the
    /// house pattern of `[spot_1m_rest]` / `[option_chain_1m]`).
    #[test]
    fn test_base_toml_enables_order_update_events() {
        let toml_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("config")
            .join("base.toml");
        let raw = std::fs::read_to_string(&toml_path).expect("config/base.toml must be readable");
        let section_start = raw
            .find("[order_update_events]")
            .expect("config/base.toml must carry the [order_update_events] section");
        let section = &raw[section_start..];
        let section_end = section[1..]
            .find("\n[")
            .map(|idx| idx + 1)
            .unwrap_or(section.len());
        let body = &section[..section_end];
        assert!(
            body.lines().any(|line| {
                let trimmed = line.split('#').next().unwrap_or("").trim();
                trimmed == "enabled = true"
            }),
            "config/base.toml [order_update_events] must set enabled = true \
             (the deliberate opt-in; serde default is OFF)"
        );
    }
}
