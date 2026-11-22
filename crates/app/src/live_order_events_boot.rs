//! The `live_order_events` writer task (plan item LF-6b; table from LF-5).
//!
//! Started when `[live_fill] enabled` is set, even while no strategy engine
//! exists, so spill files a previous process left behind still drain.
//!
//! Order of work (the LF-5 review's requirement, pinned by a source scan
//! below): the table's ensure (CREATE + columns + DEDUP key) is awaited and
//! retried until it answers keyed, then the audit disk tier's drain is
//! started for this table, and only then is the ILP writer built. Rows wait
//! in the bounded channel meanwhile; a full channel drops at the sender
//! (counted there).
//!
//! The writer appends each row (O(columns)) and flushes once a second or at
//! 256 pending rows; a failed flush goes to the audit disk tier inside the
//! writer. Cold path: a few rows per paper order, never per tick.

use std::time::Duration;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::audit_spill::{AuditSpillTable, spawn_audit_spill_drain_once};
use tickvault_storage::live_order_events_persistence::{
    LiveOrderEvent, LiveOrderEventsWriter, ensure_live_order_events_table,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Rows that trigger a flush before the 1 s timer.
pub const LIVE_ORDER_EVENTS_FLUSH_ROWS: usize = 256;
/// The timed flush cadence, seconds.
const FLUSH_INTERVAL_SECS: u64 = 1;
/// Longest wait between two ensure attempts.
const ENSURE_RETRY_MAX_SECS: u64 = 60;

/// The next ensure retry delay: 2 s doubling, capped at 60 s. Pure, O(1).
#[must_use]
pub fn ensure_retry_secs(attempt: u32) -> u64 {
    2u64.saturating_mul(1u64 << attempt.min(6))
        .min(ENSURE_RETRY_MAX_SECS)
}

/// Spawns the writer task. Cold, once per process.
// TEST-EXEMPT: tokio spawn wrapper; the order of work is pinned by test_spawn_live_order_events_writer_ensures_and_drains_before_it_builds_the_writer and the retry delay by test_ensure_retry_secs_doubles_to_the_cap.
pub fn spawn_live_order_events_writer(
    questdb: QuestDbConfig,
    rx: mpsc::Receiver<LiveOrderEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_live_order_events_writer(questdb, rx))
}

async fn run_live_order_events_writer(
    questdb: QuestDbConfig,
    mut rx: mpsc::Receiver<LiveOrderEvent>,
) {
    let mut attempt: u32 = 0;
    while !ensure_live_order_events_table(&questdb).await {
        let wait = ensure_retry_secs(attempt);
        attempt = attempt.saturating_add(1);
        warn!(
            attempt,
            retry_in_secs = wait,
            "live_order_events: table not keyed yet; rows wait in the channel"
        );
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
    let _drain_spawned = spawn_audit_spill_drain_once(AuditSpillTable::LiveOrderEvents, &questdb);
    let mut writer = LiveOrderEventsWriter::new(&questdb);
    info!("live_order_events: writer running");
    let mut tick = tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            row = rx.recv() => {
                let Some(row) = row else {
                    flush(&mut writer);
                    info!("live_order_events: channel closed; writer stopped");
                    return;
                };
                if let Err(err) = writer.append(&row) {
                    error!(
                        code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                        stage = "append",
                        signal_id = row.signal_id,
                        event = row.event.as_str(),
                        ?err,
                        "AUDIT-06: live_order_events row not appended (paper chain only)"
                    );
                }
                if writer.pending() >= LIVE_ORDER_EVENTS_FLUSH_ROWS {
                    flush(&mut writer);
                }
            }
            _ = tick.tick() => flush(&mut writer),
        }
    }
}

/// One synchronous flush, off the async worker on a multi-thread runtime.
fn flush(writer: &mut LiveOrderEventsWriter) {
    let mut run = || writer.flush();
    let result = if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(run)
    } else {
        run()
    };
    if let Err(err) = result {
        error!(
            code = ErrorCode::Audit06OrderWriteFailed.code_str(),
            stage = "flush",
            ?err,
            "AUDIT-06: live_order_events flush failed and the disk tier refused the rows"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_retry_secs_doubles_to_the_cap() {
        assert_eq!(ensure_retry_secs(0), 2);
        assert_eq!(ensure_retry_secs(1), 4);
        assert_eq!(ensure_retry_secs(4), 32);
        assert_eq!(ensure_retry_secs(5), 60);
        assert_eq!(ensure_retry_secs(u32::MAX), 60);
    }

    /// LF-5 review: the ensure and the disk-tier drain run BEFORE the writer
    /// is built, in this file. Fails the build if either call goes missing or
    /// moves after `LiveOrderEventsWriter::new`.
    #[test]
    fn test_spawn_live_order_events_writer_ensures_and_drains_before_it_builds_the_writer() {
        let src = include_str!("live_order_events_boot.rs");
        let body_start = src
            .find("async fn run_live_order_events_writer(")
            .expect("writer fn");
        let body = &src[body_start..];
        let ensure = body
            .find("ensure_live_order_events_table(&questdb).await")
            .expect("ensure awaited");
        let drain = body
            .find("spawn_audit_spill_drain_once(AuditSpillTable::LiveOrderEvents")
            .expect("disk-tier drain started");
        let new = body
            .find("LiveOrderEventsWriter::new(")
            .expect("writer built");
        assert!(
            ensure < drain && drain < new,
            "ensure, then drain, then the writer"
        );
    }

    #[tokio::test]
    async fn test_flush_on_a_current_thread_runtime_does_not_panic() {
        let mut w = LiveOrderEventsWriter::for_test();
        flush(&mut w);
        assert_eq!(w.pending(), 0);
    }
}
