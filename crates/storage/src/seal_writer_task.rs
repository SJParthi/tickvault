//! Wave 6 Sub-PR #1 item 1.2f.3 — sealed-candle writer-task drain logic.
//!
//! The COLD-PATH consumer that drains the [`SealAbsorptionPipeline`]
//! ring, batches into the [`ShadowCandleWriter`] ILP buffer, attempts
//! a flush, and on flush failure rescues the in-flight seals via
//! [`SealAbsorptionPipeline::rescue_in_flight`] (escalating to disk
//! spill → NDJSON DLQ → `Dropped`).
//!
//! ## Why a sync `drain_once` and not a tokio task here
//!
//! Per Wave-6 plan Item 1.2f.3 — the SLICE that lands the actual
//! `tokio::spawn` loop + interval timer + cancellation token is item
//! 1.2f.4. THIS slice ships the pure synchronous drain function so
//! the rescue cascade is testable end-to-end without spinning up a
//! tokio runtime + async test harness.
//!
//! The eventual tokio loop in 1.2f.4 will be a thin shell that:
//! ```ignore
//! loop {
//!     tokio::time::sleep(SEAL_DRAIN_INTERVAL).await;
//!     let outcome = drain_once(&mut pipeline, &mut writer, MAX_DRAIN, now_unix_secs());
//!     // … emit Prom counters per outcome.field …
//! }
//! ```
//!
//! ## What this slice ships
//!
//! - [`DrainOutcome`] — counts every outcome category for one drain
//!   cycle (idle, flushed, rescued-to-spill, rescued-to-dlq,
//!   rescued-dropped). Maps 1:1 to the future
//!   `tv_seal_writer_drain_total{kind=...}` Prometheus counter.
//! - [`drain_once`] — the function itself (sync, single drain cycle).
//! - 11 unit tests covering: idle path; bounded drain
//!   (`max_drain` cap); flush-fail-rescue-to-spill (full cascade
//!   verified by inspecting the spill file); flush-fail-rescue-to-dlq
//!   (spill blocked → DLQ catches); flush-fail-rescue-dropped (both
//!   blocked → caller-must-fire AGGREGATOR-DROP-01); ring drained to
//!   zero on rescue (no FIFO inversion); pending_count semantics on
//!   rescue.
//!
//! ## Hot-path safety
//!
//! `drain_once` is COLD path (writer task only). It is allowed to
//! allocate (`Vec::with_capacity(max_drain)` rescue buffer) — the
//! hot-path zero-alloc rule applies to `MultiTfAggregator::consume_tick`
//! and below, NOT to the storage drain task.

use tracing::error;

use tickvault_common::error_code::ErrorCode;
use tickvault_trading::candles::BufferedSeal;

use crate::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use crate::shadow_candle_writer::ShadowCandleWriter;

/// Outcome counters for one [`drain_once`] cycle. Maps 1:1 to the
/// future `tv_seal_writer_drain_total{kind="ring_pop"|"flushed"|...}`
/// Prometheus counter labels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DrainOutcome {
    /// Number of seals popped from the ring this cycle.
    /// `0` means the ring was empty (idle path).
    pub ring_seals_popped: usize,
    /// `true` if the ILP `flush()` returned `Ok(())` and the popped
    /// seals are now committed to QuestDB.
    pub flushed_ok: bool,
    /// On flush failure: how many of the in-flight seals landed in
    /// the disk spill tier (binary fixed-record file).
    pub rescued_to_spill: usize,
    /// On flush failure: how many landed in the NDJSON DLQ tier
    /// (because spill was full / failed).
    pub rescued_to_dlq: usize,
    /// On flush failure: how many were truly dropped (all 3 tiers
    /// failed). Caller MUST fire
    /// `error!(code = ErrorCode::AggregatorDrop01.code_str(), …)`
    /// per the AGGREGATOR-DROP-01 runbook for each.
    pub rescued_dropped: usize,
}

impl DrainOutcome {
    /// `true` if no seals were popped (the ring was empty).
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.ring_seals_popped == 0
    }

    /// `true` if any seals were rescued to spill / DLQ / dropped
    /// because flush failed.
    #[must_use]
    pub const fn has_rescues(&self) -> bool {
        self.rescued_to_spill > 0 || self.rescued_to_dlq > 0 || self.rescued_dropped > 0
    }
}

/// Drain up to `max_drain` seals from the [`SealAbsorptionPipeline`]
/// ring, batch them into the [`ShadowCandleWriter`] ILP buffer,
/// attempt a flush, and on flush failure rescue the in-flight seals
/// down the spill → DLQ → drop cascade.
///
/// Single drain cycle — caller is responsible for calling this in a
/// loop on whatever cadence is appropriate (the future tokio task in
/// item 1.2f.4 sleeps `SEAL_DRAIN_INTERVAL` between cycles).
///
/// **Cold path.** Allowed to allocate; allocates one
/// `Vec::with_capacity(max_drain)` rescue buffer per call. Reused
/// across calls in the future tokio loop via a writer-task struct
/// holding the buffer; this slice's pure function takes the simpler
/// once-per-call allocation.
///
/// **No FIFO inversion on rescue.** Popped seals are NOT pushed back
/// into the ring (which would put them at the BACK and invert
/// drain order). Instead they go straight to disk spill via
/// [`SealAbsorptionPipeline::rescue_in_flight`].
///
/// `now_unix_secs` is the UTC unix timestamp passed by the caller
/// per locked decision **L-H7** — never `Utc::now()` here.
pub fn drain_once(
    pipeline: &mut SealAbsorptionPipeline,
    writer: &mut ShadowCandleWriter,
    max_drain: usize,
    now_unix_secs: i64,
) -> DrainOutcome {
    let mut outcome = DrainOutcome::default();

    // Idle short-circuit.
    if pipeline.ring_len() == 0 || max_drain == 0 {
        return outcome;
    }

    // Pop up to max_drain seals into the rescue buffer. We pop FIRST
    // (so the ring is drained), THEN attempt flush. If flush fails,
    // every popped seal cascades to spill/DLQ/drop — they do NOT
    // re-enter the ring (would invert FIFO).
    let mut popped: Vec<BufferedSeal> = Vec::with_capacity(max_drain);
    while popped.len() < max_drain {
        match pipeline.pop_oldest() {
            Some(seal) => {
                if let Err(append_err) = writer.append_seal(&seal) {
                    // ILP buffer-fill error (extremely rare — column
                    // type / table-name issue). Rescue this one
                    // immediately and stop draining further.
                    // Finding S3: seal-time ILP append failure is a
                    // persist failure — logged at `error!` with the
                    // AGGREGATOR-SEAL-01 code per
                    // `error_level_meta_guard.rs` Rule 5.
                    error!(
                        code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                        ?append_err,
                        security_id = seal.security_id,
                        "candle append_seal failed — rescuing in-flight"
                    );
                    rescue_one(pipeline, &mut outcome, seal, now_unix_secs);
                    break;
                }
                popped.push(seal);
            }
            None => break, // ring exhausted
        }
    }

    outcome.ring_seals_popped = popped.len();

    // Idle if nothing landed in the writer (ring was racing-empty by
    // the time we started, or every append failed).
    if outcome.ring_seals_popped == 0 {
        return outcome;
    }

    // Attempt the flush. On success: drop the rescue buffer (all
    // committed). On failure: cascade EVERY popped seal through the
    // rescue path.
    match writer.flush() {
        Ok(()) => {
            outcome.flushed_ok = true;
        }
        Err(flush_err) => {
            // Finding S3: seal-time ILP flush failure is a persist
            // failure — logged at `error!` with the AGGREGATOR-SEAL-01
            // code per `error_level_meta_guard.rs` Rule 5.
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?flush_err,
                count = popped.len(),
                "candle flush failed — rescuing in-flight seals to spill/DLQ"
            );
            for seal in popped {
                rescue_one(pipeline, &mut outcome, seal, now_unix_secs);
            }
        }
    }

    outcome
}

/// Walks one in-flight seal through the rescue cascade and updates
/// the `DrainOutcome` counters.
fn rescue_one(
    pipeline: &SealAbsorptionPipeline,
    outcome: &mut DrainOutcome,
    seal: BufferedSeal,
    now_unix_secs: i64,
) {
    match pipeline.rescue_in_flight(seal, now_unix_secs) {
        SubmitOutcome::Spilled => outcome.rescued_to_spill += 1,
        SubmitOutcome::DlqWritten => outcome.rescued_to_dlq += 1,
        SubmitOutcome::Dropped(_) => outcome.rescued_dropped += 1,
        // SubmitOutcome::Buffered cannot occur via rescue_in_flight
        // (it skips the ring). If it ever does, treat as a logic
        // bug and surface for triage; for now we count nothing.
        SubmitOutcome::Buffered => {}
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
            "tickvault-seal-writer-task-spill-{}-{}",
            name,
            std::process::id()
        ));
        dlq.push(format!(
            "tickvault-seal-writer-task-dlq-{}-{}",
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
    fn test_drain_once_returns_idle_when_ring_is_empty() {
        let (spill, dlq) = temp_pair("idle");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, jan1_noon_utc());
        assert!(outcome.is_idle());
        assert_eq!(outcome.ring_seals_popped, 0);
        assert!(!outcome.flushed_ok);
        assert!(!outcome.has_rescues());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_with_max_drain_zero_is_idle() {
        let (spill, dlq) = temp_pair("max-zero");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        pipeline.submit(
            mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0),
            jan1_noon_utc(),
        );
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 0, jan1_noon_utc());
        assert!(outcome.is_idle());
        assert_eq!(pipeline.ring_len(), 1, "ring untouched on max_drain=0");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_caps_at_max_drain() {
        // Ring has 5 seals, max_drain is 3 → only 3 popped.
        let (spill, dlq) = temp_pair("cap");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        for i in 0..5 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_000_900 + i as u32,
                    100.0 + i as f64,
                ),
                jan1_noon_utc(),
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 3, jan1_noon_utc());
        assert_eq!(outcome.ring_seals_popped, 3);
        assert_eq!(pipeline.ring_len(), 2, "2 seals must remain in ring");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_disconnected_writer_rescues_all_to_spill() {
        // ShadowCandleWriter::for_test() always errs on flush. With
        // a healthy spill dir, every popped seal must rescue to
        // spill (tier 2) successfully.
        let (spill, dlq) = temp_pair("rescue-spill");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        for i in 0..5 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_000_900 + i as u32,
                    100.0 + i as f64,
                ),
                now,
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 5);
        assert!(!outcome.flushed_ok, "disconnected writer can't flush");
        assert_eq!(outcome.rescued_to_spill, 5);
        assert_eq!(outcome.rescued_to_dlq, 0);
        assert_eq!(outcome.rescued_dropped, 0);
        assert_eq!(pipeline.ring_len(), 0, "ring fully drained");

        // Verify spill file actually contains the 5 evicted seals.
        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = spill_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 5);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_disconnected_writer_rescues_to_dlq_when_spill_blocked() {
        // Block tier-2 spill by pointing it at a regular file (so
        // create_dir_all errs); tier-3 DLQ has a real dir → rescue
        // lands in DLQ.
        let mut spill_blocker = std::env::temp_dir();
        spill_blocker.push(format!(
            "tickvault-seal-writer-task-spill-blocker-{}-{}",
            "rescue-dlq",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&spill_blocker);
        let _ = std::fs::remove_dir_all(&spill_blocker);
        std::fs::write(&spill_blocker, b"not a dir").expect("create blocker");

        let (_spill_unused, dlq) = temp_pair("rescue-dlq");
        let mut pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            8,
            spill_blocker.clone(),
            dlq.clone(),
        );
        let now = jan1_noon_utc();
        for i in 0..3 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_000_900 + i as u32,
                    100.0 + i as f64,
                ),
                now,
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 3);
        assert!(!outcome.flushed_ok);
        assert_eq!(outcome.rescued_to_spill, 0);
        assert_eq!(outcome.rescued_to_dlq, 3);
        assert_eq!(outcome.rescued_dropped, 0);

        // Verify DLQ file actually contains 3 records.
        let dlq_writer = crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(dlq.clone());
        let drained = dlq_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 3);

        let _ = std::fs::remove_file(&spill_blocker);
        cleanup(&spill_blocker, &dlq);
    }

    #[test]
    fn test_drain_once_disconnected_writer_drops_when_spill_and_dlq_blocked() {
        // Block BOTH tier-2 + tier-3 → rescued_dropped counter
        // increments for every popped seal.
        let mut spill_blocker = std::env::temp_dir();
        spill_blocker.push(format!(
            "tickvault-seal-writer-task-spill-blocker2-{}-{}",
            "rescue-dropped",
            std::process::id()
        ));
        let mut dlq_blocker = std::env::temp_dir();
        dlq_blocker.push(format!(
            "tickvault-seal-writer-task-dlq-blocker-{}-{}",
            "rescue-dropped",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&spill_blocker);
        let _ = std::fs::remove_dir_all(&spill_blocker);
        let _ = std::fs::remove_file(&dlq_blocker);
        let _ = std::fs::remove_dir_all(&dlq_blocker);
        std::fs::write(&spill_blocker, b"file").expect("blocker");
        std::fs::write(&dlq_blocker, b"file").expect("blocker");

        let mut pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            8,
            spill_blocker.clone(),
            dlq_blocker.clone(),
        );
        let now = jan1_noon_utc();
        for i in 0..2 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_000_900 + i as u32,
                    100.0 + i as f64,
                ),
                now,
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 2);
        assert!(!outcome.flushed_ok);
        assert_eq!(outcome.rescued_to_spill, 0);
        assert_eq!(outcome.rescued_to_dlq, 0);
        assert_eq!(
            outcome.rescued_dropped, 2,
            "both seals MUST count as dropped when spill+DLQ blocked"
        );

        let _ = std::fs::remove_file(&spill_blocker);
        let _ = std::fs::remove_file(&dlq_blocker);
    }

    #[test]
    fn test_drain_outcome_default_is_zeroed_idle() {
        let o = DrainOutcome::default();
        assert!(o.is_idle());
        assert!(!o.has_rescues());
        assert_eq!(o.ring_seals_popped, 0);
        assert!(!o.flushed_ok);
        assert_eq!(o.rescued_to_spill, 0);
        assert_eq!(o.rescued_to_dlq, 0);
        assert_eq!(o.rescued_dropped, 0);
    }

    #[test]
    fn test_drain_outcome_is_idle_returns_false_when_seals_popped() {
        let o = DrainOutcome {
            ring_seals_popped: 1,
            ..DrainOutcome::default()
        };
        assert!(!o.is_idle());
    }

    #[test]
    fn test_drain_outcome_has_rescues_for_each_kind() {
        for o in [
            DrainOutcome {
                rescued_to_spill: 1,
                ..DrainOutcome::default()
            },
            DrainOutcome {
                rescued_to_dlq: 1,
                ..DrainOutcome::default()
            },
            DrainOutcome {
                rescued_dropped: 1,
                ..DrainOutcome::default()
            },
        ] {
            assert!(o.has_rescues(), "{o:?} must report has_rescues");
        }
    }

    #[test]
    fn test_drain_once_does_not_re_buffer_rescued_seals_in_ring() {
        // FIFO invariant: rescued seals MUST go straight to spill,
        // NOT back into the ring. Verify ring_len returns to 0 even
        // though flush failed.
        let (spill, dlq) = temp_pair("no-rebuffer");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0), now);
        pipeline.submit(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0), now);
        let mut writer = ShadowCandleWriter::for_test();
        drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(
            pipeline.ring_len(),
            0,
            "rescued seals MUST go to spill, NOT re-buffer into ring"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_preserves_seal_identity_through_rescue() {
        // I-P1-11 + payload integrity: same security_id with two
        // different segments rescues both records to spill, distinct.
        let (spill, dlq) = temp_pair("identity");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0), now);
        pipeline.submit(mk_seal(13, 1, TfIndex::M1, 1_716_001_500, 200.0), now);
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 2);
        assert_eq!(outcome.rescued_to_spill, 2);

        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = spill_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        // First popped = oldest = seg 0 (added first).
        assert_eq!(drained[0].security_id, 13);
        assert_eq!(drained[0].exchange_segment_code, 0);
        assert_eq!(drained[0].close, 100.0);
        assert_eq!(drained[1].security_id, 13);
        assert_eq!(drained[1].exchange_segment_code, 1);
        assert_eq!(drained[1].close, 200.0);
        cleanup(&spill, &dlq);
    }
}
