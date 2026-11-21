//! Dhan trade-time correction (scope lock 2026-11-20, "DHAN TRADE TIME SENT AS
//! UTC: CORRECTED BY EXACTLY +19,800 s FROM A PERSISTED, ARMED PER-DAY
//! RECORD"; owner card "Auto-detect daily (Recommended)").
//!
//! On 2026-11-20 Dhan sent every main-feed Last Trade Time as plain UTC epoch
//! seconds, 19,800 s (5 h 30 min) early under the documented IST-epoch rule.
//! This module holds the four parts of the fix:
//!
//! 1. **The table** ([`LttTable`]): per IST day, at most
//!    [`MAX_ENTRIES_PER_DAY`] armed entries. An entry says "from receipt
//!    `from` until receipt `to`, Dhan trade times are UTC". It confirms ONE
//!    segment (the one the detector voted on); every other segment is
//!    corrected only for fresh prints. The live process reads it through
//!    [`corrected_ltt`], which the core dispatcher calls on the Ticker, Quote
//!    and Full arms only. On a day with nothing armed anywhere that is one
//!    atomic load.
//! 2. **The record** (one append-only file per IST day under
//!    [`LTT_RECORD_DIR`], every line CRC-32 framed and fsynced). A change is a
//!    two-step commit: the INTENT line is fsynced first, then the live drain
//!    applies the change and only then writes the COMMIT line carrying the
//!    receipt instant it took effect from. Replay and every rebuild apply
//!    only committed changes, keyed on the frame's WAL receipt stamp, so live
//!    and replay produce identical rows. An intent with neither commit nor
//!    void HOLDS that day from replay ([`any_receipt_day_held`]): the live
//!    process may or may not have applied it, so no replay can know.
//! 3. **The detector** ([`LttDetector`]): live frames only (never replay),
//!    index ticks and NSE stock spot trades. Each fresh print votes "normal"
//!    (raw time near the receipt in IST) or "UTC" (raw time + 19,800 s near
//!    it). It needs [`MIN_VOTES`] votes, [`MIN_DISTINCT`] distinct
//!    instruments and at least 99% agreement, both to arm and to close.
//! 4. **The controller** ([`LttController`]): owned by the live drain. It
//!    turns detector verdicts into record intents, waits for the record
//!    thread's fsync acknowledgement, applies the change with an effective
//!    receipt one nanosecond past every receipt the drain has already
//!    dispatched, and writes the commit. Nothing on the drain blocks: the
//!    record file is written by its own thread ([`spawn_record_writer`]).
//!
//! Guards on every correction ([`correction_verdict`]): exactly +19,800 s;
//! never a result later than receipt + 5 s; never on another IST day than the
//! receipt's, except inside the previous IST day's session; never for `0` or
//! an out-of-band value; never for a frame with no WAL receipt stamp (the
//! caller does not ask); and on a segment the entry did not confirm only for
//! fresh prints (within [`UNCONFIRMED_FRESH_SECS`] of receipt).
//!
//! Complexity: [`corrected_ltt`] is O(1) — one atomic load when nothing is
//! armed, otherwise one `ArcSwap` load, one slot index and at most
//! [`MAX_ENTRIES_PER_DAY`] compares. No allocation. The detector and the
//! controller are O(1) per vote and per frame. Record reads and writes are
//! cold (boot, and a few lines on a day the correction arms).

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use arc_swap::ArcSwap;
use chrono::NaiveDate;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use tickvault_common::config::LttUtcAutocorrect;
use tickvault_common::constants::{
    MUHURAT_PERSIST_END_SECS_OF_DAY_IST, MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

use crate::wal_applied_watermark::crc32_ieee;
use crate::ws_frame_spill::{WAL_RECEIPT_UNKNOWN_NANOS, plausible_receipt_nanos};

/// The one offset this module ever applies (scope lock 2026-11-20 REJECT:
/// "any offset other than +19,800 s").
pub const LTT_UTC_OFFSET_SECS: u32 = 19_800;
/// Where the per-day records live.
pub const LTT_RECORD_DIR: &str = "data/state/ltt-correction";
/// At most this many entries a day; past it the detector freezes with a
/// coded error.
pub const MAX_ENTRIES_PER_DAY: usize = 8;
/// IST days the table holds at once (a ring indexed by day number).
pub const DAY_SLOTS: usize = 32;
/// Days of records loaded at boot (one less than [`DAY_SLOTS`], so no two
/// loaded days share a slot).
pub const BOOT_LOAD_DAYS: i64 = 31;
/// A corrected time may be at most this far after the receipt.
pub const RECEIPT_FUTURE_CAP_SECS: i64 = 5;
/// On a segment the entry did not confirm, only prints this fresh are
/// corrected.
pub const UNCONFIRMED_FRESH_SECS: i64 = 120;
/// A vote counts a print this far ahead of the receipt.
pub const VOTE_EARLY_SECS: i64 = 5;
/// A vote counts a print at most this far behind the receipt.
pub const VOTE_LATE_SECS: i64 = 120;
/// Votes a segment collects before the detector decides.
pub const MIN_VOTES: u32 = 100;
/// Distinct instruments the winning side needs.
pub const MIN_DISTINCT: usize = 3;
/// Agreement the winning side needs, in percent.
pub const AGREE_PERCENT: u32 = 99;
/// The plausible band for a raw trade time (2020-09-13 .. 2050-01-01, the
/// exchange band).
const MIN_PLAUSIBLE_LTT: u32 = 1_600_000_000;
const MAX_PLAUSIBLE_LTT: u32 = 2_524_608_000;
const SECS_PER_DAY: i64 = 86_400;
const NANOS_PER_SEC: i64 = 1_000_000_000;
const IST_OFFSET_SECS: i64 = LTT_UTC_OFFSET_SECS as i64;
/// 1970-01-01 as a day count from the common era (chrono's numbering).
const UNIX_EPOCH_DAYS_FROM_CE: i64 = 719_163;
/// Segment codes the detector votes on.
pub const SEGMENT_IDX_I: u8 = 0;
pub const SEGMENT_NSE_EQ: u8 = 1;
/// The record line prefix (format version 1).
const RECORD_PREFIX: &str = "TVLTT1";
/// The record file name suffix.
const RECORD_SUFFIX: &str = ".rec";
/// A record file larger than this is refused (it holds at most a few dozen
/// lines).
const MAX_RECORD_BYTES: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------

/// The IST wall-clock second of a UTC receipt in nanoseconds.
#[inline]
#[must_use]
pub const fn receipt_ist_secs(receipt_nanos: i64) -> i64 {
    receipt_nanos.div_euclid(NANOS_PER_SEC) + IST_OFFSET_SECS
}

/// The IST day number (days since 1970-01-01 IST) of a UTC receipt.
#[inline]
#[must_use]
pub const fn receipt_ist_day(receipt_nanos: i64) -> i64 {
    receipt_ist_secs(receipt_nanos).div_euclid(SECS_PER_DAY)
}

/// The calendar date of an IST day number.
#[must_use]
pub fn ist_day_date(day: i64) -> Option<NaiveDate> {
    let ce = day.checked_add(UNIX_EPOCH_DAYS_FROM_CE)?;
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(ce).ok()?)
}

/// The IST day number of a calendar date.
#[must_use]
pub fn date_ist_day(date: NaiveDate) -> i64 {
    i64::from(chrono::Datelike::num_days_from_ce(&date)) - UNIX_EPOCH_DAYS_FROM_CE
}

#[inline]
const fn slot_index(day: i64) -> usize {
    day.rem_euclid(DAY_SLOTS as i64) as usize
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// The corrected trade time, or `None` when the correction must not apply.
///
/// `confirmed` is true when an entry that covers this receipt confirmed the
/// packet's own segment. O(1), no allocation; the previous-day Muhurat probe
/// (one installed-calendar lookup) runs only for a corrected time on the
/// receipt's previous IST day.
#[must_use]
pub fn correction_verdict(raw: u32, receipt_nanos: i64, confirmed: bool) -> Option<u32> {
    if plausible_receipt_nanos(receipt_nanos) == WAL_RECEIPT_UNKNOWN_NANOS {
        return None;
    }
    if !(MIN_PLAUSIBLE_LTT..=MAX_PLAUSIBLE_LTT).contains(&raw) {
        return None;
    }
    let corrected = raw.checked_add(LTT_UTC_OFFSET_SECS)?;
    let corrected_secs = i64::from(corrected);
    let receipt_secs = receipt_ist_secs(receipt_nanos);
    if corrected_secs > receipt_secs + RECEIPT_FUTURE_CAP_SECS {
        return None;
    }
    let corrected_day = corrected_secs.div_euclid(SECS_PER_DAY);
    let receipt_day = receipt_secs.div_euclid(SECS_PER_DAY);
    if corrected_day != receipt_day {
        if corrected_day != receipt_day - 1 {
            return None;
        }
        // Inside the previous IST day's session only.
        let sod = corrected_secs.rem_euclid(SECS_PER_DAY) as u32;
        let regular =
            (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sod);
        let muhurat = (MUHURAT_PERSIST_START_SECS_OF_DAY_IST..MUHURAT_PERSIST_END_SECS_OF_DAY_IST)
            .contains(&sod)
            && ist_day_date(corrected_day)
                .and_then(tickvault_common::market_hours::installed_calendar_says_muhurat)
                == Some(true);
        if !(regular || muhurat) {
            return None;
        }
    }
    if !confirmed && receipt_secs - corrected_secs > UNCONFIRMED_FRESH_SECS {
        return None;
    }
    Some(corrected)
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// One armed entry: Dhan trade times are UTC for receipts in `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LttEntry {
    /// The entry's id within its day (0..MAX_ENTRIES_PER_DAY).
    pub id: u8,
    /// The segment code the detector confirmed.
    pub segment: u8,
    /// First receipt (UTC nanos) the entry covers.
    pub from_nanos: i64,
    /// First receipt it no longer covers (`i64::MAX` while open).
    pub to_nanos: i64,
}

const EMPTY_ENTRY: LttEntry = LttEntry {
    id: 0,
    segment: 0,
    from_nanos: i64::MAX,
    to_nanos: i64::MAX,
};

/// One IST day of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaySlot {
    /// The IST day number, or `i64::MIN` when unused.
    pub day: i64,
    entries: [LttEntry; MAX_ENTRIES_PER_DAY],
    len: u8,
    /// Entry ids the day's record has used (intents included).
    pub ids_used: u8,
    /// An intent on this day was neither committed nor voided.
    pub held: bool,
}

const EMPTY_SLOT: DaySlot = DaySlot {
    day: i64::MIN,
    entries: [EMPTY_ENTRY; MAX_ENTRIES_PER_DAY],
    len: 0,
    ids_used: 0,
    held: false,
};

impl DaySlot {
    /// The armed entries.
    #[must_use]
    pub fn entries(&self) -> &[LttEntry] {
        &self.entries[..usize::from(self.len)]
    }
}

/// The whole correction table: a ring of [`DAY_SLOTS`] IST days.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LttTable {
    slots: [DaySlot; DAY_SLOTS],
    /// IST days older than the loaded window that HAVE a record file: their
    /// correction cannot be applied from the ring, so a replay of them is held
    /// (fail toward keeping the frames). Sorted, cold, usually empty.
    old_record_days: Vec<i64>,
}

impl Default for LttTable {
    fn default() -> Self {
        Self::empty()
    }
}

impl LttTable {
    /// A table with nothing armed and nothing held.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            slots: [EMPTY_SLOT; DAY_SLOTS],
            old_record_days: Vec::new(),
        }
    }

    /// The slot of `day`, if the table holds that day.
    #[must_use]
    pub fn day(&self, day: i64) -> Option<&DaySlot> {
        let slot = &self.slots[slot_index(day)];
        (slot.day == day).then_some(slot)
    }

    fn day_mut(&mut self, day: i64) -> &mut DaySlot {
        let slot = &mut self.slots[slot_index(day)];
        if slot.day != day {
            *slot = EMPTY_SLOT;
            slot.day = day;
        }
        slot
    }

    /// True when any day has an armed entry.
    #[must_use]
    pub fn any_entry(&self) -> bool {
        self.slots.iter().any(|s| s.day != i64::MIN && s.len > 0)
    }

    /// True when any day is held.
    #[must_use]
    pub fn any_held(&self) -> bool {
        !self.old_record_days.is_empty() || self.slots.iter().any(|s| s.day != i64::MIN && s.held)
    }

    /// Adds an armed entry to `day`. `false` when the day is full.
    pub fn arm(&mut self, day: i64, entry: LttEntry) -> bool {
        let slot = self.day_mut(day);
        let len = usize::from(slot.len);
        if len >= MAX_ENTRIES_PER_DAY {
            return false;
        }
        slot.entries[len] = entry;
        slot.len = slot.len.saturating_add(1);
        slot.ids_used = slot.ids_used.max(entry.id.saturating_add(1));
        true
    }

    /// Ends entry `id` of `day` at receipt `to_nanos`. `false` when absent.
    pub fn close(&mut self, day: i64, id: u8, to_nanos: i64) -> bool {
        let Some(slot) = self.slots.get_mut(slot_index(day)) else {
            return false;
        };
        if slot.day != day {
            return false;
        }
        let len = usize::from(slot.len);
        for entry in &mut slot.entries[..len] {
            if entry.id == id {
                entry.to_nanos = to_nanos;
                return true;
            }
        }
        false
    }

    /// Replaces `day` with a record read from disk.
    pub fn install_day(&mut self, day: i64, record: &DayRecord) {
        let slot = self.day_mut(day);
        *slot = EMPTY_SLOT;
        slot.day = day;
        slot.ids_used = record.ids_used;
        slot.held = record.held();
        for entry in record.armed_entries() {
            let len = usize::from(slot.len);
            if len >= MAX_ENTRIES_PER_DAY {
                break;
            }
            slot.entries[len] = entry;
            slot.len = slot.len.saturating_add(1);
        }
    }

    /// The corrected trade time for a packet, or `None` to keep the raw one.
    /// O(1): one slot index and at most [`MAX_ENTRIES_PER_DAY`] compares.
    #[must_use]
    pub fn correct(&self, raw: u32, segment: u8, receipt_nanos: i64) -> Option<u32> {
        let day = receipt_ist_day(receipt_nanos);
        let slot = &self.slots[slot_index(day)];
        if slot.day != day || slot.len == 0 {
            return None;
        }
        let mut covered = false;
        let mut confirmed = false;
        for entry in slot.entries() {
            if receipt_nanos >= entry.from_nanos && receipt_nanos < entry.to_nanos {
                covered = true;
                if entry.segment == segment {
                    confirmed = true;
                    break;
                }
            }
        }
        if !covered {
            return None;
        }
        correction_verdict(raw, receipt_nanos, confirmed)
    }

    /// True when the IST day of `receipt_nanos` is held from replay.
    #[must_use]
    pub fn receipt_day_held(&self, receipt_nanos: i64) -> bool {
        let day = receipt_ist_day(receipt_nanos);
        let slot = &self.slots[slot_index(day)];
        (slot.day == day && slot.held) || self.old_record_days.binary_search(&day).is_ok()
    }
}

// ---------------------------------------------------------------------------
// The process-wide table
// ---------------------------------------------------------------------------

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ANY_HELD: AtomicBool = AtomicBool::new(false);
static TABLE: LazyLock<ArcSwap<LttTable>> =
    LazyLock::new(|| ArcSwap::from_pointee(LttTable::empty()));

/// Publishes `table` to every reader. Cold (boot, and a change on the drain).
pub fn install_table(table: LttTable) {
    let active = table.any_entry();
    let held = table.any_held();
    TABLE.store(Arc::new(table));
    ANY_HELD.store(held, Ordering::Release);
    ACTIVE.store(active, Ordering::Release);
}

/// A copy of the published table (cold).
#[must_use]
pub fn current_table() -> LttTable {
    LttTable::clone(&TABLE.load())
}

/// The trade time every consumer should read for a Ticker, Quote or Full
/// packet. Called by the core dispatcher only for a frame whose receipt is a
/// WAL stamp. O(1): one atomic load on a day with nothing armed anywhere.
#[inline]
#[must_use]
pub fn corrected_ltt(raw: u32, segment: u8, receipt_nanos: i64) -> u32 {
    if !ACTIVE.load(Ordering::Acquire) {
        return raw;
    }
    TABLE
        .load()
        .correct(raw, segment, receipt_nanos)
        .unwrap_or(raw)
}

/// True when any receipt's IST day is held from replay. O(1) when no day is
/// held; otherwise O(receipts) with one slot read each (cold, before a replay
/// chunk).
pub fn any_receipt_day_held(mut receipts: impl Iterator<Item = i64>) -> bool {
    if !ANY_HELD.load(Ordering::Acquire) {
        return false;
    }
    let table = TABLE.load();
    receipts.any(|r| table.receipt_day_held(r))
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// The kind of one record line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// Intent to arm entry `id` confirming `segment`.
    Entry,
    /// Entry `id` armed from receipt `nanos`.
    Armed,
    /// Entry `id` was never armed.
    Void,
    /// Intent to close entry `id`.
    Close,
    /// Entry `id` closed at receipt `nanos`.
    Closed,
    /// The close intent of entry `id` was never applied.
    CloseVoid,
    /// The once-a-day notice was sent.
    Notice,
}

impl RecordKind {
    const fn letter(self) -> char {
        match self {
            Self::Entry => 'E',
            Self::Armed => 'A',
            Self::Void => 'V',
            Self::Close => 'C',
            Self::Closed => 'K',
            Self::CloseVoid => 'X',
            Self::Notice => 'N',
        }
    }

    const fn from_letter(letter: char) -> Option<Self> {
        Some(match letter {
            'E' => Self::Entry,
            'A' => Self::Armed,
            'V' => Self::Void,
            'C' => Self::Close,
            'K' => Self::Closed,
            'X' => Self::CloseVoid,
            'N' => Self::Notice,
            _ => return None,
        })
    }

    /// An intent: the drain waits for its fsync acknowledgement.
    #[must_use]
    pub const fn is_intent(self) -> bool {
        matches!(self, Self::Entry | Self::Close)
    }
}

/// One record line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLine {
    pub kind: RecordKind,
    pub id: u8,
    pub segment: u8,
    pub nanos: i64,
}

/// The line's text, newline included, with its CRC-32.
#[must_use]
pub fn encode_line(line: &RecordLine) -> String {
    let body = format!(
        "{RECORD_PREFIX} {} {} {} {}",
        line.kind.letter(),
        line.id,
        line.segment,
        line.nanos
    );
    let crc = crc32_ieee(body.as_bytes());
    format!("{body} {crc:08x}\n")
}

/// One line parsed back; `None` for a torn, damaged or foreign line.
#[must_use]
pub fn decode_line(text: &str) -> Option<RecordLine> {
    let text = text.trim_end_matches(['\n', '\r']);
    let (body, crc) = text.rsplit_once(' ')?;
    if crc.len() != 8 || u32::from_str_radix(crc, 16).ok()? != crc32_ieee(body.as_bytes()) {
        return None;
    }
    let mut parts = body.split(' ');
    if parts.next()? != RECORD_PREFIX {
        return None;
    }
    let mut letters = parts.next()?.chars();
    let kind = RecordKind::from_letter(letters.next()?)?;
    if letters.next().is_some() {
        return None;
    }
    let id: u8 = parts.next()?.parse().ok()?;
    let segment: u8 = parts.next()?.parse().ok()?;
    let nanos: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || usize::from(id) >= MAX_ENTRIES_PER_DAY {
        return None;
    }
    Some(RecordLine {
        kind,
        id,
        segment,
        nanos,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct IdState {
    intent: bool,
    segment: u8,
    armed_from: Option<i64>,
    voided: bool,
    close_intent: bool,
    closed_to: Option<i64>,
    close_voided: bool,
}

/// One IST day's record, parsed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DayRecord {
    ids: [IdState; MAX_ENTRIES_PER_DAY],
    /// Entry ids used (one past the highest intent id seen).
    pub ids_used: u8,
    /// The once-a-day notice was recorded as sent.
    pub notice_sent: bool,
    /// Lines refused as torn or damaged.
    pub damaged_lines: u32,
}

impl DayRecord {
    /// Parses a whole record file's text.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut record = Self::default();
        for raw in text.lines() {
            if raw.is_empty() {
                continue;
            }
            let Some(line) = decode_line(raw) else {
                record.damaged_lines = record.damaged_lines.saturating_add(1);
                continue;
            };
            record.apply(&line);
        }
        record
    }

    /// Applies one line (the same rules as a parse).
    pub fn apply(&mut self, line: &RecordLine) {
        let Some(state) = self.ids.get_mut(usize::from(line.id)) else {
            return;
        };
        match line.kind {
            RecordKind::Entry => {
                if !state.intent {
                    state.intent = true;
                    state.segment = line.segment;
                }
                self.ids_used = self.ids_used.max(line.id.saturating_add(1));
            }
            RecordKind::Armed => {
                if state.armed_from.is_none() {
                    state.armed_from = Some(line.nanos);
                }
            }
            RecordKind::Void => state.voided = true,
            RecordKind::Close => {
                // A new close attempt after a voided one: the void cancelled
                // only the attempt before it (review fix 2026-11-21).
                state.close_intent = true;
                state.close_voided = false;
            }
            RecordKind::Closed => {
                if state.closed_to.is_none() {
                    state.closed_to = Some(line.nanos);
                }
            }
            RecordKind::CloseVoid => state.close_voided = true,
            RecordKind::Notice => self.notice_sent = true,
        }
    }

    /// The committed, not voided entries.
    #[must_use]
    pub fn armed_entries(&self) -> Vec<LttEntry> {
        let mut out = Vec::with_capacity(MAX_ENTRIES_PER_DAY);
        for (id, state) in self.ids.iter().enumerate() {
            let (true, false, Some(from)) = (state.intent, state.voided, state.armed_from) else {
                continue;
            };
            let to = match (state.close_voided, state.closed_to) {
                (false, Some(to)) => to,
                _ => i64::MAX,
            };
            out.push(LttEntry {
                id: id as u8,
                segment: state.segment,
                from_nanos: from,
                to_nanos: to,
            });
        }
        out
    }

    /// True when an intent was neither committed nor voided: the live run may
    /// or may not have applied it, so no replay of the day can know.
    #[must_use]
    pub fn held(&self) -> bool {
        self.ids.iter().any(|s| {
            let entry_dangling = s.intent && !s.voided && s.armed_from.is_none();
            let close_dangling = s.intent
                && !s.voided
                && s.armed_from.is_some()
                && s.close_intent
                && !s.close_voided
                && s.closed_to.is_none();
            entry_dangling || close_dangling
        })
    }
}

/// The record path of `day` under `dir`.
#[must_use]
pub fn record_path(dir: &Path, day: i64) -> Option<PathBuf> {
    let date = ist_day_date(day)?;
    Some(dir.join(format!("{}{RECORD_SUFFIX}", date.format("%Y-%m-%d"))))
}

/// Reads one day's record. `Ok(None)` when there is no file.
///
/// # Errors
/// An unreadable or oversized file.
pub fn read_day_record(dir: &Path, day: i64) -> std::io::Result<Option<DayRecord>> {
    let Some(path) = record_path(dir, day) else {
        return Ok(None);
    };
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.take(MAX_RECORD_BYTES.saturating_add(1))
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_RECORD_BYTES {
        return Err(std::io::Error::other("ltt correction record is oversized"));
    }
    Ok(Some(DayRecord::parse(&text)))
}

/// What the boot load found, logged once tracing is up.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootReport {
    pub mode: &'static str,
    pub days_with_entries: u32,
    pub entries: u32,
    pub held_days: Vec<NaiveDate>,
    pub unreadable_days: Vec<NaiveDate>,
    pub damaged_lines: u32,
}

/// Loads the records of `today` and the [`BOOT_LOAD_DAYS`] days before it.
/// `Off` loads nothing (the full rollback). O(days) file reads, cold.
#[must_use]
pub fn load_record_dir(dir: &Path, today: i64, mode: LttUtcAutocorrect) -> (LttTable, BootReport) {
    let mut table = LttTable::empty();
    let mut report = BootReport {
        mode: mode.as_str(),
        ..BootReport::default()
    };
    if mode == LttUtcAutocorrect::Off {
        return (table, report);
    }
    table.old_record_days = old_record_days(dir, today - BOOT_LOAD_DAYS);
    for back in 0..=BOOT_LOAD_DAYS {
        let day = today - back;
        match read_day_record(dir, day) {
            Ok(None) => {}
            Ok(Some(record)) => {
                let armed = record.armed_entries().len();
                report.damaged_lines = report.damaged_lines.saturating_add(record.damaged_lines);
                if armed > 0 {
                    report.days_with_entries = report.days_with_entries.saturating_add(1);
                    report.entries = report.entries.saturating_add(armed as u32);
                }
                if record.held()
                    && let Some(date) = ist_day_date(day)
                {
                    report.held_days.push(date);
                }
                if armed > 0 || record.held() || record.ids_used > 0 {
                    table.install_day(day, &record);
                }
            }
            Err(_) => {
                // An unreadable record: the live run may have corrected that
                // day, so its replay is held (fail toward keeping the frames).
                if let Some(date) = ist_day_date(day) {
                    report.unreadable_days.push(date);
                }
                let mut record = DayRecord::default();
                record.ids[0].intent = true;
                record.ids_used = 1;
                table.install_day(day, &record);
            }
        }
    }
    (table, report)
}

/// IST days before `oldest_loaded` that have a record file (cold: one folder
/// listing at boot; record files exist only for days the correction touched).
fn old_record_days(dir: &Path, oldest_loaded: i64) -> Vec<i64> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut days: Vec<i64> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let stem = name.to_str()?.strip_suffix(RECORD_SUFFIX)?.to_string();
            let date = NaiveDate::parse_from_str(&stem, "%Y-%m-%d").ok()?;
            let day = date_ist_day(date);
            (day < oldest_loaded).then_some(day)
        })
        .collect();
    days.sort_unstable();
    days.dedup();
    days
}

static BOOT_REPORT: OnceLock<BootReport> = OnceLock::new();

/// Loads and publishes the records at boot, before any frame is dispatched
/// (main.rs, right after the WAL folder is locked). Once per process.
pub fn install_at_boot(dir: &Path, mode: LttUtcAutocorrect) {
    let today = receipt_ist_day(now_utc_nanos());
    let (table, report) = load_record_dir(dir, today, mode);
    install_table(table);
    if BOOT_REPORT.set(report).is_err() {
        // A second install in one process (tests): the first report stands.
        tracing::debug!("ltt correction boot report already recorded");
    }
}

/// [`install_at_boot`] over [`LTT_RECORD_DIR`] with the configured mode
/// (the one main.rs call).
pub fn install_at_boot_from(config: &tickvault_common::config::ApplicationConfig) {
    install_at_boot(
        Path::new(LTT_RECORD_DIR),
        config.dhan_feed.ltt_utc_autocorrect,
    );
}

/// Logs what [`install_at_boot`] found (it runs before tracing exists).
pub fn log_boot_summary() {
    let Some(report) = BOOT_REPORT.get() else {
        return;
    };
    info!(
        mode = report.mode,
        days_with_entries = report.days_with_entries,
        entries = report.entries,
        damaged_lines = report.damaged_lines,
        "Dhan trade-time correction records loaded"
    );
    if !report.held_days.is_empty() || !report.unreadable_days.is_empty() {
        error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "ltt_record_day_held",
            held_days = ?report.held_days,
            unreadable_days = ?report.unreadable_days,
            "A Dhan trade-time correction record has a change that was started but never \
             confirmed (or cannot be read). The live run may or may not have corrected those \
             days, so their saved frames are held from replay and stay on disk. Nothing is \
             lost; the day needs the manual repair."
        );
    }
}

fn now_utc_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(WAL_RECEIPT_UNKNOWN_NANOS)
}

// ---------------------------------------------------------------------------
// The record writer thread
// ---------------------------------------------------------------------------

/// One write the drain asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordCmd {
    pub day: i64,
    pub line: RecordLine,
}

/// The acknowledgement of an intent line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordAck {
    pub day: i64,
    pub id: u8,
    pub kind: RecordKind,
    /// The line reached the disk (written and `sync_data` returned `Ok`).
    pub durable: bool,
}

/// The notice sender the app hands in (it wraps the Telegram service).
pub type LttNoticeFn = Arc<dyn Fn(String) + Send + Sync>;

static NOTICE: OnceLock<LttNoticeFn> = OnceLock::new();

/// Installs the process-wide notice sender (main.rs, once the Telegram
/// service exists; it is built after the lane starts). A record writer with no
/// sender of its own uses this one. Once per process.
pub fn install_notice(notice: LttNoticeFn) {
    if NOTICE.set(notice).is_err() {
        // A second install in one process: the first sender stands.
        tracing::debug!("ltt correction notice sender already installed");
    }
}

/// The drain's end of the record writer.
#[derive(Debug)]
pub struct RecordWriterHandle {
    tx: Sender<RecordCmd>,
    acks: Receiver<RecordAck>,
}

impl RecordWriterHandle {
    /// A handle over caller-made channels (tests).
    #[must_use]
    pub fn from_channels(tx: Sender<RecordCmd>, acks: Receiver<RecordAck>) -> Self {
        Self { tx, acks }
    }

    /// A handle plus the writer's end of both channels, for a test that plays
    /// the record writer itself.
    #[must_use]
    pub fn stub_pair() -> (Self, Receiver<RecordCmd>, Sender<RecordAck>) {
        let (tx, cmds) = crossbeam_channel::bounded(RECORD_CMD_DEPTH);
        let (acks_tx, acks) = crossbeam_channel::bounded(RECORD_ACK_DEPTH);
        (Self { tx, acks }, cmds, acks_tx)
    }
}

const RECORD_CMD_DEPTH: usize = 64;
const RECORD_ACK_DEPTH: usize = 16;

/// Appends one line and makes it durable. Cold.
///
/// # Errors
/// Any create, open, write or sync failure.
pub fn append_line(dir: &Path, day: i64, line: &RecordLine) -> std::io::Result<()> {
    let path = record_path(dir, day).ok_or_else(|| std::io::Error::other("day out of range"))?;
    std::fs::create_dir_all(dir)?;
    let existed = path.exists();
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(encode_line(line).as_bytes())?;
    file.sync_data()?;
    if !existed {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn segment_label(segment: u8) -> &'static str {
    match segment {
        SEGMENT_IDX_I => "index prices",
        SEGMENT_NSE_EQ => "NSE stock prices",
        _ => "prices",
    }
}

/// The once-a-day Telegram text.
#[must_use]
pub fn notice_text(segment: u8, from_nanos: i64) -> String {
    let when = chrono::DateTime::from_timestamp(from_nanos.div_euclid(NANOS_PER_SEC), 0)
        .map(|t| {
            t.with_timezone(&tickvault_common::trading_calendar::ist_offset())
                .format("%-I:%M:%S %p")
                .to_string()
        })
        .unwrap_or_else(|| "now".to_string());
    format!(
        "Dhan sent trade times 5 h 30 min early (seen on {}). Correcting from {when} IST. \
         Raw data is kept unchanged.",
        segment_label(segment)
    )
}

/// Spawns the record writer thread. Every line is fsynced before an intent
/// is acknowledged. On the day's first armed line it sends the once-a-day
/// notice and records that it did, so a restart never sends it twice.
///
/// # Errors
/// The thread could not be spawned.
pub fn spawn_record_writer(
    dir: PathBuf,
    notice: Option<LttNoticeFn>,
) -> std::io::Result<RecordWriterHandle> {
    let (tx, rx) = crossbeam_channel::bounded::<RecordCmd>(RECORD_CMD_DEPTH);
    let (ack_tx, acks) = crossbeam_channel::bounded::<RecordAck>(RECORD_ACK_DEPTH);
    std::thread::Builder::new()
        .name("tv-ltt-record".to_string())
        .spawn(move || run_record_writer(&dir, &rx, &ack_tx, notice.as_ref()))?;
    Ok(RecordWriterHandle { tx, acks })
}

fn run_record_writer(
    dir: &Path,
    rx: &Receiver<RecordCmd>,
    ack_tx: &Sender<RecordAck>,
    notice: Option<&LttNoticeFn>,
) {
    let mut notice_day: Option<(i64, bool)> = None;
    while let Ok(cmd) = rx.recv() {
        let durable = match append_line(dir, cmd.day, &cmd.line) {
            Ok(()) => true,
            Err(err) => {
                error!(
                    code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                    source = "ltt_record_write_failed",
                    kind = ?cmd.line.kind,
                    id = cmd.line.id,
                    %err,
                    "Could not save a Dhan trade-time correction record line. The change is \
                     not applied (or, for a commit line, the day is held from replay at the \
                     next start). Raw data is unchanged."
                );
                false
            }
        };
        if cmd.line.kind.is_intent()
            && ack_tx
                .try_send(RecordAck {
                    day: cmd.day,
                    id: cmd.line.id,
                    kind: cmd.line.kind,
                    durable,
                })
                .is_err()
        {
            warn!(
                code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                source = "ltt_record_ack_lost",
                "the trade-time correction record could not acknowledge a line"
            );
        }
        if cmd.line.kind == RecordKind::Armed && durable {
            let sent = match notice_day {
                Some((day, sent)) if day == cmd.day => sent,
                _ => read_day_record(dir, cmd.day)
                    .ok()
                    .flatten()
                    .is_some_and(|r| r.notice_sent),
            };
            notice_day = Some((cmd.day, sent));
            // No sender yet (the Telegram service is not built): leave the day
            // unmarked so the next armed line tries again.
            let sender = notice.or_else(|| NOTICE.get());
            if !sent && let Some(send) = sender {
                send(notice_text(cmd.line.segment, cmd.line.nanos));
                let marked = append_line(
                    dir,
                    cmd.day,
                    &RecordLine {
                        kind: RecordKind::Notice,
                        id: 0,
                        segment: cmd.line.segment,
                        nanos: cmd.line.nanos,
                    },
                )
                .is_ok();
                notice_day = Some((cmd.day, marked));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The detector
// ---------------------------------------------------------------------------

/// What one vote window decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// At least 99% of the window read as UTC.
    Utc,
    /// At least 99% of the window read as normal (IST).
    Normal,
}

/// How one print votes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    Utc,
    Normal,
    None,
}

/// How a raw print votes against its receipt. O(1).
#[must_use]
pub fn classify_vote(raw: u32, receipt_nanos: i64) -> Vote {
    if plausible_receipt_nanos(receipt_nanos) == WAL_RECEIPT_UNKNOWN_NANOS
        || !(MIN_PLAUSIBLE_LTT..=MAX_PLAUSIBLE_LTT).contains(&raw)
    {
        return Vote::None;
    }
    let receipt = receipt_ist_secs(receipt_nanos);
    let raw = i64::from(raw);
    let fresh = |t: i64| {
        let behind = receipt - t;
        (-VOTE_EARLY_SECS..=VOTE_LATE_SECS).contains(&behind)
    };
    if fresh(raw) {
        Vote::Normal
    } else if fresh(raw + IST_OFFSET_SECS) {
        Vote::Utc
    } else {
        Vote::None
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SideVotes {
    votes: u32,
    distinct: [u64; MIN_DISTINCT],
    distinct_len: u8,
}

impl SideVotes {
    fn add(&mut self, instrument: u64) {
        self.votes = self.votes.saturating_add(1);
        let len = usize::from(self.distinct_len);
        if len < MIN_DISTINCT && !self.distinct[..len].contains(&instrument) {
            self.distinct[len] = instrument;
            self.distinct_len = self.distinct_len.saturating_add(1);
        }
    }

    fn distinct_enough(&self) -> bool {
        usize::from(self.distinct_len) >= MIN_DISTINCT
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SegmentWindow {
    utc: SideVotes,
    normal: SideVotes,
}

/// The vote detector for the two voting segments. Pure and O(1) per vote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LttDetector {
    windows: [SegmentWindow; 2],
}

impl LttDetector {
    /// A fresh detector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears every window (a new IST day).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Records one print. Returns a verdict when the segment's window filled.
    /// `instrument` is any id unique within the segment (the security id).
    pub fn vote(
        &mut self,
        segment: u8,
        instrument: u64,
        raw: u32,
        receipt_nanos: i64,
    ) -> Option<Verdict> {
        let window = self.windows.get_mut(usize::from(segment))?;
        match classify_vote(raw, receipt_nanos) {
            Vote::Utc => window.utc.add(instrument),
            Vote::Normal => window.normal.add(instrument),
            Vote::None => return None,
        }
        let total = window.utc.votes.saturating_add(window.normal.votes);
        if total < MIN_VOTES {
            return None;
        }
        let agree = |side: &SideVotes| {
            u64::from(side.votes) * 100 >= u64::from(total) * u64::from(AGREE_PERCENT)
                && side.distinct_enough()
        };
        let verdict = if agree(&window.utc) {
            Some(Verdict::Utc)
        } else if agree(&window.normal) {
            Some(Verdict::Normal)
        } else {
            None
        };
        *window = SegmentWindow::default();
        verdict
    }
}

// ---------------------------------------------------------------------------
// The controller (owned by the live drain)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Arm,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    day: i64,
    id: u8,
    segment: u8,
    kind: PendingKind,
}

/// What the controller did, for tests and counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ControllerCounts {
    pub votes: u64,
    pub armed: u32,
    pub closed: u32,
    pub intents_failed: u32,
    pub frozen: bool,
}

/// How the controller publishes a changed table (the process table in
/// production, a capture in tests).
pub type PublishFn = fn(LttTable);

/// The live drain's correction controller. Single owner, O(1) per frame and
/// per vote; never blocks.
#[derive(Debug)]
pub struct LttController {
    mode: LttUtcAutocorrect,
    detector: LttDetector,
    table: LttTable,
    publish: PublishFn,
    writer: Option<RecordWriterHandle>,
    day: i64,
    ids_used: u8,
    open: [Option<u8>; 2],
    shadow_open: [bool; 2],
    pending: Option<Pending>,
    max_receipt: i64,
    counts: ControllerCounts,
}

impl LttController {
    /// A controller over the published table and the record writer.
    #[must_use]
    pub fn new(mode: LttUtcAutocorrect, writer: Option<RecordWriterHandle>) -> Self {
        Self::with_table(mode, writer, current_table(), install_table)
    }

    /// A controller over an explicit table and publisher (tests).
    #[must_use]
    pub fn with_table(
        mode: LttUtcAutocorrect,
        writer: Option<RecordWriterHandle>,
        table: LttTable,
        publish: PublishFn,
    ) -> Self {
        Self {
            mode,
            detector: LttDetector::new(),
            table,
            publish,
            writer,
            day: i64::MIN,
            ids_used: 0,
            open: [None; 2],
            shadow_open: [false; 2],
            pending: None,
            max_receipt: i64::MIN,
            counts: ControllerCounts::default(),
        }
    }

    /// What the controller has done.
    #[must_use]
    pub const fn counts(&self) -> ControllerCounts {
        self.counts
    }

    /// True while an armed entry is open on `segment` today.
    #[must_use]
    pub fn open_on(&self, segment: u8) -> bool {
        self.open
            .get(usize::from(segment))
            .is_some_and(Option::is_some)
    }

    /// The controller's table.
    #[must_use]
    pub const fn table(&self) -> &LttTable {
        &self.table
    }

    /// Call once per live WAL-stamped frame, BEFORE its packets are
    /// dispatched. Rolls the day, applies an acknowledged change, then counts
    /// the frame's receipt. O(1).
    pub fn on_frame(&mut self, receipt_nanos: i64) {
        if self.mode == LttUtcAutocorrect::Off
            || plausible_receipt_nanos(receipt_nanos) == WAL_RECEIPT_UNKNOWN_NANOS
        {
            return;
        }
        let day = receipt_ist_day(receipt_nanos);
        if day != self.day {
            self.roll_day(day);
        }
        if self.pending.is_some() {
            self.poll_ack();
        }
        if receipt_nanos > self.max_receipt {
            self.max_receipt = receipt_nanos;
        }
    }

    /// One fresh print on a voting segment (index tick or NSE stock spot).
    /// `raw` is the trade time as Dhan sent it. O(1).
    pub fn observe(&mut self, segment: u8, instrument: u64, raw: u32, receipt_nanos: i64) {
        if self.mode == LttUtcAutocorrect::Off || segment > SEGMENT_NSE_EQ {
            return;
        }
        if receipt_ist_day(receipt_nanos) != self.day {
            return;
        }
        self.counts.votes = self.counts.votes.saturating_add(1);
        let Some(verdict) = self.detector.vote(segment, instrument, raw, receipt_nanos) else {
            return;
        };
        let seg = usize::from(segment);
        match (verdict, self.mode) {
            (Verdict::Utc, LttUtcAutocorrect::Shadow) => {
                if !self.shadow_open[seg] {
                    self.shadow_open[seg] = true;
                    warn!(
                        code = ErrorCode::DhanLtt01CorrectionChanged.code_str(),
                        source = "ltt_shadow_would_arm",
                        segment,
                        "Dhan trade times read 5 h 30 min early on this segment. Shadow mode: \
                         the correction is NOT applied (switch [dhan_feed] ltt_utc_autocorrect \
                         to act to apply it)."
                    );
                }
            }
            (Verdict::Normal, LttUtcAutocorrect::Shadow) => {
                if self.shadow_open[seg] {
                    self.shadow_open[seg] = false;
                    warn!(
                        code = ErrorCode::DhanLtt01CorrectionChanged.code_str(),
                        source = "ltt_shadow_would_close",
                        segment,
                        "Dhan trade times read normal again on this segment (shadow mode)."
                    );
                }
            }
            (Verdict::Utc, LttUtcAutocorrect::Act) => {
                if self.open[seg].is_none() && self.pending.is_none() {
                    self.propose_arm(segment, receipt_nanos);
                }
            }
            (Verdict::Normal, LttUtcAutocorrect::Act) => {
                if let Some(id) = self.open[seg]
                    && self.pending.is_none()
                {
                    self.propose_close(id, segment, receipt_nanos);
                }
            }
            (_, LttUtcAutocorrect::Off) => {}
        }
    }

    fn roll_day(&mut self, day: i64) {
        if let Some(pending) = self.pending.take() {
            // A change started yesterday and never applied: void it, so the
            // record says it never took effect (an entry never crosses IST
            // midnight).
            self.send_line(
                pending.day,
                RecordLine {
                    kind: match pending.kind {
                        PendingKind::Arm => RecordKind::Void,
                        PendingKind::Close => RecordKind::CloseVoid,
                    },
                    id: pending.id,
                    segment: pending.segment,
                    nanos: 0,
                },
            );
        }
        self.day = day;
        self.detector.reset();
        self.shadow_open = [false; 2];
        self.open = [None; 2];
        self.ids_used = 0;
        self.counts.frozen = false;
        if let Some(slot) = self.table.day(day) {
            self.ids_used = slot.ids_used;
            for entry in slot.entries() {
                if entry.to_nanos == i64::MAX
                    && let Some(open) = self.open.get_mut(usize::from(entry.segment))
                {
                    *open = Some(entry.id);
                }
            }
        }
    }

    fn propose_arm(&mut self, segment: u8, receipt_nanos: i64) {
        if usize::from(self.ids_used) >= MAX_ENTRIES_PER_DAY {
            if !self.counts.frozen {
                self.counts.frozen = true;
                error!(
                    code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                    source = "ltt_detector_frozen",
                    entries = self.ids_used,
                    "The Dhan trade-time correction changed {MAX_ENTRIES_PER_DAY} times today, \
                     so it stops changing for the rest of the day. Check the feed."
                );
            }
            return;
        }
        let id = self.ids_used;
        let line = RecordLine {
            kind: RecordKind::Entry,
            id,
            segment,
            nanos: receipt_nanos,
        };
        if self.send_line(self.day, line) {
            self.ids_used = self.ids_used.saturating_add(1);
            self.pending = Some(Pending {
                day: self.day,
                id,
                segment,
                kind: PendingKind::Arm,
            });
        } else {
            self.counts.intents_failed = self.counts.intents_failed.saturating_add(1);
        }
    }

    fn propose_close(&mut self, id: u8, segment: u8, receipt_nanos: i64) {
        let line = RecordLine {
            kind: RecordKind::Close,
            id,
            segment,
            nanos: receipt_nanos,
        };
        if self.send_line(self.day, line) {
            self.pending = Some(Pending {
                day: self.day,
                id,
                segment,
                kind: PendingKind::Close,
            });
        } else {
            self.counts.intents_failed = self.counts.intents_failed.saturating_add(1);
        }
    }

    fn send_line(&self, day: i64, line: RecordLine) -> bool {
        let Some(writer) = &self.writer else {
            return false;
        };
        match writer.tx.try_send(RecordCmd { day, line }) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                error!(
                    code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                    source = "ltt_record_queue_refused",
                    kind = ?line.kind,
                    id = line.id,
                    "the trade-time correction record writer did not take a line"
                );
                false
            }
        }
    }

    fn poll_ack(&mut self) {
        let Some(pending) = self.pending else {
            return;
        };
        let Some(writer) = &self.writer else {
            self.pending = None;
            return;
        };
        let Ok(ack) = writer.acks.try_recv() else {
            return;
        };
        if ack.day != pending.day || ack.id != pending.id {
            return;
        }
        self.pending = None;
        let void_kind = match pending.kind {
            PendingKind::Arm => RecordKind::Void,
            PendingKind::Close => RecordKind::CloseVoid,
        };
        if !ack.durable {
            self.counts.intents_failed = self.counts.intents_failed.saturating_add(1);
            // The intent may have reached the disk anyway: void it, so a
            // replay is not held for a change that was never applied.
            self.send_line(
                pending.day,
                RecordLine {
                    kind: void_kind,
                    id: pending.id,
                    segment: pending.segment,
                    nanos: 0,
                },
            );
            return;
        }
        // The effective receipt: one past every receipt already dispatched,
        // so live and replay split the frames at the same place.
        let effective = self.max_receipt.saturating_add(1);
        let seg = usize::from(pending.segment);
        match pending.kind {
            PendingKind::Arm => {
                let entry = LttEntry {
                    id: pending.id,
                    segment: pending.segment,
                    from_nanos: effective,
                    to_nanos: i64::MAX,
                };
                if !self.table.arm(pending.day, entry) {
                    self.send_line(
                        pending.day,
                        RecordLine {
                            kind: RecordKind::Void,
                            id: pending.id,
                            segment: pending.segment,
                            nanos: 0,
                        },
                    );
                    return;
                }
                (self.publish)(self.table.clone());
                if let Some(open) = self.open.get_mut(seg) {
                    *open = Some(pending.id);
                }
                self.counts.armed = self.counts.armed.saturating_add(1);
                self.send_line(
                    pending.day,
                    RecordLine {
                        kind: RecordKind::Armed,
                        id: pending.id,
                        segment: pending.segment,
                        nanos: effective,
                    },
                );
                warn!(
                    code = ErrorCode::DhanLtt01CorrectionChanged.code_str(),
                    source = "ltt_correction_armed",
                    segment = pending.segment,
                    id = pending.id,
                    from_nanos = effective,
                    "Dhan trade times read 5 h 30 min early: correcting by +19,800 s from now \
                     (raw data unchanged)."
                );
            }
            PendingKind::Close => {
                if !self.table.close(pending.day, pending.id, effective) {
                    self.send_line(
                        pending.day,
                        RecordLine {
                            kind: RecordKind::CloseVoid,
                            id: pending.id,
                            segment: pending.segment,
                            nanos: 0,
                        },
                    );
                    return;
                }
                (self.publish)(self.table.clone());
                if let Some(open) = self.open.get_mut(seg) {
                    *open = None;
                }
                self.counts.closed = self.counts.closed.saturating_add(1);
                self.send_line(
                    pending.day,
                    RecordLine {
                        kind: RecordKind::Closed,
                        id: pending.id,
                        segment: pending.segment,
                        nanos: effective,
                    },
                );
                warn!(
                    code = ErrorCode::DhanLtt01CorrectionChanged.code_str(),
                    source = "ltt_correction_closed",
                    segment = pending.segment,
                    id = pending.id,
                    to_nanos = effective,
                    "Dhan trade times read normal again on this segment: the correction stops \
                     here."
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "ltt_correction_tests.rs"]
mod tests;
