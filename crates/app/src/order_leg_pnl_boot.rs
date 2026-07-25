//! Order-leg P&L capture boot: the supervised QuestDB persistence consumer
//! for the dry-run order runtime's per-leg realized/unrealized P&L events.
//!
//! Producer side: `order_runtime::emit_leg_pnl` — a bounded `try_send` on
//! the channel created here (the mark/fill hot path never blocks; drops are
//! counted + edge-latched under ORDER-PNL-01 stage="sink_drop").
//!
//! Consumer side (this module, cold path): resolves each event's option-leg
//! identity lock-free from the shared leg-identity index the Groww cadence
//! executor publishes once per daily master download, stamps `event_seq`
//! ONCE per persisted row (consumer-side — a binding pin of the approved
//! plan), and drains to the `order_leg_pnl` QuestDB table with ONE ILP
//! flush per drain batch.
//!
//! Effective gate: `order_runtime.enabled && order_leg_pnl.enabled`,
//! resolved ONCE at spawn. OFF ⇒ `None` sender — byte-identical dormant
//! boot (`emit_leg_pnl` no-ops on `None`). Dry-run paper mode only; no
//! live-order behavior change.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use metrics::counter;
use tickvault_common::broker_order_events::next_event_seq;
use tickvault_common::config::{OrderLegPnlConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::order_leg_pnl_persistence::{
    OrderLegPnlRecord, OrderLegPnlWriter, ensure_order_leg_pnl_table,
};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::groww_cadence_executor::SharedLegIdentityIndex;
use crate::order_runtime::{LegPnlEvent, LegPnlKind};

/// Seconds between supervised consumer respawns after an abnormal exit.
const CONSUMER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// IST is UTC+05:30 — fixed offset, no DST (`market-hours.md`).
const IST_OFFSET_NANOS: i64 = 19_800 * 1_000_000_000;

/// Identity sentinel for a leg whose `(security_id, segment)` is absent
/// from the published leg-identity index (the pre-publish window, or a
/// non-watch contract). Persisted honestly + counted — never fabricated.
const UNRESOLVED_IDENTITY: &str = "unresolved";

fn segment_slug(code: u8) -> &'static str {
    match code {
        2 => "NSE_FNO",
        8 => "BSE_FNO",
        _ => "UNKNOWN",
    }
}

fn event_kind_slug(kind: LegPnlKind) -> &'static str {
    match kind {
        LegPnlKind::Mark => "mark",
        LegPnlKind::Fill => "fill",
    }
}

/// Shared slot the supervisor uses to hand the single receiver to each
/// consumer incarnation; the RAII guard re-parks it on drop so a respawn
/// can take it again.
type SharedReceiverSlot<T> = Arc<Mutex<Option<mpsc::Receiver<T>>>>;

struct ReceiverGuard<T> {
    slot: SharedReceiverSlot<T>,
    receiver: Option<mpsc::Receiver<T>>,
}

impl<T> ReceiverGuard<T> {
    fn take(slot: &SharedReceiverSlot<T>) -> Option<Self> {
        let receiver = slot.lock().unwrap_or_else(PoisonError::into_inner).take()?;
        Some(Self {
            slot: Arc::clone(slot),
            receiver: Some(receiver),
        })
    }

    async fn recv(&mut self) -> Option<T> {
        match self.receiver.as_mut() {
            Some(receiver) => receiver.recv().await,
            None => None,
        }
    }

    fn try_recv(&mut self) -> Option<T> {
        self.receiver
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
    }
}

impl<T> Drop for ReceiverGuard<T> {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(receiver);
        }
    }
}

/// Build the persisted row for one leg P&L event. `event_seq` is stamped
/// HERE, once per persisted row (consumer-side — producer-side stamping is
/// forbidden by the plan's binding pin).
fn record_from_event(
    event: &LegPnlEvent,
    identity_index: &SharedLegIdentityIndex,
) -> OrderLegPnlRecord {
    let published = identity_index.load_full();
    let identity = published
        .as_ref()
        .and_then(|day_index| day_index.1.get(&(event.sid, event.segment_code)));
    if identity.is_none() {
        counter!("tv_order_leg_pnl_identity_unresolved_total").increment(1);
    }
    let (underlying, expiry, strike_paise, option_type) = match identity {
        Some(id) => (
            id.underlying,
            id.expiry.to_string(),
            id.strike_paise,
            id.option_type.as_str(),
        ),
        None => (UNRESOLVED_IDENTITY, "n/a".to_string(), 0_i64, "n/a"),
    };
    OrderLegPnlRecord {
        ts_ist_nanos: event.ts_utc_ns.saturating_add(IST_OFFSET_NANOS),
        feed: "groww",
        security_id: i64::try_from(event.sid).unwrap_or(-1),
        segment: segment_slug(event.segment_code),
        event_seq: next_event_seq(),
        event_kind: event_kind_slug(event.event_kind),
        underlying,
        expiry,
        strike_paise,
        option_type,
        net_lots: event.net_lots,
        lot_size: event.lot_size,
        avg_entry_price: event.avg_entry_price,
        mark_price: event.mark_price,
        realized_pnl: event.realized_pnl,
        unrealized_pnl: event.unrealized_pnl,
    }
}

fn persist_leg_pnl_record(writer: &mut OrderLegPnlWriter, record: &OrderLegPnlRecord) {
    if let Err(err) = writer.append(record) {
        counter!("tv_order_leg_pnl_persist_errors_total", "stage" => "append").increment(1);
        error!(
            code = ErrorCode::OrderPnl01PersistFailed.code_str(),
            stage = "append",
            error = %err,
            "order_leg_pnl: row append rejected — row skipped, consumer continues"
        );
    }
}

fn flush_leg_pnl_writer(writer: &mut OrderLegPnlWriter) {
    if writer.pending() == 0 {
        return;
    }
    if let Err(err) = writer.flush() {
        counter!("tv_order_leg_pnl_persist_errors_total", "stage" => "flush").increment(1);
        error!(
            code = ErrorCode::OrderPnl01PersistFailed.code_str(),
            stage = "flush",
            error = %err,
            "order_leg_pnl: ILP flush refused — pending rows discarded \
             (poisoned-buffer defense; DEDUP-idempotent re-appends)"
        );
    }
}

async fn run_order_leg_pnl_consumer(
    questdb: QuestDbConfig,
    rx_slot: SharedReceiverSlot<LegPnlEvent>,
    identity_index: SharedLegIdentityIndex,
) {
    ensure_order_leg_pnl_table(&questdb).await;
    let Some(mut guard) = ReceiverGuard::take(&rx_slot) else {
        error!(
            code = ErrorCode::OrderPnl01PersistFailed.code_str(),
            stage = "receiver_missing",
            "order_leg_pnl: receiver slot empty — consumer exiting"
        );
        return;
    };
    let mut writer = OrderLegPnlWriter::new(&questdb);
    info!(
        "order_leg_pnl: consumer running — draining leg P&L events to QuestDB \
         (feed=groww, paper mode)"
    );
    while let Some(event) = guard.recv().await {
        persist_leg_pnl_record(&mut writer, &record_from_event(&event, &identity_index));
        while let Some(event) = guard.try_recv() {
            persist_leg_pnl_record(&mut writer, &record_from_event(&event, &identity_index));
        }
        flush_leg_pnl_writer(&mut writer);
    }
    info!("order_leg_pnl: channel closed — consumer exiting");
}

fn spawn_supervised_order_leg_pnl_consumer(
    questdb: QuestDbConfig,
    rx: mpsc::Receiver<LegPnlEvent>,
    identity_index: SharedLegIdentityIndex,
) -> tokio::task::JoinHandle<()> {
    let rx_slot: SharedReceiverSlot<LegPnlEvent> = Arc::new(Mutex::new(Some(rx)));
    tokio::spawn(async move {
        loop {
            let consumer = tokio::spawn(run_order_leg_pnl_consumer(
                questdb.clone(),
                Arc::clone(&rx_slot),
                Arc::clone(&identity_index),
            ));
            match consumer.await {
                // Clean exit (channel closed / receiver missing): no respawn.
                Ok(()) => return,
                Err(join_error) if join_error.is_panic() => {
                    // Unwind-build self-heal only: release builds abort on
                    // panic (`panic = "abort"`), so this arm serves dev/test
                    // unwinds (the TICK-FLUSH-01 honesty note).
                    counter!("tv_order_leg_pnl_task_respawn_total", "reason" => "panic")
                        .increment(1);
                    error!(
                        code = ErrorCode::OrderPnl01PersistFailed.code_str(),
                        stage = "consumer_respawn",
                        "order_leg_pnl: consumer panicked — respawning after backoff"
                    );
                    tokio::time::sleep(Duration::from_secs(CONSUMER_RESPAWN_BACKOFF_SECS)).await;
                }
                Err(_cancelled) => {
                    counter!("tv_order_leg_pnl_task_respawn_total", "reason" => "cancelled")
                        .increment(1);
                    info!(
                        "order_leg_pnl: consumer cancelled (runtime shutdown) — \
                         supervisor exiting"
                    );
                    return;
                }
            }
        }
    })
}

/// Spawn the order-leg P&L capture consumer.
///
/// The effective gate is `order_runtime.enabled && order_leg_pnl.enabled`,
/// resolved ONCE here at spawn time. OFF ⇒ returns `None` and spawns
/// nothing — the producer seam (`emit_leg_pnl`) treats a `None` sender as
/// a no-op, so a disabled boot is byte-identical to pre-feature behavior.
#[must_use]
pub fn spawn_order_leg_pnl_capture(
    order_runtime_enabled: bool,
    config: &OrderLegPnlConfig,
    questdb: &QuestDbConfig,
    identity_index: SharedLegIdentityIndex,
) -> Option<mpsc::Sender<LegPnlEvent>> {
    if !(order_runtime_enabled && config.enabled) {
        info!(
            order_runtime_enabled,
            leg_pnl_enabled = config.enabled,
            "order_leg_pnl: capture disabled — no consumer spawned, sender absent"
        );
        return None;
    }
    let capacity = config.channel_capacity.max(1);
    let (tx, rx) = mpsc::channel(capacity);
    let _supervisor = spawn_supervised_order_leg_pnl_consumer(questdb.clone(), rx, identity_index);
    info!(
        channel_capacity = capacity,
        "order_leg_pnl: supervised QuestDB consumer spawned (feed=groww, paper mode)"
    );
    Some(tx)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;
    use crate::groww_cadence_executor::{
        LegIdentityIndex, OptionLegIdentity, new_shared_leg_identity_index,
    };

    fn questdb_config() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn sample_event() -> LegPnlEvent {
        LegPnlEvent {
            ts_utc_ns: 1_000_000_000,
            sid: 42,
            segment_code: 2,
            event_kind: LegPnlKind::Mark,
            net_lots: 2,
            lot_size: 75,
            avg_entry_price: 100.5,
            mark_price: 110.25,
            realized_pnl: 0.0,
            unrealized_pnl: 1_462.5,
        }
    }

    #[test]
    fn test_boot_gate_resolved_once() {
        let questdb = questdb_config();
        let on = OrderLegPnlConfig {
            enabled: true,
            channel_capacity: 4,
        };
        let off = OrderLegPnlConfig {
            enabled: false,
            channel_capacity: 4,
        };
        // Runtime disabled dominates: no sender regardless of the flag.
        assert!(
            spawn_order_leg_pnl_capture(false, &on, &questdb, new_shared_leg_identity_index())
                .is_none()
        );
        // Feature disabled: no sender even with the runtime enabled.
        assert!(
            spawn_order_leg_pnl_capture(true, &off, &questdb, new_shared_leg_identity_index())
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_order_leg_pnl_capture_enabled_returns_sender() {
        let cfg = OrderLegPnlConfig {
            enabled: true,
            channel_capacity: 0, // clamps to 1 — never a zero-capacity panic
        };
        let tx = spawn_order_leg_pnl_capture(
            true,
            &cfg,
            &questdb_config(),
            new_shared_leg_identity_index(),
        );
        assert!(tx.is_some());
    }

    #[test]
    fn test_record_identity_sentinels_when_unresolved() {
        let record = record_from_event(&sample_event(), &new_shared_leg_identity_index());
        assert_eq!(record.underlying, "unresolved");
        assert_eq!(record.expiry, "n/a");
        assert_eq!(record.option_type, "n/a");
        assert_eq!(record.strike_paise, 0);
        assert_eq!(record.feed, "groww");
        assert_eq!(record.segment, "NSE_FNO");
        assert_eq!(record.event_kind, "mark");
        assert_eq!(record.security_id, 42);
        assert_eq!(record.ts_ist_nanos, 1_000_000_000 + IST_OFFSET_NANOS);
    }

    #[test]
    fn test_record_resolves_identity_from_index() {
        let index = new_shared_leg_identity_index();
        let mut map = LegIdentityIndex::new();
        let expiry = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        map.insert(
            (42, 2),
            OptionLegIdentity {
                underlying: "NIFTY",
                expiry,
                strike_paise: 2_500_000,
                option_type: tickvault_common::types::OptionType::Call,
            },
        );
        index.store(Some(std::sync::Arc::new((expiry, map))));
        let record = record_from_event(&sample_event(), &index);
        assert_eq!(record.underlying, "NIFTY");
        assert_eq!(record.expiry, "2026-07-30");
        assert_eq!(record.strike_paise, 2_500_000);
        assert_eq!(record.option_type, "CE");
    }

    #[test]
    fn test_event_seq_stamped_consumer_side_strictly_increasing() {
        let index = new_shared_leg_identity_index();
        let first = record_from_event(&sample_event(), &index);
        let second = record_from_event(&sample_event(), &index);
        assert!(second.event_seq > first.event_seq);
    }

    #[test]
    fn test_segment_and_kind_slugs() {
        assert_eq!(segment_slug(2), "NSE_FNO");
        assert_eq!(segment_slug(8), "BSE_FNO");
        assert_eq!(segment_slug(0), "UNKNOWN");
        assert_eq!(event_kind_slug(LegPnlKind::Mark), "mark");
        assert_eq!(event_kind_slug(LegPnlKind::Fill), "fill");
    }
}
