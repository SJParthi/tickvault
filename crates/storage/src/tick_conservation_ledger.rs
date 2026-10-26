//! In-process tick conservation ledger (plan item K4, 2026-10-26).
//!
//! The daily tick conservation check compares what this process ACCEPTED for
//! the `ticks` table with what QuestDB holds for the day. The database side is
//! one `count()` query; this module is the process side.
//!
//! ## What it counts
//!
//! - **Accepted rows, per IST day.** One increment per `ticks` row the Dhan
//!   writer appended to its ILP buffer, keyed by the IST day of the row's
//!   designated `ts` (the same day the database count filters on). A row the
//!   session-window gate refused, or one that failed to append, is never
//!   counted, because it was never offered to QuestDB.
//! - **Rows that left the buffer without reaching QuestDB**, over the process
//!   lifetime: rescued to the tick spill file, rescued and failed, or handed to
//!   the WAL replay. These are the rows the database legitimately does not
//!   hold (yet). They are counted at the same sites that increment
//!   `tv_ticks_dropped_total`, so the two cannot disagree in kind.
//!
//! ## Cost
//!
//! The per-tick half ([`TickConservationLedger::note_row_appended`]) is one
//! euclidean division, one relaxed load and one relaxed `fetch_add`: **O(1)**,
//! no allocation, no lock. A slot is reset (two stores) only on the first row
//! of a new day.
//!
//! ## Honest limits
//!
//! - The per-day slots assume ONE Dhan tick writer, which is what production
//!   runs (the frame drain). Two writers racing on a day's first row could
//!   lose a count to the reset; the check would then read low on the process
//!   side, which reads as extra database rows (a mismatch), never as a match.
//! - The not-landed total is per process, not per day, and a rescued batch
//!   can carry `feed_aux_packets` rows beside `ticks` rows. It is therefore an
//!   UPPER bound on the rows the database may lack, used as a tolerance, never
//!   as an exact term.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Nanoseconds in one day.
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// Days the accepted-row ring holds. Eight covers a boot replay of a
/// week-old backlog without one day's slot overwriting another's.
pub const LEDGER_DAY_SLOTS: usize = 8;

/// Sentinel day for an unused slot (no real IST day number is negative here).
const NO_DAY: i64 = i64::MIN;

/// One accepted-row slot: the IST day it holds and the rows counted for it.
#[derive(Debug)]
struct DaySlot {
    day: AtomicI64,
    rows: AtomicU64,
}

impl DaySlot {
    const fn new() -> Self {
        Self {
            day: AtomicI64::new(NO_DAY),
            rows: AtomicU64::new(0),
        }
    }
}

/// The process-side counts the daily conservation check reads. See the
/// module header.
#[derive(Debug)]
pub struct TickConservationLedger {
    accepted: [DaySlot; LEDGER_DAY_SLOTS],
    not_landed: AtomicU64,
}

impl Default for TickConservationLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl TickConservationLedger {
    /// An empty ledger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            accepted: [const { DaySlot::new() }; LEDGER_DAY_SLOTS],
            not_landed: AtomicU64::new(0),
        }
    }

    /// The IST day number (days since the epoch) of an IST-epoch nanosecond
    /// timestamp. O(1).
    #[must_use]
    pub const fn ist_day_of(ts_ist_nanos: i64) -> i64 {
        ts_ist_nanos.div_euclid(NANOS_PER_DAY)
    }

    fn slot_for(&self, day: i64) -> &DaySlot {
        // `rem_euclid` is in 0..LEDGER_DAY_SLOTS, so the index is in bounds.
        let index = usize::try_from(day.rem_euclid(LEDGER_DAY_SLOTS as i64)).unwrap_or(0);
        &self.accepted[index % LEDGER_DAY_SLOTS]
    }

    /// Counts one `ticks` row appended to the ILP buffer, under the IST day
    /// of its designated timestamp.
    ///
    /// # Performance
    /// O(1), no allocation, no lock: one division, one relaxed load and one
    /// relaxed `fetch_add` (plus two stores on a day's first row). Called once
    /// per accepted tick on the frame drain.
    #[inline]
    pub fn note_row_appended(&self, ts_ist_nanos: i64) {
        let day = Self::ist_day_of(ts_ist_nanos);
        let slot = self.slot_for(day);
        if slot.day.load(Ordering::Acquire) != day {
            slot.rows.store(0, Ordering::Relaxed);
            slot.day.store(day, Ordering::Release);
        }
        slot.rows.fetch_add(1, Ordering::Relaxed);
    }

    /// Rows counted for `day` (an [`TickConservationLedger::ist_day_of`]
    /// value), or `0` when the slot holds another day. The day is read before
    /// and after the count, so a slot reset in between reads as `0`. O(1).
    #[must_use]
    pub fn accepted_rows_on(&self, day: i64) -> u64 {
        let slot = self.slot_for(day);
        if slot.day.load(Ordering::Acquire) != day {
            return 0;
        }
        let rows = slot.rows.load(Ordering::Acquire);
        if slot.day.load(Ordering::Acquire) != day {
            return 0;
        }
        rows
    }

    /// Counts `rows` that left the tick buffer without reaching QuestDB
    /// (spilled, failed rescue, or handed to the WAL replay). O(1).
    pub fn note_rows_not_landed(&self, rows: u64) {
        self.not_landed.fetch_add(rows, Ordering::Relaxed);
    }

    /// Rows counted by [`TickConservationLedger::note_rows_not_landed`] over
    /// this process's life. O(1).
    #[must_use]
    pub fn rows_not_landed(&self) -> u64 {
        self.not_landed.load(Ordering::Relaxed)
    }
}

/// The ledger the production Dhan tick writer reports to.
static LEDGER: TickConservationLedger = TickConservationLedger::new();

/// The process-wide ledger. O(1).
#[must_use]
pub fn ledger() -> &'static TickConservationLedger {
    &LEDGER
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 20_387; // 2025-10-26 as a day number, any value works
    const NOON: i64 = 12 * 3_600 * 1_000_000_000;

    fn ts_on(day: i64) -> i64 {
        day * NANOS_PER_DAY + NOON
    }

    #[test]
    fn test_ledger_new_is_empty() {
        let l = TickConservationLedger::new();
        assert_eq!(l.accepted_rows_on(DAY), 0);
        assert_eq!(l.rows_not_landed(), 0);
    }

    #[test]
    fn test_note_row_appended_and_accepted_rows_on_count_per_day() {
        let l = TickConservationLedger::new();
        for _ in 0..3 {
            l.note_row_appended(ts_on(DAY));
        }
        l.note_row_appended(ts_on(DAY - 1));
        assert_eq!(l.accepted_rows_on(DAY), 3);
        assert_eq!(l.accepted_rows_on(DAY - 1), 1);
        assert_eq!(l.accepted_rows_on(DAY + 1), 0);
    }

    #[test]
    fn test_day_boundary_uses_the_row_timestamp_day() {
        let l = TickConservationLedger::new();
        l.note_row_appended(DAY * NANOS_PER_DAY); // 00:00:00 starts DAY
        l.note_row_appended(DAY * NANOS_PER_DAY - 1); // last nano of DAY - 1
        assert_eq!(l.accepted_rows_on(DAY), 1);
        assert_eq!(l.accepted_rows_on(DAY - 1), 1);
        assert_eq!(TickConservationLedger::ist_day_of(-1), -1);
    }

    #[test]
    fn test_slot_reused_by_a_later_day_resets_the_count() {
        let l = TickConservationLedger::new();
        l.note_row_appended(ts_on(DAY));
        l.note_row_appended(ts_on(DAY));
        let later = DAY + LEDGER_DAY_SLOTS as i64; // same slot
        l.note_row_appended(ts_on(later));
        assert_eq!(l.accepted_rows_on(later), 1);
        // The old day is gone, never reported as the new day's count.
        assert_eq!(l.accepted_rows_on(DAY), 0);
    }

    #[test]
    fn test_note_rows_not_landed_accumulates() {
        let l = TickConservationLedger::new();
        l.note_rows_not_landed(5);
        l.note_rows_not_landed(0);
        l.note_rows_not_landed(7);
        assert_eq!(l.rows_not_landed(), 12);
    }

    #[test]
    fn test_global_ledger_is_one_instance() {
        assert!(std::ptr::eq(ledger(), ledger()));
    }

    #[test]
    fn test_extreme_timestamps_do_not_panic() {
        let l = TickConservationLedger::new();
        l.note_row_appended(i64::MAX);
        l.note_row_appended(i64::MIN);
        assert_eq!(
            l.accepted_rows_on(TickConservationLedger::ist_day_of(i64::MAX)),
            1
        );
    }
}
