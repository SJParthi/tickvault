//! Off-thread tick ILP flush worker (B6, operator directive 2026-07-03).
//!
//! Before B6, `TickPersistenceWriter::append_tick_with_seq` performed the
//! 1-in-1000 batch ILP TCP flush SYNCHRONOUSLY inside the tick-consumer
//! loop's timed span — folding flush I/O (~hundreds of µs) into the compute
//! histogram (`tv_tick_processing_duration_ns` p99 ≈ 475µs vs p50 ≈ 30µs).
//!
//! This module moves the blocking `Sender::flush` onto a dedicated,
//! SUPERVISED OS thread (mirrors `ws_frame_spill.rs` / WS-SPILL-01):
//!
//! ```text
//! tick consumer (hot path)                 tick-flush-worker (OS thread)
//! ────────────────────────                 ─────────────────────────────
//! append rows into active Buffer
//! at TICK_FLUSH_BATCH_SIZE:
//!   spare = recycle_rx.try_recv()   ◀──────  cleared (Buffer, Vec) pairs
//!   mem::replace(active, spare)               back after each flush
//!   work_tx.try_send(FlushJob)      ──────▶  sender.flush(&mut buffer)
//!                                            records tv_tick_flush_duration_ns
//!                                            on error: batch → failed_tx
//!   failed_rx.try_recv() batches    ◀──────  Vec<(ParsedTick, capture_seq)>
//!   → ring → spill → DLQ rescue
//! ```
//!
//! **Zero-loss contract (unchanged):** every fallback either flushes inline
//! (legacy behavior, measured as stall), blocks bounded on the work channel,
//! or returns the batch for the EXISTING ring → spill → DLQ rescue chain.
//! No path drops a tick. All channels are BOUNDED (hot-path rule).
//!
//! **Ordering + idempotency:** a single worker + FIFO channel preserves
//! batch order among offloaded batches. The worker owns its OWN questdb
//! `Sender` (a second ILP TCP connection); interleaving with the writer's
//! synchronous drain/recovery flushes cannot corrupt data because QuestDB
//! orders rows by designated timestamp and the `ticks` DEDUP key
//! `(ts, security_id, segment, capture_seq, feed)` makes any replay
//! idempotent (TICK-SEQ-01). `capture_seq` rides inside every job so a
//! rescued batch is re-persisted replay-stable.

use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender as ChannelSender, bounded};
use questdb::ingress::{Buffer, ProtocolVersion, Sender};
use tracing::{debug, error, info, warn};

use tickvault_common::constants::TICK_FLUSH_BATCH_SIZE;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::ParsedTick;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Bounded work-queue depth: at most this many full batches may be in flight
/// to the worker before the hot path blocks on `send` (bounded by one flush
/// duration). 4 batches × 1000 ticks ≈ 4s of tick flow at typical rates.
pub(crate) const FLUSH_WORK_QUEUE_CAP: usize = 4;

/// Spare `(Buffer, Vec)` pairs pre-allocated into the recycle channel at
/// construction. Total pool = spares + the writer's active pair. Steady
/// state: the hot-path batch handoff is a zero-allocation O(1)
/// `mem::replace` + `try_recv` + `try_send`.
pub(crate) const FLUSH_BUFFER_POOL_SPARES: usize = 3;

/// Failed-batch channel capacity. MUST exceed the maximum number of
/// batches that can be outstanding at once (work queue cap + 1 in-process
/// + pool size), so the worker NEVER blocks sending a failed batch while
/// the writer blocks sending a job — deadlock-impossible by construction.
pub(crate) const FLUSH_FAILED_QUEUE_CAP: usize = 16;

/// Backoff before the supervisor re-enters the worker loop after a panic or
/// fatal return — a hard-failing worker cannot pin a CPU in a hot respawn
/// loop. Mirrors WS-SPILL-01 / WS-GAP-05 / DISK-WATCHER-01.
const FLUSH_WORKER_RESPAWN_BACKOFF: Duration = Duration::from_millis(200); // APPROVED: this IS the named constant the rule asks for

/// Throttle between the worker's QuestDB ILP (re)connect attempts — same
/// value class as the writer-side `RECONNECT_THROTTLE_SECS` so a sustained
/// outage produces at most one connect attempt per window on each side.
const WORKER_RECONNECT_THROTTLE: Duration = Duration::from_secs(30); // APPROVED: this IS the named constant the rule asks for

/// Poll interval for the bounded shutdown join (std `JoinHandle` has no
/// timed join). Cold path — graceful shutdown only.
const SHUTDOWN_JOIN_POLL_INTERVAL: Duration = Duration::from_millis(10); // APPROVED: this IS the named constant the rule asks for

/// Bounded wait for the worker to drain + exit at graceful shutdown. A
/// worker wedged past this is abandoned (WAL replay covers the in-flight
/// batch on next boot — honest envelope).
pub(crate) const FLUSH_WORKER_SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(10); // APPROVED: this IS the named constant the rule asks for

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A full ILP batch handed to the worker: the row buffer plus the
/// `(tick, capture_seq)` mirror used for ring/spill/DLQ rescue on failure.
pub(crate) struct FlushJob {
    pub(crate) buffer: Buffer,
    pub(crate) in_flight: Vec<(ParsedTick, i64)>,
}

/// A cleared, capacity-warm `(Buffer, Vec)` pair returned to the writer.
pub(crate) type PoolPair = (Buffer, Vec<(ParsedTick, i64)>);

/// A batch the worker could not flush — returned for ring/spill/DLQ rescue.
pub(crate) type FailedBatch = Vec<(ParsedTick, i64)>;

/// Writer-side handle to the off-thread flush worker.
pub(crate) struct FlushOffload {
    pub(crate) work_tx: ChannelSender<FlushJob>,
    pub(crate) recycle_rx: Receiver<PoolPair>,
    pub(crate) failed_rx: Receiver<FailedBatch>,
    join: Option<thread::JoinHandle<()>>,
}

/// Outcome of a hot-path dispatch attempt (see [`FlushOffload::try_dispatch`]).
pub(crate) enum DispatchOutcome {
    /// Batch handed to the worker without blocking. Zero stall.
    Sent,
    /// Work queue was full — the hot path blocked on the bounded `send` for
    /// the contained number of nanoseconds (persistence stall).
    SentAfterBlock(u64),
    /// No spare buffer in the pool (worker deeply behind) — caller must fall
    /// back to the legacy inline flush.
    NoSpare,
    /// Worker channel gone (near-impossible under supervision) — the job was
    /// restored into the writer's active buffer/vec; caller falls back to
    /// the legacy inline flush.
    WorkerGone,
}

impl FlushOffload {
    /// Attempts the zero-stall batch handoff: take a spare pair from the
    /// pool, swap it with the caller's full `buffer` + `in_flight`, and send
    /// the full pair to the worker.
    ///
    /// On `NoSpare` / `WorkerGone` the caller's buffer + in_flight are left
    /// exactly as they were (full) so the inline fallback can flush them.
    pub(crate) fn try_dispatch(
        &self,
        buffer: &mut Buffer,
        in_flight: &mut Vec<(ParsedTick, i64)>,
    ) -> DispatchOutcome {
        let Ok((spare_buf, spare_vec)) = self.recycle_rx.try_recv() else {
            return DispatchOutcome::NoSpare;
        };
        let full_buf = std::mem::replace(buffer, spare_buf);
        let full_vec = std::mem::replace(in_flight, spare_vec);
        let job = FlushJob {
            buffer: full_buf,
            in_flight: full_vec,
        };
        match self.work_tx.try_send(job) {
            Ok(()) => DispatchOutcome::Sent,
            Err(crossbeam_channel::TrySendError::Full(job)) => {
                // Worker > FLUSH_WORK_QUEUE_CAP batches behind. Block on the
                // bounded send (bounded by one flush duration) — zero-loss
                // preferred over drop. The caller records the stall so the
                // compute histogram stays honest.
                metrics::counter!(
                    "tv_tick_flush_backpressure_block_total",
                    "reason" => "queue_full"
                )
                .increment(1);
                let block_start = Instant::now();
                match self.work_tx.send(job) {
                    Ok(()) => {
                        let stall =
                            u64::try_from(block_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                        DispatchOutcome::SentAfterBlock(stall)
                    }
                    Err(crossbeam_channel::SendError(job)) => {
                        // Worker channel disconnected mid-block: restore the
                        // full pair so the inline fallback flushes it.
                        *buffer = job.buffer;
                        *in_flight = job.in_flight;
                        DispatchOutcome::WorkerGone
                    }
                }
            }
            Err(crossbeam_channel::TrySendError::Disconnected(job)) => {
                *buffer = job.buffer;
                *in_flight = job.in_flight;
                DispatchOutcome::WorkerGone
            }
        }
    }

    /// Shuts the worker down: drops the work channel (the worker drains every
    /// queued job, then its supervisor exits cleanly) and joins the thread
    /// with a bounded wait. Returns the failed-batch receiver so the caller
    /// can rescue any batches the worker could not flush.
    ///
    /// A worker wedged past `timeout` is abandoned with a warn — the frames
    /// of any in-flight batch are already durably captured in the WAL
    /// (`ws_frame_spill`) and replay on next boot (honest envelope).
    pub(crate) fn shutdown_and_join(self, timeout: Duration) -> Receiver<FailedBatch> {
        let FlushOffload {
            work_tx,
            recycle_rx,
            failed_rx,
            join,
        } = self;
        drop(work_tx);
        drop(recycle_rx);
        if let Some(handle) = join {
            let deadline = Instant::now() + timeout;
            // Cold path (graceful shutdown only): a bounded poll-join. std
            // JoinHandle has no timed join; 10ms polling is fine off-market.
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(SHUTDOWN_JOIN_POLL_INTERVAL);
            }
            if handle.is_finished() {
                if handle.join().is_err() {
                    // Supervisor catch_unwind makes this ~unreachable; still
                    // surfaced honestly rather than discarded.
                    warn!("tick flush worker supervisor panicked at final join");
                }
            } else {
                warn!(
                    "tick flush worker did not finish within shutdown timeout — \
                     proceeding (WAL replay covers any in-flight batch)"
                );
            }
        }
        failed_rx
    }
}

// ---------------------------------------------------------------------------
// Spawn + supervision
// ---------------------------------------------------------------------------

/// Spawns the supervised flush worker and pre-allocates the buffer pool.
///
/// The worker's questdb `Sender` is built LAZILY (throttled) from
/// `ilp_conf_string` on the first job — so a writer constructed in
/// disconnected mode does not attempt TCP at spawn time.
pub(crate) fn spawn_flush_offload(ilp_conf_string: &str) -> anyhow::Result<FlushOffload> {
    let (work_tx, work_rx) = bounded::<FlushJob>(FLUSH_WORK_QUEUE_CAP);
    let (recycle_tx, recycle_rx) = bounded::<PoolPair>(FLUSH_BUFFER_POOL_SPARES + 1);
    let (failed_tx, failed_rx) = bounded::<FailedBatch>(FLUSH_FAILED_QUEUE_CAP);

    // Pre-allocate the spare pool so the steady-state hot path never
    // allocates: the swap is mem::replace + try_recv + try_send.
    // O(1) EXEMPT: begin — one-time construction, bounded pool
    for _ in 0..FLUSH_BUFFER_POOL_SPARES {
        let pair: PoolPair = (
            Buffer::new(ProtocolVersion::V1),
            Vec::with_capacity(TICK_FLUSH_BATCH_SIZE),
        );
        if recycle_tx.try_send(pair).is_err() {
            anyhow::bail!("flush offload pool pre-allocation overflowed its channel");
        }
    }
    // O(1) EXEMPT: end

    let conf = ilp_conf_string.to_string();
    let handle = thread::Builder::new()
        .name("tick-flush-worker".to_string())
        .spawn(move || {
            // Supervisor loop (mirrors WS-SPILL-01 / WS-GAP-05 /
            // DISK-WATCHER-01): a panic or fatal return must NOT silently
            // remove the off-thread flush path — re-enter `worker_loop` with
            // the SAME channels so queued batches survive and the writer's
            // dispatch never sees `Disconnected`.
            loop {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_loop(&work_rx, &recycle_tx, &failed_tx, &conf)
                }));
                match outcome {
                    Ok(()) => {
                        // Clean shutdown: work channel closed + drained.
                        info!("tick-flush-worker exited cleanly (channel closed)");
                        break;
                    }
                    Err(_panic) => {
                        error!(
                            code = ErrorCode::TickFlush01WorkerRespawn.code_str(),
                            "CRITICAL: tick flush worker PANICKED — respawning to keep \
                             the ILP flush off the tick-consumer thread"
                        );
                        metrics::counter!(
                            "tv_tick_flush_worker_respawn_total",
                            "reason" => "panic"
                        )
                        .increment(1);
                    }
                }
                thread::sleep(FLUSH_WORKER_RESPAWN_BACKOFF);
            }
        })
        .map_err(|e| anyhow::anyhow!("spawn tick-flush-worker thread: {e}"))?;

    // The supervisor loop IS the thread — this one handle covers every
    // respawn, so the bounded shutdown join waits on the supervisor itself.
    Ok(FlushOffload {
        work_tx,
        recycle_rx,
        failed_rx,
        join: Some(handle),
    })
}

/// Worker state: its own questdb `Sender` (a SECOND ILP TCP connection,
/// independent of the writer's) plus a reconnect throttle.
struct WorkerState {
    sender: Option<Sender>,
    next_reconnect_allowed: Instant,
}

/// Drains `FlushJob`s until the work channel is closed AND empty (crossbeam
/// `iter()` yields every queued message before terminating on disconnect —
/// no queued batch is lost on shutdown).
fn worker_loop(
    work_rx: &Receiver<FlushJob>,
    recycle_tx: &ChannelSender<PoolPair>,
    failed_tx: &ChannelSender<FailedBatch>,
    conf: &str,
) {
    // Metric handle captured once per (re)entry — cold-path lookups only.
    let m_flush_duration = metrics::histogram!("tv_tick_flush_duration_ns");
    let mut state = WorkerState {
        sender: None,
        next_reconnect_allowed: Instant::now(),
    };
    for job in work_rx.iter() {
        process_job(
            job,
            recycle_tx,
            failed_tx,
            conf,
            &mut state,
            &m_flush_duration,
        );
    }
}

/// Flushes one batch; on any failure the batch is returned to the writer via
/// the failed channel for ring → spill → DLQ rescue. A per-batch failure
/// NEVER kills the thread (mirrors the ws_frame_spill resilient loop).
fn process_job(
    mut job: FlushJob,
    recycle_tx: &ChannelSender<PoolPair>,
    failed_tx: &ChannelSender<FailedBatch>,
    conf: &str,
    state: &mut WorkerState,
    m_flush_duration: &metrics::Histogram,
) {
    // Lazily (re)connect, throttled — same policy class as the writer's
    // try_reconnect_on_error so a sustained outage never storms QuestDB.
    if state.sender.is_none() {
        let now = Instant::now();
        if now >= state.next_reconnect_allowed {
            state.next_reconnect_allowed = now + WORKER_RECONNECT_THROTTLE;
            match Sender::from_conf(conf) {
                Ok(s) => {
                    info!("tick flush worker: QuestDB ILP connected");
                    state.sender = Some(s);
                }
                Err(err) => {
                    // Rule 5: flush/persist failures are error!, never warn!.
                    error!(
                        ?err,
                        batch = job.in_flight.len(),
                        "tick flush worker: QuestDB ILP connect failed — batch \
                         returned to writer for ring/spill/DLQ rescue"
                    );
                }
            }
        } else {
            debug!(
                batch = job.in_flight.len(),
                "tick flush worker: reconnect throttled — batch returned for rescue"
            );
        }
    }

    let Some(sender) = state.sender.as_mut() else {
        fail_batch(job, recycle_tx, failed_tx);
        return;
    };

    // Time the actual ILP TCP write — the operator's flush-latency tile
    // reads tv_tick_flush_duration_ns (same histogram force_flush records
    // on the synchronous escape-hatch path). Recorded on the failure path
    // too: the time until the TCP error is real latency.
    let flush_start = Instant::now();
    let flush_result = sender.flush(&mut job.buffer);
    #[allow(clippy::cast_precision_loss)]
    // APPROVED: same cast as the force_flush histogram record
    m_flush_duration.record(flush_start.elapsed().as_nanos() as f64);

    match flush_result {
        Ok(()) => {
            let flushed = job.in_flight.len();
            job.buffer.clear();
            job.in_flight.clear();
            // Pool channel capacity == pool size, so try_send cannot
            // legitimately fail; a failure here only shrinks the pool
            // (writer degrades to inline fallback — still zero-loss).
            if recycle_tx.try_send((job.buffer, job.in_flight)).is_err() {
                debug!("tick flush worker: recycle channel unavailable — pool pair dropped");
            }
            debug!(
                flushed_rows = flushed,
                "tick batch flushed to QuestDB (off-thread)"
            );
        }
        Err(err) => {
            metrics::counter!("tv_tick_flush_worker_errors_total").increment(1);
            // Rule 5: flush failures are error! (Telegram-routable).
            error!(
                ?err,
                batch = job.in_flight.len(),
                "off-thread tick flush failed — batch returned to writer for \
                 ring/spill/DLQ rescue"
            );
            // Sender is broken — drop it; the next job re-connects (throttled).
            state.sender = None;
            fail_batch(job, recycle_tx, failed_tx);
        }
    }
}

/// Returns a batch the worker could not flush to the writer (failed channel)
/// and recycles the buffer with a fresh in-flight Vec.
fn fail_batch(
    mut job: FlushJob,
    recycle_tx: &ChannelSender<PoolPair>,
    failed_tx: &ChannelSender<FailedBatch>,
) {
    job.buffer.clear();
    // O(1) EXEMPT: cold failure path — one Vec allocation per FAILED batch
    // (the original Vec travels to the writer on the failed channel).
    let batch = std::mem::replace(
        &mut job.in_flight,
        Vec::with_capacity(TICK_FLUSH_BATCH_SIZE),
    );
    // FLUSH_FAILED_QUEUE_CAP exceeds the max outstanding batches, so this
    // send cannot block against a writer that is itself blocked on work_tx
    // (deadlock-impossible by construction). `send` (not try_send) so a
    // transiently-slow writer never causes a DROP.
    if failed_tx.send(batch).is_err() {
        // Writer side gone (abrupt teardown). The frames are already durable
        // in the WAL (captured at the socket, before the pipeline) — replay
        // recovers them on next boot.
        error!(
            "tick flush worker: failed-batch channel closed — batch recovery \
             deferred to WAL replay on next boot"
        );
    }
    if recycle_tx.try_send((job.buffer, job.in_flight)).is_err() {
        debug!("tick flush worker: recycle channel unavailable — pool pair dropped");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn sample_tick(security_id: u64) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: 0,
            last_traded_price: 100.5,
            last_trade_quantity: 0,
            exchange_timestamp: 1_770_000_000,
            received_at_nanos: 1_770_000_000_000_000_000,
            average_traded_price: 100.0,
            volume: 0,
            total_sell_quantity: 0,
            total_buy_quantity: 0,
            day_open: 99.0,
            day_close: 98.0,
            day_high: 101.0,
            day_low: 97.0,
            open_interest: 0,
            oi_day_high: 0,
            oi_day_low: 0,
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
        }
    }

    /// Spawns a loopback TCP listener that reads-and-discards (ILP TCP V1 is
    /// handshake-free). Returns the port, or None when the sandbox forbids
    /// loopback sockets.
    fn spawn_tcp_drain() -> Option<u16> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                std::thread::spawn(move || {
                    let mut sink = [0_u8; 65_536];
                    while let Ok(n) = s.read(&mut sink) {
                        if n == 0 {
                            break;
                        }
                    }
                });
            }
        });
        Some(port)
    }

    #[test]
    fn test_offload_pool_preallocates_spares() {
        let offload = spawn_flush_offload("tcp::addr=127.0.0.1:1;").expect("spawn"); // APPROVED: test
        let mut drained = 0;
        while offload.recycle_rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(
            drained, FLUSH_BUFFER_POOL_SPARES,
            "pool must pre-allocate exactly FLUSH_BUFFER_POOL_SPARES pairs"
        );
        drop(offload.shutdown_and_join(Duration::from_secs(2)));
    }

    #[test]
    fn test_worker_fail_path_returns_batch_on_failed_channel() {
        // Port 9 (discard) on loopback is almost certainly closed → the
        // worker's lazy connect fails → the batch must come back intact.
        let offload = spawn_flush_offload("tcp::addr=127.0.0.1:9;").expect("spawn"); // APPROVED: test
        let (buf, mut vec) = offload.recycle_rx.try_recv().expect("pool has spares"); // APPROVED: test
        vec.push((sample_tick(13), 1));
        vec.push((sample_tick(25), 2));
        offload
            .work_tx
            .send(FlushJob {
                buffer: buf,
                in_flight: vec,
            })
            .expect("worker alive"); // APPROVED: test
        let failed = offload
            .failed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("failed batch must be returned"); // APPROVED: test
        assert_eq!(failed.len(), 2, "both ticks must survive the failure");
        assert_eq!(failed[0].1, 1, "capture_seq order preserved");
        assert_eq!(failed[1].1, 2, "capture_seq order preserved");
        drop(offload.shutdown_and_join(Duration::from_secs(2)));
    }

    #[test]
    fn test_worker_success_recycles_pair() {
        let Some(port) = spawn_tcp_drain() else {
            return; // sandbox forbids loopback sockets — skip honestly
        };
        let offload = spawn_flush_offload(&format!("tcp::addr=127.0.0.1:{port};")).expect("spawn"); // APPROVED: test
        let (mut buf, mut vec) = offload.recycle_rx.try_recv().expect("pool has spares"); // APPROVED: test
        // One real row so the flush actually writes bytes.
        crate::tick_persistence_testing::build_tick_row_seq_pub(&mut buf, &sample_tick(13), 7)
            .expect("row build"); // APPROVED: test
        vec.push((sample_tick(13), 7));
        offload
            .work_tx
            .send(FlushJob {
                buffer: buf,
                in_flight: vec,
            })
            .expect("worker alive"); // APPROVED: test
        // The cleared pair must come back on the recycle channel.
        let (rbuf, rvec) = offload
            .recycle_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("recycled pair"); // APPROVED: test
        assert_eq!(rbuf.len(), 0, "recycled buffer must be cleared");
        assert!(rvec.is_empty(), "recycled in-flight vec must be cleared");
        assert!(
            offload.failed_rx.try_recv().is_err(),
            "no failed batch on the success path"
        );
        drop(offload.shutdown_and_join(Duration::from_secs(2)));
    }

    #[test]
    fn test_shutdown_drains_queued_jobs_before_join() {
        let Some(port) = spawn_tcp_drain() else {
            return; // sandbox forbids loopback sockets — skip honestly
        };
        let offload = spawn_flush_offload(&format!("tcp::addr=127.0.0.1:{port};")).expect("spawn"); // APPROVED: test
        let (mut buf, mut vec) = offload.recycle_rx.try_recv().expect("pool has spares"); // APPROVED: test
        crate::tick_persistence_testing::build_tick_row_seq_pub(&mut buf, &sample_tick(51), 9)
            .expect("row build"); // APPROVED: test
        vec.push((sample_tick(51), 9));
        offload
            .work_tx
            .send(FlushJob {
                buffer: buf,
                in_flight: vec,
            })
            .expect("worker alive"); // APPROVED: test
        // Shutdown must let the worker drain the queued job (crossbeam
        // iter() yields queued messages before Disconnected) — no failed
        // batch, clean join.
        let failed_rx = offload.shutdown_and_join(Duration::from_secs(5));
        assert!(
            failed_rx.try_recv().is_err(),
            "queued job must be flushed (not failed) during shutdown drain"
        );
    }

    #[test]
    fn test_failed_queue_cap_exceeds_max_outstanding_batches() {
        // Deadlock-impossibility invariant: the worker's blocking send on
        // the failed channel can never wedge against a writer blocked on
        // the work channel.
        assert!(
            FLUSH_FAILED_QUEUE_CAP > FLUSH_WORK_QUEUE_CAP + 1 + FLUSH_BUFFER_POOL_SPARES + 1,
            "failed-channel cap must exceed max outstanding batches"
        );
    }
}
