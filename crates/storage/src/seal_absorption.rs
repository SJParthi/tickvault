//! Wave 6 Sub-PR #1 item 1.2e — sealed-candle 3-tier absorption pipeline.
//!
//! Orchestrator façade that wires the three already-merged absorbing
//! tiers (locked decision **L-C1**) into one infallible `submit()`
//! entry point:
//!
//! ```text
//!   submit(seal)
//!         │
//!         ▼
//!   Tier 1 — SealRing  (in-memory FIFO, capacity SEAL_BUFFER_CAPACITY)
//!   if accepted → SubmitOutcome::Buffered
//!   if full → drop-OLDEST returns evicted seal → fall through to tier 2
//!         │
//!         ▼
//!   Tier 2 — SealSpillWriter  (binary 128-byte fixed records on disk)
//!   if append succeeds → SubmitOutcome::Spilled
//!   if append fails (disk full / permission / I/O error) → fall through
//!         │
//!         ▼
//!   Tier 3 — SealDlqWriter  (NDJSON, recoverable text)
//!   if append succeeds → SubmitOutcome::DlqWritten
//!   if append fails → SubmitOutcome::Dropped(seal)
//!     ↳ caller MUST fire `error!(code = ErrorCode::AggregatorDrop01.code_str(), …)`
//!       per AGGREGATOR-DROP-01 runbook (`.claude/rules/project/wave-6-error-codes.md`).
//! ```
//!
//! ## What this slice ships
//!
//! - [`SealAbsorptionPipeline`] — owns the 3 tiers, exposes `submit()`,
//!   `pop_oldest()`, `drain_all()`, observability accessors.
//! - [`SubmitOutcome`] — explicit success/escalation/loss enum so the
//!   future writer task (item 1.2f) can branch on outcome and emit
//!   the right Prometheus counter labels:
//!   `tv_seal_absorption_total{tier="ring"|"spill"|"dlq"|"dropped"}`.
//! - 18 unit tests covering: pure-buffer happy path, ring-overflow
//!   spill happy path, spill-fail-then-DLQ escalation, DLQ-fail-then-drop
//!   triple failure, FIFO drain order, I-P1-11 segment isolation
//!   through eviction, idempotent shutdown drain.
//!
//! ## What this slice does NOT ship
//!
//! - The async writer task that DRAINS the ring and ILP-sends to the
//!   `candles_*_shadow` tables — item 1.2f.
//! - Boot wiring + `mpsc::Sender<BufferedSeal>` channel from the
//!   aggregator hot path — item 1.4.
//! - Prometheus counter increments — wired by item 1.2f when the
//!   async task lands (this slice exposes outcome-counting accessors so
//!   the future task / a unit test can drive the metric).
//!
//! ## Why not return Result<...>
//!
//! The whole point of L-C1 is that the producer (the aggregator hot
//! path) NEVER blocks on I/O. `submit()` is therefore infallible by
//! design: every absorption failure escalates one tier deeper, and
//! the worst case `SubmitOutcome::Dropped` is a typed value, not a
//! propagated error. A `Result` would force every aggregator call
//! site to handle a failure that the design already absorbs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use tracing::warn;

use tickvault_trading::candles::{BufferOutcome, BufferedSeal, SealRing};

use crate::seal_dlq::{SealDlqRecord, SealDlqWriter};
use crate::seal_spill::{SealSpillWriter, SerializedSeal};
use crate::tick_persistence::{
    BlindWritePolicy, DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES, SPILL_MIN_FREE_HEADROOM_BYTES,
    SpillFloorVerdict, classify_spill_floor, spill_free_bytes,
};

/// Free-space floor for the seal ESCALATION CHAIN — spill AND DLQ together.
///
/// # Why the check lives here and not in `append_seal`
///
/// An adversarial review found seal spill to be the one writer on the volume
/// with no free-space floor, no byte cap and no size-based prune, and
/// recommended mirroring the depth tier's floor inside `append_seal`. That
/// would have been a NO-OP for disk pressure: a spill `Err` escalates to the
/// DLQ, which writes to `data/dlq/` on the SAME volume with a LARGER record
/// and a `warn!` per seal. The bytes would move, not stop. The decision
/// therefore governs both tiers, once, before either is attempted.
///
/// # Where 4 GiB sits, and why
///
/// The volume already has a priority ladder, and this completes it:
///
/// | tier | floor | rationale |
/// |---|---|---|
/// | depth | 16 GiB | record-only, ~24x the tick row volume — refuse first |
/// | **seals** | **4 GiB** | derived candles: above depth, below ticks |
/// | ticks | 2 GiB | decision-critical — refuse last |
///
/// Const-asserted below, so the ladder cannot be inverted by editing one
/// number in isolation.
pub const SEAL_ESCALATION_MIN_FREE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const _: () = assert!(
    SEAL_ESCALATION_MIN_FREE_BYTES > SPILL_MIN_FREE_HEADROOM_BYTES
        && SEAL_ESCALATION_MIN_FREE_BYTES < DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES,
    "the seal escalation floor must sit strictly between the tick floor and \
     the depth floor: seals are derived candles, more valuable than depth \
     rows and less valuable than the ticks they are derived from"
);

/// Minimum gap between free-space probes, in seconds.
///
/// `spill_free_bytes` forks `df`. `escalate_evicted` is reachable from the
/// FRAME-DRAIN task and a ring-overflow episode calls it in a tight burst,
/// so an unthrottled probe would spawn a process per seal on the one task
/// that must never block. THAT is why this tier had no floor, and a floor
/// added without the throttle would have been worse than none.
const SEAL_DISK_PROBE_INTERVAL_SECS: i64 = 5;

/// Outcome of [`SealAbsorptionPipeline::submit`]. Maps 1:1 to the
/// counter label `tv_seal_absorption_total{tier=...}` the async writer
/// task (item 1.2f) emits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SubmitOutcome {
    /// Tier 1 happy path — seal accepted into the ring, no eviction.
    Buffered,
    /// Tier 1 overflow → evicted seal escalated to tier 2 (binary spill).
    /// The new seal is now in the ring; the old seal is on disk.
    Spilled,
    /// Tier 2 escalation failed → evicted seal escalated to tier 3
    /// (NDJSON DLQ). The new seal is in the ring; the old seal is in
    /// the DLQ as recoverable text.
    DlqWritten,
    /// All three tiers failed — the evicted seal is unrecoverable.
    /// Caller MUST fire `error!(code = ErrorCode::AggregatorDrop01.code_str(), …)`
    /// per AGGREGATOR-DROP-01 runbook. Carries the lost seal for
    /// forensic logging (the operator should inspect host disk/memory
    /// state before deciding how to recover).
    Dropped(BufferedSeal),
}

/// Three-tier absorption pipeline for sealed candles.
///
/// Single-threaded by design — the future writer task (item 1.2f)
/// owns the pipeline; producers push via a tokio `mpsc` channel
/// ahead of it. NO concurrent `submit()` calls.
pub struct SealAbsorptionPipeline {
    ring: SealRing,
    /// `Arc` since 2026-08-19 so the SAME writer instance can be shared with
    /// the producer-side overflow escalator (`seal_writer_runner`).
    ///
    /// Sharing the INSTANCE, not just the directory, is the point.
    /// `SealSpillWriter` caches an open append handle behind its own
    /// `Mutex`; two independent writers pointed at one day-file would each
    /// hold their own handle and interleave `write` calls with no shared
    /// lock, which is how a spilled seal becomes a half-written NDJSON line.
    /// One instance means one mutex serialises every append, from either
    /// side.
    spill: Arc<SealSpillWriter>,
    /// Same reasoning as `spill`. The DLQ writer is stateless today, so this
    /// is defensive rather than load-bearing — but it keeps the two tiers
    /// symmetric so a future cached handle on the DLQ side cannot
    /// reintroduce the split-writer hazard silently.
    dlq: Arc<SealDlqWriter>,
    /// Cached free-space verdict for the escalation floor.
    ///
    /// `u64::MAX` is the BLIND sentinel: the probe failed, or has never run.
    /// Blind means ALLOW (see `has_room_to_escalate`), so the sentinel is the
    /// safe initial value — a pipeline that has not probed yet never refuses.
    last_free_bytes: AtomicU64,
    /// IST-epoch second of the last probe. `i64::MIN` forces the first call
    /// to probe rather than trusting the sentinel above.
    last_probe_secs: AtomicI64,
}

impl SealAbsorptionPipeline {
    /// Production constructor. Uses the locked
    /// `SEAL_BUFFER_CAPACITY` ring + `data/spill/` + `data/dlq/`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ring: SealRing::new(),
            spill: Arc::new(SealSpillWriter::new()),
            dlq: Arc::new(SealDlqWriter::new()),
            last_free_bytes: AtomicU64::new(u64::MAX),
            last_probe_secs: AtomicI64::new(i64::MIN),
        }
    }

    /// Test constructor. Tests inject isolated `tempdir` paths so they
    /// can run in parallel without filesystem contention.
    #[must_use]
    // TEST-EXEMPT: test-only helper used as construction source by every test in this module (ring-overflow, spill-fail, DLQ-fail, drain, FIFO ordering scenarios all build via this factory).
    pub fn with_dirs_for_test(spill_dir: PathBuf, dlq_dir: PathBuf) -> Self {
        Self {
            ring: SealRing::new(),
            spill: Arc::new(SealSpillWriter::with_spill_dir_for_test(spill_dir)),
            dlq: Arc::new(SealDlqWriter::with_dlq_dir_for_test(dlq_dir)),
            last_free_bytes: AtomicU64::new(u64::MAX),
            last_probe_secs: AtomicI64::new(i64::MIN),
        }
    }

    /// Test constructor with a custom ring capacity to make
    /// overflow-cascade tests deterministic without populating the
    /// production-sized ring. Honours the same temp-dir pattern.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by overflow / spill-fail / DLQ cascade tests to make ring overflow deterministic without pushing 200K seals.
    pub fn with_capacity_and_dirs_for_test(
        ring_capacity: usize,
        spill_dir: PathBuf,
        dlq_dir: PathBuf,
    ) -> Self {
        Self {
            ring: SealRing::with_capacity(ring_capacity),
            spill: Arc::new(SealSpillWriter::with_spill_dir_for_test(spill_dir)),
            dlq: Arc::new(SealDlqWriter::with_dlq_dir_for_test(dlq_dir)),
            last_free_bytes: AtomicU64::new(u64::MAX),
            last_probe_secs: AtomicI64::new(i64::MIN),
        }
    }

    /// The tier-2 writer, shareable. Handed to the producer-side overflow
    /// escalator so a seal refused by the writer channel lands in the SAME
    /// spill file this pipeline drains on boot — not a second one nobody
    /// reads.
    #[must_use]
    pub fn spill_handle(&self) -> Arc<SealSpillWriter> {
        Arc::clone(&self.spill)
    }

    /// The tier-3 writer, shareable. Same reasoning as [`Self::spill_handle`].
    #[must_use]
    pub fn dlq_handle(&self) -> Arc<SealDlqWriter> {
        Arc::clone(&self.dlq)
    }

    /// Producer entry point. Infallible by design — every absorption
    /// failure escalates one tier deeper. Worst case returns
    /// [`SubmitOutcome::Dropped`] carrying the lost seal.
    ///
    /// `now_unix_secs` is the UTC unix timestamp used to derive the
    /// IST-date filename for the spill / DLQ files (per locked
    /// decision **L-H7** — NEVER `Utc::now()` on the hot path; the
    /// caller passes the wall-clock).
    pub fn submit(&mut self, seal: BufferedSeal, now_unix_secs: i64) -> SubmitOutcome {
        match self.ring.try_buffer(seal) {
            BufferOutcome::Buffered => SubmitOutcome::Buffered,
            BufferOutcome::DroppedOldest(evicted) => self.escalate_evicted(evicted, now_unix_secs),
        }
    }

    /// Rescue path for a seal that left the ring (was popped by the
    /// writer task) but FAILED to flush via ILP. Walks the SAME
    /// tier-2 → tier-3 cascade as ring overflow, BYPASSING the ring
    /// (the seal was already drained from there).
    ///
    /// Used by the future writer task slice (item 1.2f.3+) when
    /// `ShadowCandleWriter::flush()` returns `Err`. The popped seals
    /// have already left the ring so we do NOT want to re-buffer them
    /// (would invert FIFO order); instead we walk straight to disk
    /// spill, escalating to DLQ then `Dropped` if the lower tiers
    /// also fail.
    ///
    /// Returns the same [`SubmitOutcome`] enum as [`Self::submit`] —
    /// `Buffered` is impossible (we skip the ring), so the caller
    /// will only ever observe `Spilled` / `DlqWritten` / `Dropped`.
    pub fn rescue_in_flight(&self, seal: BufferedSeal, now_unix_secs: i64) -> SubmitOutcome {
        self.escalate_evicted(seal, now_unix_secs)
    }

    /// Is there room on the volume to escalate at all?
    ///
    /// THROTTLED: `spill_free_bytes` forks `df`, and this is reachable from
    /// the frame drain, so the answer is cached for
    /// [`SEAL_DISK_PROBE_INTERVAL_SECS`]. A burst of a thousand escalations
    /// inside one second costs ONE probe, not a thousand processes.
    ///
    /// FAILS OPEN when the probe cannot answer, matching the TICK tier and
    /// deliberately unlike depth: a seal that is refused here is a CANDLE
    /// that exists nowhere else, so one broken `df` must never be able to
    /// start discarding candles. Depth can afford the opposite default
    /// because a depth row is a record, not a decision input.
    fn has_room_to_escalate(&self, payload_len: u64, now_unix_secs: i64) -> bool {
        let last = self.last_probe_secs.load(Ordering::Relaxed);
        let due = now_unix_secs.saturating_sub(last) >= SEAL_DISK_PROBE_INTERVAL_SECS;
        let free = if due {
            // Store the probe TIME before the value: a concurrent caller that
            // sees the new time and the old value re-uses a verdict at most
            // one interval stale, which is the same guarantee a single
            // caller gets. The inverse order could let every racing caller
            // decide it is due and fork `df` in lockstep.
            self.last_probe_secs.store(now_unix_secs, Ordering::Relaxed);
            let probed = spill_free_bytes(self.spill.spill_dir());
            self.last_free_bytes
                .store(probed.unwrap_or(u64::MAX), Ordering::Relaxed);
            probed
        } else {
            match self.last_free_bytes.load(Ordering::Relaxed) {
                u64::MAX => None,
                bytes => Some(bytes),
            }
        };

        match classify_spill_floor(
            free,
            payload_len,
            SEAL_ESCALATION_MIN_FREE_BYTES,
            BlindWritePolicy::FailOpen,
        ) {
            SpillFloorVerdict::Allow | SpillFloorVerdict::AllowProbeFailed => true,
            SpillFloorVerdict::RefuseNoRoom | SpillFloorVerdict::RefuseProbeFailed => false,
        }
    }

    /// Tier 2 + tier 3 escalation chain for an evicted seal.
    fn escalate_evicted(&self, evicted: BufferedSeal, now_unix_secs: i64) -> SubmitOutcome {
        let serialised = SerializedSeal::from(&evicted);

        // FREE-SPACE FLOOR FOR THE WHOLE CHAIN (2026-09-01).
        //
        // Checked ONCE, here, rather than inside `append_seal` — a refusal
        // there would fall through to the DLQ, which writes a LARGER record
        // to the SAME volume plus a `warn!` per seal. That is more bytes on a
        // full disk, not fewer.
        //
        // Refusing costs candles, and that is the deliberate trade: below
        // 4 GiB free, QuestDB is close to the state that on 2026-08-25
        // suspended fifteen tables and left the box unreachable over SSM.
        // The loss is LOUD — `Dropped` is what makes the caller fire
        // AGGREGATOR-DROP-01, which is alarmed.
        if !self.has_room_to_escalate(serialised.to_bytes().len() as u64, now_unix_secs) {
            warn!(
                security_id = evicted.security_id,
                exchange_segment_code = evicted.exchange_segment_code,
                floor_bytes = SEAL_ESCALATION_MIN_FREE_BYTES,
                "refusing seal escalation: free space is below the seal floor — \
                 spilling would take the volume closer to the state that stalls \
                 QuestDB, and the DLQ is on the same volume. Caller MUST fire \
                 AGGREGATOR-DROP-01"
            );
            return SubmitOutcome::Dropped(evicted);
        }
        match self.spill.append_seal(&serialised, now_unix_secs) {
            Ok(()) => SubmitOutcome::Spilled,
            Err(spill_err) => {
                warn!(
                    ?spill_err,
                    security_id = evicted.security_id,
                    exchange_segment_code = evicted.exchange_segment_code,
                    "tier-2 spill failed — escalating to tier-3 DLQ"
                );
                let dlq_record = SealDlqRecord::from(&serialised);
                match self.dlq.append_record(&dlq_record, now_unix_secs) {
                    Ok(()) => SubmitOutcome::DlqWritten,
                    Err(dlq_err) => {
                        warn!(
                            ?dlq_err,
                            security_id = evicted.security_id,
                            exchange_segment_code = evicted.exchange_segment_code,
                            "tier-3 DLQ also failed — caller MUST fire AGGREGATOR-DROP-01"
                        );
                        SubmitOutcome::Dropped(evicted)
                    }
                }
            }
        }
    }

    /// Number of seals currently buffered in tier 1 (ring). Used by
    /// the future writer task to drive the
    /// `tv_seal_ring_depth` Prometheus gauge and for observability
    /// in unit tests.
    #[must_use]
    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Configured ring capacity. Used by the future writer task to
    /// emit the high-water-mark watermark gauge.
    #[must_use]
    pub fn ring_capacity(&self) -> usize {
        self.ring.capacity()
    }

    /// Pop the oldest buffered seal. Used by the future writer task
    /// (item 1.2f) to drain the ring and ILP-send to the
    /// `candles_*_shadow` tables. Returns `None` if the ring is
    /// empty.
    #[must_use]
    pub fn pop_oldest(&mut self) -> Option<BufferedSeal> {
        self.ring.pop_oldest()
    }

    /// Drain every remaining seal into the provided sink in FIFO
    /// order. Used by graceful shutdown to flush before exit.
    pub fn drain_all<F: FnMut(BufferedSeal)>(&mut self, sink: F) {
        self.ring.drain_all(sink);
    }
}

impl Default for SealAbsorptionPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
        let mut spill = std::env::temp_dir();
        let mut dlq = std::env::temp_dir();
        spill.push(format!(
            "tickvault-seal-pipeline-spill-{}-{}",
            name,
            std::process::id()
        ));
        dlq.push(format!(
            "tickvault-seal-pipeline-dlq-{}-{}",
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

    fn mk_buffered_seal(sid: u64, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
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

    fn jan1_noon_utc() -> i64 {
        chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp()
    }

    use chrono::TimeZone;

    #[test]
    fn test_submit_returns_buffered_when_ring_has_space() {
        let (spill, dlq) = temp_pair("submit-buffered");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(4, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        let s = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        assert_eq!(p.submit(s, now), SubmitOutcome::Buffered);
        assert_eq!(p.ring_len(), 1);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_submit_overflow_spills_evicted_seal() {
        // Fill the ring (capacity 2), then submit a 3rd → oldest
        // evicted → spill must succeed → SubmitOutcome::Spilled.
        let (spill, dlq) = temp_pair("submit-spilled");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(2, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_buffered_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        let s3 = mk_buffered_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0);
        assert_eq!(p.submit(s1, now), SubmitOutcome::Buffered);
        assert_eq!(p.submit(s2, now), SubmitOutcome::Buffered);
        assert_eq!(p.submit(s3, now), SubmitOutcome::Spilled);
        assert_eq!(p.ring_len(), 2);
        // Verify the evicted s1 actually landed on disk.
        let writer = SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = writer.read_all(now).expect("read spill");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].security_id, 13);
        assert_eq!(drained[0].close, 100.0);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_submit_spill_failure_escalates_to_dlq() {
        // Force tier-2 to fail by pointing the spill dir at a
        // pre-created FILE (not a directory) so `create_dir_all` errs.
        // Tier-3 DLQ has a real dir, so escalation must succeed →
        // SubmitOutcome::DlqWritten.
        let (_unused_spill, dlq) = temp_pair("submit-dlq");
        let mut spill_as_file = std::env::temp_dir();
        spill_as_file.push(format!(
            "tickvault-seal-pipeline-spill-as-file-{}-{}",
            "submit-dlq",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&spill_as_file);
        let _ = std::fs::remove_dir_all(&spill_as_file);
        // Create a *file* at this path so create_dir_all fails.
        std::fs::write(&spill_as_file, b"this is a file, not a dir").expect("create blocker");
        let mut p = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            1,
            spill_as_file.clone(),
            dlq.clone(),
        );
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_buffered_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        assert_eq!(p.submit(s1, now), SubmitOutcome::Buffered);
        assert_eq!(p.submit(s2, now), SubmitOutcome::DlqWritten);
        // Verify the evicted s1 landed in the DLQ.
        let dlq_writer = SealDlqWriter::with_dlq_dir_for_test(dlq.clone());
        let drained = dlq_writer.read_all(now).expect("read dlq");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].security_id, 13);
        assert_eq!(drained[0].close, 100.0);
        let _ = std::fs::remove_file(&spill_as_file);
        cleanup(&spill_as_file, &dlq);
    }

    #[test]
    fn test_submit_dlq_failure_returns_dropped_with_seal() {
        // Force BOTH tier-2 spill AND tier-3 DLQ to fail by pointing
        // each at a pre-created FILE so `create_dir_all` errs. The
        // call must return SubmitOutcome::Dropped(evicted_seal).
        let mut spill_as_file = std::env::temp_dir();
        spill_as_file.push(format!(
            "tickvault-seal-pipeline-spill-blocker-{}-{}",
            "submit-dropped",
            std::process::id()
        ));
        let mut dlq_as_file = std::env::temp_dir();
        dlq_as_file.push(format!(
            "tickvault-seal-pipeline-dlq-blocker-{}-{}",
            "submit-dropped",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&spill_as_file);
        let _ = std::fs::remove_dir_all(&spill_as_file);
        let _ = std::fs::remove_file(&dlq_as_file);
        let _ = std::fs::remove_dir_all(&dlq_as_file);
        std::fs::write(&spill_as_file, b"file").expect("create spill blocker");
        std::fs::write(&dlq_as_file, b"file").expect("create dlq blocker");
        let mut p = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            1,
            spill_as_file.clone(),
            dlq_as_file.clone(),
        );
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_buffered_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        assert_eq!(p.submit(s1, now), SubmitOutcome::Buffered);
        match p.submit(s2, now) {
            SubmitOutcome::Dropped(lost) => {
                // The evicted oldest = s1 — that's what's lost.
                assert_eq!(lost.security_id, 13);
                assert_eq!(lost.state.close, 100.0);
            }
            other => panic!("expected Dropped, got {other:?}"),
        }
        let _ = std::fs::remove_file(&spill_as_file);
        let _ = std::fs::remove_file(&dlq_as_file);
    }

    #[test]
    fn test_submit_outcomes_are_distinct() {
        // Every variant of SubmitOutcome must compare unequal to the
        // others — operator's "tv_seal_absorption_total{tier=...}"
        // counter relies on this.
        let s = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let buffered = SubmitOutcome::Buffered;
        let spilled = SubmitOutcome::Spilled;
        let dlq = SubmitOutcome::DlqWritten;
        let dropped = SubmitOutcome::Dropped(s);
        assert_ne!(buffered, spilled);
        assert_ne!(buffered, dlq);
        assert_ne!(buffered, dropped);
        assert_ne!(spilled, dlq);
        assert_ne!(spilled, dropped);
        assert_ne!(dlq, dropped);
    }

    #[test]
    fn test_pop_oldest_returns_fifo_order() {
        let (spill, dlq) = temp_pair("pop-fifo");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        for i in 0..3 {
            let s = mk_buffered_seal(
                13 + i,
                0,
                TfIndex::M1,
                1_716_000_900 + i as u32,
                100.0 + i as f64,
            );
            assert_eq!(p.submit(s, now), SubmitOutcome::Buffered);
        }
        assert_eq!(p.pop_oldest().expect("first").security_id, 13);
        assert_eq!(p.pop_oldest().expect("second").security_id, 14);
        assert_eq!(p.pop_oldest().expect("third").security_id, 15);
        assert!(p.pop_oldest().is_none());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_all_drains_in_fifo_order_and_empties_ring() {
        let (spill, dlq) = temp_pair("drain-fifo");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        for i in 0..5 {
            let s = mk_buffered_seal(
                13 + i,
                0,
                TfIndex::M1,
                1_716_000_900 + i as u32,
                100.0 + i as f64,
            );
            assert_eq!(p.submit(s, now), SubmitOutcome::Buffered);
        }
        let mut collected: Vec<u64> = Vec::new();
        p.drain_all(|seal| collected.push(seal.security_id));
        assert_eq!(collected, vec![13, 14, 15, 16, 17]);
        assert_eq!(p.ring_len(), 0);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_all_on_empty_ring_is_noop() {
        let (spill, dlq) = temp_pair("drain-empty");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let mut count = 0;
        p.drain_all(|_| count += 1);
        assert_eq!(count, 0);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_overflow_evicts_oldest_not_newest() {
        // L-C1: drop-OLDEST FIFO. The evicted seal MUST be the oldest
        // (s1), not the newest (s3). Verified by inspecting the spill
        // file: s1 must be there, s2 + s3 must remain in the ring.
        let (spill, dlq) = temp_pair("evict-oldest");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(2, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_buffered_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        let s3 = mk_buffered_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0);
        p.submit(s1, now);
        p.submit(s2, now);
        p.submit(s3, now);
        // Ring should now hold [s2, s3] in FIFO order.
        assert_eq!(p.pop_oldest().expect("first").security_id, 25);
        assert_eq!(p.pop_oldest().expect("second").security_id, 51);
        assert!(p.pop_oldest().is_none());
        // Spill should hold s1.
        let writer = SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = writer.read_all(now).expect("read spill");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].security_id, 13);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_overflow_distinguishes_segments_for_i_p1_11() {
        // I-P1-11: the eviction path must preserve composite key
        // (security_id, exchange_segment_code). Same security_id with
        // different segments must round-trip distinctly through tier-2
        // spill.
        let (spill, dlq) = temp_pair("evict-i-p1-11");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(2, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        let seg0 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let seg1 = mk_buffered_seal(13, 1, TfIndex::M1, 1_716_001_500, 200.0);
        let filler = mk_buffered_seal(99, 0, TfIndex::M1, 1_716_002_100, 300.0);
        p.submit(seg0, now);
        p.submit(seg1, now);
        // 3rd submit evicts seg0 (oldest). Spill receives composite-key-distinct seg0.
        p.submit(filler, now);
        let writer = SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = writer.read_all(now).expect("read spill");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].security_id, 13);
        assert_eq!(drained[0].exchange_segment_code, 0);
        // seg1 (security_id=13, segment=1) must STILL be in the ring,
        // not collapsed with seg0.
        assert_eq!(p.ring_len(), 2);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_ring_capacity_accessor_returns_configured_value() {
        let (spill, dlq) = temp_pair("ring-capacity");
        let p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(17, spill.clone(), dlq.clone());
        assert_eq!(p.ring_capacity(), 17);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_ring_len_tracks_inserts() {
        let (spill, dlq) = temp_pair("ring-len");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        assert_eq!(p.ring_len(), 0);
        for i in 0..3 {
            let s = mk_buffered_seal(
                13 + i,
                0,
                TfIndex::M1,
                1_716_000_900 + i as u32,
                100.0 + i as f64,
            );
            p.submit(s, now);
        }
        assert_eq!(p.ring_len(), 3);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_default_pipeline_uses_production_dirs() {
        let p = SealAbsorptionPipeline::default();
        // Ring capacity must match the locked constant.
        assert_eq!(
            p.ring_capacity(),
            tickvault_trading::candles::SEAL_BUFFER_CAPACITY
        );
    }

    #[test]
    fn test_new_pipeline_uses_production_dirs() {
        let p = SealAbsorptionPipeline::new();
        assert_eq!(
            p.ring_capacity(),
            tickvault_trading::candles::SEAL_BUFFER_CAPACITY
        );
    }

    #[test]
    fn test_repeated_submit_after_overflow_keeps_spilling() {
        // After the first overflow, every subsequent submit on a
        // capacity-1 ring must continue to evict-and-spill, NOT silently
        // succeed without spilling.
        let (spill, dlq) = temp_pair("repeated-spill");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(1, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_buffered_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        let s3 = mk_buffered_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0);
        assert_eq!(p.submit(s1, now), SubmitOutcome::Buffered);
        assert_eq!(p.submit(s2, now), SubmitOutcome::Spilled);
        assert_eq!(p.submit(s3, now), SubmitOutcome::Spilled);
        let writer = SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = writer.read_all(now).expect("read spill");
        assert_eq!(drained.len(), 2);
        // First evicted = s1; second evicted = s2.
        assert_eq!(drained[0].security_id, 13);
        assert_eq!(drained[1].security_id, 25);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_submit_preserves_seal_payload_through_spill_round_trip() {
        // The full LiveCandleState (including the 3 Wave-5 pct fields)
        // must round-trip from BufferedSeal → ring → eviction → spill
        // file → on-disk SerializedSeal record without any field loss.
        let (spill, dlq) = temp_pair("preserve-fields");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(1, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M15, 1_716_000_900, 102.5);
        let filler = mk_buffered_seal(99, 0, TfIndex::M1, 1_716_001_500, 200.0);
        p.submit(s1, now);
        p.submit(filler, now);
        let writer = SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = writer.read_all(now).expect("read");
        let recovered = drained[0]
            .try_into_buffered_seal()
            .expect("valid tf_ordinal");
        assert_eq!(recovered, s1);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_all_after_overflow_returns_only_currently_resident_seals() {
        // After overflow has evicted s1 to spill, drain_all must return
        // s2 + s3 only — NOT the evicted s1 (which is now on disk).
        let (spill, dlq) = temp_pair("drain-after-overflow");
        let mut p =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(2, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_buffered_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        let s3 = mk_buffered_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0);
        p.submit(s1, now);
        p.submit(s2, now);
        p.submit(s3, now);
        let mut collected: Vec<u64> = Vec::new();
        p.drain_all(|seal| collected.push(seal.security_id));
        assert_eq!(collected, vec![25, 51]);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_pipeline_dropped_outcome_carries_correct_seal_identity() {
        // When the cascade fully fails, the SubmitOutcome::Dropped
        // payload must be the EVICTED seal (s1, the oldest), NOT the
        // newly-inserted seal (s2, which is now in the ring).
        let mut spill_as_file = std::env::temp_dir();
        spill_as_file.push(format!(
            "tickvault-seal-pipeline-spill-id-blocker-{}-{}",
            "dropped-identity",
            std::process::id()
        ));
        let mut dlq_as_file = std::env::temp_dir();
        dlq_as_file.push(format!(
            "tickvault-seal-pipeline-dlq-id-blocker-{}-{}",
            "dropped-identity",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&spill_as_file);
        let _ = std::fs::remove_dir_all(&spill_as_file);
        let _ = std::fs::remove_file(&dlq_as_file);
        let _ = std::fs::remove_dir_all(&dlq_as_file);
        std::fs::write(&spill_as_file, b"file").expect("blocker");
        std::fs::write(&dlq_as_file, b"file").expect("blocker");
        let mut p = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            1,
            spill_as_file.clone(),
            dlq_as_file.clone(),
        );
        let now = jan1_noon_utc();
        let s1 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_buffered_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0);
        p.submit(s1, now);
        let outcome = p.submit(s2, now);
        match outcome {
            SubmitOutcome::Dropped(lost) => {
                // The OLDEST (s1) was evicted; that's what must be
                // reported as lost — NOT s2 (which is now buffered).
                assert_eq!(lost, s1);
                assert_ne!(lost, s2);
            }
            other => panic!("expected Dropped, got {other:?}"),
        }
        // s2 must still be in the ring.
        assert_eq!(p.ring_len(), 1);
        let _ = std::fs::remove_file(&spill_as_file);
        let _ = std::fs::remove_file(&dlq_as_file);
    }

    #[test]
    fn test_spill_handle_and_dlq_handle_share_the_pipelines_own_writers() {
        // Sharing the INSTANCE is the contract, not the directory: two
        // independent spill writers on one day-file each cache their own
        // append handle behind their own mutex and interleave partial writes.
        let dir = std::env::temp_dir().join(format!("tv-handle-share-{}", std::process::id()));
        let spill_dir = dir.join("spill");
        let dlq_dir = dir.join("dlq");
        std::fs::create_dir_all(&spill_dir).expect("spill dir");
        std::fs::create_dir_all(&dlq_dir).expect("dlq dir");

        let pipeline =
            SealAbsorptionPipeline::with_dirs_for_test(spill_dir.clone(), dlq_dir.clone());
        let a = pipeline.spill_handle();
        let b = pipeline.spill_handle();
        assert!(
            Arc::ptr_eq(&a, &b),
            "spill_handle must hand out the SAME writer, not a clone of its configuration"
        );
        let d1 = pipeline.dlq_handle();
        let d2 = pipeline.dlq_handle();
        assert!(
            Arc::ptr_eq(&d1, &d2),
            "dlq_handle must hand out the SAME writer for the same reason"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- escalation free-space floor (2026-09-01) ---------------------
    //
    // Seal spill was the ONE writer on the volume with no free-space floor,
    // no byte cap and no size prune. Found by adversarial review; the review
    // also proposed the fix that does not work (a floor inside `append_seal`,
    // which only redirects the bytes to the DLQ on the same volume).

    #[test]
    fn the_seal_floor_sits_between_the_tick_and_depth_floors() {
        // The const assert above enforces this at compile time; this test
        // states the ORDERING in the language of the ladder, so a reader who
        // changes one number sees why the other two constrain it.
        assert!(
            SEAL_ESCALATION_MIN_FREE_BYTES > SPILL_MIN_FREE_HEADROOM_BYTES,
            "seals must be refused BEFORE ticks: a tick is a decision input, \
             a seal is derived from ticks"
        );
        assert!(
            SEAL_ESCALATION_MIN_FREE_BYTES < DEPTH_SPILL_MIN_FREE_HEADROOM_BYTES,
            "depth must be refused BEFORE seals: depth is record-only and \
             carries ~24x the row volume"
        );
    }

    #[test]
    fn has_room_to_escalate_fails_open_when_the_probe_cannot_answer() {
        // A directory that does not exist: `spill_free_bytes` returns None.
        // Blind must ALLOW here — unlike depth — because a refused seal is a
        // candle that exists nowhere else, and one broken `df` must never be
        // able to start discarding candles.
        let mut missing = std::env::temp_dir();
        missing.push(format!(
            "tickvault-seal-floor-absent-{}-{}",
            std::process::id(),
            "blind"
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let dlq = missing.join("dlq");
        let pipeline = SealAbsorptionPipeline::with_dirs_for_test(missing.clone(), dlq);
        assert!(
            pipeline.has_room_to_escalate(128, 1_000),
            "a blind probe must fail OPEN for the seal tier"
        );
    }

    #[test]
    fn has_room_to_escalate_refuses_when_the_floor_cannot_be_left_behind() {
        let (spill, dlq) = temp_pair("floor-refuse");
        let pipeline = SealAbsorptionPipeline::with_dirs_for_test(spill, dlq);
        // A payload so large that `payload + floor` cannot fit on any real
        // volume. This exercises the REFUSE arm deterministically without
        // needing to actually fill a disk.
        assert!(
            !pipeline.has_room_to_escalate(u64::MAX / 2, 2_000),
            "a write that cannot leave the floor behind must be refused"
        );
        // And an ordinary 128-byte seal on a working volume must still land.
        assert!(
            pipeline.has_room_to_escalate(128, 2_000),
            "the floor must not refuse an ordinary seal on a healthy volume — \
             a floor that always refuses is not a floor, it is an outage"
        );
    }

    #[test]
    fn the_free_space_probe_is_throttled_so_a_burst_costs_one_probe() {
        // `spill_free_bytes` forks `df`, and this path is reachable from the
        // frame drain. An unthrottled probe would spawn a process per seal
        // during exactly the burst it exists to survive.
        let (spill, dlq) = temp_pair("floor-throttle");
        let pipeline = SealAbsorptionPipeline::with_dirs_for_test(spill, dlq);

        assert!(pipeline.has_room_to_escalate(128, 10_000));
        let first = pipeline.last_probe_secs.load(Ordering::Relaxed);
        assert_eq!(first, 10_000, "the first call must probe");

        // Inside the interval: the stamp must NOT move, i.e. no new fork.
        for t in 10_001..10_000 + SEAL_DISK_PROBE_INTERVAL_SECS {
            assert!(pipeline.has_room_to_escalate(128, t));
            assert_eq!(
                pipeline.last_probe_secs.load(Ordering::Relaxed),
                first,
                "a call {t} seconds in must reuse the cached verdict"
            );
        }

        // At the interval boundary it probes again.
        assert!(pipeline.has_room_to_escalate(128, 10_000 + SEAL_DISK_PROBE_INTERVAL_SECS));
        assert_eq!(
            pipeline.last_probe_secs.load(Ordering::Relaxed),
            10_000 + SEAL_DISK_PROBE_INTERVAL_SECS,
            "the cache must expire after the interval, or a disk that filled \
             up mid-session would never be noticed"
        );
    }
}
