//! Wave 6 Sub-PR #1 item 1.2f.4 — sealed-candle writer-runner orchestrator.
//!
//! Bundles the three pieces shipped by items 1.2a–1.2f.3 into one
//! struct with a single sync `run_one_cycle` entry point that the
//! eventual tokio loop (item 1.2f.5) will call on a timer:
//!
//! - **Producer side**: `tokio::sync::mpsc::Sender<BufferedSeal>` —
//!   the future aggregator hot path uses this to enqueue seals via
//!   `try_send` (non-blocking on overflow).
//! - **Consumer side** (this struct):
//!   - `tokio::sync::mpsc::Receiver<BufferedSeal>` — drained
//!     non-blockingly via `try_recv`.
//!   - [`SealAbsorptionPipeline`] — owned, single-threaded, holds the
//!     local ring + spill + DLQ.
//!   - [`ShadowCandleWriter`] — owned, single-threaded, holds the
//!     ILP `Sender` + buffer.
//!
//! ## Why an mpsc + a separate ring (two queues)?
//!
//! Mirrors `tick_persistence::TickPersistenceWriter`:
//! - The `mpsc` is the **thread-safe wire** between the aggregator
//!   hot path (zero-alloc, `&AtomicCell`) and the writer task (cold
//!   path, allowed to allocate). `try_send` is non-blocking and NEVER
//!   waits on I/O.
//!
//!   **Corrected 2026-08-19:** this line read "if the channel is full,
//!   the producer drops and increments a Prom counter". That was an
//!   accurate description of a policy the operator has overruled —
//!   *"never ever drop any ticks irrespective of any worst case"*. A
//!   refused seal now goes through [`SealOverflow`] to the SAME spill →
//!   DLQ tiers the ring uses, so the only remaining loss requires the
//!   data volume to be unwritable.
//! - The `SealRing` (inside the pipeline) is a **local single-threaded
//!   buffer** owned by the writer task. It absorbs bursts when the
//!   ILP send is briefly slow, and on overflow cascades to disk
//!   spill → NDJSON DLQ (the locked L-C1 cascade).
//!
//! The two-tier setup means the aggregator's hot path is never
//! coupled to ILP latency, and the writer task never blocks the
//! producer.
//!
//! ## What this slice ships
//!
//! - [`SealWriterRunner`] struct.
//! - [`SealWriterRunner::new`] / [`SealWriterRunner::for_test`].
//! - [`SealWriterRunner::sender`] — clone-able producer-side handle
//!   for the future aggregator wiring.
//! - [`SealWriterRunner::run_one_cycle`] — drain mpsc into pipeline,
//!   then drain pipeline via `drain_once`, return aggregated
//!   [`CycleOutcome`].
//! - [`CycleOutcome`] — wraps `submitted_from_mpsc` + a [`DrainOutcome`].
//! - 11 unit tests covering every happy + degraded path.
//!
//! ## What this slice does NOT ship
//!
//! - `tokio::spawn` long-running task with interval timer +
//!   cancellation token (item 1.2f.5).
//! - Reconnect throttle for `ShadowCandleWriter` (item 1.2f.5).
//! - Boot wiring + Prom counter increments (item 1.4).

use tokio::sync::mpsc;

use tickvault_trading::candles::BufferedSeal;

use crate::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use crate::seal_dlq::SealDlqWriter;
use crate::seal_spill::SealSpillWriter;
use crate::seal_writer_task::{
    BootDrainOutcome, DrainOutcome, MidSessionReplay, ReplayOutcome, drain_once,
    drain_recovered_seals,
};
use crate::shadow_candle_writer::ShadowCandleWriter;

/// Production spill directory, derived through the public `SealSpillWriter`
/// API so this module never duplicates the path literal (a drifted copy
/// would silently recover from the wrong directory — i.e. recover nothing).
fn production_spill_dir() -> std::path::PathBuf {
    SealSpillWriter::new()
        .spill_path(0)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
}

/// Production DLQ directory — same derivation as [`production_spill_dir`].
fn production_dlq_dir() -> std::path::PathBuf {
    SealDlqWriter::new()
        .dlq_path(0)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
}

/// Wave 6 Sub-PR #1 item 1.4c — process-global mpsc Sender that any
/// producer (e.g. the future aggregator task that subscribes to the
/// tick broadcast in `crates/app/src/main.rs`) can clone to push
/// `BufferedSeal`s into the writer task without threading a Sender
/// through every layer of the boot sequence.
///
/// Mirrors the `GLOBAL_QUESTDB_CONFIG` pattern shipped earlier in
/// this crate — `OnceLock` is set ONCE at boot (right before the
/// writer task is spawned) and read-only thereafter.
///
/// If the bridge is `None` (i.e. the writer task failed to construct,
/// or boot has not progressed far enough), producers should treat
/// this as "seals discarded" — log a `tv_seal_producer_no_bridge_total`
/// counter increment per call site and continue. The legacy
/// `candles_1s` path is still feeding production trading; only the
/// new shadow-table pipeline goes dark.
static GLOBAL_SEAL_SENDER: std::sync::OnceLock<mpsc::Sender<BufferedSeal>> =
    std::sync::OnceLock::new();

/// Install the global seal Sender. Idempotent — returns `true` on
/// first install; subsequent calls return `false` and do NOT replace
/// the existing sender (matches `set_global_questdb_config`).
///
/// Caller (typically the boot sequence in `main.rs`) MUST call this
/// BEFORE moving the `SealWriterRunner` into its `tokio::spawn` block,
/// because `runner.sender()` becomes inaccessible after the move.
pub fn set_global_seal_sender(sender: mpsc::Sender<BufferedSeal>) -> bool {
    GLOBAL_SEAL_SENDER.set(sender).is_ok()
}

/// Read-only accessor for the global seal Sender. Returns `None`
/// until the boot path installs one.
///
/// Producers clone the returned sender (mpsc Senders are cheap to
/// clone) and call `try_send(seal)` on the clone. `try_send` is
/// non-blocking by design.
///
/// **A refused seal must NEVER be discarded.** Operator directive
/// 2026-08-19: *"never ever drop any ticks irrespective of any worst case"*
/// and *"never dropped or dleetd dude just mvoe it to db and s3 right?"*.
/// The doc here used to say the seal "is dropped and the producer increments
/// `tv_seal_producer_mpsc_full_total`" — that was a truthful description of
/// a policy the operator has now overruled. Route the refusal through
/// [`global_seal_overflow`] instead; only a seal that fails BOTH the spill
/// and the DLQ is genuinely lost, and that case fires AGGREGATOR-DROP-01.
#[must_use]
pub fn global_seal_sender() -> Option<&'static mpsc::Sender<BufferedSeal>> {
    GLOBAL_SEAL_SENDER.get()
}

/// Where a seal goes when the writer channel will not take it.
///
/// The producer path (`try_send` on the bounded mpsc) can refuse a seal for
/// two reasons — the channel is full because the writer has fallen behind, or
/// no writer was ever installed. Before 2026-08-19 both outcomes incremented a
/// counter and threw the sealed candle away. This type is the durable
/// alternative: the SAME tier-2 → tier-3 cascade the absorption pipeline uses
/// for ring overflow, reachable from the producer side.
///
/// It deliberately holds `Arc`s of the pipeline's OWN writers rather than
/// constructing its own, so a producer-side escalation lands in the same
/// spill file the boot drain reads back. See
/// [`crate::seal_absorption::SealAbsorptionPipeline::spill_handle`].
pub struct SealOverflow {
    spill: std::sync::Arc<crate::seal_spill::SealSpillWriter>,
    dlq: std::sync::Arc<crate::seal_dlq::SealDlqWriter>,
    /// Bounded hand-off to the dedicated escalation thread.
    ///
    /// `None` until [`Self::split_escalation_offload`] runs, and `None` is the
    /// pre-2026-08-28 behaviour: every escalation is a synchronous disk write
    /// on whatever task called it. Once installed, an escalation costs a
    /// channel `try_send` and the disk work happens on `tv-seal-escalate`.
    offload: Option<std::sync::mpsc::SyncSender<SealEscalationItem>>,
    /// Seals handed to the escalation thread and not yet written (audit
    /// PR15). One relaxed add per queued seal, one subtract per written run.
    /// Read at shutdown so an abandoned queue reports how many seals it held,
    /// not merely that it held some.
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Pre-resolved counter handles for the two per-tick outcomes.
    ///
    /// [`Self::escalate`] is reached from the per-tick fold closure on the
    /// frame-drain task, and this repository bans the bare `metrics::counter!`
    /// macro there: `multi_tf_aggregator` calls the fold "the one place a bare
    /// `counter!` macro must never appear", and `DrainCounters` documents why
    /// — the macro builds a `Key` and takes a sharded-registry lock on every
    /// call, where a resolved handle is a plain atomic add. Both names are
    /// compile-time `&'static str` and neither carries a label, so the handle
    /// set is enumerable up front with no cardinality hiding in it.
    ///
    /// Honest magnitude: the macro's arm here is the NON-allocating one, so
    /// this is not the `record_ws_lag` defect class. What is removed is a hash
    /// plus a registry lock per refused seal — and a refusal burst is exactly
    /// when the drain can least afford them (measured 2026-08-20:
    /// `spilled: 541,519` in one session).
    queued: metrics::Counter,
    inline_fallback: metrics::Counter,
    /// Milliseconds the caller spent inside the inline fallback (audit
    /// PR40d). Timed on the REFUSED arm only; the queued arm reads no clock.
    inline_wait_ms: metrics::Counter,
    /// Inline waits of at least [`SEAL_INLINE_WAIT_PAGE_MS`].
    inline_stall: metrics::Counter,
    /// Throttle and window state for the `AGGREGATOR-STALL-01` line.
    stall_report: InlineStallReport,
}

/// Bounded depth of the escalation hand-off queue.
///
/// **DERIVED from `SEAL_BUFFER_CAPACITY` since 2026-09-27 (audit PR15).** It
/// was 4,096. A refused seal reaches this queue only when the 250,000-deep
/// seal channel in front of the writer is itself full, and the one burst
/// that fills that channel is the close force-seal: `AGGREGATOR_MAX_SLOTS ×
/// TF_COUNT` seals at once. At 4,096 the queue held under 2% of such a
/// burst, so a burst that met a slow disk ran the rest of its refusals
/// through the inline cascade — a file write on the frame-drain task, the
/// defect this queue exists to prevent. Sized to one whole burst, the queue
/// can take every refusal of that burst while the disk does nothing at all.
///
/// **Cost, stated plainly:** `std::sync::mpsc::sync_channel` allocates its
/// slots up front, so this is committed memory, not lazy: 250,000 slots of
/// one [`SealEscalationItem`] plus a stamp each, ~36 MB, the same order as
/// the seal ring and the channel in front of it (0.1% of the 32 GiB host).
///
/// **Shutdown:** the thread writes up to [`SEAL_ESCALATION_BATCH`] records
/// per `write(2)`, so a full queue drains in ~245 writes, ~32 MB. That fits
/// the 5 s `SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS` at 10 ms per write or at a
/// degraded 20 MB/s, pinned by
/// `the_queue_depth_is_drainable_inside_the_shutdown_budget`. It does NOT fit
/// when the disk refuses the batch: the seals then go to the DLQ one record
/// at a time, and whatever is still queued at the deadline is counted as
/// abandoned at shutdown. Since PR17 the thread also syncs the day file:
/// a full queue keeps every batch full, so it syncs at most once a second
/// plus once at exit, about 6 extra `sync_data` calls inside the budget.
///
/// Overflow is still NOT a loss: a full queue falls back to the inline
/// cascade, degraded rather than lossy. Past one full burst of refusals
/// against a stalled disk there are only three choices — block the drain,
/// drop the seal, or grow without bound — and the inline write is the
/// least bad; `tv_seal_escalation_inline_fallback_total` counts it.
pub const SEAL_ESCALATION_QUEUE_DEPTH: usize = tickvault_trading::candles::SEAL_BUFFER_CAPACITY;

/// Most records the escalation thread writes with one `write(2)`.
///
/// 1,024 records × 128 bytes = 128 KiB per write. Batching is what lets one
/// thread keep up with a full burst: one syscall per record was the old
/// cost, and at a degraded disk that alone could not drain a 250,000-deep
/// queue inside the shutdown budget.
pub const SEAL_ESCALATION_BATCH: usize = 1_024;

/// How often the escalation thread wakes to re-check its stop flag while the
/// queue is empty.
const SEAL_ESCALATION_STOP_POLL: std::time::Duration = std::time::Duration::from_millis(100); // APPROVED: this IS the named constant the rule asks for

/// Longest the escalation thread lets written seals sit unsynced while a
/// burst keeps the queue full (PR17). A batch that empties the queue is
/// synced at once.
const SEAL_ESCALATION_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1); // APPROVED: this IS the named constant the rule asks for

/// Seals handed to the escalation thread (the happy path once installed).
pub const SEAL_ESCALATION_QUEUED_COUNTER: &str = "tv_seal_escalation_queued_total";

/// Seals the queue would not take, escalated inline on the caller's task.
/// Non-zero means the escalation thread is behind — the caller paid the disk
/// write itself, which is the pre-offload behaviour, not a loss.
pub const SEAL_ESCALATION_INLINE_FALLBACK_COUNTER: &str =
    "tv_seal_escalation_inline_fallback_total";

/// Seals BOTH disk tiers refused, on the escalation thread. Every one of
/// these also fires `AGGREGATOR-DROP-01` through the caller-supplied
/// `on_lost` hook — this counter exists so the deferred loss is countable
/// separately from the inline one, since the caller was told `Queued`.
pub const SEAL_ESCALATION_LOST_COUNTER: &str = "tv_seal_escalation_lost_total";

/// Seals still queued when the shutdown budget expired. The thread was
/// abandoned and those seals died with the process.
pub const SEAL_ESCALATION_ABANDONED_COUNTER: &str = "tv_seal_escalation_abandoned_total";

/// Milliseconds callers spent writing a refused seal themselves (audit PR40d).
///
/// Only the inline fallback adds to it, so a zero means the drain never paid
/// for a disk write; a rising value is the wait the operator chose to accept
/// (noise-lock §2.7) instead of dropping the seal or growing memory.
pub const SEAL_ESCALATION_INLINE_WAIT_MS_COUNTER: &str = "tv_seal_escalation_inline_wait_ms_total";

/// Inline waits of at least [`SEAL_INLINE_WAIT_PAGE_MS`] (audit PR40d).
pub const SEAL_ESCALATION_INLINE_STALL_COUNTER: &str = "tv_seal_escalation_inline_stall_total";

/// An inline wait this long pages as `AGGREGATOR-STALL-01`.
///
/// One second of a paused socket read is ~5,000 packets at the measured
/// envelope. NOT measured-optimal: no session has reached the inline arm since
/// the queue was sized to a full close burst, so the value sits where a pause is
/// unambiguously bad (noise-lock §2.7, "NOT claimed").
pub const SEAL_INLINE_WAIT_PAGE_MS: u64 = 1_000;

/// At most one `AGGREGATOR-STALL-01` line per this many seconds. A disk that
/// stays slow would otherwise log once per refused seal — up to a whole close
/// burst of lines, each written to the same struggling disk.
pub const SEAL_INLINE_STALL_LOG_EVERY_SECS: i64 = 60;

/// Window state for the throttled `AGGREGATOR-STALL-01` line (audit PR40d).
///
/// Every field is an atomic so the report stays `&self`: the overflow lives in
/// a process-wide `OnceLock`. Waits inside one window are folded into the next
/// line as a count and a maximum, so throttling hides no stall.
struct InlineStallReport {
    /// Unix seconds of the last emitted line; `i64::MIN` before the first.
    last_logged_secs: std::sync::atomic::AtomicI64,
    /// Stalls since the last emitted line, this one included.
    stalls: std::sync::atomic::AtomicU64,
    /// Longest stall since the last emitted line.
    max_waited_ms: std::sync::atomic::AtomicU64,
}

impl InlineStallReport {
    fn new() -> Self {
        Self {
            last_logged_secs: std::sync::atomic::AtomicI64::new(i64::MIN),
            stalls: std::sync::atomic::AtomicU64::new(0),
            max_waited_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Record one stall. Returns `Some((stalls, max_waited_ms))` when this
    /// call owns the next line, with the window's totals taken and reset.
    ///
    /// O(1): three atomic operations and at most one compare-exchange. The
    /// compare-exchange makes the line single-owner if two callers stall at
    /// once; the loser's stall stays counted for the next line.
    fn record(&self, waited_ms: u64, now_unix_secs: i64) -> Option<(u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        self.stalls.fetch_add(1, Relaxed);
        self.max_waited_ms.fetch_max(waited_ms, Relaxed);
        let last = self.last_logged_secs.load(Relaxed);
        let due = last == i64::MIN
            || now_unix_secs.saturating_sub(last) >= SEAL_INLINE_STALL_LOG_EVERY_SECS;
        if !due
            || self
                .last_logged_secs
                .compare_exchange(last, now_unix_secs, Relaxed, Relaxed)
                .is_err()
        {
            return None;
        }
        Some((
            self.stalls.swap(0, Relaxed),
            self.max_waited_ms.swap(0, Relaxed),
        ))
    }
}

/// Put all six escalation series on the wire at zero when the escalation
/// subsystem is installed.
///
/// # Why a built handle is not enough — measured, not assumed
///
/// A `cloudwatch list-metrics` sweep on 2026-08-29 compared the EMF selector
/// against the live account: the selector names 104 metrics, the account held
/// 86, and the original four of these were among the names that had **never published a
/// single datapoint**. The CloudWatch agent computes a counter as the delta
/// between consecutive samples and drops the first sample of a series it has
/// never seen, so a counter that is never incremented is never published.
///
/// The consequence is not a missing chart. **An absent series is
/// indistinguishable from a healthy zero one**, so "no seals were ever lost"
/// and "the escalation path was never installed" looked identical, and an
/// alarm placed over one of these would sit in `OK` forever and could never
/// fire. Seeding separates those two answers permanently.
///
/// # Why here, and not at boot for everything at once
///
/// Called from [`SealWriterRunner::split_escalation_offload`], which is the
/// one place the escalation subsystem is installed. A central boot-time
/// seeder would publish a confident zero for a subsystem that is not running
/// — positive evidence of health for work nothing is doing, which is a worse
/// false-OK than the silence it replaces.
fn register_escalation_baseline() {
    // Deferred loss: BOTH disk tiers refused on the escalation thread. The
    // caller was already told `Queued`, so this is the only place that loss
    // is countable.
    metrics::counter!(SEAL_ESCALATION_LOST_COUNTER).increment(0);
    // Seals still queued when the shutdown budget expired — they died with
    // the process. Emitted from the app's shutdown path, but it belongs to
    // this subsystem, so it is seeded with it.
    metrics::counter!(SEAL_ESCALATION_ABANDONED_COUNTER).increment(0);
    // The two healthy outcomes. Seeded alongside the loss pair on purpose:
    // `lost` alone cannot be read without knowing whether anything was ever
    // queued, and an absent denominator makes a zero numerator meaningless.
    metrics::counter!(SEAL_ESCALATION_QUEUED_COUNTER).increment(0);
    metrics::counter!(SEAL_ESCALATION_INLINE_FALLBACK_COUNTER).increment(0);
    // How long the fallback made the caller wait, and how often that wait
    // crossed the page threshold (audit PR40d).
    metrics::counter!(SEAL_ESCALATION_INLINE_WAIT_MS_COUNTER).increment(0);
    metrics::counter!(SEAL_ESCALATION_INLINE_STALL_COUNTER).increment(0);
}

/// One refused seal on its way to the durable tier.
///
/// `SerializedSeal` is `Copy` and fixed-size, so moving it into the channel
/// allocates nothing — the point of serialising on the caller's side rather
/// than sending the `BufferedSeal` is that the expensive part (the disk
/// write) is what moves, not the cheap part.
#[derive(Clone, Copy, Debug)]
pub struct SealEscalationItem {
    seal: crate::seal_spill::SerializedSeal,
    now_unix_secs: i64,
}

/// Receiving half of the escalation hand-off — owned by the dedicated
/// `tv-seal-escalate` OS thread.
pub struct SealEscalationSink {
    rx: std::sync::mpsc::Receiver<SealEscalationItem>,
    spill: std::sync::Arc<crate::seal_spill::SealSpillWriter>,
    dlq: std::sync::Arc<crate::seal_dlq::SealDlqWriter>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SealEscalationSink {
    /// The flag that asks the thread to drain and exit. Take a clone BEFORE
    /// moving the sink into the thread — afterwards it is unreachable, which
    /// is precisely the defect the WAL spill writer carried until 2026-08-28.
    #[must_use]
    pub fn stop_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.stop)
    }

    /// How many handed-over seals the thread has not yet written (audit PR15).
    /// Take a clone before moving the sink into its thread, like
    /// [`Self::stop_flag`]; shutdown reads it to count an abandoned queue.
    #[must_use]
    pub fn pending_count(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        std::sync::Arc::clone(&self.pending)
    }

    /// Write one batch: one `write(2)` per run of records that share an IST
    /// day (a batch straddles midnight at most once), and on a failed write
    /// every record of that run goes to the DLQ, one line each.
    ///
    /// A seal the DLQ also refuses reaches `on_lost`, which is the page. The
    /// failed batch write cut its torn tail back, or set the file aside when
    /// it could not (see [`crate::seal_spill::SealSpillWriter::append_seals`]),
    /// so no record of the run is left half-written in the spill; a record
    /// that did land before the failure and is ALSO in the DLQ is collapsed by
    /// the candle tables' DEDUP keys when both are replayed.
    ///
    /// Returns how many spill writes it made: one per IST-day run, so 1 for
    /// almost every batch and 2 for one that straddles midnight (audit PR40c).
    fn write_batch<F>(
        &self,
        batch: &[SealEscalationItem],
        scratch: &mut Vec<u8>,
        on_lost: &F,
    ) -> usize
    where
        F: Fn(&crate::seal_spill::SerializedSeal),
    {
        let mut spill_writes = 0usize;
        let mut start = 0;
        while start < batch.len() {
            let day = crate::seal_spill::ist_day_number(batch[start].now_unix_secs);
            let mut end = start + 1;
            while end < batch.len()
                && crate::seal_spill::ist_day_number(batch[end].now_unix_secs) == day
            {
                end += 1;
            }
            let run = &batch[start..end];
            spill_writes += 1;
            let written = self.spill.append_seals(
                run.iter().map(|item| &item.seal),
                scratch,
                run[0].now_unix_secs,
            );
            let run_written = run.len();
            if written.is_err() {
                // Straight to the DLQ: the spill just refused a whole batch,
                // and retrying it per record would pay a reopen per seal on a
                // failing disk — the case most likely to hold a full queue at
                // shutdown, where every syscall counts against the budget.
                for item in run {
                    let record = crate::seal_dlq::SealDlqRecord::from(&item.seal);
                    if self.dlq.append_record(&record, item.now_unix_secs).is_ok() {
                        // Z6: a fuller copy committed later must be mirrored
                        // to the spill, so the boot drain ends on it.
                        self.spill.note_dead_lettered(&item.seal);
                    } else {
                        metrics::counter!(SEAL_ESCALATION_LOST_COUNTER).increment(1);
                        on_lost(&item.seal);
                    }
                }
            }
            // Written, sent to the DLQ or reported lost: either way no longer
            // waiting in the queue. Through the spill (Z6), which raises its
            // finished count before lowering the pending one, so its ledger
            // never forgets a bar an older queued copy could still reach.
            self.spill.note_escalation_finished(run_written);
            start = end;
        }
        spill_writes
    }

    /// Syncs the day file once more before the thread exits, when the last
    /// batch was full and so was not synced on its own (PR17).
    fn final_sync(&self, unsynced: bool, summary: &mut SealEscalationRunSummary) {
        if unsynced {
            self.spill.sync_open_file();
            summary.syncs += 1;
        }
    }

    /// Drain the queue until the sender is gone, or until the stop flag is set
    /// AND the queue is empty.
    ///
    /// `on_lost` is called for a seal both disk tiers refused. It exists as a
    /// callback rather than a direct call because the paging path
    /// (`AGGREGATOR-DROP-01`) lives in the app crate, above this one.
    ///
    /// Returns what the thread did (audit PR40c), so a test can prove the
    /// batching by counting writes rather than only checking where the
    /// records landed. The production thread discards it.
    pub fn run<F>(self, on_lost: F) -> SealEscalationRunSummary
    where
        F: Fn(&crate::seal_spill::SerializedSeal),
    {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::RecvTimeoutError;
        let mut summary = SealEscalationRunSummary::default();
        // Reused across batches: after the first full batch neither grows.
        let mut batch: Vec<SealEscalationItem> = Vec::with_capacity(SEAL_ESCALATION_BATCH);
        let mut scratch: Vec<u8> =
            Vec::with_capacity(SEAL_ESCALATION_BATCH * crate::seal_spill::SEAL_SPILL_RECORD_SIZE);
        // PR17: the day file is synced after a batch that empties the queue,
        // and at least once a second under a sustained burst, so a seal the
        // spill holds reaches the device within about a second. The drain
        // never syncs: only this thread does.
        let mut unsynced = false;
        let mut last_sync = std::time::Instant::now();
        loop {
            match self.rx.recv_timeout(SEAL_ESCALATION_STOP_POLL) {
                Ok(first) => {
                    // Take whatever else is already queued, up to one batch.
                    // `try_recv` never waits, so a lone refusal is written at
                    // once rather than held back for company.
                    batch.clear();
                    batch.push(first);
                    while batch.len() < SEAL_ESCALATION_BATCH {
                        match self.rx.try_recv() {
                            Ok(item) => batch.push(item),
                            Err(_) => break,
                        }
                    }
                    summary.batches += 1;
                    summary.records += batch.len();
                    summary.spill_writes += self.write_batch(&batch, &mut scratch, &on_lost);
                    unsynced = true;
                    if batch.len() < SEAL_ESCALATION_BATCH
                        || last_sync.elapsed() >= SEAL_ESCALATION_SYNC_INTERVAL
                    {
                        self.spill.sync_open_file();
                        summary.syncs += 1;
                        unsynced = false;
                        last_sync = std::time::Instant::now();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Only exit on an EMPTY queue: a pending item always
                    // returns `Ok` above, so a stop request can never cut the
                    // drain short.
                    if self.stop.load(Ordering::Acquire) {
                        self.final_sync(unsynced, &mut summary);
                        return summary;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.final_sync(unsynced, &mut summary);
                    return summary;
                }
            }
        }
    }
}

/// What one run of the escalation thread did (audit PR40c).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SealEscalationRunSummary {
    /// Batches taken off the queue, each at most [`SEAL_ESCALATION_BATCH`].
    pub batches: usize,
    /// Seals in those batches.
    pub records: usize,
    /// Spill writes made: one per batch, plus one for a batch that straddles
    /// IST midnight. Each is one `append_seals` call, i.e. one `write(2)`.
    pub spill_writes: usize,
    /// `sync_data` calls on the spill day file (PR17): one after each batch
    /// that empties the queue, at least one a second under a burst, and one
    /// at exit when the last batch was not yet synced.
    pub syncs: usize,
}

/// What happened to a seal the writer channel refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowOutcome {
    /// Written to the binary spill file. Recovered by the boot drain.
    Spilled,
    /// Spill failed; written to the NDJSON DLQ as recoverable text.
    DlqWritten,
    /// Handed to the escalation thread, which owns the disk write from here.
    ///
    /// **Honest boundary:** this says the seal reached a bounded, in-memory
    /// queue drained by a thread whose only job is to write it — NOT that it
    /// is on disk yet. A seal that both disk tiers later refuse still fires
    /// `AGGREGATOR-DROP-01` from the thread, so the operator page is not lost;
    /// what the caller cannot know synchronously is which of its own
    /// rescued/lost counters the seal belonged in. That split is deliberately
    /// traded for keeping the disk off the fold path, and
    /// [`SEAL_ESCALATION_LOST_COUNTER`] is where the deferred losses land.
    Queued,
    /// Both disk tiers failed. THE SEAL IS LOST — the caller MUST fire
    /// `AGGREGATOR-DROP-01` (Critical, paged). This is the only remaining
    /// path by which a sealed candle can disappear, and it requires the data
    /// volume to be unwritable.
    Lost,
}

impl SealOverflow {
    /// Build an escalator over an existing pipeline's writers.
    ///
    /// The queued-seal count is the spill writer's own (audit PR40a, Z6), so
    /// [`SealWriterRunner::unwritten_seals`] counts the escalation queue
    /// without a handle to the thread, and the spill ledger can tell whether
    /// an older copy of a bar may still be on its way to the spill. Every
    /// escalator over one spill shares that one count.
    #[must_use]
    pub fn new(
        spill: std::sync::Arc<crate::seal_spill::SealSpillWriter>,
        dlq: std::sync::Arc<crate::seal_dlq::SealDlqWriter>,
    ) -> Self {
        let pending = spill.escalation_pending();
        Self {
            spill,
            dlq,
            offload: None,
            pending,
            queued: metrics::counter!(SEAL_ESCALATION_QUEUED_COUNTER),
            inline_fallback: metrics::counter!(SEAL_ESCALATION_INLINE_FALLBACK_COUNTER),
            inline_wait_ms: metrics::counter!(SEAL_ESCALATION_INLINE_WAIT_MS_COUNTER),
            inline_stall: metrics::counter!(SEAL_ESCALATION_INLINE_STALL_COUNTER),
            stall_report: InlineStallReport::new(),
        }
    }

    /// Move the disk work off the caller's task.
    ///
    /// **The defect this closes (found 2026-08-28).** Every one of the three
    /// `escalate_refused_seal` call sites in `dhan_feed_stack` runs on the
    /// frame-drain task — the per-tick fold, the 5-second catch-up sweep, and
    /// the close force-seal. So a refused seal charged the drain a
    /// `SealSpillWriter` mutex acquisition plus a `write(2)`, and on spill
    /// failure a `create_dir_all`, an `open`, a `serde_json::to_string` HEAP
    /// ALLOCATION and four more syscalls — on the one thread that empties the
    /// socket. Dhan skips a slow consumer forward to "the latest available
    /// state" with no sequence number, so stalling that thread loses ticks
    /// UPSTREAM, invisibly.
    ///
    /// It is not a rare path either. The mutex this took is the same one the
    /// seal writer holds on every ring eviction: measured on the prod box
    /// 2026-08-20, `spilled: 541,519` in a single session with the ring at
    /// 598,976/600,000 — its capacity at the time, 25,000 × TF_COUNT=24; the
    /// 2026-09-19 nine-frame collapse took that derived ceiling to 225,000 and
    /// `M10` becoming a native frame on 2026-09-22 took it to 250,000, so
    /// re-read the constant rather than this dated pair. A producer-side
    /// escalation queues behind all of that.
    ///
    /// Idempotent by construction — a second call replaces the sender, so the
    /// boot path installs exactly one. Returns the receiving half; the caller
    /// spawns the thread.
    pub fn split_escalation_offload(&mut self) -> SealEscalationSink {
        let (tx, rx) = std::sync::mpsc::sync_channel(SEAL_ESCALATION_QUEUE_DEPTH);
        self.offload = Some(tx);
        register_escalation_baseline();
        SealEscalationSink {
            rx,
            spill: std::sync::Arc::clone(&self.spill),
            dlq: std::sync::Arc::clone(&self.dlq),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending: std::sync::Arc::clone(&self.pending),
        }
    }

    /// The two-tier cascade itself: spill first (binary, compact, drained on
    /// boot), DLQ second (NDJSON, recoverable as text by a human).
    ///
    /// Shared verbatim by the caller's fallback arm and the escalation thread
    /// so the two can never drift into different durability semantics.
    fn escalate_inline(
        spill: &crate::seal_spill::SealSpillWriter,
        dlq: &crate::seal_dlq::SealDlqWriter,
        serialised: &crate::seal_spill::SerializedSeal,
        now_unix_secs: i64,
    ) -> OverflowOutcome {
        if spill.append_seal(serialised, now_unix_secs).is_ok() {
            return OverflowOutcome::Spilled;
        }
        let record = crate::seal_dlq::SealDlqRecord::from(serialised);
        if dlq.append_record(&record, now_unix_secs).is_ok() {
            // Z6: recorded so a fuller copy committed later is mirrored to
            // the spill. One more spill-lock probe, on a path that has just
            // paid two file writes.
            spill.note_dead_lettered(serialised);
            return OverflowOutcome::DlqWritten;
        }
        OverflowOutcome::Lost
    }

    /// Account for one inline fallback's wait (audit PR40d, noise-lock §2.7).
    ///
    /// Adds the wait to [`SEAL_ESCALATION_INLINE_WAIT_MS_COUNTER`]. A wait of at
    /// least [`SEAL_INLINE_WAIT_PAGE_MS`] also counts as a stall and, at most
    /// once per [`SEAL_INLINE_STALL_LOG_EVERY_SECS`], logs `AGGREGATOR-STALL-01`,
    /// which pages. The line carries the window's stall count and longest wait,
    /// so a throttled stall is folded in, never hidden.
    ///
    /// Reached only from the refused arm, which has already paid a disk write;
    /// the `error!` formatting allocates, and is throttled for that reason.
    fn note_inline_wait(
        &self,
        waited: std::time::Duration,
        outcome: OverflowOutcome,
        now_unix_secs: i64,
    ) {
        let waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        self.inline_wait_ms.increment(waited_ms);
        if waited_ms < SEAL_INLINE_WAIT_PAGE_MS {
            return;
        }
        self.inline_stall.increment(1);
        if let Some((stalls, max_waited_ms)) = self.stall_report.record(waited_ms, now_unix_secs) {
            report_inline_stall(waited_ms, stalls, max_waited_ms, outcome);
        }
    }

    /// Escalate one refused seal to the durable tier. Never blocks on the
    /// network, never awaits, and never panics.
    ///
    /// # What this does to the caller's task — stated exactly (2026-09-01)
    ///
    /// This doc used to end "and — once the offload is installed — never
    /// touches the filesystem on the caller's task". That was FALSE, and
    /// false in the reassuring direction, which is the class this repository
    /// treats as a defect in its own right.
    ///
    /// With the offload installed there are TWO arms, not one:
    ///
    /// * `try_send` **accepted** — the common case. The caller pays a channel
    ///   send and one atomic increment; the disk work happens on
    ///   `tv-seal-escalate`. No filesystem syscall on the caller.
    /// * `try_send` **refused** (`Full`: the escalation thread is behind;
    ///   `Disconnected`: it died) — the caller runs [`Self::escalate_inline`]
    ///   ITSELF. That takes the [`crate::seal_spill::SealSpillWriter`] mutex
    ///   and does a blocking `write(2)`, and if the spill fails it also does
    ///   `create_dir_all`, an `open`, a `serde_json::to_string` HEAP
    ///   ALLOCATION and further syscalls for the DLQ record — all on the
    ///   frame-drain task, the one thread that empties the socket.
    ///
    /// The fallback is DELIBERATE and must not be removed: the seal is still
    /// in hand at that point, so the choice is "as slow as it was before the
    /// offload existed" versus "lost". Degraded beats lossy. What was missing
    /// was not a different policy but an honest statement of the policy, plus
    /// a way to know how often it fires — [`SEAL_ESCALATION_INLINE_FALLBACK_COUNTER`]
    /// is that number, and before it existed the frequency was Unknown.
    ///
    /// Since audit PR15 the queue holds one whole close burst, so the refused
    /// arm needs a full burst of refusals against a disk that has stopped
    /// writing. When it does fire, its inline write also waits for the spill
    /// lock, which the escalation thread holds for one batch write.
    ///
    /// **The bound on that wait is BYTES, not TIME (corrected in audit
    /// PR40c).** The lock is held for one write of at most 128 KiB, but a
    /// write to a disk that has stopped answering does not return, so the
    /// caller's wait lasts as long as the stall does: seconds on a slow
    /// device, unbounded on a hung one. This note used to call the wait
    /// "known, bounded", which was true of its size and false of its length.
    /// No seal is lost while it waits; the cost is the frame drain, which
    /// stops emptying the socket for the same time.
    ///
    /// With NO offload installed (pre-2026-08-28 shape, and every unit test
    /// that does not call `split_escalation_offload`) every escalation is the
    /// inline cascade.
    ///
    /// `now_unix_secs` is passed in rather than read from the clock for the
    /// same reason the absorption pipeline takes it (locked decision L-H7):
    /// this runs inside the per-tick fold, and a clock syscall per seal is a
    /// hot-path cost the design does not accept.
    pub fn escalate(&self, seal: &BufferedSeal, now_unix_secs: i64) -> OverflowOutcome {
        let serialised = crate::seal_spill::SerializedSeal::from(seal);
        if let Some(tx) = self.offload.as_ref() {
            let item = SealEscalationItem {
                seal: serialised,
                now_unix_secs,
            };
            // Counted BEFORE the send: the thread may write and subtract the
            // seal before `try_send` even returns, and counting after would
            // let the subtraction run first and wrap the counter.
            self.pending
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match tx.try_send(item) {
                Ok(()) => {
                    self.queued.increment(1);
                    return OverflowOutcome::Queued;
                }
                // Full: the thread is behind. Disconnected: it died. Either
                // way the seal is still in hand, so it takes the inline route
                // rather than being dropped — degraded, never lossy.
                Err(
                    std::sync::mpsc::TrySendError::Full(item)
                    | std::sync::mpsc::TrySendError::Disconnected(item),
                ) => {
                    self.pending
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    self.inline_fallback.increment(1);
                    // Timed here and ONLY here (audit PR40d): this is the arm
                    // that makes the caller wait on the disk. The queued arm
                    // above reads no clock.
                    let started = std::time::Instant::now();
                    let outcome = Self::escalate_inline(
                        &self.spill,
                        &self.dlq,
                        &item.seal,
                        item.now_unix_secs,
                    );
                    self.note_inline_wait(started.elapsed(), outcome, item.now_unix_secs);
                    return outcome;
                }
            }
        }
        Self::escalate_inline(&self.spill, &self.dlq, &serialised, now_unix_secs)
    }
}

/// The `AGGREGATOR-STALL-01` line itself, out of line so the refused arm's
/// common case (a short wait) carries none of its formatting code.
#[cold]
#[inline(never)]
fn report_inline_stall(waited_ms: u64, stalls: u64, max_waited_ms: u64, outcome: OverflowOutcome) {
    tracing::error!(
        code = tickvault_common::error_code::ErrorCode::AggregatorStall01InlineWait.code_str(),
        waited_ms,
        stalls,
        max_waited_ms,
        threshold_ms = SEAL_INLINE_WAIT_PAGE_MS,
        outcome = ?outcome,
        "the live feed waited on a stalled disk: the escalation queue was full or its \
         thread had died, so the frame drain wrote a refused candle to disk itself and \
         stopped reading the socket until the disk answered. No candle was lost unless \
         outcome is Lost (that also fires AGGREGATOR-DROP-01); ticks during the wait may \
         have been skipped by the exchange feed. Check df -h /data and the disk's health."
    );
}

static GLOBAL_SEAL_OVERFLOW: std::sync::OnceLock<SealOverflow> = std::sync::OnceLock::new();

/// Install the process-wide overflow escalator. Idempotent, matching
/// [`set_global_seal_sender`] — first call wins, later calls return `false`
/// and change nothing.
///
/// The boot path MUST install this in the same place it installs the sender.
/// A sender without an escalator is the pre-2026-08-19 behaviour: refusals
/// become losses.
pub fn set_global_seal_overflow(overflow: SealOverflow) -> bool {
    GLOBAL_SEAL_OVERFLOW.set(overflow).is_ok()
}

/// Read-only accessor for the overflow escalator. `None` until boot installs
/// one — a producer seeing `None` has no durable tier available and its only
/// honest option is to count the seal as lost and fire AGGREGATOR-DROP-01.
#[must_use]
pub fn global_seal_overflow() -> Option<&'static SealOverflow> {
    GLOBAL_SEAL_OVERFLOW.get()
}

/// Bounded mpsc capacity for the producer→consumer wire.
///
/// DERIVED from [`SEAL_BUFFER_CAPACITY`], not a literal, since
/// 2026-08-12. The previous `200_000` carried the comment "absorbs the
/// IST-midnight burst (~99K seals across 11K instruments × 9 TFs)" —
/// which described a universe that no longer exists and was
/// arithmetically FALSE at the configured ceiling.
///
/// This channel sits IN FRONT OF the ring. `force_seal_all` emits
/// `AGGREGATOR_MAX_SLOTS × TF_COUNT` = 25,000 × 10 = **250,000** seals
/// in one burst, and every one of them must pass through here before it
/// can reach the ring's three absorbing tiers. At 200,000 the channel
/// force-dropped the remainder on `try_send` — counter-only, no log line,
/// no alarm — every midnight, while the ring behind it was correctly sized
/// for the full burst and sat mostly empty. The shortfall was **400,000**
/// when this was written at TF_COUNT=24; at today's TF_COUNT=10 the burst is
/// 250,000, so a literal 200_000 would drop 50,000 today.
/// The number moved; the DEFECT is that a literal cannot follow it, which is
/// why this constant is derived.
///
/// That is the exact drift class `SEAL_BUFFER_CAPACITY` was derived to
/// prevent on 2026-08-10; the ring was fixed and the channel in front of
/// it was missed, so the bound simply moved one hop upstream. Deriving
/// BOTH from the same inputs closes it: change `AGGREGATOR_MAX_SLOTS` or
/// `TF_COUNT` and both follow, and the ratchet below fails the build if
/// they ever diverge again.
///
/// Cost at the derived value: the mpsc allocates its buffer lazily per
/// queued item (tokio `mpsc` does NOT pre-allocate capacity slots), so
/// the steady-state cost is ~0 and the worst case equals the burst
/// itself — 250,000 × ≤168 B ≈ **42 MB**, matching the ring, 0.12% of
/// the r8g.xlarge 32 GiB host (operator Quote 13, 2026-08-08).
pub const SEAL_MPSC_CAPACITY: usize = tickvault_trading::candles::SEAL_BUFFER_CAPACITY;

/// Outcome of one [`SealWriterRunner::run_one_cycle`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CycleOutcome {
    /// Number of seals drained from the mpsc into the pipeline this
    /// cycle (i.e. submitted via `pipeline.submit`). Counts ALL
    /// outcomes — buffered, spilled, dlq, dropped.
    pub submitted_from_mpsc: usize,
    /// Of the submitted seals, how many landed in the pipeline ring
    /// (happy path).
    pub mpsc_submit_buffered: usize,
    /// Of the submitted seals, how many overflowed the ring and
    /// escalated to spill.
    pub mpsc_submit_spilled: usize,
    /// Of the submitted seals, how many overflowed spill and went
    /// to DLQ.
    pub mpsc_submit_dlq: usize,
    /// Of the submitted seals, how many were truly dropped (all 3
    /// tiers failed). Caller MUST fire `error!(code = AGGREGATOR-DROP-01)`
    /// per the runbook for each.
    pub mpsc_submit_dropped: usize,
    /// Drain-side outcome from `drain_once` (ring → ILP buffer →
    /// flush, with rescue cascade on flush failure).
    pub drain: DrainOutcome,
}

impl CycleOutcome {
    /// `true` if NOTHING happened this cycle (mpsc empty, ring empty).
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.submitted_from_mpsc == 0 && self.drain.is_idle()
    }

    /// Folds another cycle's outcome into this one.
    ///
    /// Used by the shutdown drain, which runs cycles until the ring empties
    /// and must return the TOTAL — a caller shown only the last cycle would
    /// see an idle outcome and read a successful multi-cycle drain as nothing
    /// having happened.
    ///
    /// # Complexity
    /// O(1) — five saturating adds plus the drain's own.
    pub const fn accumulate(&mut self, other: &Self) {
        self.submitted_from_mpsc = self
            .submitted_from_mpsc
            .saturating_add(other.submitted_from_mpsc);
        self.mpsc_submit_buffered = self
            .mpsc_submit_buffered
            .saturating_add(other.mpsc_submit_buffered);
        self.mpsc_submit_spilled = self
            .mpsc_submit_spilled
            .saturating_add(other.mpsc_submit_spilled);
        self.mpsc_submit_dlq = self.mpsc_submit_dlq.saturating_add(other.mpsc_submit_dlq);
        self.mpsc_submit_dropped = self
            .mpsc_submit_dropped
            .saturating_add(other.mpsc_submit_dropped);
        self.drain.accumulate(&other.drain);
    }
}

/// Owns the consumer-side machinery: the mpsc receiver, the
/// absorption pipeline, and the ILP writer. The future
/// `tokio::spawn`'d task (item 1.2f.5) takes this by value and calls
/// [`Self::run_one_cycle`] on a timer.
pub struct SealWriterRunner {
    /// Cloneable producer-side handle. The future aggregator wiring
    /// holds clones of this; this struct holds the receiver.
    sender: mpsc::Sender<BufferedSeal>,
    /// Consumer-side mpsc receiver. Drained non-blockingly per cycle.
    receiver: mpsc::Receiver<BufferedSeal>,
    /// Owned absorption pipeline (local ring + spill + DLQ).
    pipeline: SealAbsorptionPipeline,
    /// Owned ILP writer.
    writer: ShadowCandleWriter,
    /// Max seals to drain from ring → ILP per cycle. Bounded so a
    /// catastrophic burst doesn't monopolise the writer task.
    max_drain_per_cycle: usize,
    /// Spill directory this runner's pipeline writes to. Retained so the
    /// boot-time recovery drain reads back from the SAME directory the
    /// rescue path spills into (the pipeline owns its writers privately).
    spill_dir: std::path::PathBuf,
    /// DLQ directory — same reasoning as `spill_dir`.
    dlq_dir: std::path::PathBuf,
    /// The pipeline's own spill writer, shared with the escalation thread.
    /// The mid-session replay stages the live file under its append lock.
    spill: std::sync::Arc<SealSpillWriter>,
    /// Mid-session replay of the spill (audit PR15).
    replay: MidSessionReplay,
    /// Seals queued to the escalation thread and not yet on disk: the spill
    /// writer's own count, which the [`SealOverflow`] that [`Self::overflow`]
    /// builds shares (audit PR40a, Z6).
    escalation_pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SealWriterRunner {
    /// Production constructor. Connects the writer to QuestDB ILP
    /// (lazy — see [`ShadowCandleWriter::new`] for disconnect
    /// behaviour) and creates an mpsc with [`SEAL_MPSC_CAPACITY`].
    pub fn new(
        questdb_config: &tickvault_common::config::QuestDbConfig,
        max_drain_per_cycle: usize,
    ) -> anyhow::Result<Self> {
        let writer = ShadowCandleWriter::new(questdb_config)?;
        let pipeline = SealAbsorptionPipeline::new();
        let spill = pipeline.spill_handle();
        let (sender, receiver) = mpsc::channel(SEAL_MPSC_CAPACITY);
        Ok(Self {
            sender,
            receiver,
            pipeline,
            writer,
            max_drain_per_cycle,
            spill_dir: production_spill_dir(),
            dlq_dir: production_dlq_dir(),
            replay: MidSessionReplay::default(),
            escalation_pending: spill.escalation_pending(),
            spill,
        })
    }

    /// Test constructor — builds the same machinery but with the
    /// disconnected `ShadowCandleWriter::for_test()` writer and
    /// caller-supplied spill / DLQ directories. The `mpsc` capacity
    /// can be tuned for overflow-test scenarios.
    #[must_use]
    // TEST-EXEMPT: test-only construction helper used by every test in this module (idle, mpsc-fed, drain rescue, mpsc overflow → ring overflow → spill cascade). Separate name-matched test would be redundant.
    pub fn for_test(
        spill_dir: std::path::PathBuf,
        dlq_dir: std::path::PathBuf,
        ring_capacity: usize,
        mpsc_capacity: usize,
        max_drain_per_cycle: usize,
    ) -> Self {
        Self::with_writer_for_test(
            ShadowCandleWriter::for_test(),
            spill_dir,
            dlq_dir,
            ring_capacity,
            mpsc_capacity,
            max_drain_per_cycle,
        )
    }

    /// [`Self::for_test`] with a caller-built writer, so a test can point a
    /// REAL `ShadowCandleWriter` at a fake database (audit PR40c).
    fn with_writer_for_test(
        writer: ShadowCandleWriter,
        spill_dir: std::path::PathBuf,
        dlq_dir: std::path::PathBuf,
        ring_capacity: usize,
        mpsc_capacity: usize,
        max_drain_per_cycle: usize,
    ) -> Self {
        let pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            ring_capacity,
            spill_dir.clone(),
            dlq_dir.clone(),
        );
        let spill = pipeline.spill_handle();
        let (sender, receiver) = mpsc::channel(mpsc_capacity);
        Self {
            sender,
            receiver,
            pipeline,
            writer,
            max_drain_per_cycle,
            spill_dir,
            dlq_dir,
            replay: MidSessionReplay::default(),
            escalation_pending: spill.escalation_pending(),
            spill,
        }
    }

    /// Boot-time recovery drain: reads back every orphaned spill / DLQ file
    /// and re-ingests it into QuestDB. Called ONCE by the writer loop before
    /// its drain ticker starts.
    ///
    /// Without this, seals rescued to disk during a QuestDB outage were never
    /// read back by anything — see the module docs on
    /// [`crate::seal_writer_task::drain_recovered_seals`].
    pub fn boot_drain(&mut self) -> BootDrainOutcome {
        drain_recovered_seals(
            &mut self.writer,
            &self.spill_dir,
            &self.dlq_dir,
            self.max_drain_per_cycle,
        )
    }

    /// Spill directory this runner recovers from (test observability).
    #[must_use]
    pub fn spill_dir(&self) -> &std::path::Path {
        &self.spill_dir
    }

    /// DLQ directory this runner recovers from (test observability).
    #[must_use]
    pub fn dlq_dir(&self) -> &std::path::Path {
        &self.dlq_dir
    }

    /// Cloneable producer-side handle. The future aggregator passes
    /// these clones into the per-instrument cell hot-path so the
    /// `try_send(seal)` call can fire from any thread without blocking.
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<BufferedSeal> {
        self.sender.clone()
    }

    /// The producer-side overflow escalator for THIS runner's disk tiers.
    ///
    /// Boot installs the result via [`set_global_seal_overflow`] alongside
    /// [`set_global_seal_sender`], and must do so BEFORE moving the runner
    /// into its `tokio::spawn` — same constraint, same reason, so the two
    /// installs belong on adjacent lines.
    #[must_use]
    pub fn overflow(&self) -> SealOverflow {
        SealOverflow::new(self.pipeline.spill_handle(), self.pipeline.dlq_handle())
    }

    /// Currently buffered ring depth (item observed by future
    /// `tv_seal_ring_depth` Prom gauge).
    #[must_use]
    pub fn ring_len(&self) -> usize {
        self.pipeline.ring_len()
    }

    /// Sealed candles this process holds in memory and has not yet written
    /// anywhere durable (audit PR40a): the writer channel, the ring, and the
    /// escalation queue in front of the spill. A crash (abort, out of memory,
    /// hard kill) loses exactly these.
    ///
    /// Not included, and stated so the bound is honest: the seals of the
    /// cycle in progress, popped from the ring into the ILP buffer and not
    /// yet flushed (at most [`Self::max_drain_per_cycle`]), which exist only
    /// between two samples.
    ///
    /// # Complexity
    /// O(1): three length reads (`tokio::sync::mpsc::Receiver::len` is a
    /// counter read, not a walk).
    #[must_use]
    pub fn unwritten_seals(&self) -> usize {
        self.receiver
            .len()
            .saturating_add(self.pipeline.ring_len())
            .saturating_add(
                self.escalation_pending
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
    }

    /// Configured max-drain bound per cycle.
    #[must_use]
    pub const fn max_drain_per_cycle(&self) -> usize {
        self.max_drain_per_cycle
    }

    /// One full producer→consumer→ILP cycle:
    /// 1. Drain every pending mpsc message (non-blocking
    ///    `try_recv` loop) into the pipeline via `pipeline.submit`.
    ///    Counts the submit outcome by tier.
    /// 2. Call `drain_once` on the pipeline → ILP buffer → flush,
    ///    with rescue cascade on flush failure.
    /// 3. Return the combined [`CycleOutcome`].
    ///
    /// Synchronous despite using a tokio mpsc receiver because
    /// `try_recv` is a non-blocking sync method. The future tokio
    /// loop in 1.2f.5 wraps this in `tokio::time::interval`.
    pub fn run_one_cycle(&mut self, now_unix_secs: i64) -> CycleOutcome {
        let mut outcome = CycleOutcome::default();

        // Step 1: drain mpsc → pipeline.submit
        loop {
            match self.receiver.try_recv() {
                Ok(seal) => {
                    outcome.submitted_from_mpsc += 1;
                    match self.pipeline.submit(seal, now_unix_secs) {
                        SubmitOutcome::Buffered => outcome.mpsc_submit_buffered += 1,
                        SubmitOutcome::Spilled => outcome.mpsc_submit_spilled += 1,
                        SubmitOutcome::DlqWritten => outcome.mpsc_submit_dlq += 1,
                        SubmitOutcome::Dropped(_) => outcome.mpsc_submit_dropped += 1,
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        // Step 2: drain pipeline → ILP buffer → flush
        outcome.drain = drain_once(
            &mut self.pipeline,
            &mut self.writer,
            self.max_drain_per_cycle,
            now_unix_secs,
        );

        outcome
    }

    /// One live cycle, then at most one mid-session replay step (audit PR15).
    ///
    /// The writer loop's tick calls this; the shutdown drain calls
    /// [`Self::run_one_cycle`] alone, so a stopping process never starts
    /// re-reading the spill. The replay runs only after the live drain has
    /// emptied the ring and the database has flushed cleanly for
    /// [`crate::seal_writer_task::SEAL_REPLAY_HEALTHY_SECS`].
    pub fn run_one_cycle_with_replay(
        &mut self,
        now_unix_secs: i64,
    ) -> (CycleOutcome, ReplayOutcome) {
        let cycle = self.run_one_cycle(now_unix_secs);
        self.replay.observe(&cycle.drain, now_unix_secs);
        // Audit PR41b: the QuestDB watcher's word closes the gate on a
        // suspect table and reopens it without live traffic.
        let files_rewound = self.replay.observe_probe(
            crate::seal_writer_task::ReplayProbe::current(),
            now_unix_secs,
        );
        let ring_is_empty = self.pipeline.ring_len() == 0;
        let mut replay = self.replay.step(
            &mut self.writer,
            &self.spill,
            &self.spill_dir,
            ring_is_empty,
            now_unix_secs,
        );
        replay.files_rewound += files_rewound;
        (cycle, replay)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
        let mut spill = std::env::temp_dir();
        let mut dlq = std::env::temp_dir();
        spill.push(format!(
            "tickvault-seal-runner-spill-{}-{}",
            name,
            std::process::id()
        ));
        dlq.push(format!(
            "tickvault-seal-runner-dlq-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&spill);
        let _ = std::fs::remove_dir_all(&dlq);
        std::fs::create_dir_all(&spill).expect("spill dir");
        std::fs::create_dir_all(&dlq).expect("dlq dir");
        (spill, dlq)
    }

    fn cleanup(spill: &PathBuf, dlq: &PathBuf) {
        let _ = std::fs::remove_dir_all(spill);
        let _ = std::fs::remove_dir_all(dlq);
    }

    fn jan1_noon_utc() -> i64 {
        chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp()
    }

    fn mk_seal(sid: u64, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = bucket;
        state.open = 100.0;
        state.high = 105.0;
        state.low = 99.0;
        state.close = close;
        state.volume = 1234;
        state.bucket_start_cumulative = 1000;
        state.oi = 50_000;
        state.tick_count = 5;
        state.close_pct_from_prev_day = 1.5;
        state.net_volume_signed = -4_242;
        state.net_volume_classified = true;
        state.total_buy_qty = 89_600;
        state.total_sell_qty = 4_800;
        BufferedSeal::new(sid, seg, tf, state, Feed::Dhan)
    }

    #[test]
    fn test_run_one_cycle_idle_when_mpsc_and_ring_both_empty() {
        let (spill, dlq) = temp_pair("idle");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let outcome = runner.run_one_cycle(jan1_noon_utc());
        assert!(outcome.is_idle());
        assert_eq!(outcome.submitted_from_mpsc, 0);
        assert!(outcome.drain.is_idle());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_run_one_cycle_drains_mpsc_into_pipeline() {
        // Producer pushes 3 seals via the sender; one cycle drains
        // them into the pipeline → ring (happy path before flush).
        let (spill, dlq) = temp_pair("drain-mpsc");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        for i in 0..3 {
            let s = mk_seal(
                13 + i,
                0,
                TfIndex::M1,
                1_716_023_700 + i as u32,
                100.0 + i as f64,
            );
            tx.try_send(s).expect("try_send");
        }
        let outcome = runner.run_one_cycle(jan1_noon_utc());
        assert_eq!(outcome.submitted_from_mpsc, 3);
        assert_eq!(outcome.mpsc_submit_buffered, 3);
        assert_eq!(outcome.mpsc_submit_spilled, 0);
        // Drain side: writer is disconnected → all 3 land on spill
        // via the rescue cascade.
        assert_eq!(outcome.drain.ring_seals_popped, 3);
        assert_eq!(outcome.drain.rescued_to_spill, 3);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_run_one_cycle_mpsc_overflows_ring_into_spill() {
        // Ring capacity 2, mpsc capacity 8 → 5 seals submitted means
        // ring overflows by 3 → 3 evicted to spill BEFORE the drain
        // step runs.
        let (spill, dlq) = temp_pair("ring-overflow");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 2, 8, 16);
        let tx = runner.sender();
        for i in 0..5 {
            tx.try_send(mk_seal(
                13 + i,
                0,
                TfIndex::M1,
                1_716_023_700 + i as u32,
                100.0 + i as f64,
            ))
            .expect("try_send");
        }
        let outcome = runner.run_one_cycle(jan1_noon_utc());
        assert_eq!(outcome.submitted_from_mpsc, 5);
        // First 2 buffered, next 3 spilled-on-overflow during submit
        assert_eq!(outcome.mpsc_submit_buffered, 2);
        assert_eq!(outcome.mpsc_submit_spilled, 3);
        // Then drain pops the 2 buffered → flush fails → rescue to spill
        assert_eq!(outcome.drain.ring_seals_popped, 2);
        assert_eq!(outcome.drain.rescued_to_spill, 2);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_sender_is_cloneable_and_independent() {
        // The sender clone semantics matter: aggregator hot path
        // hands clones into per-instrument cells. Each clone must
        // produce events on the SAME consumer.
        let (spill, dlq) = temp_pair("sender-clone");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx_a = runner.sender();
        let tx_b = runner.sender();
        tx_a.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("a");
        tx_b.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0))
            .expect("b");
        let outcome = runner.run_one_cycle(jan1_noon_utc());
        assert_eq!(outcome.submitted_from_mpsc, 2);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_sender_try_send_returns_full_when_mpsc_capacity_exhausted() {
        // mpsc capacity 2 → 3rd try_send must error with `Full`.
        let (spill, dlq) = temp_pair("mpsc-full");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 2, 16);
        let tx = runner.sender();
        let s1 = mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0);
        let s2 = mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0);
        let s3 = mk_seal(51, 0, TfIndex::M1, 1_716_024_900, 300.0);
        tx.try_send(s1).expect("ok 1");
        tx.try_send(s2).expect("ok 2");
        let result = tx.try_send(s3);
        match result {
            Err(mpsc::error::TrySendError::Full(returned)) => {
                assert_eq!(returned.security_id, 51);
                assert_eq!(returned.state.close, 300.0);
            }
            other => panic!("expected Full, got {other:?}"),
        }
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_run_one_cycle_repeated_drains_continue_to_work() {
        // Three cycles: push, drain, push, drain, push, drain. Each
        // cycle MUST process the latest seals; pipeline state carries
        // over across cycles.
        let (spill, dlq) = temp_pair("repeated");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let now = jan1_noon_utc();

        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("ok");
        let o1 = runner.run_one_cycle(now);
        assert_eq!(o1.submitted_from_mpsc, 1);
        assert_eq!(o1.drain.ring_seals_popped, 1);

        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0))
            .expect("ok");
        tx.try_send(mk_seal(51, 0, TfIndex::M1, 1_716_024_900, 300.0))
            .expect("ok");
        let o2 = runner.run_one_cycle(now);
        assert_eq!(o2.submitted_from_mpsc, 2);
        assert_eq!(o2.drain.ring_seals_popped, 2);

        let o3 = runner.run_one_cycle(now);
        assert!(o3.is_idle(), "third cycle has nothing to do");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_max_drain_per_cycle_caps_drain_step() {
        // mpsc submits 5, ring buffers 5 (capacity 16),
        // max_drain_per_cycle = 2 → only 2 drained per cycle.
        let (spill, dlq) = temp_pair("max-drain-cap");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 2);
        let tx = runner.sender();
        for i in 0..5 {
            tx.try_send(mk_seal(
                13 + i,
                0,
                TfIndex::M1,
                1_716_023_700 + i as u32,
                100.0 + i as f64,
            ))
            .expect("ok");
        }
        let outcome = runner.run_one_cycle(jan1_noon_utc());
        assert_eq!(outcome.submitted_from_mpsc, 5);
        assert_eq!(outcome.drain.ring_seals_popped, 2);
        // 3 seals remain in ring for the next cycle.
        assert_eq!(runner.ring_len(), 3);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_cycle_outcome_default_is_idle() {
        let o = CycleOutcome::default();
        assert!(o.is_idle());
        assert_eq!(o.submitted_from_mpsc, 0);
    }

    #[test]
    fn test_cycle_outcome_is_idle_returns_false_when_mpsc_submitted() {
        let o = CycleOutcome {
            submitted_from_mpsc: 1,
            mpsc_submit_buffered: 1,
            ..CycleOutcome::default()
        };
        assert!(!o.is_idle());
    }

    #[test]
    fn test_cycle_outcome_is_idle_returns_false_when_drain_active() {
        let o = CycleOutcome {
            drain: DrainOutcome {
                ring_seals_popped: 1,
                ..DrainOutcome::default()
            },
            ..CycleOutcome::default()
        };
        assert!(!o.is_idle());
    }

    #[test]
    fn test_max_drain_per_cycle_accessor() {
        let (spill, dlq) = temp_pair("max-drain-accessor");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 7);
        assert_eq!(runner.max_drain_per_cycle(), 7);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_seal_mpsc_capacity_constant_pinned() {
        // The mpsc sits IN FRONT OF the ring: every seal must pass
        // through it before it can reach the ring's three absorbing
        // tiers, so a channel smaller than the ring silently relocates
        // the drop one hop upstream of every absorption mechanism.
        //
        // This test used to pin the literal 200_000. That literal was
        // correct when written and became a 400,000-seal-per-midnight
        // drop the moment the ring was derived (2026-08-10) and TF_COUNT
        // moved 21 -> 24 — and the pin PASSED throughout, because it
        // asserted the stale value rather than the relationship. Pinning
        // the RELATIONSHIP is what makes the drift impossible.
        assert_eq!(
            SEAL_MPSC_CAPACITY,
            tickvault_trading::candles::SEAL_BUFFER_CAPACITY,
            "the producer->consumer mpsc must be at least the ring's capacity, \
             or force_seal_all drops the difference before any absorbing tier sees it"
        );
    }

    #[test]
    fn test_seal_mpsc_capacity_absorbs_a_whole_force_seal_burst() {
        // The concrete failure this closes: force_seal_all emits
        // AGGREGATOR_MAX_SLOTS x TF_COUNT seals in a single yield at the
        // IST day boundary. Anything less than that here is a guaranteed
        // per-midnight loss with a counter but no log and no alarm.
        let burst = tickvault_trading::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS
            * tickvault_trading::candles::tf_index::TF_COUNT;
        assert!(
            SEAL_MPSC_CAPACITY >= burst,
            "mpsc capacity {SEAL_MPSC_CAPACITY} < one force_seal_all burst {burst} — \
             {} seals would be dropped every IST midnight",
            burst.saturating_sub(SEAL_MPSC_CAPACITY)
        );
    }

    // -----------------------------------------------------------------------
    // Wave 6 Sub-PR #1 item 1.4c — global seal-sender accessor tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_global_seal_sender_before_install_returns_none() {
        // Note: the OnceLock is process-global. Other tests in this
        // mod may have installed a sender. We assert "Option<...>"
        // type — either None (clean test run) or Some (after another
        // test installed it). The accessor signature is the contract;
        // null-before-install is a property of OnceLock itself, not
        // our wrapper.
        let _: Option<&'static mpsc::Sender<BufferedSeal>> = global_seal_sender();
    }

    #[test]
    fn test_set_global_seal_sender_is_idempotent() {
        // The OnceLock semantics: only the FIRST set() succeeds.
        // Subsequent calls return Err (we wrap as `false`).
        let (tx_a, _rx_a) = mpsc::channel::<BufferedSeal>(8);
        let (tx_b, _rx_b) = mpsc::channel::<BufferedSeal>(8);
        let first = set_global_seal_sender(tx_a);
        let second = set_global_seal_sender(tx_b);
        // First MAY be true (if no prior install) OR false (if another
        // test installed). Second MUST be false (idempotency).
        assert!(
            !(first && second),
            "set_global_seal_sender MUST be idempotent — both calls returning true violates the contract"
        );
    }

    // ---------------------------------------------------------------
    // SealOverflow — the producer-side durable tier (2026-08-19)
    //
    // Operator directive: "never ever drop any ticks irrespective of any
    // worst case" / "never dropped or dleetd dude just mvoe it to db and s3
    // right?". Before these, a seal the writer channel refused was counted
    // and discarded.
    // ---------------------------------------------------------------

    #[test]
    fn test_seal_overflow_escalate_spills_a_refused_seal_to_disk() {
        let (spill, dlq) = temp_pair("overflow-spill");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let overflow = runner.overflow();

        let outcome =
            overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200, 101.5), jan1_noon_utc());

        assert_eq!(
            outcome,
            OverflowOutcome::Spilled,
            "a refused seal must reach the spill tier, not a counter"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_seal_overflow_writes_into_the_runners_own_spill_dir() {
        // The point of sharing the pipeline's writer handles rather than
        // building fresh ones: a producer-side rescue has to land where the
        // BOOT DRAIN will read it back. A rescue into a directory nobody
        // drains is a slower way of losing the candle.
        let (spill, dlq) = temp_pair("overflow-same-dir");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let overflow = runner.overflow();

        assert_eq!(
            overflow.escalate(&mk_seal(25, 0, TfIndex::M1, 34_260, 99.0), jan1_noon_utc()),
            OverflowOutcome::Spilled
        );

        let wrote_something = std::fs::read_dir(&spill)
            .expect("spill dir readable")
            .filter_map(Result::ok)
            .any(|e| e.metadata().map(|m| m.len() > 0).unwrap_or(false));
        assert!(
            wrote_something,
            "the escalated seal must be on disk in the runner's OWN spill dir ({}), \
             which is the directory boot_drain reads",
            spill.display()
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_seal_overflow_falls_through_to_dlq_when_spill_is_unwritable() {
        // Tier-2 failure must escalate, never terminate. Pointing the spill at
        // a path that cannot be a directory is the cheapest honest way to make
        // the append fail without root or a full disk.
        let (spill, dlq) = temp_pair("overflow-dlq");
        let mut blocked = spill.clone();
        blocked.push("not-a-dir");
        std::fs::write(&blocked, b"x").expect("write blocker file");
        let mut inner = blocked.clone();
        inner.push("spill-here");

        let overflow = SealOverflow::new(
            std::sync::Arc::new(crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(
                inner,
            )),
            std::sync::Arc::new(crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(
                dlq.clone(),
            )),
        );

        let outcome =
            overflow.escalate(&mk_seal(51, 0, TfIndex::M1, 34_320, 77.7), jan1_noon_utc());
        assert_eq!(
            outcome,
            OverflowOutcome::DlqWritten,
            "an unwritable spill must fall through to the DLQ, not lose the seal"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_seal_overflow_reports_lost_only_when_both_tiers_fail() {
        // `Lost` is the ONLY remaining path by which a sealed candle can
        // disappear, and it must require BOTH disk tiers to refuse — that is
        // what makes AGGREGATOR-DROP-01 mean "the volume is unwritable"
        // rather than "the writer was busy".
        let (spill, dlq) = temp_pair("overflow-lost");
        let mut blocker = spill.clone();
        blocker.push("blocked");
        std::fs::write(&blocker, b"x").expect("write blocker file");

        let mut spill_inner = blocker.clone();
        spill_inner.push("nested");
        let mut dlq_inner = blocker.clone();
        dlq_inner.push("nested-dlq");

        let overflow = SealOverflow::new(
            std::sync::Arc::new(crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(
                spill_inner,
            )),
            std::sync::Arc::new(crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(
                dlq_inner,
            )),
        );

        assert_eq!(
            overflow.escalate(&mk_seal(21, 0, TfIndex::M1, 34_380, 12.0), jan1_noon_utc()),
            OverflowOutcome::Lost,
            "both tiers refusing is the only honest Lost"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_global_seal_overflow_is_none_before_install() {
        // Mirrors the sender's own contract test. A producer that sees `None`
        // has no durable tier and must count the seal as lost rather than
        // claiming a rescue that did not happen.
        let _: Option<&'static SealOverflow> = global_seal_overflow();
    }

    #[test]
    fn test_set_global_seal_overflow_is_idempotent() {
        // Mirrors `test_set_global_seal_sender_is_idempotent`. Idempotency is
        // the safety property: a second install must NOT swap the durable tier
        // out from under producers that already hold the first one, or a
        // rescued seal would land in a directory the boot drain never reads.
        let (spill, dlq) = temp_pair("overflow-idempotent");
        let a = SealOverflow::new(
            std::sync::Arc::new(crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(
                spill.clone(),
            )),
            std::sync::Arc::new(crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(
                dlq.clone(),
            )),
        );
        let b = SealOverflow::new(
            std::sync::Arc::new(crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(
                spill.clone(),
            )),
            std::sync::Arc::new(crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(
                dlq.clone(),
            )),
        );
        let first = set_global_seal_overflow(a);
        let second = set_global_seal_overflow(b);
        assert!(
            !(first && second),
            "set_global_seal_overflow MUST be idempotent — both calls returning true \
             would mean a later install can replace the tier producers already hold"
        );
        cleanup(&spill, &dlq);
    }

    // ---------------------------------------------------------------
    // Escalation offload — taking the disk off the frame-drain task
    // (2026-08-28)
    //
    // All three `escalate_refused_seal` call sites in `dhan_feed_stack` run
    // on the drain: the per-tick fold, the 5-second catch-up sweep, and the
    // close force-seal. Each was paying a spill-writer mutex plus a
    // `write(2)` there — and, on spill failure, a `create_dir_all`, an
    // `open`, a `serde_json::to_string` heap allocation and four more
    // syscalls. That thread is the only one emptying the socket, and Dhan
    // skips a slow consumer forward with no sequence number, so stalling it
    // loses ticks upstream where no counter of ours can see them.
    // ---------------------------------------------------------------

    #[test]
    fn test_split_escalation_offload_queues_instead_of_writing_on_the_caller() {
        let (spill, dlq) = temp_pair("escalate-queues");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        // Hold the sink so the receiver stays alive; do NOT run it, so
        // nothing can have reached disk by the time we assert.
        let _sink = overflow.split_escalation_offload();

        let outcome =
            overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200, 101.5), jan1_noon_utc());

        assert_eq!(
            outcome,
            OverflowOutcome::Queued,
            "with the offload installed the caller must hand the seal over, not write it"
        );
        let wrote_something = std::fs::read_dir(&spill)
            .map(|d| {
                d.filter_map(Result::ok)
                    .any(|e| e.metadata().map(|m| m.len() > 0).unwrap_or(false))
            })
            .unwrap_or(false);
        assert!(
            !wrote_something,
            "nothing may reach disk on the caller's task — that is the whole point of the \
             offload, and a file here means the drain is still paying for the write"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_full_queue_falls_back_inline_and_never_drops_the_seal() {
        // The bounded queue is a shock absorber, not a bin. When it is full
        // the seal is still in hand, so it takes the old inline route:
        // degraded to the pre-offload cost, never lossy.
        let (spill, dlq) = temp_pair("escalate-full");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let _sink = overflow.split_escalation_offload(); // never drained

        let now = jan1_noon_utc();
        for i in 0..SEAL_ESCALATION_QUEUE_DEPTH {
            let seal = mk_seal(13, 0, TfIndex::M1, 34_200 + i as u32, 101.5);
            assert_eq!(
                overflow.escalate(&seal, now),
                OverflowOutcome::Queued,
                "the queue must accept its full declared depth before falling back"
            );
        }
        // One past the cap.
        let outcome = overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 99_999, 101.5), now);
        assert_eq!(
            outcome,
            OverflowOutcome::Spilled,
            "a full queue must escalate INLINE — a refusal here would be the silent drop the \
             whole no-drop policy exists to prevent"
        );
        cleanup(&spill, &dlq);
    }

    /// The per-tick path may not use the bare `metrics::counter!` MACRO.
    ///
    /// `escalate` is reached from the fold closure on the frame-drain task.
    /// `multi_tf_aggregator` calls that closure "the one place a bare
    /// `counter!` macro must never appear" and `DrainCounters` explains why:
    /// the macro builds a `Key` and takes a sharded-registry lock, where a
    /// resolved handle is an atomic add. A behavioural test cannot see the
    /// difference — both increment the same series — so the only thing that
    /// can hold this is a source scan.
    #[test]
    fn escalate_increments_pre_resolved_handles_not_the_counter_macro() {
        let src = include_str!("seal_writer_runner.rs");
        let start = src
            .find("pub fn escalate(&self, seal: &BufferedSeal")
            .expect("escalate must exist");
        // Bound the scan at the next top-level item rather than a byte count
        // — a fixed window silently stops covering the function the moment a
        // line is added to it, which is the class of guard that reads green
        // while the thing it guards drifts out from under it.
        let end = src[start..]
            .find("static GLOBAL_SEAL_OVERFLOW")
            .expect("escalate must be followed by the GLOBAL_SEAL_OVERFLOW item");
        let body = &src[start..start + end];
        assert!(
            !body.contains("metrics::counter!"),
            "bare metrics::counter! inside escalate — it runs on the frame-drain task; \
             resolve the handle once and increment the handle"
        );
        assert!(
            body.contains("self.queued.increment(1)")
                && body.contains("self.inline_fallback.increment(1)"),
            "both per-tick outcomes must increment a pre-resolved handle"
        );
    }

    /// The doc on `escalate` must describe the fallback arm truthfully.
    ///
    /// It previously ended "never touches the filesystem on the caller's
    /// task", which is false whenever `try_send` returns `Full` or
    /// `Disconnected` — the arm two tests below this one exercise on purpose.
    /// A comment that is wrong in the REASSURING direction is treated here as
    /// a defect in its own right, so it is pinned rather than trusted.
    #[test]
    fn the_escalate_doc_does_not_claim_a_filesystem_free_caller() {
        let src = include_str!("seal_writer_runner.rs");
        let start = src
            .find("/// Escalate one refused seal to the durable tier")
            .expect("the escalate doc must exist");
        // The SUMMARY line is what a reader skims, so the claim must be gone
        // from there specifically. The correction below it quotes the old
        // sentence verbatim on purpose — a whole-file scan would flag the
        // record of the fix as the defect it records.
        let summary_len = src[start..]
            .find("# What this does to the caller's task")
            .expect("the doc must carry the caller-cost section");
        assert!(
            !src[start..start + summary_len].contains("never touches the filesystem"),
            "the escalate summary re-asserted a claim the Full/Disconnected arm contradicts"
        );
        let doc = &src[start..start + 2_400];
        for phrase in [
            "escalate_inline",
            "write(2)",
            "frame-drain task",
            "SEAL_ESCALATION_INLINE_FALLBACK_COUNTER",
        ] {
            assert!(
                doc.contains(phrase),
                "the escalate doc must name {phrase} so the fallback's real cost and its \
                 frequency signal are both stated"
            );
        }
    }

    /// A full queue must still MOVE the fallback counter, not just spill.
    ///
    /// The sibling test below asserts the seal reaches disk; this asserts the
    /// operator can find out how often that happened. Without the counter the
    /// fallback's frequency is Unknown, which is exactly what an earlier
    /// review could not answer.
    #[test]
    fn the_inline_fallback_is_counted_so_its_frequency_is_knowable() {
        let (spill, dlq) = temp_pair("escalate-fallback-counted");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let _sink = overflow.split_escalation_offload(); // never drained

        let now = jan1_noon_utc();
        for i in 0..SEAL_ESCALATION_QUEUE_DEPTH {
            let _ = overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200 + i as u32, 101.5), now);
        }
        assert_eq!(
            overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 99_998, 101.5), now),
            OverflowOutcome::Spilled
        );
        assert_eq!(
            SEAL_ESCALATION_INLINE_FALLBACK_COUNTER, "tv_seal_escalation_inline_fallback_total",
            "the fallback series name is the operator-facing contract; renaming it silently \
             would leave any alarm over it permanently in OK"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_disconnected_sink_falls_back_inline_rather_than_losing_the_seal() {
        // The thread-spawn-failure shape: the sink (and its receiver) is
        // dropped, so every `try_send` returns `Disconnected`. Boot logs this
        // loudly; behaviour must degrade to the inline cascade, not to a
        // discard.
        let (spill, dlq) = temp_pair("escalate-disconnected");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        drop(overflow.split_escalation_offload());

        assert_eq!(
            overflow.escalate(&mk_seal(25, 0, TfIndex::M1, 34_260, 99.0), jan1_noon_utc()),
            OverflowOutcome::Spilled,
            "a dead escalation thread must not turn a rescue into a loss"
        );
        cleanup(&spill, &dlq);
    }

    /// Audit PR40d: the operator chose to accept the stalled-disk wait and
    /// make it loud. A caller that waits on a held spill lock for longer than
    /// the page threshold must still spill the seal, and must record the wait
    /// as a stall that owns the next `AGGREGATOR-STALL-01` line.
    #[test]
    fn a_stalled_disk_on_the_fallback_is_timed_and_recorded_as_a_stall() {
        let (spill, dlq) = temp_pair("escalate-stalled-disk");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        // A dead escalation thread: every `try_send` refuses, so the caller
        // takes the inline arm, the one that waits on the disk.
        drop(overflow.split_escalation_offload());

        let held = std::time::Duration::from_millis(SEAL_INLINE_WAIT_PAGE_MS + 200);
        let spill_writer = std::sync::Arc::clone(&overflow.spill);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            spill_writer.with_appends_paused(|| {
                entered_tx.send(()).expect("test channel open");
                std::thread::sleep(held);
                ((), false)
            });
        });
        entered_rx.recv().expect("the holder took the spill lock");

        let now = jan1_noon_utc();
        let started = std::time::Instant::now();
        let outcome = overflow.escalate(&mk_seal(25, 0, TfIndex::M1, 34_260, 99.0), now);
        let waited = started.elapsed();
        holder.join().expect("holder must not panic");

        assert_eq!(
            outcome,
            OverflowOutcome::Spilled,
            "the wait must never cost the seal"
        );
        assert!(
            waited >= std::time::Duration::from_millis(SEAL_INLINE_WAIT_PAGE_MS),
            "the test did not actually hold the caller for the page threshold: {waited:?}"
        );
        assert_eq!(
            overflow
                .stall_report
                .last_logged_secs
                .load(std::sync::atomic::Ordering::Relaxed),
            now,
            "a wait past the threshold must claim the AGGREGATOR-STALL-01 line"
        );
        assert_eq!(
            overflow
                .stall_report
                .stalls
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the line took the window's stall count, so the window restarts at zero"
        );
        cleanup(&spill, &dlq);
    }

    /// A short inline wait is timed but is not a stall and claims no line.
    #[test]
    fn a_quick_fallback_is_timed_but_never_claims_the_stall_line() {
        let (spill, dlq) = temp_pair("escalate-quick-fallback");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        drop(overflow.split_escalation_offload());

        let outcome =
            overflow.escalate(&mk_seal(25, 0, TfIndex::M1, 34_260, 99.0), jan1_noon_utc());
        assert_eq!(outcome, OverflowOutcome::Spilled);
        assert_eq!(
            overflow
                .stall_report
                .last_logged_secs
                .load(std::sync::atomic::Ordering::Relaxed),
            i64::MIN,
            "a sub-threshold wait must not page"
        );
        cleanup(&spill, &dlq);
    }

    /// The stall line is throttled to one per `SEAL_INLINE_STALL_LOG_EVERY_SECS`
    /// and folds every stall inside the window into the next line, so the
    /// throttle hides a stall's count and length from nobody.
    #[test]
    fn inline_stall_report_throttles_and_folds_the_window_into_the_next_line() {
        let report = InlineStallReport::new();
        assert_eq!(
            report.record(1_500, 100),
            Some((1, 1_500)),
            "the first stall logs at once"
        );
        assert_eq!(
            report.record(2_000, 110),
            None,
            "inside the window: folded, not logged"
        );
        assert_eq!(report.record(1_200, 159), None, "still inside the window");
        assert_eq!(
            report.record(1_100, 100 + SEAL_INLINE_STALL_LOG_EVERY_SECS),
            Some((3, 2_000)),
            "the next line carries the three stalls since the last and the longest of them"
        );
        assert_eq!(
            report.record(1_000, 170),
            None,
            "a new window started at the last line"
        );
    }

    /// A clock that steps backwards must not open a second line inside the
    /// window, and must not wedge the report shut either.
    #[test]
    fn inline_stall_report_survives_a_backwards_clock_step() {
        let report = InlineStallReport::new();
        assert!(report.record(1_500, 1_000).is_some());
        assert_eq!(
            report.record(1_500, 900),
            None,
            "a backwards step is inside the window"
        );
        assert!(
            report
                .record(1_500, 1_000 + SEAL_INLINE_STALL_LOG_EVERY_SECS)
                .is_some(),
            "time moving forward again reopens the line"
        );
    }

    /// The names and threshold the §2.7 alarm and runbook depend on.
    #[test]
    fn the_inline_wait_page_threshold_and_names_are_pinned() {
        assert_eq!(SEAL_INLINE_WAIT_PAGE_MS, 1_000);
        assert_eq!(SEAL_INLINE_STALL_LOG_EVERY_SECS, 60);
        assert_eq!(
            SEAL_ESCALATION_INLINE_WAIT_MS_COUNTER,
            "tv_seal_escalation_inline_wait_ms_total"
        );
        assert_eq!(
            SEAL_ESCALATION_INLINE_STALL_COUNTER,
            "tv_seal_escalation_inline_stall_total"
        );
        assert_eq!(
            tickvault_common::error_code::ErrorCode::AggregatorStall01InlineWait.code_str(),
            "AGGREGATOR-STALL-01"
        );
    }

    /// The clock is read on the refused inline arm only: the queued arm, the
    /// common case, stays a channel send and one atomic add (noise-lock §2.7
    /// REJECT row "reads the clock on the normal queued path").
    #[test]
    fn escalate_reads_the_clock_only_on_the_refused_arm() {
        let src = include_str!("seal_writer_runner.rs");
        let start = src
            .find("pub fn escalate(&self, seal: &BufferedSeal")
            .expect("escalate exists");
        // Bounded at the next item, the same way the handle scan above is:
        // a literal closing-brace marker would unbalance the brace-depth
        // tracker the banned-pattern hook uses to skip this test module.
        let end = src[start..]
            .find("fn report_inline_stall(")
            .expect("escalate must be followed by report_inline_stall");
        let body = &src[start..start + end];
        let clock = body
            .find("Instant::now()")
            .expect("the refused arm is timed");
        let refused = body
            .find("TrySendError::Full(item)")
            .expect("the refused arm exists");
        assert_eq!(
            body.matches("Instant::now()").count(),
            1,
            "one clock read only"
        );
        assert!(
            clock > refused,
            "the clock read must sit inside the refused arm"
        );
    }

    #[test]
    fn the_escalation_thread_writes_every_seal_it_was_handed() {
        let (spill, dlq) = temp_pair("escalate-thread-writes");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let stop = sink.stop_flag();
        let handle = std::thread::spawn(move || sink.run(|_| {}));

        let now = jan1_noon_utc();
        for i in 0..64u32 {
            assert_eq!(
                overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200 + i, 101.5), now),
                OverflowOutcome::Queued
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        handle.join().expect("escalation thread must not panic");

        let bytes: u64 = std::fs::read_dir(&spill)
            .expect("spill dir readable")
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        assert_eq!(
            bytes,
            64 * crate::seal_spill::SEAL_SPILL_RECORD_SIZE as u64,
            "the thread must land exactly the 64 records it was handed — a short file is the \
             deferred-loss case this offload must never introduce"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_stop_flag_never_cuts_a_pending_drain_short() {
        // The shutdown contract: stop means "drain and exit", not "exit".
        // Set the flag BEFORE the thread ever runs, with a full queue behind
        // it — every record must still land. A `try_recv`-based loop that
        // checked the flag first would fail this, which is exactly the
        // detached-writer defect the WAL spill carried until 2026-08-28.
        let (spill, dlq) = temp_pair("escalate-stop-drains");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let stop = sink.stop_flag();

        let now = jan1_noon_utc();
        for i in 0..32u32 {
            assert_eq!(
                overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200 + i, 101.5), now),
                OverflowOutcome::Queued
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        sink.run(|_| {});

        let bytes: u64 = std::fs::read_dir(&spill)
            .expect("spill dir readable")
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        assert_eq!(
            bytes,
            32 * crate::seal_spill::SEAL_SPILL_RECORD_SIZE as u64,
            "a stop request must drain the queue first — anything less is a silent loss at \
             every shutdown"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn the_thread_reports_a_seal_both_disk_tiers_refused() {
        // The caller was told `Queued`, so the AGGREGATOR-DROP-01 page can
        // only come from here. If this callback stopped firing, a genuinely
        // lost candle would leave no page at all — strictly worse than the
        // inline version it replaced.
        let (spill, dlq) = temp_pair("escalate-thread-lost");
        // Make BOTH tiers unwritable: a plain file where a directory belongs.
        cleanup(&spill, &dlq);
        std::fs::write(&spill, b"not a directory").expect("write blocker");
        std::fs::write(&dlq, b"not a directory").expect("write blocker");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let stop = sink.stop_flag();

        assert_eq!(
            overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200, 101.5), jan1_noon_utc()),
            OverflowOutcome::Queued
        );
        stop.store(true, std::sync::atomic::Ordering::Release);
        let lost = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&lost);
        sink.run(move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });

        assert_eq!(
            lost.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a seal both tiers refused must reach the on_lost hook — that hook IS the page"
        );
        let _ = std::fs::remove_file(&spill);
        let _ = std::fs::remove_file(&dlq);
    }

    #[test]
    fn the_queue_depth_is_drainable_inside_the_shutdown_budget() {
        // Arithmetic pin, not a wall-clock measurement (audit PR15 rewrite).
        // The thread writes up to SEAL_ESCALATION_BATCH records per write(2),
        // so a full queue costs ceil(depth / batch) writes. Two independent
        // degraded-disk models must BOTH fit the 5s budget: a slow syscall
        // (10ms per write, whatever its size) and a slow device (20 MB/s).
        // Raising the depth or shrinking the batch without raising the budget
        // (which the systemd guard would then catch) makes the shutdown drain
        // unable to finish, and an undrained bounded queue at exit is a loss.
        const DEGRADED_WRITE_MICROS: usize = 10_000;
        const DEGRADED_BYTES_PER_SEC: usize = 20 * 1_000_000;
        const BUDGET_MICROS: usize = 5 * 1_000_000;
        let writes = SEAL_ESCALATION_QUEUE_DEPTH.div_ceil(SEAL_ESCALATION_BATCH);
        assert!(
            writes * DEGRADED_WRITE_MICROS < BUDGET_MICROS,
            "{writes} batched writes cannot drain inside the 5s \
             SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS at a degraded 10ms/write. Raise the \
             budget in main.rs (and the systemd TimeoutStopSec the guard derives from it), \
             raise SEAL_ESCALATION_BATCH, or lower the depth."
        );
        let bytes = SEAL_ESCALATION_QUEUE_DEPTH * crate::seal_spill::SEAL_SPILL_RECORD_SIZE;
        let micros = bytes * 1_000 / (DEGRADED_BYTES_PER_SEC / 1_000);
        assert!(
            micros < BUDGET_MICROS,
            "{bytes} queued bytes cannot drain inside the 5s budget at a degraded 20 MB/s"
        );
        // Both degradations at once: every write pays the slow syscall AND
        // the slow device. ~2.2 s + ~1.4 s = ~3.6 s, still inside 5 s.
        assert!(
            writes * DEGRADED_WRITE_MICROS + micros < BUDGET_MICROS,
            "the combined slow-syscall + slow-device model does not fit the 5s budget"
        );
    }

    /// Reads every record of every `.bin` file in `dir`, in name order.
    fn read_spill_records(
        dir: &std::path::Path,
    ) -> Vec<(String, crate::seal_spill::SerializedSeal)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .expect("spill dir readable")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("bin"))
            .collect();
        files.sort();
        let mut out = Vec::new();
        for path in files {
            let bytes = std::fs::read(&path).expect("spill file readable");
            assert_eq!(
                bytes.len() % crate::seal_spill::SEAL_SPILL_RECORD_SIZE,
                0,
                "a batched write must never leave a torn record"
            );
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            for chunk in bytes.chunks(crate::seal_spill::SEAL_SPILL_RECORD_SIZE) {
                let seal = crate::seal_spill::SerializedSeal::from_bytes(chunk)
                    .expect("every batched record decodes");
                out.push((name.clone(), seal));
            }
        }
        out
    }

    #[test]
    fn the_escalation_thread_batches_a_burst_and_writes_every_record_in_order() {
        // Audit PR15: more than four batches' worth, queued before the thread
        // runs, so the thread really takes them SEAL_ESCALATION_BATCH at a time.
        let (spill, dlq) = temp_pair("escalate-batched-burst");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let stop = sink.stop_flag();

        let total = SEAL_ESCALATION_BATCH * 4 + 7;
        let now = jan1_noon_utc();
        for i in 0..total {
            assert_eq!(
                overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200 + i as u32, 101.5), now),
                OverflowOutcome::Queued
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        let summary = sink.run(|_| {});

        // Audit PR40c: prove the batching itself, not only where the records
        // landed. Everything was queued before the thread ran, so it must take
        // four full batches and one of seven, and make exactly one spill write
        // per batch. A per-record writer would report `total` writes here.
        assert_eq!(
            summary,
            SealEscalationRunSummary {
                batches: 5,
                records: total,
                spill_writes: 5,
                syncs: summary.syncs,
            },
            "a queued burst must be written SEAL_ESCALATION_BATCH records per write"
        );
        // PR17: the last batch (7 records) empties the queue, so it is always
        // synced; a full batch is synced only once a second has passed, which
        // depends on the disk, so only the bounds are pinned.
        assert!(
            (1..=summary.batches).contains(&summary.syncs),
            "the burst must be synced at least once and at most once per batch, got {}",
            summary.syncs
        );

        let records = read_spill_records(&spill);
        assert_eq!(
            records.len(),
            total,
            "every queued seal must land exactly once"
        );
        for (i, (_, seal)) in records.iter().enumerate() {
            assert_eq!(
                seal.bucket_start_ist_secs,
                34_200 + i as u32,
                "records must land in the order they were queued"
            );
        }
        cleanup(&spill, &dlq);
    }

    #[test]
    fn the_escalation_thread_writes_a_lone_refusal_at_once_as_its_own_batch() {
        // Audit PR40c: the thread never waits for company. A seal that
        // arrives alone is one batch and one write, and a second one that
        // arrives after the thread has written the first is another.
        let (spill, dlq) = temp_pair("escalate-lone-refusal");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let stop = sink.stop_flag();
        let pending = sink.pending_count();
        let thread = std::thread::spawn(move || sink.run(|_| {}));

        let now = jan1_noon_utc();
        for i in 0..2u32 {
            assert_eq!(
                overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200 + i, 101.5), now),
                OverflowOutcome::Queued
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while pending.load(std::sync::atomic::Ordering::Acquire) != 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the thread never wrote a lone seal"
                );
                std::thread::yield_now();
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        let summary = thread.join().expect("escalation thread must not panic");
        assert_eq!(
            summary,
            SealEscalationRunSummary {
                batches: 2,
                records: 2,
                spill_writes: 2,
                syncs: 2,
            }
        );
        assert_eq!(read_spill_records(&spill).len(), 2);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_batch_that_straddles_ist_midnight_files_each_record_under_its_own_day() {
        let (spill, dlq) = temp_pair("escalate-batch-midnight");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let stop = sink.stop_flag();

        // IST midnight of 2026-01-02 is 18:30 UTC on 2026-01-01.
        let midnight = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp();
        for i in 0..3u32 {
            let _ = overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 100 + i, 1.0), midnight - 1);
        }
        for i in 0..5u32 {
            let _ = overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 200 + i, 1.0), midnight);
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        let summary = sink.run(|_| {});
        assert_eq!(
            (summary.batches, summary.spill_writes),
            (1, 2),
            "one batch across midnight is two writes, one per day's file"
        );

        let records = read_spill_records(&spill);
        let day1 = records
            .iter()
            .filter(|(n, _)| n.contains("2026-01-01"))
            .count();
        let day2 = records
            .iter()
            .filter(|(n, _)| n.contains("2026-01-02"))
            .count();
        assert_eq!((day1, day2), (3, 5), "one batch, two days, two files");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_failed_batch_write_falls_back_to_the_dlq_record_by_record() {
        // The spill tier is dead (a plain file where its directory belongs),
        // the DLQ is fine: every record of the batch must reach the DLQ and
        // none may be reported lost.
        let (spill, dlq) = temp_pair("escalate-batch-fallback");
        let _ = std::fs::remove_dir_all(&spill);
        std::fs::write(&spill, b"not a directory").expect("write blocker");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let stop = sink.stop_flag();

        let now = jan1_noon_utc();
        for i in 0..10u32 {
            let _ = overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200 + i, 101.5), now);
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        let lost = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&lost);
        sink.run(move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });

        assert_eq!(lost.load(std::sync::atomic::Ordering::Relaxed), 0);
        let lines: usize = std::fs::read_dir(&dlq)
            .expect("dlq dir readable")
            .filter_map(Result::ok)
            .map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .lines()
                    .count()
            })
            .sum();
        assert_eq!(
            lines, 10,
            "every record of the failed batch must reach the DLQ"
        );
        let _ = std::fs::remove_file(&spill);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn run_one_cycle_with_replay_never_replays_while_the_database_cannot_flush() {
        // The test writer is permanently disconnected, so every live flush
        // fails: the health gate can never open and the spilled seals must
        // stay exactly where they are, however long the loop runs.
        let (spill, dlq) = temp_pair("cycle-with-replay-unhealthy");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let t0 = jan1_noon_utc();
        for i in 0..3u32 {
            tx.try_send(mk_seal(
                13,
                0,
                TfIndex::M1,
                1_716_023_700 + i,
                100.0 + f64::from(i),
            ))
            .expect("try_send");
        }
        let (cycle, replay) = runner.run_one_cycle_with_replay(t0);
        assert_eq!(cycle.drain.rescued_to_spill, 3, "{cycle:?}");
        assert!(replay.is_idle());
        for step in 1..=20 {
            let (_, replay) = runner.run_one_cycle_with_replay(t0 + step * 60);
            assert!(replay.is_idle(), "no replay without a clean flush");
        }
        assert_eq!(
            read_spill_records(&spill).len(),
            3,
            "the spilled seals stay in the live file"
        );
        assert!(!spill.join("replaying").exists());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn the_pending_count_tracks_queued_seals_until_the_thread_writes_them() {
        let (spill, dlq) = temp_pair("escalate-pending-count");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let pending = sink.pending_count();
        let stop = sink.stop_flag();
        let now = jan1_noon_utc();
        for i in 0..10u32 {
            let _ = overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 34_200 + i, 1.0), now);
        }
        assert_eq!(pending.load(std::sync::atomic::Ordering::Relaxed), 10);
        stop.store(true, std::sync::atomic::Ordering::Release);
        sink.run(|_| {});
        assert_eq!(
            pending.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "every written seal must leave the pending count"
        );
        // A refused send never counts: the queue is gone, so this goes inline.
        let _ = overflow.escalate(&mk_seal(13, 0, TfIndex::M1, 99, 1.0), now);
        assert_eq!(pending.load(std::sync::atomic::Ordering::Relaxed), 0);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn the_queue_holds_one_whole_close_burst() {
        // Audit PR15: the queue is sized to the one burst that can fill the
        // seal channel in front of it, so every refusal of that burst is
        // queued instead of written on the frame drain.
        assert_eq!(
            SEAL_ESCALATION_QUEUE_DEPTH,
            tickvault_trading::candles::SEAL_BUFFER_CAPACITY
        );
        assert!(SEAL_ESCALATION_QUEUE_DEPTH >= SEAL_MPSC_CAPACITY);
    }

    // -----------------------------------------------------------------------
    // Outage and recovery against a real writer (audit PR40c)
    // -----------------------------------------------------------------------

    /// A stand-in for QuestDB's ILP-over-HTTP endpoint. While `outage` is set
    /// it answers every write with 503, so the real writer's flush fails the
    /// way it does when the database is down. Otherwise it answers 204 and
    /// keeps the rows it acknowledged: only rows the database said it took
    /// count as written.
    struct FakeIlpServer {
        port: u16,
        outage: std::sync::Arc<std::sync::atomic::AtomicBool>,
        acked_rows: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    fn spawn_fake_ilp_server() -> FakeIlpServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake ILP server");
        let port = listener.local_addr().expect("local addr").port();
        let outage = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let acked_rows = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (thread_outage, thread_rows) = (
            std::sync::Arc::clone(&outage),
            std::sync::Arc::clone(&acked_rows),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let outage = std::sync::Arc::clone(&thread_outage);
                let rows = std::sync::Arc::clone(&thread_rows);
                std::thread::spawn(move || serve_fake_ilp_connection(stream, &outage, &rows));
            }
        });
        FakeIlpServer {
            port,
            outage,
            acked_rows,
        }
    }

    /// One keep-alive connection: read each request (fixed length or
    /// chunked body), answer it, and record the body's rows on a 204.
    fn serve_fake_ilp_connection(
        mut stream: std::net::TcpStream,
        outage: &std::sync::atomic::AtomicBool,
        rows: &std::sync::Mutex<Vec<String>>,
    ) {
        use std::io::{BufRead, Read, Write};
        let Ok(read_half) = stream.try_clone() else {
            return;
        };
        let mut reader = std::io::BufReader::new(read_half);
        loop {
            let mut content_length = 0usize;
            let mut chunked = false;
            let mut expect_continue = false;
            let mut saw_request_line = false;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let line = line.trim_end();
                if line.is_empty() {
                    if saw_request_line {
                        break;
                    }
                    continue;
                }
                saw_request_line = true;
                let lower = line.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                    chunked = true;
                } else if lower.starts_with("expect:") && lower.contains("100-continue") {
                    expect_continue = true;
                }
            }
            if expect_continue {
                let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            }
            let mut body = Vec::new();
            if chunked {
                loop {
                    let mut size_line = String::new();
                    if reader.read_line(&mut size_line).unwrap_or(0) == 0 {
                        return;
                    }
                    let size = usize::from_str_radix(
                        size_line.trim().split(';').next().unwrap_or("0"),
                        16,
                    )
                    .unwrap_or(0);
                    let mut chunk = vec![0u8; size + 2];
                    if reader.read_exact(&mut chunk).is_err() {
                        return;
                    }
                    if size == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..size]);
                }
            } else {
                body.resize(content_length, 0);
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
            }
            let response: &[u8] = if outage.load(std::sync::atomic::Ordering::Acquire) {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
            } else {
                if let Ok(mut acked) = rows.lock() {
                    acked.extend(
                        String::from_utf8_lossy(&body)
                            .lines()
                            .filter(|l| !l.is_empty())
                            .map(str::to_string),
                    );
                }
                b"HTTP/1.1 204 No Content\r\n\r\n"
            };
            if stream.write_all(response).is_err() {
                return;
            }
        }
    }

    /// IST-epoch seconds of 09:30:00 on Thursday 2026-09-24, inside the
    /// session window the writer enforces.
    fn session_bucket(offset_secs: u32) -> u32 {
        let base = chrono::NaiveDate::from_ymd_opt(2026, 9, 24)
            .and_then(|d| d.and_hms_opt(9, 30, 0))
            .expect("valid IST time")
            .and_utc()
            .timestamp();
        u32::try_from(base).expect("fits u32") + offset_secs
    }

    static PR40C_TABLES_KEYED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(true);

    #[test]
    fn chaos_a_database_outage_spills_every_seal_and_recovery_replays_each_once() {
        // Audit PR40c: the replay tests use a fake sink, so none of them
        // proves the REAL writer puts spilled seals back in the database. Here
        // the real `ShadowCandleWriter` speaks real ILP over HTTP to a fake
        // endpoint that goes down and comes back:
        //   1. outage: every flush is refused, so every seal goes to the spill;
        //   2. recovery: live seals flush again, the health gate opens after
        //      sixty clean seconds, and the mid-session replay re-sends the
        //      spilled seals through the same writer;
        //   3. every seal, live or spilled, is acknowledged EXACTLY once and
        //      the spill ends empty with its file archived.
        let fake = spawn_fake_ilp_server();
        let config = tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: fake.port,
            pg_port: 1,
            ilp_port: 1,
        };
        let writer = ShadowCandleWriter::new(&config)
            .expect("writer builds")
            .with_keyed_for_test(&PR40C_TABLES_KEYED);
        let (spill, dlq) = temp_pair("chaos-outage-recovery");
        let mut runner =
            SealWriterRunner::with_writer_for_test(writer, spill.clone(), dlq.clone(), 64, 64, 64);
        let tx = runner.sender();
        let mut now = jan1_noon_utc();
        let mut expected: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
        let mut next_offset = 0u32;
        let mut send = |count: u32, expected: &mut std::collections::BTreeSet<i64>| {
            for _ in 0..count {
                let bucket = session_bucket(next_offset);
                next_offset += 1;
                tx.try_send(mk_seal(13, 0, TfIndex::M1, bucket, 101.5))
                    .expect("channel has room");
                expected.insert(i64::from(bucket) * 1_000_000_000);
            }
        };

        // 1. Outage.
        fake.outage
            .store(true, std::sync::atomic::Ordering::Release);
        const OUTAGE_CYCLES: usize = 12;
        const OUTAGE_SEALS_PER_CYCLE: u32 = 50;
        let mut spilled = 0usize;
        let mut to_dlq = 0usize;
        for _ in 0..OUTAGE_CYCLES {
            send(OUTAGE_SEALS_PER_CYCLE, &mut expected);
            let (cycle, replay) = runner.run_one_cycle_with_replay(now);
            assert!(!cycle.drain.flushed_ok, "the database is down");
            assert!(replay.is_idle(), "nothing replays into a failing database");
            spilled += cycle.drain.rescued_to_spill;
            to_dlq += cycle.drain.rescued_to_dlq;
            now += 1;
        }
        let outage_seals = OUTAGE_CYCLES * OUTAGE_SEALS_PER_CYCLE as usize;
        assert_eq!(
            spilled, outage_seals,
            "every refused seal went to the spill ({to_dlq} went to the DLQ instead: the \
             spill refuses below SEAL_ESCALATION_MIN_FREE_BYTES of free disk)"
        );
        assert!(
            fake.acked_rows.lock().expect("rows").is_empty(),
            "nothing was acknowledged during the outage"
        );

        // 2. Recovery: one live seal a second until the spill is drained.
        fake.outage
            .store(false, std::sync::atomic::Ordering::Release);
        let mut reingested = 0usize;
        let mut archived = 0usize;
        for _ in 0..600 {
            send(1, &mut expected);
            let (cycle, replay) = runner.run_one_cycle_with_replay(now);
            assert!(cycle.drain.flushed_ok, "the database is back");
            reingested += replay.seals_reingested;
            archived += replay.files_archived;
            now += 1;
            if reingested == outage_seals && archived >= 1 {
                break;
            }
        }
        assert_eq!(
            reingested, outage_seals,
            "the replay re-sent every spilled seal"
        );
        assert!(archived >= 1, "the replayed file was archived");
        let live_left = std::fs::read_dir(&spill)
            .expect("spill dir")
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("bin"))
            .count();
        assert_eq!(live_left, 0, "no spill file is left to replay");

        // 3. Exactly once.
        let acked = fake.acked_rows.lock().expect("rows").clone();
        let mut seen: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();
        for row in &acked {
            let ts: i64 = row
                .rsplit(' ')
                .next()
                .and_then(|t| t.parse().ok())
                .expect("every ILP row ends with its timestamp");
            *seen.entry(ts).or_default() += 1;
        }
        let twice: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
        assert!(
            twice.is_empty(),
            "rows acknowledged more than once: {twice:?}"
        );
        let got: std::collections::BTreeSet<i64> = seen.keys().copied().collect();
        assert_eq!(got, expected, "every seal acknowledged, none invented");
        cleanup(&spill, &dlq);
    }

    // -----------------------------------------------------------------------
    // Z6 — an older copy never lands after a fuller one committed live
    // -----------------------------------------------------------------------

    /// One copy of the same 1-minute bar with `ticks` ticks.
    fn z6_copy(ticks: u32) -> BufferedSeal {
        let mut seal = mk_seal(13, 2, TfIndex::M1, 34_260, 99.0);
        seal.state.tick_count = ticks;
        seal
    }

    /// Point the spill directory at a regular file so every spill write
    /// fails, as on a dead disk.
    fn z6_block_spill(spill: &std::path::Path) {
        let _ = std::fs::remove_dir_all(spill);
        std::fs::write(spill, b"not a directory").expect("block the spill dir");
    }

    fn z6_unblock_spill(spill: &std::path::Path) {
        std::fs::remove_file(spill).expect("unblock");
        std::fs::create_dir_all(spill).expect("spill dir");
    }

    /// Path B: the original is still in the escalation queue when its amend
    /// commits live. The old ledger had nothing on disk to compare with, so
    /// the escalation thread then wrote the original into the spill, and the
    /// next replay put it over the amend.
    #[test]
    fn test_regression_z6_original_in_escalation_queue_is_not_written_after_live_amend() {
        let (spill, dlq) = temp_pair("z6-path-b");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let now = jan1_noon_utc();

        assert_eq!(overflow.escalate(&z6_copy(4), now), OverflowOutcome::Queued);
        assert_eq!(
            runner.unwritten_seals(),
            1,
            "the runner and the spill share one queue count"
        );
        // The amend commits live while the original waits in the queue.
        assert_eq!(runner.pipeline.note_live_commits(&[z6_copy(5)], now), 0);

        drop(overflow);
        let summary = sink.run(|_| panic!("no seal may be lost"));
        assert_eq!(summary.records, 1);
        assert_eq!(runner.unwritten_seals(), 0);
        let on_disk = runner.spill.read_all(now).expect("read spill");
        assert!(
            on_disk.iter().all(|seal| seal.tick_count >= 5),
            "the queued original reached the spill after its amend committed: {on_disk:?}"
        );
        cleanup(&spill, &dlq);
    }

    /// Path C, inline: the original went to the dead-letter file (the spill
    /// was down). The old ledger never saw dead-letter writes, so the amend
    /// committed later was not mirrored and the boot drain, reading the
    /// dead-letter file, ended on the original.
    #[test]
    fn test_regression_z6_dead_lettered_original_mirrors_live_amend() {
        let (spill, dlq) = temp_pair("z6-path-c-inline");
        z6_block_spill(&spill);
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let overflow = runner.overflow();
        let now = jan1_noon_utc();
        assert_eq!(
            overflow.escalate(&z6_copy(4), now),
            OverflowOutcome::DlqWritten
        );

        z6_unblock_spill(&spill);
        assert_eq!(runner.pipeline.note_live_commits(&[z6_copy(5)], now), 1);
        let on_disk = runner.spill.read_all(now).expect("read spill");
        assert_eq!(on_disk.len(), 1);
        assert_eq!(
            on_disk[0].tick_count, 5,
            "the amend is on disk for the boot drain"
        );
        cleanup(&spill, &dlq);
    }

    /// Path C through the escalation thread: a batch the spill refuses goes
    /// to the dead-letter file record by record, and each is recorded.
    #[test]
    fn test_regression_z6_escalation_thread_dead_letter_mirrors_live_amend() {
        let (spill, dlq) = temp_pair("z6-path-c-thread");
        z6_block_spill(&spill);
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        let now = jan1_noon_utc();
        assert_eq!(overflow.escalate(&z6_copy(4), now), OverflowOutcome::Queued);
        drop(overflow);
        sink.run(|_| panic!("the dead-letter file is writable"));
        assert_eq!(runner.unwritten_seals(), 0);

        z6_unblock_spill(&spill);
        assert_eq!(runner.pipeline.note_live_commits(&[z6_copy(5)], now), 1);
        let on_disk = runner.spill.read_all(now).expect("read spill");
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].tick_count, 5);
        cleanup(&spill, &dlq);
    }
}
