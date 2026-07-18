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

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use metrics::counter;
use tickvault_common::broker_order_events::{
    ORDER_UPDATE_EVENTS_CHANNEL_CAPACITY, OrderUpdateEventRecord, PositionUpdateEventRecord,
};
use tickvault_common::config::{OrderUpdateEventsConfig, QuestDbConfig};
use tickvault_common::constants::{GROWW_DATA_DIR, IST_UTC_OFFSET_SECONDS_I64};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_core::feed::groww::watch_reader::{
    WatchFileDoc, WatchFileEntry, WatchFileKind, parse_watch_file,
};
use tickvault_storage::order_update_events_persistence::{
    OrderUpdateEventsWriter, ensure_order_update_events_table,
};
use tickvault_storage::position_update_events_persistence::{
    PositionUpdateEventsWriter, ensure_position_update_events_table,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Supervisor respawn backoff (the house 5s).
const CONSUMER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// The record builders' honest "unresolvable" security-id sentinel
/// (`broker_order_events.rs`: "`-1` ONLY when unresolvable").
const UNRESOLVED_SECURITY_ID: i64 = -1;

/// Bounded + sanitized broker identity for LOG interpolation (the
/// `order_runtime.rs::log_safe_id` house pattern) — strips control/BiDi
/// chars and caps the length so a crafted multi-KB vendor `order_id` /
/// `symbol_isin` cannot amplify `errors.log` (review round 1 SECURITY
/// MEDIUM-2; cold error-path alloc is fine). The PERSISTED values are
/// separately ILP-sanitized + bounded by the storage writers.
fn log_safe_id(raw: &str) -> String {
    tickvault_common::sanitize::sanitize_audit_string(raw)
        .chars()
        .take(64)
        .collect()
}

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

// ---------------------------------------------------------------------------
// Best-effort Groww security-id resolution (review round 1 Fix 5 — hostile #1)
// ---------------------------------------------------------------------------

/// Per-day Groww security-id lookup built from the day's watch file — the
/// SAME id space the Groww lane persists everywhere else (stocks: the
/// numeric `exchange_token`; indices: `stable_index_security_id` — both
/// already RESOLVED by the watch BUILDER (`instruments.rs`); this consumer
/// only indexes the builder's output, it never re-derives an id).
struct GrowwSecurityIdIndex {
    /// Uppercased ISIN → `(security_id, exchange_segment slug)`.
    by_isin: HashMap<String, (i64, String)>,
    /// Uppercased display/contract symbol (`symbol_name` / `index_name`) →
    /// `(security_id, exchange_segment slug)`.
    by_symbol: HashMap<String, (i64, String)>,
}

/// House segment slug for a watch entry (the I-P1-11 shared-table
/// vocabulary: `IDX_I` / `NSE_EQ` / `BSE_EQ` / `NSE_FNO` / `BSE_FNO`);
/// an unrecognized combo falls back raw-preserving (`<EXCH>_<SEG>`),
/// never a guess.
fn groww_watch_segment_slug(entry: &WatchFileEntry) -> String {
    if entry.kind == WatchFileKind::IndexValue {
        return "IDX_I".to_owned();
    }
    match (entry.exchange.as_str(), entry.segment.as_str()) {
        ("NSE", "CASH") => "NSE_EQ".to_owned(),
        ("BSE", "CASH") => "BSE_EQ".to_owned(),
        ("NSE", "FNO") => "NSE_FNO".to_owned(),
        ("BSE", "FNO") => "BSE_FNO".to_owned(),
        (exchange, segment) => format!("{exchange}_{segment}"),
    }
}

/// Build the per-day lookup from a parsed watch file. Pure; first entry
/// wins on a duplicate key (the watch builder already dedups identities —
/// a duplicate here would be vendor-master corruption, kept deterministic).
fn build_groww_security_id_index(doc: &WatchFileDoc) -> GrowwSecurityIdIndex {
    let mut by_isin: HashMap<String, (i64, String)> = HashMap::new();
    let mut by_symbol: HashMap<String, (i64, String)> = HashMap::new();
    for entry in &doc.entries {
        let slug = groww_watch_segment_slug(entry);
        if let Some(isin) = entry.isin.as_deref() {
            let key = isin.trim().to_ascii_uppercase();
            if !key.is_empty() {
                by_isin
                    .entry(key)
                    .or_insert_with(|| (entry.security_id, slug.clone()));
            }
        }
        for symbol in [entry.symbol_name.as_deref(), entry.index_name.as_deref()]
            .into_iter()
            .flatten()
        {
            let key = symbol.trim().to_ascii_uppercase();
            if !key.is_empty() {
                by_symbol
                    .entry(key)
                    .or_insert_with(|| (entry.security_id, slug.clone()));
            }
        }
    }
    GrowwSecurityIdIndex { by_isin, by_symbol }
}

impl GrowwSecurityIdIndex {
    /// Resolve the FIRST candidate that hits — each candidate is tried
    /// against the ISIN map (exact identity) then the symbol map
    /// (display/contract fallback). Pure; `None` = honest unresolved
    /// (the record's `-1` stands, never fabricated).
    fn resolve(&self, candidates: &[&str]) -> Option<(i64, String)> {
        for candidate in candidates {
            let key = candidate.trim().to_ascii_uppercase();
            if key.is_empty() {
                continue;
            }
            if let Some((id, slug)) = self.by_isin.get(&key).or_else(|| self.by_symbol.get(&key)) {
                return Some((*id, slug.clone()));
            }
        }
        None
    }
}

/// Today's IST date (`YYYY-MM-DD`) — the watch-file naming key.
fn today_ist_date_string() -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64))
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// Read + parse + index today's watch file. Thin fs shim over the pure
/// reader (`parse_watch_file`) + pure index build.
// TEST-EXEMPT: thin fs-read shim — parse/index/resolve are pure and unit-tested below.
fn load_groww_watch_index(date: &str) -> Result<GrowwSecurityIdIndex, String> {
    let path = crate::groww_watch_paths::watch_file_path_for(Path::new(GROWW_DATA_DIR), date);
    let json =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc = parse_watch_file(&json).map_err(|e| e.to_string())?;
    Ok(build_groww_security_id_index(&doc))
}

/// LAZY per-IST-day watch-index cache. Best-effort by contract: the daily
/// watch build lands asynchronously AFTER boot (the `[groww_universe]`
/// rider), so the load is attempted on the FIRST Groww record that needs
/// resolution — not at spawn, where a miss would be a guaranteed boot-race
/// false warn. An absent/unparseable file logs ONE coalesced `warn!` per
/// IST day and leaves records at the honest `-1` (re-attempted per
/// unresolved record — bounded by the handful of push events per session,
/// cold path).
struct GrowwIdResolver {
    date: String,
    index: Option<GrowwSecurityIdIndex>,
    warned: bool,
}

impl GrowwIdResolver {
    fn new() -> Self {
        Self {
            date: String::new(),
            index: None,
            warned: false,
        }
    }

    /// The index for today's watch file, loading it on first need (and
    /// re-loading after an IST day roll). `None` = file absent/unusable —
    /// warned once per day, records stay `-1`.
    fn index_for_today(&mut self) -> Option<&GrowwSecurityIdIndex> {
        let today = today_ist_date_string();
        if today != self.date {
            self.date = today;
            self.index = None;
            self.warned = false;
        }
        if self.index.is_none() {
            match load_groww_watch_index(&self.date) {
                Ok(index) => {
                    info!(
                        date = %self.date,
                        isin_keys = index.by_isin.len(),
                        symbol_keys = index.by_symbol.len(),
                        "order_update_events: Groww watch-file id index loaded \
                         (best-effort security_id resolution active)"
                    );
                    self.index = Some(index);
                }
                Err(reason) => {
                    if !self.warned {
                        self.warned = true;
                        warn!(
                            date = %self.date,
                            reason = %reason,
                            "order_update_events: Groww watch file unavailable — \
                             push-event security_id stays the honest -1 for the day \
                             (best-effort resolution; coalesced once per day)"
                        );
                    }
                }
            }
        }
        self.index.as_ref()
    }
}

/// Best-effort enrich a Groww ORDER record whose builder left the honest
/// `-1`: candidates in identity order — `contract_id` (ISIN for equity /
/// contract symbol for F&O) then `symbol`. On a hit BOTH `security_id`
/// and `exchange_segment` are set from the watch entry (the same-id-space
/// contract); a miss leaves the record untouched.
fn resolve_groww_order_identity(
    record: &mut OrderUpdateEventRecord,
    resolver: &mut GrowwIdResolver,
) {
    if record.feed != Feed::Groww || record.security_id != UNRESOLVED_SECURITY_ID {
        return;
    }
    let Some(index) = resolver.index_for_today() else {
        return;
    };
    if let Some((id, slug)) = index.resolve(&[&record.contract_id, &record.symbol]) {
        record.security_id = id;
        record.exchange_segment = slug;
    }
}

/// Best-effort enrich a Groww POSITION record (same contract as the order
/// leg): candidates — `symbol_isin` (the wire identity), `contract_id`,
/// then `symbol`.
fn resolve_groww_position_identity(
    record: &mut PositionUpdateEventRecord,
    resolver: &mut GrowwIdResolver,
) {
    if record.feed != Feed::Groww || record.security_id != UNRESOLVED_SECURITY_ID {
        return;
    }
    let Some(index) = resolver.index_for_today() else {
        return;
    };
    if let Some((id, slug)) =
        index.resolve(&[&record.symbol_isin, &record.contract_id, &record.symbol])
    {
        record.security_id = id;
        record.exchange_segment = slug;
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
            order_id = %log_safe_id(&record.order_id),
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
            order_id = %log_safe_id(&record.order_id),
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
            symbol_isin = %log_safe_id(&record.symbol_isin),
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
            symbol_isin = %log_safe_id(&record.symbol_isin),
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
    let mut groww_resolver = GrowwIdResolver::new();

    loop {
        tokio::select! {
            record = async {
                match order_guard.as_mut() {
                    Some(guard) => guard.recv().await,
                    None => None,
                }
            }, if order_open => {
                match record {
                    Some(mut r) => {
                        resolve_groww_order_identity(&mut r, &mut groww_resolver);
                        persist_order_record(&mut order_writer, &r);
                    }
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
                    Some(mut r) => {
                        resolve_groww_position_identity(&mut r, &mut groww_resolver);
                        persist_position_record(&mut position_writer, &r);
                    }
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

    fn watch_doc() -> WatchFileDoc {
        WatchFileDoc {
            trading_date_ist: "2026-07-18".to_owned(),
            entries: vec![
                WatchFileEntry {
                    exchange: "NSE".to_owned(),
                    segment: "CASH".to_owned(),
                    exchange_token: "2885".to_owned(),
                    kind: WatchFileKind::Ltp,
                    security_id: 2885,
                    index_name: None,
                    symbol_name: Some("RELIANCE".to_owned()),
                    isin: Some("INE002A01018".to_owned()),
                },
                WatchFileEntry {
                    exchange: "NSE".to_owned(),
                    segment: "CASH".to_owned(),
                    exchange_token: "NIFTY".to_owned(),
                    kind: WatchFileKind::IndexValue,
                    security_id: 4_611_686_018_427_387_917,
                    index_name: Some("NSE-NIFTY".to_owned()),
                    symbol_name: Some("Nifty 50".to_owned()),
                    isin: None,
                },
                WatchFileEntry {
                    exchange: "BSE".to_owned(),
                    segment: "FNO".to_owned(),
                    exchange_token: "71001".to_owned(),
                    kind: WatchFileKind::Ltp,
                    security_id: 71001,
                    index_name: None,
                    symbol_name: Some("BSE-SENSEX-31Jul26-FUT".to_owned()),
                    isin: None,
                },
            ],
        }
    }

    /// Fix 5 pure core: ISIN candidates resolve to the stock's numeric
    /// exchange-token id + the house `NSE_EQ` slug (case/whitespace
    /// tolerant).
    #[test]
    fn test_groww_index_resolves_by_isin() {
        let index = build_groww_security_id_index(&watch_doc());
        assert_eq!(
            index.resolve(&["ine002a01018"]),
            Some((2885, "NSE_EQ".to_owned()))
        );
        assert_eq!(
            index.resolve(&["  INE002A01018  "]),
            Some((2885, "NSE_EQ".to_owned()))
        );
    }

    /// Symbol fallback: display names (`symbol_name`), index names
    /// (`index_name` → `IDX_I` + the stable index id), and FNO contract
    /// symbols (`BSE_FNO`) all resolve; a miss is an honest `None` and
    /// empty candidates are skipped.
    #[test]
    fn test_groww_index_resolves_by_symbol_and_misses_honestly() {
        let index = build_groww_security_id_index(&watch_doc());
        assert_eq!(
            index.resolve(&["", "RELIANCE"]),
            Some((2885, "NSE_EQ".to_owned())),
            "empty candidates are skipped; symbol fallback hits"
        );
        assert_eq!(
            index.resolve(&["NSE-NIFTY"]),
            Some((4_611_686_018_427_387_917, "IDX_I".to_owned()))
        );
        assert_eq!(
            index.resolve(&["bse-sensex-31jul26-fut"]),
            Some((71001, "BSE_FNO".to_owned()))
        );
        assert_eq!(index.resolve(&["UNKNOWN-THING"]), None);
        assert_eq!(index.resolve(&[]), None);
    }

    /// Fix 5 record enrichment: a Groww order record with the honest `-1`
    /// gains BOTH `security_id` and `exchange_segment` on a hit; a Dhan
    /// record and an already-resolved Groww record are never touched.
    #[test]
    fn test_resolve_groww_order_identity_mutates_only_unresolved_groww() {
        let mut resolver = GrowwIdResolver {
            date: today_ist_date_string(),
            index: Some(build_groww_security_id_index(&watch_doc())),
            warned: false,
        };

        // Groww + unresolved → enriched via contract_id (ISIN).
        let mut groww = order_record();
        groww.feed = Feed::Groww;
        groww.security_id = UNRESOLVED_SECURITY_ID;
        groww.contract_id = "INE002A01018".to_owned();
        groww.exchange_segment = "CASH".to_owned();
        resolve_groww_order_identity(&mut groww, &mut resolver);
        assert_eq!(groww.security_id, 2885);
        assert_eq!(groww.exchange_segment, "NSE_EQ");

        // Dhan record: untouched even with a matching symbol.
        let mut dhan = order_record();
        dhan.security_id = UNRESOLVED_SECURITY_ID;
        dhan.symbol = "RELIANCE".to_owned();
        let before = dhan.exchange_segment.clone();
        resolve_groww_order_identity(&mut dhan, &mut resolver);
        assert_eq!(dhan.security_id, UNRESOLVED_SECURITY_ID);
        assert_eq!(dhan.exchange_segment, before);

        // Groww record already carrying a real id: untouched.
        let mut resolved = order_record();
        resolved.feed = Feed::Groww;
        resolved.security_id = 42;
        resolved.symbol = "RELIANCE".to_owned();
        resolve_groww_order_identity(&mut resolved, &mut resolver);
        assert_eq!(resolved.security_id, 42);
    }

    /// A miss (unknown identity) leaves the honest `-1` + the builder's
    /// segment label untouched — never fabricated.
    #[test]
    fn test_resolve_groww_order_identity_miss_keeps_honest_minus_one() {
        let mut resolver = GrowwIdResolver {
            date: today_ist_date_string(),
            index: Some(build_groww_security_id_index(&watch_doc())),
            warned: false,
        };
        let mut groww = order_record();
        groww.feed = Feed::Groww;
        groww.security_id = UNRESOLVED_SECURITY_ID;
        groww.contract_id = "INE_UNKNOWN".to_owned();
        groww.symbol = "NOT-IN-WATCH".to_owned();
        groww.exchange_segment = "FNO".to_owned();
        resolve_groww_order_identity(&mut groww, &mut resolver);
        assert_eq!(groww.security_id, UNRESOLVED_SECURITY_ID);
        assert_eq!(groww.exchange_segment, "FNO");
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
