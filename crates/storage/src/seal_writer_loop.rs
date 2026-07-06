//! Wave 6 Sub-PR #1 item 1.2f.5 — sealed-candle writer-task tokio loop.
//!
//! Long-running async wrapper that calls
//! [`SealWriterRunner::run_one_cycle`] on a fixed interval, with
//! cooperative cancellation via a [`tokio::sync::watch`] channel.
//! The future boot wiring (item 1.4) spawns this task at app
//! startup; the shutdown path flips the cancellation flag, the loop
//! does ONE final drain, then exits gracefully.
//!
//! ## What this slice ships
//!
//! - [`SEAL_DRAIN_INTERVAL_MS`] — locked drain interval constant
//!   (100 ms — bounds the worst-case in-flight latency from
//!   `aggregator.consume_tick → ILP commit` to ~100 ms +
//!   one `flush()` round-trip).
//! - [`run_seal_writer_loop`] — the async fn; runs forever until
//!   cancelled, returns on cancel after one final drain cycle so no
//!   buffered seal is lost.
//! - 5 `#[tokio::test]` cases covering: idle ticks; submit-then-tick
//!   processes seals; cancellation exits the loop; cancellation does
//!   ONE final drain (`final_drain_outcome` non-idle); `MissedTickBehavior::Skip`
//!   ratcheted (no thundering-herd if the runtime stalls).
//!
//! ## 2026-07-06 exam-fix — the loop is no longer silent
//!
//! Before 2026-07-06 the loop DROPPED every `CycleOutcome` (the item-1.4
//! counter fan-out was never wired) and logged non-idle cycles at `debug!`
//! only. On the 2026-07-06 groww-only live exam that meant ~71K sealed
//! candles produced ZERO candle-table rows AND ZERO log lines — total
//! silence. The loop now:
//!
//! - fans every cycle into the `tv_seal_writer_drain_total{kind=...}`
//!   Prometheus counters (the counter name promised by
//!   `seal_writer_task.rs` since Wave 6 Sub-PR #1),
//! - fires `error!(code = AGGREGATOR-DROP-01)` whenever a cycle reports
//!   truly-dropped seals (the `CycleOutcome` doc contract the old loop
//!   violated by discarding the outcome),
//! - emits an UNCONDITIONAL once-per-60s `info!` progress report
//!   ([`SEAL_WRITER_PROGRESS_REPORT_SECS`]) carrying rows-written /
//!   rescued / dropped counts since the last report — so silence can
//!   never again mean "unknown".
//!
//! ## What this slice does NOT ship
//!
//! - Boot wiring + `tokio::spawn` of this loop into `main.rs` (item 1.4).
//!
//! ## Why `tokio::sync::watch` and not `tokio_util::CancellationToken`
//!
//! `tokio-util` is NOT a workspace dependency today. Adding it
//! requires Parthiban approval per CLAUDE.md "New dep additions need
//! Parthiban approval". `tokio::sync::watch` ships with the existing
//! `tokio` dep and provides identical semantics
//! (`watch::Sender::send(true)` flips a flag; the receiver's
//! `.changed().await` wakes pending tasks). The other long-running
//! tasks in the codebase (`ip_monitor.rs`, `depth_rebalancer.rs`)
//! already use this pattern.

use std::time::Duration;

use chrono::Utc;
use tokio::sync::watch;
use tracing::{debug, error, info};

use tickvault_common::error_code::ErrorCode;

use crate::seal_writer_runner::{CycleOutcome, SealWriterRunner};

/// Drain interval — the loop wakes every 100 ms during normal
/// operation and drains the mpsc + ring through the ILP buffer.
/// Sized to bound the producer→ILP latency at ~100 ms + one flush
/// round-trip. A future SLO ratchet may tighten this; for now it
/// matches the existing tick-persistence drain cadence.
pub const SEAL_DRAIN_INTERVAL_MS: u64 = 100;

/// Cadence of the UNCONDITIONAL positive progress report (2026-07-06
/// exam-fix). Every 60 s the loop `info!`s the rows written / rescued /
/// dropped since the last report — even when everything is zero — so a
/// silent seal-writer leg can never again be mistaken for a healthy one.
pub const SEAL_WRITER_PROGRESS_REPORT_SECS: u64 = 60;

/// Convenience accessor for the drain interval as a `Duration`.
#[must_use]
pub const fn seal_drain_interval() -> Duration {
    Duration::from_millis(SEAL_DRAIN_INTERVAL_MS)
}

/// Rolling accumulation of [`CycleOutcome`]s between two progress reports.
/// Pure (no I/O) so every transition is unit-testable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SealWriterProgress {
    /// Cycles absorbed since the last report (idle cycles included).
    pub cycles: u64,
    /// Seals drained from the producer mpsc into the pipeline.
    pub submitted: u64,
    /// Seals committed to QuestDB by a successful flush (lower bound —
    /// a flush that also commits rows retained from an earlier failed
    /// batch counts only the current cycle's popped seals).
    pub flushed_rows: u64,
    /// Cycles whose flush failed (each already fired AGGREGATOR-SEAL-01
    /// inside `drain_once`).
    pub flush_failures: u64,
    /// Seals rescued to the disk-spill tier (submit-side overflow +
    /// drain-side flush-failure rescue combined).
    pub rescued_to_spill: u64,
    /// Seals rescued to the NDJSON DLQ tier.
    pub rescued_to_dlq: u64,
    /// Seals truly dropped (ring + spill + DLQ all failed) — each cycle
    /// with a non-zero count fires `error!(code = AGGREGATOR-DROP-01)`.
    pub dropped: u64,
}

impl SealWriterProgress {
    /// Folds one cycle outcome into the rolling counts. Returns the
    /// number of seals truly dropped THIS cycle so the caller can fire
    /// the AGGREGATOR-DROP-01 error on a per-cycle basis.
    pub fn absorb(&mut self, outcome: &CycleOutcome) -> u64 {
        self.cycles = self.cycles.saturating_add(1);
        self.submitted = self
            .submitted
            .saturating_add(outcome.submitted_from_mpsc as u64);
        if outcome.drain.flushed_ok {
            self.flushed_rows = self
                .flushed_rows
                .saturating_add(outcome.drain.ring_seals_popped as u64);
        } else if outcome.drain.ring_seals_popped > 0 {
            self.flush_failures = self.flush_failures.saturating_add(1);
        }
        self.rescued_to_spill = self
            .rescued_to_spill
            .saturating_add((outcome.mpsc_submit_spilled + outcome.drain.rescued_to_spill) as u64);
        self.rescued_to_dlq = self
            .rescued_to_dlq
            .saturating_add((outcome.mpsc_submit_dlq + outcome.drain.rescued_to_dlq) as u64);
        let dropped_this_cycle =
            (outcome.mpsc_submit_dropped + outcome.drain.rescued_dropped) as u64;
        self.dropped = self.dropped.saturating_add(dropped_this_cycle);
        dropped_this_cycle
    }

    /// Returns the accumulated snapshot and resets the counts for the
    /// next reporting window.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

/// Per-cycle observability fan-out (2026-07-06 exam-fix): Prometheus
/// counters for every non-zero outcome dimension + the AGGREGATOR-DROP-01
/// `error!` the `CycleOutcome` contract requires for truly-dropped seals.
/// Cold path — static counter labels only, no allocation.
fn record_cycle_observability(outcome: &CycleOutcome, dropped_this_cycle: u64) {
    if outcome.submitted_from_mpsc > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "submitted")
            .increment(outcome.submitted_from_mpsc as u64);
    }
    if outcome.drain.flushed_ok && outcome.drain.ring_seals_popped > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "flushed_rows")
            .increment(outcome.drain.ring_seals_popped as u64);
    }
    if !outcome.drain.flushed_ok && outcome.drain.ring_seals_popped > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "flush_failed").increment(1);
    }
    let spilled = (outcome.mpsc_submit_spilled + outcome.drain.rescued_to_spill) as u64;
    if spilled > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "rescued_spill")
            .increment(spilled);
    }
    let dlq = (outcome.mpsc_submit_dlq + outcome.drain.rescued_to_dlq) as u64;
    if dlq > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "rescued_dlq").increment(dlq);
    }
    if dropped_this_cycle > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "dropped")
            .increment(dropped_this_cycle);
        // The CycleOutcome contract: truly-dropped seals (ring + spill +
        // DLQ all failed) MUST fire AGGREGATOR-DROP-01 — silent data loss
        // for a sealed candle is the ONLY thing this code path can mean.
        error!(
            code = ErrorCode::AggregatorDrop01.code_str(),
            dropped = dropped_this_cycle,
            "sealed candles DROPPED after ring+spill+DLQ all failed — see AGGREGATOR-DROP-01 runbook"
        );
    }
}

/// Emits the unconditional once-per-window positive progress report.
/// `info!` even when every count is zero — an idle seal writer must say
/// so out loud (2026-07-06 exam-fix: silence can never mean unknown).
fn emit_progress_report(snapshot: &SealWriterProgress, ring_len: usize) {
    info!(
        cycles = snapshot.cycles,
        submitted = snapshot.submitted,
        rows_written = snapshot.flushed_rows,
        flush_failures = snapshot.flush_failures,
        rescued_to_spill = snapshot.rescued_to_spill,
        rescued_to_dlq = snapshot.rescued_to_dlq,
        dropped = snapshot.dropped,
        ring_len,
        "seal writer progress (last 60s window)"
    );
}

/// Returns the current UTC unix timestamp in seconds. Used to
/// derive the IST-date filename for spill / DLQ via the
/// downstream `now_unix_secs` parameter — per locked decision
/// **L-H7** the SHARED storage layer takes the wall-clock from the
/// caller, never `Utc::now()` inside the hot path. The writer task
/// is COLD path so this `Utc::now()` call IS allowed here.
fn utc_now_secs() -> i64 {
    Utc::now().timestamp()
}

/// Long-running tokio task. Calls `runner.run_one_cycle` on a fixed
/// 100 ms interval until cancelled. On cancellation, performs ONE
/// final drain cycle so no buffered seal is silently lost.
///
/// `cancel_rx` is the receive end of a `tokio::sync::watch::channel`.
/// The producer side (typically `main.rs` shutdown handler) flips the
/// flag via `cancel_tx.send(true)`. The loop wakes immediately and
/// exits after one final drain.
///
/// Returns the outcome of the final post-cancel drain so the caller
/// can log / surface it for graceful-shutdown observability.
pub async fn run_seal_writer_loop(
    mut runner: SealWriterRunner,
    interval: Duration,
    mut cancel_rx: watch::Receiver<bool>,
) -> CycleOutcome {
    info!(
        interval_ms = interval.as_millis(),
        max_drain = runner.max_drain_per_cycle(),
        "seal writer loop starting"
    );
    let mut ticker = tokio::time::interval(interval);
    // If the runtime stalls and we miss multiple ticks (e.g. tokio
    // reactor was pinned by another task), DO NOT fire all the
    // missed ticks back-to-back (which would burn CPU on a
    // back-to-back drain barrage). Skip them; the next cycle is
    // sufficient to catch up.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 2026-07-06 exam-fix: rolling progress + per-cycle observability so
    // the seal-writer leg can never again go dark (see module docs).
    let mut progress = SealWriterProgress::default();
    let mut last_report = tokio::time::Instant::now();
    let report_every = Duration::from_secs(SEAL_WRITER_PROGRESS_REPORT_SECS);

    loop {
        tokio::select! {
            biased; // Prefer the cancel branch so a pending shutdown
                    // is not held up by an extra tick.
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    info!("seal writer loop cancelled — performing final drain");
                    let now = utc_now_secs();
                    let final_outcome = runner.run_one_cycle(now);
                    let dropped = progress.absorb(&final_outcome);
                    record_cycle_observability(&final_outcome, dropped);
                    emit_progress_report(&progress.take(), runner.ring_len());
                    info!(
                        submitted_from_mpsc = final_outcome.submitted_from_mpsc,
                        rescued_to_spill = final_outcome.drain.rescued_to_spill,
                        rescued_to_dlq = final_outcome.drain.rescued_to_dlq,
                        rescued_dropped = final_outcome.drain.rescued_dropped,
                        "seal writer loop final drain complete"
                    );
                    return final_outcome;
                }
                // The watch receiver's `.changed()` future also
                // resolves on the initial `false → false` change
                // (which doesn't happen) and on sender drop; ignore.
            }
            _ = ticker.tick() => {
                let now = utc_now_secs();
                let outcome = runner.run_one_cycle(now);
                let dropped = progress.absorb(&outcome);
                record_cycle_observability(&outcome, dropped);
                if !outcome.is_idle() {
                    debug!(
                        submitted_from_mpsc = outcome.submitted_from_mpsc,
                        ring_seals_popped = outcome.drain.ring_seals_popped,
                        flushed_ok = outcome.drain.flushed_ok,
                        "seal writer cycle"
                    );
                }
                if last_report.elapsed() >= report_every {
                    emit_progress_report(&progress.take(), runner.ring_len());
                    last_report = tokio::time::Instant::now();
                }
            }
        }
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
    use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

    fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
        let mut spill = std::env::temp_dir();
        let mut dlq = std::env::temp_dir();
        spill.push(format!(
            "tickvault-seal-loop-spill-{}-{}",
            name,
            std::process::id()
        ));
        dlq.push(format!(
            "tickvault-seal-loop-dlq-{}-{}",
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
    fn test_seal_drain_interval_constant_pinned() {
        // Pin the locked interval so a regression PR can't silently
        // raise it (which would balloon producer→ILP latency).
        assert_eq!(SEAL_DRAIN_INTERVAL_MS, 100);
        assert_eq!(seal_drain_interval(), Duration::from_millis(100));
    }

    #[test]
    fn test_progress_report_interval_constant_pinned() {
        // 2026-07-06 exam-fix ratchet: the unconditional positive progress
        // report fires once per 60s. Raising it widens the window in which
        // a dead seal-writer leg is invisible; removing it restores the
        // silent-candle-persist bug.
        assert_eq!(SEAL_WRITER_PROGRESS_REPORT_SECS, 60);
    }

    #[test]
    fn test_progress_absorb_accumulates_every_dimension() {
        use crate::seal_writer_task::DrainOutcome;
        let mut p = SealWriterProgress::default();

        // Cycle 1: healthy flush of 3 seals.
        let healthy = CycleOutcome {
            submitted_from_mpsc: 3,
            mpsc_submit_buffered: 3,
            drain: DrainOutcome {
                ring_seals_popped: 3,
                flushed_ok: true,
                ..DrainOutcome::default()
            },
            ..CycleOutcome::default()
        };
        assert_eq!(p.absorb(&healthy), 0, "healthy cycle drops nothing");

        // Cycle 2: flush failure — 2 popped, 1 rescued to spill, 1 to DLQ.
        let failed = CycleOutcome {
            submitted_from_mpsc: 2,
            mpsc_submit_buffered: 2,
            drain: DrainOutcome {
                ring_seals_popped: 2,
                flushed_ok: false,
                rescued_to_spill: 1,
                rescued_to_dlq: 1,
                ..DrainOutcome::default()
            },
            ..CycleOutcome::default()
        };
        assert_eq!(p.absorb(&failed), 0);

        // Cycle 3: submit-side overflow spill + a truly-dropped seal on
        // BOTH the submit and drain sides.
        let dropping = CycleOutcome {
            submitted_from_mpsc: 3,
            mpsc_submit_spilled: 1,
            mpsc_submit_dropped: 1,
            drain: DrainOutcome {
                ring_seals_popped: 1,
                flushed_ok: false,
                rescued_dropped: 1,
                ..DrainOutcome::default()
            },
            ..CycleOutcome::default()
        };
        assert_eq!(
            p.absorb(&dropping),
            2,
            "absorb must return this cycle's truly-dropped count (submit + drain)"
        );

        assert_eq!(p.cycles, 3);
        assert_eq!(p.submitted, 8);
        assert_eq!(p.flushed_rows, 3, "only the flushed_ok cycle counts rows");
        assert_eq!(p.flush_failures, 2);
        assert_eq!(p.rescued_to_spill, 2, "submit-spill + drain-rescue-spill");
        assert_eq!(p.rescued_to_dlq, 1);
        assert_eq!(p.dropped, 2);
    }

    #[test]
    fn test_progress_take_returns_snapshot_and_resets() {
        use crate::seal_writer_task::DrainOutcome;
        let mut p = SealWriterProgress::default();
        let outcome = CycleOutcome {
            submitted_from_mpsc: 1,
            mpsc_submit_buffered: 1,
            drain: DrainOutcome {
                ring_seals_popped: 1,
                flushed_ok: true,
                ..DrainOutcome::default()
            },
            ..CycleOutcome::default()
        };
        let _ = p.absorb(&outcome);
        let snapshot = p.take();
        assert_eq!(snapshot.cycles, 1);
        assert_eq!(snapshot.flushed_rows, 1);
        assert_eq!(
            p,
            SealWriterProgress::default(),
            "take() must reset the rolling window"
        );
    }

    #[test]
    fn test_progress_idle_cycle_still_counts_the_cycle() {
        // The 60s report is UNCONDITIONAL — an all-idle window must still
        // report (cycles > 0, everything else 0) so silence is provably
        // "idle", never "unknown".
        let mut p = SealWriterProgress::default();
        let idle = CycleOutcome::default();
        assert_eq!(p.absorb(&idle), 0);
        assert_eq!(p.cycles, 1);
        assert_eq!(p.submitted, 0);
        assert_eq!(p.flushed_rows, 0);
        assert_eq!(p.flush_failures, 0, "idle cycle is not a flush failure");
    }

    #[test]
    fn test_progress_absorbs_real_failed_cycle_from_runner() {
        // End-to-end: a REAL runner cycle with a dead (disconnected) writer
        // must surface in the progress window as a flush failure + spill
        // rescues — the exact signature the 2026-07-06 exam session should
        // have reported instead of silence.
        let (spill, dlq) = temp_pair("progress-real");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
            .expect("ok");
        let outcome = runner.run_one_cycle(chrono::Utc::now().timestamp());
        let mut p = SealWriterProgress::default();
        let dropped = p.absorb(&outcome);
        assert_eq!(dropped, 0, "spill dir is healthy — nothing truly dropped");
        assert_eq!(p.submitted, 2);
        assert_eq!(p.flush_failures, 1, "dead writer = one failed flush cycle");
        assert_eq!(p.rescued_to_spill, 2, "both seals rescued to spill");
        assert_eq!(p.flushed_rows, 0, "no rows may be claimed written");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_ratchet_loop_wires_observability_not_silence() {
        // Source-scan ratchet (2026-07-06 exam-fix): the loop body MUST keep
        // (a) the per-cycle counter fan-out, (b) the AGGREGATOR-DROP-01
        // error emit for truly-dropped seals, and (c) the unconditional
        // progress report. Deleting any of these restores the silent
        // candle-persist bug class.
        let src = include_str!("seal_writer_loop.rs");
        assert!(
            src.contains("record_cycle_observability(&outcome, dropped)"),
            "ticker arm must fan the CycleOutcome into counters — not drop it"
        );
        assert!(
            src.contains("ErrorCode::AggregatorDrop01.code_str()"),
            "truly-dropped seals must fire error!(code = AGGREGATOR-DROP-01)"
        );
        assert!(
            src.contains("tv_seal_writer_drain_total"),
            "the promised tv_seal_writer_drain_total counter family must be emitted"
        );
        assert!(
            src.contains("emit_progress_report(&progress.take(), runner.ring_len())"),
            "the unconditional 60s progress report must remain wired"
        );
    }

    #[tokio::test]
    async fn test_run_seal_writer_loop_exits_on_cancellation() {
        let (spill, dlq) = temp_pair("cancel-exits");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(10),
            cancel_rx,
        ));
        // Let it tick once
        tokio::time::sleep(Duration::from_millis(15)).await;
        // Signal cancellation
        cancel_tx.send(true).expect("send");
        // Loop must exit promptly
        let result = tokio::time::timeout(Duration::from_millis(500), task).await;
        let join_result = result.expect("loop must exit within 500ms after cancel");
        let _final_outcome = join_result.expect("task must not panic");
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_run_seal_writer_loop_processes_seals_on_tick() {
        let (spill, dlq) = temp_pair("processes-seals");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        // Push 3 seals BEFORE starting the loop so the very first
        // tick has work to do.
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
            .expect("ok");
        tx.try_send(mk_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0))
            .expect("ok");

        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(10),
            cancel_rx,
        ));
        // Let two ticks fire
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel_tx.send(true).expect("send");
        let outcome = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        // Loop drained the seals over the ticks. By the time we
        // cancel, the ring is empty AND the rescue counters are
        // populated (writer is disconnected → all 3 went to spill).
        // Final cycle is a noop (everything already drained).
        let _ = outcome;

        // Verify spill file has 3 records (proves the loop drained
        // through the writer + cascade-rescued correctly).
        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let now = chrono::Utc::now().timestamp();
        let drained = spill_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 3, "all 3 seals must reach spill");
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_cancel_triggers_final_drain() {
        // Push seals AFTER cancel signal but BEFORE the loop sees it.
        // The final-drain on cancel MUST process them.
        let (spill, dlq) = temp_pair("final-drain");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            // Long interval so the loop is parked between ticks.
            Duration::from_secs(10),
            cancel_rx,
        ));
        // Give the spawned task a moment to enter `select!`
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Push seals THEN cancel
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
            .expect("ok");
        cancel_tx.send(true).expect("send");
        let outcome = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        // Final drain should have processed both seals
        assert_eq!(
            outcome.submitted_from_mpsc, 2,
            "final-drain MUST submit both pending seals to pipeline"
        );
        assert_eq!(
            outcome.drain.ring_seals_popped, 2,
            "final-drain MUST pop both from the ring"
        );
        assert_eq!(
            outcome.drain.rescued_to_spill, 2,
            "final-drain MUST rescue both via spill (writer disconnected)"
        );
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_loop_handles_no_seals_gracefully_across_many_ticks() {
        let (spill, dlq) = temp_pair("idle-ticks");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(5),
            cancel_rx,
        ));
        // Let many ticks fire with no work — loop must not error
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_tx.send(true).expect("send");
        let final_outcome = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        // Every cycle was idle; the final cycle is also idle.
        assert!(final_outcome.is_idle());
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_loop_does_not_panic_on_sender_drop() {
        // If all producer-side senders drop, `try_recv` returns
        // Disconnected. The loop must continue ticking without
        // panicking; it's the caller's job to cancel.
        let (spill, dlq) = temp_pair("sender-drop");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(5),
            cancel_rx,
        ));
        // Drop the producer-side sender and let several ticks fire
        drop(tx);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Loop must still be alive
        assert!(!task.is_finished(), "loop must survive sender drop");
        cancel_tx.send(true).expect("send");
        let _ = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        cleanup(&spill, &dlq);
    }
}
