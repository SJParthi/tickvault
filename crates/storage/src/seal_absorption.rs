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

use tracing::warn;

use tickvault_trading::candles::{BufferOutcome, BufferedSeal, SealRing};

use crate::seal_dlq::{SealDlqRecord, SealDlqWriter};
use crate::seal_spill::{SealSpillWriter, SerializedSeal};

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
    spill: SealSpillWriter,
    dlq: SealDlqWriter,
}

impl SealAbsorptionPipeline {
    /// Production constructor. Uses the locked
    /// `SEAL_BUFFER_CAPACITY` ring + `data/spill/` + `data/dlq/`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ring: SealRing::new(),
            spill: SealSpillWriter::new(),
            dlq: SealDlqWriter::new(),
        }
    }

    /// Test constructor. Tests inject isolated `tempdir` paths so they
    /// can run in parallel without filesystem contention.
    #[must_use]
    // TEST-EXEMPT: test-only helper used as construction source by every test in this module (ring-overflow, spill-fail, DLQ-fail, drain, FIFO ordering scenarios all build via this factory).
    pub fn with_dirs_for_test(spill_dir: PathBuf, dlq_dir: PathBuf) -> Self {
        Self {
            ring: SealRing::new(),
            spill: SealSpillWriter::with_spill_dir_for_test(spill_dir),
            dlq: SealDlqWriter::with_dlq_dir_for_test(dlq_dir),
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
            spill: SealSpillWriter::with_spill_dir_for_test(spill_dir),
            dlq: SealDlqWriter::with_dlq_dir_for_test(dlq_dir),
        }
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

    /// Tier 2 + tier 3 escalation chain for an evicted seal.
    fn escalate_evicted(&self, evicted: BufferedSeal, now_unix_secs: i64) -> SubmitOutcome {
        let serialised = SerializedSeal::from(&evicted);
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
        let s1 = mk_buffered_seal(13, 0, TfIndex::H4, 1_716_000_900, 102.5);
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
}
