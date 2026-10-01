//! Wave 6 Sub-PR #1 item 1.2f.5 — sealed-candle writer-task tokio loop.
//!
//! Long-running async wrapper that calls
//! [`SealWriterRunner::run_one_cycle`] on a fixed interval, with
//! cooperative cancellation via a [`tokio::sync::watch`] channel.
//! The future boot wiring (item 1.4) spawns this task at app
//! startup; the shutdown path flips the cancellation flag, the loop
//! drains the ring to EMPTY (bounded by `FINAL_DRAIN_BUDGET_SECS`), then
//! exits gracefully. Until 2026-08-25 it ran exactly ONE bounded cycle, so
//! anything past `max_drain_per_cycle` died in RAM while the log said the
//! drain had completed -- see `final_drain`.
//!
//! ## What this slice ships
//!
//! - [`SEAL_DRAIN_INTERVAL_MS`] — locked drain interval constant
//!   (100 ms — bounds the worst-case in-flight latency from
//!   `aggregator.consume_tick → ILP commit` to ~100 ms +
//!   one `flush()` round-trip).
//! - [`run_seal_writer_loop`] — the async fn; runs forever until
//!   cancelled, then drains the ring to empty so no buffered seal is lost.
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

use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::watch;
use tracing::{debug, error, info};

use tickvault_common::error_code::ErrorCode;

use crate::seal_writer_runner::{CycleOutcome, SealWriterRunner};
use crate::seal_writer_task::{BootDrainOutcome, ReplayOutcome};

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

/// Counter for the mid-session spill replay (audit PR15). One series per
/// `kind`: `files_staged`, `reingested`, `skipped`, `files_archived`,
/// `flush_failed`, and since audit PR41b `files_parked` (a file that stopped
/// making progress, set aside so the next could go ahead) and `files_rewound`
/// (a file read again because QuestDB turned suspect before confirming it).
pub const SEAL_REPLAY_COUNTER: &str = "tv_seal_replay_total";

/// The seven `kind` labels of [`SEAL_REPLAY_COUNTER`].
const SEAL_REPLAY_KINDS: [&str; 7] = [
    "files_staged",
    "reingested",
    "skipped",
    "files_archived",
    "flush_failed",
    "files_parked",
    "files_rewound",
];

/// Put every replay series on the wire at zero when the loop starts, so "the
/// replay never ran" and "the replay found nothing" read differently from "the
/// replay is not installed" (the first-sample baseline the EMF agent drops).
fn register_replay_baseline() {
    for kind in SEAL_REPLAY_KINDS {
        metrics::counter!(SEAL_REPLAY_COUNTER, "kind" => kind).increment(0);
    }
}

/// Emit one replay step's counters. Silent when the step did nothing.
fn record_replay_observability(replay: &ReplayOutcome) {
    if replay.is_idle() {
        return;
    }
    let values = [
        replay.files_staged,
        replay.seals_reingested,
        replay.records_skipped,
        replay.files_archived,
        usize::from(replay.flush_failed),
        replay.files_parked,
        replay.files_rewound,
    ];
    for (kind, value) in SEAL_REPLAY_KINDS.into_iter().zip(values) {
        if value > 0 {
            metrics::counter!(SEAL_REPLAY_COUNTER, "kind" => kind).increment(value as u64);
        }
    }
    let _ = report_unrecovered_seals(UnrecoveredStage::Replay, replay.records_skipped);
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
        // A SECOND, UNLABELLED counter for the same event -- added 2026-08-28,
        // and it is not duplication.
        //
        // `tv-<env>-seal-writer-dropped` is meant to be the redundant
        // metric-side pager for this exact loss, so a sealed candle still pages
        // when the log-filter leg is degraded (the 2026-07-06 class). Its
        // `metric_name` was `tv_seal_writer_drain_dropped_total` -- a name with
        // ZERO producers anywhere in the workspace -- while its own description
        // said it read `tv_seal_writer_drain_total kind=dropped`. The field and
        // the description disagreed, and the field is the half AWS reads, so the
        // redundancy it promised never existed and the alarm has been
        // permanently green since it shipped.
        //
        // Pointing the alarm at the labelled counter instead would be WORSE than
        // leaving it dead: the EMF processor folds label values into one summed
        // series per host, so `tv_seal_writer_drain_total` in CloudWatch is
        // submitted + flushed_rows + rescued_spill + rescued_dlq + dropped --
        // dominated by successes, and a `>= 1` threshold on it would fire on
        // every healthy cycle. A drop needs its own name to be visible at all.
        metrics::counter!("tv_seal_writer_drain_dropped_total").increment(dropped_this_cycle);
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
///
/// ## 2026-08-11 honesty fix — "rescued" is not "absorbed"
///
/// Before this change a window that rescued seals to spill/DLQ reported them
/// at `info!` under the word "rescued", and the only `error!` in the loop
/// fired for `dropped`. That read as SUCCESS — three tiers of absorption
/// working as designed — when in fact those candles were sitting in a file
/// that nothing ever read back (see
/// [`crate::seal_writer_task::drain_recovered_seals`]). Now that a boot-time
/// recovery drain exists, spill/DLQ is genuinely recoverable — but it is
/// still NOT persisted yet, and a window that produced any is escalated to
/// `error!` so the operator sees a real signal at the time it happens rather
/// than discovering it at the next restart.
fn emit_progress_report(snapshot: &SealWriterProgress, ring_len: usize) {
    let on_disk = snapshot
        .rescued_to_spill
        .saturating_add(snapshot.rescued_to_dlq);
    if on_disk > 0 {
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            cycles = snapshot.cycles,
            submitted = snapshot.submitted,
            rows_written = snapshot.flushed_rows,
            flush_failures = snapshot.flush_failures,
            rescued_to_spill = snapshot.rescued_to_spill,
            rescued_to_dlq = snapshot.rescued_to_dlq,
            dropped = snapshot.dropped,
            ring_len,
            "seal writer: {on_disk} sealed candles went to DISK, NOT QuestDB this window \
             — recoverable at next boot, but NOT persisted now"
        );
        return;
    }
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

/// Fans the boot-recovery outcome into the existing
/// `tv_seal_writer_drain_total` counter (NEW LABEL VALUES on the EXISTING
/// metric name — deliberately not a new metric, so the metrics-catalog and
/// dashboard guards keep matching on a name that already ships).
fn record_boot_drain_observability(outcome: &BootDrainOutcome) {
    if outcome.seals_recovered > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "boot_recovered")
            .increment(outcome.seals_recovered as u64);
    }
    if outcome.seals_reingested > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "boot_reingested")
            .increment(outcome.seals_reingested as u64);
    }
    if outcome.records_undecodable > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "boot_undecodable")
            .increment(outcome.records_undecodable as u64);
    }
    if outcome.seals_left_pending > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "boot_pending")
            .increment(outcome.seals_left_pending as u64);
    }
    if outcome.seals_append_failed > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "boot_append_failed")
            .increment(outcome.seals_append_failed as u64);
    }
    if outcome.seals_superseded > 0 {
        metrics::counter!("tv_seal_writer_drain_total", "kind" => "boot_superseded")
            .increment(outcome.seals_superseded as u64);
    }
    let _ = report_unrecovered_seals(
        UnrecoveredStage::BootDrain,
        outcome
            .records_undecodable
            .saturating_add(outcome.seals_append_failed),
    );
}

// ---------------------------------------------------------------------------
// Unwritten-seal count and crash marker (audit PR40a)
// ---------------------------------------------------------------------------
//
// A crash (abort, out of memory, a hard kill) loses every sealed candle the
// process still holds in memory: the writer channel, the ring, and the
// escalation queue in front of the spill. Up to 750,000 of them can be queued
// at once (three queues of `SEAL_BUFFER_CAPACITY` = 250,000 each), and until
// this change nothing counted them, because a dead process cannot log its own
// loss.
//
// Two things now bound and report it:
//
// * `tv_seal_unwritten` is set on every writer cycle (every 100 ms) to
//   `SealWriterRunner::unwritten_seals`. It is scraped with every other series
//   into the metrics log group, so the last value before a crash is readable
//   afterwards.
// * The same number is written to a small marker file next to the spill, at
//   most once per wall-clock second and only when it changed. A clean shutdown
//   rewrites it with `clean=1` after the final drain (whose own residue is
//   already reported with AGGREGATOR-DROP-01). The next boot reads it before
//   anything else: a marker that is not clean and holds seals means the
//   previous process ended abruptly while holding them, and the boot reports
//   the number with AGGREGATOR-DROP-01 and adds it to
//   `tv_seal_crash_unwritten_total`.
//
// Chosen over a durable per-window seal watermark (the alternative in the
// plan) because the re-fold that would consume a watermark is PR31b's warm-up,
// which does not exist yet; counting is what can ship now, and the watermark
// rides PR31b.
//
// Honest limits:
// * The marker is up to one second plus one cycle stale, so the reported
//   number is the count at the last sample, not at the instant of death; the
//   seals popped into the ILP buffer by the cycle in progress (at most
//   `max_drain_per_cycle`) are in neither sample.
// * The marker is renamed into place but not fsynced: it survives a process
//   crash (the page cache outlives the process), not a host crash or power
//   loss, like every other unflushed file on the box.
// * These seals are REPORTED, not recovered. Their ticks survive in the
//   capture log, but the boot's re-fold only rebuilds windows after the
//   applied watermark, so treat the reported seals as lost until PR31b.

/// Seals held in memory and not yet written anywhere durable, sampled on
/// every writer cycle (audit PR40a).
pub const SEAL_UNWRITTEN_GAUGE: &str = "tv_seal_unwritten";

/// Seals a previous process held unwritten when it ended without a clean
/// shutdown, read from its marker at boot (audit PR40a).
pub const SEAL_CRASH_UNWRITTEN_COUNTER: &str = "tv_seal_crash_unwritten_total";

/// Marker writes that failed. The gauge still works; only the next boot's
/// report is at risk.
pub const SEAL_UNWRITTEN_MARK_ERRORS_COUNTER: &str = "tv_seal_unwritten_mark_errors_total";

/// Marker file name, in the spill directory. Its `seal-` prefix and `.mark`
/// extension keep it out of every spill and DLQ file filter, which match
/// `seals_v4-*` / `seals-*` names ending `.bin` or `.ndjson`.
pub const SEAL_UNWRITTEN_MARK_FILE: &str = "seal-unwritten.mark";

/// Fewest wall-clock seconds between two marker writes.
pub const SEAL_UNWRITTEN_MARK_EVERY_SECS: i64 = 1;

/// First token of every marker line: names the format so a future change can
/// be refused rather than misread.
const SEAL_UNWRITTEN_MARK_HEADER: &str = "tv-seal-unwritten-v1";

/// What a marker file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnwrittenSealRecord {
    /// Seals held unwritten at the sample.
    pub unwritten: usize,
    /// `true` when written by a clean shutdown after its final drain.
    pub clean: bool,
    /// Wall clock (unix seconds) of the sample.
    pub sampled_at_unix_secs: i64,
}

impl UnwrittenSealRecord {
    /// The one-line text form written to the marker.
    #[must_use]
    pub fn to_line(&self) -> String {
        format!(
            "{SEAL_UNWRITTEN_MARK_HEADER} unwritten={} clean={} sampled_at={}\n",
            self.unwritten,
            u8::from(self.clean),
            self.sampled_at_unix_secs
        )
    }

    /// Parse a marker line. `None` for anything that is not exactly the
    /// current format, so a torn or foreign file is reported as unreadable
    /// rather than read as a number.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut tokens = text.split_whitespace();
        if tokens.next()? != SEAL_UNWRITTEN_MARK_HEADER {
            return None;
        }
        let unwritten = tokens.next()?.strip_prefix("unwritten=")?.parse().ok()?;
        let clean = match tokens.next()?.strip_prefix("clean=")? {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        let sampled_at_unix_secs = tokens.next()?.strip_prefix("sampled_at=")?.parse().ok()?;
        if tokens.next().is_some() {
            return None;
        }
        Some(Self {
            unwritten,
            clean,
            sampled_at_unix_secs,
        })
    }
}

/// What the previous process's marker said, as read at boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviousUnwritten {
    /// No marker: first boot on this volume, or the directory was cleared.
    Absent,
    /// A marker exists but is not the current format.
    Unreadable,
    /// The marker as written.
    Read(UnwrittenSealRecord),
}

/// Writes the marker, at most once per [`SEAL_UNWRITTEN_MARK_EVERY_SECS`] and
/// only when the count changed.
#[derive(Debug)]
pub struct UnwrittenSealMark {
    path: std::path::PathBuf,
    last_written: Option<usize>,
    last_write_unix_secs: i64,
    /// Latched on the first failed write and cleared by the next good one, so
    /// a full disk logs once per episode, not once per second.
    failing: bool,
}

impl UnwrittenSealMark {
    /// A marker at `dir/`[`SEAL_UNWRITTEN_MARK_FILE`].
    #[must_use]
    pub fn in_dir(dir: &std::path::Path) -> Self {
        Self {
            path: dir.join(SEAL_UNWRITTEN_MARK_FILE),
            last_written: None,
            last_write_unix_secs: i64::MIN,
            failing: false,
        }
    }

    /// Where the marker lives.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Read the marker the previous process left. Call once, before the first
    /// [`Self::sample`] overwrites it.
    #[must_use]
    pub fn read_previous(&self) -> PreviousUnwritten {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => UnwrittenSealRecord::parse(&text)
                .map_or(PreviousUnwritten::Unreadable, PreviousUnwritten::Read),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => PreviousUnwritten::Absent,
            Err(_) => PreviousUnwritten::Unreadable,
        }
    }

    /// Record this cycle's count. Writes only when the count changed and a
    /// full second has passed since the last write (a clock that stepped
    /// backwards counts as due). Returns `true` when it wrote.
    ///
    /// # Complexity
    /// O(1). At most one small write and one rename per second.
    pub fn sample(&mut self, unwritten: usize, now_unix_secs: i64) -> bool {
        if self.last_written == Some(unwritten) {
            return false;
        }
        let due = now_unix_secs < self.last_write_unix_secs
            || now_unix_secs.saturating_sub(self.last_write_unix_secs)
                >= SEAL_UNWRITTEN_MARK_EVERY_SECS;
        if !due {
            return false;
        }
        self.write(UnwrittenSealRecord {
            unwritten,
            clean: false,
            sampled_at_unix_secs: now_unix_secs,
        })
    }

    /// Record a clean shutdown after the final drain. Always writes.
    pub fn mark_clean(&mut self, unwritten: usize, now_unix_secs: i64) -> bool {
        self.write(UnwrittenSealRecord {
            unwritten,
            clean: true,
            sampled_at_unix_secs: now_unix_secs,
        })
    }

    /// Write-then-rename, so a reader sees the old marker or the new one,
    /// never half of either.
    fn write(&mut self, record: UnwrittenSealRecord) -> bool {
        let tmp = self.path.with_extension("mark.tmp");
        let written = std::fs::write(&tmp, record.to_line().as_bytes())
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        match written {
            Ok(()) => {
                self.last_written = Some(record.unwritten);
                self.last_write_unix_secs = record.sampled_at_unix_secs;
                self.failing = false;
                true
            }
            Err(err) => {
                metrics::counter!(SEAL_UNWRITTEN_MARK_ERRORS_COUNTER).increment(1);
                if !self.failing {
                    self.failing = true;
                    tracing::warn!(
                        path = %self.path.display(),
                        error = %err,
                        "seal writer: could not write the unwritten-seal marker — the live \
                         count is still on tv_seal_unwritten, but a crash now would not be \
                         reported at the next boot"
                    );
                }
                false
            }
        }
    }
}

/// Report what the previous process's marker says. Called once at boot.
///
/// Returns the seals reported as held unwritten by a process that did not
/// shut down cleanly (0 otherwise).
pub fn report_previous_unwritten(previous: PreviousUnwritten, now_unix_secs: i64) -> usize {
    metrics::counter!(SEAL_CRASH_UNWRITTEN_COUNTER).increment(0);
    match previous {
        PreviousUnwritten::Absent => 0,
        PreviousUnwritten::Unreadable => {
            tracing::warn!(
                "seal writer: the unwritten-seal marker from the previous process could not \
                 be read — whether it held unwritten seals when it ended is unknown"
            );
            0
        }
        PreviousUnwritten::Read(record) if record.clean => 0,
        PreviousUnwritten::Read(record) if record.unwritten == 0 => {
            info!(
                sampled_at_unix_secs = record.sampled_at_unix_secs,
                "seal writer: the previous process ended without a clean shutdown, holding \
                 no unwritten seals at its last sample"
            );
            0
        }
        PreviousUnwritten::Read(record) => {
            metrics::counter!(SEAL_CRASH_UNWRITTEN_COUNTER).increment(record.unwritten as u64);
            error!(
                code = ErrorCode::AggregatorDrop01.code_str(),
                source = "crash_unwritten",
                seals_unwritten = record.unwritten,
                sampled_at_unix_secs = record.sampled_at_unix_secs,
                sample_age_secs = now_unix_secs.saturating_sub(record.sampled_at_unix_secs),
                "seal writer: the previous process ended WITHOUT a clean shutdown while \
                 holding {} sealed candles in memory at its last sample (up to one second \
                 before it died). They were never written and this boot does not rebuild \
                 them: treat those candles as lost. Their ticks are still in the capture log.",
                record.unwritten
            );
            record.unwritten
        }
    }
}

// ---------------------------------------------------------------------------
// Unrecovered seals page (audit PR40b)
// ---------------------------------------------------------------------------
//
// Two recovery paths give up on a seal for good and, until PR40b, said so only
// at `warn!` or under AGGREGATOR-SEAL-01, which no alarm filters on:
//
// * the boot drain, for a record it cannot decode (most often a file written
//   by an older build) and for a seal the writer refuses; the file moves to
//   `archive/` and nothing retries it;
// * the mid-session replay, for the same two cases plus a seal the database
//   refused in every attempt; the cursor moves past it and nothing retries it.
//
// Each is a sealed candle that will not reach QuestDB without an operator. The
// counters already existed (`tv_seal_writer_drain_total{kind="boot_undecodable"
// | "boot_append_failed"}`, `tv_seal_replay_total{kind="skipped"}`); what was
// missing was the page. It rides the EXISTING AGGREGATOR-DROP-01 errcode alarm
// with `source = "seal_unrecovered"`, the same way PR40a's `crash_unwritten`
// does: one ERROR line per boot drain or replay step that skipped something,
// never per seal, and no new alarm, filter or metric.

/// `source` field value on the AGGREGATOR-DROP-01 line for seals a recovery
/// path skipped for good.
pub const SEAL_UNRECOVERED_SOURCE: &str = "seal_unrecovered";

/// Which recovery path gave up on the seals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnrecoveredStage {
    /// The boot drain of `replaying/`.
    BootDrain,
    /// The mid-session spill replay.
    Replay,
}

impl UnrecoveredStage {
    /// The `stage` field value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BootDrain => "boot_drain",
            Self::Replay => "replay",
        }
    }
}

/// Page the seals a recovery path skipped for good. Silent at zero.
///
/// Returns the number reported. O(1), cold path.
pub fn report_unrecovered_seals(stage: UnrecoveredStage, unrecovered: usize) -> usize {
    if unrecovered == 0 {
        return 0;
    }
    error!(
        code = ErrorCode::AggregatorDrop01.code_str(),
        source = SEAL_UNRECOVERED_SOURCE,
        stage = stage.as_str(),
        seals_unrecovered = unrecovered,
        "seal recovery gave up on {} sealed candle(s) for good: they could not be decoded, \
         the writer refused them, or the database refused them in every attempt. They are \
         NOT in QuestDB and nothing retries them. Their bytes stay in the spill archive/ \
         folder until the retention sweep removes them; their ticks are still in the \
         capture log.",
        unrecovered
    );
    unrecovered
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

/// Run one drain cycle, keeping its SYNCHRONOUS ILP-over-HTTP flush off the
/// async worker (2026-07-06 hostile-review fix, HIGH).
///
/// `SealWriterRunner::run_one_cycle` ultimately calls the writer's blocking
/// HTTP `flush()` — bounded per attempt by the conf-pinned
/// `request_timeout=5000` (+ the questdb-rs min-throughput extension, ≤ ~2.5s
/// at the 1024-seal max batch) with the library's internal retry DISABLED
/// (`retry_timeout=0`), so a failed cycle against a hung QuestDB blocks at
/// most ~4 × ~7.5s ≈ 30s (see `shadow_candle_writer::build_ilp_conf_string`).
/// Even that bounded block must not pin one of the two tokio workers on the
/// r8g.large prod box, so on a multi-thread runtime the cycle runs under
/// `tokio::task::block_in_place` — the worker's other tasks migrate while the
/// flush blocks. Current-thread runtimes (the `#[tokio::test]` harness) call
/// directly, because `block_in_place` panics there.
/// Wall-clock ceiling on the shutdown drain.
///
/// The app's shutdown path budgets ~75 s for the whole sequence, so this must
/// leave room for everything after it. A drain that has not finished by then
/// is reported as loss rather than waited on indefinitely — a shutdown that
/// hangs is worse than one that says exactly what it could not persist.
pub const FINAL_DRAIN_BUDGET_SECS: u64 = 45;

/// Drains the ring to empty on cancellation, bounded by
/// [`FINAL_DRAIN_BUDGET_SECS`], and reports any residue LOUDLY.
///
/// # Why this is a loop
///
/// Until 2026-08-25 the cancel arm ran exactly ONE cycle and returned.
/// `run_one_cycle` is bounded by the runner's `max_drain_per_cycle`, so
/// everything beyond that bound stayed in the ring and died with the process
/// — and the shutdown log said `"final drain complete"` with
/// `rescued_dropped = 0`, because that counter measures triple-tier failure,
/// never ring residue. A silent loss reported as a success.
///
/// It fires on the most ordinary event there is: the 17:30 stop, every
/// weekday, immediately after the close force-seal has just produced one seal
/// per instrument per timeframe — the largest burst of the day. At today's
/// universe the burst fits under the per-cycle bound with room to spare, so
/// nothing is being lost right now; at the authorised universe it would not,
/// and the first symptom would have been a success message.
///
/// # Termination
///
/// Three independent exits, so this can never spin: the ring empties, the
/// budget expires, or a cycle makes no progress at all (which is what a
/// permanently-stuck tier looks like — continuing would burn the budget to
/// achieve nothing).
///
/// # Complexity
/// O(seals in the ring), bounded by the budget. No allocation.
fn final_drain(runner: &mut SealWriterRunner, progress: &mut SealWriterProgress) -> CycleOutcome {
    let deadline = Instant::now() + Duration::from_secs(FINAL_DRAIN_BUDGET_SECS);
    let mut total = CycleOutcome::default();
    let mut cycles: u64 = 0;

    loop {
        let outcome = run_cycle(runner, utc_now_secs());
        let dropped = progress.absorb(&outcome);
        record_cycle_observability(&outcome, dropped);
        total.accumulate(&outcome);
        cycles = cycles.saturating_add(1);

        if runner.ring_len() == 0 {
            break;
        }
        if outcome.is_idle() {
            // The ring is non-empty and yet nothing moved — every tier is
            // refusing. More cycles cannot change that.
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    emit_progress_report(&progress.take(), runner.ring_len());

    let residue = runner.ring_len();
    if residue > 0 {
        // NOT an `info!` "complete" line. These seals are in RAM, the process
        // is exiting, and no tier accepted them — they are gone.
        error!(
            code = ErrorCode::AggregatorDrop01.code_str(),
            ring_len = residue,
            cycles,
            submitted_from_mpsc = total.submitted_from_mpsc,
            "seal writer final drain ENDED WITH SEALS STILL IN MEMORY — \
             {residue} sealed candles are lost at shutdown"
        );
        metrics::counter!("tv_seal_final_drain_residue_total").increment(residue as u64);
    } else {
        info!(
            cycles,
            submitted_from_mpsc = total.submitted_from_mpsc,
            rescued_to_spill = total.drain.rescued_to_spill,
            rescued_to_dlq = total.drain.rescued_to_dlq,
            rescued_dropped = total.drain.rescued_dropped,
            "seal writer loop final drain complete — ring empty"
        );
    }

    total
}

/// [`run_cycle`] plus one mid-session replay step, for the loop's tick. The
/// shutdown drain keeps calling [`run_cycle`] alone.
///
/// Also samples [`SealWriterRunner::unwritten_seals`] after the cycle and
/// hands it to the crash marker, inside the same blocking section, because the
/// marker write is a file write too (audit PR40a). Returns the sample.
fn run_cycle_with_replay(
    runner: &mut SealWriterRunner,
    mark: &mut UnwrittenSealMark,
    now_unix_secs: i64,
) -> (CycleOutcome, ReplayOutcome, usize) {
    let mut work = || {
        let (outcome, replay) = runner.run_one_cycle_with_replay(now_unix_secs);
        let unwritten = runner.unwritten_seals();
        mark.sample(unwritten, now_unix_secs);
        (outcome, replay, unwritten)
    };
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

/// Rewrite the crash marker as a clean shutdown once the final drain is done.
/// Whatever the drain could not place is already reported by the drain itself
/// (and by the escalation thread's shutdown path), so the next boot must not
/// report it a second time as a crash.
fn mark_clean_after_final_drain(runner: &SealWriterRunner, mark: &mut UnwrittenSealMark) {
    let unwritten = runner.unwritten_seals();
    let now = utc_now_secs();
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(|| mark.mark_clean(unwritten, now));
    } else {
        mark.mark_clean(unwritten, now);
    }
}

fn run_cycle(runner: &mut SealWriterRunner, now_unix_secs: i64) -> CycleOutcome {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(|| runner.run_one_cycle(now_unix_secs))
    } else {
        runner.run_one_cycle(now_unix_secs)
    }
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

    // 2026-08-11 — BOOT RECOVERY DRAIN, before the first tick.
    //
    // Seals rescued to spill/DLQ during a QuestDB outage used to sit in files
    // that NO production code ever read back (verified: every `read_all` call
    // site lived inside a test-only module). This is that missing caller. It
    // runs here rather than in `main.rs` because the loop already owns the
    // runner and is itself the boot-time entry point for the writer task.
    //
    // NOTE for future editors: do NOT write the cfg-test attribute literally
    // in this region. The `test_ratchet_loop_wires_observability_not_silence`
    // ratchet splits this file at the FIRST occurrence of that token to scan
    // production code only; an earlier literal silently shrinks the scanned
    // region and would let the observability wiring be deleted unnoticed.
    // Audit PR40a: read what the previous process left BEFORE anything here
    // can overwrite it, report it, then claim the marker for this process at
    // once, so a crash during the boot drain is not reported twice.
    let mut mark = UnwrittenSealMark::in_dir(runner.spill_dir());
    let boot_now = utc_now_secs();
    let _ = report_previous_unwritten(mark.read_previous(), boot_now);
    let _ = mark.sample(runner.unwritten_seals(), boot_now);
    let unwritten_gauge = metrics::gauge!(SEAL_UNWRITTEN_GAUGE);

    let boot = runner.boot_drain();
    record_boot_drain_observability(&boot);
    if !boot.is_clean() {
        info!(
            files_staged = boot.files_staged,
            seals_recovered = boot.seals_recovered,
            seals_reingested = boot.seals_reingested,
            files_archived = boot.files_archived,
            files_left_pending = boot.files_left_pending,
            seals_left_pending = boot.seals_left_pending,
            records_undecodable = boot.records_undecodable,
            seals_append_failed = boot.seals_append_failed,
            seals_superseded = boot.seals_superseded,
            "seal writer boot recovery drain finished"
        );
    }

    register_replay_baseline();

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
            changed = cancel_rx.changed() => {
                match changed {
                    Ok(()) => {
                        if *cancel_rx.borrow() {
                            info!("seal writer loop cancelled — performing final drain");
                            let final_outcome = final_drain(&mut runner, &mut progress);
                            mark_clean_after_final_drain(&runner, &mut mark);
                            return final_outcome;
                        }
                        // A `false → false` change never happens; keep draining.
                    }
                    Err(_) => {
                        // CORRECTED 2026-08-28 — the comment this replaces named
                        // the exact hazard and got its consequence backwards. It
                        // read "and on sender drop; ignore", but ignoring a
                        // dropped sender is not a no-op: `changed()` on a closed
                        // watch channel resolves `Err` IMMEDIATELY and does so
                        // forever, and this `select!` is `biased`, so the cancel
                        // branch is polled first and is permanently ready.
                        //
                        // The loop therefore spins at 100% of a core, and the
                        // ticker arm — which is the ONLY thing that drains the
                        // mpsc, flushes to QuestDB, and emits the 60s progress
                        // report — never runs again. That last part is the worst
                        // of it: this module's own header says the report exists
                        // so "silence can never again mean unknown", and this
                        // path silences it. On a 4-vCPU box the pinned core is
                        // also stolen from the frame drain, and a stalled drain
                        // is how Dhan's skip-forward loses ticks upstream.
                        //
                        // A closed channel means no shutdown signal can ever
                        // arrive, so there is nothing left to wait for. Drain
                        // what we hold and exit loudly: strictly better on data
                        // (the final drain persists the buffer instead of
                        // stranding it), on CPU, and on visibility.
                        //
                        // Not reachable today — `SEAL_WRITER_CANCEL` holds the
                        // sender for the process lifetime — but "guarded by a
                        // comment in another crate" is not a guarantee, and the
                        // main.rs gate landing with this closes the one entry
                        // that could open it.
                        error!(
                            code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                            "seal writer loop: every cancel sender dropped — no shutdown \
                             signal can arrive and re-polling would spin forever. Draining \
                             what is buffered and exiting. Sealed candles are NOT lost: \
                             further seals fall through to the overflow tier (spill/DLQ) \
                             and replay at the next boot, but candles_* will stop gaining \
                             rows this session. Restart the process."
                        );
                        let final_outcome = final_drain(&mut runner, &mut progress);
                        mark_clean_after_final_drain(&runner, &mut mark);
                        return final_outcome;
                    }
                }
            }
            _ = ticker.tick() => {
                let now = utc_now_secs();
                let (outcome, replay, unwritten) =
                    run_cycle_with_replay(&mut runner, &mut mark, now);
                unwritten_gauge.set(unwritten as f64);
                let dropped = progress.absorb(&outcome);
                record_cycle_observability(&outcome, dropped);
                record_replay_observability(&replay);
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
        state.net_volume_signed = -4_242;
        state.net_volume_classified = true;
        state.total_buy_qty = 89_600;
        state.total_sell_qty = 4_800;
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
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0))
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
        //
        // 2026-07-06 hostile-review fix (HIGH — vacuous ratchet): the scan is
        // restricted to the PRODUCTION region of this file (everything before
        // the `#[cfg(test)]` module marker). The previous version scanned the
        // whole file — including this test's own assertion string literals —
        // so deleting every piece of production observability left the test
        // green (the false-OK class audit Rule 11 bans). Splitting at the
        // FIRST `#[cfg(test)]` occurrence (the real module attribute, which
        // precedes any test-body literal) makes each needle provable against
        // production code only.
        let full = include_str!("seal_writer_loop.rs");
        let (prod, _tests) = full
            .split_once("#[cfg(test)]")
            .unwrap_or_else(|| panic!("seal_writer_loop.rs must contain a test module marker"));
        assert!(
            prod.contains("record_cycle_observability(&outcome, dropped)"),
            "ticker arm must fan the CycleOutcome into counters — not drop it"
        );
        assert!(
            prod.contains("ErrorCode::AggregatorDrop01.code_str()"),
            "truly-dropped seals must fire error!(code = AGGREGATOR-DROP-01)"
        );
        assert!(
            prod.contains("tv_seal_writer_drain_total"),
            "the promised tv_seal_writer_drain_total counter family must be emitted"
        );
        assert!(
            prod.contains("emit_progress_report(&progress.take(), runner.ring_len())"),
            "the unconditional 60s progress report must remain wired"
        );
        assert!(
            prod.contains("block_in_place"),
            "the drain cycle must stay off-worker on the multi-thread runtime \
             (sync HTTP flush must never pin a tokio worker's other tasks)"
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
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0))
            .expect("ok");
        tx.try_send(mk_seal(51, 0, TfIndex::M1, 1_716_024_900, 300.0))
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
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0))
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

    /// A dropped cancel sender must END the loop, not spin it forever.
    ///
    /// `watch::Receiver::changed()` on a closed channel resolves `Err`
    /// immediately and keeps doing so, and this loop's `select!` is `biased`
    /// — so the pre-fix `_ = cancel_rx.changed() => { ... ignore }` made the
    /// cancel arm permanently ready and starved the ticker arm outright: no
    /// drain, no flush, and no 60s progress report, on a pinned core, with no
    /// log line to say any of it was happening.
    ///
    /// `multi_thread` is deliberate. Under the pre-fix code this test must
    /// FAIL on the timeout rather than hang the suite, and a busy loop with no
    /// yield point would starve a current-thread runtime's timer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_cancel_sender_ends_the_loop_instead_of_spinning() {
        let (spill, dlq) = temp_pair("cancel-sender-dropped");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        // A seal is buffered BEFORE the sender dies, so the assertion covers
        // both halves of the contract: the loop must exit, and it must take
        // the buffer with it rather than stranding it.
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("submit before the sender dies");
        drop(cancel_tx);

        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            // Long interval: if the loop only exits because a tick fired, the
            // test would be proving the wrong thing.
            Duration::from_secs(30),
            cancel_rx,
        ));

        let outcome = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the loop must RETURN on a closed cancel channel — a timeout here is the spin")
            .expect("no panic");

        assert_eq!(
            outcome.submitted_from_mpsc, 1,
            "the exit must carry a final drain, or the buffered seal is stranded"
        );
        assert_eq!(
            outcome.drain.ring_seals_popped, 1,
            "final drain must pop the buffered seal from the ring"
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

    /// `record_boot_drain_observability` had no direct test — it reached
    /// coverage only incidentally, through whichever boot-drain path happened
    /// to run, and none of its four branches was exercised deliberately.
    ///
    /// Each branch is guarded by `> 0`, and the guards are the point: a boot
    /// that recovered nothing must emit NOTHING rather than four zero-valued
    /// counter increments. A zero increment is not harmless here — the
    /// `boot_pending` label is the honest "seals still on disk and not in
    /// QuestDB" signal, and a series that appears on every clean boot at zero
    /// trains the reader to ignore it, which is precisely when it next carries
    /// a real number.
    ///
    /// The assertion is on reachability, not on emitted values: no metrics
    /// recorder is installed in a unit test, so `metrics::counter!` is a no-op
    /// by construction and reading it back would assert on the test harness
    /// rather than on this function. What is genuinely verified is that every
    /// branch is entered and none panics — including the all-zero case, which
    /// must enter none of them.
    #[test]
    fn boot_drain_observability_covers_every_branch_and_emits_nothing_when_idle() {
        // All-zero: the clean-boot shape. Every guard must refuse.
        record_boot_drain_observability(&BootDrainOutcome::default());

        // Each field alone, so a branch cannot pass by riding another's guard.
        for outcome in [
            BootDrainOutcome {
                seals_recovered: 3,
                ..Default::default()
            },
            BootDrainOutcome {
                seals_reingested: 2,
                ..Default::default()
            },
            BootDrainOutcome {
                records_undecodable: 1,
                ..Default::default()
            },
            BootDrainOutcome {
                seals_left_pending: 7,
                ..Default::default()
            },
            BootDrainOutcome {
                seals_superseded: 5,
                ..Default::default()
            },
        ] {
            record_boot_drain_observability(&outcome);
        }

        // All four at once — the worst real boot: something recovered, something
        // re-ingested, something undecodable, and something still on disk.
        record_boot_drain_observability(&BootDrainOutcome {
            files_staged: 4,
            seals_recovered: 900,
            seals_reingested: 850,
            files_archived: 3,
            files_left_pending: 1,
            seals_left_pending: 50,
            records_undecodable: 2,
            seals_append_failed: 3,
            seals_superseded: 4,
        });
    }

    #[tokio::test]
    async fn test_final_drain_empties_a_ring_deeper_than_one_cycles_bound() {
        // THE REGRESSION. Until 2026-08-25 the cancel arm ran exactly ONE
        // cycle, and `run_one_cycle` is bounded by `max_drain_per_cycle` — so
        // everything past that bound stayed in the ring and died with the
        // process, while the shutdown log said "final drain complete" with
        // `rescued_dropped = 0` (a counter that measures a different kind of
        // loss entirely).
        //
        // Fixture: a ring holding 12 seals against a per-cycle bound of 3, so
        // one cycle provably cannot finish and four are required.
        let (spill, dlq) = temp_pair("final-drain-deep");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 64, 64, 3);
        assert_eq!(runner.max_drain_per_cycle(), 3, "fixture bound");
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_secs(10),
            cancel_rx,
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;

        for i in 0..12u32 {
            tx.try_send(mk_seal(
                13,
                0,
                TfIndex::M1,
                1_716_023_700 + i * 60,
                100.0 + f64::from(i),
            ))
            .expect("queue seal");
        }
        cancel_tx.send(true).expect("send");

        let outcome = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("final drain must not hang")
            .expect("task panicked");

        assert_eq!(
            outcome.submitted_from_mpsc, 12,
            "every queued seal must reach the pipeline, not just one cycle's worth"
        );
        assert_eq!(
            outcome.drain.ring_seals_popped, 12,
            "the ring must be drained to EMPTY, across as many cycles as that takes"
        );
        assert_eq!(
            outcome.drain.rescued_to_spill, 12,
            "with the writer disconnected all twelve land on disk — recoverable, not lost"
        );
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_final_drain_budget_is_under_the_shutdown_allowance() {
        // The app budgets ~75 s for the whole shutdown sequence. A drain
        // allowed to consume all of it would turn a bounded loss into a hung
        // stop, which is worse.
        assert!(
            FINAL_DRAIN_BUDGET_SECS < 75,
            "the drain budget must leave room for the rest of shutdown"
        );
        assert!(
            FINAL_DRAIN_BUDGET_SECS >= 10,
            "and must be long enough to drain a real close-seal burst"
        );
    }

    // --- Audit PR40a: unwritten-seal count and crash marker ---------------

    fn temp_dir_for(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tickvault-seal-mark-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mark dir");
        dir
    }

    #[test]
    fn test_unwritten_seal_record_to_line_round_trips_and_parse_refuses_anything_else() {
        let record = UnwrittenSealRecord {
            unwritten: 250_000,
            clean: false,
            sampled_at_unix_secs: 1_790_000_000,
        };
        assert_eq!(UnwrittenSealRecord::parse(&record.to_line()), Some(record));
        let clean = UnwrittenSealRecord {
            clean: true,
            ..record
        };
        assert_eq!(UnwrittenSealRecord::parse(&clean.to_line()), Some(clean));
        for bad in [
            "",
            "tv-seal-unwritten-v2 unwritten=1 clean=0 sampled_at=1",
            "tv-seal-unwritten-v1 unwritten=-1 clean=0 sampled_at=1",
            "tv-seal-unwritten-v1 unwritten=1 clean=2 sampled_at=1",
            "tv-seal-unwritten-v1 unwritten=1 clean=0",
            "tv-seal-unwritten-v1 unwritten=1 clean=0 sampled_at=1 extra=1",
            "tv-seal-unwritten-v1 clean=0 unwritten=1 sampled_at=1",
            "tv-seal-unwritten-v1 unwritten=1 cle",
        ] {
            assert_eq!(UnwrittenSealRecord::parse(bad), None, "must refuse {bad:?}");
        }
    }

    #[test]
    fn unwritten_mark_file_is_outside_every_spill_and_dlq_filter() {
        // The spill and DLQ filters match `seals_v4-*` / `seals-*` names
        // ending `.bin` or `.ndjson`; the retention sweep deletes only `.bin`.
        assert!(!SEAL_UNWRITTEN_MARK_FILE.starts_with("seals"));
        assert!(!SEAL_UNWRITTEN_MARK_FILE.ends_with(".bin"));
        assert!(!SEAL_UNWRITTEN_MARK_FILE.ends_with(".ndjson"));
    }

    #[test]
    fn unwritten_mark_writes_only_on_change_and_at_most_once_a_second() {
        let dir = temp_dir_for("rate");
        let mut mark = UnwrittenSealMark::in_dir(&dir);
        assert_eq!(mark.read_previous(), PreviousUnwritten::Absent);
        assert!(mark.sample(10, 1_000), "first sample always writes");
        assert!(!mark.sample(10, 1_005), "unchanged count never rewrites");
        assert!(!mark.sample(20, 1_000), "a change in the same second waits");
        assert!(mark.sample(20, 1_001), "a change a second later writes");
        assert!(
            mark.sample(30, 900),
            "a clock that stepped back is due at once"
        );
        assert_eq!(
            mark.read_previous(),
            PreviousUnwritten::Read(UnwrittenSealRecord {
                unwritten: 30,
                clean: false,
                sampled_at_unix_secs: 900,
            })
        );
        assert!(mark.mark_clean(30, 900), "a clean mark always writes");
        match mark.read_previous() {
            PreviousUnwritten::Read(r) => assert!(r.clean),
            other => panic!("expected a clean marker, got {other:?}"),
        }
        assert!(
            !dir.join("seal-unwritten.mark.tmp").exists(),
            "the temporary file is renamed into place, never left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_unwritten_mark_in_dir_read_previous_reports_an_unreadable_file_as_unreadable() {
        let dir = temp_dir_for("unreadable");
        std::fs::write(dir.join(SEAL_UNWRITTEN_MARK_FILE), b"half a li").expect("write");
        let mark = UnwrittenSealMark::in_dir(&dir);
        assert_eq!(mark.read_previous(), PreviousUnwritten::Unreadable);
        assert_eq!(
            report_previous_unwritten(PreviousUnwritten::Unreadable, 0),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritten_mark_write_failure_is_counted_not_fatal() {
        // A directory that does not exist: the write fails, nothing panics,
        // and the next good write is still attempted.
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tickvault-seal-mark-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut mark = UnwrittenSealMark::in_dir(&dir);
        assert!(!mark.sample(5, 1_000));
        assert!(
            !mark.sample(5, 1_001),
            "a failed write is retried, not remembered"
        );
        std::fs::create_dir_all(&dir).expect("dir");
        assert!(mark.sample(5, 1_002));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_report_unrecovered_seals_is_silent_at_zero_and_reports_the_count() {
        assert_eq!(report_unrecovered_seals(UnrecoveredStage::BootDrain, 0), 0);
        assert_eq!(report_unrecovered_seals(UnrecoveredStage::Replay, 0), 0);
        assert_eq!(report_unrecovered_seals(UnrecoveredStage::BootDrain, 4), 4);
        assert_eq!(report_unrecovered_seals(UnrecoveredStage::Replay, 1), 1);
        assert_eq!(UnrecoveredStage::BootDrain.as_str(), "boot_drain");
        assert_eq!(UnrecoveredStage::Replay.as_str(), "replay");
        assert_eq!(SEAL_UNRECOVERED_SOURCE, "seal_unrecovered");
    }

    #[test]
    fn test_report_unrecovered_seals_is_wired_into_both_recovery_paths() {
        // Audit PR40b: the page is only as good as its call sites. Both the
        // boot drain and the mid-session replay must feed it every seal they
        // give up on, and the line must carry the paging code at ERROR level.
        // A function body ends at a closing brace alone on its line. Spelled as
        // an escape so the banned-pattern scanner's brace counter, which reads
        // string contents, stays balanced.
        const FN_END: &str = "\n\u{7d}\n";
        let src = include_str!("seal_writer_loop.rs");
        let prod = src.split("#[cfg(test)]").next().expect("production half");
        let boot = prod
            .split("fn record_boot_drain_observability")
            .nth(1)
            .and_then(|s| s.split(FN_END).next())
            .expect("boot fan-out");
        assert!(
            boot.contains("UnrecoveredStage::BootDrain")
                && boot.contains("records_undecodable")
                && boot.contains("seals_append_failed"),
            "the boot drain must page undecodable AND refused seals"
        );
        let replay = prod
            .split("fn record_replay_observability")
            .nth(1)
            .and_then(|s| s.split(FN_END).next())
            .expect("replay fan-out");
        assert!(
            replay.contains("UnrecoveredStage::Replay") && replay.contains("records_skipped"),
            "the replay must page every skipped seal"
        );
        let report = prod
            .split("pub fn report_unrecovered_seals")
            .nth(1)
            .and_then(|s| s.split(FN_END).next())
            .expect("report fn");
        assert!(
            report.contains(concat!("error", "!("))
                && report.contains("ErrorCode::AggregatorDrop01")
                && report.contains("SEAL_UNRECOVERED_SOURCE"),
            "the page must be an ERROR line with the paging code and the source tag"
        );
        // `record_replay_observability` returns early on an idle step, so a
        // step that skipped a seal must never read as idle.
        assert!(
            include_str!("seal_writer_task.rs").contains("&& self.records_skipped == 0"),
            "ReplayOutcome::is_idle must treat a skipped seal as activity"
        );
    }

    #[test]
    fn test_report_previous_unwritten_only_for_an_unclean_marker_holding_seals() {
        let unclean = |n| {
            PreviousUnwritten::Read(UnwrittenSealRecord {
                unwritten: n,
                clean: false,
                sampled_at_unix_secs: 100,
            })
        };
        assert_eq!(report_previous_unwritten(PreviousUnwritten::Absent, 200), 0);
        assert_eq!(report_previous_unwritten(unclean(0), 200), 0);
        assert_eq!(report_previous_unwritten(unclean(7), 200), 7);
        assert_eq!(
            report_previous_unwritten(
                PreviousUnwritten::Read(UnwrittenSealRecord {
                    unwritten: 7,
                    clean: true,
                    sampled_at_unix_secs: 100,
                }),
                200
            ),
            0,
            "a clean shutdown's residue is reported by that shutdown, not again at boot"
        );
    }

    #[test]
    fn unwritten_seals_counts_channel_ring_and_escalation_queue() {
        let (spill, dlq) = temp_pair("unwritten-count");
        let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 4);
        assert_eq!(runner.unwritten_seals(), 0);
        let tx = runner.sender();
        for i in 0..3u32 {
            tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_000 + i * 60, 100.0))
                .expect("queued");
        }
        assert_eq!(runner.unwritten_seals(), 3, "three in the channel");
        // One cycle moves the channel into the ring and drains up to 4 of it;
        // the test writer is disconnected, so the drained ones spill to disk.
        let _ = runner.run_one_cycle(1_716_000_000);
        assert_eq!(
            runner.unwritten_seals(),
            runner.ring_len(),
            "after a cycle only the ring can hold seals"
        );
        let mut overflow = runner.overflow();
        let sink = overflow.split_escalation_offload();
        sink.pending_count()
            .fetch_add(5, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            runner.unwritten_seals(),
            runner.ring_len() + 5,
            "the escalation queue's count is shared with the runner"
        );
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn a_clean_shutdown_rewrites_the_marker_and_the_next_boot_reports_nothing() {
        let (spill, dlq) = temp_pair("mark-clean");
        // Leave an unclean marker as a crashed process would.
        let mut crashed = UnwrittenSealMark::in_dir(&spill);
        assert!(crashed.sample(42, 1_000));
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_secs(10),
            cancel_rx,
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        // The loop claimed the marker at boot: it is this process's now.
        match UnwrittenSealMark::in_dir(&spill).read_previous() {
            PreviousUnwritten::Read(r) => {
                assert!(!r.clean);
                assert_eq!(r.unwritten, 0, "boot overwrote the crashed process's count");
            }
            other => panic!("expected the loop's own marker, got {other:?}"),
        }
        cancel_tx.send(true).expect("send");
        let _ = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        match UnwrittenSealMark::in_dir(&spill).read_previous() {
            PreviousUnwritten::Read(r) => assert!(r.clean, "a clean shutdown marks clean"),
            other => panic!("expected a clean marker, got {other:?}"),
        }
        cleanup(&spill, &dlq);
    }

    #[test]
    fn the_loop_reads_the_previous_marker_before_it_writes_its_own() {
        // Source-order ratchet over production code only: reversing these
        // lines would overwrite the crashed process's count before reading it.
        let src = include_str!("seal_writer_loop.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        let body = prod
            .split("pub async fn run_seal_writer_loop(")
            .nth(1)
            .expect("loop fn");
        let read = body.find("mark.read_previous()").expect("reads the marker");
        let sample = body.find("mark.sample(").expect("claims the marker");
        let drain = body.find("runner.boot_drain()").expect("boot drain");
        assert!(read < sample && sample < drain);
        assert_eq!(
            body.matches("mark_clean_after_final_drain(&runner, &mut mark)")
                .count(),
            2,
            "both final-drain exits mark the shutdown clean"
        );
    }
}
