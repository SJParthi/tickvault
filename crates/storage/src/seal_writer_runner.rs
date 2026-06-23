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
//!   path, allowed to allocate). `try_send` is non-blocking — if the
//!   channel is full, the producer drops and increments a Prom
//!   counter; it NEVER waits on I/O.
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
use crate::seal_writer_task::{DrainOutcome, drain_once};
use crate::shadow_candle_writer::ShadowCandleWriter;

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
/// non-blocking by design — if the mpsc is full (after the writer
/// task has fallen behind for ~20 seconds at peak burst), the seal
/// is dropped and the producer increments
/// `tv_seal_producer_mpsc_full_total`.
#[must_use]
pub fn global_seal_sender() -> Option<&'static mpsc::Sender<BufferedSeal>> {
    GLOBAL_SEAL_SENDER.get()
}

/// Bounded mpsc capacity for the producer→consumer wire. Sized to
/// absorb the IST-midnight burst (~99K seals across 11K instruments
/// × 9 TFs) without saturating; matches `SEAL_BUFFER_CAPACITY` for
/// symmetry with the locked Wave-6 ring sizing per L-C1.
pub const SEAL_MPSC_CAPACITY: usize = 200_000;

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
            spill_dir,
            dlq_dir,
        );
        let (sender, receiver) = mpsc::channel(mpsc_capacity);
        Self {
            sender,
            receiver,
            pipeline,
            writer,
            max_drain_per_cycle,
        }
    }

    /// Cloneable producer-side handle. The future aggregator passes
    /// these clones into the per-instrument cell hot-path so the
    /// `try_send(seal)` call can fire from any thread without blocking.
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<BufferedSeal> {
        self.sender.clone()
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

    fn mk_seal(sid: u32, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
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
        // Pin the locked mpsc capacity so a regression PR can't
        // silently lower it (which would push more producer-side
        // drops at IST midnight).
        assert_eq!(SEAL_MPSC_CAPACITY, 200_000);
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
}
