//! Wall-clock steps this process has seen (PR AA review, finding 2,
//! 2026-11-08; reworked on its recheck the same day).
//!
//! # Why
//!
//! The archive's in-flight check maps an hour to frame sequences
//! (`partition_archive::snapshot_gate::hour_frame_seq_span`), assuming a
//! row's receipt time and its frame sequence differ by about 2 s. A wall
//! clock step breaks that:
//!
//! * Back by S: a frame sequence is `max(previous + 1, wall)`, so it stays
//!   at the old time until the wall catches up, while the receipt anchor
//!   takes the step at its next refresh. Rows land in the earlier hour with
//!   sequences of the later one.
//! * Forward by S: the sequence jumps at once (it reads the wall per frame);
//!   the receipt follows at the next refresh. Until then rows land in the
//!   earlier hour with sequences S later.
//!
//! A shed or deferred mark on such a row lies outside the hour's span, so
//! the hour read clear and could be dropped while a row of it was still on
//! its way. `ClockAhead` only compares the newest sequence with the wall at
//! archive time, which has cleared by then, and never sees a forward step.
//!
//! # What this records
//!
//! One window `[lo, hi]` of wall nanos (UTC): the union of every step seen.
//! A step seen at a receipt anchor refresh is widened from the PREVIOUS
//! anchor's time (where the step can have begun: the refresh runs on a timer
//! arm a busy ring can starve, so it can be late) to the new anchor's time
//! plus the step, with one refresh interval ([`CLOCK_STEP_MARGIN_NANOS`]) of
//! margin each side. A step seen at a WAL segment rotation (the new name or
//! the frame counter ahead of the wall: a backward step, or a restart after
//! one) is widened by its size plus the margin both ways from the rotation.
//!
//! # How the archive reads it
//!
//! An hour whose span overlaps the window is judged against the WHOLE
//! window as well as its own span: it is held
//! (`InFlightVerdict::ClockStepped`) while any frame check (after-close
//! depth marks, unapplied ranges, a rescue floor, an acknowledgement that has
//! not passed the window) would hold a frame of the window. So the hold ends
//! once nothing the step could have displaced is still on its way, and the
//! window itself is dropped once it reads settled for both sinks
//! ([`retire_clock_step_window_if_settled`]).
//!
//! # Persistence
//!
//! The window is written to [`CLOCK_STEP_FILE`] beside `applied.tvaw`
//! (fresh temp file, `fsync`, rename) by the tick writer thread at most once
//! a second, only when it changed, and read back when the WAL directory is
//! bound at boot, before any sink or socket exists. The unapplied ranges and
//! the deferred-depth marks it guards survive a restart, so it must too. An
//! unreadable file is replaced by the window from
//! [`REJECTED_FILE_LOOKBACK_NANOS`] (24 h) before the last process's newest
//! WAL receipt (the boot when the WAL holds none) up to the later of that
//! receipt and the boot ([`rejected_file_window`]), with one coded warning:
//! every step of the session the last process ran in.
//!
//! # Retirement
//!
//! A window is dropped only once it reads settled for both sinks
//! ([`retire_clock_step_window_if_settled`]), never on a wall-clock timer.
//! The recheck of PR AA finding 2 (2026-11-08) had added a 24 h expiry; its
//! own recheck the same day showed it reopened the path it guards: a row a
//! step displaced and still unapplied after 24 h (a backlog that has not
//! drained in a day, which this box has measured as normal: 225 GB pinned 21
//! to 26 Oct) was then judged by its own hour's span only, whose ranges and
//! acknowledgements miss the displaced sequences, so the hour could be
//! dropped with that row still in the WAL. The expiry was also read on the
//! wall clock the window records steps of. So there is none.
//!
//! # Honest limits
//!
//! - A step recorded less than about a second before a crash, or while the
//!   tick writer writes nothing, can miss the file. Rows need frames flowing
//!   to be displaced, and frames flowing means the tick writer writes.
//! - Two steps far apart are one window over both (the union keeps more,
//!   never less), so every hour between them is judged against all of it.
//! - A step smaller than 1 s is not recorded; the span's own 2 s receipt
//!   slop covers it.
//! - Without an expiry a window holds the hours it reaches for as long as any
//!   frame of it is unapplied, marked, under a rescue floor or not yet
//!   acknowledged; on a box with a multi-day WAL backlog that can be days.
//!   The pressure leg then frees other hours only. That is the price of never
//!   dropping a displaced row unapplied (Quote 51); the held set is bounded
//!   by the window (one step's reach plus margins, or one session for an
//!   unreadable file), never every hour before the boot.
//! - An unreadable file covers only steps in the 24 h before the last
//!   process's newest WAL receipt. A step in an earlier session whose
//!   window had not retired when the file was lost is not covered, and an
//!   unreadable newest segment makes the boot the anchor. An unreadable file
//!   needs a torn write or a rollback across a version change.
//! - The frame counter can run ahead of the wall without a step under a
//!   burst above about 7,600 frames a second (one 131 µs frame base each).
//!   A rotation then records a small spurious window; it is retired with
//!   the rest of the union, so it holds while any frame of the union is on
//!   its way.
//! - After a backward step the frame counter stays ahead of the wall for
//!   S/(1 - r) seconds (r: frame bases spent per second) and climbs past the
//!   top the step recorded. While a window is recorded, each tick-writer
//!   batch raises the top over the counter
//!   ([`ClockStepLog::follow_counter_lead`]), so the window covers every
//!   sequence the counter reached by the last batch that saw it ahead. A
//!   tick writer stalled for longer than one margin of counter advance
//!   while the counter is ahead (30 s of sequence, about 200 s of wall at
//!   r = 0.15) leaves the sequences of that stretch above the top until its
//!   next batch, unless a segment rotation records them first.
//!
//! # Complexity
//!
//! O(1): one uncontended lock per recorded step (rare: a refresh every 30 s
//! or a segment rotation that finds the clock ahead), one lock per archive
//! verdict, two atomic loads per tick-writer batch while no window is
//! recorded (one more load and one clock read while one is, and the lock
//! only when the counter passes the top less one margin).
//! Never on the frame path. An unreadable file adds one header walk of the
//! newest WAL segments at boot (`wal_last_receipt`), cold.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use tracing::{info, warn};

/// The persisted window, beside `applied.tvaw` in the WAL directory.
pub const CLOCK_STEP_FILE: &str = "clock_step.tvcs";

/// The temp file [`CLOCK_STEP_FILE`] is written through.
pub const CLOCK_STEP_TMP: &str = "clock_step.tvcs.tmp";

/// How far past a step its effect can reach: one receipt anchor refresh
/// interval (the refresh runs every 30 s), since the anchor takes a step only
/// at its next refresh.
pub(crate) const CLOCK_STEP_MARGIN_NANOS: u64 = 30_000_000_000;

/// A move of the clock this size or smaller is not a step: the span's 2 s
/// receipt slop already covers it, and NTP slew over one 30 s refresh is
/// about 15 ms.
pub(crate) const CLOCK_STEP_MIN_NANOS: u64 = 1_000_000_000;

/// How far before the last process's newest WAL receipt the window of an
/// unreadable file reaches: 24 h. A process runs at most from the 08:30 start
/// to the 07:30 drain-hold limit (23 h), so this covers the whole session the
/// last process ran in, whatever the gap to this boot (a weekend included).
/// A step in an earlier session is not covered.
pub(crate) const REJECTED_FILE_LOOKBACK_NANOS: u64 = 24 * 3_600 * 1_000_000_000;

/// The window an unreadable file is replaced by: from
/// [`REJECTED_FILE_LOOKBACK_NANOS`] before the earlier of `last_receipt_nanos`
/// (the newest receipt on the WAL, `None` when it holds none) and the boot,
/// up to the later of the two (a clock that reads behind the WAL at the boot
/// stepped back across the restart). Pure, O(1).
#[must_use]
pub(crate) fn rejected_file_window(boot_nanos: u64, last_receipt_nanos: Option<u64>) -> (u64, u64) {
    let anchor = last_receipt_nanos.unwrap_or(boot_nanos);
    (
        anchor
            .min(boot_nanos)
            .saturating_sub(REJECTED_FILE_LOOKBACK_NANOS),
        anchor.max(boot_nanos),
    )
}

/// File layout: magic, version, `lo`, `hi`, CRC-32 of the first 24 bytes.
const FILE_MAGIC: [u8; 4] = *b"TVCS";
const FILE_VERSION: u32 = 1;
const FILE_LEN: usize = 28;

/// The window one step can affect, or `None` when the move is too small to
/// be a step. `start_nanos` is the earliest wall time the step can have
/// begun (the previous anchor), `at_wall_nanos` where it was seen, and
/// `magnitude_nanos` its size. Pure, O(1).
#[must_use]
pub(crate) const fn step_window(
    start_nanos: u64,
    at_wall_nanos: u64,
    magnitude_nanos: u64,
) -> Option<(u64, u64)> {
    if magnitude_nanos <= CLOCK_STEP_MIN_NANOS {
        return None;
    }
    let reach = magnitude_nanos.saturating_add(CLOCK_STEP_MARGIN_NANOS);
    let from_start = start_nanos.saturating_sub(CLOCK_STEP_MARGIN_NANOS);
    let from_seen = at_wall_nanos.saturating_sub(reach);
    let lo = if from_start < from_seen {
        from_start
    } else {
        from_seen
    };
    Some((lo, at_wall_nanos.saturating_add(reach)))
}

/// Whether `window` overlaps the inclusive span `[lo, hi]`. Pure, O(1).
#[must_use]
pub(crate) const fn window_overlaps(window: Option<(u64, u64)>, lo: u64, hi: u64) -> bool {
    match window {
        Some((w_lo, w_hi)) => w_lo <= hi && lo <= w_hi,
        None => false,
    }
}

/// How far ahead of the wall `now_nanos` a rotation finds the clock-derived
/// state: the new segment's name (above the wall only after a clamp) or the
/// frame counter. `0` when neither is ahead. Pure, O(1).
#[must_use]
pub(crate) fn rotation_lead_nanos(now_nanos: u128, named_nanos: u128, frame_seq: u64) -> u64 {
    let ahead = named_nanos.max(u128::from(frame_seq));
    u64::try_from(ahead.saturating_sub(now_nanos)).unwrap_or(u64::MAX)
}

/// The file bytes for `window` (`None` is written as `lo > hi`). Pure.
fn encode(window: Option<(u64, u64)>) -> [u8; FILE_LEN] {
    let (lo, hi) = window.unwrap_or((u64::MAX, 0));
    let mut out = [0_u8; FILE_LEN];
    out[0..4].copy_from_slice(&FILE_MAGIC);
    out[4..8].copy_from_slice(&FILE_VERSION.to_le_bytes());
    out[8..16].copy_from_slice(&lo.to_le_bytes());
    out[16..24].copy_from_slice(&hi.to_le_bytes());
    let crc = crate::wal_applied_watermark::crc32_ieee(&out[0..24]);
    out[24..28].copy_from_slice(&crc.to_le_bytes());
    out
}

/// What a read of the file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepFile {
    /// No file: no step was ever recorded here.
    Absent,
    /// A valid file and the window it holds.
    Loaded(Option<(u64, u64)>),
    /// A file that cannot be trusted (wrong size, magic, version or CRC, or
    /// an I/O error other than "not found").
    Rejected,
}

/// Decodes the file bytes. Pure.
fn decode(bytes: &[u8]) -> Option<Option<(u64, u64)>> {
    if bytes.len() != FILE_LEN || bytes[0..4] != FILE_MAGIC {
        return None;
    }
    let word = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    let version = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?);
    let crc = u32::from_le_bytes(bytes.get(24..28)?.try_into().ok()?);
    if version != FILE_VERSION || crc != crate::wal_applied_watermark::crc32_ieee(&bytes[0..24]) {
        return None;
    }
    let (lo, hi) = (word(8)?, word(16)?);
    Some((lo <= hi).then_some((lo, hi)))
}

fn read_file(wal_dir: &Path) -> StepFile {
    // APPROVED: boot-time bind, one 28-byte read, cold
    match std::fs::read(wal_dir.join(CLOCK_STEP_FILE)) {
        Ok(bytes) => decode(&bytes).map_or(StepFile::Rejected, StepFile::Loaded),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => StepFile::Absent,
        Err(_) => StepFile::Rejected,
    }
}

/// The union of two optional windows. Pure, O(1).
fn union(a: Option<(u64, u64)>, b: Option<(u64, u64)>) -> Option<(u64, u64)> {
    match (a, b) {
        (Some((a_lo, a_hi)), Some((b_lo, b_hi))) => Some((a_lo.min(b_lo), a_hi.max(b_hi))),
        (one, None) | (None, one) => one,
    }
}

/// One step log. The process uses one ([`note_clock_step`] and the other
/// free functions); tests build their own so they never share state.
#[derive(Debug)]
pub(crate) struct ClockStepLog {
    window: Mutex<Option<(u64, u64)>>,
    /// The window changed since the last write of the file.
    dirty: AtomicBool,
    /// `(file, temp file)` once bound to a WAL directory.
    paths: OnceLock<(PathBuf, PathBuf)>,
    /// A write is failing (one warning per episode).
    persist_failing: AtomicBool,
    /// A window is recorded (kept with `window` under its lock), so the tick
    /// writer reads the clock and the frame counter only while one exists.
    active: AtomicBool,
}

impl ClockStepLog {
    /// An empty, unbound log. `const` so the process instance needs no lazy
    /// setup.
    pub(crate) const fn new() -> Self {
        Self {
            window: Mutex::new(None),
            dirty: AtomicBool::new(false),
            paths: OnceLock::new(),
            persist_failing: AtomicBool::new(false),
            active: AtomicBool::new(false),
        }
    }

    /// The recorded window, or `None`. It is never dropped on a timer, only
    /// once it reads settled ([`Self::retire_if_settled`]). O(1).
    pub(crate) fn window(&self) -> Option<(u64, u64)> {
        *self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `add` to the window (the union keeps more, never less).
    fn widen(&self, add: Option<(u64, u64)>) -> Option<(u64, u64)> {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        let widened = union(*window, add);
        if widened != *window {
            *window = widened;
            self.dirty.store(true, Ordering::Release);
        }
        self.active.store(widened.is_some(), Ordering::Release);
        widened
    }

    /// Extends the window's top over the frame counter while the counter runs
    /// ahead of the wall (recheck 3 of PR AA finding 2). After a backward
    /// step of S the counter adds one frame base per frame, so at a frame
    /// rate below one base per base it stays ahead for S/(1 - r) seconds and
    /// climbs past the `at_wall + S + margin` the step recorded; rows received
    /// then carry sequences above the window's top. With a window recorded
    /// and the counter more than 1 s ahead, the top is raised to the counter
    /// plus two margins whenever the counter comes within one margin of it,
    /// so a row's sequence is inside the window whenever the counter was
    /// seen ahead at most one margin of counter advance after it. The low end
    /// is kept (lowered to `now_nanos` at most). No window or the counter
    /// not ahead: nothing. O(1); the lock only when the top moves, about once
    /// per 30 s of counter advance, so the file is rewritten as rarely.
    pub(crate) fn follow_counter_lead(&self, now_nanos: u64, frame_seq: u64) {
        if !self.active.load(Ordering::Acquire)
            || frame_seq.saturating_sub(now_nanos) <= CLOCK_STEP_MIN_NANOS
        {
            return;
        }
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((lo, hi)) = *window else {
            return;
        };
        if frame_seq.saturating_add(CLOCK_STEP_MARGIN_NANOS) <= hi {
            return;
        }
        let top = frame_seq.saturating_add(CLOCK_STEP_MARGIN_NANOS.saturating_mul(2));
        *window = Some((lo.min(now_nanos), top));
        self.dirty.store(true, Ordering::Release);
    }

    /// Records a step (see [`step_window`]); a move of 1 s or less is
    /// ignored. O(1).
    pub(crate) fn note(&self, start_nanos: u64, at_wall_nanos: u64, magnitude_nanos: u64) {
        let Some((lo, hi)) = step_window(start_nanos, at_wall_nanos, magnitude_nanos) else {
            return;
        };
        let window = self.widen(Some((lo, hi)));
        info!(
            step_ms = magnitude_nanos / 1_000_000,
            window_lo_nanos = lo,
            window_hi_nanos = hi,
            union = ?window,
            "wall clock step recorded: archive hours that overlap this window are kept \
             until nothing of the window is still on its way to QuestDB"
        );
    }

    /// Drops the window when `settled` and it is still exactly `seen` (a
    /// step recorded since is kept). Returns whether it was dropped. O(1).
    pub(crate) fn retire_if_settled(&self, seen: (u64, u64), settled: bool) -> bool {
        if !settled {
            return false;
        }
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        if *window != Some(seen) {
            return false;
        }
        *window = None;
        self.dirty.store(true, Ordering::Release);
        self.active.store(false, Ordering::Release);
        drop(window);
        info!(
            window_lo_nanos = seen.0,
            window_hi_nanos = seen.1,
            "wall clock step window retired: every frame of it is acknowledged and unmarked"
        );
        true
    }

    /// Binds the log to `wal_dir` and adds the window the file there holds,
    /// however old (it retires only once settled). First caller wins. An
    /// unreadable file adds [`rejected_file_window`] over the newest receipt
    /// on the WAL there and is rewritten at the next persist. Cold, once per
    /// process.
    pub(crate) fn bind(&self, wal_dir: &Path, now_nanos: u64) {
        let set = self
            .paths
            .set((wal_dir.join(CLOCK_STEP_FILE), wal_dir.join(CLOCK_STEP_TMP)))
            .is_ok();
        if !set {
            return;
        }
        match read_file(wal_dir) {
            StepFile::Absent => {}
            StepFile::Loaded(window) => {
                let window = self.widen(window);
                if window.is_some() {
                    info!(window = ?window, "wall clock step window carried over from the last process");
                }
            }
            StepFile::Rejected => {
                // Cold and rare: one header walk of the newest segments.
                let last_receipt = crate::wal_last_receipt::newest_receipt_nanos_on_disk(wal_dir)
                    .and_then(|nanos| u64::try_from(nanos).ok());
                let window = rejected_file_window(now_nanos, last_receipt);
                self.widen(Some(window));
                self.dirty.store(true, Ordering::Release);
                warn!(
                    code = tickvault_common::error_code::ErrorCode::WsSpill01WriterRespawn
                        .code_str(),
                    source = "clock_step_window_file_rejected",
                    path = %wal_dir.join(CLOCK_STEP_FILE).display(),
                    lookback_hours = REJECTED_FILE_LOOKBACK_NANOS / 3_600_000_000_000,
                    last_receipt_nanos = ?last_receipt,
                    window = ?window,
                    "wall clock step file cannot be read: archive hours from 24 hours before \
                     the last process's newest frame up to the boot are judged as stepped \
                     until every frame of them is acknowledged and unmarked"
                );
            }
        }
    }

    /// Writes the window when it changed since the last write. Unbound or
    /// unchanged: one load and nothing else. A failed write keeps it dirty
    /// and warns once per episode. Cold (tick writer thread, once a batch).
    pub(crate) fn persist_if_dirty(&self) {
        let Some((path, tmp)) = self.paths.get() else {
            return;
        };
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let bytes = encode(self.window());
        let written = crate::wal_applied_watermark::write_fresh(tmp, &bytes)
            .and_then(|()| std::fs::rename(tmp, path));
        match written {
            Ok(()) => {
                if self.persist_failing.swap(false, Ordering::AcqRel) {
                    info!(path = %path.display(), "wall clock step window is persisting again");
                }
            }
            Err(err) => {
                self.dirty.store(true, Ordering::Release);
                if !self.persist_failing.swap(true, Ordering::AcqRel) {
                    warn!(
                        code = tickvault_common::error_code::ErrorCode::WsSpill01WriterRespawn
                            .code_str(),
                        source = "clock_step_window_persist_failed",
                        path = %path.display(),
                        error = %err,
                        "wall clock step window could not be persisted: kept in RAM and \
                         retried; a restart before it lands forgets the step"
                    );
                }
            }
        }
    }
}

/// The process log.
static LOG: ClockStepLog = ClockStepLog::new();

/// Records a clock step on the process log. See [`ClockStepLog::note`].
pub(crate) fn note_clock_step(start_nanos: u64, at_wall_nanos: u64, magnitude_nanos: u64) {
    LOG.note(start_nanos, at_wall_nanos, magnitude_nanos);
}

/// The process window `[lo, hi]`, or `None` when no step is recorded or the
/// window retired. Never read against the wall clock. O(1).
#[must_use]
pub(crate) fn clock_step_window() -> Option<(u64, u64)> {
    LOG.window()
}

/// Drops the process window when it reads settled. See
/// [`ClockStepLog::retire_if_settled`].
pub(crate) fn retire_clock_step_window_if_settled(seen: (u64, u64), settled: bool) -> bool {
    LOG.retire_if_settled(seen, settled)
}

/// Binds the process log to the WAL directory and carries the last process's
/// window over (WAL boot, before any sink or socket exists).
pub(crate) fn bind_clock_step_log(wal_dir: &Path) {
    LOG.bind(wal_dir, crate::wal_applied_watermark::wall_nanos());
}

/// Writes the process window when it changed (tick writer thread, at most
/// once a batch). First, while a window is recorded, raises its top over a
/// frame counter still ahead of the wall ([`ClockStepLog::follow_counter_lead`]:
/// one clock read and one atomic load). No window and nothing changed: two
/// atomic loads.
pub(crate) fn persist_clock_step_log_if_dirty() {
    if LOG.active.load(Ordering::Acquire) {
        LOG.follow_counter_lead(
            crate::wal_applied_watermark::wall_nanos(),
            crate::ws_frame_spill::current_frame_seq(),
        );
    }
    LOG.persist_if_dirty();
}

/// Called at every WAL segment rotation (cold, writer thread): records a
/// step when the name or the frame counter is ahead of the wall. O(1).
pub(crate) fn note_segment_rotation(now_nanos: u128, named_nanos: u128) {
    note_rotation_on(
        &LOG,
        now_nanos,
        named_nanos,
        crate::ws_frame_spill::current_frame_seq(),
    );
}

fn note_rotation_on(log: &ClockStepLog, now_nanos: u128, named_nanos: u128, frame_seq: u64) {
    let lead = rotation_lead_nanos(now_nanos, named_nanos, frame_seq);
    let now = u64::try_from(now_nanos).unwrap_or(u64::MAX);
    log.note(now, now, lead);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-clock-step-{tag}-{}-{}",
            std::process::id(),
            crate::wal_applied_watermark::wall_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn test_step_window_ignores_slew_and_widens_a_step_both_ways() {
        assert_eq!(step_window(100 * SEC, 100 * SEC, SEC), None);
        assert_eq!(step_window(100 * SEC, 100 * SEC, 0), None);
        assert_eq!(
            step_window(1_000 * SEC, 1_000 * SEC, 600 * SEC),
            Some((370 * SEC, 1_630 * SEC))
        );
        // Saturates at both ends instead of wrapping.
        assert_eq!(step_window(0, 0, 10 * SEC), Some((0, 40 * SEC)));
        assert_eq!(
            step_window(u64::MAX, u64::MAX, 10 * SEC),
            Some((u64::MAX - 40 * SEC, u64::MAX))
        );
    }

    /// Recheck of finding 2: a forward step whose refresh ran 60 s after the
    /// previous anchor (a busy ring starves the timer arm). The step began
    /// 10 s after that anchor, before `new - S - margin`; the window must
    /// reach back to the previous anchor.
    #[test]
    fn test_step_window_reaches_back_to_the_previous_anchor_on_a_late_refresh() {
        let old = 10_000 * SEC;
        let step_began = old + 10 * SEC;
        let new = old + 60 * SEC + 600 * SEC;
        let (lo, hi) = step_window(old, new, 600 * SEC).expect("a step");
        assert!(
            lo <= step_began,
            "the window starts where the step can have begun"
        );
        assert_eq!(lo, old - CLOCK_STEP_MARGIN_NANOS);
        assert_eq!(hi, new + 630 * SEC);
        // Without the previous anchor the window would start after the step.
        let (seen_only, _) = step_window(new, new, 600 * SEC).expect("a step");
        assert!(seen_only > step_began);
    }

    #[test]
    fn test_window_overlaps_is_inclusive_and_none_never_overlaps() {
        assert!(!window_overlaps(None, 0, u64::MAX));
        let w = Some((10, 20));
        assert!(window_overlaps(w, 0, 10));
        assert!(window_overlaps(w, 20, 30));
        assert!(window_overlaps(w, 12, 14));
        assert!(window_overlaps(w, 0, 100));
        assert!(!window_overlaps(w, 0, 9));
        assert!(!window_overlaps(w, 21, 30));
    }

    #[test]
    fn test_rotation_lead_nanos_reads_the_name_or_the_counter_ahead_of_the_wall() {
        let now = u128::from(1_000 * SEC);
        assert_eq!(rotation_lead_nanos(now, now, 999 * SEC), 0);
        assert_eq!(rotation_lead_nanos(now, now + 5, 0), 5);
        assert_eq!(rotation_lead_nanos(now, now, 1_600 * SEC), 600 * SEC);
        assert_eq!(rotation_lead_nanos(now, now + 7, 1_000 * SEC + 3), 7);
    }

    #[test]
    fn test_log_records_steps_as_one_union_and_ignores_slew() {
        let log = ClockStepLog::new();
        log.note(1_000 * SEC, 1_000 * SEC, SEC);
        assert_eq!(log.window(), None);
        log.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        assert_eq!(log.window(), Some((370 * SEC, 1_630 * SEC)));
        log.note(5_000 * SEC, 5_000 * SEC, 10 * SEC);
        assert_eq!(log.window(), Some((370 * SEC, 5_040 * SEC)));
    }

    /// Recheck of finding 2: the rotation source had no positive test. A
    /// rotation that finds the name or the counter 600 s ahead of the wall
    /// records the window around the rotation; nothing ahead records
    /// nothing.
    #[test]
    fn test_segment_rotation_records_a_step_when_the_clock_state_is_ahead() {
        let now = 50_000 * SEC;
        let log = ClockStepLog::new();
        note_rotation_on(&log, u128::from(now), u128::from(now), now - SEC);
        assert_eq!(log.window(), None, "nothing ahead of the wall");
        note_rotation_on(&log, u128::from(now), u128::from(now + 600 * SEC), 0);
        assert_eq!(log.window(), Some((now - 630 * SEC, now + 630 * SEC)));
        let log = ClockStepLog::new();
        note_rotation_on(&log, u128::from(now), u128::from(now), now + 600 * SEC);
        assert_eq!(log.window(), Some((now - 630 * SEC, now + 630 * SEC)));
    }

    /// Recheck 3 of finding 2: after a 600 s step back the frame counter can
    /// stay ahead of the wall for minutes and climb past the top the step
    /// recorded (`at_wall + S + margin`), so rows received before the
    /// counter meets the wall carry sequences above that top. Each batch that
    /// sees the counter ahead raises the top over it. Bites: with the follow
    /// a no-op, `row_seq` stays outside the window.
    #[test]
    fn test_regression_follow_counter_lead_keeps_the_window_top_over_a_counter_still_ahead() {
        let hms = |h: u64, m: u64, s: u64| ((h * 60 + m) * 60 + s) * SEC;
        let log = ClockStepLog::new();
        // No window: the counter far ahead records nothing.
        log.follow_counter_lead(hms(11, 50, 0), hms(12, 0, 0));
        assert_eq!(log.window(), None);
        log.note(hms(11, 58, 54), hms(11, 59, 34), 600 * SEC);
        let (lo, top) = log.window().expect("the step");
        assert_eq!(top, hms(11, 59, 34) + 630 * SEC);
        let row_seq = top + SEC;
        assert!(!window_overlaps(log.window(), row_seq, row_seq));
        // Counter not ahead (within 1 s of the wall), or ahead but well under
        // the top: unchanged and not rewritten.
        log.dirty.store(false, Ordering::Release);
        log.follow_counter_lead(hms(11, 59, 50), hms(11, 59, 50) + SEC);
        log.follow_counter_lead(hms(11, 59, 50), top - 2 * CLOCK_STEP_MARGIN_NANOS);
        assert_eq!(log.window(), Some((lo, top)));
        assert!(!log.dirty.load(Ordering::Acquire));
        // The counter comes within one margin of the top: raised over it.
        let seen = top - CLOCK_STEP_MARGIN_NANOS + SEC;
        log.follow_counter_lead(hms(11, 59, 55), seen);
        assert_eq!(log.window(), Some((lo, seen + 2 * CLOCK_STEP_MARGIN_NANOS)));
        assert!(
            log.dirty.load(Ordering::Acquire),
            "the new top is persisted"
        );
        // Every sequence the counter reached by that batch, plus one margin
        // of later advance, is inside.
        assert!(window_overlaps(
            log.window(),
            seen + CLOCK_STEP_MARGIN_NANOS,
            seen + CLOCK_STEP_MARGIN_NANOS
        ));
        assert!(window_overlaps(log.window(), row_seq, row_seq));
        // A retired window stops the follow.
        let now = log.window().expect("kept");
        assert!(log.retire_if_settled(now, true));
        log.follow_counter_lead(hms(12, 1, 0), hms(12, 30, 0));
        assert_eq!(log.window(), None);
    }

    #[test]
    fn test_retire_if_settled_drops_only_a_settled_unchanged_window() {
        let log = ClockStepLog::new();
        log.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        let seen = log.window().expect("recorded");
        assert!(!log.retire_if_settled(seen, false));
        assert_eq!(log.window(), Some(seen));
        // A step recorded after the read keeps the window.
        log.note(9_000 * SEC, 9_000 * SEC, 10 * SEC);
        assert!(!log.retire_if_settled(seen, true));
        let now = log.window().expect("still there");
        assert!(log.retire_if_settled(now, true));
        assert_eq!(log.window(), None);
    }

    /// The process wrappers drive the one process log. The steps are in the
    /// far future (year 2508), so no other test's acknowledgement can settle
    /// and retire them and no test hour overlaps them; the log only grows
    /// otherwise, so the assertions hold whatever else runs.
    #[test]
    fn test_process_log_note_clock_step_note_segment_rotation_bind_clock_step_log_persist_clock_step_log_if_dirty_retire_clock_step_window_if_settled()
     {
        let covers = |at: u64| {
            clock_step_window().is_some_and(|(lo, hi)| lo <= at - 630 * SEC && hi >= at + 630 * SEC)
        };
        let at = 17_000_000_000 * SEC;
        note_clock_step(at, at, 600 * SEC);
        assert!(covers(at), "{:?}", clock_step_window());
        let rotated = at + 10_000 * SEC;
        note_segment_rotation(u128::from(rotated), u128::from(rotated + 600 * SEC));
        assert!(covers(rotated), "{:?}", clock_step_window());
        let window = clock_step_window().expect("recorded");
        assert!(!retire_clock_step_window_if_settled(window, false));
        assert!(covers(at) && covers(rotated));
        // Bound (first bind wins, maybe another test's folder): the change is
        // written there.
        let dir = temp_dir("process");
        bind_clock_step_log(&dir);
        note_clock_step(at + 20_000 * SEC, at + 20_000 * SEC, 600 * SEC);
        persist_clock_step_log_if_dirty();
        let (path, _) = LOG.paths.get().expect("bound");
        let folder_left = path.parent().is_some_and(Path::exists);
        assert!(
            path.exists() || !folder_left,
            "{} written while its folder exists",
            path.display()
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_file_round_trips_and_refuses_damage() {
        for window in [None, Some((7, 9)), Some((0, u64::MAX))] {
            assert_eq!(decode(&encode(window)), Some(window));
        }
        let mut bytes = encode(Some((7, 9)));
        bytes[10] ^= 1;
        assert_eq!(decode(&bytes), None, "a flipped bit fails the CRC");
        assert_eq!(decode(&bytes[..FILE_LEN - 1]), None, "a short file");
        let mut wrong = encode(Some((7, 9)));
        wrong[0] = b'X';
        assert_eq!(decode(&wrong), None, "wrong magic");
    }

    /// Regression: recheck of PR AA finding 2 (2026-11-08). The window lived
    /// in RAM only while the unapplied ranges and the deferred-depth marks it
    /// guards survive a restart, so after a restart the hour read clear. The
    /// window now persists and a new process carries it over.
    #[test]
    fn test_regression_window_survives_a_restart() {
        let dir = temp_dir("restart");
        let first = ClockStepLog::new();
        first.bind(&dir, 0);
        first.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        let recorded = first.window();
        first.persist_if_dirty();
        assert!(dir.join(CLOCK_STEP_FILE).exists(), "the step is written");
        assert!(
            !dir.join(CLOCK_STEP_TMP).exists(),
            "the temp file is renamed"
        );

        let restarted = ClockStepLog::new();
        restarted.bind(&dir, 2_000 * SEC);
        assert_eq!(restarted.window(), recorded, "the restart keeps the step");

        // A retired window is written too, so the next restart has none.
        let seen = restarted.window().expect("carried over");
        assert!(restarted.retire_if_settled(seen, true));
        restarted.persist_if_dirty();
        let third = ClockStepLog::new();
        third.bind(&dir, 3_000 * SEC);
        assert_eq!(third.window(), None);
        drop(std::fs::remove_dir_all(&dir));
    }

    /// The newest-receipt anchor for [`rejected_file_window`]: the window runs
    /// from 24 h before the earlier of the receipt and the boot to the later
    /// of the two, and saturates at the epoch.
    #[test]
    fn test_rejected_file_window_spans_the_last_session_up_to_the_boot() {
        let hour = 3_600 * SEC;
        assert_eq!(REJECTED_FILE_LOOKBACK_NANOS, 24 * hour);
        let boot = 1_000 * hour;
        assert_eq!(rejected_file_window(boot, None), (boot - 24 * hour, boot));
        let last = boot - 16 * hour;
        assert_eq!(
            rejected_file_window(boot, Some(last)),
            (last - 24 * hour, boot)
        );
        // A clock that reads behind the WAL at the boot (a backward step
        // across the restart): the window reaches up to the WAL's receipt.
        let ahead = boot + 2 * hour;
        assert_eq!(
            rejected_file_window(boot, Some(ahead)),
            (boot - 24 * hour, ahead)
        );
        assert_eq!(rejected_file_window(4_000 * SEC, None), (0, 4_000 * SEC));
    }

    /// Regression: recheck of the bounded fallback (2026-11-08). An unreadable
    /// file became `[boot - 6 h, boot]`, which at a normal 08:30 boot covers
    /// nothing of the session that ended at 16:30 the day before. It now
    /// reaches 24 h back from the last process's newest WAL receipt, so the
    /// whole last session is covered, and it is rewritten whole.
    #[test]
    fn test_regression_unreadable_file_covers_the_whole_last_session() {
        use crate::ws_frame_spill::{WalEndpoint, WsFrameSpill, WsType};
        let hour = 3_600 * SEC;
        // The last session's final frame at 2026-11-06 16:30 IST (11:00 UTC),
        // the boot at 08:30 IST the next morning, 16 h later.
        let last = 1_793_962_800 * SEC;
        let boot = last + 16 * hour;
        let session_start = last - 8 * hour;
        let written = temp_dir("rejected-spill");
        {
            let spill = WsFrameSpill::new(&written).expect("spill");
            let seq = crate::ws_frame_spill::next_frame_seq();
            let at = i64::try_from(last).expect("fits");
            spill.append_with_seq_at(
                WsType::LiveFeed,
                vec![1u8; 16],
                seq,
                at,
                WalEndpoint::MainFeed,
            );
            spill.shutdown(std::time::Duration::from_secs(5));
        }
        // A fresh folder holding only the segments, bound by nothing else.
        let dir = temp_dir("rejected");
        for entry in std::fs::read_dir(&written).expect("list").flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "wal") {
                std::fs::copy(&path, dir.join(entry.file_name())).expect("copy segment");
            }
        }
        std::fs::write(dir.join(CLOCK_STEP_FILE), b"torn").expect("write");
        let log = ClockStepLog::new();
        log.bind(&dir, boot);
        let expected = (last - REJECTED_FILE_LOOKBACK_NANOS, boot);
        assert_eq!(log.window(), Some(expected));
        assert!(
            window_overlaps(log.window(), session_start, session_start + hour),
            "the morning of the last session is judged as stepped"
        );
        assert!(
            !window_overlaps(log.window(), 0, expected.0 - 1),
            "an hour before the last session is not"
        );
        log.persist_if_dirty();
        let again = ClockStepLog::new();
        again.bind(&dir, boot + 30 * 24 * hour);
        assert_eq!(again.window(), Some(expected), "rewritten whole, kept");
        drop(std::fs::remove_dir_all(&written));
        drop(std::fs::remove_dir_all(&dir));
    }

    /// Regression: recheck of the 24 h expiry (2026-11-08). A window expired
    /// 24 h after its `hi`, on the wall clock, so an hour whose displaced row
    /// was still unapplied a day later was judged by its own span and could be
    /// dropped with the row in the WAL, and a clock read a day ahead at boot
    /// erased the record. A window now stays, at a restart and in RAM, until
    /// it reads settled, however far the wall clock has moved.
    #[test]
    fn test_regression_a_window_is_kept_until_settled_whatever_the_wall_clock() {
        let day = 24 * 3_600 * SEC;
        let dir = temp_dir("kept");
        let first = ClockStepLog::new();
        first.bind(&dir, 0);
        first.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        let recorded = first.window().expect("recorded");
        first.persist_if_dirty();

        // A boot a month later (or a clock read far ahead) keeps it.
        let restarted = ClockStepLog::new();
        restarted.bind(&dir, recorded.1 + 30 * day);
        assert_eq!(restarted.window(), Some(recorded));
        restarted.persist_if_dirty();
        let third = ClockStepLog::new();
        third.bind(&dir, recorded.1 + 60 * day);
        assert_eq!(third.window(), Some(recorded), "the file still holds it");

        // A second step days later joins the first instead of replacing it.
        let later = recorded.1 + 2 * day;
        restarted.note(later, later, 10 * SEC);
        assert_eq!(
            restarted.window(),
            Some((recorded.0, later + 40 * SEC)),
            "the union keeps the earlier step"
        );

        // Only a settled read retires it.
        let seen = restarted.window().expect("kept");
        assert!(!restarted.retire_if_settled(seen, false));
        assert!(restarted.retire_if_settled(seen, true));
        assert_eq!(restarted.window(), None);
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_persist_if_dirty_writes_nothing_unbound_or_clean() {
        let log = ClockStepLog::new();
        log.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        log.persist_if_dirty();
        assert!(log.dirty.load(Ordering::Acquire), "kept until bound");
        let dir = temp_dir("clean");
        let clean = ClockStepLog::new();
        clean.bind(&dir, 0);
        clean.persist_if_dirty();
        assert!(
            !dir.join(CLOCK_STEP_FILE).exists(),
            "nothing recorded, nothing written"
        );
        drop(std::fs::remove_dir_all(&dir));
    }
}
