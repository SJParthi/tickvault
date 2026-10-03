//! Shared `ws_event_audit` channel + consumer helper.
//!
//! **Relocated from the `main.rs` binary in Phase C1 of the 2026-07-13 Dhan
//! live-WS retirement** so the LIB-side `dhan_rest_stack` (which now owns the
//! functional-dormant order-update WS per the operator's Q4-i ruling) can
//! create its own audit consumer with the exact machinery the main-feed pool
//! and the legacy order-update spawn sites use. Pure move — zero behavior
//! change; `main.rs` re-imports [`spawn_ws_event_audit_consumer`].
//!
//! Self-contained by design: each call owns one consumer writing to the
//! shared `ws_event_audit` table (ILP appends are independent), so every
//! WebSocket producer reuses this exact pattern with no boot refactor.

use tickvault_core::websocket::audit_drop_latch::DropLatch;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{error, info};

/// Bounded capacity for the WS-event audit channel. WS lifecycle events are
/// rare (a few per connection per day), so a small bound is ample; the producer
/// `try_send`s and drops on the (practically unreachable) full case rather than
/// ever blocking the WS read loop.
const WS_EVENT_AUDIT_CHANNEL_CAPACITY: usize = 1024;

/// Creates the WS-event audit channel + spawns its consumer task, returning the
/// `Sender` to hand to a WebSocket producer (the main-feed pool, the
/// order-update connection — legacy main.rs sites OR the `dhan_rest_stack`
/// rewire site). Self-contained: each call owns one consumer writing to the
/// shared `ws_event_audit` table (ILP appends are independent).
#[must_use]
pub fn spawn_ws_event_audit_consumer(
    questdb_cfg: tickvault_common::config::QuestDbConfig,
) -> tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow> {
    let (tx, rx) = tokio::sync::mpsc::channel::<tickvault_common::ws_event_types::WsEventAuditRow>(
        WS_EVENT_AUDIT_CHANNEL_CAPACITY,
    );
    tokio::spawn(async move {
        run_ws_event_audit_consumer(rx, questdb_cfg).await;
    });
    tx
}

/// Creates the LIVE-FEED socket lifecycle channel and spawns a forwarder that
/// stamps each event and widens it into a [`WsEventAuditRow`].
///
/// # Why the stamping happens HERE and not at the socket
///
/// `pool_supervisor.rs` is under a blanket ban on wall-clock reads
/// (`test_pool_supervisor_source_never_reads_the_wall_clock`): the ladder,
/// token expiry and backoff are all monotonic so an NTP step cannot expire all
/// sixteen sockets at once. An audit timestamp is not special enough to earn a
/// carve-out in a guard whose value is that it has none. So the socket reports
/// WHAT happened — allocation-free, every field `Copy` — and this task, which
/// already owns the IST convention, records WHEN.
///
/// [`WsEventAuditRow`]: tickvault_common::ws_event_types::WsEventAuditRow
#[must_use]
// TEST-EXEMPT: spawn wrapper — needs a tokio runtime and a live QuestDB to reach. Its LOGIC is `lifecycle_row`, which is tested directly below.
pub fn spawn_live_feed_lifecycle_audit(
    questdb_cfg: tickvault_common::config::QuestDbConfig,
) -> tokio::sync::mpsc::Sender<tickvault_core::websocket::pool_supervisor::WsLifecycleEvent> {
    use tickvault_core::websocket::pool_supervisor::WsLifecycleEvent;

    let rows_tx = spawn_ws_event_audit_consumer(questdb_cfg.clone());
    let gap_tx = spawn_feed_gap_audit_consumer(questdb_cfg);
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<WsLifecycleEvent>(WS_EVENT_AUDIT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        // One latch per forwarder: a drop EPISODE is a property of this
        // channel, and the page for it is said once per episode (see
        // `record_forward_drop`).
        let latch = DropLatch::new();
        // 2026-10-02: the open gap per connection slot, so the reconnect row
        // carries `down_secs` and every gap becomes one `feed_gap_audit` row.
        // Fixed 32 slots, O(1) per event, allocation-free.
        let mut gaps = GapTracker::new();
        let mut stop_poll = tokio::time::interval(GAP_STOP_POLL_INTERVAL);
        let mut stop_seen = false;
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    let now_ist_nanos = now_ist_nanos();
                    let Some(event) = maybe else {
                        // Every socket is gone: a gap still open is written
                        // closed at this instant rather than lost.
                        gaps.close_all(now_ist_nanos, |row| forward_gap_row(&gap_tx, row));
                        break;
                    };
                    let step = gaps.on_event(&event, now_ist_nanos);
                    if let Some(row) = step.closed {
                        forward_gap_row(&gap_tx, row);
                    }
                    if is_shutdown_close(&event) {
                        // Shutdown is process-wide: every gap still open
                        // (a socket parked for the session, or one mid-
                        // backoff) ends at this instant.
                        gaps.close_all(now_ist_nanos, |row| forward_gap_row(&gap_tx, row));
                    }
                    match rows_tx.try_send(lifecycle_row(event, now_ist_nanos, step)) {
                        Ok(()) => {
                            let _ = record_forward_ok(&latch, &event);
                        }
                        Err(TrySendError::Full(_)) => {
                            let _ = record_forward_drop(&latch, &event, "full");
                        }
                        Err(TrySendError::Closed(_)) => {
                            let _ = record_forward_drop(&latch, &event, "closed");
                        }
                    }
                }
                _ = stop_poll.tick(), if !stop_seen => {
                    // A process stop with every socket already parked sends
                    // no further lifecycle event, so the stop itself closes
                    // the open gaps.
                    if tickvault_core::websocket::pool_supervisor::SOCKET_STOP.is_requested() {
                        stop_seen = true;
                        gaps.close_all(now_ist_nanos(), |row| forward_gap_row(&gap_tx, row));
                    }
                }
            }
        }
    });
    tx
}

/// How often the forwarder checks whether the process asked its sockets to
/// stop (one atomic load per tick).
const GAP_STOP_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(GAP_STOP_POLL_SECS);

/// [`GAP_STOP_POLL_INTERVAL`] in whole seconds.
const GAP_STOP_POLL_SECS: u64 = 1;

/// Connection slots tracked for open gaps: one per global connection index,
/// the same 32 the supervisor's per-slot registers use.
const GAP_SLOTS: usize = tickvault_core::websocket::pool_supervisor::GHOST_REDIAL_SLOTS;

/// The wall clock as IST epoch nanos (the audit tables' convention).
fn now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_default()
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// Is this the close a socket makes when the process stops it?
fn is_shutdown_close(event: &tickvault_core::websocket::pool_supervisor::WsLifecycleEvent) -> bool {
    event.kind == tickvault_common::ws_event_types::WsEventKind::Disconnected
        && event.reason == tickvault_core::websocket::pool_supervisor::ParkReason::Shutdown.as_str()
}

/// The pool an endpoint's socket belongs to, as the audit tables name it.
fn ws_type_of(
    endpoint: tickvault_core::websocket::pool_budget::DhanEndpointType,
) -> tickvault_common::ws_event_types::WsType {
    use tickvault_common::ws_event_types::WsType;
    use tickvault_core::websocket::pool_budget::DhanEndpointType;
    match endpoint {
        DhanEndpointType::Depth20 => WsType::Depth20,
        DhanEndpointType::Depth200 => WsType::Depth200,
        _ => WsType::MainFeed,
    }
}

/// One connection slot's open gap: opened by its FIRST disconnect after a
/// connect, closed by the next connect. `Copy`, so the tracker allocates
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenGap {
    start_ist_nanos: i64,
    endpoint: tickvault_core::websocket::pool_budget::DhanEndpointType,
    reason: &'static str,
    dhan_code: Option<u16>,
    instruments_held: u32,
    attempts: u32,
}

/// What one lifecycle event contributes to the reconnect record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GapStep {
    /// The closed gap's length, on the CONNECTED row that closed it; else 0.
    down_secs: i64,
    /// Failed dials inside the closed gap; else 0.
    attempts: i64,
    /// The `feed_gap_audit` row, when this event closed a gap.
    closed: Option<tickvault_storage::feed_gap_audit_persistence::FeedGapRow>,
}

/// The open gap per connection slot, in a fixed array indexed by the global
/// connection index: O(1) per event, no allocation, no map.
#[derive(Debug)]
struct GapTracker {
    open: [Option<OpenGap>; GAP_SLOTS],
}

impl GapTracker {
    const fn new() -> Self {
        Self {
            open: [None; GAP_SLOTS],
        }
    }

    /// Feeds one lifecycle event. O(1).
    ///
    /// - A disconnect opens the slot's gap, unless one is already open: the
    ///   FIRST close since the last connect is when the socket went dark (a
    ///   socket parked for 805 and later released emits a second close row
    ///   for the release, which must not move the start).
    /// - A failed dial counts an attempt on the open gap.
    /// - A connect (subscribe acked) closes the gap and returns its row.
    /// - The close a socket makes at shutdown opens nothing; the caller ends
    ///   every open gap at that instant through [`GapTracker::close_all`].
    fn on_event(
        &mut self,
        event: &tickvault_core::websocket::pool_supervisor::WsLifecycleEvent,
        now_ist_nanos: i64,
    ) -> GapStep {
        use tickvault_common::ws_event_types::WsEventKind;
        let Some(slot) = self.open.get_mut(usize::from(event.connection_index)) else {
            return GapStep::default();
        };
        match event.kind {
            WsEventKind::Disconnected | WsEventKind::DisconnectedOffHours => {
                if slot.is_none() && !is_shutdown_close(event) {
                    *slot = Some(OpenGap {
                        start_ist_nanos: now_ist_nanos,
                        endpoint: event.endpoint,
                        reason: event.reason,
                        dhan_code: event.dhan_code,
                        instruments_held: event.instruments_held,
                        attempts: 0,
                    });
                }
                GapStep::default()
            }
            WsEventKind::DialFailed => {
                if let Some(gap) = slot.as_mut() {
                    gap.attempts = gap.attempts.saturating_add(1);
                }
                GapStep::default()
            }
            WsEventKind::Connected => match slot.take() {
                Some(gap) => {
                    let row = gap_row(event.connection_index, gap, now_ist_nanos, false);
                    GapStep {
                        down_secs: row.down_secs(),
                        attempts: row.attempts,
                        closed: Some(row),
                    }
                }
                None => GapStep::default(),
            },
            _ => GapStep::default(),
        }
    }

    /// Ends every open gap at `now_ist_nanos` (shutdown), handing each row to
    /// `emit`. O(`GAP_SLOTS`) = 32, once per stop. Returns how many it closed.
    fn close_all(
        &mut self,
        now_ist_nanos: i64,
        mut emit: impl FnMut(tickvault_storage::feed_gap_audit_persistence::FeedGapRow),
    ) -> usize {
        let mut closed = 0_usize;
        // O(1) EXEMPT: begin — fixed 32-slot sweep, once per process stop
        for (index, slot) in self.open.iter_mut().enumerate() {
            if let Some(gap) = slot.take() {
                let connection_index = u8::try_from(index).unwrap_or(u8::MAX);
                emit(gap_row(connection_index, gap, now_ist_nanos, true));
                closed = closed.saturating_add(1);
            }
        }
        // O(1) EXEMPT: end
        closed
    }
}

/// Builds the `feed_gap_audit` row for a closed gap.
fn gap_row(
    connection_index: u8,
    gap: OpenGap,
    end_ist_nanos: i64,
    open_at_shutdown: bool,
) -> tickvault_storage::feed_gap_audit_persistence::FeedGapRow {
    tickvault_storage::feed_gap_audit_persistence::FeedGapRow {
        gap_start_ist_nanos: gap.start_ist_nanos,
        gap_end_ist_nanos: end_ist_nanos,
        feed: tickvault_common::feed::Feed::Dhan,
        ws_type: ws_type_of(gap.endpoint),
        connection_index: i64::from(connection_index),
        reason: gap.reason,
        dhan_code: gap.dhan_code.map_or(
            tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE,
            i64::from,
        ),
        instruments_held: i64::from(gap.instruments_held),
        attempts: i64::from(gap.attempts),
        open_at_shutdown,
    }
}

/// Bounded capacity of the gap-row channel. Gaps are rarer than lifecycle
/// events, so the lifecycle bound covers them.
const FEED_GAP_CHANNEL_CAPACITY: usize = WS_EVENT_AUDIT_CHANNEL_CAPACITY;

/// Hands one gap row to its writer. `try_send`, never blocking the forwarder;
/// a refused row is counted and logged (gap rows are a few a day, so one line
/// per refusal cannot storm).
fn forward_gap_row(
    tx: &tokio::sync::mpsc::Sender<tickvault_storage::feed_gap_audit_persistence::FeedGapRow>,
    row: tickvault_storage::feed_gap_audit_persistence::FeedGapRow,
) {
    if let Err(err) = tx.try_send(row) {
        let reason = match err {
            TrySendError::Full(_) => "full",
            TrySendError::Closed(_) => "closed",
        };
        metrics::counter!("tv_feed_gap_audit_dropped_total", "reason" => reason).increment(1);
        error!(
            code = tickvault_common::error_code::ErrorCode::HotPath02WriterQueueDrop.code_str(),
            source = "feed_gap_audit_row_dropped",
            reason,
            ws_type = row.ws_type.as_str(),
            connection_index = row.connection_index,
            down_secs = row.down_secs(),
            "feed_gap_audit: a reconnect-gap row was DROPPED before it reached the writer \
             ({reason}); the gap is still visible as its two ws_event_audit rows"
        );
    }
}

/// Creates the gap-row channel and spawns its writer task, which ensures the
/// table at start (the same boot self-heal `ws_event_audit` gets).
fn spawn_feed_gap_audit_consumer(
    questdb_cfg: tickvault_common::config::QuestDbConfig,
) -> tokio::sync::mpsc::Sender<tickvault_storage::feed_gap_audit_persistence::FeedGapRow> {
    let (tx, rx) = tokio::sync::mpsc::channel::<
        tickvault_storage::feed_gap_audit_persistence::FeedGapRow,
    >(FEED_GAP_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        run_feed_gap_audit_consumer(rx, questdb_cfg).await;
    });
    tx
}

/// Drains gap rows into `feed_gap_audit`: ensure the table once, then append
/// and flush each row. A failure is AUDIT-WS-01 (the same forensic class as
/// `ws_event_audit`), never a recovery-path failure.
async fn run_feed_gap_audit_consumer(
    mut rx: tokio::sync::mpsc::Receiver<tickvault_storage::feed_gap_audit_persistence::FeedGapRow>,
    questdb_cfg: tickvault_common::config::QuestDbConfig,
) {
    use tickvault_common::error_code::ErrorCode;
    use tickvault_storage::feed_gap_audit_persistence::{
        FeedGapAuditWriter, ensure_feed_gap_audit_table,
    };

    ensure_feed_gap_audit_table(&questdb_cfg).await;
    let mut writer = FeedGapAuditWriter::new(&questdb_cfg);
    while let Some(row) = rx.recv().await {
        if let Err(err) = writer.append_row(&row) {
            error!(
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                ws_type = row.ws_type.as_str(),
                connection_index = row.connection_index,
                ?err,
                "feed_gap_audit: append failed"
            );
            metrics::counter!("tv_feed_gap_audit_write_errors_total", "stage" => "append")
                .increment(1);
            continue;
        }
        // A blocking ILP-over-HTTP round trip (the questdb-rs default retry
        // loop applies, so it can take seconds against a stalled database):
        // run it off the shared tokio worker (O(1) sweep, 2026-10-03).
        if let Err(err) = crate::order_observability::blocking_flush(|| writer.flush()) {
            error!(
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                ws_type = row.ws_type.as_str(),
                connection_index = row.connection_index,
                ?err,
                "feed_gap_audit: flush failed"
            );
            metrics::counter!("tv_feed_gap_audit_write_errors_total", "stage" => "flush")
                .increment(1);
        } else {
            metrics::counter!("tv_feed_gap_audit_rows_total", "ws_type" => row.ws_type.as_str())
                .increment(1);
        }
    }
    info!("feed_gap_audit consumer: all producers dropped — exiting");
}

/// A socket-lifecycle audit row could NOT be handed to the consumer.
///
/// Counted EVERY time (the counter is the total), logged ONCE per episode
/// (the latch's rising edge). Returns whether this call was the one that
/// logged, so a test can assert the edge without a log subscriber.
///
/// # Why this is `error!` and not the bare counter it replaced (2026-09-02)
///
/// Before this, the forwarder's drop arm was `metrics::counter!` and nothing
/// else — a counter that is not EMF-selected and sits on no dashboard. A
/// wedged consumer therefore cost every socket-lifecycle row of a session
/// with no operator-visible trace, which is the false-OK class `ws_event_audit`
/// was created to prevent: the table's only job is to answer "which pool went
/// dark and when?", and the one time it is most needed is exactly when the
/// consumer behind it is stalled. The order-update socket's own drop arm has
/// been `error!` since 2026-07-05; this path never inherited that.
///
/// HOT-PATH-02 is the persistence layer's existing filtered loss code (a
/// `writer queue drop` is what this is); the `source` field scopes it, so no
/// new metric name and no new alarm.
fn record_forward_drop(
    latch: &DropLatch,
    event: &tickvault_core::websocket::pool_supervisor::WsLifecycleEvent,
    reason: &'static str,
) -> bool {
    metrics::counter!(
        "tv_ws_event_audit_dropped_total",
        "reason" => "live_feed_forward"
    )
    .increment(1);
    let first_of_episode = latch.on_drop();
    if first_of_episode {
        error!(
            code = tickvault_common::error_code::ErrorCode::HotPath02WriterQueueDrop.code_str(),
            source = "ws_event_audit_row_dropped",
            reason,
            endpoint = event.endpoint.as_str(),
            connection_index = event.connection_index,
            event_kind = event.kind.as_str(),
            event_reason = event.reason,
            "ws_event_audit: a socket-lifecycle row was DROPPED before it reached the \
             writer ({reason}) — the audit table is now MISSING this socket's history \
             and will keep missing rows until the consumer drains. Counted on every \
             drop, paged once per episode; a recovery line follows when a row lands"
        );
    }
    first_of_episode
}

/// A socket-lifecycle audit row reached the consumer. Closes an open drop
/// episode (one `info!` on the falling edge); routine otherwise. Returns
/// whether this call closed an episode.
fn record_forward_ok(
    latch: &DropLatch,
    event: &tickvault_core::websocket::pool_supervisor::WsLifecycleEvent,
) -> bool {
    let recovered = latch.on_ok();
    if recovered {
        info!(
            endpoint = event.endpoint.as_str(),
            connection_index = event.connection_index,
            event_kind = event.kind.as_str(),
            "ws_event_audit: socket-lifecycle rows are reaching the writer again — the \
             drop episode is over (rows dropped during it are gone; the count is in \
             tv_ws_event_audit_dropped_total)"
        );
    }
    recovered
}

/// Widens one socket lifecycle event into the audit row, at a given instant.
///
/// Split out of the spawn wrapper so the mapping can be asserted without a
/// tokio runtime or a live QuestDB — this is where every decision that could
/// be silently wrong actually lives.
fn lifecycle_row(
    event: tickvault_core::websocket::pool_supervisor::WsLifecycleEvent,
    now_ist_nanos: i64,
    step: GapStep,
) -> tickvault_common::ws_event_types::WsEventAuditRow {
    use tickvault_common::ws_event_types::WsEventAuditRow;
    use tickvault_core::websocket::pool_budget::DhanEndpointType;

    let nanos_per_day: i64 = 86_400 * 1_000_000_000;
    WsEventAuditRow {
        event_ts_ist_nanos: now_ist_nanos,
        trading_date_ist_nanos: now_ist_nanos - now_ist_nanos.rem_euclid(nanos_per_day),
        feed: tickvault_common::feed::Feed::Dhan,
        // Derived from the ENDPOINT. The sink's own `ws_type` is the WAL
        // discriminant and reads `LiveFeed` for all fifteen market-data
        // sockets, so building the row from it would file a depth-200 park
        // under the main feed — and "which pool went dark?" is the only
        // question this table exists to answer.
        ws_type: ws_type_of(event.endpoint),
        connection_index: i64::from(event.connection_index),
        // The authorized per-endpoint cap, not a live count: this row records
        // ONE socket's event and must not imply a fleet-wide reading it
        // cannot have.
        pool_size: match event.endpoint {
            DhanEndpointType::Depth20 => {
                i64::try_from(tickvault_common::constants::MAX_TWENTY_DEPTH_CONNECTIONS)
                    .unwrap_or(i64::MAX)
            }
            DhanEndpointType::Depth200 => {
                i64::try_from(tickvault_common::constants::MAX_TWO_HUNDRED_DEPTH_CONNECTIONS)
                    .unwrap_or(i64::MAX)
            }
            _ => i64::try_from(tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS)
                .unwrap_or(i64::MAX),
        },
        event_kind: event.kind,
        source: event.endpoint.as_str().to_string(),
        reason: event.reason.to_string(),
        // The Dhan close code when the socket was closed with one; -1 otherwise.
        dhan_code: event.dhan_code.map_or(
            tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE,
            i64::from,
        ),
        // On the CONNECTED row that ends a gap: how long the slot was dark and
        // how many dials failed inside it. 0 on every other row.
        down_secs: step.down_secs,
        attempts: step.attempts,
        market_hours: tickvault_common::market_hours::is_within_market_hours_ist(),
    }
}

/// Drains the WS-event audit channel into the `ws_event_audit` QuestDB table.
///
/// Owns the ILP writer for the table's lifetime, ensures the table exists once
/// at start, then appends + flushes each row as it arrives. A flush failure
/// emits AUDIT-WS-01 (Medium) — the WS events still reached CloudWatch logs +
/// Telegram, so this is a forensic-record gap, never a recovery-path failure.
/// Exits cleanly when all producers (the pool connections) drop their senders.
async fn run_ws_event_audit_consumer(
    mut rx: tokio::sync::mpsc::Receiver<tickvault_common::ws_event_types::WsEventAuditRow>,
    questdb_cfg: tickvault_common::config::QuestDbConfig,
) {
    use tickvault_common::error_code::ErrorCode;
    use tickvault_storage::ws_event_audit_persistence::{
        WsEventAuditWriter, ensure_ws_event_audit_table,
    };

    ensure_ws_event_audit_table(&questdb_cfg).await;
    let mut writer = WsEventAuditWriter::new(&questdb_cfg);
    while let Some(row) = rx.recv().await {
        if let Err(err) = writer.append_row(&row) {
            error!(
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                ws_type = row.ws_type.as_str(),
                connection_index = row.connection_index,
                event_kind = row.event_kind.as_str(),
                ?err,
                "ws_event_audit: append failed"
            );
            metrics::counter!("tv_ws_event_audit_write_errors_total", "stage" => "append")
                .increment(1);
            continue;
        }
        // A blocking ILP-over-HTTP round trip (the questdb-rs default retry
        // loop applies, so it can take seconds against a stalled database):
        // run it off the shared tokio worker (O(1) sweep, 2026-10-03).
        if let Err(err) = crate::order_observability::blocking_flush(|| writer.flush()) {
            error!(
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                ws_type = row.ws_type.as_str(),
                connection_index = row.connection_index,
                event_kind = row.event_kind.as_str(),
                ?err,
                "ws_event_audit: flush failed"
            );
            metrics::counter!("tv_ws_event_audit_write_errors_total", "stage" => "flush")
                .increment(1);
        } else {
            metrics::counter!(
                "tv_ws_event_audit_rows_total",
                "ws_type" => row.ws_type.as_str(),
                "event_kind" => row.event_kind.as_str(),
            )
            .increment(1);
        }
    }
    info!("ws_event_audit consumer: all producers dropped — exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A QuestDB config pointing at a port nothing listens on.
    ///
    /// The consumer's DB work is deliberately NOT exercised here — see the
    /// honest-scope note on the spawn test below.
    fn unreachable_questdb() -> tickvault_common::config::QuestDbConfig {
        tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    /// The bound is what keeps a stalled writer off the WS read loop.
    ///
    /// The producer `try_send`s and drops on full rather than ever blocking.
    /// That contract only holds if the channel is BOUNDED — an unbounded
    /// channel would swap a dropped audit row for unbounded memory growth on
    /// the socket task, which is the worse of the two failures.
    #[test]
    fn test_ws_event_audit_channel_is_bounded_at_the_documented_capacity() {
        assert_eq!(
            WS_EVENT_AUDIT_CHANNEL_CAPACITY, 1024,
            "the audit channel bound is load-bearing: the producer try_sends \
             and drops on full instead of blocking the WS read loop"
        );
    }

    /// The returned Sender must carry that bound, not merely be *a* channel.
    ///
    /// Pins the wiring, not the constant: a refactor that built the channel
    /// with a different capacity — or with `unbounded_channel` — would leave
    /// the constant above untouched and still break the no-block contract.
    ///
    /// **Honest scope:** this exercises the channel the spawn returns, NOT the
    /// consumer's QuestDB path. `run_ws_event_audit_consumer` calls
    /// `ensure_ws_event_audit_table` first, which needs a live QuestDB; with
    /// none listening the task fails its DB work and the rows are never
    /// written. That is why this asserts on capacity and send-acceptance
    /// rather than on rows landing — a test claiming the latter without a
    /// database would be the false-OK this repo bans.
    /// O(1) sweep 2026-10-03: both audit consumers flush through
    /// `blocking_flush`, never bare on the shared tokio worker.
    #[test]
    fn test_audit_consumers_never_flush_bare_on_the_worker() {
        let src = include_str!("ws_audit_consumer.rs");
        let production = src.split(concat!("#[cfg(", "test)]")).next().unwrap_or(src);
        let wrapped = production
            .matches("blocking_flush(|| writer.flush())")
            .count();
        let all = production.matches("writer.flush()").count();
        assert_eq!(wrapped, 2, "both consumers must flush off the worker");
        assert_eq!(
            all, wrapped,
            "a bare writer.flush() sits on the tokio worker"
        );
    }

    #[tokio::test]
    async fn test_spawn_returns_a_sender_with_the_bounded_capacity() {
        let tx = spawn_ws_event_audit_consumer(unreachable_questdb());
        assert_eq!(
            tx.capacity(),
            WS_EVENT_AUDIT_CHANNEL_CAPACITY,
            "the spawned channel must carry the documented bound — a channel \
             built with a different capacity, or an unbounded one, breaks the \
             producer's try_send-and-drop contract while leaving the constant \
             above green"
        );
        assert_eq!(
            tx.max_capacity(),
            WS_EVENT_AUDIT_CHANNEL_CAPACITY,
            "max_capacity is the bound itself and must not drift from the \
             constant"
        );
    }

    /// A producer must never be blocked by this channel while it has room.
    ///
    /// `try_send` is the exact call the WS read loop makes. If it ever returns
    /// `Full` below the bound, the loop starts dropping lifecycle events while
    /// capacity remains — the failure mode the bound was chosen to avoid.
    #[tokio::test]
    async fn test_try_send_accepts_rows_up_to_the_bound_without_blocking() {
        use tickvault_common::ws_event_types::{WsEventAuditRow, WsEventKind, WsType};

        // No consumer: the receiver is held here, so nothing drains and the
        // buffer fills deterministically. This isolates the CHANNEL's
        // behaviour from the consumer's timing, which a spawned drain would
        // make racy.
        let (tx, _rx) =
            tokio::sync::mpsc::channel::<WsEventAuditRow>(WS_EVENT_AUDIT_CHANNEL_CAPACITY);

        let row = WsEventAuditRow {
            event_ts_ist_nanos: 0,
            trading_date_ist_nanos: 0,
            feed: tickvault_common::feed::Feed::Dhan,
            ws_type: WsType::OrderUpdate,
            connection_index: 0,
            pool_size: 1,
            event_kind: WsEventKind::Connected,
            source: String::new(),
            reason: String::new(),
            dhan_code: 0,
            down_secs: 0,
            attempts: 0,
            market_hours: false,
        };

        for i in 0..WS_EVENT_AUDIT_CHANNEL_CAPACITY {
            assert!(
                tx.try_send(row.clone()).is_ok(),
                "try_send must succeed at index {i} — the channel still has \
                 room, and a producer refused below the bound would drop \
                 lifecycle events for no reason"
            );
        }

        // At the bound the producer is REFUSED, not blocked. That refusal is
        // the designed behaviour: a dropped audit row costs forensic depth,
        // whereas blocking here would stall the WebSocket read loop itself.
        assert!(
            tx.try_send(row).is_err(),
            "at capacity try_send must return Full so the caller drops the \
             row — never block the WS read loop"
        );
    }
}

#[cfg(test)]
mod lifecycle_row_tests {
    use super::lifecycle_row;
    use tickvault_common::ws_event_types::{WsEventKind, WsType};
    use tickvault_core::websocket::pool_budget::DhanEndpointType;
    use tickvault_core::websocket::pool_supervisor::WsLifecycleEvent;

    #[test]
    fn the_row_names_the_endpoints_own_pool_not_the_wal_discriminant() {
        // The load-bearing mapping. Every one of the fifteen market-data
        // sockets carries the SAME `ws_type` on its sink — `LiveFeed`, the WAL
        // record discriminant — so a row built from that field would file a
        // depth-200 park under the main feed, and "which pool went dark?" is
        // the only question this table exists to answer. The endpoint travels
        // on the event precisely so that mistake is unavailable here.
        for (endpoint, ws_type, pool) in [
            (DhanEndpointType::MainFeed, WsType::MainFeed, 5_i64),
            (DhanEndpointType::Depth20, WsType::Depth20, 5),
            (DhanEndpointType::Depth200, WsType::Depth200, 5),
        ] {
            let row = lifecycle_row(
                WsLifecycleEvent {
                    endpoint,
                    connection_index: 4,
                    kind: WsEventKind::Disconnected,
                    reason: "park_fatal",
                    dhan_code: None,
                    instruments_held: 0,
                },
                1_700_000_000_000_000_000,
                super::GapStep::default(),
            );
            assert_eq!(row.ws_type, ws_type, "endpoint {endpoint:?}");
            assert_eq!(row.pool_size, pool, "the AUTHORIZED cap, not a live count");
            assert_eq!(row.connection_index, 4);
            assert_eq!(row.source, endpoint.as_str());
            assert_eq!(row.reason, "park_fatal");
            assert_eq!(row.event_kind, WsEventKind::Disconnected);
            assert_eq!(row.feed, tickvault_common::feed::Feed::Dhan);
        }
    }

    #[test]
    fn the_trading_date_is_the_ist_midnight_of_the_event_not_of_now() {
        // `trading_date_ist_nanos` is a DEDUP key column. Deriving it from the
        // wall clock at write time rather than from the event's own stamp
        // would file a 23:59 event under the next day whenever the forwarder
        // lagged across midnight — and the archival and retention paths key on
        // exactly this column.
        let nanos_per_day = 86_400_i64 * 1_000_000_000;
        let midnight = 1_700_000_000_000_000_000_i64 - 1_700_000_000_000_000_000 % nanos_per_day;
        let late = midnight + nanos_per_day - 1; // 23:59:59.999999999 IST
        let row = lifecycle_row(
            WsLifecycleEvent {
                endpoint: DhanEndpointType::MainFeed,
                connection_index: 0,
                kind: WsEventKind::Connected,
                reason: "subscribe_acked",
                dhan_code: None,
                instruments_held: 0,
            },
            late,
            super::GapStep::default(),
        );
        assert_eq!(row.event_ts_ist_nanos, late);
        assert_eq!(
            row.trading_date_ist_nanos, midnight,
            "the LAST nanosecond of a day must still belong to that day"
        );
    }

    fn park_event() -> WsLifecycleEvent {
        WsLifecycleEvent {
            endpoint: DhanEndpointType::Depth200,
            connection_index: 2,
            kind: WsEventKind::Disconnected,
            reason: "park_fatal",
            dhan_code: None,
            instruments_held: 0,
        }
    }

    /// 2026-09-02 (audit finding 9): a forwarder drop is LOUD exactly once per
    /// episode. The first drop logs; the next thousand are counted and
    /// silent; a landed row closes the episode; the drop after THAT is a new
    /// episode and logs again. Before this the arm was a bare counter.
    #[test]
    fn a_forward_drop_is_loud_once_per_episode_and_re_arms_on_success() {
        use super::{record_forward_drop, record_forward_ok};
        use tickvault_core::websocket::audit_drop_latch::DropLatch;

        let latch = DropLatch::new();
        let event = park_event();

        assert!(
            !record_forward_ok(&latch, &event),
            "a landed row with no open episode is routine, not a recovery"
        );
        assert!(
            record_forward_drop(&latch, &event, "full"),
            "the FIRST drop of an episode must be the one that logs"
        );
        for _ in 0..1_000 {
            assert!(
                !record_forward_drop(&latch, &event, "full"),
                "later drops in the same episode are counted, never re-paged"
            );
        }
        assert!(
            record_forward_ok(&latch, &event),
            "the first landed row after a drop closes the episode"
        );
        assert!(
            record_forward_drop(&latch, &event, "closed"),
            "after a recovery the next drop is a NEW episode and must log again"
        );
    }
}

/// 2026-10-02: reconnect gaps are durably recorded. A disconnect opens a gap
/// per connection slot, the next connect closes it into one `feed_gap_audit`
/// row and stamps `down_secs` / `attempts` on the `ws_event_audit` CONNECTED
/// row, and a shutdown closes every gap still open at that instant.
#[cfg(test)]
mod feed_gap_tests {
    use super::{GapStep, GapTracker, lifecycle_row};
    use tickvault_common::ws_event_types::{WS_EVENT_NO_DHAN_CODE, WsEventKind, WsType};
    use tickvault_core::websocket::pool_budget::DhanEndpointType;
    use tickvault_core::websocket::pool_supervisor::{ParkReason, WsLifecycleEvent};
    use tickvault_storage::feed_gap_audit_persistence::FeedGapRow;

    const SEC: i64 = 1_000_000_000;
    const T0: i64 = 1_790_000_000 * SEC;

    fn event(
        endpoint: DhanEndpointType,
        slot: u8,
        kind: WsEventKind,
        reason: &'static str,
        dhan_code: Option<u16>,
    ) -> WsLifecycleEvent {
        WsLifecycleEvent {
            endpoint,
            connection_index: slot,
            kind,
            reason,
            dhan_code,
            instruments_held: 4_000,
        }
    }

    fn close_805(slot: u8) -> WsLifecycleEvent {
        event(
            DhanEndpointType::MainFeed,
            slot,
            WsEventKind::Disconnected,
            "park_pool_overflow",
            Some(805),
        )
    }

    fn connected(slot: u8) -> WsLifecycleEvent {
        event(
            DhanEndpointType::MainFeed,
            slot,
            WsEventKind::Connected,
            "subscribe_acked",
            None,
        )
    }

    #[test]
    fn test_gap_down_secs_and_attempts_on_the_reconnect() {
        let mut gaps = GapTracker::new();
        assert_eq!(gaps.on_event(&close_805(3), T0), GapStep::default());
        let failed = event(
            DhanEndpointType::MainFeed,
            3,
            WsEventKind::DialFailed,
            "dial_failed",
            None,
        );
        assert_eq!(gaps.on_event(&failed, T0 + 5 * SEC), GapStep::default());
        assert_eq!(gaps.on_event(&failed, T0 + 9 * SEC), GapStep::default());
        let step = gaps.on_event(&connected(3), T0 + 47 * SEC + SEC / 2);
        assert_eq!(step.down_secs, 47, "whole seconds, rounded down");
        assert_eq!(step.attempts, 2, "two failed dials inside the gap");
        let row = step
            .closed
            .expect("a connect after a disconnect closes the gap");
        assert_eq!(row.gap_start_ist_nanos, T0);
        assert_eq!(row.gap_end_ist_nanos, T0 + 47 * SEC + SEC / 2);
        assert_eq!(row.ws_type, WsType::MainFeed);
        assert_eq!(row.connection_index, 3);
        assert_eq!(row.reason, "park_pool_overflow");
        assert_eq!(row.dhan_code, 805);
        assert_eq!(row.instruments_held, 4_000);
        assert!(!row.open_at_shutdown);
        // The slot is clear again: the next connect writes nothing.
        assert_eq!(
            gaps.on_event(&connected(3), T0 + 60 * SEC),
            GapStep::default()
        );
    }

    #[test]
    fn test_gap_keeps_the_first_disconnect_as_its_start() {
        // A socket parked for 805 and later released by the overflow probe
        // emits a second close row; the gap still starts at the FIRST one.
        let mut gaps = GapTracker::new();
        let _ = gaps.on_event(&close_805(1), T0);
        let bare = event(
            DhanEndpointType::MainFeed,
            1,
            WsEventKind::Disconnected,
            "reconnect_backoff",
            None,
        );
        let _ = gaps.on_event(&bare, T0 + 300 * SEC);
        let row = gaps
            .on_event(&connected(1), T0 + 320 * SEC)
            .closed
            .expect("gap row");
        assert_eq!(row.gap_start_ist_nanos, T0);
        assert_eq!(row.down_secs(), 320);
        assert_eq!(row.dhan_code, 805, "the code of the close that opened it");
    }

    #[test]
    fn test_gap_open_at_shutdown_ends_at_the_shutdown_instant() {
        let mut gaps = GapTracker::new();
        let _ = gaps.on_event(&close_805(0), T0);
        let depth = event(
            DhanEndpointType::Depth200,
            12,
            WsEventKind::Disconnected,
            "reconnect_backoff",
            None,
        );
        let _ = gaps.on_event(&depth, T0 + 10 * SEC);
        // The process stops: the shutdown close itself opens nothing.
        let shutdown_at = T0 + 3_600 * SEC;
        let stop = event(
            DhanEndpointType::MainFeed,
            4,
            WsEventKind::Disconnected,
            ParkReason::Shutdown.as_str(),
            None,
        );
        assert!(super::is_shutdown_close(&stop));
        assert_eq!(gaps.on_event(&stop, shutdown_at), GapStep::default());
        let mut rows: Vec<FeedGapRow> = Vec::new();
        let closed = gaps.close_all(shutdown_at, |row| rows.push(row));
        assert_eq!(closed, 2, "both open gaps, and not the shutdown close");
        for row in &rows {
            assert_eq!(row.gap_end_ist_nanos, shutdown_at);
            assert!(row.open_at_shutdown);
        }
        assert_eq!(rows[0].connection_index, 0);
        assert_eq!(rows[0].down_secs(), 3_600);
        assert_eq!(rows[1].connection_index, 12);
        assert_eq!(rows[1].ws_type, WsType::Depth200);
        assert_eq!(rows[1].dhan_code, WS_EVENT_NO_DHAN_CODE);
        // Nothing left open: a second stop writes nothing.
        assert_eq!(gaps.close_all(shutdown_at + SEC, |_| {}), 0);
    }

    #[test]
    fn test_gap_connect_without_a_gap_and_out_of_range_slot_write_nothing() {
        let mut gaps = GapTracker::new();
        assert_eq!(gaps.on_event(&connected(2), T0), GapStep::default());
        let _ = gaps.on_event(&close_805(200), T0);
        assert_eq!(gaps.on_event(&connected(200), T0 + SEC), GapStep::default());
        assert_eq!(gaps.close_all(T0 + 2 * SEC, |_| {}), 0);
    }

    #[test]
    fn test_ws_event_audit_rows_carry_the_real_code_and_down_secs() {
        let mut gaps = GapTracker::new();
        let close = close_805(5);
        let step = gaps.on_event(&close, T0);
        let close_row = lifecycle_row(close, T0, step);
        assert_eq!(close_row.dhan_code, 805, "no longer hard-coded to -1");
        assert_eq!(close_row.down_secs, 0);

        let reconnect = connected(5);
        let step = gaps.on_event(&reconnect, T0 + 90 * SEC);
        let open_row = lifecycle_row(reconnect, T0 + 90 * SEC, step);
        assert_eq!(open_row.dhan_code, WS_EVENT_NO_DHAN_CODE);
        assert_eq!(open_row.down_secs, 90, "no longer hard-coded to 0");
        assert_eq!(open_row.attempts, 0);
        assert_eq!(open_row.event_kind, WsEventKind::Connected);
    }
}
