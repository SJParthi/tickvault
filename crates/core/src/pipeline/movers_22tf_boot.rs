//! Boot helper for the movers 22-tf pipeline — Phase 10b-2 of v3 plan.
//!
//! Encapsulates the wiring that brings up the full 22-timeframe snapshot
//! pipeline at app startup. Provides one entry point
//! (`build_movers_22tf_pipeline`) that:
//!
//! 1. Constructs the shared `Movers22TfTracker` (papaya-backed).
//! 2. Allocates 22 `tokio::sync::mpsc::channel(8192)` pairs (one per
//!    timeframe).
//! 3. Returns the senders array (boot integration installs into
//!    `init_global_writer_state`) + receivers vec (consumer tasks
//!    take ownership) + the shared tracker (ticker processor + 22
//!    snapshot tasks share via Arc::clone).
//!
//! ## Why a separate boot helper?
//!
//! `crates/app/src/main.rs` is 8K+ lines. A single misplaced edit risks
//! breaking the existing boot sequence. By isolating the wiring math
//! into a testable helper, the main.rs insertion is a single function
//! call that's easy to audit + revert.
//!
//! ## See also
//!
//! - `crates/core/src/pipeline/movers_22tf_writer_state.rs` — the
//!   `OnceLock<[Sender; 22]>` registry.
//! - `crates/core/src/pipeline/movers_22tf_tracker.rs` — the papaya
//!   tracker.
//! - `crates/core/src/pipeline/movers_22tf_scheduler.rs` — the
//!   per-timeframe cadence primitives.

use std::sync::Arc;

use tickvault_common::mover_types::MoverRow;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::pipeline::movers_22tf_scheduler::MOVERS_22TF_MPSC_CAPACITY;
use crate::pipeline::movers_22tf_tracker::Movers22TfTracker;
use crate::pipeline::movers_22tf_writer_state::{MOVERS_22TF_WRITER_COUNT, Movers22TfWriterState};

use tickvault_storage::movers_22tf_writer::Movers22TfWriter;

/// Default flush threshold for consumer tasks — flush after N rows
/// accumulate in the ILP buffer. Picked to balance ILP TCP throughput
/// (larger batches = better) vs latency (smaller batches = quicker
/// dashboard updates). 1024 rows ≈ 256 KB at 256 B/row.
pub const CONSUMER_FLUSH_THRESHOLD: usize = 1024;

/// Output of `build_movers_22tf_pipeline` — everything the boot wiring
/// needs to install in main.rs.
pub struct Movers22TfPipeline {
    /// Shared papaya-backed tracker. Cloned via Arc into:
    /// - tick_processor (single writer; calls `update_security_state`)
    /// - 22 snapshot tasks (concurrent readers; call `snapshot_into`)
    pub tracker: Movers22TfTracker,
    /// Writer-state registry holding the 22 senders. Pass to
    /// `init_global_writer_state(state)` immediately after construction.
    pub writer_state: Movers22TfWriterState,
    /// 22 receivers — boot wiring takes ownership of each, spawning a
    /// per-timeframe consumer task that drains the channel into the
    /// `Movers22TfWriter` ILP buffer.
    pub receivers: Vec<mpsc::Receiver<MoverRow>>,
}

/// Constructs the full movers 22-tf runtime: tracker + 22 channels +
/// writer-state registry. Pure function (no I/O); the caller installs
/// each piece into the running tokio runtime.
///
/// Call exactly once at boot, after `ensure_movers_22tf_tables` has
/// completed but before spawning the 22 consumer tasks + the snapshot
/// scheduler tasks.
#[must_use]
pub fn build_movers_22tf_pipeline() -> Movers22TfPipeline {
    let tracker = Movers22TfTracker::new();

    let mut senders: Vec<mpsc::Sender<MoverRow>> = Vec::with_capacity(MOVERS_22TF_WRITER_COUNT);
    let mut receivers: Vec<mpsc::Receiver<MoverRow>> = Vec::with_capacity(MOVERS_22TF_WRITER_COUNT);
    for _ in 0..MOVERS_22TF_WRITER_COUNT {
        let (tx, rx) = mpsc::channel::<MoverRow>(MOVERS_22TF_MPSC_CAPACITY);
        senders.push(tx);
        receivers.push(rx);
    }

    // Convert Vec<Sender; 22> -> [Sender; 22] for the typed array.
    let senders_array: [mpsc::Sender<MoverRow>; MOVERS_22TF_WRITER_COUNT] =
        senders.try_into().unwrap_or_else(|_| {
            // O(1) EXEMPT: structural invariant — the loop above pushed
            // exactly MOVERS_22TF_WRITER_COUNT entries, so try_into is
            // infallible. The unreachable branch keeps the type system
            // happy without a panic in production.
            unreachable!("MOVERS_22TF_WRITER_COUNT push count is fixed")
        });
    let writer_state = Movers22TfWriterState::new(senders_array);

    Movers22TfPipeline {
        tracker,
        writer_state,
        receivers,
    }
}

/// Returns a clonable handle to the tracker for downstream consumers
/// (tick processor, snapshot tasks, observability metrics emitters).
/// Wraps the tracker in `Arc` for ergonomic sharing.
#[must_use]
pub fn arc_tracker(pipeline: &Movers22TfPipeline) -> Arc<Movers22TfTracker> {
    Arc::new(pipeline.tracker.clone())
}

// ---------------------------------------------------------------------------
// Phase 10b-3a — consumer task spawner
// ---------------------------------------------------------------------------

/// Spawns ONE consumer task that drains the given mpsc receiver into the
/// given `Movers22TfWriter`. Each row is appended to the writer's ILP
/// buffer; the buffer is flushed every `CONSUMER_FLUSH_THRESHOLD` rows.
///
/// The task exits cleanly when the sender side of the channel is
/// dropped (returns `None` from `recv`). Final flush attempts to drain
/// any remaining buffered rows before exit; failures are logged at
/// WARN (the writer's own append/flush already emit MOVERS-22TF-01
/// at ERROR if QuestDB is unreachable).
///
/// Generic over the writer to keep the spawner testable with
/// `MockConsumerWriter` impls in the unit tests.
pub fn spawn_consumer_task<W: ConsumerWriter + Send + 'static>(
    mut writer: W,
    mut receiver: mpsc::Receiver<MoverRow>,
    flush_threshold: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let label = writer.timeframe_label();
        info!(
            timeframe = label,
            flush_threshold, "movers_22tf consumer task starting"
        );
        let mut total_appended: u64 = 0;
        let mut total_dropped: u64 = 0;

        while let Some(row) = receiver.recv().await {
            match writer.append_row(&row) {
                Ok(()) => {
                    total_appended = total_appended.saturating_add(1);
                }
                Err(_) => {
                    // Writer's append_row already emitted MOVERS-22TF-01
                    // at ERROR with full context — no need to re-log here.
                    total_dropped = total_dropped.saturating_add(1);
                }
            }
            if writer.pending_count() >= flush_threshold {
                if let Err(err) = writer.flush() {
                    warn!(
                        timeframe = label,
                        ?err,
                        "movers_22tf consumer flush failed — writer will reconnect"
                    );
                }
            }
        }

        // Sender side dropped — drain final buffered rows.
        if let Err(err) = writer.flush() {
            warn!(
                timeframe = label,
                ?err,
                "movers_22tf consumer final flush failed on shutdown"
            );
        }
        info!(
            timeframe = label,
            total_appended, total_dropped, "movers_22tf consumer task exiting"
        );
    })
}

/// Trait abstraction for the consumer writer — implemented by
/// `Movers22TfWriter` (the runtime ILP impl) and by `MockConsumerWriter`
/// in tests. Keeps the spawn function generic + testable without a
/// live QuestDB.
pub trait ConsumerWriter {
    /// Returns the timeframe label for tracing context.
    fn timeframe_label(&self) -> &'static str;
    /// Appends a row to the writer's buffer.
    fn append_row(&mut self, row: &MoverRow) -> anyhow::Result<()>;
    /// Returns the current pending row count.
    fn pending_count(&self) -> usize;
    /// Drains the buffer to QuestDB.
    fn flush(&mut self) -> anyhow::Result<()>;
}

impl ConsumerWriter for Movers22TfWriter {
    fn timeframe_label(&self) -> &'static str {
        Movers22TfWriter::timeframe_label(self)
    }
    fn append_row(&mut self, row: &MoverRow) -> anyhow::Result<()> {
        Movers22TfWriter::append_row(self, row)
    }
    fn pending_count(&self) -> usize {
        Movers22TfWriter::pending_count(self)
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        Movers22TfWriter::flush(self)
    }
}

// ---------------------------------------------------------------------------
// Phase 10b-3b — snapshot task spawner
// ---------------------------------------------------------------------------

/// Spawns ONE snapshot task for the timeframe at `slot_idx`. The task:
/// 1. Sleeps until the next cadence-aligned tick.
/// 2. Checks `is_market_hours()` per audit-findings Rule 3.
/// 3. Calls `tracker.snapshot_into(&mut arena, ts_nanos)`.
/// 4. For each row in the arena, calls `enqueue_fn(slot_idx, row)`.
/// 5. Reports outcome via `on_cycle_complete(rows_written)` callback.
///
/// `enqueue_fn` is injected so tests can substitute a mock that
/// counts rows without needing the global writer-state OnceLock to
/// be initialised. In production `enqueue_fn` calls
/// `try_enqueue_global(slot_idx, row)`.
///
/// `now_secs_fn` is injected for deterministic time control in tests
/// (production passes `|| std::time::SystemTime::now()...`).
///
/// The task exits when `shutdown.load(Relaxed)` is true.
pub fn spawn_snapshot_task<F, I, M, N>(
    slot_idx: usize,
    cadence_secs: u64,
    timeframe_label: &'static str,
    tracker: Arc<Movers22TfTracker>,
    mut enqueue_fn: F,
    is_market_hours: I,
    now_nanos_fn: N,
    mut on_cycle_complete: M,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()>
where
    F: FnMut(usize, MoverRow) + Send + 'static,
    I: Fn() -> bool + Send + 'static,
    M: FnMut(usize) + Send + 'static,
    N: Fn() -> i64 + Send + 'static,
{
    use std::sync::atomic::Ordering;

    tokio::spawn(async move {
        info!(
            slot_idx,
            timeframe = timeframe_label,
            cadence_secs,
            "movers_22tf snapshot task starting"
        );
        let mut arena: Vec<MoverRow> =
            Vec::with_capacity(super::movers_22tf_scheduler::MOVERS_ARENA_CAPACITY);
        let interval = std::time::Duration::from_secs(cadence_secs);

        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!(
                    slot_idx,
                    timeframe = timeframe_label,
                    "movers_22tf snapshot task shutting down"
                );
                break;
            }

            tokio::time::sleep(interval).await;

            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Market-hours gate per audit-findings Rule 3.
            if !is_market_hours() {
                on_cycle_complete(0);
                continue;
            }

            let ts_nanos = now_nanos_fn();
            tracker.snapshot_into(&mut arena, ts_nanos);
            let rows = arena.len();
            for row in arena.drain(..) {
                enqueue_fn(slot_idx, row);
            }
            on_cycle_complete(rows);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    /// Phase 10b-2 ratchet: pipeline build allocates exactly 22
    /// senders + 22 receivers.
    #[test]
    fn test_build_movers_22tf_pipeline_creates_22_channels() {
        let pipeline = build_movers_22tf_pipeline();
        assert_eq!(pipeline.receivers.len(), MOVERS_22TF_WRITER_COUNT);
        assert_eq!(pipeline.writer_state.len(), MOVERS_22TF_WRITER_COUNT);
        assert_eq!(MOVERS_22TF_WRITER_COUNT, 22);
    }

    /// Phase 10b-2 ratchet: tracker starts empty + writer state has
    /// senders for every index in [0, 22).
    #[test]
    fn test_build_movers_22tf_pipeline_writer_state_has_all_22_senders() {
        let pipeline = build_movers_22tf_pipeline();
        for idx in 0..MOVERS_22TF_WRITER_COUNT {
            assert!(
                pipeline.writer_state.sender_for(idx).is_some(),
                "writer_state.sender_for({idx}) should be Some"
            );
        }
        assert!(pipeline.writer_state.sender_for(22).is_none());
    }

    /// Phase 10b-2 ratchet: arc_tracker returns a clonable Arc that
    /// shares state with the pipeline tracker.
    #[test]
    fn test_arc_tracker_shares_state() {
        let pipeline = build_movers_22tf_pipeline();
        let arc = arc_tracker(&pipeline);
        let state = crate::pipeline::movers_22tf_tracker::SecurityState {
            ltp: 19500.0,
            prev_close: 19400.0,
            ..crate::pipeline::movers_22tf_tracker::SecurityState::empty()
        };
        // Update via the pipeline's tracker — Arc<>'d clone should see it.
        pipeline
            .tracker
            .update_security_state(13, ExchangeSegment::IdxI, state);
        assert_eq!(arc.len(), 1);
        assert_eq!(arc.get(13, ExchangeSegment::IdxI).unwrap().ltp, 19500.0);
    }

    /// Phase 10b-2 ratchet: pipeline channels actually flow data —
    /// sending on one writer-state sender lands on the matching receiver.
    #[tokio::test]
    async fn test_build_movers_22tf_pipeline_channels_flow_data() {
        let mut pipeline = build_movers_22tf_pipeline();
        let row = MoverRow::empty();

        // Send via writer_state sender at index 0.
        let sender = pipeline.writer_state.sender_for(0).unwrap();
        sender
            .try_send(row)
            .expect("first send to slot 0 should succeed");

        // Receive on the matching receiver at index 0.
        let received = pipeline.receivers[0]
            .try_recv()
            .expect("recv should succeed");
        assert_eq!(received.security_id, row.security_id);
    }

    // -----------------------------------------------------------------
    // Phase 10b-3a — consumer task spawner tests
    // -----------------------------------------------------------------

    /// Mock consumer writer for testing the spawn function without
    /// needing a live QuestDB. Records every operation in counters.
    struct MockConsumerWriter {
        label: &'static str,
        appended: Arc<std::sync::atomic::AtomicUsize>,
        flushed: Arc<std::sync::atomic::AtomicUsize>,
        pending: usize,
    }

    impl ConsumerWriter for MockConsumerWriter {
        fn timeframe_label(&self) -> &'static str {
            self.label
        }
        fn append_row(&mut self, _row: &MoverRow) -> anyhow::Result<()> {
            self.pending += 1;
            self.appended
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn pending_count(&self) -> usize {
            self.pending
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            self.pending = 0;
            self.flushed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    /// Phase 10b-3a ratchet: consumer task drains the receiver into
    /// the writer and flushes when threshold reached.
    #[tokio::test]
    async fn test_spawn_consumer_task_drains_and_flushes() {
        use std::sync::atomic::Ordering;

        let appended = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = MockConsumerWriter {
            label: "1s",
            appended: Arc::clone(&appended),
            flushed: Arc::clone(&flushed),
            pending: 0,
        };
        let (tx, rx) = mpsc::channel::<MoverRow>(64);
        let handle = spawn_consumer_task(writer, rx, 5);

        // Send 12 rows — flush threshold is 5, so we expect 2 mid-stream
        // flushes (after 5 + 10) plus 1 final flush on channel close.
        for _ in 0..12 {
            tx.send(MoverRow::empty())
                .await
                .expect("send should succeed");
        }
        drop(tx);

        // Wait for the task to finish.
        handle.await.expect("task should complete cleanly");

        assert_eq!(appended.load(Ordering::Relaxed), 12, "12 rows appended");
        assert!(
            flushed.load(Ordering::Relaxed) >= 2,
            "at least 2 flushes (mid-stream at 5+10 OR end-of-stream); got {}",
            flushed.load(Ordering::Relaxed)
        );
    }

    /// Phase 10b-3a ratchet: consumer task exits cleanly when sender
    /// drops with no rows received (final flush on empty buffer is a
    /// no-op).
    #[tokio::test]
    async fn test_spawn_consumer_task_exits_on_empty_drop() {
        use std::sync::atomic::Ordering;

        let appended = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = MockConsumerWriter {
            label: "1m",
            appended: Arc::clone(&appended),
            flushed: Arc::clone(&flushed),
            pending: 0,
        };
        let (tx, rx) = mpsc::channel::<MoverRow>(64);
        let handle = spawn_consumer_task(writer, rx, 5);

        drop(tx);
        handle.await.expect("task should complete on drop");

        assert_eq!(appended.load(Ordering::Relaxed), 0);
        assert_eq!(
            flushed.load(Ordering::Relaxed),
            1,
            "exactly one final flush on shutdown"
        );
    }

    /// Phase 10b-3a ratchet: CONSUMER_FLUSH_THRESHOLD constant pinned.
    #[test]
    fn test_consumer_flush_threshold_is_1024() {
        assert_eq!(CONSUMER_FLUSH_THRESHOLD, 1024);
    }

    // -----------------------------------------------------------------
    // Phase 10b-3b — snapshot task spawner tests
    // -----------------------------------------------------------------

    /// Phase 10b-3b ratchet: snapshot task shut down before any cycle
    /// runs exits cleanly without invoking enqueue/cycle callbacks.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_spawn_snapshot_task_shutdown_before_first_cycle() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let tracker = Arc::new(Movers22TfTracker::new());
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let cycle_count = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(true)); // already requested

        let enqueue = {
            let counter = Arc::clone(&enqueue_count);
            move |_slot: usize, _row: MoverRow| {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        };
        let on_cycle = {
            let counter = Arc::clone(&cycle_count);
            move |_rows: usize| {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        };

        let handle = spawn_snapshot_task(
            0,
            1,
            "1s",
            tracker,
            enqueue,
            || true,
            || 1_700_000_000_000_000_000_i64,
            on_cycle,
            shutdown,
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("task should exit on pre-set shutdown")
            .expect("task should complete cleanly");

        assert_eq!(enqueue_count.load(Ordering::Relaxed), 0);
        assert_eq!(cycle_count.load(Ordering::Relaxed), 0);
    }

    /// Phase 10b-3b ratchet: market-hours gate calls on_cycle_complete(0)
    /// without invoking enqueue when off-market.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_spawn_snapshot_task_off_hours_skips_enqueue() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use tickvault_common::types::ExchangeSegment;

        let tracker = Arc::new(Movers22TfTracker::new());
        // Pre-populate so any snapshot would emit rows.
        tracker.update_security_state(
            13,
            ExchangeSegment::IdxI,
            crate::pipeline::movers_22tf_tracker::SecurityState::empty(),
        );

        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let cycle_count = Arc::new(AtomicUsize::new(0));
        let zero_cycle_count = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let enqueue = {
            let counter = Arc::clone(&enqueue_count);
            move |_slot, _row| {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        };
        let on_cycle = {
            let total = Arc::clone(&cycle_count);
            let zeros = Arc::clone(&zero_cycle_count);
            let shutdown_signal = Arc::clone(&shutdown);
            move |rows: usize| {
                total.fetch_add(1, Ordering::Relaxed);
                if rows == 0 {
                    zeros.fetch_add(1, Ordering::Relaxed);
                }
                if total.load(Ordering::Relaxed) >= 1 {
                    shutdown_signal.store(true, Ordering::Relaxed);
                }
            }
        };

        let handle = spawn_snapshot_task(
            0,
            1,
            "1s",
            tracker,
            enqueue,
            || false, // always off-market
            || 1_700_000_000_000_000_000_i64,
            on_cycle,
            Arc::clone(&shutdown),
        );

        tokio::time::timeout(std::time::Duration::from_secs(60), handle)
            .await
            .expect("task should exit on shutdown")
            .expect("task should complete cleanly");

        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            0,
            "off-market path must NOT call enqueue"
        );
        assert!(
            zero_cycle_count.load(Ordering::Relaxed) >= 1,
            "off-market cycle must report rows=0; got {}",
            zero_cycle_count.load(Ordering::Relaxed)
        );
    }

    /// Phase 10b-3b ratchet: in-market cycle drains tracker into the
    /// enqueue callback — exactly tracker.len() rows per cycle.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_spawn_snapshot_task_in_market_drains_tracker() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use tickvault_common::types::ExchangeSegment;

        let tracker = Arc::new(Movers22TfTracker::new());
        // 5 distinct securities.
        for i in 0..5_u32 {
            tracker.update_security_state(
                i,
                ExchangeSegment::NseEquity,
                crate::pipeline::movers_22tf_tracker::SecurityState::empty(),
            );
        }

        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let cycle_count = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let enqueue = {
            let counter = Arc::clone(&enqueue_count);
            move |slot: usize, _row: MoverRow| {
                assert_eq!(slot, 7, "snapshot task must pass its own slot_idx");
                counter.fetch_add(1, Ordering::Relaxed);
            }
        };
        let on_cycle = {
            let total = Arc::clone(&cycle_count);
            let shutdown_signal = Arc::clone(&shutdown);
            move |rows: usize| {
                assert_eq!(rows, 5, "snapshot must drain all 5 tracker entries");
                total.fetch_add(1, Ordering::Relaxed);
                if total.load(Ordering::Relaxed) >= 1 {
                    shutdown_signal.store(true, Ordering::Relaxed);
                }
            }
        };

        let handle = spawn_snapshot_task(
            7,
            1,
            "1s",
            tracker,
            enqueue,
            || true, // always in-market
            || 1_700_000_000_000_000_000_i64,
            on_cycle,
            Arc::clone(&shutdown),
        );

        tokio::time::timeout(std::time::Duration::from_secs(60), handle)
            .await
            .expect("task should exit on shutdown")
            .expect("task should complete cleanly");

        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            5,
            "exactly 5 rows must be enqueued (tracker has 5 entries)"
        );
    }
}
