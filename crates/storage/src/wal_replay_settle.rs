//! WAL replay SETTLE step (DA-13c, 2026-10-13).
//!
//! # The two holes this closes
//!
//! Until this module, the moment a replayed batch's rows were acked by the
//! writer threads, `ws_frame_spill::confirm_replayed` renamed its segments
//! from `replaying/` into `archive/`, which no replay ever reads again. Two
//! things could still lose those rows after that rename, uncounted:
//!
//! 1. **The durability lag (c2).** An ILP `2xx` means QuestDB wrote the rows
//!    into its page cache (`nosync`), not to disk. The persisted applied
//!    watermark is held back [`WATERMARK_DURABILITY_LAG_SECS`] for exactly
//!    this reason; the archive step had no such delay. A host crash inside
//!    that window lost rows whose segments were already in `archive/`.
//! 2. **The suspension probe's lag (c6).** A table that WAL-suspends keeps
//!    acking ILP writes. The watcher sees it at most one probe interval
//!    (60 s) later, so a batch acked in between was archived on the strength
//!    of acks the database never applied. The suspicion window the watermark
//!    marks on detection covers the live high-water range, not the OLD
//!    sequences a replay re-folds, so nothing protected them.
//!
//! # The shape of the fix
//!
//! An acked batch moves from `replaying/` to `settling/` instead of
//! `archive/`. `settling/` is NOT read by an in-process replay, so the
//! catch-up drain never re-folds a batch it already landed. A settled batch
//! is promoted to `archive/` only when ALL of these hold, the rule the seal
//! mid-session replay (`seal_writer_task::MidSessionReplay`) already uses:
//!
//! * [`WATERMARK_DURABILITY_LAG_SECS`] have passed since the ack;
//! * at least [`WAL_REPLAY_SETTLE_CLEAN_PROBES`] clean QuestDB probes were
//!   reported after the ack (so one whole probe ran after it);
//! * no suspicion episode began after the batch STARTED writing (review fix:
//!   rows are acked all through a refold, so the count is read where the
//!   batch begins, [`replay_batch_start`], and the oldest waiting start wins).
//!
//! The watcher calls [`promote_settled_now`] after each probe. If a
//! suspicion episode began, the settled segments are HELD in `settling/` for
//! the rest of the process and counted; nothing is archived unread.
//!
//! At boot, before the first replay pass reads the watermark, segments left
//! in `settling/` by an earlier process have their sequence ranges marked
//! unapplied in the watermark (persisted, then verified on disk) and are
//! moved back to `replaying/`, so the replay reads them in full. Each range is
//! read with the replay's own reader, so a torn first record still yields
//! the range of the intact frames behind it; a segment that cannot be read at
//! all is set apart in `settling/unreadable/`, never returned unmarked and
//! never archived, and tried again at every boot. The failure
//! direction of every step is "replay more"; the DEDUP keys absorb repeats.
//!
//! # Cost
//!
//! Cold path only, never the frame drain. Settle: one directory listing and
//! one rename per staged segment, once per replayed batch. Promote: one lock
//! and three atomic loads per watcher probe (60 s) while nothing is settling;
//! one listing and one rename per settled segment when promoting. Boot
//! recovery: one full CRC-checked read of each segment in `settling/` (O(bytes),
//! one segment in memory at a time, no frame kept), once per process.
//!
//! # Honest limits
//!
//! * One mark covers every settled segment: a newer batch restarts the clock
//!   for older ones, so a long catch-up drain promotes everything only after
//!   its last batch has settled. A crash before then re-reads all of it.
//! * Held segments are re-read only at the next boot, so rows QuestDB dropped
//!   during a suspension reach it a session late, not never.
//! * The 60 s lag is the watermark's own assumption about kernel writeback;
//!   it is not an fsync of QuestDB's files.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

use crate::wal_applied_watermark::{
    AppliedSnapshot, AppliedWatermark, WATERMARK_DURABILITY_LAG_SECS, applied_watermark,
};
use crate::ws_frame_spill::{
    ARCHIVE_SUBDIR, REPLAYING_SUBDIR, WalEndpoint, read_segment_frames_matching, wal_segments_in,
};

/// Acked-but-not-yet-durable replay segments. Never read by an in-process
/// replay; read (after its ranges are marked unapplied) by the next boot's.
pub const SETTLING_SUBDIR: &str = "settling";

/// Under `settling/`: leftovers a boot could not read, so their sequence
/// range could not be marked unapplied (DA-13c review). Never archived, never
/// returned to the replay unmarked; every boot's recovery tries them again.
pub const SETTLING_UNREADABLE_SUBDIR: &str = "unreadable";

/// The watermark's suspicion-episode count when a replay batch STARTED
/// writing (DA-13c review). Rows are acked all through a refold, so an
/// episode that begins and ends inside it is invisible to a count taken at
/// the settle; the settle compares against this one instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleBatchStart {
    episodes: u64,
}

impl SettleBatchStart {
    /// The batch-start reading of `wm`. Two atomic loads.
    ///
    /// An episode ALREADY OPEN at the start (the sink is suspect now) counts
    /// as one that began after it: the batch is written into a sink that may
    /// ack without applying, so it must hold even if the episode clears
    /// before the settle. The count is read BEFORE the flag: the watermark
    /// sets the flag before it bumps the count, so an episode OPENING between
    /// the two loads only makes this reading lower, which holds more. One
    /// CLOSING between them was over before the batch wrote a row.
    #[must_use]
    pub fn of(wm: &AppliedWatermark) -> Self {
        let count = wm.suspect_episode_count();
        let open = wm.is_sink_suspect();
        Self {
            episodes: count.saturating_sub(u64::from(open)),
        }
    }
}

/// [`SettleBatchStart::of`] the process-global watermark. Take it BEFORE the
/// batch's first row is handed to a writer (where `unlanded_before` is read).
#[must_use]
// TEST-EXEMPT: thin global wrapper; SettleBatchStart::of is covered by the module tests.
pub fn replay_batch_start() -> SettleBatchStart {
    SettleBatchStart::of(applied_watermark())
}

/// The lowest and highest sequence of the INTACT records in a segment, found
/// with the replay's own reader (CRC-checked, resyncing past damage), so the
/// range covers every frame the replay will return from it. `Ok(None)` when
/// no record carries a sequence. Cold: once per leftover segment at boot,
/// reads the whole file; no frame payload is kept.
fn intact_frame_seq_range(seg: &Path) -> anyhow::Result<Option<(u64, u64)>> {
    let lo = std::cell::Cell::new(u64::MAX);
    let hi = std::cell::Cell::new(0u64);
    let keep = |seq: u64, _endpoint: WalEndpoint| {
        if seq > 0 {
            lo.set(lo.get().min(seq));
            hi.set(hi.get().max(seq));
        }
        false
    };
    read_segment_frames_matching(seg, &keep)?;
    Ok((hi.get() > 0).then(|| (lo.get(), hi.get())))
}

/// Clean QuestDB probes that must be reported after a batch's ack before its
/// segments may be archived. Two, so at least one whole probe ran after the
/// ack: a probe already in flight at the ack may have queried before it.
pub const WAL_REPLAY_SETTLE_CLEAN_PROBES: u64 = 2;

/// Segments kept in `settling/` for the next boot because a suspicion
/// episode began after their ack, or because leftovers from an earlier
/// process could not be recovered. Local `/metrics` only.
pub const WAL_REPLAY_SETTLE_HELD_COUNTER: &str = "tv_wal_replay_settle_held_segments_total";

/// Segments an earlier process left in `settling/`, marked unapplied and
/// returned to `replaying/` at boot so they are read again. Local only.
pub const WAL_REPLAY_SETTLE_RECOVERED_COUNTER: &str =
    "tv_wal_replay_settle_recovered_segments_total";

/// Bytes held in `settling/unreadable/` (DA-13c review): leftovers no boot
/// could read, kept and never pruned. Set at the once-per-process check;
/// local `/metrics` only, no alarm.
pub const WAL_REPLAY_SETTLE_UNREADABLE_BYTES_GAUGE: &str = "tv_wal_replay_settle_unreadable_bytes";

/// The existing counter `confirm_replayed` used; a segment now counts as
/// confirmed when it reaches `archive/`.
const CONFIRMED_COUNTER: &str = "tv_wal_replay_confirmed_segments_total";

/// What the watermark looked like when the newest batch was acked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettleMark {
    acked_at: Instant,
    clean_probes_at: u64,
    episodes_at: u64,
}

/// What [`SettleState::promote`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleVerdict {
    /// Nothing has been settled in this process.
    Idle,
    /// Settled segments wait for the lag or for clean probes.
    Waiting,
    /// This many segments moved to `archive/`.
    Promoted(u64),
    /// Held for the next boot (a suspicion episode began after the ack, or an
    /// earlier process left segments that could not be recovered).
    Held,
}

/// The settle bookkeeping of one process (one WAL directory).
#[derive(Debug)]
pub struct SettleState {
    dir: Option<PathBuf>,
    mark: Option<SettleMark>,
    held: bool,
    settled_any: bool,
    /// `settling/unreadable/` was tried this process (DA-13c review): once
    /// per process, so a replay pass that runs before the first settle does
    /// not re-read, re-warn and re-count it.
    unreadable_checked: bool,
}

impl Default for SettleState {
    fn default() -> Self {
        Self::new()
    }
}

impl SettleState {
    /// Empty state: nothing settled, nothing held.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dir: None,
            mark: None,
            held: false,
            settled_any: false,
            unreadable_checked: false,
        }
    }

    /// Moves every staged segment from `replaying/` to `settling/` and
    /// records the watermark's state at this ack. Returns the number moved.
    ///
    /// `start` is the episode count read when the batch STARTED writing: an
    /// episode that began after it, even one already over, holds the batch.
    /// A newer batch keeps the oldest start still waiting, so it can never
    /// hide an episode that touched an older one.
    ///
    /// Settled leftovers from an EARLIER process found here (the boot
    /// recovery did not run, for example a pass refused on the disk floor)
    /// would otherwise be promoted with this batch unread, so they hold the
    /// whole set for the next boot instead.
    pub fn settle(
        &mut self,
        wal_dir: &Path,
        wm: &AppliedWatermark,
        now: Instant,
        start: SettleBatchStart,
    ) -> u64 {
        let settling_dir = wal_dir.join(SETTLING_SUBDIR);
        if !self.settled_any && !self.held {
            let leftovers = wal_segments_in(&settling_dir).len() as u64;
            if leftovers > 0 {
                self.hold(leftovers, "leftovers_not_recovered");
            }
        }
        let staged = wal_segments_in(&wal_dir.join(REPLAYING_SUBDIR));
        let moved = rename_all(&staged, &settling_dir, "replay_settle_rename");
        self.settled_any = true;
        self.dir = Some(wal_dir.to_path_buf());
        self.mark = Some(SettleMark {
            acked_at: now,
            clean_probes_at: wm.clean_probe_count(),
            episodes_at: self
                .mark
                .map_or(start.episodes, |m| m.episodes_at.min(start.episodes)),
        });
        if self.held && moved > 0 {
            metrics::counter!(WAL_REPLAY_SETTLE_HELD_COUNTER).increment(moved);
        }
        info!(
            wal_dir = ?wal_dir,
            settled_segments = moved,
            staged = staged.len(),
            held = self.held,
            "WAL replay acked: staged segments moved to settling/ until the acked rows are durable"
        );
        moved
    }

    /// Archives the settled segments once the lag has passed, two clean
    /// probes ran after the ack and no suspicion episode began since.
    pub fn promote(&mut self, wm: &AppliedWatermark, now: Instant) -> SettleVerdict {
        let (Some(dir), Some(mark)) = (self.dir.clone(), self.mark) else {
            return SettleVerdict::Idle;
        };
        if self.held {
            return SettleVerdict::Held;
        }
        let settling_dir = dir.join(SETTLING_SUBDIR);
        if wm.suspect_episode_count() != mark.episodes_at {
            let held = wal_segments_in(&settling_dir).len() as u64;
            self.hold(held, "questdb_suspect_after_ack");
            return SettleVerdict::Held;
        }
        let lag = Duration::from_secs(WATERMARK_DURABILITY_LAG_SECS);
        if now.saturating_duration_since(mark.acked_at) < lag
            || wm.clean_probe_count()
                < mark
                    .clean_probes_at
                    .saturating_add(WAL_REPLAY_SETTLE_CLEAN_PROBES)
            || wm.is_sink_suspect()
        {
            return SettleVerdict::Waiting;
        }
        let settled = wal_segments_in(&settling_dir);
        let promoted = rename_all(&settled, &dir.join(ARCHIVE_SUBDIR), "replay_settle_archive");
        if promoted > 0 {
            metrics::counter!(CONFIRMED_COUNTER).increment(promoted);
        }
        self.mark = None;
        info!(
            promoted_segments = promoted,
            settled = settled.len(),
            "WAL replay settled: the acked rows are past the durability lag and two clean \
             QuestDB probes, so their segments are archived"
        );
        SettleVerdict::Promoted(promoted)
    }

    /// Boot step, before the first replay pass loads the watermark: every
    /// segment an earlier process left in `settling/` has its sequence range
    /// marked unapplied (persisted and read back), then moves to
    /// `replaying/`, so the replay reads it in full. Returns the number
    /// recovered. Does nothing once this process has settled a batch: from
    /// then on `settling/` holds this process's own acked batches.
    ///
    /// If the marks cannot be shown on disk, the segments are HELD where they
    /// are instead: moved back unmarked, the replay could skip them as
    /// applied and the next settle would archive them unread.
    pub fn recover(&mut self, wal_dir: &Path, wm: &AppliedWatermark) -> u64 {
        if self.settled_any || self.held {
            return 0;
        }
        let settling_dir = wal_dir.join(SETTLING_SUBDIR);
        let mut leftovers = wal_segments_in(&settling_dir);
        // Segments an earlier boot could not read are tried again, once per
        // process.
        if !self.unreadable_checked {
            self.unreadable_checked = true;
            leftovers.extend(wal_segments_in(
                &settling_dir.join(SETTLING_UNREADABLE_SUBDIR),
            ));
        }
        if leftovers.is_empty() {
            return 0;
        }
        // O(1) EXEMPT: begin — boot-time recovery, one read per leftover segment
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(leftovers.len()); // APPROVED: boot-time, cold path
        let mut readable: Vec<PathBuf> = Vec::with_capacity(leftovers.len()); // APPROVED: boot-time, cold path
        let mut unreadable: Vec<PathBuf> = Vec::new(); // APPROVED: boot-time, cold path
        for seg in &leftovers {
            match intact_frame_seq_range(seg) {
                Ok(Some(range)) => {
                    ranges.push(range);
                    readable.push(seg.clone());
                }
                // No record carries a sequence: the replay's applied filter
                // cannot drop any frame of it, so it needs no mark.
                Ok(None) => readable.push(seg.clone()),
                // DA-13c review: no range can be derived, so it must never go
                // back unmarked. Set apart, retried at every boot.
                Err(err) => {
                    warn!(
                        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                        source = "replay_settle_unreadable",
                        segment = ?seg,
                        error = %err,
                        "WAL replay: a segment an earlier process acked could not be read, so \
                         its sequence range cannot be marked unapplied. It is kept in \
                         settling/unreadable/ and tried again at the next boot; never archived."
                    );
                    unreadable.push(seg.clone());
                }
            }
        }
        // O(1) EXEMPT: end
        if !unreadable.is_empty() {
            let set_apart = rename_all(
                &unreadable,
                &settling_dir.join(SETTLING_UNREADABLE_SUBDIR),
                "replay_settle_unreadable",
            );
            metrics::counter!(WAL_REPLAY_SETTLE_HELD_COUNTER).increment(set_apart);
            metrics::gauge!(WAL_REPLAY_SETTLE_UNREADABLE_BYTES_GAUGE)
                .set(segment_bytes(&settling_dir.join(SETTLING_UNREADABLE_SUBDIR)) as f64);
            if set_apart < unreadable.len() as u64 {
                // One stayed in settling/: the next settle must not archive it.
                self.hold(
                    unreadable.len() as u64 - set_apart,
                    "unreadable_not_set_apart",
                );
                return 0;
            }
        }
        let leftovers = readable;
        if leftovers.is_empty() {
            return 0;
        }
        wm.bind(wal_dir);
        // 2026-10-20 (PR W review): bounded ranges, never the bucket table. A
        // range here spans whole segments (possibly days of them), which the
        // 128-slot table overflowed into an unbounded bit.
        // O(1) EXEMPT: one O(32) range insert per leftover segment, boot-time
        for &(lo, hi) in &ranges {
            wm.add_unapplied_range(lo, hi);
        }
        wm.persist_now();
        let marked_on_disk = AppliedSnapshot::load(wal_dir).is_none_or(|snap| {
            ranges
                .iter()
                .all(|&(lo, hi)| snap.range_has_unapplied(lo, hi))
        });
        if !marked_on_disk {
            self.hold(leftovers.len() as u64, "recovery_marks_not_persisted");
            return 0;
        }
        let recovered = rename_all(
            &leftovers,
            &wal_dir.join(REPLAYING_SUBDIR),
            "replay_settle_recover",
        );
        if recovered > 0 {
            metrics::counter!(WAL_REPLAY_SETTLE_RECOVERED_COUNTER).increment(recovered);
        }
        info!(
            wal_dir = ?wal_dir,
            recovered_segments = recovered,
            marked_ranges = ranges.len(),
            "WAL replay: segments an earlier process acked but had not yet archived are \
             marked unapplied and read again (their rows may not have reached disk)"
        );
        recovered
    }

    fn hold(&mut self, segments: u64, reason: &'static str) {
        if !self.held {
            warn!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                source = "replay_settle_held",
                reason,
                segments,
                "WAL replay: settled segments are kept in settling/ for the next boot, which \
                 marks them unapplied and reads them again. Nothing is archived unread."
            );
        }
        self.held = true;
        metrics::counter!(WAL_REPLAY_SETTLE_HELD_COUNTER).increment(segments);
    }
}

/// Total size of the entries directly in `dir` (a directory entry counts its
/// own metadata size). Cold: once per process.
fn segment_bytes(dir: &Path) -> u64 {
    // O(1) EXEMPT: once per process, bounded by the set-apart segments
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(|e| e.ok()?.metadata().ok())
            .map(|m| m.len())
            .fold(0u64, u64::saturating_add)
    })
}

/// Renames each file into `dst_dir` (created if missing). A failed rename
/// leaves the file where it was, coded; returns how many moved.
fn rename_all(files: &[PathBuf], dst_dir: &Path, source: &'static str) -> u64 {
    if files.is_empty() {
        return 0;
    }
    drop(std::fs::create_dir_all(dst_dir)); // O(1) EXEMPT: cold settle step
    let mut moved = 0u64;
    for seg in files {
        let Some(name) = seg.file_name() else {
            continue;
        };
        // O(1) EXEMPT: cold settle step, one rename per segment
        match std::fs::rename(seg, dst_dir.join(name)) {
            Ok(()) => moved = moved.saturating_add(1),
            Err(err) => error!(
                code = ErrorCode::HotPath01SyncFsFailed.code_str(),
                source,
                segment = ?seg,
                error = %err,
                "WAL replay settle: a segment rename did not happen; it stays where it was, \
                 which only ever re-reads it (idempotent via the capture_seq DEDUP key)"
            ),
        }
    }
    moved
}

static STATE: Mutex<SettleState> = Mutex::new(SettleState::new());

fn state() -> std::sync::MutexGuard<'static, SettleState> {
    // Poison only means a panic mid-update; every field errs toward holding.
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Production replacement for `ws_frame_spill::confirm_replayed` at the two
/// places a batch's rows were ACKED (the boot refold and each catch-up
/// round): moves the staged segments to `settling/`. The watcher's
/// [`promote_settled_now`] archives them later.
// TEST-EXEMPT: thin global wrapper; SettleState::settle is covered by the module tests.
pub fn settle_confirm_replayed<P: AsRef<Path>>(wal_dir: P, start: SettleBatchStart) -> u64 {
    let moved = state().settle(wal_dir.as_ref(), applied_watermark(), Instant::now(), start);
    // 2026-10-20 (plan item PR W): after the boot refold and after each
    // catch-up round, drop the unapplied ranges below the oldest segment still
    // waiting (settled ones included), so the record shrinks as the backlog
    // is read.
    let _trimmed =
        crate::wal_unapplied_ranges::trim_to_waiting(applied_watermark(), wal_dir.as_ref());
    moved
}

/// Called by the WAL-suspension watcher after each probe.
// TEST-EXEMPT: thin global wrapper; SettleState::promote is covered by the module tests.
pub fn promote_settled_now() -> SettleVerdict {
    state().promote(applied_watermark(), Instant::now())
}

/// Called at the top of every replay pass; does work only at boot, when an
/// earlier process left segments in `settling/`.
// TEST-EXEMPT: thin global wrapper; SettleState::recover is covered by the module tests.
pub(crate) fn recover_settling_leftovers(wal_dir: &Path) -> u64 {
    state().recover(wal_dir, applied_watermark())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-settle-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(dir.join(REPLAYING_SUBDIR)).expect("mkdir"); // APPROVED: test-only
        dir
    }

    fn plant(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"x").expect("write"); // APPROVED: test-only
        p
    }

    fn names(dir: &Path) -> usize {
        wal_segments_in(dir).len()
    }

    fn lagged(t: Instant) -> Instant {
        t + Duration::from_secs(WATERMARK_DURABILITY_LAG_SECS)
    }

    /// c2: an acked batch is not archived until the durability lag has
    /// passed, even when the probes are clean.
    #[test]
    fn test_regression_da13c_settled_batch_waits_out_the_durability_lag() {
        let dir = tmp("lag");
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000001.wal",
        );
        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        let t0 = Instant::now();
        assert_eq!(st.settle(&dir, &wm, t0, SettleBatchStart::of(&wm)), 1);
        assert_eq!(
            names(&dir.join(SETTLING_SUBDIR)),
            1,
            "settled, not archived"
        );
        assert_eq!(names(&dir.join(ARCHIVE_SUBDIR)), 0);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(
            st.promote(&wm, t0 + Duration::from_secs(1)),
            SettleVerdict::Waiting,
            "two clean probes alone do not make page-cache rows durable"
        );
        assert_eq!(names(&dir.join(ARCHIVE_SUBDIR)), 0);
        assert_eq!(st.promote(&wm, lagged(t0)), SettleVerdict::Promoted(1));
        assert_eq!(names(&dir.join(ARCHIVE_SUBDIR)), 1);
        assert_eq!(names(&dir.join(SETTLING_SUBDIR)), 0);
        drop(std::fs::remove_dir_all(&dir));
    }

    /// c6: the suspension flag lags by up to one probe, so archiving needs
    /// two clean probes AFTER the ack, and a suspicion episode in between
    /// holds the batch for the next boot even after QuestDB recovers.
    #[test]
    fn test_regression_da13c_settled_batch_needs_clean_probes_after_the_ack() {
        let dir = tmp("probe");
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000002.wal",
        );
        let wm = AppliedWatermark::new_for_tests();
        wm.note_questdb_probe(true); // a probe from BEFORE the ack
        let mut st = SettleState::new();
        let t0 = Instant::now();
        st.settle(&dir, &wm, t0, SettleBatchStart::of(&wm));
        assert_eq!(
            st.promote(&wm, lagged(t0)),
            SettleVerdict::Waiting,
            "no probe has run since the ack"
        );
        wm.note_questdb_probe(true);
        assert_eq!(st.promote(&wm, lagged(t0)), SettleVerdict::Waiting);
        // The table suspended inside the ack window; the next probe sees it.
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(
            st.promote(&wm, lagged(t0)),
            SettleVerdict::Held,
            "a suspicion episode after the ack holds the batch, even once it recovered"
        );
        assert_eq!(
            names(&dir.join(ARCHIVE_SUBDIR)),
            0,
            "nothing archived unread"
        );
        assert_eq!(names(&dir.join(SETTLING_SUBDIR)), 1);
        // A later batch in the same process is held too.
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000003.wal",
        );
        st.settle(&dir, &wm, t0, SettleBatchStart::of(&wm));
        assert_eq!(st.promote(&wm, lagged(t0)), SettleVerdict::Held);
        assert_eq!(names(&dir.join(SETTLING_SUBDIR)), 2);
        drop(std::fs::remove_dir_all(&dir));
    }

    fn seq(secs: u64) -> u64 {
        use crate::ws_frame_spill::PACKET_INDEX_BITS;
        ((1_780_000_000 + secs) * 1_000_000_000) >> PACKET_INDEX_BITS << PACKET_INDEX_BITS
    }

    /// One v4 segment of `frames` main-feed records, one second apart.
    fn segment(dir: &Path, first_secs: u64, frames: u64) {
        use crate::ws_frame_spill::{WalEndpoint, WsType, crc32_ieee_of};
        let mut bytes = Vec::new();
        for i in 0..frames {
            let s = seq(first_secs + i).to_le_bytes();
            let recv = 7_i64.to_le_bytes();
            let frame = [0xAB_u8; 32];
            let len = 32_u32.to_le_bytes();
            let ws = WsType::LiveFeed.as_u8();
            let ep = WalEndpoint::MainFeed.as_u8();
            let crc = crc32_ieee_of(&[&[ws], &s[..], &recv[..], &[ep], &len[..], &frame]);
            bytes.extend_from_slice(b"TVW4");
            bytes.push(ws);
            bytes.extend_from_slice(&s);
            bytes.extend_from_slice(&recv);
            bytes.push(ep);
            bytes.extend_from_slice(&len);
            bytes.extend_from_slice(&frame);
            bytes.extend_from_slice(&crc.to_le_bytes());
        }
        let path = dir.join(format!("ws-frames-{:020}.wal", seq(first_secs)));
        std::fs::write(path, bytes).expect("write"); // APPROVED: test-only
    }

    /// Persists a DURABLE high-water of `seq(secs)` for `dir`: the ack, then a
    /// persist past the durability lag (a persist inside it writes 0).
    fn persist_high_water(dir: &Path, secs: u64) {
        let before = AppliedWatermark::new_for_tests();
        before.bind(dir);
        before.note_ticks_acked(seq(secs));
        before.note_depth_acked(seq(secs));
        before.persist_at(1);
        before.persist_at(1 + WATERMARK_DURABILITY_LAG_SECS * 1_000_000_000);
        let snap = AppliedSnapshot::load(dir).expect("persisted");
        assert!(
            snap.frame_is_applied(
                crate::wal_applied_watermark::AppliedSink::Ticks,
                seq(secs / 2)
            ),
            "the persisted watermark really claims everything well below it as applied"
        );
    }

    fn replay(dir: &Path) -> usize {
        crate::ws_frame_spill::replay_all_with_report_guarded(
            dir,
            usize::MAX,
            || None,
            None,
            crate::ws_frame_spill::WAL_REPLAY_RSS_STOP_PCT,
        )
        .expect("replay")
        .frames
        .len()
    }

    /// c2 at the next boot: a crash inside the settle window leaves acked
    /// segments in `settling/`, and the persisted watermark already claims
    /// their (old) sequences as applied. Recovery marks them unapplied and
    /// returns them to `replaying/`, so the boot replay reads every frame.
    #[test]
    fn test_regression_da13c_boot_rereads_segments_left_settling_by_a_crash() {
        let dir = tmp("recover");
        let settling = dir.join(SETTLING_SUBDIR);
        std::fs::create_dir_all(&settling).expect("mkdir"); // APPROVED: test-only
        segment(&settling, 0, 5);
        segment(&settling, 1_000, 5);
        segment(&dir, 2_000, 5); // a live segment after them
        // Live acks lifted the watermark past every segment here (the normal
        // state after a catch-up), and that watermark was persisted durably.
        persist_high_water(&dir, 5_000);

        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        assert_eq!(st.recover(&dir, &wm), 2, "both leftovers recovered");
        assert_eq!(names(&settling), 0);
        // 2026-10-20 (PR W review): the marks are bounded ranges, never the
        // bucket table, which a recovery spanning days used to overflow.
        let snap = wm.snapshot();
        assert!(!snap.overflowed);
        assert!(snap.buckets.iter().all(|&(id, _)| id == 0));
        assert_eq!(snap.ranges.len(), 2, "one range per recovered segment");
        assert_eq!(
            replay(&dir),
            10,
            "every frame of the two settled segments is read again; the live one \
             really is applied and is filtered"
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    /// DA-13c review: a leftover whose FIRST record is torn still has its
    /// intact frames marked (the range comes from the replay's own reader),
    /// so they are read again instead of being filtered as applied.
    #[test]
    fn test_regression_da13c_recover_marks_a_segment_whose_first_record_is_corrupt() {
        let dir = tmp("torn-first");
        let settling = dir.join(SETTLING_SUBDIR);
        std::fs::create_dir_all(&settling).expect("mkdir"); // APPROVED: test-only
        segment(&settling, 0, 5);
        let seg = settling.join(format!("ws-frames-{:020}.wal", seq(0)));
        let mut bytes = std::fs::read(&seg).expect("read"); // APPROVED: test-only
        bytes[40] ^= 0xFF; // inside the first record's payload: its CRC fails
        std::fs::write(&seg, bytes).expect("write"); // APPROVED: test-only
        assert_eq!(
            crate::ws_frame_spill::first_frame_seq_in_segment(&seg),
            0,
            "the header probe cannot read this segment's range"
        );
        segment(&dir, 2_000, 5);
        persist_high_water(&dir, 5_000);

        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        assert_eq!(st.recover(&dir, &wm), 1);
        assert_eq!(
            replay(&dir),
            4,
            "the four intact frames behind the torn one are read again, not \
             filtered as applied"
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    /// DA-13c review: a leftover that cannot be read at all gets no range, so
    /// it is set apart (never returned to the replay unmarked, never
    /// archived) and does not hold the rest of the process.
    #[test]
    fn test_regression_da13c_recover_sets_apart_a_leftover_it_cannot_read() {
        let dir = tmp("unreadable");
        let settling = dir.join(SETTLING_SUBDIR);
        // A directory with a segment's name: listed, but unreadable as a file.
        let bad = settling.join("ws-frames-00000000000000000009.wal");
        std::fs::create_dir_all(&bad).expect("mkdir"); // APPROVED: test-only
        segment(&settling, 0, 5);
        persist_high_water(&dir, 5_000);

        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        assert_eq!(st.recover(&dir, &wm), 1, "the readable one is recovered");
        let apart = settling.join(SETTLING_UNREADABLE_SUBDIR);
        assert!(apart.join("ws-frames-00000000000000000009.wal").exists());
        assert_eq!(names(&settling), 0);
        assert_eq!(replay(&dir), 5);
        // The next settle and promote archive only this process's batch.
        st.settle(&dir, &wm, Instant::now(), SettleBatchStart::of(&wm));
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(
            st.promote(&wm, lagged(Instant::now())),
            SettleVerdict::Promoted(1)
        );
        assert!(
            apart.join("ws-frames-00000000000000000009.wal").exists(),
            "the unreadable leftover is never archived"
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    /// DA-13c review: an episode that begins AND ends while the batch is
    /// being written (acks all through it) holds the batch, though the count
    /// read at the settle has already moved past it.
    #[test]
    fn test_regression_da13c_episode_inside_the_refold_holds_the_batch() {
        let dir = tmp("inside");
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000007.wal",
        );
        let wm = AppliedWatermark::new_for_tests();
        let start = SettleBatchStart::of(&wm);
        // The refold runs; the table suspends and auto-resumes meanwhile.
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(true);
        assert!(!wm.is_sink_suspect(), "clean again by the settle");
        let mut st = SettleState::new();
        let t0 = Instant::now();
        st.settle(&dir, &wm, t0, start);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(st.promote(&wm, lagged(t0)), SettleVerdict::Held);
        assert_eq!(names(&dir.join(ARCHIVE_SUBDIR)), 0);
        drop(std::fs::remove_dir_all(&dir));
    }

    /// DA-13c review: a newer batch whose own start came after an episode
    /// cannot replace the older batch's earlier start.
    #[test]
    fn test_regression_da13c_a_newer_batch_keeps_the_older_batch_start() {
        let dir = tmp("older");
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000010.wal",
        );
        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        let t0 = Instant::now();
        let first = SettleBatchStart::of(&wm);
        st.settle(&dir, &wm, t0, first);
        // An episode while the first batch waits; the second starts after it.
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(true);
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000011.wal",
        );
        let second = SettleBatchStart::of(&wm);
        st.settle(&dir, &wm, t0, second);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(st.promote(&wm, lagged(t0)), SettleVerdict::Held);
        assert_eq!(names(&dir.join(ARCHIVE_SUBDIR)), 0);
        drop(std::fs::remove_dir_all(&dir));
    }

    /// DA-13c re-review: a batch that STARTS while the sink is already
    /// suspect is written into a sink that may ack without applying; it holds
    /// even when auto-resume clears the episode before the settle.
    #[test]
    fn test_regression_da13c_an_episode_open_at_batch_start_holds_the_batch() {
        let dir = tmp("open");
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000012.wal",
        );
        let wm = AppliedWatermark::new_for_tests();
        wm.note_questdb_probe(false);
        assert!(wm.is_sink_suspect());
        let start = SettleBatchStart::of(&wm);
        wm.note_questdb_probe(true); // auto-resume, during the refold
        let mut st = SettleState::new();
        let t0 = Instant::now();
        st.settle(&dir, &wm, t0, start);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(st.promote(&wm, lagged(t0)), SettleVerdict::Held);
        assert_eq!(names(&dir.join(ARCHIVE_SUBDIR)), 0);
        drop(std::fs::remove_dir_all(&dir));
    }

    /// DA-13c re-review: `settling/unreadable/` is tried once per process, not
    /// on every replay pass before the first settle; it stays listed for the
    /// after-close depth pass and is uploaded to the cold bucket.
    #[test]
    fn test_regression_da13c_unreadable_leftovers_are_tried_once_per_process() {
        let dir = tmp("once");
        let apart = dir.join(SETTLING_SUBDIR).join(SETTLING_UNREADABLE_SUBDIR);
        let name = format!("ws-frames-{:020}.wal", seq(0));
        std::fs::create_dir_all(apart.join(&name)).expect("mkdir"); // APPROVED: test-only
        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        assert_eq!(st.recover(&dir, &wm), 0, "unreadable: set apart");
        // It becomes readable later in the SAME process: not tried again.
        std::fs::remove_dir(apart.join(&name)).expect("rmdir"); // APPROVED: test-only
        segment(&apart, 0, 3);
        assert_eq!(
            st.recover(&dir, &wm),
            0,
            "a second pass does not re-read it"
        );
        assert!(apart.join(&name).is_file());
        let spans = crate::ws_frame_spill::deferred_segment_spans(&dir).expect("spans");
        assert!(
            spans.iter().any(|s| s.path == apart.join(&name)),
            "the after-close depth pass still sees the set-apart segment"
        );
        // The next process (a fresh state) tries it again and recovers it.
        let mut next = SettleState::new();
        assert_eq!(next.recover(&dir, &wm), 1);
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_settle_and_promote_with_nothing_staged() {
        let dir = tmp("empty");
        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        assert_eq!(st.promote(&wm, Instant::now()), SettleVerdict::Idle);
        assert_eq!(
            st.settle(&dir, &wm, Instant::now(), SettleBatchStart::of(&wm)),
            0
        );
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(
            st.promote(&wm, lagged(Instant::now())),
            SettleVerdict::Promoted(0)
        );
        assert_eq!(st.promote(&wm, Instant::now()), SettleVerdict::Idle);
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_leftovers_from_an_earlier_process_hold_the_first_settle() {
        let dir = tmp("leftover");
        std::fs::create_dir_all(dir.join(SETTLING_SUBDIR)).expect("mkdir"); // APPROVED: test-only
        plant(
            &dir.join(SETTLING_SUBDIR),
            "ws-frames-00000000000000000004.wal",
        );
        plant(
            &dir.join(REPLAYING_SUBDIR),
            "ws-frames-00000000000000000005.wal",
        );
        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        st.settle(&dir, &wm, Instant::now(), SettleBatchStart::of(&wm));
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        assert_eq!(
            st.promote(&wm, lagged(Instant::now())),
            SettleVerdict::Held,
            "unrecovered leftovers must never be archived with this batch"
        );
        assert_eq!(names(&dir.join(SETTLING_SUBDIR)), 2);
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_recover_does_nothing_without_leftovers_or_after_a_settle() {
        let dir = tmp("noop");
        let wm = AppliedWatermark::new_for_tests();
        let mut st = SettleState::new();
        assert_eq!(st.recover(&dir, &wm), 0);
        st.settle(&dir, &wm, Instant::now(), SettleBatchStart::of(&wm));
        std::fs::create_dir_all(dir.join(SETTLING_SUBDIR)).expect("mkdir"); // APPROVED: test-only
        plant(
            &dir.join(SETTLING_SUBDIR),
            "ws-frames-00000000000000000006.wal",
        );
        assert_eq!(
            st.recover(&dir, &wm),
            0,
            "after a settle, settling/ holds this process's own batches"
        );
        assert_eq!(names(&dir.join(SETTLING_SUBDIR)), 1);
        drop(std::fs::remove_dir_all(&dir));
    }
}
