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
//!   flooded QuestDB on 2026-09-29. 15:45–17:15 IST is the quietest window the
//!   box has, and it ends a quarter hour before the 17:30 stop.
//! * **On its own thread with its own depth writer.** The live drain's writer
//!   mixes rows into shared ILP batches; a failed batch marks its whole
//!   sequence range unapplied, and an hours-old row beside a live one would
//!   span dozens of buckets. A separate synchronous writer keeps every batch to
//!   one bucket and gives this pass a direct acknowledgement.
//! * **Paced.** It pauses while any QuestDB table's WAL apply lag is growing,
//!   while the applied watermark distrusts the sink, and while the ingest shed
//!   still refuses the kind it would write. It stops at 17:15.
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
//! # Honest limits
//!
//! * A bucket still overlapping the segment the writer holds open is carried
//!   to the next day (segments rotate by size, so the last bucket of a session
//!   usually is).
//! * A frame recorded without a receipt time (a pre-TVW3 record) cannot be
//!   rewritten onto the row the live drain wrote, so it is counted and left.
//! * A segment damaged mid-file loses what follows the damage; that is counted
//!   and coded, and the mark is cleared because nothing more can be read.
//! * If the rescue of a failed batch ALSO fails, the depth writer marks that
//!   batch's range unapplied as it always does, and the next boot re-folds one
//!   bucket in full. Bounded to one bucket by flushing at every bucket edge.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tickvault_common::error_code::ErrorCode;
use tickvault_core::parser::ParsedFrame;
use tickvault_core::parser::depth::DepthFeedKind;
use tickvault_core::parser::dispatcher::{dispatch_frame, main_feed_packet_len};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
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
/// 17:15 IST — the pass stops starting new work.
pub const AFTER_CLOSE_DEPTH_STOP_SECS_IST: u64 = 17 * 3_600 + 15 * 60;
/// 17:30 IST — the scheduled box stop (`main.tf` daily_stop). Stated here only
/// to prove the window ends in time; the stop itself is terraform's.
const BOX_STOP_SECS_IST: u64 = 17 * 3_600 + 30 * 60;
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

// The settle must fit between the stop and the box stop.
const _: () =
    assert!(AFTER_CLOSE_DEPTH_STOP_SECS_IST + AFTER_CLOSE_SETTLE.as_secs() < BOX_STOP_SECS_IST);
const _: () = assert!(AFTER_CLOSE_DEPTH_START_SECS_IST < AFTER_CLOSE_DEPTH_STOP_SECS_IST);

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

/// Counter: rows this pass wrote back, and rows it refused.
pub const DEFERRED_DEPTH_ROWS_COUNTER: &str = "tv_wal_deferred_depth_rows_total";
/// Counter: per-bucket outcomes of the pass.
pub const DEFERRED_DEPTH_PASS_COUNTER: &str = "tv_wal_deferred_depth_pass_total";

/// Why the pass stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassStop {
    /// Before 15:45 IST — nothing is started.
    BeforeWindow,
    /// 17:15 IST reached.
    Deadline,
    /// The lane is shutting down.
    Cancelled,
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
/// Cancellation and the window win over everything; inside the window the
/// pass pauses while QuestDB is falling behind (`lag_growing_tables > 0`),
/// while the applied watermark distrusts the sink, or while the ingest shed
/// still refuses the depth kind being written (`shed_blocks`) — writing back
/// under disk pressure would defeat the shed that protects the disk.
#[must_use]
pub const fn after_close_step(
    secs_ist: u64,
    lag_growing_tables: u32,
    sink_suspect: bool,
    shed_blocks: bool,
    cancelled: bool,
) -> PassStep {
    if cancelled {
        return PassStep::Stop(PassStop::Cancelled);
    }
    if secs_ist < AFTER_CLOSE_DEPTH_START_SECS_IST {
        return PassStep::Stop(PassStop::BeforeWindow);
    }
    if secs_ist >= AFTER_CLOSE_DEPTH_STOP_SECS_IST {
        return PassStep::Stop(PassStop::Deadline);
    }
    if lag_growing_tables > 0 || sink_suspect || shed_blocks {
        return PassStep::Pause;
    }
    PassStep::Run
}

/// Seconds until the next window opens, from `secs_ist`. `0` inside the
/// window. O(1).
#[must_use]
pub const fn secs_until_window(secs_ist: u64) -> u64 {
    if secs_ist < AFTER_CLOSE_DEPTH_START_SECS_IST {
        AFTER_CLOSE_DEPTH_START_SECS_IST - secs_ist
    } else if secs_ist < AFTER_CLOSE_DEPTH_STOP_SECS_IST {
        0
    } else {
        86_400 - secs_ist + AFTER_CLOSE_DEPTH_START_SECS_IST
    }
}

/// The live signals the pass paces on, injectable for tests.
pub struct PassSignals<'a> {
    pub now_secs_ist: &'a dyn Fn() -> u64,
    pub lag_growing_tables: &'a dyn Fn() -> u32,
    pub sink_suspect: &'a dyn Fn() -> bool,
    /// `true` while the ingest shed refuses any of `kinds`.
    pub shed_blocks: &'a dyn Fn(u8) -> bool,
    pub sleep: &'a dyn Fn(Duration),
}

/// The production signals.
#[must_use]
pub fn live_signals() -> PassSignals<'static> {
    PassSignals {
        now_secs_ist: &|| super::now_ist_secs_of_day(),
        lag_growing_tables: &|| tickvault_common::ingest_shed::wal_apply_lag_growing(),
        sink_suspect: &|| {
            tickvault_storage::wal_applied_watermark::applied_watermark().is_sink_suspect()
        },
        shed_blocks: &|kinds| {
            let shed = &tickvault_common::ingest_shed::INGEST_SHED;
            (kinds & ShedDepth::Inline as u8 != 0 && !shed.allows_inline_depth())
                || (kinds & ShedDepth::Dedicated as u8 != 0 && !shed.allows_dedicated_depth())
        },
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
    pub stop: Option<PassStop>,
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
fn wait_for_run(signals: &PassSignals<'_>, kinds: u8, cancel: &AtomicBool) -> Result<(), PassStop> {
    loop {
        match after_close_step(
            (signals.now_secs_ist)(),
            (signals.lag_growing_tables)(),
            (signals.sink_suspect)(),
            (signals.shed_blocks)(kinds),
            cancel.load(Ordering::Acquire),
        ) {
            PassStep::Run => return Ok(()),
            PassStep::Pause => (signals.sleep)(AFTER_CLOSE_PAUSE),
            PassStep::Stop(reason) => return Err(reason),
        }
    }
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
        for span in &spans {
            if span.lo == 0 || span.is_open {
                overflow_complete = false;
                continue;
            }
            let Some(hi) = span.hi else {
                // An unbounded segment cannot be swept completely.
                overflow_complete = false;
                continue;
            };
            if span.lo > snap.overflow_high_seq {
                continue;
            }
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
    info!(
        buckets = work.len(),
        overflowed = snap.overflowed,
        "after-close deferred-depth pass starting — writing back order-book rows the ingest \
         shed skipped during the session"
    );

    let c = pass_counters();
    let pass_counter = |outcome: &'static str| {
        metrics::counter!(DEFERRED_DEPTH_PASS_COUNTER, "outcome" => outcome).increment(1);
    };
    let mut done: Vec<(u64, u8)> = Vec::with_capacity(work.len()); // APPROVED: cold

    for &(id, kinds) in &work {
        if let Err(reason) = wait_for_run(signals, kinds, cancel) {
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
                summary.rows_written = summary.rows_written.saturating_add(rows);
                if sink.flush_due() && sink.flush().is_err() {
                    failed = true;
                    break 'segments;
                }
            }
        }
        // Flush at every bucket edge so no ILP batch spans two buckets.
        if !failed && sink.flush().is_err() {
            failed = true;
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
            if (signals.now_secs_ist)()
                >= AFTER_CLOSE_DEPTH_STOP_SECS_IST + AFTER_CLOSE_SETTLE.as_secs()
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

/// Spawns the daily after-close supervisor. It sleeps until 15:45 IST, runs
/// one pass on its own OS thread with its own synchronous depth writer, then
/// sleeps until the next window. `cancel` stops it (set by the lane when its
/// drain ends); it never waits on the lane's shutdown `Notify`, which wakes a
/// single waiter and would be stolen from the drain.
///
/// `book_since` is the lane's array-row start instant (plan item 49e step 3),
/// passed rather than re-read so the pass routes every frame exactly as the
/// lane does: a frame received before it is rewritten into `market_depth`,
/// one at or after it into `market_depth_book`.
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
                tokio::time::sleep(Duration::from_secs(wait)).await;
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                continue;
            }
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            let questdb = questdb.clone();
            let wal_dir = wal_dir.clone();
            let thread_cancel = std::sync::Arc::clone(&cancel);
            let spawned = std::thread::Builder::new()
                .name("tv-deferred-depth".to_string())
                .spawn(move || {
                    let mut sink = DepthIngest::new(&questdb, book_since);
                    let summary = run_after_close_pass(
                        &mut sink,
                        tickvault_storage::wal_deferred_depth::deferred_depth(),
                        &wal_dir,
                        &thread_cancel,
                        &live_signals(),
                    );
                    drop(tx.send(summary));
                });
            if let Err(err) = spawned {
                error!(
                    code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                    source = "deferred_depth_thread_spawn_failed",
                    error = %err,
                    "after-close depth pass thread could not start — skipped order-book rows \
                     stay marked and on disk until a later pass"
                );
            } else {
                drop(rx.await);
            }
            // Sleep past the window so one day runs one pass.
            let rest = super::now_ist_secs_of_day();
            let until_close = AFTER_CLOSE_DEPTH_STOP_SECS_IST.saturating_sub(rest);
            let mut remaining = until_close;
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

    #[test]
    fn after_close_step_over_the_full_grid() {
        use PassStep::{Pause, Run, Stop};
        let before = AFTER_CLOSE_DEPTH_START_SECS_IST - 1;
        let at_stop = AFTER_CLOSE_DEPTH_STOP_SECS_IST;
        assert_eq!(after_close_step(IN_WINDOW, 0, false, false, false), Run);
        assert_eq!(after_close_step(IN_WINDOW, 1, false, false, false), Pause);
        assert_eq!(after_close_step(IN_WINDOW, 0, true, false, false), Pause);
        assert_eq!(after_close_step(IN_WINDOW, 0, false, true, false), Pause);
        assert_eq!(
            after_close_step(before, 0, false, false, false),
            Stop(PassStop::BeforeWindow)
        );
        assert_eq!(
            after_close_step(at_stop, 0, false, false, false),
            Stop(PassStop::Deadline)
        );
        // Cancellation wins over everything, including a paused state.
        assert_eq!(
            after_close_step(IN_WINDOW, 5, true, true, true),
            Stop(PassStop::Cancelled)
        );
    }

    proptest::proptest! {
        /// Outside the window the pass never runs; inside, it runs only when
        /// every pressure signal is clear.
        #[test]
        fn after_close_pass_stops_before_the_stop_time(
            secs in 0u64..86_400,
            lag in 0u32..4,
            suspect in proptest::bool::ANY,
            shed in proptest::bool::ANY,
        ) {
            let step = after_close_step(secs, lag, suspect, shed, false);
            let in_window = (AFTER_CLOSE_DEPTH_START_SECS_IST..AFTER_CLOSE_DEPTH_STOP_SECS_IST).contains(&secs);
            if !in_window {
                proptest::prop_assert!(matches!(step, PassStep::Stop(_)));
            } else if lag > 0 || suspect || shed {
                proptest::prop_assert_eq!(step, PassStep::Pause);
            } else {
                proptest::prop_assert_eq!(step, PassStep::Run);
            }
        }
    }

    #[test]
    fn secs_until_window_is_zero_inside_and_wraps_to_tomorrow() {
        assert_eq!(secs_until_window(IN_WINDOW), 0);
        assert_eq!(
            secs_until_window(AFTER_CLOSE_DEPTH_START_SECS_IST - 100),
            100
        );
        assert_eq!(
            secs_until_window(AFTER_CLOSE_DEPTH_STOP_SECS_IST),
            86_400 - AFTER_CLOSE_DEPTH_STOP_SECS_IST + AFTER_CLOSE_DEPTH_START_SECS_IST
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

    /// Plan item 49e step 3 (review fix 2026-10-09): with array rows on, the
    /// after-close pass routes each written-back frame by its recorded
    /// receipt, exactly as the live drain did: at or after the start instant
    /// to `market_depth_book`, before it to `market_depth`.
    #[test]
    fn test_rewrite_inline_depth_routes_by_receipt_with_array_rows_on() {
        let frame = full_packet(13);
        let (seq, rx) = in_session();

        let mut book = DepthIngest::for_test_book(rx);
        let rows = rewrite_inline_depth(&mut book, &frame, seq, rx, pass_counters());
        assert_eq!(rows, 10, "5 levels x 2 sides, counted in levels");
        assert!(!book.pending_book_bytes().is_empty(), "array rows written");
        assert_eq!(
            book.book.as_ref().map(|b| b.levels.pending()),
            Some(0),
            "nothing on the older-frame writer"
        );

        let mut older = DepthIngest::for_test_book(rx + 1);
        let rows = rewrite_inline_depth(&mut older, &frame, seq, rx, pass_counters());
        assert_eq!(rows, 10);
        assert!(older.pending_book_bytes().is_empty(), "no array row");
        assert_eq!(older.pending_rows(), 10, "ten level rows");
        assert!(older.pending_ilp().contains("market_depth,"));
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
            sink_suspect: &|| false,
            shed_blocks: &|_| false,
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
            sink_suspect: &|| false,
            shed_blocks: &|_| false,
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
            sink_suspect: &|| false,
            shed_blocks: &|_| false,
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
        let dir = std::env::temp_dir().join(format!(
            "tv-deferred-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let rx: i64 = 1_780_000_000 * 1_000_000_000; // 01:56 IST
        let seq = u64::try_from(rx).unwrap() >> 17 << 17;
        let wal = tickvault_storage::ws_frame_spill::WsFrameSpill::new(&dir).expect("wal");
        wal.append_with_seq_at(
            WsType::LiveFeed,
            full_packet(13),
            seq,
            rx,
            WalEndpoint::MainFeed,
        );
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
            sink_suspect: &|| false,
            shed_blocks: &|_| false,
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
            clock.set(clock.get() + d.as_secs());
        };
        let signals = PassSignals {
            now_secs_ist: &|| clock.get(),
            // Healthy while writing, lagging from the settle onwards.
            lag_growing_tables: &|| u32::from(settling.get()),
            sink_suspect: &|| false,
            shed_blocks: &|_| false,
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
        assert!(
            clock.get()
                <= AFTER_CLOSE_DEPTH_STOP_SECS_IST
                    + AFTER_CLOSE_SETTLE.as_secs()
                    + AFTER_CLOSE_PAUSE.as_secs(),
            "the settle wait is bounded by the deadline"
        );
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

    /// The production signals read real state and never block: the clock is
    /// a second of the day, and an empty kinds mask is never blocked.
    #[test]
    fn test_live_signals_read_a_valid_clock_and_an_empty_mask_never_blocks() {
        let signals = live_signals();
        assert!((signals.now_secs_ist)() < 86_400);
        assert!(!(signals.shed_blocks)(0));
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
