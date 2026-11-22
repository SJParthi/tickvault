//! The rebuild and the verify gate of the one-off repair of 2026-11-20
//! (daily-universe Quote 63, plan ITEM UTC-REPAIR).
//!
//! **Rebuild.** The day's raw WAL segments come from the cold bucket's
//! `raw-frames/2026-11-20/` copy (plus the previous date's newest segment,
//! which can hold the first frames of the day), one segment at a time into a
//! staging folder this run creates; each is read, removed, and its main-feed
//! frames RECEIVED on the day are folded through the normal live ingest
//! (`LiveIngest::refold_past_day_segment`, `dhan_feed_stack::day_rebuild`).
//! The permanent UTC trade-time rule corrects every frame in the dispatcher
//! from its WAL receipt, so no correction record is involved. Rows go to
//! QuestDB synchronously (no offload threads: an offline job, one round trip
//! per batch), sealed candles through a seal writer this run owns on its own
//! thread, paced so its queue never fills (a full queue spills, and a spill
//! fails the verify).
//!
//! **Verify gate**, after QuestDB applied every written transaction: the
//! NIFTY index 1-minute candles cover 09:15 to at least 15:29 and no candle
//! sits before 09:15; no tick sits before 09:00 beyond the prints the rule
//! refused (the old ~19,800 s offset); the rows in `ticks` + `tick_quotes`
//! and `feed_aux_packets` cover what the rebuild wrote; no spill or DLQ file
//! appeared and every seal flush landed.
//!
//! O(frames + packets + rows) for the rebuild, O(tables) per verify probe;
//! offline (the app is stopped). Never on a live path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, bail};
use chrono::{NaiveDate, TimeZone as _};
use tickvault_common::config::ApplicationConfig;
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_storage::ws_frame_spill::{ReplayedFrame, WalEndpoint};
use tracing::{error, info};

use crate::dhan_feed_stack::{LiveIngest, WalRefoldOutcome};

/// Frames folded between two seal-queue checks. Small enough that the
/// sealed candles of one chunk fit in the queue's free room.
pub const CHUNK_FRAMES: usize = 10_000;

/// The seal queue depth the rebuild waits under before the next chunk.
pub const SEAL_PACE_HIGH: usize = 50_000;

/// A receipt silence longer than this between two main-feed frames inside
/// the continuous session (09:15 to 15:30 IST) is a restart or an outage on
/// the day: the next frame is folded after a replay gap, so no bar takes the
/// downtime's volume (undercount, never overcount). Outside the continuous
/// session frames can legitimately pause, so no gap is inferred there.
pub const RESTART_SILENCE_SECS: i64 = 10;

/// 09:15 IST, seconds of day.
const CONTINUOUS_OPEN_SECS: i64 = 9 * 3_600 + 15 * 60;
/// 15:30 IST, seconds of day.
const CONTINUOUS_CLOSE_SECS: i64 = 15 * 3_600 + 30 * 60;
/// 16:05 IST, seconds of day: the end-of-day catch-up seal cutoff, past
/// every bucket of the day (the persistence window ends at 15:40).
pub const CLOSE_SEAL_CUTOFF_SECS: u32 = 16 * 3_600 + 5 * 60;

/// Seals one seal-writer cycle may drain (the live writer's value).
const SEAL_MAX_DRAIN_PER_CYCLE: usize = 16_384;
/// Cycles the final seal drain may take before it gives up (10 minutes).
const SEAL_FINAL_DRAIN_MAX_CYCLES: u32 = 6_000;
/// Pause between seal cycles that found nothing to do.
const SEAL_IDLE_PAUSE: std::time::Duration = std::time::Duration::from_millis(100); // APPROVED: this IS the named constant the rule asks for
/// Pause while waiting for the seal queue to drain under the pace mark.
const SEAL_PACE_PAUSE: std::time::Duration = std::time::Duration::from_millis(20); // APPROVED: this IS the named constant the rule asks for

/// `[start, end)` of `day` (IST) as UTC epoch nanoseconds, the unit of a
/// WAL receipt stamp. O(1).
///
/// # Errors
/// The day does not map to a single IST instant (cannot happen for IST).
// TEST-EXEMPT: covered by test_day_bounds_match_the_utc_rule_cutover
pub fn day_receipt_bounds_nanos(day: NaiveDate) -> anyhow::Result<(i64, i64)> {
    let ist = chrono::FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).context("IST offset")?;
    let midnight = day.and_hms_opt(0, 0, 0).context("midnight")?;
    let start = ist
        .from_local_datetime(&midnight)
        .single()
        .context("IST midnight")?
        .timestamp();
    let start_nanos = start
        .checked_mul(1_000_000_000)
        .context("receipt bound overflow")?;
    Ok((start_nanos, start_nanos.saturating_add(86_400_000_000_000)))
}

/// 00:00 IST of `day` as fold seconds (epoch seconds plus the IST offset),
/// the unit of the aggregator's watermark. O(1).
///
/// # Errors
/// As [`day_receipt_bounds_nanos`], or a day past 2106.
// TEST-EXEMPT: covered by test_day_bounds_match_the_utc_rule_cutover
pub fn day_start_fold_secs(day: NaiveDate) -> anyhow::Result<u32> {
    let (start_nanos, _) = day_receipt_bounds_nanos(day)?;
    let secs = start_nanos / 1_000_000_000 + i64::from(IST_UTC_OFFSET_SECONDS);
    u32::try_from(secs).context("day start does not fit fold seconds")
}

/// A receipt in IST seconds of day. O(1).
#[must_use]
// TEST-EXEMPT: covered by test_restart_silence_only_inside_the_continuous_session.
pub fn receipt_ist_secs_of_day(receipt_nanos: i64) -> i64 {
    (receipt_nanos.div_euclid(1_000_000_000) + i64::from(IST_UTC_OFFSET_SECONDS)).rem_euclid(86_400)
}

/// Where a frame's receipt falls against the repaired day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptDay {
    /// Before the day (the previous date's segment): not folded.
    Before,
    /// On the day: folded.
    On,
    /// After the day (21 Nov 00:00 onward): refused, counted (Quote 63
    /// covers 20 Nov only).
    After,
}

/// Classifies a receipt. O(1).
#[must_use]
pub fn receipt_day(receipt_nanos: i64, bounds: (i64, i64)) -> ReceiptDay {
    if receipt_nanos < bounds.0 {
        ReceiptDay::Before
    } else if receipt_nanos < bounds.1 {
        ReceiptDay::On
    } else {
        ReceiptDay::After
    }
}

/// Whether a silence between two receipts on the day reads as a restart or
/// an outage: longer than [`RESTART_SILENCE_SECS`], with both ends inside
/// the continuous session. O(1).
#[must_use]
// TEST-EXEMPT: covered by test_restart_silence_only_inside_the_continuous_session
pub fn is_restart_silence(prev_receipt_nanos: i64, next_receipt_nanos: i64) -> bool {
    let gap_secs = next_receipt_nanos.saturating_sub(prev_receipt_nanos) / 1_000_000_000;
    let in_session = |r: i64| {
        let s = receipt_ist_secs_of_day(r);
        (CONTINUOUS_OPEN_SECS..CONTINUOUS_CLOSE_SECS).contains(&s)
    };
    gap_secs > RESTART_SILENCE_SECS
        && in_session(prev_receipt_nanos)
        && in_session(next_receipt_nanos)
}

/// The day's frames of one segment, ready to fold, and where the gaps are.
#[derive(Debug, Default)]
pub struct DayFrames {
    /// `(frame_seq, receipt, endpoint, bytes)` of the frames received on the day.
    pub frames: Vec<(u64, i64, WalEndpoint, bytes::Bytes)>,
    /// Ascending indexes into `frames` of frames that follow a gap.
    pub gaps: Vec<usize>,
    /// Frames received before the day (not folded).
    pub before: u64,
    /// Frames received after the day (refused, counted).
    pub after: u64,
}

/// Keeps the frames of `read` received on the day and marks the gaps:
/// a frame the reader resynced to (`after_gap`), the first kept frame after
/// a segment whose tail was unread (`pending_gap`), and a restart silence.
/// `last_receipt` carries the previous kept frame's receipt across segments.
/// O(frames).
pub fn select_day_frames(
    read: Vec<ReplayedFrame>,
    bounds: (i64, i64),
    pending_gap: bool,
    last_receipt: &mut Option<i64>,
) -> DayFrames {
    let mut out = DayFrames::default();
    let mut gap_next = pending_gap;
    for f in read {
        // The live drain's own rule: a Dhan live-feed record from the main-feed
        // socket. A TrueData or order-update record shares the endpoint byte
        // and carries no tick, so it is skipped (never walked as Dhan binary).
        if !tickvault_storage::ws_frame_spill::frame_feeds_the_candle_fold(f.ws_type, f.endpoint) {
            continue;
        }
        match receipt_day(f.received_at_nanos, bounds) {
            ReceiptDay::Before => {
                out.before = out.before.saturating_add(1);
                continue;
            }
            ReceiptDay::After => {
                out.after = out.after.saturating_add(1);
                continue;
            }
            ReceiptDay::On => {}
        }
        let silence = last_receipt.is_some_and(|p| is_restart_silence(p, f.received_at_nanos));
        if gap_next || f.after_gap || silence {
            out.gaps.push(out.frames.len());
            gap_next = false;
        }
        *last_receipt = Some(f.received_at_nanos);
        out.frames.push((
            f.frame_seq,
            f.received_at_nanos,
            f.endpoint,
            bytes::Bytes::from(f.frame),
        ));
    }
    out
}

/// The gap indexes that fall inside `[from, to)`, rebased to `from`. O(gaps).
#[must_use]
pub fn chunk_gaps(gaps: &[usize], from: usize, to: usize) -> Vec<usize> {
    gaps.iter()
        .filter(|&&g| g >= from && g < to)
        .map(|&g| g - from)
        .collect()
}

/// Everything the rebuild counted; written into the progress file so the
/// verify (a separate invocation) can read it back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
    pub segments: u64,
    pub damaged_segments: u64,
    pub frames: u64,
    pub frames_before_day: u64,
    pub frames_after_day: u64,
    pub gaps: u64,
    pub refolded: u64,
    pub tick_rows: u64,
    pub quote_rows: u64,
    pub aux_rows: u64,
    pub lost: u64,
    pub wrong_day: u64,
    pub undecodable: u64,
    pub unparseable: u64,
    pub abandoned_bytes: u64,
    pub depth_frames: u64,
    pub seals_emitted: u64,
    pub seals_dropped: u64,
    pub seals_rescued: u64,
    pub seal_rows_flushed: u64,
    pub seal_spilled: u64,
    pub seal_dlq: u64,
    pub seal_lost: u64,
    pub seal_flush_failed: u64,
    pub seal_residue: u64,
    pub new_spill_files: u64,
    pub ltt_refused: u64,
}

macro_rules! report_fields {
    ($m:ident) => {
        $m!(
            segments,
            damaged_segments,
            frames,
            frames_before_day,
            frames_after_day,
            gaps,
            refolded,
            tick_rows,
            quote_rows,
            aux_rows,
            lost,
            wrong_day,
            undecodable,
            unparseable,
            abandoned_bytes,
            depth_frames,
            seals_emitted,
            seals_dropped,
            seals_rescued,
            seal_rows_flushed,
            seal_spilled,
            seal_dlq,
            seal_lost,
            seal_flush_failed,
            seal_residue,
            new_spill_files,
            ltt_refused
        )
    };
}

impl RebuildReport {
    /// `key=value;...`, every field. O(fields).
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        macro_rules! put {
            ($($f:ident),*) => {
                $(
                    if !out.is_empty() {
                        out.push(';');
                    }
                    out.push_str(stringify!($f));
                    out.push('=');
                    out.push_str(&self.$f.to_string());
                )*
            };
        }
        report_fields!(put);
        out
    }

    /// Parses [`Self::encode`]'s text; `None` when any field is missing.
    #[must_use]
    pub fn decode(detail: &str) -> Option<Self> {
        let mut r = Self::default();
        macro_rules! get {
            ($($f:ident),*) => {
                $(
                    r.$f = super::state::detail_u64(detail, stringify!($f))?;
                )*
            };
        }
        report_fields!(get);
        Some(r)
    }

    /// Why the rebuild itself is not finished (empty when it is): a lost
    /// tick, a seal the ingest could not hand over, a seal flush that did not
    /// land, a new spill or DLQ file. Pure.
    #[must_use]
    pub fn unfinished_reasons(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut need_zero = |name: &str, v: u64| {
            if v > 0 {
                out.push(format!("{name}={v}"));
            }
        };
        need_zero("seals_dropped", self.seals_dropped);
        need_zero("seals_rescued", self.seals_rescued);
        need_zero("seal_spilled", self.seal_spilled);
        need_zero("seal_dlq", self.seal_dlq);
        need_zero("seal_lost", self.seal_lost);
        need_zero("seal_flush_failed", self.seal_flush_failed);
        need_zero("seal_residue", self.seal_residue);
        need_zero("new_spill_files", self.new_spill_files);
        // Frames the rebuild could not read or fold (2026-11-21 review): the
        // original rows are dropped, so a skipped stretch is lost data.
        need_zero("damaged_segments", self.damaged_segments);
        need_zero("abandoned_bytes", self.abandoned_bytes);
        need_zero("unparseable", self.unparseable);
        need_zero("lost", self.lost);
        if self.frames == 0 {
            out.push("no frame of the day was folded".to_owned());
        }
        out
    }
}

/// Every file under `data/spill` and `data/dlq` (recursively). Taken before
/// and after the rebuild: a file that appeared is a rescue the verify fails
/// on. O(files).
#[must_use]
pub fn spill_files(root: &Path) -> BTreeSet<PathBuf> {
    fn walk(dir: &Path, out: &mut BTreeSet<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.insert(p);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&root.join("data/spill"), &mut out);
    walk(&root.join("data/dlq"), &mut out);
    out
}

/// What the seal writer thread did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SealTotals {
    rows_flushed: u64,
    spilled: u64,
    dlq: u64,
    lost: u64,
    flush_failed: u64,
    residue: u64,
}

/// The repair's own seal writer: a `SealWriterRunner` cycled on a std
/// thread until stopped, then drained until nothing is held (bounded).
struct SealThread {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<SealTotals>,
}

impl SealThread {
    fn spawn(config: &ApplicationConfig) -> anyhow::Result<Self> {
        use tickvault_storage::seal_writer_runner::{
            SealWriterRunner, set_global_seal_overflow, set_global_seal_sender,
        };
        let encoder = if config.tick_storage.own_ilp_encoder {
            tickvault_storage::tick_ilp::TickIlpEncoder::Own
        } else {
            tickvault_storage::tick_ilp::TickIlpEncoder::Library
        };
        let mut runner =
            SealWriterRunner::new_with_encoder(&config.questdb, SEAL_MAX_DRAIN_PER_CYCLE, encoder)
                .map_err(|e| anyhow::anyhow!("seal writer: {e:?}"))?;
        if !set_global_seal_sender(runner.sender()) {
            bail!("a seal sender is already installed in this process");
        }
        if !set_global_seal_overflow(runner.overflow()) {
            bail!("a seal overflow is already installed in this process");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_seen = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("tv-day-repair-seal".to_owned())
            .spawn(move || {
                let mut t = SealTotals::default();
                let mut final_cycles = 0u32;
                loop {
                    let o = runner.run_one_cycle(chrono::Utc::now().timestamp());
                    t.spilled = t.spilled.saturating_add(
                        (o.mpsc_submit_spilled
                            .saturating_add(o.drain.rescued_to_spill))
                            as u64,
                    );
                    t.dlq = t.dlq.saturating_add(
                        (o.mpsc_submit_dlq.saturating_add(o.drain.rescued_to_dlq)) as u64,
                    );
                    t.lost = t.lost.saturating_add(
                        (o.mpsc_submit_dropped
                            .saturating_add(o.drain.rescued_dropped))
                            as u64,
                    );
                    if o.drain.ring_seals_popped > 0 {
                        if o.drain.flushed_ok {
                            t.rows_flushed = t
                                .rows_flushed
                                .saturating_add(o.drain.ring_seals_popped as u64);
                        } else {
                            t.flush_failed = t.flush_failed.saturating_add(1);
                        }
                    }
                    if stop_seen.load(Ordering::Acquire) {
                        if runner.unwritten_seals() == 0 && o.is_idle() {
                            break;
                        }
                        final_cycles = final_cycles.saturating_add(1);
                        if final_cycles >= SEAL_FINAL_DRAIN_MAX_CYCLES {
                            t.residue = runner.unwritten_seals() as u64;
                            break;
                        }
                    }
                    if o.is_idle() {
                        std::thread::sleep(SEAL_IDLE_PAUSE);
                    }
                }
                t
            })
            .context("spawn the seal writer thread")?;
        Ok(Self { stop, handle })
    }

    fn alive(&self) -> bool {
        !self.handle.is_finished()
    }

    fn finish(self) -> anyhow::Result<SealTotals> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("the seal writer thread panicked"))
    }
}

/// Runs `f` off the async worker when the runtime allows it (multi-thread),
/// inline otherwise (the shape of `dhan_feed_stack::blocking_flush`).
fn off_runtime<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Waits while the seal queue is above [`SEAL_PACE_HIGH`]. Fails when the
/// seal thread died (nothing would drain it).
fn pace_seals(seals: &SealThread) -> anyhow::Result<()> {
    loop {
        let queued =
            tickvault_storage::seal_writer_runner::global_seal_sender().map_or(0, |s| s.len());
        if queued <= SEAL_PACE_HIGH {
            return Ok(());
        }
        if !seals.alive() {
            bail!("the seal writer thread stopped with {queued} seals queued");
        }
        std::thread::sleep(SEAL_PACE_PAUSE);
    }
}

/// Rebuilds `day` from the cold bucket's raw frames. See the module docs.
///
/// `root` is the app folder (`data/` lives under it); `staging` a new or
/// empty folder this run owns (segments are downloaded and removed there).
///
/// # Errors
/// Any failure to name the day's contracts, key the tables, start the seal
/// writer, list or download a segment, or read one. Rows written before the
/// failure stay (they upsert onto themselves on the next run).
// TEST-EXEMPT: network and S3 orchestration; its pieces (day filter, gaps, chunks, report, refold) are unit-tested here and in dhan_feed_stack::day_rebuild.
pub async fn rebuild_day(
    config: &ApplicationConfig,
    day: NaiveDate,
    root: &Path,
    staging: &Path,
) -> anyhow::Result<RebuildReport> {
    let day_text = day.format("%Y-%m-%d").to_string();
    // The day's own contract names, never an earlier or later day's.
    let labels = crate::dhan_contract_universe::contract_names_for_day_strict(&day_text)?;
    let names = tickvault_storage::candle_contract_labels::publish_candle_contract_labels(labels);
    if !crate::candle_ddl_boot::run_live_table_ddl_at_boot(&config.questdb).await {
        bail!("ticks / feed_aux_packets could not be keyed; refusing to write rows");
    }
    crate::candle_ddl_boot::run_candle_ddl_at_boot(&config.questdb).await;
    let s3 = tickvault_storage::s3_cold::S3Cold::load(&config.partition_retention.archive_bucket)
        .await
        .context("no cold bucket resolves (TV_ENVIRONMENT)")?;
    let listing = tickvault_storage::raw_frame_fetch::list_raw_frame_day(&s3, &day_text).await?;
    if !listing.sidecars.is_empty() {
        bail!(
            "the bucket holds a second, different copy of a segment ({}); refusing to pick one",
            listing.sidecars.join(", ")
        );
    }
    if listing.day_segments == 0 {
        bail!("no raw segment under raw-frames/{day_text}/");
    }
    info!(
        day = %day_text,
        segments = listing.keys.len(),
        bytes = listing.total_bytes,
        contract_names = names,
        "day repair: rebuilding the day from its raw frames"
    );
    std::fs::create_dir_all(staging).with_context(|| format!("create {}", staging.display()))?;
    let spill_before = spill_files(root);
    let refused_before = ltt_refused_total();
    let bounds = day_receipt_bounds_nanos(day)?;
    let day_start = day_start_fold_secs(day)?;

    let seals = SealThread::spawn(config)?;
    let encoder = if config.tick_storage.own_ilp_encoder {
        tickvault_storage::tick_ilp::TickIlpEncoder::Own
    } else {
        tickvault_storage::tick_ilp::TickIlpEncoder::Library
    };
    let slots = tickvault_trading::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS;
    let mut ingest = LiveIngest::new(
        tickvault_storage::tick_persistence::TickWriter::new_with_encoder(
            &config.questdb,
            Feed::Dhan,
            encoder,
        ),
        slots,
    )
    .with_detector_capacity(slots)
    .with_fold_capacity(slots);

    let mut report = RebuildReport::default();
    let mut out = WalRefoldOutcome::default();
    let mut begun = false;
    let mut pending_gap = false;
    let mut last_receipt: Option<i64> = None;
    let folded = async {
        for (key, _) in &listing.keys {
            // Stop at 08:00 IST on a session day (the app must start on
            // time); rows written so far upsert onto themselves next run.
            super::window_open_now(config)?;
            let wal =
                tickvault_storage::raw_frame_fetch::fetch_raw_frame_segment(&s3, key, staging)
                    .await
                    .with_context(|| format!("fetch {key}"))?;
            let read =
                tickvault_storage::ws_frame_spill::read_segment_frames_matching(&wal, &|_, ep| {
                    ep == WalEndpoint::MainFeed
                });
            let removed = std::fs::remove_file(&wal);
            let read = read.with_context(|| format!("read {}", wal.display()))?;
            removed.with_context(|| format!("remove {}", wal.display()))?;
            report.segments = report.segments.saturating_add(1);
            let tail_unread = read.stopped_early || read.trailing_gap;
            if read.damaged {
                report.damaged_segments = report.damaged_segments.saturating_add(1);
            }
            let kept = select_day_frames(read.frames, bounds, pending_gap, &mut last_receipt);
            pending_gap = tail_unread;
            report.frames_before_day = report.frames_before_day.saturating_add(kept.before);
            report.frames_after_day = report.frames_after_day.saturating_add(kept.after);
            if kept.frames.is_empty() {
                continue;
            }
            if !begun {
                let capture =
                    kept.frames[0].1.div_euclid(1_000_000_000) + i64::from(IST_UTC_OFFSET_SECONDS);
                ingest
                    .begin_past_day_refold(day_start, u32::try_from(capture).unwrap_or(day_start));
                begun = true;
            }
            report.frames = report.frames.saturating_add(kept.frames.len() as u64);
            report.gaps = report.gaps.saturating_add(kept.gaps.len() as u64);
            let mut from = 0usize;
            while from < kept.frames.len() {
                let to = from.saturating_add(CHUNK_FRAMES).min(kept.frames.len());
                let gaps = chunk_gaps(&kept.gaps, from, to);
                let chunk = &kept.frames[from..to];
                // The fold flushes through `blocking_flush`, which already leaves the
                // async worker for each round trip.
                ingest.refold_past_day_segment(chunk, &gaps, &mut out);
                off_runtime(|| pace_seals(&seals))?;
                from = to;
            }
        }
        anyhow::Ok(())
    }
    .await;
    // The end-of-day seals run whatever happened, so a partial run never
    // leaves bars held only in memory; a failed run is still not finished.
    let mut pace_err: Option<anyhow::Error> = None;
    if begun {
        let close =
            ingest.finish_past_day_refold(day_start.saturating_add(CLOSE_SEAL_CUTOFF_SECS), || {
                if pace_err.is_none()
                    && let Err(e) = off_runtime(|| pace_seals(&seals))
                {
                    pace_err = Some(e);
                }
            });
        info!(
            catch_up_bars = close.catch_up_bars,
            close_bars = close.close_bars,
            tainted = close.tainted,
            suppressed = close.suppressed,
            "day repair: end-of-day seals routed"
        );
    }
    let _flushed = ingest.flush_past_day_rows();
    let seal_totals = off_runtime(|| seals.finish())?;
    folded?;
    if let Some(e) = pace_err {
        return Err(e.context("the seal queue did not drain at the day's close"));
    }

    report.refolded = out.refolded;
    report.lost = out.lost;
    report.wrong_day = out.refused_wrong_day;
    report.undecodable = out.undecodable;
    report.unparseable = out.unparseable;
    report.abandoned_bytes = out.abandoned_bytes;
    report.depth_frames = out.depth_frames;
    let (tick_rows, quote_rows, aux_rows) = ingest.past_day_row_counts();
    report.tick_rows = tick_rows;
    report.quote_rows = quote_rows;
    report.aux_rows = aux_rows;
    report.seals_emitted = ingest.seals_emitted();
    report.seals_dropped = ingest.seals_dropped();
    report.seals_rescued = ingest.seals_rescued();
    report.seal_rows_flushed = seal_totals.rows_flushed;
    report.seal_spilled = seal_totals.spilled;
    report.seal_dlq = seal_totals.dlq;
    report.seal_lost = seal_totals.lost;
    report.seal_flush_failed = seal_totals.flush_failed;
    report.seal_residue = seal_totals.residue;
    let spill_after = spill_files(root);
    report.new_spill_files = spill_after.difference(&spill_before).count() as u64;
    report.ltt_refused = ltt_refused_total().saturating_sub(refused_before);
    Ok(report)
}

/// Every trade-time refusal so far in this process. O(1).
fn ltt_refused_total() -> u64 {
    let c = tickvault_storage::ltt_correction::refusal_counts();
    c.receipt.saturating_add(c.band).saturating_add(c.cap)
}

/// One `/exec` query; returns the dataset rows. O(rows).
///
/// # Errors
/// Transport, a non-2xx answer, or a body that is not the `/exec` JSON.
// TEST-EXEMPT: one HTTP round trip; first_count, which reads its answer, is unit-tested.
pub async fn exec_dataset(
    client: &reqwest::Client,
    exec_url: &str,
    sql: &str,
) -> anyhow::Result<Vec<Vec<serde_json::Value>>> {
    let resp = client
        .get(exec_url)
        .query(&[("query", sql)])
        .send()
        .await
        .with_context(|| format!("query {sql}"))?;
    let status = resp.status();
    let body = resp.text().await.context("read query body")?;
    if !status.is_success() {
        bail!("query {sql} returned {status}: {body}");
    }
    let v: serde_json::Value = serde_json::from_str(&body).context("query body is not JSON")?;
    let rows = v
        .get("dataset")
        .and_then(serde_json::Value::as_array)
        .context("query answer has no dataset")?;
    Ok(rows
        .iter()
        .map(|r| r.as_array().cloned().unwrap_or_default())
        .collect())
}

/// The first column of the first row as a count (0 when absent). O(1).
#[must_use]
pub fn first_count(rows: &[Vec<serde_json::Value>]) -> u64 {
    rows.first()
        .and_then(|r| r.first())
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_i64().map(|x| x.max(0).unsigned_abs()))
        })
        .unwrap_or(0)
}

/// One count query. O(1) round trips.
///
/// # Errors
/// As [`exec_dataset`].
// TEST-EXEMPT: exec_dataset plus first_count.
pub async fn exec_count(
    client: &reqwest::Client,
    exec_url: &str,
    sql: &str,
) -> anyhow::Result<u64> {
    Ok(first_count(&exec_dataset(client, exec_url, sql).await?))
}

/// Streams one QuestDB `/exp` answer (CSV: a header line, then one row per
/// line) to `take`, chunk by chunk, so tens of millions of rows never sit in
/// memory as text. A 429 (QuestDB's export connection limit) is waited out
/// and the same request sent again from scratch: 1, 2, 4, 8, then 15 s
/// steps, at most `EXPORT_BUSY_MAX_WAIT` in all; `take` has seen nothing of
/// a refused answer. Returns the data rows. O(rows).
///
/// # Errors
/// The request fails, QuestDB answers non-2xx (a 429 past the wait), or
/// `take` refuses a line.
pub async fn export_lines(
    client: &reqwest::Client,
    exp_url: &str,
    sql: &str,
    take: impl FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    export_csv(client, exp_url, sql, |_| Ok(()), take).await
}

/// [`export_lines`] that also hands the header line to `on_header` before
/// any row. O(rows).
///
/// # Errors
/// As [`export_lines`], or `on_header` refuses the header.
pub async fn export_csv(
    client: &reqwest::Client,
    exp_url: &str,
    sql: &str,
    mut on_header: impl FnMut(&str) -> anyhow::Result<()>,
    mut take: impl FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    let mut waited = std::time::Duration::ZERO;
    let mut step = EXPORT_BUSY_FIRST_WAIT;
    let mut resp = loop {
        let resp = client
            .get(exp_url)
            .query(&[("query", sql)])
            .send()
            .await
            .with_context(|| format!("export {sql}"))?;
        let status = resp.status();
        if status.is_success() {
            break resp;
        }
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 429 && waited < EXPORT_BUSY_MAX_WAIT {
            tokio::time::sleep(step).await;
            waited += step;
            step = (step * 2).min(EXPORT_BUSY_MAX_STEP);
            continue;
        }
        bail!("export {sql} returned {status}: {body}");
    };
    let mut pending: Vec<u8> = Vec::new();
    let mut header_seen = false;
    let mut rows = 0u64;
    let mut line_of = |line: &[u8]| -> anyhow::Result<()> {
        let text = std::str::from_utf8(line).context("export line is not UTF-8")?;
        let text = text.trim_end_matches(['\r', '\n']);
        if !header_seen {
            header_seen = true;
            return on_header(text);
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        rows += 1;
        take(text)
    };
    while let Some(chunk) = resp.chunk().await.context("read the export")? {
        pending.extend_from_slice(&chunk);
        let mut start = 0usize;
        while let Some(pos) = pending[start..].iter().position(|b| *b == b'\n') {
            line_of(&pending[start..start + pos])?;
            start += pos + 1;
        }
        pending.drain(..start);
    }
    if !pending.is_empty() {
        line_of(&pending)?;
    }
    Ok(rows)
}

/// How long [`export_lines`] waits out QuestDB's export connection limit.
const EXPORT_BUSY_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120); // APPROVED: this IS the named constant the rule asks for
/// The first wait after a 429 in [`export_lines`]; doubles each time.
const EXPORT_BUSY_FIRST_WAIT: std::time::Duration = std::time::Duration::from_secs(1); // APPROVED: this IS the named constant the rule asks for
/// The longest single wait after a 429 in [`export_lines`].
const EXPORT_BUSY_MAX_STEP: std::time::Duration = std::time::Duration::from_secs(15); // APPROVED: this IS the named constant the rule asks for

/// `SELECT capture_seq ...` through `/exp` into `out`. O(rows).
///
/// # Errors
/// As [`export_lines`], or a line that is not a number.
pub async fn export_seqs(
    client: &reqwest::Client,
    exp_url: &str,
    sql: &str,
    out: &mut Vec<u64>,
) -> anyhow::Result<u64> {
    export_lines(client, exp_url, sql, |line| {
        let text = line.trim().trim_matches('"');
        let v: u64 = text
            .parse()
            .with_context(|| format!("export value {text:?} is not a capture_seq"))?;
        out.push(v);
        Ok(())
    })
    .await
}

/// `SELECT DISTINCT security_id, segment ...` through `/exp` into `out` as
/// `security_id|segment` (the copy reader's contract key). O(rows).
///
/// # Errors
/// As [`export_lines`], or a line with fewer than two fields.
pub async fn export_contracts(
    client: &reqwest::Client,
    exp_url: &str,
    sql: &str,
    out: &mut std::collections::BTreeSet<String>,
) -> anyhow::Result<u64> {
    export_lines(client, exp_url, sql, |line| {
        let f = tickvault_storage::day_repair_coverage::csv_fields(line);
        match (f.first(), f.get(1)) {
            (Some(sid), Some(seg)) => {
                out.insert(format!("{}|{}", sid.trim(), seg.trim()));
                Ok(())
            }
            _ => bail!("export line {line:?} has no security_id and segment"),
        }
    })
    .await
}

/// Streams a `SELECT * FROM <tick-path table> ...` `/exp` answer into
/// `spare`: every rebuilt row whose `capture_seq` is not in sorted `dropped`,
/// keyed by every stored value column. The columns are read from the answer's
/// own header, so each table's real column set is used (`ticks` has no
/// `exchange_timestamp`; its `ts` is the exchange time). O(rows log dropped).
///
/// # Errors
/// As [`export_csv`], a header without a content column, or a row that does
/// not parse.
pub async fn export_spares(
    client: &reqwest::Client,
    exp_url: &str,
    sql: &str,
    dropped: &[u64],
    spare: &mut std::collections::BTreeMap<tickvault_storage::day_repair_coverage::ContentKey, u64>,
) -> anyhow::Result<u64> {
    use tickvault_storage::day_repair_coverage as cov;
    let cols: std::cell::RefCell<Option<cov::ContentCols>> = std::cell::RefCell::new(None);
    export_csv(
        client,
        exp_url,
        sql,
        |header| {
            let c = cov::ContentCols::from_header(header)
                .with_context(|| format!("the export header lacks a content column: {header}"))?;
            *cols.borrow_mut() = Some(c);
            Ok(())
        },
        |line| {
            let guard = cols.borrow();
            let c = guard.as_ref().context("a row before the header")?;
            cov::add_spare(&cov::csv_fields(line), c, dropped, spare)
        },
    )
    .await
}

/// The offset fingerprint of one day: index ticks whose exchange time is
/// before 09:00 IST. The `ticks` writer refuses such a tick to
/// `feed_aux_packets` as `out_of_window_tick` (it never holds a `ticks` row
/// before 09:00, so the first version of this query could never fire;
/// 2026-11-21 review), so that is where the 20 Nov UTC shift (session at
/// 03:45 to 10:00) shows. A correct day holds none. One query. Used on 19, 20 and 21 Nov before anything changes.
///
/// # Errors
/// As [`exec_dataset`].
// TEST-EXEMPT: one count query through exec_count.
pub async fn offset_fingerprint(
    client: &reqwest::Client,
    exec_url: &str,
    day: NaiveDate,
) -> anyhow::Result<u64> {
    let d = day.format("%Y-%m-%d");
    exec_count(
        client,
        exec_url,
        &format!(
            "SELECT count() FROM feed_aux_packets WHERE ts IN '{d}' AND segment = 'IDX_I' \
             AND kind = 'out_of_window_tick' AND exchange_ts >= '{d}T00:00:00.000000Z' \
             AND exchange_ts < '{d}T09:00:00.000000Z'"
        ),
    )
    .await
}

/// How long the verify waits for QuestDB to apply the rebuild's rows.
const APPLY_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(1_800); // APPROVED: this IS the named constant the rule asks for
/// Pause between two apply probes.
const APPLY_WAIT_STEP: std::time::Duration = std::time::Duration::from_secs(10); // APPROVED: this IS the named constant the rule asks for

/// Waits until every held-kind table (`ticks`, `tick_quotes`,
/// `feed_aux_packets`, `candles_*`) has applied its WAL and none is
/// suspended. O(tables) per probe.
///
/// # Errors
/// A suspended table, or still behind after [`APPLY_WAIT_MAX`].
async fn wait_applied(client: &reqwest::Client, exec_url: &str) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    loop {
        let rows =
            tickvault_storage::wal_suspension_watcher::fetch_wal_tables(client, exec_url).await?;
        let mut behind = Vec::new();
        for r in rows
            .iter()
            .filter(|r| tickvault_storage::held_days::table_is_held_kind(&r.name))
        {
            if r.suspended {
                bail!("table {} is WAL-suspended", r.name);
            }
            if r.writer_txn != r.sequencer_txn {
                behind.push(r.name.clone());
            }
        }
        if behind.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= APPLY_WAIT_MAX {
            bail!(
                "tables still applying after 30 minutes: {}",
                behind.join(", ")
            );
        }
        tokio::time::sleep(APPLY_WAIT_STEP).await;
    }
}

/// The numbers the verify gate read from QuestDB.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReading {
    pub nifty_1m_count: u64,
    pub nifty_1m_first: String,
    pub nifty_1m_last: String,
    pub candles_1m_before_open: u64,
    pub ticks_before_0900: u64,
    pub ticks_rows: u64,
    pub quote_rows: u64,
    pub aux_rows: u64,
}

/// The gate's verdict on a reading and the rebuild's own counts: every
/// reason the day is not repaired (empty means pass). Pure.
#[must_use]
pub fn verify_findings(r: &VerifyReading, rep: &RebuildReport, day: NaiveDate) -> Vec<String> {
    let d = day.format("%Y-%m-%d").to_string();
    let mut out = rep.unfinished_reasons();
    if !r.nifty_1m_first.starts_with(&format!("{d}T09:15:00")) {
        out.push(format!(
            "NIFTY 1m first bar is {:?}, not 09:15",
            r.nifty_1m_first
        ));
    }
    if r.nifty_1m_last.as_str() < format!("{d}T15:29:00").as_str() {
        out.push(format!(
            "NIFTY 1m last bar is {:?}, before 15:29",
            r.nifty_1m_last
        ));
    }
    if r.nifty_1m_count < 375 {
        out.push(format!("NIFTY 1m has {} bars, under 375", r.nifty_1m_count));
    }
    if r.candles_1m_before_open > 0 {
        out.push(format!(
            "{} candles_1m rows before 09:15 (the old offset)",
            r.candles_1m_before_open
        ));
    }
    if r.ticks_before_0900 > rep.ltt_refused {
        out.push(format!(
            "{} index ticks before 09:00, over the {} prints the rule refused",
            r.ticks_before_0900, rep.ltt_refused
        ));
    }
    // Each table must hold exactly the rows the rebuild's writer appended to
    // it (2026-11-21 review): fewer means rows did not land; more means rows
    // the drop did not remove. Compared per table, because an out-of-window
    // tick is folded but lands in `feed_aux_packets`, not `ticks`.
    for (table, held, wrote) in [
        ("ticks", r.ticks_rows, rep.tick_rows),
        ("tick_quotes", r.quote_rows, rep.quote_rows),
        ("feed_aux_packets", r.aux_rows, rep.aux_rows),
    ] {
        if held != wrote {
            out.push(format!(
                "{table} holds {held} rows of the day, the rebuild wrote {wrote}"
            ));
        }
    }
    out
}

/// Reads the verify gate's numbers from QuestDB after the apply wait.
///
/// # Errors
/// Any query failure, a suspended table, or the apply wait running out.
// TEST-EXEMPT: builds the client and URL, then read_verify_at (stub-server tested).
pub async fn read_verify(
    config: &ApplicationConfig,
    day: NaiveDate,
) -> anyhow::Result<VerifyReading> {
    let client = tickvault_storage::http_client::build_probe_client(120)
        .map_err(|e| anyhow::anyhow!("http client: {e}"))?;
    let url = format!(
        "http://{}:{}/exec",
        config.questdb.host, config.questdb.http_port
    );
    read_verify_at(&client, &url, day).await
}

/// [`read_verify`] against one `/exec` URL: the apply wait, then the reads.
///
/// # Errors
/// Any query failure, a suspended table, or the apply wait running out.
pub async fn read_verify_at(
    client: &reqwest::Client,
    exec_url: &str,
    day: NaiveDate,
) -> anyhow::Result<VerifyReading> {
    let url = exec_url;
    wait_applied(client, url).await?;
    let d = day.format("%Y-%m-%d").to_string();
    let nifty = exec_dataset(
        client,
        url,
        &format!(
            "SELECT count(), min(ts), max(ts) FROM candles_1m WHERE ts IN '{d}' \
             AND security_id = 13 AND segment = 'IDX_I'"
        ),
    )
    .await?;
    let cell = |i: usize| {
        nifty
            .first()
            .and_then(|r| r.get(i))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(VerifyReading {
        nifty_1m_count: first_count(&nifty),
        nifty_1m_first: cell(1),
        nifty_1m_last: cell(2),
        candles_1m_before_open: exec_count(
            client,
            url,
            &format!(
                "SELECT count() FROM candles_1m WHERE ts IN '{d}' AND ts < '{d}T09:15:00.000000Z'"
            ),
        )
        .await?,
        ticks_before_0900: offset_fingerprint(client, url, day).await?,
        ticks_rows: exec_count(
            client,
            url,
            &format!("SELECT count() FROM ticks WHERE ts IN '{d}'"),
        )
        .await?,
        quote_rows: exec_count(
            client,
            url,
            &format!("SELECT count() FROM tick_quotes WHERE ts IN '{d}'"),
        )
        .await?,
        aux_rows: exec_count(
            client,
            url,
            &format!("SELECT count() FROM feed_aux_packets WHERE ts IN '{d}'"),
        )
        .await?,
    })
}

/// The verify gate: reads QuestDB and judges. Logs one coded line on a
/// failure. Returns the findings (empty = repaired).
///
/// # Errors
/// The reading failed (the day stays held).
// TEST-EXEMPT: read_verify plus verify_findings (unit-tested).
pub async fn verify_day(
    config: &ApplicationConfig,
    day: NaiveDate,
    rep: &RebuildReport,
) -> anyhow::Result<Vec<String>> {
    let reading = read_verify(config, day).await?;
    let findings = verify_findings(&reading, rep, day);
    if findings.is_empty() {
        info!(?reading, "day repair: the verify gate passed");
    } else {
        error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "day_repair_verify_failed",
            findings = %findings.join("; "),
            ?reading,
            "day repair: the verify gate FAILED; the day stays held and no .done is written"
        );
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nov20() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 20).expect("date")
    }

    fn frame(seq: u64, receipt: i64, ep: WalEndpoint, after_gap: bool) -> ReplayedFrame {
        ReplayedFrame {
            ws_type: tickvault_storage::ws_frame_spill::WsType::LiveFeed,
            frame: vec![1, 2, 3],
            frame_seq: seq,
            received_at_nanos: receipt,
            endpoint: ep,
            after_gap,
            first_in_segment: false,
        }
    }

    #[test]
    fn test_day_bounds_match_the_utc_rule_cutover() {
        let (start, end) = day_receipt_bounds_nanos(nov20()).expect("bounds");
        assert_eq!(
            start,
            tickvault_storage::ltt_correction::DHAN_LTT_UTC_FROM_RECEIPT_NANOS
        );
        assert_eq!(end - start, 86_400_000_000_000);
        assert_eq!(
            i64::from(day_start_fold_secs(nov20()).expect("fold")),
            start / 1_000_000_000 + 19_800
        );
    }

    #[test]
    fn test_receipt_day_classifies_the_three_sides() {
        let b = day_receipt_bounds_nanos(nov20()).expect("bounds");
        assert_eq!(receipt_day(b.0 - 1, b), ReceiptDay::Before);
        assert_eq!(receipt_day(b.0, b), ReceiptDay::On);
        assert_eq!(receipt_day(b.1 - 1, b), ReceiptDay::On);
        assert_eq!(receipt_day(b.1, b), ReceiptDay::After);
    }

    #[test]
    fn test_restart_silence_only_inside_the_continuous_session() {
        let b = day_receipt_bounds_nanos(nov20()).expect("bounds").0;
        let at = |h: i64, m: i64, s: i64| b + ((h * 3_600 + m * 60 + s) * 1_000_000_000);
        assert!(is_restart_silence(at(10, 0, 0), at(10, 0, 11)));
        assert!(!is_restart_silence(at(10, 0, 0), at(10, 0, 10)));
        // A pre-open pause is not a restart.
        assert!(!is_restart_silence(at(9, 5, 0), at(9, 10, 0)));
        // After the close.
        assert!(!is_restart_silence(at(15, 35, 0), at(15, 39, 0)));
    }

    #[test]
    fn test_select_day_frames_filters_days_and_marks_gaps() {
        let b = day_receipt_bounds_nanos(nov20()).expect("bounds");
        let at = |h: i64, m: i64| b.0 + (h * 3_600 + m * 60) * 1_000_000_000;
        let read = vec![
            frame(1, b.0 - 5, WalEndpoint::MainFeed, false),
            frame(2, at(10, 0), WalEndpoint::MainFeed, false),
            frame(3, at(10, 0) + 1, WalEndpoint::Depth20, false),
            frame(4, at(10, 5), WalEndpoint::MainFeed, false),
            frame(5, at(10, 5) + 1, WalEndpoint::MainFeed, true),
            frame(6, b.1 + 1, WalEndpoint::MainFeed, false),
        ];
        let mut last = None;
        let got = select_day_frames(read, b, true, &mut last);
        assert_eq!(got.before, 1);
        assert_eq!(got.after, 1);
        assert_eq!(
            got.frames.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![2, 4, 5]
        );
        // 2: carried gap; 4: a five-minute in-session silence; 5: resync.
        assert_eq!(got.gaps, vec![0, 1, 2]);
        assert_eq!(last, Some(at(10, 5) + 1));
    }

    #[test]
    fn test_chunk_gaps_rebases() {
        assert_eq!(chunk_gaps(&[0, 5, 12, 20], 10, 20), vec![2]);
        assert_eq!(chunk_gaps(&[0, 5], 0, 6), vec![0, 5]);
        assert!(chunk_gaps(&[], 0, 10).is_empty());
    }

    #[test]
    fn test_rebuild_report_round_trips_through_the_progress_detail() {
        let r = RebuildReport {
            segments: 3,
            frames: 1_000,
            refolded: 900,
            aux_rows: 40,
            ltt_refused: 2,
            ..RebuildReport::default()
        };
        assert_eq!(RebuildReport::decode(&r.encode()), Some(r));
        assert_eq!(RebuildReport::decode("segments=1"), None);
    }

    #[test]
    fn test_unfinished_reasons_name_every_rescue() {
        let ok = RebuildReport {
            frames: 1,
            ..RebuildReport::default()
        };
        assert!(ok.unfinished_reasons().is_empty());
        let bad = RebuildReport {
            frames: 1,
            seal_spilled: 2,
            new_spill_files: 1,
            ..RebuildReport::default()
        };
        let reasons = bad.unfinished_reasons();
        assert_eq!(reasons.len(), 2);
        assert!(RebuildReport::default().unfinished_reasons()[0].contains("no frame"));
    }

    fn passing_reading() -> VerifyReading {
        VerifyReading {
            nifty_1m_count: 375,
            nifty_1m_first: "2026-11-20T09:15:00.000000Z".to_owned(),
            nifty_1m_last: "2026-11-20T15:29:00.000000Z".to_owned(),
            candles_1m_before_open: 0,
            ticks_before_0900: 0,
            ticks_rows: 800,
            quote_rows: 100,
            aux_rows: 40,
        }
    }

    #[test]
    fn test_verify_findings_pass_and_each_failure() {
        // 900 ticks folded: 800 landed in `ticks`, 100 in `tick_quotes`, and
        // the out-of-window ones in `feed_aux_packets` (the review's false
        // alarm: the old gate compared `ticks + tick_quotes` with `refolded`).
        let rep = RebuildReport {
            frames: 10,
            refolded: 940,
            tick_rows: 800,
            quote_rows: 100,
            aux_rows: 40,
            ..RebuildReport::default()
        };
        assert!(verify_findings(&passing_reading(), &rep, nov20()).is_empty());
        let damaged = RebuildReport {
            damaged_segments: 1,
            abandoned_bytes: 512,
            ..rep
        };
        assert_eq!(
            verify_findings(&passing_reading(), &damaged, nov20()).len(),
            2
        );
        let shifted = VerifyReading {
            nifty_1m_first: "2026-11-20T03:45:00.000000Z".to_owned(),
            candles_1m_before_open: 300,
            ticks_before_0900: 50,
            ..passing_reading()
        };
        assert_eq!(verify_findings(&shifted, &rep, nov20()).len(), 3);
        let short = VerifyReading {
            ticks_rows: 10,
            aux_rows: 1,
            ..passing_reading()
        };
        assert_eq!(verify_findings(&short, &rep, nov20()).len(), 2);
        let leftover = VerifyReading {
            ticks_rows: 5_000,
            ..passing_reading()
        };
        assert_eq!(verify_findings(&leftover, &rep, nov20()).len(), 1);
    }

    #[test]
    fn test_first_count_reads_ints_and_absent() {
        assert_eq!(first_count(&[vec![serde_json::json!(7)]]), 7);
        assert_eq!(first_count(&[]), 0);
        assert_eq!(first_count(&[vec![serde_json::json!("x")]]), 0);
    }

    #[test]
    fn test_spill_files_lists_both_trees() {
        let root = std::env::temp_dir().join(format!(
            "tv-day-repair-spill-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("data/spill/ticks")).expect("dir");
        std::fs::create_dir_all(root.join("data/dlq")).expect("dir");
        std::fs::write(root.join("data/spill/ticks/a.ilp"), b"x").expect("file");
        std::fs::write(root.join("data/dlq/b.ndjson"), b"x").expect("file");
        let before = spill_files(&root);
        assert_eq!(before.len(), 2);
        std::fs::write(root.join("data/spill/seals_v4-2026-11-20.bin"), b"x").expect("file");
        assert_eq!(spill_files(&root).difference(&before).count(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}

/// The QuestDB reads of the verify gate against a local stub `/exec` server
/// (2026-11-21): the apply wait, the NIFTY read, the counts and the error
/// arms, with no real database.
#[cfg(test)]
mod stub_exec_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What the stub answers: a status, a body, keyed on a substring of the
    /// decoded SQL (first match wins; the last entry is the fallback).
    type Script = Vec<(&'static str, u16, String)>;

    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b'%' if i + 2 < b.len() => {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                    i += 3;
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Starts a stub that answers every request from `script` and returns
    /// its `/exec` URL plus the SQL it saw.
    async fn stub(script: Script) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut part = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut part).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&part[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&req).into_owned();
                let line = head.lines().next().unwrap_or_default().to_owned();
                let query = line
                    .split("query=")
                    .nth(1)
                    .and_then(|q| q.split([' ', '&']).next())
                    .map(percent_decode)
                    .unwrap_or_default();
                log.lock().expect("lock").push(query.clone());
                let (status, body) = script
                    .iter()
                    .find(|(k, _, _)| query.contains(k))
                    .or(script.last())
                    .map(|(_, s, b)| (*s, b.clone()))
                    .unwrap_or((200, String::from("{}")));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{addr}/exec"), seen)
    }

    fn wal(rows: &[(&str, bool, i64, i64)]) -> String {
        let data: Vec<String> = rows
            .iter()
            .map(|(n, s, w, q)| format!("[\"{n}\",{s},{w},{q}]"))
            .collect();
        format!(
            "{{\"columns\":[{{\"name\":\"name\",\"type\":\"STRING\"}},{{\"name\":\"suspended\",\"type\":\"BOOLEAN\"}},{{\"name\":\"writerTxn\",\"type\":\"LONG\"}},{{\"name\":\"sequencerTxn\",\"type\":\"LONG\"}}],\"dataset\":[{}]}}",
            data.join(",")
        )
    }

    fn count(n: u64) -> String {
        format!("{{\"dataset\":[[{n}]]}}")
    }

    fn nov20() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 20).expect("date")
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn test_read_verify_at_reads_every_number_after_the_apply_wait() {
        let (url, seen) = stub(vec![
            ("wal_tables", 200, wal(&[("ticks", false, 7, 7), ("market_depth", false, 1, 9)])),
            (
                "security_id = 13",
                200,
                String::from(
                    "{\"dataset\":[[375,\"2026-11-20T09:15:00.000000Z\",\"2026-11-20T15:29:00.000000Z\"]]}",
                ),
            ),
            ("ts < '2026-11-20T09:15", 200, count(0)),
            ("out_of_window_tick", 200, count(2)),
            ("FROM ticks", 200, count(800)),
            ("FROM tick_quotes", 200, count(100)),
            ("FROM feed_aux_packets", 200, count(40)),
            ("", 500, String::from("unexpected")),
        ])
        .await;
        let r = read_verify_at(&client(), &url, nov20())
            .await
            .expect("reading");
        assert_eq!(
            r,
            VerifyReading {
                nifty_1m_count: 375,
                nifty_1m_first: "2026-11-20T09:15:00.000000Z".to_owned(),
                nifty_1m_last: "2026-11-20T15:29:00.000000Z".to_owned(),
                candles_1m_before_open: 0,
                ticks_before_0900: 2,
                ticks_rows: 800,
                quote_rows: 100,
                aux_rows: 40,
            }
        );
        let seen = seen.lock().expect("lock").clone();
        // The apply wait runs first; market_depth (not a held kind) lagging
        // does not hold it.
        assert!(seen[0].contains("wal_tables"), "{seen:?}");
        assert_eq!(seen.len(), 7, "{seen:?}");
    }

    #[tokio::test]
    async fn test_export_seqs_streams_numbers_after_the_header() {
        let mut body = String::from("\"capture_seq\"\r\n");
        for i in 0..5_000u64 {
            body.push_str(&format!("{}\r\n", 1_000_000 + i));
        }
        body.push_str("42");
        let (url, _) = stub(vec![
            ("FROM ticks", 200, body),
            ("FROM bad", 200, String::from("capture_seq\n1\nx\n")),
            ("", 500, String::from("boom")),
        ])
        .await;
        let mut out = Vec::new();
        let n = export_seqs(&client(), &url, "SELECT capture_seq FROM ticks", &mut out)
            .await
            .expect("export");
        assert_eq!(n, 5_001);
        assert_eq!(out.first(), Some(&1_000_000));
        assert_eq!(out.last(), Some(&42));
        let e = export_seqs(&client(), &url, "SELECT capture_seq FROM bad", &mut out)
            .await
            .expect_err("not a number");
        assert!(format!("{e:#}").contains("\"x\""), "{e:#}");
        let e = export_seqs(&client(), &url, "other", &mut out)
            .await
            .expect_err("500");
        assert!(format!("{e:#}").contains("500"), "{e:#}");
    }

    #[tokio::test]
    async fn test_export_lines_waits_out_a_429_and_export_contracts_reads_pairs() {
        // First answer 429 (QuestDB's export connection limit), then the rows.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut n = 0u32;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut part = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut part).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&part[..n]),
                    }
                }
                let (status, body) = if n == 0 {
                    (429, String::from("busy"))
                } else {
                    (
                        200,
                        String::from(
                            "\"security_id\",\"segment\"\r\n13,IDX_I\r\n52411,\"NSE_FNO\"\r\n",
                        ),
                    )
                };
                n += 1;
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        let url = format!("http://{addr}/exp");
        let mut out = std::collections::BTreeSet::new();
        let rows = export_contracts(&client(), &url, "SELECT DISTINCT", &mut out)
            .await
            .expect("waited out the 429");
        assert_eq!(rows, 2);
        let want: std::collections::BTreeSet<String> = ["13|IDX_I", "52411|NSE_FNO"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(out, want);
        // export_lines hands each data line to `take` and stops on its error.
        let mut seen = Vec::new();
        let e = export_lines(&client(), &url, "SELECT DISTINCT", |l| {
            seen.push(l.to_owned());
            anyhow::bail!("refused")
        })
        .await
        .expect_err("take refused");
        assert!(format!("{e:#}").contains("refused"), "{e:#}");
        assert_eq!(seen, vec![String::from("13,IDX_I")]);
    }

    #[tokio::test]
    async fn test_export_spares_and_export_csv_count_rebuilt_rows_whose_number_is_no_dropped_packet()
     {
        let hdr = "ts,security_id,segment,ltp,open,high,low,close,volume,oi,oi_day_high,oi_day_low,avg_price,last_trade_qty,total_buy_qty,total_sell_qty,received_at,capture_seq\r\n";
        let row = |seq: u64| {
            format!("2026-11-20T09:30:00.000000Z,7,NSE_FNO,5.5,,,,,100,,,,,,,,,{seq}\r\n")
        };
        let body = format!("{hdr}{}{}{}", row(10), row(90), row(91));
        let (url, seen) = stub(vec![
            ("FROM ticks", 200, body),
            ("", 500, String::from("x")),
        ])
        .await;
        let mut spare = std::collections::BTreeMap::new();
        let rows = export_spares(
            &client(),
            &url,
            "SELECT capture_seq FROM ticks",
            &[10, 11],
            &mut spare,
        )
        .await
        .expect("export");
        assert_eq!(rows, 3);
        assert_eq!(spare.len(), 1);
        assert_eq!(spare.values().copied().sum::<u64>(), 2, "90 and 91 only");
        assert!(
            seen.lock()
                .expect("lock")
                .iter()
                .any(|q| q.contains("FROM ticks"))
        );
        assert!(
            export_spares(&client(), &url, "other", &[], &mut spare)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_read_verify_at_refuses_a_suspended_held_table() {
        let (url, _) = stub(vec![(
            "wal_tables",
            200,
            wal(&[("candles_1m", true, 3, 3)]),
        )])
        .await;
        let e = read_verify_at(&client(), &url, nov20())
            .await
            .expect_err("suspended");
        assert!(format!("{e:#}").contains("WAL-suspended"), "{e:#}");
    }

    #[tokio::test]
    async fn test_exec_dataset_error_arms_and_first_count_shapes() {
        let (url, _) = stub(vec![
            ("bad_status", 400, String::from("{\"error\":\"no\"}")),
            ("not_json", 200, String::from("<html>")),
            ("no_dataset", 200, String::from("{\"columns\":[]}")),
            ("neg", 200, String::from("{\"dataset\":[[-4]]}")),
            ("", 200, count(9)),
        ])
        .await;
        let c = client();
        let e = exec_dataset(&c, &url, "bad_status").await.expect_err("400");
        assert!(format!("{e:#}").contains("400"), "{e:#}");
        assert!(exec_dataset(&c, &url, "not_json").await.is_err());
        let e = exec_dataset(&c, &url, "no_dataset")
            .await
            .expect_err("no dataset");
        assert!(format!("{e:#}").contains("no dataset"), "{e:#}");
        assert_eq!(exec_count(&c, &url, "neg").await.expect("neg"), 0);
        assert_eq!(exec_count(&c, &url, "anything").await.expect("count"), 9);
        assert_eq!(offset_fingerprint(&c, &url, nov20()).await.expect("fp"), 9);
        // Nothing listening: the request itself fails with the query named.
        let e = exec_dataset(&c, "http://127.0.0.1:1/exec", "SELECT 1")
            .await
            .expect_err("refused");
        assert!(format!("{e:#}").contains("SELECT 1"), "{e:#}");
    }
}
