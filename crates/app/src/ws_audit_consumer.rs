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
