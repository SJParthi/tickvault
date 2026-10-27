//! The after-close deferred-depth pass (plan item 45a, 2026-09-29).
//!
//! # What it does
//!
//! When the database falls behind or the disk runs short, the ingest shed
//! stops writing order-book rows. The frame itself is already in the
//! capture-at-receipt WAL, and `wal_deferred_depth` records which ~4.6-minute
//! bucket it came from so the prunes keep that segment. This pass writes those
//! rows back: after the close, it re-reads ONLY the marked buckets' frames and
//! writes ONLY the depth the shed skipped — the five inline levels of a Full
//! packet for an inline mark, the dedicated depth-20/200 frame for a dedicated
//! mark. Ticks and candles are never touched: they were written live.
//!
//! Operator, 2026-09-29: "neevr ever anythign shodul be fucking missed dude
//! okay?" MEASURED the same morning: 5,792,427 inline-depth packets shed by
//! 10:08 IST, each one lost for good two to three days later before this pass
//! and the prune protection existed.
//!
//! # Why it runs where it runs
//!
//! * **After the close, not at the next boot.** The morning boot replay is what
//!   flooded QuestDB on 2026-09-29. It runs from 15:45 IST, the quietest time
//!   the box has, and since 2026-10-21 (operator Quote 31) it keeps running
//!   through the night, up to 07:15 IST, until every mark is written back;
//!   the drain hold (`crate::drain_hold`) keeps the box up until then.
//! * **On its own thread with its own depth writer.** The live drain's writer
//!   mixes rows into shared ILP batches; a failed batch marks its whole
//!   sequence range unapplied, and an hours-old row beside a live one would
//!   span dozens of buckets. A separate synchronous writer keeps every batch to
//!   one bucket and gives this pass a direct acknowledgement.
//! * **Paced.** It pauses while any QuestDB table's WAL apply lag is growing
//!   and while the applied watermark distrusts the sink, and (Quote 31) while
//!   QuestDB's apply backlog is above [`AFTER_CLOSE_APPLY_LAG_CEILING`]. It
//!   stops starting new work at 07:15 IST the next morning,
//!   and it stops early when free disk on the WAL volume falls below
//!   [`AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION`].
//!
//! # Why the ingest shed does not pause it (fixed 2026-10-13)
//!
//! Until 2026-10-13 the pass also paused while the ingest shed refused the
//! kind it would write. MEASURED on 2026-10-12: the disk shed turned on at
//! 09:52 IST and never turned off (inline depth restores only above 28% free),
//! 51.4M inline-depth packets were shed, and the pass waited from 15:55 to
//! 17:15 and wrote nothing (`rows=0`, `cleared=0`, `stop=Deadline`). The shed
//! frames stay pinned in the WAL until this pass writes them back, so the disk
//! the shed was waiting for could only be freed by the pass that waited for
//! it, and below 5% free the byte prune deletes the pinned segments as a loss.
//!
//! After the close the shed gates nothing live (the pass starts after the
//! capture window ends, asserted below). Its two inputs are covered directly:
//! the apply-lag half is the same `wal_apply_lag_growing` signal this pass
//! already pauses on, and the disk half is replaced by a measured floor that
//! STOPS the pass (finished buckets are still settled and released) well
//! above the 5% point where pinned segments start being deleted.
//!
//! # When a mark is cleared
//!
//! Only after every row of the bucket was flushed without error AND a settle
//! period (one WAL-suspension poll) shows QuestDB applying, not lagging: a
//! lagging or suspended table acknowledges rows it has not applied. Anything
//! short of that keeps the mark for the next day's pass; a rewrite is
//! idempotent because `market_depth`'s DEDUP key carries `capture_seq`, and the
//! packet walk here numbers packets exactly as the live drain does.
//!
//! # Which table (ITEM 57a, 2026-10-13)
//!
//! With array rows on, the pass writes ONLY `market_depth_book`, every frame,
//! whatever its receipt time: its ingest is book-only
//! ([`DepthIngest::new_book_only`]) and opens no `market_depth` writer. Until
//! 2026-10-13 a frame received before the array-row switch went to
//! `market_depth`, and measured on 13 Oct those rewrites added about 9,842
//! transactions to that table's WAL backlog between 15:53 and 19:00, the
//! backlog whose apply holds the disk busy the next session. `market_depth_book`
//! carries `capture_seq` in its DEDUP key too, so a rewrite stays idempotent.
//! With array rows off (no start instant) the pass writes `market_depth` as
//! before.
//!
//! # Honest limits
//!
//! * A bucket from before the array-row switch is rewritten in full into
//!   `market_depth_book`; packets the live drain had already written to
//!   `market_depth` in that bucket then exist in both tables, and the packets
//!   that were shed exist only in `market_depth_book`, so a query of
//!   `market_depth` alone for that bucket still misses them. No loss; those
//!   rows are counted (rows SENT, so a bucket carried after a failed write is
//!   counted again) on
//!   `tv_wal_deferred_depth_rows_total{outcome="pre_switch_to_book"}`.
//!
//! * A bucket still overlapping the segment the writer holds open is carried.
//!   Segments rotate by size, so the session's last bucket usually is; since
//!   2026-10-21 each round first asks the writer to seal that segment in a
//!   lull (`wal_idle_seal::request_idle_seal`), so it is carried one round,
//!   not one day. A frame arriving in every 200 ms poll would keep it open.
//! * A frame recorded without a receipt time (a pre-TVW3 record) cannot be
//!   rewritten onto the row the live drain wrote, so it is counted and left.
//! * A segment damaged mid-file loses what follows the damage; that is counted
//!   and coded, and the mark is cleared because nothing more can be read.
//! * If the rescue of a failed batch ALSO fails, the depth writer marks that
//!   batch's range unapplied as it always does, and the next boot re-folds one
//!   bucket in full. Bounded to one bucket by flushing at every bucket edge.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tickvault_common::error_code::ErrorCode;
use tickvault_core::parser::ParsedFrame;
use tickvault_core::parser::depth::DepthFeedKind;
use tickvault_core::parser::dispatcher::{dispatch_frame, main_feed_packet_len};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::wal_applied_watermark::reorder_reach_lo;
use tickvault_storage::wal_deferred_depth::{
    DEFERRED_BUCKET_SHIFT, DeferredDepth, SHED_DEPTH_ALL, ShedDepth,
};
use tickvault_storage::ws_frame_spill::{
    ReplayedFrame, WAL_RECEIPT_UNKNOWN_NANOS, WalEndpoint, deferred_segment_spans,
    read_segment_frames_matching, segments_for_bucket,
};
use tracing::{error, info, warn};

use super::{
    DepthIngest, DrainCounters, append_inline_depth, drain_depth_frame, unknown_packet_skip,
};

/// 15:45 IST — the pass may start (five minutes after the 15:40 capture end).
pub const AFTER_CLOSE_DEPTH_START_SECS_IST: u64 = 15 * 3_600 + 45 * 60;
/// 07:15 IST the NEXT morning — the pass stops starting new work. The window
/// wraps midnight: `[15:45, 24:00) ∪ [00:00, 07:15)`. Operator Quote 31
/// (2026-10-21): the day's write-back finishes the same day, so the box stays
/// up until it is done; until then this stopped at 17:15 for a 17:30 stop and
/// a multi-day backlog arrived in one evening (3,067,442,594 rows on 20 Oct).
pub const AFTER_CLOSE_DEPTH_STOP_SECS_IST: u64 = 7 * 3_600 + 15 * 60;
/// 07:30 IST — the latest a drain hold can keep the box up (the 07:35 curfew
/// check stops it, and the 08:30 start boots it fresh). Stated here only to
/// prove the window ends in time; the stop itself is the Lambdas'.
const BOX_HOLD_CEILING_SECS_IST: u64 = 7 * 3_600 + 30 * 60;
/// Sleep while paused.
pub const AFTER_CLOSE_PAUSE_SECS: u64 = 10;
/// [`AFTER_CLOSE_PAUSE_SECS`] as a `Duration`.
pub const AFTER_CLOSE_PAUSE: Duration = Duration::from_secs(AFTER_CLOSE_PAUSE_SECS);
/// Wait after the last flush before trusting the acks: one WAL-suspension
/// poll (60 s, `wal_suspension_watcher::WAL_SUSPENSION_POLL_INTERVAL_SECS`)
/// plus five seconds.
pub const AFTER_CLOSE_SETTLE_SECS: u64 = 65;
/// [`AFTER_CLOSE_SETTLE_SECS`] as a `Duration`.
pub const AFTER_CLOSE_SETTLE: Duration = Duration::from_secs(AFTER_CLOSE_SETTLE_SECS);
/// The pass pauses while QuestDB's largest WAL apply lag (transactions) is
/// above this, or unknown. MEASURED 2026-10-21: a live session reads 12–350
/// once caught up; on 20 Oct the pass, pausing only on a GROWING lag, drove
/// it from 0 to 6,758 and the box stopped with ~6,000 unapplied, which the
/// next morning's boot then applied on top of the open (Quote 31). The value
/// is chosen above the live range so a quiet evening never pauses; it is not
/// a measured optimum.
pub const AFTER_CLOSE_APPLY_LAG_CEILING: u64 = 500;

/// Whether `secs_ist` is inside the wrapping after-close window. O(1).
#[must_use]
pub const fn in_after_close_window(secs_ist: u64) -> bool {
    secs_ist >= AFTER_CLOSE_DEPTH_START_SECS_IST || secs_ist < AFTER_CLOSE_DEPTH_STOP_SECS_IST
}

// The settle must fit between the stop and the hold ceiling.
const _: () = assert!(
    AFTER_CLOSE_DEPTH_STOP_SECS_IST + AFTER_CLOSE_SETTLE.as_secs() < BOX_HOLD_CEILING_SECS_IST
);
const _: () = assert!(AFTER_CLOSE_DEPTH_STOP_SECS_IST < AFTER_CLOSE_DEPTH_START_SECS_IST);
// The pass never runs while live capture writes depth, which is why the
// ingest shed no longer pauses it: after the capture end the shed gates
// nothing live. Moving either constant so the two overlap fails the build.
const _: () = assert!(
    AFTER_CLOSE_DEPTH_START_SECS_IST
        >= tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST as u64
);
const _: () = assert!(
    BOX_HOLD_CEILING_SECS_IST
        <= tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST as u64
);

/// Free-space fraction of the WAL volume below which the pass stops writing.
///
/// Equal to the ingest shed's all-depth threshold
/// (`ingest_shed::SHED_ALL_DEPTH_BELOW_FREE`, 8%): the pass stops where even
/// live dedicated depth would be shed. It sits three points (about 19 GB on
/// the 644 GB volume) above `ws_frame_spill::WAL_PINNED_DISK_FLOOR_FREE_FRACTION`
/// (5%), where the byte prune starts deleting pinned segments as a counted
/// loss, so the pass's own writes (checked before every bucket and before
/// every late flush, so at most one 16 MiB batch plus the partition rewrite
/// its commit causes between checks) cannot carry the volume there. It is
/// below the inline shed threshold (15%), so a disk-only inline shed does not
/// stop the pass. Both orderings are pinned by a test.
pub const AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION: f64 =
    tickvault_common::ingest_shed::SHED_ALL_DEPTH_BELOW_FREE;

/// The widest bucket range one segment may cover before the overflow sweep
/// stops trusting it (a segment normally spans one or two buckets).
const OVERFLOW_SPAN_MAX_BUCKETS: u64 = 8;

/// How long a bucket next to an unreadable segment is carried before it is
/// released as lost.
const UNCERTAIN_CARRY_MAX_DAYS: u64 = 3;

/// Age in whole days of bucket `id`, from the capture sequence it encodes
/// (nanoseconds since the epoch with low bits reserved). A bucket from the
/// future reads as age 0.
fn bucket_age_days(id: u64) -> u64 {
    let start_nanos = id.checked_shl(DEFERRED_BUCKET_SHIFT).unwrap_or(u64::MAX);
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    now_nanos.saturating_sub(start_nanos) / (86_400 * 1_000_000_000)
}

/// Counter: rows this pass wrote back (`written`), rows it refused
/// (`refused`), and, of the written ones, rows from frames received before
/// the array-row switch that went to `market_depth_book` (`pre_switch_to_book`,
/// ITEM 57a).
pub const DEFERRED_DEPTH_ROWS_COUNTER: &str = "tv_wal_deferred_depth_rows_total";
/// Counter: per-bucket outcomes of the pass.
pub const DEFERRED_DEPTH_PASS_COUNTER: &str = "tv_wal_deferred_depth_pass_total";

/// Why the pass stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassStop {
    /// Outside the window, in the afternoon (before 15:45 IST) — nothing is
    /// started.
    BeforeWindow,
    /// Outside the window, in the morning (07:15 IST reached).
    Deadline,
    /// The lane is shutting down.
    Cancelled,
    /// Free disk on the WAL volume fell below
    /// [`AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION`], or could not be measured.
    DiskFloor,
}

/// What the pass should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassStep {
    Run,
    Pause,
    Stop(PassStop),
}

/// The pass's pacing rule. Pure and O(1).
///
/// Cancellation and the window win over everything. Inside the window a disk
/// below the floor (`disk_low`) STOPS the pass: nothing the pass does frees
/// disk before it finishes, so waiting would only run out the window, while
/// stopping lets the buckets already written be settled and released. The
/// pass pauses while QuestDB is falling behind (`lag_growing_tables > 0`),
/// while its apply backlog is above [`AFTER_CLOSE_APPLY_LAG_CEILING`] or
/// unknown (`apply_lag`, Quote 31), or while the applied watermark distrusts
/// the sink.
///
/// The ingest shed is deliberately NOT an input (2026-10-13, see the module
/// header): after the close it gates nothing live, its apply-lag half is
/// `lag_growing_tables`, and its disk half is `disk_low`. Pausing on it
/// deadlocked the pass against the disk only the pass can free.
#[must_use]
pub const fn after_close_step(
    secs_ist: u64,
    lag_growing_tables: u32,
    apply_lag: Option<u64>,
    sink_suspect: bool,
    disk_low: bool,
    cancelled: bool,
) -> PassStep {
    if cancelled {
        return PassStep::Stop(PassStop::Cancelled);
    }
    if !in_after_close_window(secs_ist) {
        // Out of the window: the morning half means the pass ran out of
        // night, the afternoon half that it was started too early.
        return if secs_ist < 12 * 3_600 {
            PassStep::Stop(PassStop::Deadline)
        } else {
            PassStep::Stop(PassStop::BeforeWindow)
        };
    }
    if disk_low {
        return PassStep::Stop(PassStop::DiskFloor);
    }
    let lag_high = match apply_lag {
        Some(lag) => lag > AFTER_CLOSE_APPLY_LAG_CEILING,
        None => true,
    };
    if lag_growing_tables > 0 || lag_high || sink_suspect {
        return PassStep::Pause;
    }
    PassStep::Run
}

/// Whether a measured free fraction is below the pass's disk floor. An
/// unmeasured disk (`None`, a failed `statvfs`) reads as below it: adding
/// rows to a volume nobody can measure is the direction that loses data,
/// and stopping costs only a carry to the next evening. O(1).
fn disk_below_pass_floor(free_fraction: Option<f64>) -> bool {
    free_fraction.is_none_or(|f| !f.is_finite() || f < AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION)
}

/// Seconds until the next window opens, from `secs_ist`. `0` inside the
/// window. O(1).
#[must_use]
pub const fn secs_until_window(secs_ist: u64) -> u64 {
    if in_after_close_window(secs_ist) {
        0
    } else {
        AFTER_CLOSE_DEPTH_START_SECS_IST - secs_ist
    }
}

/// The live signals the pass paces on, injectable for tests.
pub struct PassSignals<'a> {
    pub now_secs_ist: &'a dyn Fn() -> u64,
    pub lag_growing_tables: &'a dyn Fn() -> u32,
    /// QuestDB's largest WAL apply lag, `None` when unknown (Quote 31).
    pub apply_lag: &'a dyn Fn() -> Option<u64>,
    pub sink_suspect: &'a dyn Fn() -> bool,
    /// Free-space fraction (0.0..=1.0) of the volume holding the WAL
    /// directory passed in; `None` when it cannot be measured.
    pub disk_free_fraction: &'a dyn Fn(&Path) -> Option<f64>,
    pub sleep: &'a dyn Fn(Duration),
}

/// Free-space fraction of the volume holding `path`: one `statvfs` call,
/// `None` when the probe fails or reports an empty volume. Cold path.
fn probe_free_fraction(path: &Path) -> Option<f64> {
    match tickvault_storage::disk_health_watcher::probe_disk_free_bytes(path) {
        tickvault_storage::disk_health_watcher::DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } if total_bytes > 0 => {
            #[allow(clippy::cast_precision_loss)] // APPROVED: ratio of byte counts
            let free = free_bytes as f64 / total_bytes as f64;
            Some(free)
        }
        _ => None,
    }
}

/// The production signals.
#[must_use]
pub fn live_signals() -> PassSignals<'static> {
    PassSignals {
        now_secs_ist: &|| super::now_ist_secs_of_day(),
        lag_growing_tables: &|| tickvault_common::ingest_shed::wal_apply_lag_growing(),
        apply_lag: &|| tickvault_common::ingest_shed::wal_apply_lag_max(),
        sink_suspect: &|| {
            tickvault_storage::wal_applied_watermark::applied_watermark().is_sink_suspect()
        },
        disk_free_fraction: &probe_free_fraction,
        sleep: &|d| std::thread::sleep(d),
    }
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassSummary {
    pub buckets_cleared: u64,
    pub buckets_carried: u64,
    pub rows_written: u64,
    pub frames_no_receipt: u64,
    pub segments_damaged: u64,
    pub buckets_missing: u64,
    /// Of `buckets_carried`, those carried ONLY because a neighbouring
    /// segment is unreadable: a rerun the same night cannot finish them
    /// (they wait for a restart to repair the file, or for
    /// `UNCERTAIN_CARRY_MAX_DAYS`), so they do not hold the box (Quote 31).
    pub buckets_uncertain: u64,
    pub stop: Option<PassStop>,
}

impl PassSummary {
    /// Buckets this pass left that another pass tonight could still finish.
    /// A pass that stopped early (disk floor, deadline, cancel) did not visit
    /// the rest of its work, so it reports at least one. O(1).
    #[must_use]
    pub const fn retryable_left(&self) -> u64 {
        let left = self.buckets_carried.saturating_sub(self.buckets_uncertain);
        if self.stop.is_some() && left == 0 {
            1
        } else {
            left
        }
    }
}

/// The last finished pass's [`PassSummary::retryable_left`], or
/// [`PASS_OUTCOME_UNKNOWN`] until a pass has finished in this process.
/// Read by the drain hold (`crate::drain_hold`).
static LAST_PASS_RETRYABLE_LEFT: AtomicU64 = AtomicU64::new(PASS_OUTCOME_UNKNOWN);
/// "No pass has finished yet."
pub const PASS_OUTCOME_UNKNOWN: u64 = u64::MAX;

/// Rows every pass in this process has written back, for the drain hold's
/// progress check: a long pass clears its marks only at its end, so rows are
/// the evidence it is moving. One relaxed add per frame on the pass's own
/// thread, never on the frame drain.
static PASS_ROWS_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Total rows written back by the pass in this process. O(1).
#[must_use]
pub fn pass_rows_written_total() -> u64 {
    PASS_ROWS_WRITTEN.load(Ordering::Relaxed)
}

/// Records a finished pass for the drain hold. O(1).
pub fn publish_pass_outcome(summary: &PassSummary) {
    LAST_PASS_RETRYABLE_LEFT.store(summary.retryable_left(), Ordering::Release);
}

/// Buckets the last finished pass left that a rerun could still finish;
/// `None` until a pass has finished in this process. O(1).
#[must_use]
pub fn last_pass_retryable_left() -> Option<u64> {
    let v = LAST_PASS_RETRYABLE_LEFT.load(Ordering::Acquire);
    (v != PASS_OUTCOME_UNKNOWN).then_some(v)
}

/// The counter set the pass hands to the shared drain helpers. Every drain
/// counter is a no-op except the two depth-row outcomes, which land on this
/// pass's own series — so a rewrite never double-counts the live
/// `tv_dhan_feed_depth_total`.
pub(crate) fn pass_counters() -> &'static DrainCounters {
    static COUNTERS: std::sync::OnceLock<DrainCounters> = std::sync::OnceLock::new();
    COUNTERS.get_or_init(|| {
        let noop = metrics::Counter::noop;
        DrainCounters {
            folded: noop(),
            non_tick: noop(),
            main_feed_disconnects: noop(),
            unparseable: noop(),
            seq_unrepresentable: noop(),
            aggregator_refused: noop(),
            write_failed: noop(),
            ingest_ticks: noop(),
            ingest_seq_refused: noop(),
            tick_unknown_segment: noop(),
            verdict_ltt_backward: noop(),
            verdict_volume_jump: noop(),
            refused_price: noop(),
            refused_timestamp: noop(),
            refused_slot: noop(),
            refused_session: noop(),
            seals_emitted: noop(),
            seals_dropped: noop(),
            seals_rescued: noop(),
            flush_ok: noop(),
            flush_failed: noop(),
            depth_flush_failed: noop(),
            depth_unconsumed: noop(),
            shed_inline_depth: noop(),
            shed_dedicated_depth: noop(),
            depth_rows: metrics::counter!(DEFERRED_DEPTH_ROWS_COUNTER, "outcome" => "written"),
            depth_refused: metrics::counter!(DEFERRED_DEPTH_ROWS_COUNTER, "outcome" => "refused"),
            depth_dropped: noop(),
            depth_disconnects: noop(),
            depth_length_mismatch: noop(),
            depth_ghost: noop(),
            depth_unsubscribed_grace: noop(),
            depth_ghost_unsubscribes: noop(),
            depth_ghost_exhausted: noop(),
            truncated: noop(),
            main_feed_length_mismatch: noop(),
            abandoned_bytes: noop(),
            unknown_skipped: noop(),
            frames_wal_unbacked: noop(),
            depth_unbacked_not_shed: noop(),
        }
    })
}

/// Writes back ONLY the inline depth of one main-feed frame. The packet walk
/// is the live drain's, step for step — including the unknown-packet skip and
/// the per-frame packet cap — because the packet index is part of
/// `capture_seq`, and `capture_seq` is part of the `market_depth` DEDUP key: a
/// walk that numbered packets differently would write DUPLICATE rows instead
/// of landing on the ones the live drain wrote. Returns rows appended.
pub(crate) fn rewrite_inline_depth(
    sink: &mut DepthIngest,
    bytes: &[u8],
    frame_seq: u64,
    received_at_nanos: i64,
    c: &DrainCounters,
) -> u64 {
    let mut rows = 0u64;
    let mut offset = 0usize;
    let mut packets = 0u32;
    while offset < bytes.len() {
        let Some(len) = bytes.get(offset..).and_then(main_feed_packet_len) else {
            match unknown_packet_skip(bytes, offset) {
                Some(next) => {
                    offset = next;
                    packets = packets.saturating_add(1);
                    if packets >= tickvault_common::constants::MAX_PACKETS_PER_FRAME {
                        return rows;
                    }
                    continue;
                }
                None => return rows,
            }
        };
        let end = offset.saturating_add(len);
        let Some(packet) = bytes.get(offset..end) else {
            return rows;
        };
        if let Ok(ParsedFrame::TickWithDepth(tick, levels)) =
            dispatch_frame(packet, received_at_nanos)
        {
            rows = rows.saturating_add(append_inline_depth(
                sink,
                &tick,
                &levels,
                received_at_nanos,
                frame_seq,
                packets,
                false,
                c,
            ));
        }
        offset = end;
        packets = packets.saturating_add(1);
        if packets >= tickvault_common::constants::MAX_PACKETS_PER_FRAME {
            return rows;
        }
    }
    rows
}

/// Writes back one dedicated depth frame, exactly as the boot refold does.
fn rewrite_dedicated_depth(
    sink: &mut DepthIngest,
    frame: &ReplayedFrame,
    c: &DrainCounters,
) -> u64 {
    let (kind, endpoint) = match frame.endpoint {
        WalEndpoint::Depth20 => (DepthFeedKind::Twenty, DhanEndpointType::Depth20),
        WalEndpoint::Depth200 => (DepthFeedKind::TwoHundred, DhanEndpointType::Depth200),
        _ => return 0,
    };
    let captured = CapturedFrame {
        seq: frame.frame_seq,
        endpoint,
        // Outside every real connection index: skips the first-packet tracker
        // and ghost classification, as the boot refold's sentinel does.
        connection_index: u8::MAX,
        // A dwell anchor only; the receipt used is the WAL's.
        received_at: std::time::Instant::now(),
        received_at_nanos: frame.received_at_nanos,
        // Re-read out of the WAL, so WAL-backed by definition.
        wal_backed: true,
        // APPROVED: one copy of a frame on the cold after-close path.
        bytes: bytes::Bytes::copy_from_slice(&frame.frame),
    };
    drain_depth_frame(sink, &captured, frame.received_at_nanos, kind, c).rows
}

/// Whether a frame belongs to a bucket's shed kinds.
const fn endpoint_matches(endpoint: WalEndpoint, kinds: u8) -> bool {
    match endpoint {
        WalEndpoint::MainFeed => kinds & ShedDepth::Inline as u8 != 0,
        WalEndpoint::Depth20 | WalEndpoint::Depth200 => kinds & ShedDepth::Dedicated as u8 != 0,
        _ => false,
    }
}

/// Waits until the pacing rule says run. `Err` carries the stop reason.
fn wait_for_run(
    signals: &PassSignals<'_>,
    wal_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(), PassStop> {
    loop {
        match after_close_step(
            (signals.now_secs_ist)(),
            (signals.lag_growing_tables)(),
            (signals.apply_lag)(),
            (signals.sink_suspect)(),
            disk_below_pass_floor((signals.disk_free_fraction)(wal_dir)),
            cancel.load(Ordering::Acquire),
        ) {
            PassStep::Run => return Ok(()),
            PassStep::Pause => (signals.sleep)(AFTER_CLOSE_PAUSE),
            PassStep::Stop(reason) => return Err(reason),
        }
    }
}

/// Counts and logs the pass stopping at the disk floor. Once per pass (the
/// pass ends right after). Log-only `warn!`: nothing is lost here, the
/// unfinished buckets stay marked and their segments stay pinned.
fn report_disk_floor_stop(signals: &PassSignals<'_>, wal_dir: &Path, bucket: u64) {
    metrics::counter!(DEFERRED_DEPTH_PASS_COUNTER, "outcome" => "disk_floor").increment(1);
    let free = (signals.disk_free_fraction)(wal_dir);
    warn!(
        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
        source = "deferred_depth_disk_floor",
        bucket,
        free_fraction = ?free,
        floor = AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION,
        "after-close depth pass stopped early: free disk on the frame-log volume is below its \
         floor (or unmeasured). Buckets already finished are settled and released; the rest \
         stay marked, and their segments stay on disk for the next evening's pass"
    );
}

/// Runs one after-close pass over every marked bucket in `marks`, writing to
/// `sink`. Cold path: its own thread, after the close.
///
/// O(buckets × segments per bucket × frames per segment), bounded by the
/// window; nothing here runs per tick.
pub fn run_after_close_pass(
    sink: &mut DepthIngest,
    marks: &DeferredDepth,
    wal_dir: &Path,
    cancel: &AtomicBool,
    signals: &PassSignals<'_>,
) -> PassSummary {
    let mut summary = PassSummary::default();
    let snap = marks.snapshot();
    // O(1) EXEMPT: cold after-close work list, at most DEFERRED_BUCKETS entries
    let mut work = snap.marked_buckets();
    if work.is_empty() && !snap.overflowed {
        return summary;
    }
    // A listing that failed must never read as "those segments are gone" —
    // that would clear marks while the rows are still on disk.
    let spans = match deferred_segment_spans(wal_dir) {
        Ok(spans) => spans,
        Err(err) => {
            warn!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                source = "deferred_depth_listing_failed",
                error = %err,
                "after-close depth pass could not list the WAL directory — every mark stays \
                 for the next pass"
            );
            summary.buckets_carried = work.len() as u64;
            return summary;
        }
    };
    // An overflowed table cannot name the buckets it lost: every closed
    // segment at or below the overflow height is swept for both kinds.
    let mut overflow_complete = snap.overflowed;
    if snap.overflowed {
        // Plan item W2 (2026-10-27): every lost mark is at or below the
        // height, so a segment whose reach (one reorder slack below its first
        // record, as the prune reads it) starts above the height holds none
        // of them and is skipped BEFORE the open and unbounded checks. Until
        // this change the newest segment, which never has an upper bound,
        // marked every sweep incomplete and the overflow never cleared. An
        // unknown height reaches every segment, as before.
        let sweep_height = snap.overflow_height().unwrap_or(u64::MAX);
        for span in &spans {
            if span.lo != 0 && reorder_reach_lo(span.lo) > sweep_height {
                continue;
            }
            if span.lo == 0 || span.is_open {
                overflow_complete = false;
                continue;
            }
            let Some(hi) = span.hi else {
                // An unbounded segment cannot be swept completely.
                overflow_complete = false;
                continue;
            };
            let (lo_id, hi_id) = (
                span.lo >> DEFERRED_BUCKET_SHIFT,
                hi >> DEFERRED_BUCKET_SHIFT,
            );
            // A segment spans at most two buckets at the measured write rate;
            // a wider range means a forged or clock-stepped successor, so it
            // is not trusted and the overflow is not cleared.
            if hi_id.saturating_sub(lo_id) > OVERFLOW_SPAN_MAX_BUCKETS {
                overflow_complete = false;
                continue;
            }
            // O(1) EXEMPT: cold, at most OVERFLOW_SPAN_MAX_BUCKETS + 1 per segment
            for id in lo_id..=hi_id {
                work.push((id, SHED_DEPTH_ALL));
            }
        }
        // O(1) EXEMPT: cold, one sort over the work list
        work.sort_unstable_by_key(|&(id, _)| id);
        // Duplicate ids keep the union of their kinds.
        work.dedup_by(|later, earlier| {
            if later.0 == earlier.0 {
                earlier.1 |= later.1;
                true
            } else {
                false
            }
        });
    }
    if work.is_empty() {
        return summary;
    }
    // DA-13d: no depth partition is archived (exported, recounted, dropped)
    // from the first rewrite until the marks are cleared below, so a row
    // written back here cannot land between an archive's recount and its
    // DROP and vanish with the partition. Waits for at most the one depth
    // partition an archive pass may have in flight. Held to the end of this
    // function; this runs on the pass's own OS thread, never an async one.
    //
    // Held through the lag pauses and the settle too, though nothing is
    // written then, and deliberately: from here until the marks are cleared
    // the pass still has rows to write into these partitions, so an export
    // and DROP in a pause would leave those rows out of the S3 copy and the
    // later rewrite would recreate a partition holding only them. The
    // archivers read a hold as "stand aside", never as a failure: the daily
    // leg refunds the attempt and retries, the disk-pressure leg neither
    // counts the pass nor escalates (DA-13d review). The hold ends by the
    // pass's own deadline (07:15 IST plus the settle), or earlier at its disk
    // floor or a cancel.
    let _depth_partitions =
        tickvault_storage::partition_archive::hold_depth_partitions_for_rewrite();
    info!(
        buckets = work.len(),
        overflowed = snap.overflowed,
        "after-close deferred-depth pass starting — writing back order-book rows the ingest \
         shed skipped during the session"
    );

    let c = pass_counters();
    // ITEM 57a: rows of a frame received before the array-row switch that the
    // book-only ingest sent to `market_depth_book` (a subset of `written`).
    let pre_switch_to_book =
        metrics::counter!(DEFERRED_DEPTH_ROWS_COUNTER, "outcome" => "pre_switch_to_book");
    let pass_counter = |outcome: &'static str| {
        metrics::counter!(DEFERRED_DEPTH_PASS_COUNTER, "outcome" => outcome).increment(1);
    };
    let mut done: Vec<(u64, u8)> = Vec::with_capacity(work.len()); // APPROVED: cold

    for &(id, kinds) in &work {
        if let Err(reason) = wait_for_run(signals, wal_dir, cancel) {
            if reason == PassStop::DiskFloor {
                report_disk_floor_stop(signals, wal_dir, id);
            }
            summary.stop = Some(reason);
            break;
        }
        let bucket = segments_for_bucket(&spans, id);
        // An unreadable neighbour makes a bucket uncertain. Carried while it
        // is young (the neighbour may be a writer's torn tail that a restart
        // repairs), but not forever: past UNCERTAIN_CARRY_MAX_DAYS it is
        // reported as lost and released, so one bad file cannot pin its
        // neighbourhood for as long as any other mark exists.
        let expired_uncertain = bucket.uncertain
            && !bucket.touches_open
            && bucket_age_days(id) > UNCERTAIN_CARRY_MAX_DAYS;
        if expired_uncertain {
            summary.segments_damaged += 1;
            pass_counter("uncertain_expired");
            error!(
                code = ErrorCode::WsSpill02FrameDropped.code_str(),
                source = "deferred_depth_uncertain_expired",
                bucket = id,
                kinds,
                "order-book rows skipped under load sit next to an unreadable WAL segment and \
                 could not be read for several days — released as lost"
            );
            done.push((id, kinds));
            continue;
        }
        if bucket.touches_open || bucket.uncertain {
            summary.buckets_carried += 1;
            if !bucket.touches_open {
                summary.buckets_uncertain += 1;
            }
            overflow_complete = false;
            pass_counter("carried");
            continue;
        }
        if bucket.paths.is_empty() {
            // The segments are gone — nothing can be written back.
            summary.buckets_missing += 1;
            pass_counter("missing");
            error!(
                code = ErrorCode::WsSpill02FrameDropped.code_str(),
                source = "deferred_depth_segment_missing",
                bucket = id,
                kinds,
                "order-book rows skipped under load cannot be written back: no WAL segment \
                 holding them is left on disk. They are lost."
            );
            done.push((id, kinds));
            continue;
        }
        let keep = |seq: u64, endpoint: WalEndpoint| {
            seq >> DEFERRED_BUCKET_SHIFT == id && endpoint_matches(endpoint, kinds)
        };
        let mut failed = false;
        let mut disk_stop = false;
        let mut damaged = false;
        let mut bucket_no_receipt = 0u64;
        'segments: for path in &bucket.paths {
            let read = match read_segment_frames_matching(path, &keep) {
                Ok(read) => read,
                Err(err) => {
                    warn!(
                        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                        source = "deferred_depth_read_failed",
                        segment = %path.display(),
                        error = %err,
                        "after-close depth pass could not read a WAL segment — the bucket stays \
                         marked and is retried at the next pass"
                    );
                    failed = true;
                    break 'segments;
                }
            };
            damaged |= read.damaged;
            for frame in &read.frames {
                if cancel.load(Ordering::Acquire) {
                    failed = true;
                    break 'segments;
                }
                if frame.received_at_nanos == WAL_RECEIPT_UNKNOWN_NANOS {
                    summary.frames_no_receipt += 1;
                    bucket_no_receipt += 1;
                    continue;
                }
                let rows = match frame.endpoint {
                    WalEndpoint::MainFeed => {
                        let rows = rewrite_inline_depth(
                            sink,
                            &frame.frame,
                            frame.frame_seq,
                            frame.received_at_nanos,
                            c,
                        );
                        // `append_inline_depth` does not count its own rows;
                        // the dedicated drain does.
                        c.depth_rows.increment(rows);
                        rows
                    }
                    _ => rewrite_dedicated_depth(sink, frame, c),
                };
                if sink.writes_pre_switch_to_book(frame.received_at_nanos) {
                    pre_switch_to_book.increment(rows);
                }
                summary.rows_written = summary.rows_written.saturating_add(rows);
                PASS_ROWS_WRITTEN.fetch_add(rows, Ordering::Relaxed);
                // Re-check the disk floor before every late commit (one
                // `statvfs` per 16 MiB batch): a bucket can hold many
                // batches, and each commit rewrites an hour partition.
                if sink.late_flush_due()
                    && disk_below_pass_floor((signals.disk_free_fraction)(wal_dir))
                {
                    disk_stop = true;
                    failed = true;
                    break 'segments;
                }
                // The LATE-row trigger (plan item 55): every commit here
                // rewrites a whole hour partition, so fewer, larger ones.
                if sink.late_flush_due() && sink.flush().is_err() {
                    failed = true;
                    break 'segments;
                }
            }
        }
        // Flush at every bucket edge so no ILP batch spans two buckets.
        if !failed && sink.flush().is_err() {
            failed = true;
        }
        if disk_stop {
            // The unflushed rows of this bucket are not sent: it stays
            // marked and is rewritten onto the same keys next evening.
            summary.buckets_carried += 1;
            overflow_complete = false;
            report_disk_floor_stop(signals, wal_dir, id);
            summary.stop = Some(PassStop::DiskFloor);
            break;
        }
        if failed {
            summary.buckets_carried += 1;
            overflow_complete = false;
            pass_counter("write_failed");
            warn!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                source = "deferred_depth_write_failed",
                bucket = id,
                "after-close depth pass could not finish a bucket — it stays marked; the rows \
                 written so far collapse onto the same keys when it is retried"
            );
            continue;
        }
        if damaged {
            summary.segments_damaged += 1;
            pass_counter("damaged");
            error!(
                code = ErrorCode::WsSpill02FrameDropped.code_str(),
                source = "deferred_depth_segment_corrupt",
                bucket = id,
                "a WAL segment holding skipped order-book rows is damaged mid-file — the rows \
                 before the damage were written back, the rows after it are lost"
            );
        }
        if bucket_no_receipt > 0 {
            // A pre-TVW3 record has no receipt time, so a rewrite cannot land
            // on the row the live drain keyed by receipt. Lost, and said so.
            pass_counter("no_receipt");
            error!(
                code = ErrorCode::WsSpill02FrameDropped.code_str(),
                source = "deferred_depth_no_receipt",
                bucket = id,
                frames = bucket_no_receipt,
                "skipped order-book frames carry no receipt time and cannot be written back \
                 onto their original keys — they are lost"
            );
        }
        done.push((id, kinds));
    }

    // Settle: acks from a lagging or suspended table are not proof. Wait one
    // suspension poll, then require a healthy QuestDB before clearing.
    if !done.is_empty() {
        (signals.sleep)(AFTER_CLOSE_SETTLE);
        let healthy = loop {
            if cancel.load(Ordering::Acquire) {
                break false;
            }
            if (signals.lag_growing_tables)() == 0 && !(signals.sink_suspect)() {
                break true;
            }
            // Past the morning stop plus one settle (the window wraps
            // midnight, so "past" is the daytime gap before 15:45).
            let now = (signals.now_secs_ist)();
            if now >= AFTER_CLOSE_DEPTH_STOP_SECS_IST + AFTER_CLOSE_SETTLE.as_secs()
                && now < AFTER_CLOSE_DEPTH_START_SECS_IST
            {
                break false;
            }
            (signals.sleep)(AFTER_CLOSE_PAUSE);
        };
        if healthy {
            for &(id, kinds) in &done {
                marks.clear(id, kinds);
                summary.buckets_cleared += 1;
                pass_counter("cleared");
            }
            if overflow_complete && summary.stop.is_none() {
                marks.clear_overflow_through(snap.overflow_high_seq);
            }
            marks.persist_now();
        } else {
            summary.buckets_carried += done.len() as u64;
            warn!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                source = "deferred_depth_carryover",
                buckets = done.len(),
                "after-close depth pass wrote its buckets but QuestDB never showed healthy \
                 before the deadline — the marks stay and the next pass rewrites them onto the \
                 same keys"
            );
        }
    }
    // Whatever happened, the marks on disk reflect this pass's end state.
    marks.persist_now();
    info!(
        cleared = summary.buckets_cleared,
        carried = summary.buckets_carried,
        rows = summary.rows_written,
        no_receipt = summary.frames_no_receipt,
        damaged = summary.segments_damaged,
        missing = summary.buckets_missing,
        stop = ?summary.stop,
        "after-close deferred-depth pass finished"
    );
    summary
}

/// The pass's depth writer (ITEM 57a, 2026-10-13): book-only when the lane
/// has an array-row start instant, so the pass cannot write `market_depth`
/// at all; the level writer as before when array rows are off. Cold, once per
/// pass.
fn pass_depth_ingest(
    questdb: &tickvault_common::config::QuestDbConfig,
    book_since: Option<i64>,
) -> DepthIngest {
    match book_since {
        Some(since) => DepthIngest::new_book_only(questdb, since),
        None => DepthIngest::new(questdb, None),
    }
}

/// How long a round waits after asking the WAL writer to seal its open
/// segment: ten of the writer's 200 ms lull polls.
pub const AFTER_CLOSE_SEAL_WAIT: Duration = Duration::from_secs(2); // APPROVED: this IS the named constant
/// Pause between rounds while the last one made progress or left nothing.
pub const AFTER_CLOSE_ROUND_GAP_SECS: u64 = 60;
/// Pause after a round that wrote nothing and cleared nothing (a disk floor,
/// a bucket waiting on an unreadable neighbour): retrying every minute would
/// only repeat the same log lines.
pub const AFTER_CLOSE_IDLE_ROUND_GAP_SECS: u64 = 600;

/// The pause before the next round, from the round just finished. Pure, O(1).
#[must_use]
pub const fn next_round_gap_secs(summary: &PassSummary) -> u64 {
    if summary.rows_written == 0 && summary.buckets_cleared == 0 && summary.buckets_carried > 0 {
        AFTER_CLOSE_IDLE_ROUND_GAP_SECS
    } else {
        AFTER_CLOSE_ROUND_GAP_SECS
    }
}

/// Spawns the after-close supervisor. Inside the wrapping window
/// (15:45–07:15 IST) it asks the WAL writer to seal its open segment, runs one
/// pass on its own OS thread with its own synchronous depth writer, publishes
/// the outcome for the drain hold, and repeats a minute later (ten when a
/// round made no progress) until no mark is left. Operator Quote 31
/// (2026-10-21): the day's write-back finishes the same day; until then this
/// ran ONE pass a day ending at 17:15, and whatever was left waited a day.
/// `cancel` stops it (set by the lane when its drain ends); it never waits on
/// the lane's shutdown `Notify`, which wakes a single waiter and would be
/// stolen from the drain.
///
/// `book_since` is the lane's array-row start instant (plan item 49e step 3),
/// passed rather than re-read. `Some` builds a book-only ingest
/// ([`pass_depth_ingest`], ITEM 57a): every frame goes to
/// `market_depth_book`, and the instant only marks which rows count as
/// `pre_switch_to_book`. `None` (array rows off) writes `market_depth`.
pub fn spawn_after_close_supervisor(
    questdb: tickvault_common::config::QuestDbConfig,
    wal_dir: std::path::PathBuf,
    cancel: std::sync::Arc<AtomicBool>,
    book_since: Option<i64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            // Poll the clock at most once a minute so cancellation is seen
            // promptly without a second shutdown signal.
            let wait = secs_until_window(super::now_ist_secs_of_day()).min(60);
            if wait > 0 {
                // Between windows: forget the last night's outcome, so a process
                // that outlives a day never reads yesterday's "nothing left" as
                // tonight's (hostile review, 2026-10-21).
                LAST_PASS_RETRYABLE_LEFT.store(PASS_OUTCOME_UNKNOWN, Ordering::Release);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            // Nothing marked: nothing to open a writer for. Report the empty
            // outcome so the drain hold sees this half done, and look again
            // in a minute (a crash-recovered mark file can add work).
            let marks = tickvault_storage::wal_deferred_depth::deferred_depth().snapshot();
            if marks.is_empty() {
                publish_pass_outcome(&PassSummary::default());
                tokio::time::sleep(Duration::from_secs(AFTER_CLOSE_ROUND_GAP_SECS)).await;
                continue;
            }
            // The session's last segment is still open (no frame comes after
            // the close to fill and rotate it), and a bucket overlapping an
            // open segment is carried. Ask the writer to seal it in its next
            // lull, and give it a few of its 200 ms polls to do so.
            tickvault_storage::wal_idle_seal::request_idle_seal(&wal_dir);
            tokio::time::sleep(AFTER_CLOSE_SEAL_WAIT).await;
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            let questdb = questdb.clone();
            let thread_wal_dir = wal_dir.clone();
            let thread_cancel = std::sync::Arc::clone(&cancel);
            let spawned = std::thread::Builder::new()
                .name("tv-deferred-depth".to_string())
                .spawn(move || {
                    let mut sink = pass_depth_ingest(&questdb, book_since);
                    let summary = run_after_close_pass(
                        &mut sink,
                        tickvault_storage::wal_deferred_depth::deferred_depth(),
                        &thread_wal_dir,
                        &thread_cancel,
                        &live_signals(),
                    );
                    drop(tx.send(summary));
                });
            let mut gap = AFTER_CLOSE_ROUND_GAP_SECS;
            if let Err(err) = spawned {
                error!(
                    code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                    source = "deferred_depth_thread_spawn_failed",
                    error = %err,
                    "after-close depth pass thread could not start — skipped order-book rows \
                     stay marked and on disk until a later pass"
                );
            } else if let Ok(summary) = rx.await {
                publish_pass_outcome(&summary);
                gap = next_round_gap_secs(&summary);
            }
            let mut remaining = gap;
            while remaining > 0 {
                let step = remaining.min(60);
                tokio::time::sleep(Duration::from_secs(step)).await;
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                remaining -= step;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tickvault_storage::ws_frame_spill::WsType;

    const IN_WINDOW: u64 = AFTER_CLOSE_DEPTH_START_SECS_IST + 60;

    /// A free fraction well clear of every shed and floor threshold.
    const HEALTHY_FREE: f64 = 0.5;

    #[test]
    fn after_close_step_over_the_full_grid() {
        use PassStep::{Pause, Run, Stop};
        let before = AFTER_CLOSE_DEPTH_START_SECS_IST - 1;
        let at_stop = AFTER_CLOSE_DEPTH_STOP_SECS_IST;
        let caught_up = Some(0);
        assert_eq!(
            after_close_step(IN_WINDOW, 0, caught_up, false, false, false),
            Run
        );
        assert_eq!(
            after_close_step(IN_WINDOW, 1, caught_up, false, false, false),
            Pause
        );
        assert_eq!(
            after_close_step(IN_WINDOW, 0, caught_up, true, false, false),
            Pause
        );
        // A low disk stops the pass (never a pause), even while QuestDB lags.
        assert_eq!(
            after_close_step(IN_WINDOW, 3, caught_up, true, true, false),
            Stop(PassStop::DiskFloor)
        );
        assert_eq!(
            after_close_step(before, 0, caught_up, false, false, false),
            Stop(PassStop::BeforeWindow)
        );
        assert_eq!(
            after_close_step(at_stop, 0, caught_up, false, false, false),
            Stop(PassStop::Deadline)
        );
        // Cancellation wins over everything, including a paused state.
        assert_eq!(
            after_close_step(IN_WINDOW, 5, caught_up, true, true, true),
            Stop(PassStop::Cancelled)
        );
    }

    /// Regression (Quote 31, 2026-10-21): on 20 Oct the pass paused only on a
    /// GROWING lag and drove QuestDB from 0 to 6,758 transactions behind; the
    /// box stopped ~6,000 behind and the next morning applied them on top of
    /// the open. The pass now pauses on the backlog's SIZE, and on an unknown
    /// size, and the window runs past midnight instead of ending at 17:15.
    #[test]
    fn test_regression_pass_pauses_on_a_large_backlog_and_runs_past_midnight() {
        use PassStep::{Pause, Run};
        let ceiling = AFTER_CLOSE_APPLY_LAG_CEILING;
        assert_eq!(
            after_close_step(IN_WINDOW, 0, Some(ceiling), false, false, false),
            Run
        );
        assert_eq!(
            after_close_step(IN_WINDOW, 0, Some(ceiling + 1), false, false, false),
            Pause,
            "a non-growing but large backlog must pause the pass"
        );
        assert_eq!(
            after_close_step(IN_WINDOW, 0, Some(6_758), false, false, false),
            Pause
        );
        assert_eq!(
            after_close_step(IN_WINDOW, 0, None, false, false, false),
            Pause,
            "an unmeasured backlog never reads as caught up"
        );
        // 17:30, 21:00, 23:59 and 03:00 IST are all inside the window now.
        for secs in [63_000, 75_600, 86_399, 10_800] {
            assert_eq!(
                after_close_step(secs, 0, Some(0), false, false, false),
                Run,
                "{secs}s IST must be inside the after-close window"
            );
        }
    }

    proptest::proptest! {
        /// Outside the window the pass never runs; inside, it runs only when
        /// every pressure signal is clear.
        #[test]
        fn after_close_pass_stops_before_the_stop_time(
            secs in 0u64..86_400,
            lag in 0u32..4,
            backlog in proptest::option::of(0u64..2_000),
            suspect in proptest::bool::ANY,
            disk_low in proptest::bool::ANY,
        ) {
            let step = after_close_step(secs, lag, backlog, suspect, disk_low, false);
            let in_window = secs >= AFTER_CLOSE_DEPTH_START_SECS_IST
                || secs < AFTER_CLOSE_DEPTH_STOP_SECS_IST;
            let backlog_high = backlog.is_none_or(|b| b > AFTER_CLOSE_APPLY_LAG_CEILING);
            if !in_window {
                proptest::prop_assert!(matches!(step, PassStep::Stop(_)));
            } else if disk_low {
                proptest::prop_assert_eq!(step, PassStep::Stop(PassStop::DiskFloor));
            } else if lag > 0 || suspect || backlog_high {
                proptest::prop_assert_eq!(step, PassStep::Pause);
            } else {
                proptest::prop_assert_eq!(step, PassStep::Run);
            }
        }
    }

    #[test]
    fn secs_until_window_is_zero_inside_and_counts_to_the_afternoon_start() {
        assert_eq!(secs_until_window(IN_WINDOW), 0);
        assert_eq!(secs_until_window(0), 0, "midnight is inside the window");
        assert_eq!(
            secs_until_window(AFTER_CLOSE_DEPTH_START_SECS_IST - 100),
            100
        );
        assert_eq!(
            secs_until_window(AFTER_CLOSE_DEPTH_STOP_SECS_IST),
            AFTER_CLOSE_DEPTH_START_SECS_IST - AFTER_CLOSE_DEPTH_STOP_SECS_IST
        );
    }

    #[test]
    fn pass_rows_written_total_never_falls() {
        let before = pass_rows_written_total();
        PASS_ROWS_WRITTEN.fetch_add(3, Ordering::Relaxed);
        assert!(
            pass_rows_written_total() >= before + 3,
            "a running total only grows"
        );
    }

    #[test]
    fn in_after_close_window_wraps_midnight() {
        assert!(in_after_close_window(AFTER_CLOSE_DEPTH_START_SECS_IST));
        assert!(in_after_close_window(86_399));
        assert!(in_after_close_window(0));
        assert!(in_after_close_window(AFTER_CLOSE_DEPTH_STOP_SECS_IST - 1));
        assert!(!in_after_close_window(AFTER_CLOSE_DEPTH_STOP_SECS_IST));
        assert!(!in_after_close_window(AFTER_CLOSE_DEPTH_START_SECS_IST - 1));
    }

    #[test]
    fn retryable_left_ignores_uncertain_carries_but_not_an_early_stop() {
        let mut s = PassSummary {
            buckets_carried: 3,
            buckets_uncertain: 3,
            ..PassSummary::default()
        };
        assert_eq!(
            s.retryable_left(),
            0,
            "only unreadable-neighbour carries: done tonight"
        );
        s.buckets_uncertain = 1;
        assert_eq!(s.retryable_left(), 2);
        let stopped = PassSummary {
            stop: Some(PassStop::DiskFloor),
            ..PassSummary::default()
        };
        assert_eq!(
            stopped.retryable_left(),
            1,
            "an early stop left unvisited work"
        );
        assert_eq!(PassSummary::default().retryable_left(), 0);
    }

    #[test]
    fn test_publish_pass_outcome_reads_back_via_last_pass_retryable_left() {
        publish_pass_outcome(&PassSummary {
            buckets_carried: 2,
            ..PassSummary::default()
        });
        assert_eq!(last_pass_retryable_left(), Some(2));
        publish_pass_outcome(&PassSummary::default());
        assert_eq!(last_pass_retryable_left(), Some(0));
    }

    #[test]
    fn next_round_gap_secs_backs_off_only_when_a_round_made_no_progress() {
        let idle = PassSummary {
            buckets_carried: 1,
            ..PassSummary::default()
        };
        assert_eq!(next_round_gap_secs(&idle), AFTER_CLOSE_IDLE_ROUND_GAP_SECS);
        let progress = PassSummary {
            buckets_carried: 1,
            rows_written: 10,
            ..PassSummary::default()
        };
        assert_eq!(next_round_gap_secs(&progress), AFTER_CLOSE_ROUND_GAP_SECS);
        assert_eq!(
            next_round_gap_secs(&PassSummary::default()),
            AFTER_CLOSE_ROUND_GAP_SECS
        );
    }

    /// 2026-05-28 10:00:00 IST (a Thursday, inside the depth writer's open
    /// window) as a receipt, and a capture sequence at the same instant. A
    /// receipt outside the session is refused by the writer, which would make
    /// every row-count assertion here vacuous.
    fn in_session() -> (u64, i64) {
        let rx: i64 = 1_779_942_600 * 1_000_000_000;
        (u64::try_from(rx).unwrap() >> 17 << 17, rx)
    }

    /// A minimal well-formed Full-mode packet on IDX_I.
    fn full_packet(security_id: u32) -> Vec<u8> {
        let mut buf = vec![0u8; tickvault_common::constants::FULL_QUOTE_PACKET_SIZE];
        buf[0] = tickvault_common::constants::RESPONSE_CODE_FULL;
        buf[1..3].copy_from_slice(
            &(tickvault_common::constants::FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes(),
        );
        buf[3] = 0;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&100.5f32.to_le_bytes());
        buf[14..18].copy_from_slice(&1_700_000_000u32.to_le_bytes());
        buf
    }

    /// The rewrite numbers packets exactly as the live append does: for a
    /// two-packet frame, the second packet's rows carry packet index 1.
    #[test]
    fn test_rewrite_inline_depth_matches_the_live_capture_seq_for_every_packet() {
        let mut frame = full_packet(13);
        frame.extend_from_slice(&full_packet(25));
        let (seq, rx) = in_session();

        let mut rewritten = DepthIngest::for_test();
        let rows = rewrite_inline_depth(&mut rewritten, &frame, seq, rx, pass_counters());
        assert_eq!(rows, 20, "two packets x 5 levels x 2 sides");
        assert_eq!(
            rewritten.pending_rows(),
            20,
            "the rows must reach the writer, not be refused as out of window"
        );

        let mut live = DepthIngest::for_test();
        for (idx, sid) in [(0u32, 13u32), (1, 25)] {
            let packet = full_packet(sid);
            let Ok(ParsedFrame::TickWithDepth(tick, levels)) = dispatch_frame(&packet, rx) else {
                panic!("fixture must parse as a Full packet");
            };
            append_inline_depth(
                &mut live,
                &tick,
                &levels,
                rx,
                seq,
                idx,
                false,
                pass_counters(),
            );
        }
        assert_eq!(
            rewritten.pending_ilp(),
            live.pending_ilp(),
            "same rows, same capture_seq — a rewrite lands on the live rows"
        );
    }

    /// ITEM 57a (2026-10-13; replaces the 49e step 3 receipt-routing test):
    /// with array rows on, the after-close pass writes every frame to
    /// `market_depth_book`, both one received at or after the start instant
    /// and one received before it. Nothing reaches `market_depth`, and only
    /// the older frame counts as received before the switch.
    #[test]
    fn test_rewrite_inline_depth_goes_to_the_book_table_with_array_rows_on() {
        let frame = full_packet(13);
        let (seq, rx) = in_session();

        // Received at the start instant (post-switch).
        let mut post = DepthIngest::for_test_book_only(rx);
        assert!(!post.writes_pre_switch_to_book(rx));
        let rows = rewrite_inline_depth(&mut post, &frame, seq, rx, pass_counters());
        assert_eq!(rows, 10, "5 levels x 2 sides, counted in levels");
        assert!(!post.pending_book_bytes().is_empty(), "array rows written");
        assert_eq!(
            post.book.as_ref().map(|b| b.levels.pending()),
            Some(0),
            "nothing on the level writer"
        );
        assert!(!post.pending_ilp().contains("market_depth,"));

        // Received before the start instant (pre-switch): still the book.
        let mut pre = DepthIngest::for_test_book_only(rx + 1);
        assert!(pre.writes_pre_switch_to_book(rx), "counted as pre-switch");
        assert!(pre.writes_book(rx, false), "routed to the book anyway");
        let rows = rewrite_inline_depth(&mut pre, &frame, seq, rx, pass_counters());
        assert_eq!(rows, 10);
        assert!(!pre.pending_book_bytes().is_empty(), "array rows written");
        assert_eq!(
            pre.book.as_ref().map(|b| b.levels.pending()),
            Some(0),
            "a pre-switch frame must not reach the market_depth writer"
        );
        assert!(!pre.pending_ilp().contains("market_depth,"));
        assert_eq!(
            pre.pending_book_bytes(),
            post.pending_book_bytes(),
            "the same book rows whatever the receipt time"
        );

        // The lane's receipt routing is unchanged: the same pre-switch frame
        // on a non-book-only ingest still goes to the level writer.
        let mut lane = DepthIngest::for_test_book(rx + 1);
        assert!(!lane.writes_pre_switch_to_book(rx), "only book-only counts");
        let rows = rewrite_inline_depth(&mut lane, &frame, seq, rx, pass_counters());
        assert_eq!(rows, 10);
        assert!(lane.pending_book_bytes().is_empty(), "no array row");
        assert!(lane.pending_ilp().contains("market_depth,"));
    }

    /// ITEM 57a: the pass builds its writer through `pass_depth_ingest`,
    /// which is book-only whenever there is an array-row instant, and the
    /// pass counts the pre-switch rows.
    #[test]
    fn test_the_pass_writer_is_book_only_with_an_array_row_instant() {
        let questdb = tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let since = 1_000_000;
        // Book-only: a frame from before the switch is counted as one the
        // pass moves into the book table; one at or after it is not.
        let sink = pass_depth_ingest(&questdb, Some(since));
        assert!(sink.writes_pre_switch_to_book(since - 1));
        assert!(!sink.writes_pre_switch_to_book(since));
        // No instant (array rows off): no book mode, so nothing counts.
        let sink = pass_depth_ingest(&questdb, None);
        assert!(!sink.writes_pre_switch_to_book(since - 1));
        assert!(!DepthIngest::for_test().writes_pre_switch_to_book(0));
    }

    /// A shed frame's depth is written back; a frame outside the bucket is
    /// not; the mark clears only after a healthy settle.
    #[test]
    fn after_close_pass_writes_depth_only_and_clears_after_a_healthy_settle() {
        let dir = std::env::temp_dir().join(format!(
            "tv-deferred-pass-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (seq, rx) = in_session();
        {
            let wal = tickvault_storage::ws_frame_spill::WsFrameSpill::new(&dir).expect("wal");
            wal.append_with_seq_at(
                WsType::LiveFeed,
                full_packet(13),
                seq,
                rx,
                WalEndpoint::MainFeed,
            );
            // A frame in a much later bucket: must not be rewritten.
            wal.append_with_seq_at(
                WsType::LiveFeed,
                full_packet(25),
                seq + (10u64 << DEFERRED_BUCKET_SHIFT),
                rx,
                WalEndpoint::MainFeed,
            );
            wal.shutdown(Duration::from_secs(5));
        }
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(seq, ShedDepth::Inline);

        let slept = Cell::new(0u64);
        let sleep = |d: Duration| slept.set(slept.get() + d.as_secs());
        let signals = PassSignals {
            now_secs_ist: &|| IN_WINDOW,
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| Some(HEALTHY_FREE),
            sleep: &sleep,
        };
        let cancel = AtomicBool::new(false);
        let mut sink = DepthIngest::for_test();
        let summary = run_after_close_pass(&mut sink, &marks, &dir, &cancel, &signals);

        // `for_test` has no QuestDB: its flush fails, so the bucket must be
        // CARRIED, never cleared on a write it could not prove.
        assert_eq!(summary.buckets_cleared, 0);
        assert_eq!(summary.buckets_carried, 1);
        assert_eq!(
            summary.rows_written, 10,
            "only the shed frame's 10 inline rows"
        );
        assert!(
            !marks.snapshot().is_empty(),
            "a failed flush keeps the mark"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cancellation stops the pass before it touches a bucket.
    #[test]
    fn test_run_after_close_pass_cancelled_touches_nothing() {
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(1_780_000_000_000_000_000u64, ShedDepth::Inline);
        let signals = PassSignals {
            now_secs_ist: &|| IN_WINDOW,
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| Some(HEALTHY_FREE),
            sleep: &|_| {},
        };
        let cancel = AtomicBool::new(true);
        let mut sink = DepthIngest::for_test();
        let summary =
            run_after_close_pass(&mut sink, &marks, &std::env::temp_dir(), &cancel, &signals);
        assert_eq!(summary.stop, Some(PassStop::Cancelled));
        assert_eq!(summary.buckets_cleared, 0);
        assert!(!marks.snapshot().is_empty());
    }

    /// A pass started outside the window does nothing.
    #[test]
    fn test_run_after_close_pass_outside_the_window_touches_nothing() {
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(1_780_000_000_000_000_000u64, ShedDepth::Dedicated);
        let signals = PassSignals {
            now_secs_ist: &|| AFTER_CLOSE_DEPTH_STOP_SECS_IST,
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| Some(HEALTHY_FREE),
            sleep: &|_| {},
        };
        let cancel = AtomicBool::new(false);
        let mut sink = DepthIngest::for_test();
        let summary =
            run_after_close_pass(&mut sink, &marks, &std::env::temp_dir(), &cancel, &signals);
        assert_eq!(summary.stop, Some(PassStop::Deadline));
        assert!(!marks.snapshot().is_empty());
    }

    /// Writes one shed Full frame whose receipt is OUTSIDE the session: the
    /// writer refuses its rows by design, so the flush has nothing to send and
    /// succeeds — which lets these tests reach the settle-and-clear step
    /// without a QuestDB.
    fn wal_with_one_refused_frame(tag: &str) -> (std::path::PathBuf, u64) {
        wal_with_refused_frames(tag, 1)
    }

    /// [`wal_with_one_refused_frame`] with `buckets` frames, one in each of
    /// that many consecutive deferred buckets. Returns the first frame's
    /// sequence; frame `i` is at `seq + (i << DEFERRED_BUCKET_SHIFT)`.
    fn wal_with_refused_frames(tag: &str, buckets: u64) -> (std::path::PathBuf, u64) {
        let dir = std::env::temp_dir().join(format!(
            "tv-deferred-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let rx: i64 = 1_780_000_000 * 1_000_000_000; // 01:56 IST
        let seq = u64::try_from(rx).unwrap() >> 17 << 17;
        let wal = tickvault_storage::ws_frame_spill::WsFrameSpill::new(&dir).expect("wal");
        for i in 0..buckets {
            wal.append_with_seq_at(
                WsType::LiveFeed,
                full_packet(13),
                seq + (i << DEFERRED_BUCKET_SHIFT),
                rx,
                WalEndpoint::MainFeed,
            );
        }
        wal.shutdown(Duration::from_secs(5));
        (dir, seq)
    }

    /// A clean flush plus a healthy settle clears the mark.
    #[test]
    fn deferred_mark_clears_after_a_healthy_settle() {
        let (dir, seq) = wal_with_one_refused_frame("healthy");
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(seq, ShedDepth::Inline);
        let clock = Cell::new(IN_WINDOW);
        let sleep = |d: Duration| clock.set(clock.get() + d.as_secs());
        let signals = PassSignals {
            now_secs_ist: &|| clock.get(),
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| Some(HEALTHY_FREE),
            sleep: &sleep,
        };
        let summary = run_after_close_pass(
            &mut DepthIngest::for_test(),
            &marks,
            &dir,
            &AtomicBool::new(false),
            &signals,
        );
        assert_eq!(summary.buckets_cleared, 1);
        assert!(marks.snapshot().is_empty());
        assert!(
            clock.get() >= IN_WINDOW + AFTER_CLOSE_SETTLE.as_secs(),
            "it waited to settle"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two segments, A holding a frame at `seq` and B (the newest, so with
    /// no upper bound) holding one three buckets later, each written by its
    /// own WAL so B is a separate file. Both frames are outside the session
    /// (01:56 IST), so the pass writes nothing and its flush succeeds.
    fn wal_with_two_segments(tag: &str) -> (std::path::PathBuf, u64, u64) {
        let (dir, seq) = wal_with_one_refused_frame(tag);
        let later = seq + (3u64 << DEFERRED_BUCKET_SHIFT);
        let rx: i64 = 1_780_000_000 * 1_000_000_000;
        {
            let wal = tickvault_storage::ws_frame_spill::WsFrameSpill::new(&dir).expect("wal");
            wal.append_with_seq_at(
                WsType::LiveFeed,
                full_packet(13),
                later,
                rx,
                WalEndpoint::MainFeed,
            );
            wal.shutdown(Duration::from_secs(5));
        }
        (dir, seq, later)
    }

    /// Plan item W2 (2026-10-27): an overflow whose height is below the
    /// newest segment is cleared once every segment reaching the height is
    /// swept. Before, the newest segment (no upper bound) marked every sweep
    /// incomplete, so the overflow never cleared and pinned every later
    /// segment for good. An unknown height still reaches the newest segment
    /// and stays set.
    #[test]
    fn test_regression_pass_clears_an_overflow_below_the_newest_segment() {
        use tickvault_storage::wal_deferred_depth::DeferredDepthSnapshot;
        let (dir, seq, later) = wal_with_two_segments("w2-overflow");
        let spans = deferred_segment_spans(&dir).expect("listable");
        assert_eq!(spans.len(), 2, "two segments: {spans:?}");
        assert!(spans[1].hi.is_none(), "the newest has no upper bound");
        let height = (((seq >> DEFERRED_BUCKET_SHIFT) + 1) << DEFERRED_BUCKET_SHIFT) - 1;
        assert!(height < later);
        let run = |overflow_high_seq: u64| {
            let marks = DeferredDepth::new_for_tests();
            marks.seed(&DeferredDepthSnapshot {
                overflowed: true,
                overflow_high_seq,
                ..Default::default()
            });
            let clock = Cell::new(IN_WINDOW);
            let sleep = |d: Duration| clock.set(clock.get() + d.as_secs());
            let signals = PassSignals {
                now_secs_ist: &|| clock.get(),
                lag_growing_tables: &|| 0,
                apply_lag: &|| Some(0),
                sink_suspect: &|| false,
                disk_free_fraction: &|_| Some(HEALTHY_FREE),
                sleep: &sleep,
            };
            let summary = run_after_close_pass(
                &mut DepthIngest::for_test(),
                &marks,
                &dir,
                &AtomicBool::new(false),
                &signals,
            );
            assert!(summary.stop.is_none(), "{summary:?}");
            marks.snapshot().overflowed
        };
        assert!(!run(height), "a known height below B: swept and cleared");
        assert!(run(u64::MAX), "an unknown height reaches B: kept");
        assert!(run(later), "a height reaching B: kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression: 2026-10-13 (DA-13d) — the pass wrote depth rows back while
    // the partition archiver could be between a recount and a DROP of the
    // same partition. It must hold the depth-partition gate through its
    // writes and its settle, until the marks are cleared.
    #[test]
    fn test_regression_da13d_pass_holds_the_depth_partition_gate_until_cleared() {
        let (dir, seq) = wal_with_one_refused_frame("gate");
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(seq, ShedDepth::Inline);
        let clock = Cell::new(IN_WINDOW);
        let gate_free_during_settle = Cell::new(None);
        let sleep = |d: Duration| {
            if d == AFTER_CLOSE_SETTLE {
                let free =
                    tickvault_storage::partition_archive::try_hold_depth_partitions().is_some();
                gate_free_during_settle.set(Some(free));
            }
            clock.set(clock.get() + d.as_secs());
        };
        let signals = PassSignals {
            now_secs_ist: &|| clock.get(),
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| Some(HEALTHY_FREE),
            sleep: &sleep,
        };
        let summary = run_after_close_pass(
            &mut DepthIngest::for_test(),
            &marks,
            &dir,
            &AtomicBool::new(false),
            &signals,
        );
        assert_eq!(summary.buckets_cleared, 1);
        assert_eq!(
            gate_free_during_settle.get(),
            Some(false),
            "an archive pass could take a depth partition while the pass settles"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lagging QuestDB after the writes means the acks are not proof: the
    /// mark stays for the next pass, and the pass still ends by the deadline.
    #[test]
    fn deferred_mark_clears_only_after_ack_on_a_healthy_database() {
        let (dir, seq) = wal_with_one_refused_frame("lagging");
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(seq, ShedDepth::Inline);
        let clock = Cell::new(IN_WINDOW);
        let settling = Cell::new(false);
        let sleep = |d: Duration| {
            if d == AFTER_CLOSE_SETTLE {
                settling.set(true);
            }
            // Wraps at midnight like the real IST clock: the window does.
            clock.set((clock.get() + d.as_secs()) % 86_400);
        };
        let signals = PassSignals {
            now_secs_ist: &|| clock.get(),
            // Healthy while writing, lagging from the settle onwards.
            lag_growing_tables: &|| u32::from(settling.get()),
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| Some(HEALTHY_FREE),
            sleep: &sleep,
        };
        let summary = run_after_close_pass(
            &mut DepthIngest::for_test(),
            &marks,
            &dir,
            &AtomicBool::new(false),
            &signals,
        );
        assert_eq!(summary.buckets_cleared, 0);
        assert_eq!(summary.buckets_carried, 1);
        assert!(
            !marks.snapshot().is_empty(),
            "the mark survives an unhealthy settle"
        );
        let bound = AFTER_CLOSE_DEPTH_STOP_SECS_IST + AFTER_CLOSE_SETTLE.as_secs();
        assert!(
            (bound..=bound + AFTER_CLOSE_PAUSE.as_secs()).contains(&clock.get()),
            "the settle wait is bounded by the MORNING deadline: {}",
            clock.get()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression, 2026-10-12 (MEASURED on the box): the disk shed turned on
    /// at 09:52 IST at 80% used and never turned off, because inline depth
    /// restores only above 28% free. The pass paused on the shed from 15:55 to
    /// 17:15 and wrote nothing (`rows=0`, `cleared=0`, `stop=Deadline`), so the
    /// pinned segments it alone can release kept filling the disk.
    ///
    /// Here the disk sits at 15% free (the day's reading), where the disk
    /// shed is on and stays on, and QuestDB is healthy: the pass must run,
    /// settle and clear the bucket, not wait out the window.
    #[test]
    fn test_regression_deferred_pass_runs_while_the_disk_shed_is_on() {
        use tickvault_common::ingest_shed::{ShedLevel, decide_shed_level};
        const DAY_FREE: f64 = 0.15;
        // Precondition: at this free fraction the disk shed is ON and holds.
        assert_eq!(
            decide_shed_level(ShedLevel::InlineDepth, DAY_FREE, true),
            ShedLevel::InlineDepth,
            "the fixture must reproduce a disk-held inline shed"
        );

        let (dir, seq) = wal_with_one_refused_frame("disk-shed");
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(seq, ShedDepth::Inline);
        let clock = Cell::new(IN_WINDOW);
        let sleep = |d: Duration| clock.set(clock.get() + d.as_secs());
        let signals = PassSignals {
            now_secs_ist: &|| clock.get(),
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| Some(DAY_FREE),
            sleep: &sleep,
        };
        let summary = run_after_close_pass(
            &mut DepthIngest::for_test(),
            &marks,
            &dir,
            &AtomicBool::new(false),
            &signals,
        );
        assert_eq!(summary.stop, None, "the pass must not wait out the window");
        assert_eq!(summary.buckets_cleared, 1);
        assert!(marks.snapshot().is_empty(), "the mark is released");
        assert!(
            clock.get() < IN_WINDOW + AFTER_CLOSE_SETTLE.as_secs() + AFTER_CLOSE_PAUSE.as_secs(),
            "no pause before the bucket: only the settle elapsed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The disk floor sits above the point where pinned segments are deleted
    /// as a loss, and below the inline shed threshold, so a disk-only inline
    /// shed never stops the pass on its own.
    #[test]
    fn test_disk_floor_sits_between_the_pinned_delete_floor_and_the_inline_shed() {
        assert!(
            AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION
                > tickvault_storage::ws_frame_spill::WAL_PINNED_DISK_FLOOR_FREE_FRACTION
        );
        assert!(
            AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION
                < tickvault_common::ingest_shed::SHED_INLINE_DEPTH_BELOW_FREE
        );
        assert!(!disk_below_pass_floor(Some(0.15)));
        assert!(!disk_below_pass_floor(Some(
            AFTER_CLOSE_DISK_FLOOR_FREE_FRACTION
        )));
        assert!(disk_below_pass_floor(Some(0.079)));
        assert!(disk_below_pass_floor(Some(0.0)));
        // Unmeasured or nonsense reads as low: never add rows blind.
        assert!(disk_below_pass_floor(None));
        assert!(disk_below_pass_floor(Some(f64::NAN)));
    }

    /// Two marked buckets; the disk falls below the floor after the first.
    /// The pass stops (counted), still settles and releases the finished
    /// bucket, and keeps the second marked for the next evening.
    #[test]
    fn test_deferred_pass_stops_at_the_disk_floor_and_releases_finished_buckets() {
        let (dir, seq) = wal_with_refused_frames("disk-floor", 2);
        let seq2 = seq + (1u64 << DEFERRED_BUCKET_SHIFT);
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(seq, ShedDepth::Inline);
        marks.note_shed(seq2, ShedDepth::Inline);
        assert_eq!(marks.snapshot().marked_buckets().len(), 2);

        let probes = Cell::new(0u32);
        let disk = |_: &Path| {
            probes.set(probes.get() + 1);
            if probes.get() == 1 {
                Some(HEALTHY_FREE)
            } else {
                Some(0.01)
            }
        };
        let clock = Cell::new(IN_WINDOW);
        let sleep = |d: Duration| clock.set(clock.get() + d.as_secs());
        let signals = PassSignals {
            now_secs_ist: &|| clock.get(),
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &disk,
            sleep: &sleep,
        };
        let summary = run_after_close_pass(
            &mut DepthIngest::for_test(),
            &marks,
            &dir,
            &AtomicBool::new(false),
            &signals,
        );
        assert_eq!(summary.stop, Some(PassStop::DiskFloor));
        assert_eq!(
            summary.buckets_cleared, 1,
            "the finished bucket is released"
        );
        let left = marks.snapshot().marked_buckets();
        assert_eq!(left.len(), 1, "the unfinished bucket stays marked");
        assert_eq!(left[0].0, seq2 >> DEFERRED_BUCKET_SHIFT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A disk below the floor from the start touches nothing.
    #[test]
    fn test_deferred_pass_below_the_disk_floor_touches_nothing() {
        let (dir, seq) = wal_with_one_refused_frame("disk-low");
        let marks = DeferredDepth::new_for_tests();
        marks.note_shed(seq, ShedDepth::Inline);
        let signals = PassSignals {
            now_secs_ist: &|| IN_WINDOW,
            lag_growing_tables: &|| 0,
            apply_lag: &|| Some(0),
            sink_suspect: &|| false,
            disk_free_fraction: &|_| None,
            sleep: &|_| {},
        };
        let summary = run_after_close_pass(
            &mut DepthIngest::for_test(),
            &marks,
            &dir,
            &AtomicBool::new(false),
            &signals,
        );
        assert_eq!(summary.stop, Some(PassStop::DiskFloor));
        assert_eq!(summary.rows_written, 0);
        assert_eq!(summary.buckets_cleared, 0);
        assert!(!marks.snapshot().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_matches_only_the_shed_kind() {
        let inline = ShedDepth::Inline as u8;
        let dedicated = ShedDepth::Dedicated as u8;
        assert!(endpoint_matches(WalEndpoint::MainFeed, inline));
        assert!(!endpoint_matches(WalEndpoint::MainFeed, dedicated));
        assert!(endpoint_matches(WalEndpoint::Depth20, dedicated));
        assert!(endpoint_matches(WalEndpoint::Depth200, dedicated));
        assert!(!endpoint_matches(WalEndpoint::Depth20, inline));
        assert!(!endpoint_matches(WalEndpoint::OrderUpdate, SHED_DEPTH_ALL));
    }

    /// The production signals read real state: the clock is a second of the
    /// day, and the disk probe measures a real directory as a fraction and an
    /// absent one as unmeasured.
    #[test]
    fn test_live_signals_read_a_valid_clock_and_a_real_disk() {
        let signals = live_signals();
        assert!((signals.now_secs_ist)() < 86_400);
        let free = (signals.disk_free_fraction)(&std::env::temp_dir());
        assert!(free.is_some_and(|f| (0.0..=1.0).contains(&f)), "{free:?}");
        let missing =
            std::env::temp_dir().join(format!("tv-deferred-no-such-dir-{}", std::process::id()));
        assert_eq!((signals.disk_free_fraction)(&missing), None);
    }

    /// A cancelled supervisor exits at once and never opens a writer.
    #[tokio::test]
    async fn test_spawn_after_close_supervisor_exits_when_cancelled() {
        let questdb = tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let cancel = std::sync::Arc::new(AtomicBool::new(true));
        let handle = spawn_after_close_supervisor(questdb, std::env::temp_dir(), cancel, None);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("a cancelled supervisor must exit promptly")
            .expect("the supervisor task must not panic");
    }
}
