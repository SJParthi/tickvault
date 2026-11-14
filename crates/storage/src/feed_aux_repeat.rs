//! Skips EXACT repeats of a contract's `no_trade_today` row in
//! `feed_aux_packets` (operator Quote 61, 2026-11-14).
//!
//! # Why
//!
//! A live packet for an option contract that has not traded yet today still
//! carries its last trade from an earlier day, so the day rule keeps it out of
//! `ticks` and it is written to `feed_aux_packets` instead (kind
//! `no_trade_today`, `connect_snapshot` for frames received before the
//! cutover in `feed_aux_persistence`). Measured on 2026-11-11: 3,212,510 such
//! rows, 99.69% identical to the same contract's previous row. The owner
//! chose "Skip exact repeats (Recommended)": store a row only when something
//! in it changed. Every packet is still in the raw frame log (the WAL, copied
//! to S3), so nothing received is lost.
//!
//! # The rule
//!
//! A row is SKIPPED only when its `kind` and every stored value column (all
//! thirteen of them, compared bit for bit, NULL against NULL) equal the last
//! row this filter let through for the same composite key
//! `(security_id, exchange segment)` (I-P1-11) on the same IST receipt day,
//! AND no gap happened since that row was let through. Everything else is
//! written:
//!
//! | Case | Verdict |
//! |---|---|
//! | first row for a key (in this process, this day, since the last clear) | write |
//! | any one value column differs, or NULL against a value | write |
//! | another `kind` (the receipt-day label cutover) | write |
//! | same id, other segment | its own key: written |
//! | key not tracked because the map is full | write (counted `untracked`) |
//! | the gap epoch moved since the key's last row | write |
//! | the key was judged by `check_never_skip` (a contract on two sockets) | write |
//!
//! The other stored columns: `ts` (the receipt time), `received_at` and
//! `capture_seq` differ on every packet and are the reason a repeat is a
//! repeat; `security_id` and `segment` are the key; `feed` is fixed per
//! writer. The `contract` name is not compared per row: it is read from the
//! published name table when the row is staged, and the production gap epoch
//! includes that table's publish generation, so after any publish (a name
//! added, renamed or removed) every key's next row is written.
//!
//! # The gap epoch
//!
//! The filter only sees what the drain folds. Things that can make "the last
//! row this filter let through" differ from "the last row stored" for a key:
//! a frame the live drain never saw (the socket-to-drain ring shed it; the WAL
//! keeps it and a later replay writes its row, possibly a different value), a
//! staged row that never reached QuestDB (a failed flush, rescue or spill),
//! and a replay gap (frames between two replayed frames were not re-read,
//! while live may have stored other rows from them). The first two are counted
//! process-wide and the caller passes a function that sums them, plus the name
//! table's generation ([`AuxRepeatFilter::with_gap_epoch`]); the caller marks
//! replay gaps itself ([`AuxRepeatFilter::mark_gap`]). Every remembered row is
//! stamped with the epoch read when it was let through, and a row stamped with
//! an older epoch never arms a skip, so the first packet after any such gap is
//! written. Invalidating is O(1) (no clear), because a shed burst moves the
//! epoch on almost every packet and a replay can mark a gap per frame.
//!
//! # Contracts on two sockets (never skipped)
//!
//! A ring shed is counted on the socket's own read task, so it is ordered
//! before every later frame of THAT socket, never before a frame of another
//! one. A contract the main feed carries twice (the backup copy, scope lock
//! 2026-10-02) can therefore reach the drain from the second socket before
//! the first socket's shed is counted: a packet whose value matches the last
//! written row would be skipped although a shed packet in between held
//! another value, and the stored rows would end on that other value with
//! nothing counting it (Quote 61 review, 2026-11-14). The caller judges such a
//! contract with [`AuxRepeatFilter::check_never_skip`]: its row is always
//! written, and the key stays unskippable for the rest of the IST day (and of
//! the replay pass), so a copy still in flight after the contract leaves the
//! backup set is written too.
//!
//! # Frames with no WAL copy (never skipped)
//!
//! The skip rests on the WAL keeping every packet. A frame the WAL refused
//! still reaches the drain through the ring, but has no copy anywhere else,
//! so the caller judges its rows with [`AuxRepeatFilter::check_always_write`]:
//! always written, never counted as skipped. The key is not pinned, so a later
//! repeat from a WAL-backed frame is judged against the noted values as usual
//! (2026-11-14 audit fix).
//!
//! # Complexity and space
//!
//! O(1) average per packet: one call of the epoch function (three
//! atomic loads in production), one hash probe in [`AuxRepeatFilter::check`],
//! and at most one probe and one insert in [`AuxRepeatFilter::note_written`].
//! The map is pre-sized at [`FEED_AUX_REPEAT_MAX_TRACKED`] (25,000, the same
//! figure as every per-instrument map on this universe) and never grows past
//! it, so no packet allocates. Past the cap a NEW key is written and not
//! tracked (fail closed toward writing, never toward skipping). Cleared at the
//! IST day roll (once a day, O(capacity)) and by [`AuxRepeatFilter::clear`]
//! at the start and the end of every WAL replay pass.
//!
//! **Honest limits:** a row is remembered once it is STAGED; if its batch
//! later fails to land, packets skipped between the staging and the moment the
//! failure is counted are not written again (the next packet after the count
//! is). A replay stores MORE rows than a live run over
//! the same frames: the catch-up drain refolds one WAL segment per pass and
//! every pass starts the filter over, and every replay gap writes each key's
//! next row, so each contract's first row is written again per segment and
//! per gap (up to tracked keys × segments rows for a replayed session, against
//! about 10,000 for a live day). Never fewer rows.

use std::collections::HashMap;

use crate::feed_aux_persistence::AuxPacketRow;

/// Most composite keys the filter remembers. Past it a new key is written,
/// never skipped.
pub const FEED_AUX_REPEAT_MAX_TRACKED: usize = 25_000;

/// Rows skipped because they repeated the key's last written row. Local
/// `/metrics` only; not a loss (the identical row is stored, the packet is in
/// the WAL and the raw frame log), never paged. Only rows of WAL-backed frames
/// are skipped: a frame the WAL refused is judged with
/// [`AuxRepeatFilter::check_always_write`] and never counted here (2026-11-14
/// audit fix).
pub const FEED_AUX_REPEAT_SKIPPED_COUNTER: &str = "tv_feed_aux_repeat_skipped_total";

/// Rows written for a key the full map could not track. Local `/metrics`
/// only; such rows are all written, so it measures lost SAVINGS, not data.
pub const FEED_AUX_REPEAT_UNTRACKED_COUNTER: &str = "tv_feed_aux_repeat_untracked_total";

/// Nanoseconds in a day.
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// Every stored value column of an aux row, bit for bit, plus the gap epoch
/// read when it was judged. `present` has one bit per column (NULL = 0), so
/// NULL and `0` never compare equal. `never_skip` marks a key judged by
/// [`AuxRepeatFilter::check_never_skip`]: its rows are all written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxRowValues {
    epoch: u64,
    kind: u8,
    present: u16,
    bits: [u64; 13],
    never_skip: bool,
}

impl AuxRowValues {
    fn of(row: &AuxPacketRow, epoch: u64) -> Self {
        // `kind` is a stored SYMBOL too (and in the DEDUP key): a row of
        // another kind is never a repeat. `index()` is below 8, so it fits.
        let kind = u8::try_from(row.kind.index()).unwrap_or(u8::MAX);
        let i = |v: Option<i64>| v.map(i64::cast_unsigned);
        let f = |v: Option<f64>| v.map(f64::to_bits);
        let cols = [
            i(row.oi),
            f(row.prev_close),
            i(row.prev_oi),
            f(row.ltp),
            i(row.exchange_ts_ist_nanos),
            i(row.volume),
            f(row.day_open),
            f(row.day_high),
            f(row.day_low),
            f(row.day_close),
            i(row.oi_day_high),
            i(row.oi_day_low),
            i(row.reason_code),
        ];
        let mut out = Self {
            epoch,
            kind,
            present: 0,
            bits: [0; 13],
            never_skip: false,
        };
        for (n, col) in cols.iter().enumerate() {
            if let Some(b) = col {
                out.present |= 1 << n;
                out.bits[n] = *b;
            }
        }
        out
    }
}

/// What [`AuxRepeatFilter::check`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxRepeatVerdict {
    /// Write the row; once it is staged pass these values to
    /// [`AuxRepeatFilter::note_written`].
    Write(AuxRowValues),
    /// The row repeats the key's last written row exactly; skip it.
    Skip,
}

/// The last written values per composite key. See the module docs.
pub struct AuxRepeatFilter {
    last: HashMap<(u64, u8), AuxRowValues>,
    day: i64,
    gap_epoch: fn() -> u64,
    /// Gaps the caller marked by hand ([`Self::mark_gap`]): added to the gap
    /// epoch, never reset, so it only ever moves the epoch forward.
    local_gaps: u64,
    skipped: metrics::Counter,
    untracked: metrics::Counter,
}

impl AuxRepeatFilter {
    /// A filter pre-sized at [`FEED_AUX_REPEAT_MAX_TRACKED`] that reads the
    /// gap epoch from `gap_epoch` (see the module docs; it must move whenever
    /// a gap happens). Registers both counters at 0.
    #[must_use]
    pub fn with_gap_epoch(gap_epoch: fn() -> u64) -> Self {
        let skipped = metrics::counter!(FEED_AUX_REPEAT_SKIPPED_COUNTER);
        let untracked = metrics::counter!(FEED_AUX_REPEAT_UNTRACKED_COUNTER);
        skipped.increment(0);
        untracked.increment(0);
        Self {
            last: HashMap::with_capacity(FEED_AUX_REPEAT_MAX_TRACKED),
            day: i64::MIN,
            gap_epoch,
            local_gaps: 0,
            skipped,
            untracked,
        }
    }

    /// Decides whether `row` (for composite key `key`) is written. Clears the
    /// filter when the row's IST receipt day differs from the last one seen.
    /// A skip is counted here. A key once judged by
    /// [`Self::check_never_skip`] (and still remembered) is never skipped.
    /// O(1) average, no allocation.
    pub fn check(&mut self, key: (u64, u8), row: &AuxPacketRow) -> AuxRepeatVerdict {
        self.judge(key, row, false, false)
    }

    /// [`Self::check`] for a contract the main feed carries on two sockets
    /// (the backup copy): always `Write`, and once the values are noted the
    /// key stays unskippable until the day roll or a [`Self::clear`] (see
    /// "Contracts on two sockets" in the module docs). O(1) average, no
    /// allocation.
    pub fn check_never_skip(&mut self, key: (u64, u8), row: &AuxPacketRow) -> AuxRepeatVerdict {
        self.judge(key, row, true, true)
    }

    /// [`Self::check`] for a row whose packet has NO WAL copy (the WAL
    /// refused its frame, so the drain got it through the ring unbacked):
    /// always `Write`, because the skip rests on the WAL keeping the packet
    /// (Quote 61). The key is NOT pinned: once the values are noted, a later
    /// repeat from a WAL-backed frame is judged against them as usual
    /// (2026-11-14 audit fix). O(1) average, no allocation.
    pub fn check_always_write(&mut self, key: (u64, u8), row: &AuxPacketRow) -> AuxRepeatVerdict {
        self.judge(key, row, true, false)
    }

    /// `write_always`: never `Skip`. `pin`: the noted values keep the key
    /// unskippable (a remembered pin is kept whatever is asked).
    fn judge(
        &mut self,
        key: (u64, u8),
        row: &AuxPacketRow,
        write_always: bool,
        pin: bool,
    ) -> AuxRepeatVerdict {
        let day = row.ts_ist_nanos.div_euclid(NANOS_PER_DAY);
        if day != self.day {
            self.last.clear();
            self.day = day;
        }
        let epoch = (self.gap_epoch)().wrapping_add(self.local_gaps);
        let mut values = AuxRowValues::of(row, epoch);
        match self.last.get(&key) {
            Some(prev) if prev.never_skip => values.never_skip = true,
            Some(prev) if !write_always && *prev == values => {
                self.skipped.increment(1);
                return AuxRepeatVerdict::Skip;
            }
            _ => values.never_skip = pin,
        }
        AuxRepeatVerdict::Write(values)
    }

    /// Remembers `values` as `key`'s last WRITTEN row. Call once the row is
    /// staged. A tracked key is updated in place even when the map is full;
    /// a new key past the cap is not remembered (counted). O(1) average, no
    /// allocation (the map never grows past its pre-size).
    pub fn note_written(&mut self, key: (u64, u8), values: AuxRowValues) {
        if let Some(slot) = self.last.get_mut(&key) {
            *slot = values;
        } else if self.last.len() < FEED_AUX_REPEAT_MAX_TRACKED {
            self.last.insert(key, values);
        } else {
            self.untracked.increment(1);
        }
    }

    /// Marks a gap the caller knows of (a WAL replay gap: frames between two
    /// replayed frames were not re-read, and live may have stored a different
    /// row for a key in them). Every remembered row stops arming a skip, so
    /// the first row per key after the gap is written. O(1), no clear: a
    /// replay can mark a gap per frame.
    pub fn mark_gap(&mut self) {
        self.local_gaps = self.local_gaps.wrapping_add(1);
    }

    /// Forgets every key (start and end of a WAL replay pass).
    /// O(capacity).
    pub fn clear(&mut self) {
        self.last.clear();
        self.day = i64::MIN;
    }
}

#[cfg(test)]
#[path = "feed_aux_repeat_tests.rs"]
mod tests;
