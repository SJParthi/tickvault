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
use crate::seal_writer_task::{BootDrainOutcome, DrainOutcome, drain_once, drain_recovered_seals};
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
}

/// Bounded depth of the escalation hand-off queue.
///
/// 4,096 records of ~128 bytes each is ~512 KiB of shock absorption — enough
/// to cover a multi-second disk stall at the seal rates this path sees, and
/// small enough that draining it at shutdown fits inside
/// `SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS` even at a degraded ~1 ms/write.
///
/// Overflow is NOT a loss: a full queue falls back to the inline cascade,
/// which is exactly what this path did before the offload existed. The
/// worst case is therefore "as slow as it used to be", never "lossy".
pub const SEAL_ESCALATION_QUEUE_DEPTH: usize = 4_096;

/// How often the escalation thread wakes to re-check its stop flag while the
/// queue is empty.
const SEAL_ESCALATION_STOP_POLL: std::time::Duration = std::time::Duration::from_millis(100); // APPROVED: this IS the named constant the rule asks for

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

/// Put all four escalation series on the wire at zero when the escalation
/// subsystem is installed.
///
/// # Why a built handle is not enough — measured, not assumed
///
/// A `cloudwatch list-metrics` sweep on 2026-08-29 compared the EMF selector
/// against the live account: the selector names 104 metrics, the account held
/// 86, and all four of these were among the names that had **never published a
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
}

impl SealEscalationSink {
    /// The flag that asks the thread to drain and exit. Take a clone BEFORE
    /// moving the sink into the thread — afterwards it is unreachable, which
    /// is precisely the defect the WAL spill writer carried until 2026-08-28.
    #[must_use]
    pub fn stop_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.stop)
    }

    /// Drain the queue until the sender is gone, or until the stop flag is set
    /// AND the queue is empty.
    ///
    /// `on_lost` is called for a seal both disk tiers refused. It exists as a
    /// callback rather than a direct call because the paging path
    /// (`AGGREGATOR-DROP-01`) lives in the app crate, above this one.
    pub fn run<F>(self, on_lost: F)
    where
        F: Fn(&crate::seal_spill::SerializedSeal),
    {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            match self.rx.recv_timeout(SEAL_ESCALATION_STOP_POLL) {
                Ok(item) => {
                    if SealOverflow::escalate_inline(
                        &self.spill,
                        &self.dlq,
                        &item.seal,
                        item.now_unix_secs,
                    ) == OverflowOutcome::Lost
                    {
                        metrics::counter!(SEAL_ESCALATION_LOST_COUNTER).increment(1);
                        on_lost(&item.seal);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Only exit on an EMPTY queue: a pending item always
                    // returns `Ok` above, so a stop request can never cut the
                    // drain short.
                    if self.stop.load(Ordering::Acquire) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
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
    #[must_use]
    pub fn new(
        spill: std::sync::Arc<crate::seal_spill::SealSpillWriter>,
        dlq: std::sync::Arc<crate::seal_dlq::SealDlqWriter>,
    ) -> Self {
        Self {
            spill,
            dlq,
            offload: None,
            queued: metrics::counter!(SEAL_ESCALATION_QUEUED_COUNTER),
            inline_fallback: metrics::counter!(SEAL_ESCALATION_INLINE_FALLBACK_COUNTER),
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
    /// 598,976/600,000. A producer-side escalation queues behind all of that.
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
            return OverflowOutcome::DlqWritten;
        }
        OverflowOutcome::Lost
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
                    self.inline_fallback.increment(1);
                    return Self::escalate_inline(
                        &self.spill,
                        &self.dlq,
                        &item.seal,
                        item.now_unix_secs,
                    );
                }
            }
        }
        Self::escalate_inline(&self.spill, &self.dlq, &serialised, now_unix_secs)
    }
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
/// `AGGREGATOR_MAX_SLOTS × TF_COUNT` = 25,000 × 24 = **600,000** seals
/// in one burst, and every one of them must pass through here before it
/// can reach the ring's three absorbing tiers. At 200,000 the channel
/// force-dropped **400,000** of them on `try_send` — counter-only, no
/// log line, no alarm — every midnight, while the ring behind it was
/// correctly sized for the full burst and sat mostly empty.
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
/// itself — 600,000 × ≤144 B ≈ **86 MB**, matching the ring, 0.26% of
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
        let (sender, receiver) = mpsc::channel(SEAL_MPSC_CAPACITY);
        Ok(Self {
            sender,
            receiver,
            pipeline,
            writer,
            max_drain_per_cycle,
            spill_dir: production_spill_dir(),
            dlq_dir: production_dlq_dir(),
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
        let writer = ShadowCandleWriter::for_test();
        let pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            ring_capacity,
            spill_dir.clone(),
            dlq_dir.clone(),
        );
        let (sender, receiver) = mpsc::channel(mpsc_capacity);
        Self {
            sender,
            receiver,
            pipeline,
            writer,
            max_drain_per_cycle,
            spill_dir,
            dlq_dir,
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
        state.oi_pct_from_prev_day = -0.2;
        state.volume_pct_from_prev_day = 12.3;
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
                1_716_000_900 + i as u32,
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
                1_716_000_900 + i as u32,
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
        tx_a.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("a");
        tx_b.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
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
        let s1 = mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        let s3 = mk_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0);
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

        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("ok");
        let o1 = runner.run_one_cycle(now);
        assert_eq!(o1.submitted_from_mpsc, 1);
        assert_eq!(o1.drain.ring_seals_popped, 1);

        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
            .expect("ok");
        tx.try_send(mk_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0))
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
                1_716_000_900 + i as u32,
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
        // Arithmetic pin, not a wall-clock measurement. The shutdown budget
        // is 5s and each queued record costs one ~128-byte `write(2)`; at a
        // badly degraded 1ms/write the full queue takes
        // SEAL_ESCALATION_QUEUE_DEPTH milliseconds. Raising the depth without
        // raising the budget (which the systemd guard would then catch) makes
        // the shutdown drain unable to finish, and an undrained bounded queue
        // at exit is a silent loss.
        const DEGRADED_WRITE_MICROS: usize = 1_000;
        const BUDGET_MICROS: usize = 5 * 1_000_000;
        assert!(
            SEAL_ESCALATION_QUEUE_DEPTH * DEGRADED_WRITE_MICROS < BUDGET_MICROS,
            "SEAL_ESCALATION_QUEUE_DEPTH = {SEAL_ESCALATION_QUEUE_DEPTH} cannot drain inside \
             the 5s SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS at a degraded 1ms/write. Raise the \
             budget in main.rs (and the systemd TimeoutStopSec the guard derives from it), \
             or lower the depth."
        );
    }
}
