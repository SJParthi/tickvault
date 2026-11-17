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
//! ⚠ CHANGED 2026-11-16 (live, 16 Nov 2026): the one union is now up to
//! [`MAX_STEP_WINDOWS`] (8) SEPARATE windows ([`StepWindows`], a fixed
//! array). A new window merges only with the windows it overlaps or lies
//! within one margin of; past 8 the two closest windows are merged (keeps
//! more, never less). Why: a 1.76 s step on 12 Nov (16:11 IST, right after
//! the after-close restart) never retired, and the 16 Nov step (1.1 s, 9 s
//! after the same daily restart, while the late attach dialled 16,978
//! contracts) was merged into it as one span from 12 Nov to 16 Nov. Every
//! `market_depth_book` hour between (13 Nov 09..15, 14 Nov 14, 15 Nov 15)
//! was then kept as `clock_stepped` and the Brutex export for those days was
//! blocked. With separate windows an hour is judged only against the windows
//! it overlaps, and each window retires on its own.
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
//! ⚠ CHANGED 2026-11-16: "the window" above reads "each window the hour
//! overlaps, judged against THAT window" (`partition_archive::snapshot_gate`
//! loops over them), and each window is retired on its own read.
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
//! ⚠ CHANGED 2026-11-16: the file is version 2 (every window, 144 bytes);
//! the one-window version 1 file every earlier binary wrote is still read,
//! as ONE window (where its steps were is not known, so it is never split).
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
//! ⚠ CHANGED 2026-11-16 (why the 12 Nov window lived 4 days): "settled"
//! asks every frame check over the WHOLE window at once, for both sinks.
//! The union reached from 12 Nov to the newest step, so it needed every
//! frame of every session in between to be applied, acknowledged and
//! unmarked at the same moment: each restart's boot tail range, any shed
//! bucket and any suspicion range inside it held it, and the depth
//! acknowledgement (which stops at 15:40) never passes a window ending after
//! the 16:11 restart until the next session. And each new step was merged
//! in, so the window to settle kept growing. No single check was wrong;
//! the shape was. Each window now settles and retires on its own. A window
//! recorded seconds after a restart still overlaps that restart's boot tail
//! (its low end reaches one margin back), so it retires only once the catch
//! up has replayed the last process's final minutes; it then holds only the
//! hour it lies in, which the boot tail holds anyway.
//!
//! # Honest limits
//!
//! - A step recorded less than about a second before a crash, or while the
//!   tick writer writes nothing, can miss the file. Rows need frames flowing
//!   to be displaced, and frames flowing means the tick writer writes.
//! - ~~Two steps far apart are one window over both (the union keeps more,
//!   never less), so every hour between them is judged against all of it.~~
//!   ⚠ CHANGED 2026-11-16: two steps more than one margin apart are separate
//!   windows, so an hour between them is judged against neither. Past
//!   [`MAX_STEP_WINDOWS`] (8) windows the two closest merge, and then the
//!   hours between THOSE two are judged against the merged window (keeps
//!   more). A version 1 file read at boot is still one window over all the
//!   steps the old union held: it holds every hour inside it until all of it
//!   reads settled, and cannot be split safely.
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
//!   ⚠ CHANGED 2026-11-16: it is now its own window (about one minute
//!   wide) unless it lies within one margin of another, retired on its own.
//!   It is still recorded: a burst lead above the span's 2 s receipt slop
//!   displaces rows past their hour's span exactly as a step does, and
//!   telling a burst from a step by CLOCK_MONOTONIC was not taken (see
//!   [`ClockStepLog::follow_counter_lead`]).
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
//! - The follow is for the lead that began at a step recorded in this
//!   process (2026-11-15 audit, live-3). A lead with no recorded step behind
//!   it (a window carried over from the last process, or a burst lead after
//!   the counter had met the wall) is recorded as a new, logged step. It
//!   still joins the one window, so every hour between the old step and the
//!   new lead is judged against both and is still held as `clock_stepped`,
//!   exactly as before. The change is visibility only: the lead is now in
//!   the log ("wall clock step recorded") instead of raising the top
//!   silently. Stopping that hold needs more than one span per window.
//!   ⚠ CHANGED 2026-11-16: done: the new lead is its own window unless it
//!   lies within one margin of an old one, so the hours between are no
//!   longer held for it.
//!
//! # Complexity
//!
//! O(1) (⚠ CHANGED 2026-11-16: O([`MAX_STEP_WINDOWS`]) = O(8), a fixed
//! array; an insert is at most 8 merge passes of 8 compares, no
//! allocation; the archive judges an hour against each window it overlaps
//! and checks each window for retirement, at most 8 frame checks each): one
//! uncontended lock per recorded step (rare: a refresh every 30 s
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

/// The persisted windows, beside `applied.tvaw` in the WAL directory.
pub const CLOCK_STEP_FILE: &str = "clock_step.tvcs";

/// The temp file [`CLOCK_STEP_FILE`] is written through.
pub const CLOCK_STEP_TMP: &str = "clock_step.tvcs.tmp";

/// How far past a step its effect can reach: one receipt anchor refresh
/// interval (the refresh runs every 30 s), since the anchor takes a step only
/// at its next refresh. Also how close two windows may lie before they are
/// kept as one.
pub(crate) const CLOCK_STEP_MARGIN_NANOS: u64 = 30_000_000_000;

/// A move of the clock this size or smaller is not a step: the span's 2 s
/// receipt slop already covers it, and NTP slew over one 30 s refresh is
/// about 15 ms.
pub(crate) const CLOCK_STEP_MIN_NANOS: u64 = 1_000_000_000;

/// How many separate step windows are kept (2026-11-16). Past it the two
/// closest are merged into one (keeps more, never less).
pub(crate) const MAX_STEP_WINDOWS: usize = 8;

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

/// File layout, version 1 (one window, written up to 2026-11-15): magic,
/// version, `lo`, `hi`, CRC-32 of the first 24 bytes. Still read.
const FILE_MAGIC: [u8; 4] = *b"TVCS";
const FILE_VERSION_V1: u32 = 1;
const FILE_LEN_V1: usize = 28;
/// File layout, version 2 (2026-11-16): magic, version, count, then
/// [`MAX_STEP_WINDOWS`] `(lo, hi)` pairs (unused ones zero), then a CRC-32
/// of everything before it.
const FILE_VERSION: u32 = 2;
const FILE_WINDOWS_AT: usize = 12;
const FILE_CRC_AT: usize = FILE_WINDOWS_AT + MAX_STEP_WINDOWS * 16;
const FILE_LEN: usize = FILE_CRC_AT + 4;

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

/// Whether two windows are kept as one: they overlap or lie within one
/// [`CLOCK_STEP_MARGIN_NANOS`] of each other. Pure, O(1).
const fn near(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 <= b.1.saturating_add(CLOCK_STEP_MARGIN_NANOS)
        && b.0 <= a.1.saturating_add(CLOCK_STEP_MARGIN_NANOS)
}

/// The separate step windows (2026-11-16), each `[lo, hi]` in wall nanos
/// (UTC). Sorted by `lo`, never two within one margin of each other, at most
/// [`MAX_STEP_WINDOWS`]. A fixed array: `Copy`, no allocation, every
/// operation O([`MAX_STEP_WINDOWS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StepWindows {
    /// One spare slot so an insert can sort first and merge after.
    slots: [(u64, u64); MAX_STEP_WINDOWS + 1],
    len: usize,
}

impl StepWindows {
    /// No window recorded.
    pub(crate) const EMPTY: Self = Self {
        slots: [(0, 0); MAX_STEP_WINDOWS + 1],
        len: 0,
    };

    /// Whether any window is recorded. O(1).
    #[must_use]
    pub(crate) const fn is_recorded(&self) -> bool {
        self.len > 0
    }

    /// How many windows are recorded. O(1).
    #[must_use]
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// The windows, lowest first. O(1) per item.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.slots.iter().take(self.len).copied()
    }

    /// The windows that overlap the inclusive span `[lo, hi]`.
    pub(crate) fn overlapping(&self, lo: u64, hi: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.iter()
            .filter(move |&w| window_overlaps(Some(w), lo, hi))
    }

    /// The window that contains `at`, if any. O(windows).
    fn containing(&self, at: u64) -> Option<(u64, u64)> {
        self.iter().find(|&(lo, hi)| lo <= at && at <= hi)
    }

    /// Adds `add`: merged with every window it overlaps or lies within one
    /// margin of; past [`MAX_STEP_WINDOWS`] the two closest windows are
    /// merged (keeps more, never less). Whether anything changed.
    /// O(windows²) = O(64) compares at most, no allocation.
    fn insert(&mut self, add: (u64, u64)) -> bool {
        let before = *self;
        let mut merged = (add.0.min(add.1), add.0.max(add.1));
        // Fold in every near window until none is left (the hull can reach a
        // window the original did not).
        while let Some(at) = (0..self.len).find(|&i| near(self.slots[i], merged)) {
            let w = self.slots[at];
            merged = (merged.0.min(w.0), merged.1.max(w.1));
            self.remove_at(at);
        }
        let at = (0..self.len)
            .find(|&i| self.slots[i].0 > merged.0)
            .unwrap_or(self.len);
        let mut i = self.len;
        while i > at {
            self.slots[i] = self.slots[i - 1];
            i -= 1;
        }
        self.slots[at] = merged;
        self.len += 1;
        while self.len > MAX_STEP_WINDOWS {
            // Sorted and apart, so the closest pair is an adjacent one.
            let mut best = 0;
            let mut best_gap = u64::MAX;
            for i in 0..self.len - 1 {
                let gap = self.slots[i + 1].0.saturating_sub(self.slots[i].1);
                if gap < best_gap {
                    best_gap = gap;
                    best = i;
                }
            }
            let hi = self.slots[best].1.max(self.slots[best + 1].1);
            self.slots[best].1 = hi;
            self.remove_at(best + 1);
        }
        *self != before
    }

    /// Removes the window exactly equal to `window`. Whether it was there.
    fn remove(&mut self, window: (u64, u64)) -> bool {
        match (0..self.len).find(|&i| self.slots[i] == window) {
            Some(at) => {
                self.remove_at(at);
                true
            }
            None => false,
        }
    }

    fn remove_at(&mut self, at: usize) {
        for i in at..self.len - 1 {
            self.slots[i] = self.slots[i + 1];
        }
        self.len -= 1;
        self.slots[self.len] = (0, 0);
    }
}

impl From<Option<(u64, u64)>> for StepWindows {
    fn from(window: Option<(u64, u64)>) -> Self {
        let mut windows = Self::EMPTY;
        if let Some(window) = window {
            windows.insert(window);
        }
        windows
    }
}

/// The file bytes for `windows` (version 2). Pure.
fn encode(windows: &StepWindows) -> [u8; FILE_LEN] {
    let mut out = [0_u8; FILE_LEN];
    out[0..4].copy_from_slice(&FILE_MAGIC);
    out[4..8].copy_from_slice(&FILE_VERSION.to_le_bytes());
    let count = u32::try_from(windows.len()).unwrap_or(0);
    out[8..12].copy_from_slice(&count.to_le_bytes());
    for (i, (lo, hi)) in windows.iter().enumerate() {
        let at = FILE_WINDOWS_AT + i * 16;
        out[at..at + 8].copy_from_slice(&lo.to_le_bytes());
        out[at + 8..at + 16].copy_from_slice(&hi.to_le_bytes());
    }
    let crc = crate::wal_applied_watermark::crc32_ieee(&out[0..FILE_CRC_AT]);
    out[FILE_CRC_AT..FILE_LEN].copy_from_slice(&crc.to_le_bytes());
    out
}

/// What a read of the file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepFile {
    /// No file: no step was ever recorded here.
    Absent,
    /// A valid file and the windows it holds.
    Loaded(StepWindows),
    /// A file that cannot be trusted (wrong size, magic, version, count or
    /// CRC, or an I/O error other than "not found").
    Rejected,
}

/// Decodes the file bytes, version 2 or the one-window version 1. Pure.
fn decode(bytes: &[u8]) -> Option<StepWindows> {
    if bytes.get(0..4)? != FILE_MAGIC {
        return None;
    }
    let word = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    let half = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    match half(4)? {
        FILE_VERSION_V1 => {
            if bytes.len() != FILE_LEN_V1
                || half(24)? != crate::wal_applied_watermark::crc32_ieee(bytes.get(0..24)?)
            {
                return None;
            }
            let (lo, hi) = (word(8)?, word(16)?);
            Some(StepWindows::from((lo <= hi).then_some((lo, hi))))
        }
        FILE_VERSION => {
            if bytes.len() != FILE_LEN
                || half(FILE_CRC_AT)?
                    != crate::wal_applied_watermark::crc32_ieee(bytes.get(0..FILE_CRC_AT)?)
            {
                return None;
            }
            let count = usize::try_from(half(8)?).ok()?;
            if count > MAX_STEP_WINDOWS {
                return None;
            }
            let mut windows = StepWindows::EMPTY;
            for i in 0..count {
                let at = FILE_WINDOWS_AT + i * 16;
                let (lo, hi) = (word(at)?, word(at + 8)?);
                if lo > hi {
                    return None;
                }
                windows.insert((lo, hi));
            }
            Some(windows)
        }
        _ => None,
    }
}

fn read_file(wal_dir: &Path) -> StepFile {
    // APPROVED: boot-time bind, one read of at most 144 bytes, cold
    match std::fs::read(wal_dir.join(CLOCK_STEP_FILE)) {
        Ok(bytes) => decode(&bytes).map_or(StepFile::Rejected, StepFile::Loaded),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => StepFile::Absent,
        Err(_) => StepFile::Rejected,
    }
}

/// The log's state under its one lock.
#[derive(Debug)]
struct LogState {
    windows: StepWindows,
    /// The top of the window the frame counter's lead is followed in
    /// ([`ClockStepLog::follow_counter_lead`]); meaningful only while
    /// `following` is set.
    follow_hi: u64,
}

/// One step log. The process uses one ([`note_clock_step`] and the other
/// free functions); tests build their own so they never share state.
#[derive(Debug)]
pub(crate) struct ClockStepLog {
    state: Mutex<LogState>,
    /// The windows changed since the last write of the file.
    dirty: AtomicBool,
    /// `(file, temp file)` once bound to a WAL directory.
    paths: OnceLock<(PathBuf, PathBuf)>,
    /// A write is failing (one warning per episode).
    persist_failing: AtomicBool,
    /// A window is recorded (kept with `state` under its lock), so the tick
    /// writer reads the clock and the frame counter only while one exists.
    active: AtomicBool,
    /// The frame counter's lead over the wall began at a step recorded here
    /// ([`Self::note_from`]), so [`Self::follow_counter_lead`] may raise that
    /// step's window. Never set by [`Self::bind`]; cleared once the counter
    /// meets the wall or the followed window retires.
    following: AtomicBool,
}

impl ClockStepLog {
    /// An empty, unbound log. `const` so the process instance needs no lazy
    /// setup.
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(LogState {
                windows: StepWindows::EMPTY,
                follow_hi: 0,
            }),
            dirty: AtomicBool::new(false),
            paths: OnceLock::new(),
            persist_failing: AtomicBool::new(false),
            active: AtomicBool::new(false),
            following: AtomicBool::new(false),
        }
    }

    /// The recorded windows. A window is never dropped on a timer, only once
    /// it reads settled ([`Self::retire_if_settled`]). O(1) (one copy of a
    /// fixed array).
    pub(crate) fn windows(&self) -> StepWindows {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .windows
    }

    /// Adds `add` to the windows (merged with the windows near it; past the
    /// cap the two closest are merged). Returns the window now holding it.
    fn widen(&self, add: (u64, u64)) -> (u64, u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.windows.insert(add) {
            self.dirty.store(true, Ordering::Release);
        }
        self.active.store(true, Ordering::Release);
        state.windows.containing(add.1).unwrap_or(add)
    }

    /// Extends the followed window's top over the frame counter while the
    /// counter runs ahead of the wall (recheck 3 of PR AA finding 2). After a
    /// backward step of S the counter adds one frame base per frame, so at a
    /// frame rate below one base per base it stays ahead for S/(1 - r)
    /// seconds and climbs past the `at_wall + S + margin` the step recorded;
    /// rows received then carry sequences above the window's top. With a
    /// window recorded and the counter more than 1 s ahead, the followed
    /// window's top is raised to the counter plus two margins whenever the
    /// counter comes within one margin of it, so a row's sequence is inside
    /// the window whenever the counter was seen ahead at most one margin of
    /// counter advance after it. The window's low end is kept (lowered to
    /// `now_nanos` at most). No window or the counter not ahead: nothing.
    /// O(windows); the lock only when the top moves, about once per 30 s of
    /// counter advance, so the file is rewritten as rarely.
    ///
    /// Only the lead that began at a step this log recorded is followed
    /// (2026-11-15 audit, live-3). A lead with no step behind it (a window
    /// carried over from the last process, or a lead that started after the
    /// counter had met the wall: a burst above about 7,600 frames a second)
    /// is recorded as a NEW step through [`Self::note_from`], so it is logged
    /// ("wall clock step recorded"). ⚠ CHANGED 2026-11-16: that new step is
    /// now its own window unless it lies within one margin of an old one, so
    /// the hours between an old step and a new lead are no longer held for
    /// it (the 12 to 16 Nov live hold). Not joining a burst lead at all was
    /// not taken: a counter ahead of the wall is the case the window
    /// protects, and dropping it would keep less. The follow ends when the
    /// counter is at or behind the wall.
    pub(crate) fn follow_counter_lead(&self, now_nanos: u64, frame_seq: u64) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let lead = frame_seq.saturating_sub(now_nanos);
        if lead == 0 {
            self.following.store(false, Ordering::Release);
        }
        if lead <= CLOCK_STEP_MIN_NANOS {
            return;
        }
        if !self.following.load(Ordering::Acquire) {
            self.note_from("counter_lead", now_nanos, now_nanos, lead);
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.windows.is_recorded() {
            return;
        }
        if frame_seq.saturating_add(CLOCK_STEP_MARGIN_NANOS) <= state.follow_hi {
            return;
        }
        let top = frame_seq.saturating_add(CLOCK_STEP_MARGIN_NANOS.saturating_mul(2));
        // Starts at or below the followed window's top, so it joins it (or,
        // if that window retired, stands as a new one: keeps more).
        let add = (now_nanos.min(state.follow_hi), top);
        state.windows.insert(add);
        state.follow_hi = state.windows.containing(top).map_or(top, |(_, hi)| hi);
        self.dirty.store(true, Ordering::Release);
    }

    /// [`Self::note_from`] for a test (`source = "test"`).
    #[cfg(test)]
    pub(crate) fn note(&self, start_nanos: u64, at_wall_nanos: u64, magnitude_nanos: u64) {
        self.note_from("test", start_nanos, at_wall_nanos, magnitude_nanos);
    }

    /// Records a step (see [`step_window`]); a move of 1 s or less is
    /// ignored. O(windows).
    ///
    /// ⚠ CHANGED 2026-11-17 (review): `source` names which check saw it, in
    /// the log line: `receipt_anchor` (an anchor refresh moved the projected
    /// wall time), `segment_rotation` (at a WAL rotation the frame counter or
    /// the segment name was ahead of the wall) or `counter_lead` (the frame
    /// counter ran ahead of the wall with no step behind it, e.g. a burst
    /// above about 7,600 frames a second). The three wrote the same line
    /// before, so the live 1.0 to 1.1 s steps of 17 Nov could not be told
    /// apart.
    pub(crate) fn note_from(
        &self,
        source: &'static str,
        start_nanos: u64,
        at_wall_nanos: u64,
        magnitude_nanos: u64,
    ) {
        let Some((lo, hi)) = step_window(start_nanos, at_wall_nanos, magnitude_nanos) else {
            return;
        };
        let (windows, holding) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.windows.insert((lo, hi)) {
                self.dirty.store(true, Ordering::Release);
            }
            self.active.store(true, Ordering::Release);
            let holding = state.windows.containing(hi).unwrap_or((lo, hi));
            state.follow_hi = holding.1;
            self.following.store(true, Ordering::Release);
            (state.windows, holding)
        };
        info!(
            source,
            step_ms = magnitude_nanos / 1_000_000,
            window_lo_nanos = lo,
            window_hi_nanos = hi,
            held_as = ?holding,
            windows = windows.len(),
            "wall clock step recorded: archive hours that overlap this window are kept \
             until nothing of the window is still on its way to QuestDB"
        );
    }

    /// Drops the window `seen` when `settled` and it is still recorded
    /// exactly as `seen` (a window a later step merged into is kept). Other
    /// windows are untouched. Returns whether it was dropped. O(windows).
    pub(crate) fn retire_if_settled(&self, seen: (u64, u64), settled: bool) -> bool {
        if !settled {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.windows.remove(seen) {
            return false;
        }
        if seen.0 <= state.follow_hi && state.follow_hi <= seen.1 {
            self.following.store(false, Ordering::Release);
        }
        self.dirty.store(true, Ordering::Release);
        self.active
            .store(state.windows.is_recorded(), Ordering::Release);
        let left = state.windows.len();
        drop(state);
        info!(
            window_lo_nanos = seen.0,
            window_hi_nanos = seen.1,
            windows_left = left,
            "wall clock step window retired: every frame of it is acknowledged and unmarked"
        );
        true
    }

    /// Binds the log to `wal_dir` and adds the windows the file there holds,
    /// however old (each retires only once settled). A version 1 file holds
    /// one window (the old union) and is kept as one: where its steps were is
    /// not known, so it cannot be split. First caller wins. An unreadable
    /// file adds [`rejected_file_window`] over the newest receipt on the WAL
    /// there and is rewritten at the next persist. Cold, once per process.
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
            StepFile::Loaded(loaded) => {
                for window in loaded.iter() {
                    self.widen(window);
                }
                if loaded.is_recorded() {
                    info!(
                        windows = loaded.len(),
                        lowest = ?loaded.iter().next(),
                        highest = ?loaded.iter().last(),
                        "wall clock step windows carried over from the last process"
                    );
                }
            }
            StepFile::Rejected => {
                // Cold and rare: one header walk of the newest segments.
                let last_receipt = crate::wal_last_receipt::newest_receipt_nanos_on_disk(wal_dir)
                    .and_then(|nanos| u64::try_from(nanos).ok());
                let window = rejected_file_window(now_nanos, last_receipt);
                self.widen(window);
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

    /// Writes the windows when they changed since the last write. Unbound or
    /// unchanged: one load and nothing else. A failed write keeps it dirty
    /// and warns once per episode. Cold (tick writer thread, once a batch).
    pub(crate) fn persist_if_dirty(&self) {
        let Some((path, tmp)) = self.paths.get() else {
            return;
        };
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let bytes = encode(&self.windows());
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

/// Records a clock step on the process log. See [`ClockStepLog::note_from`].
pub(crate) fn note_clock_step(start_nanos: u64, at_wall_nanos: u64, magnitude_nanos: u64) {
    LOG.note_from(
        "receipt_anchor",
        start_nanos,
        at_wall_nanos,
        magnitude_nanos,
    );
}

/// The process's step windows (empty when no step is recorded or every
/// window retired). Never read against the wall clock. O(1).
#[must_use]
pub(crate) fn clock_step_windows() -> StepWindows {
    LOG.windows()
}

/// Drops one process window when it reads settled. See
/// [`ClockStepLog::retire_if_settled`].
pub(crate) fn retire_clock_step_window_if_settled(seen: (u64, u64), settled: bool) -> bool {
    LOG.retire_if_settled(seen, settled)
}

/// Binds the process log to the WAL directory and carries the last process's
/// windows over (WAL boot, before any sink or socket exists).
pub(crate) fn bind_clock_step_log(wal_dir: &Path) {
    LOG.bind(wal_dir, crate::wal_applied_watermark::wall_nanos());
}

/// Writes the process windows when they changed (tick writer thread, at most
/// once a batch). First, while a window is recorded, raises the followed
/// window's top over a frame counter still ahead of the wall
/// ([`ClockStepLog::follow_counter_lead`]: one clock read and one atomic
/// load). No window and nothing changed: two atomic loads.
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
    log.note_from("segment_rotation", now, now, lead);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    /// 2026-11-17 review: the three production checks that record a step
    /// each name themselves in the log line, so a live step can be traced to
    /// the anchor refresh, a segment rotation or a counter lead. Production
    /// never calls the test-only `note` wrapper.
    #[test]
    fn test_each_production_step_caller_names_its_source() {
        let src = include_str!("wal_clock_step.rs");
        let prod = &src[..src.find("#[cfg(test)]\nmod tests").expect("tests module")];
        let squeezed: String = prod.split_whitespace().collect();
        for source in ["receipt_anchor", "segment_rotation", "counter_lead"] {
            let call = format!("note_from(\"{source}\",");
            assert_eq!(squeezed.matches(call.as_str()).count(), 1, "{call}");
        }
        assert_eq!(squeezed.matches("note_from(\"test\",").count(), 1);
        assert_eq!(
            prod.matches(".note(").count(),
            0,
            "production records through note_from only"
        );
        assert!(prod.contains("            source,\n            step_ms"));
    }

    /// The log's only window (`None` when it holds none); fails when it
    /// holds more than one.
    fn one(log: &ClockStepLog) -> Option<(u64, u64)> {
        let windows = log.windows();
        assert!(windows.len() <= 1, "{windows:?}");
        windows.iter().next()
    }

    /// The windows as a plain list.
    fn list(windows: &StepWindows) -> Vec<(u64, u64)> {
        windows.iter().collect()
    }

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

    /// ⚠ CHANGED 2026-11-16: steps far apart are separate windows (was one
    /// union over both); a step within one margin of a window joins it.
    /// Bites: with `insert` folding every window into one, the second assert
    /// sees one window from 370 s to 5,040 s.
    #[test]
    fn test_log_records_far_steps_as_separate_windows_and_ignores_slew() {
        let log = ClockStepLog::new();
        log.note(1_000 * SEC, 1_000 * SEC, SEC);
        assert_eq!(one(&log), None);
        log.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        assert_eq!(one(&log), Some((370 * SEC, 1_630 * SEC)));
        log.note(5_000 * SEC, 5_000 * SEC, 10 * SEC);
        assert_eq!(
            list(&log.windows()),
            vec![(370 * SEC, 1_630 * SEC), (4_960 * SEC, 5_040 * SEC)]
        );
        // A step whose window lies within one margin of the first joins it.
        log.note(1_690 * SEC, 1_690 * SEC, 5 * SEC);
        assert_eq!(
            list(&log.windows()),
            vec![(370 * SEC, 1_725 * SEC), (4_960 * SEC, 5_040 * SEC)]
        );
    }

    /// 2026-11-16: the read accessors over a fixed set of windows, and the
    /// process reader starting from whatever the process log holds.
    #[test]
    fn test_step_windows_is_recorded_len_iter_overlapping_windows_and_clock_step_windows() {
        let empty = StepWindows::EMPTY;
        assert!(!empty.is_recorded());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.iter().count(), 0);
        assert_eq!(empty.overlapping(0, u64::MAX).count(), 0);
        let mut w = StepWindows::EMPTY;
        w.insert((100 * SEC, 200 * SEC));
        w.insert((1_000 * SEC, 1_100 * SEC));
        assert!(w.is_recorded());
        assert_eq!(w.len(), 2);
        assert_eq!(
            list(&w),
            vec![(100 * SEC, 200 * SEC), (1_000 * SEC, 1_100 * SEC)]
        );
        assert_eq!(
            w.overlapping(150 * SEC, 1_000 * SEC).count(),
            2,
            "inclusive"
        );
        assert_eq!(w.overlapping(201 * SEC, 999 * SEC).count(), 0, "between");
        assert_eq!(
            w.overlapping(1_050 * SEC, 1_050 * SEC).collect::<Vec<_>>(),
            vec![(1_000 * SEC, 1_100 * SEC)]
        );
        let log = ClockStepLog::new();
        log.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        assert_eq!(log.windows().len(), 1);
        // The process log only grows in this binary (far-future steps).
        let before = clock_step_windows().len();
        let at = 18_000_000_000 * SEC;
        note_clock_step(at, at, 5 * SEC);
        assert!(clock_step_windows().overlapping(at, at).count() == 1);
        assert!(clock_step_windows().len() >= before.min(MAX_STEP_WINDOWS));
    }

    /// 2026-11-16: two windows that overlap merge, and a window bridging two
    /// merges all three. Bites: with the merge loop removed, overlapping
    /// windows stand side by side.
    #[test]
    fn test_step_windows_merge_overlapping_and_bridging_windows() {
        let mut w = StepWindows::EMPTY;
        assert!(w.insert((100 * SEC, 200 * SEC)));
        assert!(w.insert((150 * SEC, 300 * SEC)));
        assert_eq!(list(&w), vec![(100 * SEC, 300 * SEC)]);
        assert!(!w.insert((120 * SEC, 130 * SEC)), "inside: unchanged");
        assert!(w.insert((1_000 * SEC, 1_100 * SEC)));
        assert_eq!(w.len(), 2);
        // Bridges the gap: all one window.
        assert!(w.insert((250 * SEC, 1_050 * SEC)));
        assert_eq!(list(&w), vec![(100 * SEC, 1_100 * SEC)]);
        // Exactly one margin apart joins; one nanosecond more stays apart.
        let mut w = StepWindows::EMPTY;
        w.insert((0, 10 * SEC));
        w.insert((10 * SEC + CLOCK_STEP_MARGIN_NANOS, 50 * SEC));
        assert_eq!(list(&w), vec![(0, 50 * SEC)]);
        w.insert((50 * SEC + CLOCK_STEP_MARGIN_NANOS + 1, 90 * SEC));
        assert_eq!(w.len(), 2);
        // Sorted by low end whatever the insert order.
        let mut w = StepWindows::EMPTY;
        w.insert((9_000 * SEC, 9_001 * SEC));
        w.insert((1_000 * SEC, 1_001 * SEC));
        w.insert((5_000 * SEC, 5_001 * SEC));
        assert_eq!(
            list(&w),
            vec![
                (1_000 * SEC, 1_001 * SEC),
                (5_000 * SEC, 5_001 * SEC),
                (9_000 * SEC, 9_001 * SEC)
            ]
        );
    }

    /// 2026-11-16: past `MAX_STEP_WINDOWS` the two CLOSEST windows merge
    /// (keeps more). Bites: merging the first pair instead of the closest
    /// leaves the 4,000/4,100 pair apart.
    #[test]
    fn test_step_windows_cap_overflow_merges_the_closest_pair() {
        let mut w = StepWindows::EMPTY;
        // Eight windows 1,000 s apart, except the pair at 4,000 / 4,100.
        let starts = [0_u64, 1_000, 2_000, 3_000, 4_000, 4_100, 6_000, 7_000];
        for s in starts {
            w.insert((s * SEC, s * SEC + 10 * SEC));
        }
        assert_eq!(w.len(), MAX_STEP_WINDOWS);
        w.insert((9_000 * SEC, 9_010 * SEC));
        assert_eq!(w.len(), MAX_STEP_WINDOWS);
        assert!(
            list(&w).contains(&(4_000 * SEC, 4_110 * SEC)),
            "the closest pair merged: {w:?}"
        );
        // Nothing lost: every inserted window is inside some window.
        for s in starts.into_iter().chain([9_000]) {
            assert!(
                w.iter()
                    .any(|(lo, hi)| lo <= s * SEC && s * SEC + 10 * SEC <= hi)
            );
        }
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
        assert_eq!(one(&log), None, "nothing ahead of the wall");
        note_rotation_on(&log, u128::from(now), u128::from(now + 600 * SEC), 0);
        assert_eq!(one(&log), Some((now - 630 * SEC, now + 630 * SEC)));
        let log = ClockStepLog::new();
        note_rotation_on(&log, u128::from(now), u128::from(now), now + 600 * SEC);
        assert_eq!(one(&log), Some((now - 630 * SEC, now + 630 * SEC)));
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
        assert_eq!(one(&log), None);
        log.note(hms(11, 58, 54), hms(11, 59, 34), 600 * SEC);
        let (lo, top) = one(&log).expect("the step");
        assert_eq!(top, hms(11, 59, 34) + 630 * SEC);
        let row_seq = top + SEC;
        assert!(!window_overlaps(one(&log), row_seq, row_seq));
        // Counter not ahead (within 1 s of the wall), or ahead but well under
        // the top: unchanged and not rewritten.
        log.dirty.store(false, Ordering::Release);
        log.follow_counter_lead(hms(11, 59, 50), hms(11, 59, 50) + SEC);
        log.follow_counter_lead(hms(11, 59, 50), top - 2 * CLOCK_STEP_MARGIN_NANOS);
        assert_eq!(one(&log), Some((lo, top)));
        assert!(!log.dirty.load(Ordering::Acquire));
        // The counter comes within one margin of the top: raised over it.
        let seen = top - CLOCK_STEP_MARGIN_NANOS + SEC;
        log.follow_counter_lead(hms(11, 59, 55), seen);
        assert_eq!(one(&log), Some((lo, seen + 2 * CLOCK_STEP_MARGIN_NANOS)));
        assert!(
            log.dirty.load(Ordering::Acquire),
            "the new top is persisted"
        );
        // Every sequence the counter reached by that batch, plus one margin
        // of later advance, is inside.
        assert!(window_overlaps(
            one(&log),
            seen + CLOCK_STEP_MARGIN_NANOS,
            seen + CLOCK_STEP_MARGIN_NANOS
        ));
        assert!(window_overlaps(one(&log), row_seq, row_seq));
        // A retired window stops the follow.
        let now = one(&log).expect("kept");
        assert!(log.retire_if_settled(now, true));
        log.follow_counter_lead(hms(12, 1, 0), hms(12, 30, 0));
        assert_eq!(one(&log), None);
    }

    /// 2026-11-15 audit (live-3): a 1.76 s step recorded on day 1 at 16:11 was
    /// carried over by the next day's process, and the open burst pushed the
    /// frame counter 2 s ahead of the wall. The follow raised the window's top
    /// to the next morning with no log. Now a carried-over window plus a
    /// counter ahead records a NEW step (logged, `following` set) through
    /// `note`. ⚠ CHANGED 2026-11-16: it is now a SEPARATE window (two steps
    /// a day apart), so the hours between them are not held for either
    /// (asserted below; the 12 to 16 Nov live hold). Bites: with the
    /// `following` check removed, the follow raises a window to `counter +
    /// 2 margins` with no `note`, so no log line.
    #[test]
    fn test_regression_follow_counter_lead_records_a_burst_lead_on_a_carried_window_as_a_new_step()
    {
        let day = 86_400 * SEC;
        let at = |d: u64, h: u64, m: u64| d * day + (h * 60 + m) * 60 * SEC;
        let dir = temp_dir("follow-carried");
        let first = ClockStepLog::new();
        first.bind(&dir, 0);
        first.note(at(1, 16, 11), at(1, 16, 11), 1_760_000_000);
        let old = one(&first).expect("the step");
        first.persist_if_dirty();

        let restarted = ClockStepLog::new();
        restarted.bind(&dir, at(2, 8, 30));
        assert_eq!(one(&restarted), Some(old), "carried over");
        assert!(
            !restarted.following.load(Ordering::Acquire),
            "bind never follows"
        );
        let wall = at(2, 9, 15);
        let counter = wall + 2 * SEC;
        restarted.follow_counter_lead(wall, counter);
        let new = step_window(wall, wall, 2 * SEC).expect("a step");
        assert_eq!(
            list(&restarted.windows()),
            vec![old, new],
            "a new step, not a raised top"
        );
        // ⚠ CHANGED 2026-11-16: two windows, so an hour between the two steps
        // (day 1 23:00) overlaps neither and is no longer held for them.
        // Bites: with the windows folded into one union, it overlaps.
        assert_eq!(
            restarted
                .windows()
                .overlapping(at(1, 23, 0), at(1, 23, 59))
                .count(),
            0
        );
        assert!(
            restarted.following.load(Ordering::Acquire),
            "the new lead is followed"
        );
        // The counter meets the wall: the follow ends, and a later lead is
        // another new step.
        restarted.follow_counter_lead(at(2, 10, 0), at(2, 10, 0));
        assert!(!restarted.following.load(Ordering::Acquire));
        drop(std::fs::remove_dir_all(&dir));
    }

    /// ⚠ CHANGED 2026-11-16: a settled window retires on its own; a step
    /// recorded after the read into ANOTHER window does not keep it, but a
    /// step merged INTO it (the window is no longer exactly `seen`) does.
    #[test]
    fn test_retire_if_settled_drops_only_a_settled_unchanged_window() {
        let log = ClockStepLog::new();
        log.note(1_000 * SEC, 1_000 * SEC, 600 * SEC);
        let seen = one(&log).expect("recorded");
        assert!(!log.retire_if_settled(seen, false));
        assert_eq!(one(&log), Some(seen));
        // A step merged into the window after the read keeps it.
        log.note(1_650 * SEC, 1_650 * SEC, 10 * SEC);
        assert!(!log.retire_if_settled(seen, true));
        let merged = one(&log).expect("still there");
        assert_ne!(merged, seen);
        // A far step is its own window; the settled one retires alone.
        log.note(9_000 * SEC, 9_000 * SEC, 10 * SEC);
        assert!(log.retire_if_settled(merged, true));
        assert_eq!(one(&log), Some((8_960 * SEC, 9_040 * SEC)));
        assert!(log.active.load(Ordering::Acquire), "one window left");
        assert!(log.retire_if_settled((8_960 * SEC, 9_040 * SEC), true));
        assert_eq!(one(&log), None);
        assert!(!log.active.load(Ordering::Acquire));
    }

    /// The process wrappers drive the one process log. The steps are in the
    /// far future (year 2508), so no other test's acknowledgement can settle
    /// and retire them and no test hour overlaps them; the log only grows
    /// otherwise, so the assertions hold whatever else runs.
    #[test]
    fn test_process_log_note_clock_step_note_segment_rotation_bind_clock_step_log_persist_clock_step_log_if_dirty_retire_clock_step_window_if_settled()
     {
        let covers = |at: u64| {
            clock_step_windows()
                .iter()
                .any(|(lo, hi)| lo <= at - 630 * SEC && hi >= at + 630 * SEC)
        };
        let at = 17_000_000_000 * SEC;
        note_clock_step(at, at, 600 * SEC);
        assert!(covers(at), "{:?}", clock_step_windows());
        let rotated = at + 10_000 * SEC;
        note_segment_rotation(u128::from(rotated), u128::from(rotated + 600 * SEC));
        assert!(covers(rotated), "{:?}", clock_step_windows());
        let window = clock_step_windows()
            .overlapping(at, at)
            .next()
            .expect("recorded");
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
        let mut many = StepWindows::EMPTY;
        for i in 0..MAX_STEP_WINDOWS as u64 {
            many.insert((i * 1_000 * SEC, i * 1_000 * SEC + 7));
        }
        for windows in [
            StepWindows::EMPTY,
            Some((7, 9)).into(),
            Some((0, u64::MAX)).into(),
            many,
        ] {
            assert_eq!(decode(&encode(&windows)), Some(windows));
        }
        let mut bytes = encode(&Some((7, 9)).into());
        bytes[14] ^= 1;
        assert_eq!(decode(&bytes), None, "a flipped bit fails the CRC");
        assert_eq!(decode(&bytes[..FILE_LEN - 1]), None, "a short file");
        let mut wrong = encode(&Some((7, 9)).into());
        wrong[0] = b'X';
        assert_eq!(decode(&wrong), None, "wrong magic");
        // A count past the cap, or a reversed window, is refused.
        let mut over = encode(&many);
        over[8..12].copy_from_slice(&9_u32.to_le_bytes());
        let crc = crate::wal_applied_watermark::crc32_ieee(&over[0..FILE_CRC_AT]);
        over[FILE_CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode(&over), None, "count past the cap");
        let mut reversed = encode(&Some((7, 9)).into());
        reversed[FILE_WINDOWS_AT..FILE_WINDOWS_AT + 8].copy_from_slice(&10_u64.to_le_bytes());
        let crc = crate::wal_applied_watermark::crc32_ieee(&reversed[0..FILE_CRC_AT]);
        reversed[FILE_CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode(&reversed), None, "lo above hi");
    }

    /// The one-window version 1 file (written by every binary up to
    /// 2026-11-15) still loads, as one window: where its steps were is not
    /// known, so it is never split. An empty version 1 file loads empty.
    /// Bites: with the version 1 arm removed, both decode to `None` and a
    /// boot reads the file as unreadable.
    #[test]
    fn test_old_one_window_file_format_still_loads() {
        let v1 = |window: Option<(u64, u64)>| {
            let (lo, hi) = window.unwrap_or((u64::MAX, 0));
            let mut out = [0_u8; FILE_LEN_V1];
            out[0..4].copy_from_slice(&FILE_MAGIC);
            out[4..8].copy_from_slice(&FILE_VERSION_V1.to_le_bytes());
            out[8..16].copy_from_slice(&lo.to_le_bytes());
            out[16..24].copy_from_slice(&hi.to_le_bytes());
            let crc = crate::wal_applied_watermark::crc32_ieee(&out[0..24]);
            out[24..28].copy_from_slice(&crc.to_le_bytes());
            out
        };
        // The live 16 Nov union: 12 Nov 16:10 IST to 16 Nov 16:11 IST.
        let union = (1_794_480_050_881_500_204, 1_794_825_707_464_928_256);
        assert_eq!(decode(&v1(Some(union))), Some(Some(union).into()));
        assert_eq!(decode(&v1(None)), Some(StepWindows::EMPTY));
        let mut torn = v1(Some(union));
        torn[9] ^= 1;
        assert_eq!(decode(&torn), None);
        let dir = temp_dir("v1");
        std::fs::write(dir.join(CLOCK_STEP_FILE), v1(Some(union))).expect("write");
        let log = ClockStepLog::new();
        log.bind(&dir, union.1 + 3_600 * SEC);
        assert_eq!(one(&log), Some(union), "kept whole, never split");
        // Rewritten in the new format at the next persist only when changed.
        log.note(union.1 + 86_400 * SEC, union.1 + 86_400 * SEC, 5 * SEC);
        log.persist_if_dirty();
        let again = ClockStepLog::new();
        again.bind(&dir, union.1 + 2 * 86_400 * SEC);
        assert_eq!(again.windows().len(), 2);
        assert_eq!(again.windows().iter().next(), Some(union));
        drop(std::fs::remove_dir_all(&dir));
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
        let recorded = one(&first);
        first.persist_if_dirty();
        assert!(dir.join(CLOCK_STEP_FILE).exists(), "the step is written");
        assert!(
            !dir.join(CLOCK_STEP_TMP).exists(),
            "the temp file is renamed"
        );

        let restarted = ClockStepLog::new();
        restarted.bind(&dir, 2_000 * SEC);
        assert_eq!(one(&restarted), recorded, "the restart keeps the step");

        // A retired window is written too, so the next restart has none.
        let seen = one(&restarted).expect("carried over");
        assert!(restarted.retire_if_settled(seen, true));
        restarted.persist_if_dirty();
        let third = ClockStepLog::new();
        third.bind(&dir, 3_000 * SEC);
        assert_eq!(one(&third), None);
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
        assert_eq!(one(&log), Some(expected));
        assert!(
            window_overlaps(one(&log), session_start, session_start + hour),
            "the morning of the last session is judged as stepped"
        );
        assert!(
            !window_overlaps(one(&log), 0, expected.0 - 1),
            "an hour before the last session is not"
        );
        log.persist_if_dirty();
        let again = ClockStepLog::new();
        again.bind(&dir, boot + 30 * 24 * hour);
        assert_eq!(one(&again), Some(expected), "rewritten whole, kept");
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
        let recorded = one(&first).expect("recorded");
        first.persist_if_dirty();

        // A boot a month later (or a clock read far ahead) keeps it.
        let restarted = ClockStepLog::new();
        restarted.bind(&dir, recorded.1 + 30 * day);
        assert_eq!(one(&restarted), Some(recorded));
        restarted.persist_if_dirty();
        let third = ClockStepLog::new();
        third.bind(&dir, recorded.1 + 60 * day);
        assert_eq!(one(&third), Some(recorded), "the file still holds it");

        // ⚠ CHANGED 2026-11-16: a second step days later is its own window
        // and never replaces the first; each retires only on a settled read.
        let later = recorded.1 + 2 * day;
        restarted.note(later, later, 10 * SEC);
        assert_eq!(
            list(&restarted.windows()),
            vec![recorded, (later - 40 * SEC, later + 40 * SEC)],
            "the earlier step is kept"
        );
        assert!(!restarted.retire_if_settled(recorded, false));
        assert!(restarted.retire_if_settled(recorded, true));
        assert!(restarted.retire_if_settled((later - 40 * SEC, later + 40 * SEC), true));
        assert_eq!(one(&restarted), None);
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
