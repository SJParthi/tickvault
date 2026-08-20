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

    let rows_tx = spawn_ws_event_audit_consumer(questdb_cfg);
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<WsLifecycleEvent>(WS_EVENT_AUDIT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let now_ist_nanos = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
            if rows_tx
                .try_send(lifecycle_row(event, now_ist_nanos))
                .is_err()
            {
                metrics::counter!(
                    "tv_ws_event_audit_dropped_total",
                    "reason" => "live_feed_forward"
                )
                .increment(1);
            }
        }
    });
    tx
}

/// Widens one socket lifecycle event into the audit row, at a given instant.
///
/// Split out of the spawn wrapper so the mapping can be asserted without a
/// tokio runtime or a live QuestDB — this is where every decision that could
/// be silently wrong actually lives.
fn lifecycle_row(
    event: tickvault_core::websocket::pool_supervisor::WsLifecycleEvent,
    now_ist_nanos: i64,
) -> tickvault_common::ws_event_types::WsEventAuditRow {
    use tickvault_common::ws_event_types::{WsEventAuditRow, WsType};
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
        ws_type: match event.endpoint {
            DhanEndpointType::Depth20 => WsType::Depth20,
            DhanEndpointType::Depth200 => WsType::Depth200,
            _ => WsType::MainFeed,
        },
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
        dhan_code: tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE,
        down_secs: 0,
        attempts: 0,
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
        if let Err(err) = writer.flush() {
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
                },
                1_700_000_000_000_000_000,
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
            },
            late,
        );
        assert_eq!(row.event_ts_ist_nanos, late);
        assert_eq!(
            row.trading_date_ist_nanos, midnight,
            "the LAST nanosecond of a day must still belong to that day"
        );
    }
}
