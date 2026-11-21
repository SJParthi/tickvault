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
//!
//! # ⚠ CHANGED 2026-11-21 (scope lock "DHAN LIVE LTT IS UTC EPOCH FROM RECEIPT 2026-11-20 00:00 IST")
//!
//! Dhan support (ticket #6684672, 21 Nov 2026) confirmed the change was
//! deliberate, made before the open on 20 Nov, part of an API restructure and
//! permanent. Owner cards "Yes, always UTC (Recommended)" and "Before Monday if
//! checks pass (Recommended)". So the correction is no longer armed by a
//! detector or read from a record:
//!
//! - [`corrected_ltt`] is a pure function of `(raw, receipt)`: a receipt
//!   before [`DHAN_LTT_UTC_FROM_RECEIPT_NANOS`] (2026-11-20 00:00 IST) returns
//!   `raw` unchanged (one compare); at or after it [`utc_verdict`] adds exactly
//!   +19,800 s under the plausible-receipt check, the raw band and the
//!   receipt + 5 s cap. A refusal leaves `raw` and is counted by reason
//!   ([`refusal_counts`]). Live and replay decide identically from the WAL
//!   receipt; a frame with no receipt stamp is never corrected (the caller
//!   does not ask) and is counted ([`note_unstamped`]).
//! - The detector only guards against Dhan reverting: [`LttController`] pages
//!   once per segment per IST day (DHAN-FEED-TIME-01,
//!   `source = "dhan_time_format_reverted"`) on a `Normal` verdict, and never
//!   changes the correction. In `act` the page also auto-holds its day
//!   (daily-universe Quote 63): the drain's 30 s feed-time scan reads
//!   [`ControllerCounts::reversion_pages`] and hands the day to the latch
//!   thread, which writes the `.hold` off the drain. `[dhan_feed]
//!   ltt_utc_autocorrect` governs only that guard (`off`, `shadow` = warn,
//!   `act` = page and hold).
//! - Honest limit: [`utc_verdict`] has an upper bound (receipt + 5 s) and no
//!   lower bound or day check, by design. So if Dhan ever reverts, EVERY
//!   print more than about 5.5 h behind its receipt, any day (yesterday's
//!   trade on an illiquid contract's first packet included), is shifted
//!   +5.5 h wrongly until the day is held; only fresh prints stay raw.
//! - The boot load ([`install_at_boot`]) reads nothing, so no day is held from
//!   replay. The table, record, record writer and notice below are DORMANT
//!   (no production path writes or reads them) and go in a follow-up PR.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use arc_swap::ArcSwap;
use chrono::NaiveDate;
use crossbeam_channel::{Receiver, Sender};
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
/// The first WAL receipt (UTC nanoseconds) from which Dhan's Live Market Feed
/// trade time is UTC epoch: 2026-11-20 00:00:00 IST = 2026-11-19T18:30:00Z
/// (scope lock 2026-11-21; Dhan ticket #6684672). Earlier receipts stay IST
/// epoch and are never changed.
pub const DHAN_LTT_UTC_FROM_RECEIPT_NANOS: i64 = 1_795_113_000_000_000_000;
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
// TEST-EXEMPT: covered by test_receipt_ist_day_and_date_round_trip
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
// TEST-EXEMPT: covered by test_receipt_ist_day_and_date_round_trip
pub fn ist_day_date(day: i64) -> Option<NaiveDate> {
    let ce = day.checked_add(UNIX_EPOCH_DAYS_FROM_CE)?;
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(ce).ok()?)
}

/// The IST day number of a calendar date.
#[must_use]
// TEST-EXEMPT: covered by test_receipt_ist_day_and_date_round_trip
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
/// ⚠ DORMANT since 2026-11-21: only the dormant [`LttTable::correct`] calls
/// it; [`corrected_ltt`] uses [`utc_verdict`]. Deleted in the follow-up PR.
///
/// `confirmed` is true when an entry that covers this receipt confirmed the
/// packet's own segment. O(1), no allocation; the previous-day Muhurat probe
/// (one installed-calendar lookup) runs only for a corrected time on the
/// receipt's previous IST day.
#[must_use]
// TEST-EXEMPT: covered by proptest_correction_obeys_every_guard
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
// The permanent UTC rule (scope lock 2026-11-21)
// ---------------------------------------------------------------------------

/// Why a trade time at or after the cutover was left raw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LttRefusal {
    /// The receipt is outside the plausible band (not a real WAL stamp).
    Receipt,
    /// The raw time is `0`, a sentinel, or outside the exchange band.
    Band,
    /// `raw + 19,800` would be more than 5 s after the receipt: the print is
    /// already IST epoch (Dhan reverted) or our clock is behind.
    Cap,
}

impl LttRefusal {
    /// Every refusal reason, in counter order.
    pub const ALL: [Self; 3] = [Self::Receipt, Self::Band, Self::Cap];

    /// The `reason` label of the refusal counter (the one source of the label:
    /// [`publish_ltt_counters`] registers its handles from it).
    #[must_use]
    // TEST-EXEMPT: covered by test_regression_2026_11_21_counter_labels_come_from_as_str
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Receipt => "receipt",
            Self::Band => "band",
            Self::Cap => "cap",
        }
    }
}

impl LttRefusalCounts {
    /// The count for one reason. O(1).
    #[must_use]
    // TEST-EXEMPT: covered by test_regression_2026_11_21_counter_labels_come_from_as_str
    pub const fn get(&self, refusal: LttRefusal) -> u64 {
        match refusal {
            LttRefusal::Receipt => self.receipt,
            LttRefusal::Band => self.band,
            LttRefusal::Cap => self.cap,
        }
    }
}

/// Refusal counts by reason. Relaxed atomics, one add on the refused branch
/// only; never on the corrected path.
#[derive(Debug, Default)]
pub struct LttRefusals {
    receipt: AtomicU64,
    band: AtomicU64,
    cap: AtomicU64,
}

/// A copy of [`LttRefusals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LttRefusalCounts {
    pub receipt: u64,
    pub band: u64,
    pub cap: u64,
}

impl LttRefusals {
    /// All counts at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            receipt: AtomicU64::new(0),
            band: AtomicU64::new(0),
            cap: AtomicU64::new(0),
        }
    }

    /// Counts one refusal. O(1), one relaxed add, no allocation.
    #[inline]
    // TEST-EXEMPT: covered by proptest_post_cutover_exact_or_refused
    pub fn note(&self, refusal: LttRefusal) {
        let cell = match refusal {
            LttRefusal::Receipt => &self.receipt,
            LttRefusal::Band => &self.band,
            LttRefusal::Cap => &self.cap,
        };
        cell.fetch_add(1, Ordering::Relaxed);
    }

    /// The counts now.
    #[must_use]
    // TEST-EXEMPT: covered by proptest_post_cutover_exact_or_refused
    pub fn snapshot(&self) -> LttRefusalCounts {
        LttRefusalCounts {
            receipt: self.receipt.load(Ordering::Relaxed),
            band: self.band.load(Ordering::Relaxed),
            cap: self.cap.load(Ordering::Relaxed),
        }
    }
}

static REFUSALS: LttRefusals = LttRefusals::new();
static UNSTAMPED: AtomicU64 = AtomicU64::new(0);

/// The UTC rule for a receipt at or after the cutover: `raw + 19,800` when it
/// passes every guard, else why it does not. Pure, O(1) (about five compares),
/// no allocation. The guards: a plausible receipt; `raw` inside the exchange
/// band (so `0` and the sentinels stay raw); the add cannot overflow; and the
/// result is at most [`RECEIPT_FUTURE_CAP_SECS`] after the receipt (the
/// reversion firewall: an IST-epoch print fails it). No day or session check:
/// Dhan says every Live LTT is UTC, so a print two days old needs it too.
#[inline]
// TEST-EXEMPT: covered by test_utc_verdict_guards and proptest_post_cutover_exact_or_refused
pub fn utc_verdict(raw: u32, receipt_nanos: i64) -> Result<u32, LttRefusal> {
    if plausible_receipt_nanos(receipt_nanos) == WAL_RECEIPT_UNKNOWN_NANOS {
        return Err(LttRefusal::Receipt);
    }
    if !(MIN_PLAUSIBLE_LTT..=MAX_PLAUSIBLE_LTT).contains(&raw) {
        return Err(LttRefusal::Band);
    }
    let Some(corrected) = raw.checked_add(LTT_UTC_OFFSET_SECS) else {
        return Err(LttRefusal::Band);
    };
    if i64::from(corrected) > receipt_ist_secs(receipt_nanos) + RECEIPT_FUTURE_CAP_SECS {
        return Err(LttRefusal::Cap);
    }
    Ok(corrected)
}

/// [`corrected_ltt`] counting refusals into `refusals` (the process counters
/// in production, a local set in tests). O(1), no allocation.
#[inline]
#[must_use]
// TEST-EXEMPT: covered by proptest_pre_cutover_is_byte_identical and proptest_post_cutover_exact_or_refused
pub fn corrected_ltt_with(raw: u32, receipt_nanos: i64, refusals: &LttRefusals) -> u32 {
    if receipt_nanos < DHAN_LTT_UTC_FROM_RECEIPT_NANOS {
        return raw;
    }
    match utc_verdict(raw, receipt_nanos) {
        Ok(corrected) => corrected,
        Err(refusal) => {
            refusals.note(refusal);
            raw
        }
    }
}

/// The process-wide refusal counts.
#[must_use]
// TEST-EXEMPT: covered by test_regression_ist_epoch_fresh_print_stays_raw_counted_and_pages_once_per_segment_per_day
pub fn refusal_counts() -> LttRefusalCounts {
    REFUSALS.snapshot()
}

/// Counts one trade time the dispatcher left raw because its frame has no WAL
/// receipt stamp. O(1), one relaxed add.
#[inline]
// TEST-EXEMPT: covered by test_dispatcher_corrects_ticker_quote_and_full_from_the_cutover (core/tests)
pub fn note_unstamped() {
    UNSTAMPED.fetch_add(1, Ordering::Relaxed);
}

/// Trade times left raw for want of a WAL receipt stamp, process-wide.
#[must_use]
// TEST-EXEMPT: covered by test_dispatcher_corrects_ticker_quote_and_full_from_the_cutover (core/tests)
pub fn unstamped_count() -> u64 {
    UNSTAMPED.load(Ordering::Relaxed)
}

/// The refusal counter's name.
pub const LTT_REFUSED_COUNTER: &str = "tv_dhan_ltt_correction_refused_total";
/// The unstamped counter's name.
pub const LTT_UNSTAMPED_COUNTER: &str = "tv_dhan_ltt_unstamped_total";

/// The four counter handles, resolved once (the labels come from
/// [`LttRefusal::as_str`], so they cannot drift from the reasons).
struct LttCounterHandles {
    refused: [metrics::Counter; 3],
    unstamped: metrics::Counter,
}

static COUNTER_HANDLES: OnceLock<LttCounterHandles> = OnceLock::new();

fn counter_handles() -> &'static LttCounterHandles {
    COUNTER_HANDLES.get_or_init(|| LttCounterHandles {
        refused: LttRefusal::ALL
            .map(|r| metrics::counter!(LTT_REFUSED_COUNTER, "reason" => r.as_str())),
        unstamped: metrics::counter!(LTT_UNSTAMPED_COUNTER),
    })
}

/// Publishes the refusal and unstamped counts to local `/metrics`
/// (`tv_dhan_ltt_correction_refused_total{reason}`, `tv_dhan_ltt_unstamped_total`;
/// no alarm, filter or EMF name). The first call resolves the four handles
/// (the lane calls it once when it builds the guard, so the series exist at 0
/// from then); later calls set four values and allocate nothing. Cold: the
/// live controller calls it once a minute of receipts, never per packet.
///
/// The counts are DISPATCHES, not distinct packets: a WAL refold or catch-up,
/// the after-close depth pass and the offline replay dispatch through the same
/// path, so a packet dispatched again is counted again.
// TEST-EXEMPT: covered by test_controller_publishes_once_a_minute_of_receipts and test_regression_2026_11_21_counter_labels_come_from_as_str
pub fn publish_ltt_counters() {
    let c = refusal_counts();
    let h = counter_handles();
    for (handle, refusal) in h.refused.iter().zip(LttRefusal::ALL) {
        handle.absolute(c.get(refusal));
    }
    h.unstamped.absolute(unstamped_count());
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
    // TEST-EXEMPT: covered by test_table_corrects_only_inside_an_entry_and_on_its_day
    pub fn any_entry(&self) -> bool {
        self.slots.iter().any(|s| s.day != i64::MIN && s.len > 0)
    }

    /// True when any day is held.
    #[must_use]
    // TEST-EXEMPT: covered by test_held_days_are_read_by_receipt
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
    // TEST-EXEMPT: covered by test_held_days_are_read_by_receipt
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
    // TEST-EXEMPT: covered by test_held_days_are_read_by_receipt
    pub fn receipt_day_held(&self, receipt_nanos: i64) -> bool {
        let day = receipt_ist_day(receipt_nanos);
        let slot = &self.slots[slot_index(day)];
        (slot.day == day && slot.held) || self.old_record_days.binary_search(&day).is_ok()
    }
}

// ---------------------------------------------------------------------------
// The process-wide table
// ---------------------------------------------------------------------------

// ⚠ DORMANT since 2026-11-21: nothing in production publishes a table (the
// boot load reads nothing, the controller never arms, and `install_table` was
// deleted 2026-11-21), so `ANY_HELD` stays false and `any_receipt_day_held`
// returns false. Deleted in the follow-up PR.
static ANY_HELD: AtomicBool = AtomicBool::new(false);
static TABLE: LazyLock<ArcSwap<LttTable>> =
    LazyLock::new(|| ArcSwap::from_pointee(LttTable::empty()));

/// A copy of the published table (cold).
#[must_use]
// TEST-EXEMPT: covered by test_install_at_boot_reads_nothing_and_holds_nothing
pub fn current_table() -> LttTable {
    LttTable::clone(&TABLE.load())
}

/// The trade time every consumer should read for a Ticker, Quote or Full
/// packet. Called by the core dispatcher only for a frame whose receipt is a
/// WAL stamp (scope lock 2026-11-21): a receipt before
/// [`DHAN_LTT_UTC_FROM_RECEIPT_NANOS`] returns `raw` (one compare); at or after
/// it, `raw + 19,800` under [`utc_verdict`], or `raw` with the refusal counted.
/// The segment is not an input (the rule is the same for every segment).
/// O(1), no allocation, no lock, no table read.
#[inline]
#[must_use]
// TEST-EXEMPT: covered by proptest_pre_cutover_is_byte_identical and test_dispatcher_corrects_ticker_quote_and_full_from_the_cutover (core/tests)
pub fn corrected_ltt(raw: u32, _segment: u8, receipt_nanos: i64) -> u32 {
    corrected_ltt_with(raw, receipt_nanos, &REFUSALS)
}

/// True when any receipt's IST day is held from replay. O(1) when no day is
/// held; otherwise O(receipts) with one slot read each (cold, before a replay
/// chunk).
// TEST-EXEMPT: covered by test_replay_held_is_false_with_nothing_held (app ltt_feed)
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
    // TEST-EXEMPT: covered by test_record_writer_thread_acks_intents_and_sends_one_notice_a_day
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
// TEST-EXEMPT: covered by test_record_line_round_trips_and_refuses_damage
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
// TEST-EXEMPT: covered by test_record_line_round_trips_and_refuses_damage
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
    // TEST-EXEMPT: covered by test_day_record_held_semantics
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
// TEST-EXEMPT: covered by test_an_unreadable_record_holds_its_day
pub fn record_path(dir: &Path, day: i64) -> Option<PathBuf> {
    let date = ist_day_date(day)?;
    Some(dir.join(format!("{}{RECORD_SUFFIX}", date.format("%Y-%m-%d"))))
}

/// Reads one day's record. `Ok(None)` when there is no file.
///
/// # Errors
/// An unreadable or oversized file.
// TEST-EXEMPT: covered by test_an_unreadable_record_holds_its_day
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

/// The boot step (main.rs, right after the WAL folder is locked). Once per
/// process.
///
/// ⚠ CHANGED 2026-11-21: reads NOTHING. The correction is a pure function of
/// `(raw, receipt)`, so no per-day record is loaded, nothing is published and
/// no day is held from replay; a leftover `.rec` file is ignored, never
/// deleted. It only records the guard mode for [`log_boot_summary`].
// TEST-EXEMPT: covered by test_install_at_boot_reads_nothing_and_holds_nothing
pub fn install_at_boot(_dir: &Path, mode: LttUtcAutocorrect) {
    let report = BootReport {
        mode: mode.as_str(),
        ..BootReport::default()
    };
    if BOOT_REPORT.set(report).is_err() {
        // A second install in one process (tests): the first report stands.
        tracing::debug!("ltt correction boot report already recorded");
    }
}

/// [`install_at_boot`] over [`LTT_RECORD_DIR`] with the configured mode
/// (the one main.rs call).
// TEST-EXEMPT: one-line main.rs wrapper over install_at_boot, which test_install_at_boot_reads_nothing_and_holds_nothing covers (it loads nothing since 2026-11-21)
pub fn install_at_boot_from(config: &tickvault_common::config::ApplicationConfig) {
    install_at_boot(
        Path::new(LTT_RECORD_DIR),
        config.dhan_feed.ltt_utc_autocorrect,
    );
}

/// Logs the boot step's one line (it runs before tracing exists).
// TEST-EXEMPT: covered by boot log only; report built by test_install_at_boot_reads_nothing_and_holds_nothing
pub fn log_boot_summary() {
    let Some(report) = BOOT_REPORT.get() else {
        return;
    };
    info!(
        guard_mode = report.mode,
        "Dhan trade times are read as UTC epoch from receipt 2026-11-20 00:00 IST: +19,800 s, \
         the same live and in replay (permanent). Per-day correction records are no longer \
         read; the detector only watches for Dhan reverting."
    );
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

// DORMANT: nothing sets it since 2026-11-21 (`install_notice` deleted); the
// dormant record writer reads it. Deleted in the follow-up PR.
static NOTICE: OnceLock<LttNoticeFn> = OnceLock::new();

/// The drain's end of the record writer.
///
/// ⚠ DORMANT since 2026-11-21: no production path spawns the writer (the
/// lane's `production_writer` returns `None`). Deleted in the follow-up PR.
#[derive(Debug)]
// DORMANT: the fields are read only by the record-writer tests.
#[cfg_attr(not(test), allow(dead_code))]
pub struct RecordWriterHandle {
    tx: Sender<RecordCmd>,
    acks: Receiver<RecordAck>,
}

const RECORD_CMD_DEPTH: usize = 64;
const RECORD_ACK_DEPTH: usize = 16;

/// Appends one line and makes it durable. Cold.
///
/// # Errors
/// Any create, open, write or sync failure.
// TEST-EXEMPT: covered by test_append_and_load_record_dir
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
// TEST-EXEMPT: covered by test_record_writer_thread_acks_intents_and_sends_one_notice_a_day
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
// The reversion guard (owned by the live drain)
// ---------------------------------------------------------------------------

/// What the guard did, for tests and counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ControllerCounts {
    /// Fresh prints voted on.
    pub votes: u64,
    /// Windows that read as UTC (the normal state from 2026-11-20).
    pub utc_verdicts: u32,
    /// Windows that read as IST epoch (Dhan reverted, or a pre-cutover day).
    pub normal_verdicts: u32,
    /// Reversion reports logged (at most one per segment per IST day).
    pub reversion_reports: u32,
    /// Reversion PAGES (`act` mode only): each one auto-holds its day
    /// through the drain's feed-time latch thread (daily-universe Quote 63).
    pub reversion_pages: u32,
    /// The IST day number of the latest reversion page (0 before any).
    pub last_reversion_day: i64,
    /// Counter publishes (once a minute of receipts).
    pub publishes: u32,
}

/// How far before a PUBLISHED Muhurat session the reversion guard (and the
/// feed-time page's trade-time window) starts watching: 15 min, as
/// 09:15-15:30 widens to 09:00-15:40.
pub const MUHURAT_GUARD_LEAD_SECS: u32 = 15 * 60;
/// How far after a published Muhurat session they keep watching: 10 min.
pub const MUHURAT_GUARD_TAIL_SECS: u32 = 10 * 60;

/// True when an IST receipt is inside the guard's window: 09:00–15:40 IST,
/// or the PUBLISHED Muhurat session of a calendar Muhurat date, widened by
/// [`MUHURAT_GUARD_LEAD_SECS`] / [`MUHURAT_GUARD_TAIL_SECS`] (review fix
/// 2026-11-21: the fixed 18:00–19:30 window missed an afternoon or late
/// session, so its reversion page and auto-hold never fired). A Muhurat
/// date with no published times falls back to 18:00–19:30. Cold (once per
/// verdict): one calendar probe, two only on a date with no published
/// session.
fn in_guard_window(receipt_nanos: i64) -> bool {
    let secs = receipt_ist_secs(receipt_nanos);
    let sod = secs.rem_euclid(SECS_PER_DAY) as u32;
    if (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sod) {
        return true;
    }
    let Some(date) = ist_day_date(secs.div_euclid(SECS_PER_DAY)) else {
        return false;
    };
    muhurat_guard_contains(
        sod,
        tickvault_common::market_hours::installed_muhurat_session_secs(date),
        || tickvault_common::market_hours::installed_calendar_says_muhurat(date),
    )
}

/// The Muhurat half of [`in_guard_window`], pure: `published` is the date's
/// published session (IST seconds of day, end exclusive), `says_muhurat`
/// asks whether the date is a Muhurat date at all (only asked when no session
/// is published; `None` = no calendar, which watches nothing extra). O(1).
#[must_use]
fn muhurat_guard_contains(
    sod: u32,
    published: Option<(u32, u32)>,
    says_muhurat: impl FnOnce() -> Option<bool>,
) -> bool {
    match published {
        Some((start, end)) => (start.saturating_sub(MUHURAT_GUARD_LEAD_SECS)
            ..end.saturating_add(MUHURAT_GUARD_TAIL_SECS))
            .contains(&sod),
        None => {
            (MUHURAT_PERSIST_START_SECS_OF_DAY_IST..MUHURAT_PERSIST_END_SECS_OF_DAY_IST)
                .contains(&sod)
                && says_muhurat() == Some(true)
        }
    }
}

/// The live drain's reversion guard (scope lock 2026-11-21, noise lock
/// §2.24-a). It votes on Dhan's RAW trade time of live, WAL-stamped index and
/// NSE stock spot prints. A `Utc` verdict is the normal state and does
/// nothing. The first `Normal` verdict per segment per IST day, with a receipt
/// at or after [`DHAN_LTT_UTC_FROM_RECEIPT_NANOS`] and inside the guard window,
/// logs one coded line: an `error!` (`act`, pages DHAN-FEED-TIME-01 and the
/// app then holds the day) or a `warn!` (`shadow`). It never writes a record
/// and never changes the correction. The latch is in RAM, one per process. Single owner, O(1) per frame and per vote; never blocks.
#[derive(Debug)]
pub struct LttController {
    mode: LttUtcAutocorrect,
    /// False for the inert controller (tests): it never publishes counters.
    publishes: bool,
    detector: LttDetector,
    day: i64,
    reported: [bool; 2],
    publish_minute: i64,
    counts: ControllerCounts,
}

impl LttController {
    /// The production guard in `mode`.
    #[must_use]
    pub fn new(mode: LttUtcAutocorrect) -> Self {
        Self {
            mode,
            publishes: true,
            detector: LttDetector::new(),
            day: i64::MIN,
            reported: [false; 2],
            publish_minute: i64::MIN,
            counts: ControllerCounts::default(),
        }
    }

    /// The inert guard every `LiveIngest` starts with: never votes, never
    /// reports, never publishes.
    #[must_use]
    // TEST-EXEMPT: covered by test_guard_off_never_pages
    pub fn inert() -> Self {
        Self {
            publishes: false,
            ..Self::new(LttUtcAutocorrect::Off)
        }
    }

    /// What the guard has done.
    #[must_use]
    pub const fn counts(&self) -> ControllerCounts {
        self.counts
    }

    /// Call once per live WAL-stamped frame, before its packets are
    /// dispatched. Publishes the counters once a minute of receipts and rolls
    /// the day. O(1).
    // TEST-EXEMPT: covered by test_controller_publishes_once_a_minute_of_receipts
    pub fn on_frame(&mut self, receipt_nanos: i64) {
        if !self.publishes || plausible_receipt_nanos(receipt_nanos) == WAL_RECEIPT_UNKNOWN_NANOS {
            return;
        }
        let minute = receipt_nanos.div_euclid(60 * NANOS_PER_SEC);
        if minute != self.publish_minute {
            self.publish_minute = minute;
            self.counts.publishes = self.counts.publishes.saturating_add(1);
            publish_ltt_counters();
        }
        if self.mode == LttUtcAutocorrect::Off {
            return;
        }
        let day = receipt_ist_day(receipt_nanos);
        if day != self.day {
            self.day = day;
            self.detector.reset();
            self.reported = [false; 2];
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
        if verdict == Verdict::Utc {
            self.counts.utc_verdicts = self.counts.utc_verdicts.saturating_add(1);
            return;
        }
        self.counts.normal_verdicts = self.counts.normal_verdicts.saturating_add(1);
        let seg = usize::from(segment);
        if receipt_nanos < DHAN_LTT_UTC_FROM_RECEIPT_NANOS
            || self.reported[seg]
            || !in_guard_window(receipt_nanos)
        {
            return;
        }
        self.reported[seg] = true;
        self.counts.reversion_reports = self.counts.reversion_reports.saturating_add(1);
        if self.mode == LttUtcAutocorrect::Act {
            self.counts.reversion_pages = self.counts.reversion_pages.saturating_add(1);
            self.counts.last_reversion_day = self.day;
        }
        self.report_reversion(segment);
    }

    #[cold]
    fn report_reversion(&self, segment: u8) {
        match self.mode {
            LttUtcAutocorrect::Act => error!(
                code = ErrorCode::DhanFeedTime01TradeTimesWrong.code_str(),
                source = "dhan_time_format_reverted",
                broker = "Dhan",
                segment = segment_label(segment),
                "Dhan trade times look like the old IST format again on {}. Since 20 Nov 2026 \
                 Dhan sends UTC and we add 5 h 30 min; prints that would land in the future \
                 are left unchanged and counted, but older prints are shifted wrongly. Today is \
                 held out of the archive and the export until it is repaired; raw data is \
                 kept. Check the Dhan feed.",
                segment_label(segment)
            ),
            LttUtcAutocorrect::Shadow => warn!(
                code = ErrorCode::DhanFeedTime01TradeTimesWrong.code_str(),
                source = "dhan_time_format_reverted_shadow",
                broker = "Dhan",
                segment = segment_label(segment),
                "Dhan trade times look like the old IST format again (shadow mode: logged only)."
            ),
            LttUtcAutocorrect::Off => {}
        }
    }
}

#[cfg(test)]
#[path = "ltt_correction_tests.rs"]
mod tests;
