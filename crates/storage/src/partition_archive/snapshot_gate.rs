//! The WAL snapshot gate for hour-path tables (plan item PR AA, step 2;
//! owner Quote 51, 2026-11-07).
//!
//! # What changes, and why
//!
//! Every other table is archived only while QuestDB has applied EVERY WAL
//! transaction of the whole table (`writerTxn == sequencerTxn`). On a busy
//! depth table that is almost never true for long: the live writer commits
//! several times a second, so the exact check refused the whole table on
//! 233 attempts between 3 and 7 Nov and 15 pressure episodes freed nothing.
//!
//! For an OLD hour the question is narrower: has QuestDB applied every
//! transaction that could hold a row of THAT hour? The live drain writes
//! only into the current hour. Every writer that can post into an old hour
//! (the boot WAL refold, the WAL catch-up drain, the spill replays, the
//! after-close depth pass and the after-close contract name repair) takes
//! the shared side of the archive quiesce lock
//! ([`crate::archive_quiesce`]). So, with that lock held by the archiver,
//! an old hour's rows are all in transactions that already exist, and
//! waiting until `writerTxn >= S` (S = `sequencerTxn` read UNDER the lock)
//! proves they are applied, whatever the live writer commits meanwhile.
//!
//! # The protocol, per partition ([`archive_with_snapshot_gate`])
//!
//! 1. Pre-wait without the lock: read S0, wait until `writerTxn >= S0`
//!    (shortens the time the lock is held later).
//! 2. Take the archiver side of the quiesce lock, waiting at most
//!    `archive_quiesce_wait_secs`. On timeout the partition is kept and
//!    counted (`quiesce_busy`); the daily leg gives the attempt back.
//! 3. In-flight check ([`hour_in_flight_verdict`]): the hour is held when any
//!    of its frames is still on its way to QuestDB (unapplied ranges,
//!    after-close depth marks, a rescue batch, not yet acknowledged, a clock
//!    that stepped back, or a wall-clock step this process saw near the hour,
//!    `crate::wal_clock_step`). For a table on the hour path that was not
//!    forced past a pending spill file, the archiver also reads the table's
//!    own spill folder again here, under the lock (PR AA review): a batch
//!    rescued to a spill file counts as acknowledged, so only the folder
//!    shows its rows are not in QuestDB yet.
//! 4. Read S UNDER the lock and wait until `writerTxn >= S`, at most
//!    `archive_wal_snapshot_wait_secs`. A timeout keeps the partition and
//!    blocks the table for the rest of the run.
//! 5. Export, checksummed upload, verify, acknowledged audit row (unchanged).
//! 6. Read a fresh S2 and wait the same way, re-check the in-flight state,
//!    re-count (`count()` and `max(ts)` must match the verified tally), drop,
//!    then release the lock.
//!
//! Every refusal keeps the partition. Nothing here weakens the
//! verify-before-drop type state ([`VerifiedArchive`]).
//!
//! # Which tables
//!
//! [`snapshot_gate_table`]: the two depth tables, whose `ts` is the frame's
//! ARRIVAL time in IST (`market_depth`, and `market_depth_book`, both
//! stamped `received_at + IST offset` by the depth writer), in every pass.
//! Since step 3 of PR AA, also every table on the hour path in that pass
//! (`super::hour_path::uses_snapshot_gate`: `tick_quotes` and
//! `feed_aux_packets` under pressure, `ticks` only behind its switch); their
//! acknowledgements are the tick sink's ([`sink_for_table`]). Every other
//! table keeps the exact gate.
//!
//! # Complexity
//!
//! Cold path only: one archive partition at a time. Each WAL read is one
//! `wal_tables()` query (O(tables)); the wait polls every
//! [`SNAPSHOT_POLL`]. The in-flight check is O(`UNAPPLIED_BUCKETS` + 32 +
//! `DEFERRED_BUCKETS` + 16) atomic loads, once or twice per partition. Never
//! on the frame drain, never per tick.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{error, info, warn};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;

use super::{
    ArchiveOutcome, ArchiveRunSummary, DEPTH_TABLES, PartitionStep, RangeTally, VerifiedArchive,
    note_depth_partition_held, partition_range_bounds, partition_range_naive,
};
use crate::wal_applied_watermark::{AppliedSink, AppliedSnapshot, REPLAY_REORDER_SLACK_SEQ};
use crate::wal_deferred_depth::DeferredDepthSnapshot;
use crate::wal_suspension_watcher::WalTableRow;

/// How often a snapshot wait re-reads `wal_tables()`.
pub(crate) const SNAPSHOT_POLL: Duration = Duration::from_secs(SNAPSHOT_POLL_SECS);

/// [`SNAPSHOT_POLL`] in seconds.
pub(crate) const SNAPSHOT_POLL_SECS: u64 = 2;

/// Margin added past an hour's end when mapping it to frame sequences: a
/// row's `ts` comes from the frame's receipt instant (a monotonic anchor)
/// and its frame sequence from the wall clock at mint, so the two can differ
/// by a little.
pub(crate) const RECEIPT_SLOP_NANOS: u64 = 2_000_000_000;

/// A frame sequence this far ahead of the wall clock means the clock stepped
/// back (or the counter ran ahead of it), so frame sequences no longer map
/// to receipt hours and nothing can be proven about an hour.
pub(crate) const CLOCK_AHEAD_LIMIT_NANOS: u64 = 1_000_000_000;

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Whether `table` is archived through the snapshot gate in EVERY pass. The
/// two depth tables only (their `ts` is the arrival instant in IST). A table
/// on the hour path in one pass also takes the gate in that pass
/// (`super::hour_path::uses_snapshot_gate`); every other table keeps the
/// exact `writerTxn == sequencerTxn` gate. Pure, O(1).
#[must_use]
pub(crate) fn snapshot_gate_table(table: &str) -> bool {
    DEPTH_TABLES.contains(&table)
}

/// The applied-watermark sink whose acknowledgements cover `table`'s rows.
/// Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_snapshot_gate_tables_are_the_depth_tables_only
pub(crate) fn sink_for_table(table: &str) -> AppliedSink {
    if DEPTH_TABLES.contains(&table) {
        AppliedSink::Depth
    } else {
        AppliedSink::Ticks
    }
}

/// The frame-sequence span `[lo, hi]` (inclusive) that can hold a row of one
/// IST partition: the partition's UTC start less one reorder slack, to its
/// UTC end plus [`RECEIPT_SLOP_NANOS`] and one reorder slack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HourFrameSpan {
    pub(crate) lo: u64,
    pub(crate) hi: u64,
}

/// Maps a partition name (IST, `YYYY-MM-DD` or `YYYY-MM-DDTHH`) to the frame
/// sequences that can carry its rows. A frame sequence is the wall clock in
/// UTC nanoseconds at receipt; the partition is IST, so its bounds move back
/// by the IST offset. `None` for a malformed name or a span before the Unix
/// epoch (the caller keeps the partition). Pure, O(1).
#[must_use]
pub(crate) fn hour_frame_seq_span(partition: &str) -> Option<HourFrameSpan> {
    let (start, end) = partition_range_naive(partition)?;
    let offset = i64::from(IST_UTC_OFFSET_SECONDS);
    let start_utc = start.and_utc().timestamp().checked_sub(offset)?;
    let end_utc = end.and_utc().timestamp().checked_sub(offset)?;
    let start_nanos = u64::try_from(start_utc).ok()?.checked_mul(NANOS_PER_SEC)?;
    let end_nanos = u64::try_from(end_utc).ok()?.checked_mul(NANOS_PER_SEC)?;
    Some(HourFrameSpan {
        lo: start_nanos.saturating_sub(REPLAY_REORDER_SLACK_SEQ),
        hi: end_nanos
            .saturating_add(RECEIPT_SLOP_NANOS)
            .saturating_add(REPLAY_REORDER_SLACK_SEQ),
    })
}

/// One reading of a table's WAL against a snapshot target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalSnapshotVerdict {
    /// `writerTxn >= target`: every transaction that existed at the snapshot
    /// is applied.
    Reached { writer_txn: i64, target: i64 },
    /// Still applying; a wait that ends here is a `snapshot_timeout`.
    Behind { writer_txn: i64, target: i64 },
    /// WAL apply is suspended.
    Suspended,
    /// The table is not in `wal_tables()`.
    NotReported,
    /// The row lacks `writerTxn` or `sequencerTxn`.
    TxnUnreadable,
    /// `wal_tables()` itself could not be read.
    ProbeFailed,
}

impl WalSnapshotVerdict {
    /// Stable label for logs and the `cause` metric label. `Behind` is only
    /// reported once a wait ran out, hence `snapshot_timeout`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reached { .. } => "reached",
            Self::Behind { .. } => "snapshot_timeout",
            Self::Suspended => "suspended",
            Self::NotReported => "not_reported",
            Self::TxnUnreadable => "txn_unreadable",
            Self::ProbeFailed => "probe_failed",
        }
    }
}

/// The snapshot target for `table`: its current `sequencerTxn`. Fail-closed:
/// a missing, suspended or unreadable row is the refusal itself. Pure,
/// O(rows).
// TEST-EXEMPT: covered by test_wal_snapshot_verdict_truth_table
pub(crate) fn snapshot_target(
    rows: &[WalTableRow],
    table: &str,
) -> Result<i64, WalSnapshotVerdict> {
    // O(1) EXEMPT: begin — one wal_tables() row per table (tens of rows),
    // cold, at most a few times per archived partition.
    let Some(row) = rows.iter().find(|r| r.name == table) else {
        return Err(WalSnapshotVerdict::NotReported);
    };
    // O(1) EXEMPT: end
    if row.suspended {
        return Err(WalSnapshotVerdict::Suspended);
    }
    match (row.writer_txn, row.sequencer_txn) {
        (Some(_), Some(sequencer_txn)) => Ok(sequencer_txn),
        _ => Err(WalSnapshotVerdict::TxnUnreadable),
    }
}

/// Classifies `table`'s WAL against a FIXED snapshot `target`. Only a
/// non-suspended row whose `writerTxn` has reached the target is
/// [`WalSnapshotVerdict::Reached`]; later commits (a larger `sequencerTxn`)
/// do not matter. Pure, O(rows).
pub(crate) fn wal_snapshot_verdict(
    rows: &[WalTableRow],
    table: &str,
    target: i64,
) -> WalSnapshotVerdict {
    // O(1) EXEMPT: begin — as in `snapshot_target`.
    let Some(row) = rows.iter().find(|r| r.name == table) else {
        return WalSnapshotVerdict::NotReported;
    };
    // O(1) EXEMPT: end
    if row.suspended {
        return WalSnapshotVerdict::Suspended;
    }
    match (row.writer_txn, row.sequencer_txn) {
        (Some(writer_txn), Some(_)) if writer_txn >= target => {
            WalSnapshotVerdict::Reached { writer_txn, target }
        }
        (Some(writer_txn), Some(_)) => WalSnapshotVerdict::Behind { writer_txn, target },
        _ => WalSnapshotVerdict::TxnUnreadable,
    }
}

/// How many [`SNAPSHOT_POLL`] waits fit in `wait_secs` (rounded up, so any
/// non-zero wait polls at least once). Pure, O(1).
#[must_use]
pub(crate) const fn snapshot_polls_for(wait_secs: u64) -> u32 {
    let polls = wait_secs.div_ceil(SNAPSHOT_POLL_SECS);
    if polls > u32::MAX as u64 {
        u32::MAX
    } else {
        // APPROVED: cast — bounded by the check above.
        polls as u32
    }
}

/// Why an old hour cannot be archived right now, or `Clear`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InFlightVerdict {
    /// Nothing of the hour is on its way to QuestDB.
    Clear,
    /// The after-close depth pass still has rows of this hour to write back.
    DeferredDepth,
    /// The process has no WAL bound, so no frame record can answer (fail
    /// closed; normally only before the lane binds its WAL at boot).
    WalNotBound,
    /// A frame of the hour is recorded as captured but not applied (shed,
    /// rescue failure, a suspicion episode, a boot tail).
    Unapplied,
    /// A rescue batch that can hold a frame of the hour is not yet settled.
    RescueFloor,
    /// The table's sink has not acknowledged past the hour's last frame plus
    /// one reorder slack.
    NotAcked,
    /// The newest frame sequence is ahead of the wall clock, so frame
    /// sequences no longer map to hours.
    ClockAhead,
    /// This process saw a wall-clock step (receipt anchor, or a segment
    /// rotation that found the frame counter ahead of the wall) whose window
    /// overlaps the hour: its rows can carry frame sequences outside the
    /// hour's span, so no frame check can clear it (PR AA review, finding 2).
    /// Held until the next restart, which forgets the step (RAM only).
    ClockStepped,
    /// The table's own spill folder holds a file the spill replay has not
    /// read yet (PR AA review). A rescued batch counts as acknowledged, so
    /// none of the frame checks above sees its rows; the folder is read
    /// again under the quiesce lock, where no replay can be posting.
    SpillPending,
}

impl InFlightVerdict {
    /// Stable label for logs and the `cause` metric label.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::DeferredDepth => "deferred_depth",
            Self::WalNotBound => "wal_not_bound",
            Self::Unapplied => "unapplied",
            Self::RescueFloor => "rescue_floor",
            Self::NotAcked => "not_acked",
            Self::ClockAhead => "clock_ahead",
            Self::ClockStepped => "clock_stepped",
            Self::SpillPending => "spill_pending",
        }
    }
}

/// Everything [`hour_in_flight_verdict`] reads, taken by the caller so the
/// verdict stays pure.
pub(crate) struct InFlightInputs<'a> {
    /// Whether this process's applied watermark is bound to a WAL directory.
    pub(crate) wal_bound: bool,
    /// The applied-watermark state (unapplied buckets and ranges).
    pub(crate) applied: &'a AppliedSnapshot,
    /// After-close depth marks; `Some` for a depth table only.
    pub(crate) deferred: Option<&'a DeferredDepthSnapshot>,
    /// The lowest held rescue floor of the table's sink (`u64::MAX`: none).
    pub(crate) rescue_floor_lowest: u64,
    /// The raw acknowledged high-water mark of the table's sink.
    pub(crate) acked_high: u64,
    /// The newest frame sequence minted in this process.
    pub(crate) last_frame_seq: u64,
    /// The wall clock now, UTC nanoseconds.
    pub(crate) wall_nanos: u64,
    /// The wall-clock step window this process recorded (UTC nanos), or
    /// `None` (`crate::wal_clock_step::clock_step_window`).
    pub(crate) clock_step: Option<(u64, u64)>,
}

/// Whether any row of the hour spanning `span` may still be on its way to
/// QuestDB. Fail-closed: every check that cannot answer holds the hour.
/// Checked in a fixed order; the first hit is the reason. Pure, O(1) plus
/// the snapshot lookups (bounded tables).
#[must_use]
// TEST-EXEMPT: covered by test_in_flight_clear_when_nothing_is_on_its_way
pub(crate) fn hour_in_flight_verdict(
    span: HourFrameSpan,
    inputs: &InFlightInputs<'_>,
) -> InFlightVerdict {
    // Deferred depth first: the after-close pass clears these, so a hold for
    // this reason is "stand aside", never a failure.
    if let Some(deferred) = inputs.deferred
        && deferred.segment_is_deferred(span.lo, Some(span.hi))
    {
        return InFlightVerdict::DeferredDepth;
    }
    if !inputs.wal_bound {
        return InFlightVerdict::WalNotBound;
    }
    if inputs.applied.range_has_unapplied(span.lo, span.hi) {
        return InFlightVerdict::Unapplied;
    }
    if inputs.rescue_floor_lowest <= span.hi {
        return InFlightVerdict::RescueFloor;
    }
    // Every frame up to `hi` was handed to the sink before any frame past
    // `hi + slack` (reordering is bounded by the slack), and the sink acks
    // batches in order, so an ack past that point covers the whole hour.
    if inputs.acked_high <= span.hi.saturating_add(REPLAY_REORDER_SLACK_SEQ) {
        return InFlightVerdict::NotAcked;
    }
    if inputs.last_frame_seq > inputs.wall_nanos.saturating_add(CLOCK_AHEAD_LIMIT_NANOS) {
        return InFlightVerdict::ClockAhead;
    }
    if crate::wal_clock_step::window_overlaps(inputs.clock_step, span.lo, span.hi) {
        return InFlightVerdict::ClockStepped;
    }
    InFlightVerdict::Clear
}

/// [`hour_in_flight_verdict`] over this process's live state: the applied
/// watermark, the after-close depth marks (depth tables only), the rescue
/// floors, the newest frame sequence and the wall clock. Cold, O(bounded
/// tables).
#[must_use]
pub(crate) fn process_in_flight_verdict(table: &str, span: HourFrameSpan) -> InFlightVerdict {
    let watermark = crate::wal_applied_watermark::applied_watermark();
    let applied = watermark.snapshot();
    let sink = sink_for_table(table);
    let deferred = DEPTH_TABLES
        .contains(&table)
        .then(|| crate::wal_deferred_depth::deferred_depth().snapshot());
    let acked_high = match sink {
        AppliedSink::Depth => applied.hwm_depth,
        AppliedSink::Ticks => applied.hwm_ticks,
    };
    let inputs = InFlightInputs {
        wal_bound: watermark.is_bound(),
        applied: &applied,
        deferred: deferred.as_ref(),
        rescue_floor_lowest: watermark.rescue_floor_lowest(sink),
        acked_high,
        last_frame_seq: crate::ws_frame_spill::current_frame_seq(),
        wall_nanos: crate::wal_applied_watermark::wall_nanos(),
        clock_step: crate::wal_clock_step::clock_step_window(),
    };
    hour_in_flight_verdict(span, &inputs)
}

/// How the archiver reads in-flight state; the process reader in production,
/// a scripted one in tests.
pub(crate) type InFlightReader = Arc<dyn Fn(&str, HourFrameSpan) -> InFlightVerdict + Send + Sync>;

/// A partition copied to S3 and verified, waiting for its drop.
#[derive(Debug)]
pub(crate) struct CopiedPartition {
    /// The verify-before-drop type state.
    pub(crate) proof: VerifiedArchive,
    /// Where the bytes live in S3.
    pub(crate) s3_key: String,
    /// Whether THIS run uploaded the object (only then may a mismatch delete
    /// it).
    pub(crate) uploaded_this_run: bool,
    /// `count()` and `max(ts)` read right after the export.
    pub(crate) verified_tally: Option<RangeTally>,
}

/// The archiver's I/O, so the snapshot protocol can be driven by a scripted
/// fake in tests (call order, waits, failures) and by
/// [`super::PartitionArchiver`] in production.
pub(crate) trait ArchiveIo {
    /// What holds the old-hour writers out while it lives.
    type Hold: Send;

    /// One `wal_tables()` read.
    fn read_wal_rows(&mut self) -> impl Future<Output = Result<Vec<WalTableRow>>> + Send;

    /// The archiver side of the quiesce lock, waiting at most `wait`.
    fn hold_old_hour_writers(
        &mut self,
        wait: Duration,
    ) -> impl Future<Output = Option<Self::Hold>> + Send;

    /// The in-flight state of `table`'s hour spanning `span`, now.
    fn in_flight_now(&mut self, table: &str, span: HourFrameSpan) -> InFlightVerdict;

    /// Waits `duration` between WAL reads.
    fn pause_for(&mut self, duration: Duration) -> impl Future<Output = ()> + Send;

    /// Export, checksummed upload, verify and the acknowledged `verified`
    /// audit row. `None` after a recorded failure (partition kept).
    fn copy_and_verify(
        &mut self,
        table: &str,
        partition: &str,
        start: &str,
        end: &str,
        summary: &mut ArchiveRunSummary,
    ) -> impl Future<Output = Option<CopiedPartition>> + Send;

    /// `count()` and `max(ts)` over the partition range now.
    fn recount_range(
        &mut self,
        table: &str,
        start: &str,
        end: &str,
    ) -> impl Future<Output = Option<RangeTally>> + Send;

    /// Deletes an S3 object this run uploaded.
    fn delete_own_upload(&mut self, key: &str) -> impl Future<Output = Result<()>> + Send;

    /// Records one kept partition (counter, audit row, coded error).
    fn note_failure(
        &mut self,
        table: &str,
        partition: &str,
        outcome: ArchiveOutcome,
        detail: &str,
        summary: &mut ArchiveRunSummary,
    );

    /// The DROP, with its audit row and counters.
    fn drop_verified(
        &mut self,
        copied: CopiedPartition,
        summary: &mut ArchiveRunSummary,
    ) -> impl Future<Output = ()> + Send;
}

/// The per-run settings of the snapshot gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotGate {
    /// Longest wait for the quiesce lock.
    pub(crate) quiesce_wait: Duration,
    /// Polls allowed per snapshot wait.
    pub(crate) snapshot_polls: u32,
}

impl SnapshotGate {
    /// From `[partition_retention]` (`archive_quiesce_wait_secs`,
    /// `archive_wal_snapshot_wait_secs`). Pure, O(1).
    #[must_use]
    pub(crate) fn from_config(cfg: &tickvault_common::config::PartitionRetentionConfig) -> Self {
        Self {
            quiesce_wait: Duration::from_secs(cfg.archive_quiesce_wait_secs),
            snapshot_polls: snapshot_polls_for(cfg.archive_wal_snapshot_wait_secs),
        }
    }
}

/// Reads `table`'s `sequencerTxn` NOW and waits until `writerTxn` reaches
/// it, at most `polls` pauses of [`SNAPSHOT_POLL`]. Returns the last verdict
/// (`Reached` on success). The target is read once, at the call, so a
/// caller holding the quiesce lock gets a target taken under the lock.
// TEST-EXEMPT: covered by test_snapshot_timeout_keeps_and_blocks_the_table_with_bounded_polls
pub(crate) async fn wait_for_snapshot<I: ArchiveIo>(
    io: &mut I,
    table: &str,
    polls: u32,
) -> WalSnapshotVerdict {
    let rows = match io.read_wal_rows().await {
        Ok(rows) => rows,
        Err(_) => return WalSnapshotVerdict::ProbeFailed,
    };
    let target = match snapshot_target(&rows, table) {
        Ok(target) => target,
        Err(refusal) => return refusal,
    };
    let mut verdict = wal_snapshot_verdict(&rows, table, target);
    let mut used: u32 = 0;
    loop {
        match verdict {
            WalSnapshotVerdict::Behind { .. } if used < polls => {}
            other => {
                // PR AA step 6 (AA-6): how long this wait took, local
                // `/metrics` only.
                #[allow(clippy::cast_precision_loss)]
                // APPROVED: a poll count times 2 s, far below 2^53
                let waited = (u64::from(used) * SNAPSHOT_POLL_SECS) as f64;
                metrics::gauge!(super::WAL_SNAPSHOT_WAIT_GAUGE).set(waited);
                return other;
            }
        }
        io.pause_for(SNAPSHOT_POLL).await;
        used = used.saturating_add(1);
        verdict = match io.read_wal_rows().await {
            Ok(rows) => wal_snapshot_verdict(&rows, table, target),
            Err(_) => WalSnapshotVerdict::ProbeFailed,
        };
    }
}

/// Archives one hour-path partition through the snapshot protocol (see the
/// module docs). Every refusal keeps the partition.
// TEST-EXEMPT: covered by test_happy_path_drops_once_with_the_writers_held_out
pub(crate) async fn archive_with_snapshot_gate<I: ArchiveIo>(
    io: &mut I,
    table: &'static str,
    partition: &str,
    gate: SnapshotGate,
    summary: &mut ArchiveRunSummary,
) -> PartitionStep {
    let (Some((start, end)), Some(span)) = (
        partition_range_bounds(partition),
        hour_frame_seq_span(partition),
    ) else {
        io.note_failure(
            table,
            partition,
            ArchiveOutcome::ExportFailed,
            "unparseable partition name (no time range or frame span)",
            summary,
        );
        return PartitionStep::Done;
    };

    // 1. Pre-wait, WITHOUT the lock: only to shorten the locked wait later.
    let pre = wait_for_snapshot(io, table, snapshot_polls_or_one(gate)).await;
    if !matches!(pre, WalSnapshotVerdict::Reached { .. }) {
        note_snapshot_refused(table, partition, "pre_wait", pre);
        return PartitionStep::WalNotAppliedBeforeExport;
    }

    // 2. Hold every writer that can post into an old hour.
    let Some(hold) = io.hold_old_hour_writers(gate.quiesce_wait).await else {
        note_quiesce_busy(table, partition, gate.quiesce_wait, summary);
        return PartitionStep::QuiesceBusy;
    };

    // 3. Nothing of the hour may still be on its way to QuestDB.
    let in_flight = io.in_flight_now(table, span);
    if in_flight != InFlightVerdict::Clear {
        note_in_flight(table, partition, "pre_export", in_flight, summary);
        return PartitionStep::Held;
    }

    // 4. The snapshot, read UNDER the lock: every transaction that can hold
    //    a row of this hour exists now, so reaching it proves them applied.
    let snapshot = wait_for_snapshot(io, table, gate.snapshot_polls).await;
    if !matches!(snapshot, WalSnapshotVerdict::Reached { .. }) {
        note_snapshot_refused(table, partition, "snapshot", snapshot);
        return PartitionStep::WalNotAppliedBeforeExport;
    }

    // 5. Export, upload, verify, audit (unchanged).
    let Some(copied) = io
        .copy_and_verify(table, partition, &start, &end, summary)
        .await
    else {
        return PartitionStep::Done;
    };

    // 6. A fresh snapshot and the in-flight state again, then the recount
    //    and the drop. A refusal here keeps the partition AND the S3 object:
    //    the object is a correct copy of what was applied at the export, and
    //    the next run reuses it or writes a content-addressed sidecar.
    let before_drop = wait_for_snapshot(io, table, gate.snapshot_polls).await;
    if !matches!(before_drop, WalSnapshotVerdict::Reached { .. }) {
        metrics::counter!("tv_partition_drop_withheld_total").increment(1);
        note_snapshot_refused(table, partition, "pre_drop", before_drop);
        return PartitionStep::WalNotAppliedBeforeDrop;
    }
    let in_flight = io.in_flight_now(table, span);
    if in_flight != InFlightVerdict::Clear {
        metrics::counter!("tv_partition_drop_withheld_total").increment(1);
        note_in_flight(table, partition, "pre_drop", in_flight, summary);
        return PartitionStep::Done;
    }
    let step = recheck_and_drop(io, table, partition, &start, &end, copied, summary).await;
    drop(hold);
    step
}

/// The pre-wait gets at least one poll even when the configured snapshot
/// wait is 0, so a table one transaction behind does not fail before the
/// locked wait is even tried. Pure, O(1).
const fn snapshot_polls_or_one(gate: SnapshotGate) -> u32 {
    if gate.snapshot_polls == 0 {
        1
    } else {
        gate.snapshot_polls
    }
}

/// The last-moment recount and the DROP, shared by both gates.
///
/// The re-read compares `count()` AND `max(ts)` with the values taken right
/// after the export; any difference (or no answer) keeps the partition. When
/// THIS run uploaded the object it is removed, so the next run's export (of
/// the changed partition) is not refused as an S3 conflict forever; an object
/// an earlier run wrote is left alone (it may be the only copy of data
/// already dropped).
// TEST-EXEMPT: covered by test_pre_drop_recount_mismatch_keeps_and_deletes_own_upload
pub(crate) async fn recheck_and_drop<I: ArchiveIo>(
    io: &mut I,
    table: &str,
    partition: &str,
    start: &str,
    end: &str,
    copied: CopiedPartition,
    summary: &mut ArchiveRunSummary,
) -> PartitionStep {
    let recheck = io.recount_range(table, start, end).await;
    match recheck {
        Some(ref now)
            if Some(now) == copied.verified_tally.as_ref() && now.rows == copied.proof.rows => {}
        other => {
            let verified_max = copied
                .verified_tally
                .as_ref()
                .and_then(|t| t.max_ts.clone())
                .unwrap_or_default();
            let detail = match other {
                Some(now) => format!(
                    "partition CHANGED between verify and drop: verified {} rows (max ts \
                     '{verified_max}'), now {} rows (max ts '{}') — a writer touched this \
                     partition (15:31 sweep / WAL replay / DLQ drain / deferred depth). The S3 \
                     copy is stale, so the partition is KEPT, the stale object is removed, and \
                     it is re-exported next run",
                    copied.proof.rows,
                    now.rows,
                    now.max_ts.unwrap_or_default()
                ),
                None => format!(
                    "pre-drop recount UNAVAILABLE (verified {} rows) — cannot prove the \
                     partition is unchanged, so the drop is withheld and retried next run",
                    copied.proof.rows
                ),
            };
            metrics::counter!("tv_partition_drop_withheld_total").increment(1);
            if copied.uploaded_this_run
                && let Err(err) = io.delete_own_upload(&copied.s3_key).await
            {
                warn!(
                    table,
                    partition,
                    s3_key = %copied.s3_key,
                    error = %format!("{err:#}"),
                    "could not remove the stale archive object after a pre-drop mismatch — \
                     the next run will raise S3Conflict on this key until it is removed"
                );
            }
            io.note_failure(
                table,
                partition,
                ArchiveOutcome::CountMismatch,
                &detail,
                summary,
            );
            return PartitionStep::Done;
        }
    }
    io.drop_verified(copied, summary).await;
    PartitionStep::Done
}

/// Counts and logs a snapshot wait that did not reach its target. The
/// partition is kept and (by the caller's step) the table is skipped for the
/// rest of the run.
fn note_snapshot_refused(
    table: &str,
    partition: &str,
    stage: &'static str,
    verdict: WalSnapshotVerdict,
) {
    metrics::counter!(
        "tv_partition_archive_skipped_total",
        "reason" => "wal_not_applied",
        "cause" => verdict.as_str(),
        "stage" => stage
    )
    .increment(1);
    match verdict {
        WalSnapshotVerdict::Behind { writer_txn, target } => {
            warn!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                table,
                partition,
                stage,
                writer_txn,
                target,
                "partition archive: QuestDB did not apply the WAL snapshot inside the wait — \
                 partition KEPT and the table skipped this run; the next run retries"
            );
        }
        WalSnapshotVerdict::Suspended => {
            warn!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                table,
                partition,
                stage,
                "partition archive: table is WAL-SUSPENDED — partition KEPT and the table \
                 skipped (see WAL-SUSPEND-01)"
            );
        }
        WalSnapshotVerdict::Reached { .. }
        | WalSnapshotVerdict::NotReported
        | WalSnapshotVerdict::TxnUnreadable
        | WalSnapshotVerdict::ProbeFailed => {
            error!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                table,
                partition,
                stage,
                cause = verdict.as_str(),
                "partition archive: wal_tables() does not let the WAL snapshot be proven \
                 applied — partition KEPT and the table skipped this run (fail-closed)"
            );
        }
    }
}

/// Counts a partition kept because the quiesce lock could not be taken in
/// time (an old-hour writer is busy). Not a failure: a later pass archives it.
fn note_quiesce_busy(
    table: &str,
    partition: &str,
    waited: Duration,
    summary: &mut ArchiveRunSummary,
) {
    summary.held_quiesce_busy = summary.held_quiesce_busy.saturating_add(1);
    count_quiesce_busy_held(1);
    info!(
        table,
        partition,
        waited_secs = waited.as_secs(),
        "partition archive: a replay or the after-close depth pass is writing old hours — \
         partition KEPT for a later pass (not a failure)"
    );
}

/// Adds `n` to `tv_partition_archive_skipped_total{reason="quiesce_busy"}`.
/// The one place its labels are written. Local `/metrics` only. O(1).
// TEST-EXEMPT: covered by test_quiesce_timeout_keeps_the_partition_and_exports_nothing
pub(crate) fn count_quiesce_busy_held(n: u64) {
    metrics::counter!(
        "tv_partition_archive_skipped_total",
        "reason" => "quiesce_busy",
        "cause" => "writers_busy",
        "stage" => "pre_snapshot"
    )
    .increment(n);
}

/// Counts and logs an hour held because some of its rows may still be on
/// their way to QuestDB. Deferred-depth holds go on the existing
/// `deferred_depth_pass` series and `held_deferred_depth` (the daily leg and
/// the pressure leg read them as "stand aside").
fn note_in_flight(
    table: &str,
    partition: &str,
    stage: &'static str,
    verdict: InFlightVerdict,
    summary: &mut ArchiveRunSummary,
) {
    if verdict == InFlightVerdict::DeferredDepth {
        note_depth_partition_held(summary);
        info!(
            table,
            partition,
            stage,
            "partition archive: the after-close depth pass still has rows of this hour to \
             write back — partition KEPT until they are in (not a failure)"
        );
        return;
    }
    summary.held_in_flight = summary.held_in_flight.saturating_add(1);
    metrics::counter!(
        "tv_partition_archive_skipped_total",
        "reason" => "in_flight",
        "cause" => verdict.as_str(),
        "stage" => stage
    )
    .increment(1);
    match verdict {
        InFlightVerdict::ClockAhead
        | InFlightVerdict::ClockStepped
        | InFlightVerdict::WalNotBound => {
            warn!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                table,
                partition,
                stage,
                cause = verdict.as_str(),
                "partition archive: frame sequences cannot be mapped to this hour (no WAL \
                 bound, or the clock stepped back) — partition KEPT (fail-closed)"
            );
        }
        InFlightVerdict::Clear
        | InFlightVerdict::DeferredDepth
        | InFlightVerdict::Unapplied
        | InFlightVerdict::RescueFloor
        | InFlightVerdict::NotAcked
        | InFlightVerdict::SpillPending => {
            info!(
                table,
                partition,
                stage,
                cause = verdict.as_str(),
                "partition archive: rows of this hour may still be on their way to QuestDB — \
                 partition KEPT for a later pass"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_quiesce::ArchiveQuiesce;
    use crate::wal_applied_watermark::AppliedWatermark;
    use crate::wal_deferred_depth::{DeferredDepth, ShedDepth};

    const SEC: u64 = NANOS_PER_SEC;
    const DEPTH: &str = "market_depth";
    const PARTITION: &str = "2026-11-07T10";

    fn row(name: &str, suspended: bool, writer: Option<i64>, seq: Option<i64>) -> WalTableRow {
        WalTableRow {
            name: name.to_string(),
            suspended,
            writer_txn: writer,
            sequencer_txn: seq,
            error_tag: None,
            error_message: None,
        }
    }

    // ---- wal_snapshot_verdict / snapshot_target ------------------------

    #[test]
    fn test_wal_snapshot_verdict_truth_table() {
        let rows = [
            row("lagging", false, Some(5), Some(9)),
            row("equal", false, Some(9), Some(9)),
            row("above", false, Some(12), Some(15)),
            row("suspended", true, Some(9), Some(9)),
            row("no_writer", false, None, Some(9)),
            row("no_seq", false, Some(9), None),
        ];
        assert_eq!(
            wal_snapshot_verdict(&rows, "lagging", 9),
            WalSnapshotVerdict::Behind {
                writer_txn: 5,
                target: 9
            }
        );
        assert_eq!(
            wal_snapshot_verdict(&rows, "equal", 9),
            WalSnapshotVerdict::Reached {
                writer_txn: 9,
                target: 9
            }
        );
        // Later commits (sequencerTxn 15) do not matter once the writer has
        // passed the snapshot target.
        assert_eq!(
            wal_snapshot_verdict(&rows, "above", 10),
            WalSnapshotVerdict::Reached {
                writer_txn: 12,
                target: 10
            }
        );
        assert_eq!(
            wal_snapshot_verdict(&rows, "suspended", 9),
            WalSnapshotVerdict::Suspended
        );
        assert_eq!(
            wal_snapshot_verdict(&rows, "no_writer", 9),
            WalSnapshotVerdict::TxnUnreadable
        );
        assert_eq!(
            wal_snapshot_verdict(&rows, "no_seq", 9),
            WalSnapshotVerdict::TxnUnreadable
        );
        assert_eq!(
            wal_snapshot_verdict(&rows, "absent", 9),
            WalSnapshotVerdict::NotReported
        );

        assert_eq!(snapshot_target(&rows, "lagging"), Ok(9));
        assert_eq!(snapshot_target(&rows, "above"), Ok(15));
        assert_eq!(
            snapshot_target(&rows, "suspended"),
            Err(WalSnapshotVerdict::Suspended)
        );
        assert_eq!(
            snapshot_target(&rows, "no_writer"),
            Err(WalSnapshotVerdict::TxnUnreadable)
        );
        assert_eq!(
            snapshot_target(&rows, "absent"),
            Err(WalSnapshotVerdict::NotReported)
        );
    }

    #[test]
    fn test_wal_snapshot_verdict_labels_are_stable_and_distinct() {
        let all = [
            WalSnapshotVerdict::Reached {
                writer_txn: 0,
                target: 0,
            },
            WalSnapshotVerdict::Behind {
                writer_txn: 0,
                target: 0,
            },
            WalSnapshotVerdict::Suspended,
            WalSnapshotVerdict::NotReported,
            WalSnapshotVerdict::TxnUnreadable,
            WalSnapshotVerdict::ProbeFailed,
        ];
        let labels: Vec<&str> = all.iter().map(|v| v.as_str()).collect();
        assert_eq!(
            labels,
            [
                "reached",
                "snapshot_timeout",
                "suspended",
                "not_reported",
                "txn_unreadable",
                "probe_failed"
            ]
        );
    }

    #[test]
    fn test_snapshot_polls_for_rounds_up() {
        assert_eq!(snapshot_polls_for(0), 0);
        assert_eq!(snapshot_polls_for(1), 1);
        assert_eq!(snapshot_polls_for(2), 1);
        assert_eq!(snapshot_polls_for(3), 2);
        assert_eq!(snapshot_polls_for(300), 150);
        assert_eq!(snapshot_polls_for(u64::MAX), u32::MAX);
        let cfg = tickvault_common::config::PartitionRetentionConfig::default();
        let gate = SnapshotGate::from_config(&cfg);
        assert_eq!(gate.quiesce_wait, Duration::from_secs(120));
        assert_eq!(gate.snapshot_polls, 150);
    }

    // ---- hour_frame_seq_span -------------------------------------------

    fn span_of(start_utc: u64, end_utc: u64) -> HourFrameSpan {
        HourFrameSpan {
            lo: start_utc * SEC - REPLAY_REORDER_SLACK_SEQ,
            hi: end_utc * SEC + RECEIPT_SLOP_NANOS + REPLAY_REORDER_SLACK_SEQ,
        }
    }

    /// The UTC epochs below were computed with `date -u -d`, not with the
    /// chrono call under test.
    #[test]
    fn test_hour_frame_seq_span_maps_ist_hours_to_utc_frame_sequences() {
        // 10:00 IST = 04:30 UTC.
        assert_eq!(
            hour_frame_seq_span("2026-11-07T10"),
            Some(span_of(1_794_025_800, 1_794_029_400))
        );
        // IST midnight: 2026-11-07T00 IST = 2026-11-06 18:30 UTC.
        assert_eq!(
            hour_frame_seq_span("2026-11-07T00"),
            Some(span_of(1_793_989_800, 1_793_993_400))
        );
        // Across UTC midnight: 05:00 IST = 23:30 UTC the day before, ending
        // 00:30 UTC.
        assert_eq!(
            hour_frame_seq_span("2026-11-07T05"),
            Some(span_of(1_794_007_800, 1_794_011_400))
        );
        // Month rollover: the hour ends at 2026-12-01T00 IST.
        assert_eq!(
            hour_frame_seq_span("2026-11-30T23"),
            Some(span_of(1_796_059_800, 1_796_063_400))
        );
        // A day partition spans the whole IST day.
        assert_eq!(
            hour_frame_seq_span("2026-11-07"),
            Some(span_of(1_793_989_800, 1_793_989_800 + 86_400))
        );
    }

    #[test]
    fn test_hour_frame_seq_span_refuses_garbage_and_pre_epoch() {
        assert_eq!(hour_frame_seq_span(""), None);
        assert_eq!(hour_frame_seq_span("2026-11-07T24"), None);
        assert_eq!(hour_frame_seq_span("not-a-date"), None);
        // 1970-01-01T00 IST starts before the Unix epoch in UTC.
        assert_eq!(hour_frame_seq_span("1970-01-01T00"), None);
    }

    #[test]
    fn test_snapshot_gate_tables_are_the_depth_tables_only() {
        assert!(snapshot_gate_table("market_depth"));
        assert!(snapshot_gate_table("market_depth_book"));
        for table in [
            "ticks",
            "tick_quotes",
            "feed_aux_packets",
            "candles_1m",
            "order_audit",
        ] {
            assert!(!snapshot_gate_table(table), "{table} keeps the exact gate");
        }
        assert_eq!(sink_for_table("market_depth"), AppliedSink::Depth);
        assert_eq!(sink_for_table("market_depth_book"), AppliedSink::Depth);
        assert_eq!(sink_for_table("ticks"), AppliedSink::Ticks);
    }

    // ---- hour_in_flight_verdict ----------------------------------------

    fn clear_inputs(applied: &AppliedSnapshot, span: HourFrameSpan) -> InFlightInputs<'_> {
        InFlightInputs {
            wal_bound: true,
            applied,
            deferred: None,
            rescue_floor_lowest: u64::MAX,
            acked_high: span.hi + REPLAY_REORDER_SLACK_SEQ + 1,
            last_frame_seq: span.hi + 3_600 * SEC,
            wall_nanos: span.hi + 3_600 * SEC,
            clock_step: None,
        }
    }

    fn the_span() -> HourFrameSpan {
        hour_frame_seq_span(PARTITION).expect("span")
    }

    #[test]
    fn test_in_flight_clear_when_nothing_is_on_its_way() {
        let applied = AppliedSnapshot::default();
        let span = the_span();
        assert_eq!(
            hour_in_flight_verdict(span, &clear_inputs(&applied, span)),
            InFlightVerdict::Clear
        );
    }

    #[test]
    fn test_in_flight_holds_an_hour_with_after_close_depth_marks() {
        let applied = AppliedSnapshot::default();
        let span = the_span();
        let marks = DeferredDepth::new_for_tests();
        // A frame in the middle of the hour was shed under load.
        marks.note_shed(span.lo + 1_800 * SEC, ShedDepth::Inline);
        let snapshot = marks.snapshot();
        let inputs = InFlightInputs {
            deferred: Some(&snapshot),
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::DeferredDepth
        );
        // A mark a day later does not hold this hour.
        let later = DeferredDepth::new_for_tests();
        later.note_shed(span.hi + 86_400 * SEC, ShedDepth::Inline);
        let later_snapshot = later.snapshot();
        let inputs = InFlightInputs {
            deferred: Some(&later_snapshot),
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::Clear
        );
    }

    #[test]
    fn test_in_flight_holds_when_no_wal_is_bound() {
        let applied = AppliedSnapshot::default();
        let span = the_span();
        let inputs = InFlightInputs {
            wal_bound: false,
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::WalNotBound
        );
    }

    #[test]
    fn test_in_flight_holds_an_hour_with_unapplied_frames() {
        let span = the_span();
        let watermark = AppliedWatermark::new_for_tests();
        watermark.note_unapplied(span.lo + 600 * SEC);
        let applied = watermark.snapshot();
        assert_eq!(
            hour_in_flight_verdict(span, &clear_inputs(&applied, span)),
            InFlightVerdict::Unapplied
        );
    }

    #[test]
    fn test_in_flight_holds_while_a_rescue_batch_can_carry_the_hour() {
        let applied = AppliedSnapshot::default();
        let span = the_span();
        for floor in [span.lo, span.hi, 1] {
            let inputs = InFlightInputs {
                rescue_floor_lowest: floor,
                ..clear_inputs(&applied, span)
            };
            assert_eq!(
                hour_in_flight_verdict(span, &inputs),
                InFlightVerdict::RescueFloor,
                "floor {floor}"
            );
        }
        // A floor after the hour does not hold it.
        let inputs = InFlightInputs {
            rescue_floor_lowest: span.hi + 1,
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::Clear
        );
        // The watermark accessor reports the lowest held floor.
        let watermark = AppliedWatermark::new_for_tests();
        assert_eq!(watermark.rescue_floor_lowest(AppliedSink::Depth), u64::MAX);
        let floor = watermark.hold_rescue_floor(AppliedSink::Depth, 700, 900);
        assert!(floor.is_some());
        assert_eq!(watermark.rescue_floor_lowest(AppliedSink::Depth), 700);
        assert_eq!(watermark.rescue_floor_lowest(AppliedSink::Ticks), u64::MAX);
        assert!(!watermark.is_bound(), "a test instance is never bound");
    }

    #[test]
    fn test_in_flight_holds_until_the_sink_acks_past_the_hour_and_its_slack() {
        let applied = AppliedSnapshot::default();
        let span = the_span();
        for acked in [0, span.lo, span.hi, span.hi + REPLAY_REORDER_SLACK_SEQ] {
            let inputs = InFlightInputs {
                acked_high: acked,
                ..clear_inputs(&applied, span)
            };
            assert_eq!(
                hour_in_flight_verdict(span, &inputs),
                InFlightVerdict::NotAcked,
                "acked {acked}"
            );
        }
        let inputs = InFlightInputs {
            acked_high: span.hi + REPLAY_REORDER_SLACK_SEQ + 1,
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::Clear
        );
    }

    #[test]
    fn test_in_flight_holds_when_the_clock_stepped_back() {
        let applied = AppliedSnapshot::default();
        let span = the_span();
        let base = clear_inputs(&applied, span);
        let inputs = InFlightInputs {
            last_frame_seq: base.wall_nanos + CLOCK_AHEAD_LIMIT_NANOS + 1,
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::ClockAhead
        );
        let inputs = InFlightInputs {
            last_frame_seq: base.wall_nanos + CLOCK_AHEAD_LIMIT_NANOS,
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::Clear
        );
    }

    /// PR AA review finding 2. A 600 s step back at 11:05 IST: rows land in
    /// hour 10 with frame sequences of about 11:05, so a mark on them is
    /// outside hour 10's span. Every frame check reads clear (acked past the
    /// span, no ranges, counter not ahead of the wall); only the recorded
    /// step holds the hour. UTC epochs computed with `date -u -d`.
    #[test]
    fn test_in_flight_holds_an_hour_near_a_backward_clock_step() {
        let applied = AppliedSnapshot::default();
        let span = hour_frame_seq_span("2026-11-07T10").expect("span");
        // 2026-11-07 11:05:00 IST = 05:35:00 UTC.
        let step_at = 1_794_029_700 * SEC;
        let window = crate::wal_clock_step::step_window(step_at, 600 * SEC);
        let stepped = InFlightInputs {
            clock_step: window,
            ..clear_inputs(&applied, span)
        };
        assert!(stepped.last_frame_seq <= stepped.wall_nanos);
        assert_eq!(
            hour_in_flight_verdict(span, &stepped),
            InFlightVerdict::ClockStepped
        );
        // The same inputs with no step recorded read clear.
        assert_eq!(
            hour_in_flight_verdict(span, &clear_inputs(&applied, span)),
            InFlightVerdict::Clear
        );
        // An hour the window does not reach is not held for it.
        let far = hour_frame_seq_span("2026-11-07T08").expect("span");
        let far_inputs = InFlightInputs {
            clock_step: window,
            ..clear_inputs(&applied, far)
        };
        assert_eq!(
            hour_in_flight_verdict(far, &far_inputs),
            InFlightVerdict::Clear
        );
    }

    /// PR AA review finding 2. A 600 s step forward at 11:59:45 IST: frame
    /// sequences jump to about 12:09:45 while receipts stay in hour 11 until
    /// the anchor refresh, so hour 11 is held.
    #[test]
    fn test_in_flight_holds_an_hour_near_a_forward_clock_step() {
        let applied = AppliedSnapshot::default();
        let span = hour_frame_seq_span("2026-11-07T11").expect("span");
        // 2026-11-07 11:59:45 IST = 06:29:45 UTC.
        let step_at = 1_794_032_985 * SEC;
        let inputs = InFlightInputs {
            clock_step: crate::wal_clock_step::step_window(step_at, 600 * SEC),
            ..clear_inputs(&applied, span)
        };
        assert_eq!(
            hour_in_flight_verdict(span, &inputs),
            InFlightVerdict::ClockStepped
        );
    }

    #[test]
    fn test_in_flight_labels_are_stable_and_distinct() {
        let all = [
            InFlightVerdict::Clear,
            InFlightVerdict::DeferredDepth,
            InFlightVerdict::WalNotBound,
            InFlightVerdict::Unapplied,
            InFlightVerdict::RescueFloor,
            InFlightVerdict::NotAcked,
            InFlightVerdict::ClockAhead,
            InFlightVerdict::ClockStepped,
            InFlightVerdict::SpillPending,
        ];
        let mut labels: Vec<&str> = all.iter().map(|v| v.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
    }

    /// The process reader runs end to end on this process's live state.
    /// Other tests in this binary may bind or ack the global watermark, so
    /// only the shape is asserted: it answers, and an hour far in the future
    /// is never clear (no ack can be past it).
    #[test]
    fn test_process_in_flight_verdict_reads_the_process_state() {
        let verdict = process_in_flight_verdict(DEPTH, the_span());
        assert!(!verdict.as_str().is_empty());
        let far = hour_frame_seq_span("2099-01-01T10").expect("span");
        assert_ne!(
            process_in_flight_verdict(DEPTH, far),
            InFlightVerdict::Clear
        );
    }

    // ---- the protocol, through a scripted ArchiveIo ---------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        WalRead {
            writer: i64,
            sequencer: i64,
        },
        Hold,
        HoldTimedOut,
        InFlight,
        Pause,
        Copy {
            writer: i64,
            max_target_after_hold: i64,
        },
        Recount,
        Delete(String),
        Failure(ArchiveOutcome),
        Drop {
            writers_held_out: bool,
        },
    }

    /// A scripted archiver. `writer` advances by `catch_up` per pause until it
    /// reaches `sequencer`. Taking the hold commits `commit_on_hold` more
    /// transactions first (a writer that slipped in just before the lock).
    struct FakeIo {
        lock: &'static ArchiveQuiesce,
        writer: i64,
        sequencer: i64,
        catch_up: i64,
        commit_on_hold: i64,
        commit_after_copy: i64,
        hold_available: bool,
        in_flight: Vec<InFlightVerdict>,
        recount_rows: Option<u64>,
        uploaded_this_run: bool,
        probe_fails: bool,
        held: bool,
        max_target_after_hold: i64,
        events: Vec<Event>,
    }

    impl FakeIo {
        fn new() -> Self {
            Self {
                lock: Box::leak(Box::new(ArchiveQuiesce::new())),
                writer: 10,
                sequencer: 10,
                catch_up: 1,
                commit_on_hold: 0,
                commit_after_copy: 0,
                hold_available: true,
                in_flight: Vec::new(),
                recount_rows: Some(3),
                uploaded_this_run: true,
                probe_fails: false,
                held: false,
                max_target_after_hold: 0,
                events: Vec::new(),
            }
        }

        fn count(&self, wanted: fn(&Event) -> bool) -> usize {
            self.events.iter().filter(|e| wanted(e)).count()
        }
    }

    fn tally(rows: u64) -> RangeTally {
        RangeTally {
            rows,
            max_ts: Some("2026-11-07T10:59:59.000000Z".to_string()),
        }
    }

    impl ArchiveIo for FakeIo {
        type Hold = tokio::sync::RwLockWriteGuard<'static, ()>;

        async fn read_wal_rows(&mut self) -> Result<Vec<WalTableRow>> {
            if self.probe_fails {
                anyhow::bail!("probe down");
            }
            self.events.push(Event::WalRead {
                writer: self.writer,
                sequencer: self.sequencer,
            });
            if self.held {
                self.max_target_after_hold = self.max_target_after_hold.max(self.sequencer);
            }
            Ok(vec![row(
                DEPTH,
                false,
                Some(self.writer),
                Some(self.sequencer),
            )])
        }

        async fn hold_old_hour_writers(&mut self, wait: Duration) -> Option<Self::Hold> {
            if !self.hold_available {
                self.events.push(Event::HoldTimedOut);
                return None;
            }
            // A writer committed just before the lock was taken.
            self.sequencer += self.commit_on_hold;
            let guard = self.lock.hold_writers(wait).await;
            self.held = guard.is_some();
            self.events.push(Event::Hold);
            guard
        }

        fn in_flight_now(&mut self, _table: &str, _span: HourFrameSpan) -> InFlightVerdict {
            self.events.push(Event::InFlight);
            if self.in_flight.is_empty() {
                InFlightVerdict::Clear
            } else {
                self.in_flight.remove(0)
            }
        }

        async fn pause_for(&mut self, _duration: Duration) {
            self.events.push(Event::Pause);
            self.writer = (self.writer + self.catch_up).min(self.sequencer);
        }

        async fn copy_and_verify(
            &mut self,
            table: &str,
            partition: &str,
            _start: &str,
            _end: &str,
            _summary: &mut ArchiveRunSummary,
        ) -> Option<CopiedPartition> {
            self.events.push(Event::Copy {
                writer: self.writer,
                max_target_after_hold: self.max_target_after_hold,
            });
            // The live writer keeps committing into the current hour.
            self.sequencer += self.commit_after_copy;
            let proof =
                super::super::verify_archive(table, partition, 3, 30, 20, "ab", Some(3), Some(20))
                    .expect("proof");
            Some(CopiedPartition {
                proof,
                s3_key: "cold/key".to_string(),
                uploaded_this_run: self.uploaded_this_run,
                verified_tally: Some(tally(3)),
            })
        }

        async fn recount_range(&mut self, _t: &str, _s: &str, _e: &str) -> Option<RangeTally> {
            self.events.push(Event::Recount);
            self.recount_rows.map(tally)
        }

        async fn delete_own_upload(&mut self, key: &str) -> Result<()> {
            self.events.push(Event::Delete(key.to_string()));
            Ok(())
        }

        fn note_failure(
            &mut self,
            _table: &str,
            _partition: &str,
            outcome: ArchiveOutcome,
            _detail: &str,
            summary: &mut ArchiveRunSummary,
        ) {
            summary.failed += 1;
            self.events.push(Event::Failure(outcome));
        }

        async fn drop_verified(
            &mut self,
            _copied: CopiedPartition,
            summary: &mut ArchiveRunSummary,
        ) {
            summary.dropped += 1;
            let writers_held_out = self.lock.try_shared().is_none();
            self.events.push(Event::Drop { writers_held_out });
        }
    }

    fn gate(polls: u32) -> SnapshotGate {
        SnapshotGate {
            quiesce_wait: Duration::from_millis(50),
            snapshot_polls: polls,
        }
    }

    async fn run(io: &mut FakeIo, polls: u32) -> (PartitionStep, ArchiveRunSummary) {
        let mut summary = ArchiveRunSummary::default();
        let step =
            archive_with_snapshot_gate(io, DEPTH, PARTITION, gate(polls), &mut summary).await;
        (step, summary)
    }

    #[tokio::test]
    async fn test_happy_path_drops_once_with_the_writers_held_out() {
        let mut io = FakeIo::new();
        io.writer = 7; // a little behind at the start
        io.commit_after_copy = 4; // the live writer keeps committing
        let (step, summary) = run(&mut io, 10).await;
        assert_eq!(step, PartitionStep::Done);
        assert_eq!(summary.dropped, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            io.events.last(),
            Some(&Event::Drop {
                writers_held_out: true
            }),
            "the drop runs while the quiesce lock is held: {:?}",
            io.events
        );
        // After the drop the lock is released.
        assert!(io.lock.try_shared().is_some());
        // The order: hold, in-flight, snapshot, copy, snapshot, in-flight,
        // recount, drop.
        let hold = io.events.iter().position(|e| *e == Event::Hold);
        let copy = io
            .events
            .iter()
            .position(|e| matches!(e, Event::Copy { .. }));
        let recount = io.events.iter().position(|e| *e == Event::Recount);
        assert!(hold < copy && copy < recount, "{:?}", io.events);
        assert_eq!(io.count(|e| *e == Event::InFlight), 2);
    }

    /// The snapshot target must be read AFTER the lock: a writer that
    /// committed between the pre-wait and the lock (sequencer 10 → 12) has
    /// rows of the old hour in transactions 11 and 12, so the export must
    /// wait until the writer reaches 12. A gate that reused the pre-wait's
    /// target (10) would export with the writer at 10.
    #[tokio::test]
    async fn test_s_read_before_lock_is_rejected() {
        let mut io = FakeIo::new();
        io.commit_on_hold = 2;
        let (step, summary) = run(&mut io, 10).await;
        assert_eq!(step, PartitionStep::Done);
        assert_eq!(summary.dropped, 1);
        let Some(Event::Copy {
            writer,
            max_target_after_hold,
        }) = io
            .events
            .iter()
            .find(|e| matches!(e, Event::Copy { .. }))
            .cloned()
        else {
            panic!("no copy: {:?}", io.events);
        };
        assert_eq!(
            max_target_after_hold, 12,
            "a WAL read happened under the lock"
        );
        assert!(
            writer >= 12,
            "the export waited for the snapshot taken under the lock (writer {writer}): {:?}",
            io.events
        );
        // At least one WAL read happened after the hold and before the copy.
        let hold = io
            .events
            .iter()
            .position(|e| *e == Event::Hold)
            .unwrap_or(usize::MAX);
        let copy = io
            .events
            .iter()
            .position(|e| matches!(e, Event::Copy { .. }))
            .unwrap_or(0);
        assert!(
            io.events[hold..copy]
                .iter()
                .any(|e| matches!(e, Event::WalRead { .. })),
            "{:?}",
            io.events
        );
    }

    #[tokio::test]
    async fn test_quiesce_timeout_keeps_the_partition_and_exports_nothing() {
        let mut io = FakeIo::new();
        io.hold_available = false;
        let (step, summary) = run(&mut io, 10).await;
        assert_eq!(step, PartitionStep::QuiesceBusy);
        assert_eq!(summary.held_quiesce_busy, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(io.count(|e| matches!(e, Event::Copy { .. })), 0);
        assert!(!step.exported());
        assert!(!step.wal_not_applied());
    }

    /// A real lock held by a writer: the archiver's bounded wait times out.
    #[tokio::test]
    async fn test_quiesce_held_by_a_writer_times_out_the_archiver() {
        let mut io = FakeIo::new();
        let writer = io.lock.try_shared();
        assert!(writer.is_some());
        let (step, summary) = run(&mut io, 10).await;
        assert_eq!(step, PartitionStep::QuiesceBusy);
        assert_eq!(summary.held_quiesce_busy, 1);
        assert_eq!(io.count(|e| matches!(e, Event::Copy { .. })), 0);
        drop(writer);
    }

    #[tokio::test]
    async fn test_in_flight_hour_is_held_before_export() {
        for (verdict, deferred) in [
            (InFlightVerdict::Unapplied, false),
            (InFlightVerdict::RescueFloor, false),
            (InFlightVerdict::NotAcked, false),
            (InFlightVerdict::ClockAhead, false),
            (InFlightVerdict::WalNotBound, false),
            (InFlightVerdict::DeferredDepth, true),
        ] {
            let mut io = FakeIo::new();
            io.in_flight = vec![verdict];
            let (step, summary) = run(&mut io, 10).await;
            assert_eq!(step, PartitionStep::Held, "{verdict:?}");
            assert_eq!(io.count(|e| matches!(e, Event::Copy { .. })), 0);
            assert_eq!(summary.failed, 0);
            if deferred {
                assert_eq!(summary.held_deferred_depth, 1);
                assert_eq!(summary.held_in_flight, 0);
            } else {
                assert_eq!(summary.held_in_flight, 1, "{verdict:?}");
                assert_eq!(summary.held_deferred_depth, 0);
            }
            assert!(!step.exported() && !step.wal_not_applied());
        }
    }

    #[tokio::test]
    async fn test_snapshot_timeout_keeps_and_blocks_the_table_with_bounded_polls() {
        let mut io = FakeIo::new();
        io.writer = 0;
        io.sequencer = 100;
        io.catch_up = 1;
        let (step, summary) = run(&mut io, 5).await;
        assert_eq!(step, PartitionStep::WalNotAppliedBeforeExport);
        assert!(step.wal_not_applied());
        assert!(!step.exported());
        assert_eq!(io.count(|e| *e == Event::Pause), 5, "bounded by the polls");
        assert_eq!(io.count(|e| *e == Event::Hold), 0, "no lock taken");
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn test_suspended_or_unreadable_wal_keeps_the_partition() {
        let mut io = FakeIo::new();
        io.probe_fails = true;
        let (step, _) = run(&mut io, 5).await;
        assert_eq!(step, PartitionStep::WalNotAppliedBeforeExport);
        assert_eq!(io.count(|e| matches!(e, Event::Copy { .. })), 0);
    }

    #[tokio::test]
    async fn test_pre_drop_snapshot_behind_keeps_partition_and_object() {
        let mut io = FakeIo::new();
        io.commit_after_copy = 50;
        io.catch_up = 1;
        let (step, summary) = run(&mut io, 3).await;
        assert_eq!(step, PartitionStep::WalNotAppliedBeforeDrop);
        assert!(step.exported());
        assert_eq!(summary.dropped, 0);
        assert_eq!(io.count(|e| matches!(e, Event::Delete(_))), 0);
        assert_eq!(io.count(|e| matches!(e, Event::Drop { .. })), 0);
    }

    #[tokio::test]
    async fn test_pre_drop_in_flight_keeps_partition() {
        let mut io = FakeIo::new();
        io.in_flight = vec![InFlightVerdict::Clear, InFlightVerdict::Unapplied];
        let (step, summary) = run(&mut io, 3).await;
        assert_eq!(step, PartitionStep::Done);
        assert_eq!(summary.dropped, 0);
        assert_eq!(summary.held_in_flight, 1);
        assert_eq!(io.count(|e| matches!(e, Event::Drop { .. })), 0);
        assert_eq!(io.count(|e| *e == Event::Recount), 0);
    }

    #[tokio::test]
    async fn test_pre_drop_recount_mismatch_keeps_and_deletes_own_upload() {
        // This run uploaded the object: a changed partition removes it.
        let mut io = FakeIo::new();
        io.recount_rows = Some(4);
        let (step, summary) = run(&mut io, 3).await;
        assert_eq!(step, PartitionStep::Done);
        assert_eq!(summary.dropped, 0);
        assert_eq!(summary.failed, 1);
        assert!(io.events.contains(&Event::Delete("cold/key".to_string())));
        assert!(
            io.events
                .contains(&Event::Failure(ArchiveOutcome::CountMismatch))
        );
        assert_eq!(io.count(|e| matches!(e, Event::Drop { .. })), 0);

        // A REUSED object (an earlier run wrote it) is never deleted.
        let mut io = FakeIo::new();
        io.recount_rows = Some(4);
        io.uploaded_this_run = false;
        let (_, summary) = run(&mut io, 3).await;
        assert_eq!(summary.dropped, 0);
        assert_eq!(io.count(|e| matches!(e, Event::Delete(_))), 0);

        // An unavailable recount keeps the partition too.
        let mut io = FakeIo::new();
        io.recount_rows = None;
        let (_, summary) = run(&mut io, 3).await;
        assert_eq!(summary.dropped, 0);
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn test_unparseable_partition_is_a_recorded_failure() {
        let mut io = FakeIo::new();
        let mut summary = ArchiveRunSummary::default();
        let step =
            archive_with_snapshot_gate(&mut io, DEPTH, "garbage", gate(3), &mut summary).await;
        assert_eq!(step, PartitionStep::Done);
        assert_eq!(summary.failed, 1);
        assert_eq!(
            io.events,
            vec![Event::Failure(ArchiveOutcome::ExportFailed)]
        );
    }

    /// A configured snapshot wait of 0 still gives the pre-wait one poll.
    #[tokio::test]
    async fn test_zero_wait_pre_wait_still_polls_once() {
        let mut io = FakeIo::new();
        io.writer = 9;
        io.sequencer = 10;
        let (step, summary) = run(&mut io, 0).await;
        assert_eq!(step, PartitionStep::Done);
        assert_eq!(summary.dropped, 1);
        assert_eq!(io.count(|e| *e == Event::Pause), 1);
    }
}
