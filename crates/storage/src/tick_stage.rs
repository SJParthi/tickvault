//! The tick writer's STAGED rows (plan item PF-4, 2026-11-06).
//!
//! Until this change the frame drain wrote every `ticks`, `tick_quotes` and
//! `feed_aux_packets` row as ILP text into the writer's buffer: about 300
//! bytes of digit formatting per tick, plus a contract-name probe, on the
//! task that empties the sockets. Now the drain keeps the row it already
//! builds (`TickRow` or `AuxPacketRow`, every gate, counter and ledger note
//! unchanged and still on the drain) and pushes one `Copy` value into a
//! pre-sized, pre-touched [`StagedBatch`]. The text is written later, by the
//! thread that needs it, with the UNCHANGED write functions:
//!
//! * the writer thread (`TickWriterSink::write`), before the POST;
//! * the rescue thread (`TickRescueSink::rescue`), before the spill write;
//! * the drain's cold paths that already block (the inline spill and the
//!   unsplit synchronous flush), inside their existing `off_worker` closures.
//!
//! # Byte identity: contract-name pins
//!
//! The write functions read the global contract-name table
//! (`candle_contract_labels`). Written later, a row could read a NEWER table
//! than the one in force when the drain accepted it. So each batch PINS every
//! name table its rows saw: per row one table load (the same load the old
//! text step did) and one pointer compare; on a change, the table's `Arc` is
//! kept in the next of [`TICK_STAGE_LABEL_PINS`] slots (one atomic increment,
//! no allocation) and the row carries the slot number. A batch that would see
//! a ninth table is handed off early first (`TickWriter`'s pin cut), never
//! dropped. Pins are released where the batch is cleared: on the writer
//! thread for every normal batch.
//!
//! # A row the encoder refuses (owner decision 2026-11-06, "count on writer")
//!
//! The drain no longer learns that an encoder refused a row (believed
//! unreachable after the drain's gates: the encoders refuse only a negative
//! designated timestamp). The thread that encodes counts it exactly as the
//! drain used to, before that batch's acknowledgement or rescue-floor
//! release: the frame is marked unapplied (the next replay re-offers it), the
//! alarmed `tv_ticks_dropped_total` and the conservation ledger's not-landed
//! total each rise by one, and one coded line is logged at powers of two
//! (`source = "stage_encode_failed"`). The row is then replaced in the batch
//! by a `Refused` marker, so a second encode of the same batch (a failed
//! synchronous flush that falls back to the spill) cannot count it twice.
//!
//! # Complexity
//!
//! Staging: O(1) per row, no allocation inside the pre-size. Encoding:
//! O(row bytes) per row, on the thread that encodes. Pre-touching a batch:
//! O(capacity / rows per page) stores, at the split and when a fresh batch is
//! reserved on the flush arm. SPACE: [`TICK_STAGED_ROWS_PRESIZE`] ×
//! [`STAGED_ROW_BYTES`] per batch.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use arc_swap::Guard;
use tracing::error;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

use crate::candle_contract_labels::CandleContractLabels;
use crate::feed_aux_persistence::{AuxPacketKind, AuxPacketRow};
use crate::ilp_table_gate::GatedTable;
use crate::tick_ilp::{TickIlpEncoder, TickRowBuffer};
use crate::tick_persistence::raw::RawTick;
use crate::tick_persistence::{
    MAX_PRODUCER_BUFFER_BYTES, QUESTDB_MAX_BUF_SIZE_BYTES, TICKS_TABLE, TickRow,
};

/// Distinct contract-name tables one batch may pin. Names are published a few
/// times a day, so a batch normally pins one; the 09:00-09:15 attach can
/// republish while a batch is held across a full queue.
pub const TICK_STAGE_LABEL_PINS: usize = 8;

/// One staged row: exactly what the old text step consumed.
///
/// Speed fix 6 (2026-11-17): a drain-accepted tick is staged RAW (prices
/// still `f32`, see [`RawTick`]) and converted on the thread that writes the
/// text; every other row (`feed_aux_packets`, and a prepared [`TickRow`]
/// staged through the writer's test-only `append_row`) lives whole in the
/// batch's side rows ([`ExtraRow`]), and a RUN of consecutive side rows of
/// one table and one name pin is one [`StagedRow::Extra`], so this enum is
/// 96 bytes rather than 288 and an aux burst writes no staged row per aux
/// row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum StagedRow {
    /// A `ticks` row, from a drain-accepted tick.
    Tick {
        /// The tick, as the drain accepted and gated it.
        raw: RawTick,
        /// Index into the batch's name pins.
        pin: u8,
    },
    /// A `tick_quotes` row: the same tick, written with its arrival time as
    /// the designated `ts` ([`RawTick::to_quote_row`]).
    Quote {
        /// The tick, as the drain accepted and gated it.
        raw: RawTick,
        /// Index into the batch's name pins.
        pin: u8,
    },
    /// A run of `len` side rows ([`ExtraRow`]) from index `first`, all of one
    /// table and one name pin, in staging order.
    Extra {
        /// Index of the run's first side row.
        first: u32,
        /// Side rows in the run (at least one).
        len: u32,
        /// Index into the batch's name pins.
        pin: u8,
        /// The run's table (read without the side rows).
        table: GatedTable,
    },
    /// A row its encoder refused once (already counted); skipped by every
    /// later encode of the batch.
    Refused {
        /// The refused row's capture sequence.
        capture_seq: i64,
    },
}

/// A staged row too large, or too rare, for the 96-byte [`StagedRow`]
/// (speed fix 6): it is kept whole in the batch's side rows.
#[derive(Clone, Copy, Debug, PartialEq)]
// The small `Refused` marker is the point: the side rows are an inline,
// pre-sized `Vec`, and boxing the large variant would allocate per aux row.
#[allow(clippy::large_enum_variant)] // APPROVED: boxing would allocate per aux row
pub(crate) enum ExtraRow {
    /// A prepared `ticks` row (the writer's test-only `append_row`).
    #[cfg(test)]
    Tick(TickRow),
    /// A prepared `tick_quotes` row and its vendor trade time (tests).
    #[cfg(test)]
    Quote {
        /// The row, its designated `ts` already the arrival time.
        row: TickRow,
        /// The vendor's last trade time (IST nanos), `None` for a never-traded 0.
        exchange_ts: Option<i64>,
    },
    /// A `feed_aux_packets` row.
    Aux(AuxPacketRow),
    /// A side row its encoder refused once (already counted); skipped by
    /// every later encode of the batch.
    Refused {
        /// The refused row's capture sequence.
        capture_seq: i64,
    },
}

impl ExtraRow {
    /// The table the row is written to; `None` for a refused row. O(1).
    #[must_use]
    pub(crate) const fn table(&self) -> Option<GatedTable> {
        match self {
            #[cfg(test)]
            Self::Tick(_) => Some(GatedTable::Ticks),
            #[cfg(test)]
            Self::Quote { .. } => Some(GatedTable::TickQuotes),
            Self::Aux(_) => Some(GatedTable::FeedAuxPackets),
            Self::Refused { .. } => None,
        }
    }

    /// The row's capture sequence. O(1).
    #[must_use]
    pub(crate) const fn capture_seq(&self) -> i64 {
        match self {
            #[cfg(test)]
            Self::Tick(row) | Self::Quote { row, .. } => row.capture_seq,
            Self::Aux(row) => row.capture_seq,
            Self::Refused { capture_seq } => *capture_seq,
        }
    }
}

impl StagedRow {
    /// The table the row (or run) is written to; `None` for a refused row
    /// (B1 review 3). O(1).
    pub(crate) const fn table(&self) -> Option<GatedTable> {
        match self {
            Self::Tick { .. } => Some(GatedTable::Ticks),
            Self::Quote { .. } => Some(GatedTable::TickQuotes),
            Self::Extra { table, .. } => Some(*table),
            Self::Refused { .. } => None,
        }
    }
}

/// RAM one staged row holds.
pub const STAGED_ROW_BYTES: usize = std::mem::size_of::<StagedRow>();

/// RAM one side row ([`ExtraRow`]) holds.
pub const STAGED_EXTRA_ROW_BYTES: usize = std::mem::size_of::<ExtraRow>();

/// Address space one row SLOT of a batch reserves: its staged row plus a side
/// row (a batch reserves as many side rows as rows, so an all-aux batch, the
/// shape after 15:40, never grows). The park and its gauge count this
/// (speed fix 6). Both are pre-touched, so this is also committed RAM.
pub const STAGED_SLOT_BYTES: usize = STAGED_ROW_BYTES + STAGED_EXTRA_ROW_BYTES;

const _: () = assert!(
    STAGED_ROW_BYTES <= 112,
    "a staged row must stay small: it is what the drain writes per tick"
);

/// Rows every staged batch is reserved (and pre-touched) for. The old 2 MiB
/// text buffer held about 15,000 rows at the measured ~133 bytes a row; this
/// keeps at least that many. SPACE: 16,384 × [`STAGED_ROW_BYTES`] (4.5 MiB)
/// per batch. ⚠ CORRECTED 2026-11-06 (review round 1): "at most nine
/// batches" counted only the steady state (producer, four spares, four
/// queued). The real worst case, during a QuestDB stall, is the producer,
/// four queued and one in flight on the writer thread, four spares, two
/// queued and one in flight on the rescue thread (13 batches, about
/// 58.5 MiB at the pre-size) plus the park, bounded by its own row slots
/// (`PARKED_RESCUE_MAX_BYTES`, 36 MiB): about 95 MiB per tick writer. A batch
/// that grew past the pre-size holds more (counted on
/// `tv_tick_stage_grow_total`). Spent batches come back to the producer from
/// BOTH the writer and the rescue thread (cleared there), so a stall does
/// not keep allocating fresh ones on the drain.
/// ⚠ CHANGED 2026-11-17 (speed fix 6): a staged row is 96 bytes, not 288, so
/// a batch's rows are 1.5 MiB; its side rows ([`ExtraRow`]) are reserved
/// and pre-touched for the same count (4.5 MiB), so a pre-sized batch is two
/// allocations and commits about 6 MiB where it committed 4.5 MiB (13
/// batches: about 78 MiB, was 58.5). Pre-touching only part of the side rows
/// was tried and measured slower: a fresh batch's aux burst then took a page
/// fault every 14 rows on the drain (`feed_drain` wrong-day written,
/// +41%).
pub const TICK_STAGED_ROWS_PRESIZE: usize = 16_384;

/// The longest ILP line one staged row can encode to, in either encoder:
/// `tick_quotes` with every optional column, a 64-character contract name of
/// up to 4-byte characters, `i64::MIN` integers and the longest `f64` text.
/// Checked by `test_max_ilp_line_bytes_covers_a_maximal_row` in both encoders.
pub const MAX_ILP_LINE_BYTES: usize = 1_024;

/// The producer's row cut: the old 32 MiB text cut restated in rows at the
/// worst-case line, so a batch's text is never over
/// [`MAX_PRODUCER_BUFFER_BYTES`] whatever its rows. 32,768 rows.
pub const MAX_PRODUCER_ROWS: usize = MAX_PRODUCER_BUFFER_BYTES / MAX_ILP_LINE_BYTES;

const _: () = assert!(
    MAX_PRODUCER_ROWS * MAX_ILP_LINE_BYTES * 2 <= QUESTDB_MAX_BUF_SIZE_BYTES,
    "the producer row cut must keep the worst-case text at half the questdb-rs wedge"
);
const _: () = assert!(
    TICK_STAGED_ROWS_PRESIZE <= MAX_PRODUCER_ROWS,
    "a pre-sized batch must be one the spare lane will take back"
);

/// A batch of staged rows plus the name tables they saw.
#[derive(Debug)]
pub struct StagedBatch {
    rows: Vec<StagedRow>,
    /// Side rows (speed fix 6), named by [`StagedRow::Extra`]`::idx`.
    extras: Vec<ExtraRow>,
    pins: [Option<Arc<CandleContractLabels>>; TICK_STAGE_LABEL_PINS],
    pin_count: u8,
    /// Speed fix 5 (2026-11-17): the name-table publish generation read
    /// just before the table at the LAST pin was loaded, when known. While
    /// the generation has not moved, that table is the one a fresh load
    /// would return, so staging a row can skip the load. `None` after a pin
    /// of unknown generation and after [`Self::clear`].
    last_pin_generation: Option<u64>,
    /// Rows an encode marked `Refused` (each already counted by the encoder).
    refused: usize,
    /// Rows staged, side rows included (speed fix 6: a run of side rows is
    /// one staged row, so `rows.len()` no longer counts them).
    count: usize,
}

impl Default for StagedBatch {
    fn default() -> Self {
        Self::new()
    }
}

/// The table a row with no valid pin reads: unreachable (every staged row's
/// pin is set before the row is pushed); empty, so such a row would carry no
/// name rather than a guessed one.
static NO_LABELS: LazyLock<CandleContractLabels> = LazyLock::new(CandleContractLabels::default);

impl StagedBatch {
    /// An empty batch. Allocates nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows: Vec::new(),
            extras: Vec::new(),
            pins: [const { None }; TICK_STAGE_LABEL_PINS],
            pin_count: 0,
            last_pin_generation: None,
            refused: 0,
            count: 0,
        }
    }

    /// An empty batch reserved for [`TICK_STAGED_ROWS_PRESIZE`] rows (and as
    /// many side rows), touched as [`Self::reserve_presize`] says. Two
    /// allocations (rows and side rows); cold (construction and the split).
    #[must_use]
    pub(crate) fn presized() -> Self {
        let mut batch = Self::new();
        let _fresh = batch.reserve_presize();
        batch
    }

    /// Reserves the rows and the side rows to the pre-size, when smaller, and
    /// touches every new page of both. At most two allocations; a
    /// batch already that big costs two compares. Returns `true` when it
    /// reserved (2026-11-16: the writer counts a fresh reserve on the drain's
    /// hand-off, `tv_tick_stage_presize_fresh_total`).
    pub(crate) fn reserve_presize(&mut self) -> bool {
        let mut reserved = false;
        if self.rows.capacity() < TICK_STAGED_ROWS_PRESIZE {
            let have = self.rows.len();
            self.rows
                .reserve_exact(TICK_STAGED_ROWS_PRESIZE.saturating_sub(have));
            prefault(&mut self.rows, StagedRow::Refused { capture_seq: 0 });
            reserved = true;
        }
        if self.extras.capacity() < TICK_STAGED_ROWS_PRESIZE {
            let have = self.extras.len();
            self.extras
                .reserve_exact(TICK_STAGED_ROWS_PRESIZE.saturating_sub(have));
            prefault(&mut self.extras, EXTRA_FILLER);
            reserved = true;
        }
        reserved
    }

    /// Rows staged (refused ones included).
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether nothing is staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Rows the batch holds before it must grow.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.rows.capacity()
    }

    /// The pin index for the table `labels` (a fresh table load), or the
    /// load handed back when all [`TICK_STAGE_LABEL_PINS`] slots hold other
    /// tables. O(1): one pointer compare; on a new table one `Arc` increment.
    pub(crate) fn try_pin(
        &mut self,
        labels: Guard<Arc<CandleContractLabels>>,
    ) -> std::result::Result<u8, Guard<Arc<CandleContractLabels>>> {
        if let Some(last) = self
            .pin_count
            .checked_sub(1)
            .and_then(|i| self.pins.get(usize::from(i)))
            .and_then(Option::as_ref)
            && Arc::ptr_eq(last, &labels)
        {
            return Ok(self.pin_count - 1);
        }
        let Some(slot) = self.pins.get_mut(usize::from(self.pin_count)) else {
            return Err(labels);
        };
        *slot = Some(Guard::into_inner(labels));
        // Pinned with no generation: the next row loads the table again.
        self.last_pin_generation = None;
        self.pin_count += 1;
        Ok(self.pin_count - 1)
    }

    /// [`Self::try_pin`] for a table loaded right AFTER `generation` was
    /// read (speed fix 5, 2026-11-17): on success under a settled (even)
    /// generation, the generation is kept with the last pin, so [`Self::current_pin_at_generation`] can answer
    /// the next row without a table load. O(1).
    pub(crate) fn try_pin_at_generation(
        &mut self,
        labels: Guard<Arc<CandleContractLabels>>,
        generation: u64,
    ) -> std::result::Result<u8, Guard<Arc<CandleContractLabels>>> {
        let pinned = self.try_pin(labels);
        // Kept only under a settled (even) generation: an odd one means a
        // publish may be storing right now, so the next lookup loads again.
        if pinned.is_ok() && crate::candle_contract_labels::generation_is_settled(generation) {
            self.last_pin_generation = Some(generation);
        }
        pinned
    }

    /// The last pin, when it was loaded under `generation` (speed fix 5):
    /// no table was published since, so a fresh table load would return
    /// the table already pinned there and [`Self::try_pin`] would answer
    /// this same index. `None` sends the caller to the load. O(1), one
    /// compare.
    #[inline]
    pub(crate) fn current_pin_at_generation(&self, generation: u64) -> Option<u8> {
        if self.last_pin_generation == Some(generation) {
            self.pin_count.checked_sub(1)
        } else {
            None
        }
    }

    /// Pushes one row. Returns `true` when the batch had to grow (past its
    /// pre-size: one reallocation, cold, counted by the caller). Never drops.
    pub(crate) fn push(&mut self, row: StagedRow) -> bool {
        let grew = self.rows.len() == self.rows.capacity();
        self.rows.push(row);
        self.count += 1;
        grew
    }

    /// Pushes one side row (speed fix 6). When the last staged row is a run
    /// of side rows of the same table and pin, the run grows by one (one
    /// store); otherwise a new [`StagedRow::Extra`] run starts. Returns `true`
    /// when either store had to grow (cold, counted by the caller). Never
    /// drops a row. O(1).
    pub(crate) fn push_extra(&mut self, extra: ExtraRow, pin: u8) -> bool {
        // A refused side row is only ever made by an encode, in place.
        let Some(table) = extra.table() else {
            return false;
        };
        let grew = self.extras.len() == self.extras.capacity();
        // Unreachable past `u32::MAX` (the producer cut is 32,768 rows); a
        // missing index is a counted encode failure, never a silent skip.
        let first = u32::try_from(self.extras.len()).unwrap_or(u32::MAX);
        self.extras.push(extra);
        if let Some(StagedRow::Extra {
            first: run_first,
            len,
            pin: run_pin,
            table: run_table,
        }) = self.rows.last_mut()
            && *run_pin == pin
            && *run_table == table
            && run_first.checked_add(*len) == Some(first)
        {
            *len += 1;
            self.count += 1;
            return grew;
        }
        let row_grew = self.push(StagedRow::Extra {
            first,
            len: 1,
            pin,
            table,
        });
        grew || row_grew
    }

    /// Empties the batch and releases its pins, keeping the capacity.
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.extras.clear();
        self.pins = [const { None }; TICK_STAGE_LABEL_PINS];
        self.pin_count = 0;
        self.last_pin_generation = None;
        self.refused = 0;
        self.count = 0;
    }

    /// Empties the rows but KEEPS the name pins and the capacity (review
    /// round 1, 2026-11-06). This is the clear a split writer uses on the
    /// drain: dropping a pin there could free the last reference to a
    /// replaced name table (about 120,000 entries) inside a frame. The pins
    /// go when the batch next reaches the writer or rescue thread, whose
    /// [`Self::clear`] drops them; until then they stay deduplicated and at
    /// most [`TICK_STAGE_LABEL_PINS`] (a ninth table hands the batch off
    /// early). O(1).
    pub(crate) fn clear_rows(&mut self) {
        self.rows.clear();
        self.extras.clear();
        self.refused = 0;
        self.count = 0;
    }

    /// Name tables this batch pins (tests).
    #[cfg(test)]
    pub(crate) fn pin_count(&self) -> usize {
        usize::from(self.pin_count)
    }

    /// Rows an encode of this batch refused and already counted.
    #[must_use]
    pub(crate) fn refused(&self) -> usize {
        self.refused
    }

    /// An empty batch with room for exactly `rows` rows and as many side
    /// rows (tests).
    #[cfg(test)]
    pub(crate) fn with_capacity_for_test(rows: usize) -> Self {
        let mut batch = Self::new();
        batch.rows.reserve_exact(rows);
        batch.extras.reserve_exact(rows);
        batch
    }

    /// The rows' allocation, to prove a batch was reused (tests).
    #[cfg(test)]
    pub(crate) fn as_ptr_for_test(&self) -> *const StagedRow {
        self.rows.as_ptr()
    }

    /// The name table pinned at `pin`.
    fn labels(&self, pin: u8) -> &CandleContractLabels {
        self.pins
            .get(usize::from(pin))
            .and_then(Option::as_deref)
            .unwrap_or(&NO_LABELS)
    }

    /// Encodes every staged row, in order, into `out`, through the unchanged
    /// write functions; returns the rows written. A refused row is counted on
    /// THIS thread (see the module docs) and marked `Refused`. O(row bytes)
    /// per row; allocation-free while `out` has room.
    pub(crate) fn encode_into(&mut self, feed: Feed, out: &mut TickRowBuffer) -> usize {
        self.encode_matching(feed, out, |_| true)
    }

    /// [`Self::encode_into`] for the rows whose table `keep` accepts only
    /// (B1 review 2; per table since B1 review 3): the writer thread posts the
    /// `ticks` rows while `tick_quotes` or `feed_aux_packets` alone is not
    /// keyed, and sets each held table's rows aside on their own. Same
    /// counting and marking of a refused row; an already refused row belongs
    /// to no table and is skipped. O(rows) to pass, O(row bytes) per row
    /// written; allocation-free while `out` has room.
    pub(crate) fn encode_tables_into(
        &mut self,
        feed: Feed,
        out: &mut TickRowBuffer,
        keep: impl Fn(GatedTable) -> bool,
    ) -> usize {
        self.encode_matching(feed, out, |table| table.is_some_and(&keep))
    }

    /// Whether the batch holds a row for `table`. O(rows); called only while
    /// that table is not keyed (B1 review 2), never per row.
    #[must_use]
    pub(crate) fn has_rows_for(&self, table: GatedTable) -> bool {
        // O(1) EXEMPT: cold, only while that table is not keyed.
        self.rows.iter().any(|r| r.table() == Some(table))
    }

    /// The encode that marks: a row (or side row) its encoder refuses is
    /// counted and replaced by a `Refused` marker. `keep` judges each staged
    /// row (a run as a whole) by its table.
    fn encode_matching(
        &mut self,
        feed: Feed,
        out: &mut TickRowBuffer,
        keep: impl Fn(Option<GatedTable>) -> bool,
    ) -> usize {
        let mut encoded = 0;
        for i in 0..self.rows.len() {
            let Some(row) = self.rows.get(i).copied() else {
                continue;
            };
            if !keep(row.table()) {
                continue;
            }
            if let StagedRow::Extra {
                first, len, pin, ..
            } = row
            {
                for j in run_indices(first, len) {
                    match self.encode_extra(j, pin, feed.as_str(), out) {
                        Ok(true) => encoded += 1,
                        Ok(false) => {}
                        Err(err) => {
                            let capture_seq = self.extra_capture_seq(j);
                            note_encode_failure(feed, capture_seq, &err);
                            if let Some(slot) = self.extras.get_mut(j) {
                                *slot = ExtraRow::Refused { capture_seq };
                                self.refused += 1;
                            }
                        }
                    }
                }
                continue;
            }
            match encode_row(&row, self, feed.as_str(), out) {
                Ok(true) => encoded += 1,
                Ok(false) => {}
                Err(err) => {
                    let capture_seq = row_capture_seq(&row);
                    note_encode_failure(feed, capture_seq, &err);
                    if let Some(slot) = self.rows.get_mut(i) {
                        *slot = StagedRow::Refused { capture_seq };
                        self.refused += 1;
                    }
                }
            }
        }
        encoded
    }

    /// Every row in order, without marking: `on_err` gets each refused row's
    /// capture sequence. Returns the rows written. O(row bytes) per row.
    fn encode_each(
        &self,
        feed: Feed,
        out: &mut TickRowBuffer,
        mut on_err: impl FnMut(i64, &anyhow::Error),
    ) -> usize {
        let mut encoded = 0;
        for row in &self.rows {
            if let StagedRow::Extra {
                first, len, pin, ..
            } = *row
            {
                for j in run_indices(first, len) {
                    match self.encode_extra(j, pin, feed.as_str(), out) {
                        Ok(true) => encoded += 1,
                        Ok(false) => {}
                        Err(err) => on_err(self.extra_capture_seq(j), &err),
                    }
                }
                continue;
            }
            match encode_row(row, self, feed.as_str(), out) {
                Ok(true) => encoded += 1,
                Ok(false) => {}
                Err(err) => on_err(row_capture_seq(row), &err),
            }
        }
        encoded
    }

    /// [`Self::encode_into`] for a batch encoded for the LAST time (the rescue
    /// thread's spill): a refused row is counted exactly as there, but the
    /// batch is not marked, since it is dropped right after. O(row bytes) per
    /// row.
    pub(crate) fn encode_counted(&self, feed: Feed, out: &mut TickRowBuffer) -> usize {
        self.encode_each(feed, out, |capture_seq, err| {
            note_encode_failure(feed, capture_seq, err);
        })
    }

    /// The batch's ILP text in `encoder`, without touching the batch or any
    /// counter; a refused row is left out. Tests and diagnostics only:
    /// allocates the returned text. O(rows).
    #[must_use]
    pub(crate) fn encode_readonly(&self, feed: Feed, encoder: TickIlpEncoder) -> Vec<u8> {
        let mut out = TickRowBuffer::new(encoder);
        let _written = self.encode_each(feed, &mut out, |_, _| {});
        out.as_bytes().to_vec()
    }

    /// The capture sequence of side row `j` (0 for a missing one, which is
    /// unreachable). O(1).
    fn extra_capture_seq(&self, j: usize) -> i64 {
        self.extras.get(j).map_or(0, ExtraRow::capture_seq)
    }

    /// Writes side row `j` through the write function the old text step used,
    /// inside a marker. `Ok(false)` for a side row already refused. O(row
    /// bytes).
    fn encode_extra(
        &self,
        j: usize,
        pin: u8,
        feed: &'static str,
        out: &mut TickRowBuffer,
    ) -> Result<bool> {
        let labels = self.labels(pin);
        match self.extras.get(j) {
            #[cfg(test)]
            Some(ExtraRow::Tick(row)) => write_tick(out, feed, TICKS_TABLE, row, None, labels)?,
            #[cfg(test)]
            Some(ExtraRow::Quote { row, exchange_ts }) => write_tick(
                out,
                feed,
                crate::tick_quotes_persistence::TICK_QUOTES_TABLE,
                row,
                *exchange_ts,
                labels,
            )?,
            Some(ExtraRow::Aux(row)) => {
                crate::tick_persistence::append_within_marker(out, |b| match b {
                    TickRowBuffer::Library(x) => {
                        crate::feed_aux_persistence::append_aux_row_with_labels(
                            x, feed, row, labels,
                        )
                    }
                    TickRowBuffer::Own(x) => {
                        crate::feed_aux_persistence::append_aux_row_with_labels(
                            x, feed, row, labels,
                        )
                    }
                })?;
            }
            Some(ExtraRow::Refused { .. }) => return Ok(false),
            None => anyhow::bail!("staged side row {j} is missing"),
        }
        Ok(true)
    }
}

/// The side-row indices of a run. O(1) to build.
fn run_indices(first: u32, len: u32) -> std::ops::Range<usize> {
    let start = usize::try_from(first).unwrap_or(usize::MAX);
    start..start.saturating_add(usize::try_from(len).unwrap_or(0))
}

/// The capture sequence of a staged row that is not a run (a run's side rows
/// carry their own). O(1).
const fn row_capture_seq(row: &StagedRow) -> i64 {
    match row {
        StagedRow::Tick { raw, .. } | StagedRow::Quote { raw, .. } => raw.capture_seq(),
        StagedRow::Refused { capture_seq } => *capture_seq,
        StagedRow::Extra { .. } => 0,
    }
}

/// Writes one row through the write function the old text step used, inside
/// a marker (a refused row leaves `out` as it was). `Ok(false)` for a row
/// already refused. A raw tick is converted here ([`RawTick::to_row`]), on
/// the encoding thread (speed fix 6). A run of side rows is written by
/// [`StagedBatch::encode_extra`], never here.
fn encode_row(
    row: &StagedRow,
    batch: &StagedBatch,
    feed: &'static str,
    out: &mut TickRowBuffer,
) -> Result<bool> {
    match row {
        StagedRow::Tick { raw, pin } => {
            write_tick(
                out,
                feed,
                TICKS_TABLE,
                &raw.to_row(),
                None,
                batch.labels(*pin),
            )?;
        }
        StagedRow::Quote { raw, pin } => {
            let (quote, exchange_ts) = raw.to_quote_row();
            write_tick(
                out,
                feed,
                crate::tick_quotes_persistence::TICK_QUOTES_TABLE,
                &quote,
                exchange_ts,
                batch.labels(*pin),
            )?;
        }
        StagedRow::Extra { .. } => anyhow::bail!("a run of side rows reached the row encoder"),
        StagedRow::Refused { .. } => return Ok(false),
    }
    Ok(true)
}

/// One `ticks` or `tick_quotes` line through the unchanged writer, inside a
/// marker. O(row bytes).
fn write_tick(
    out: &mut TickRowBuffer,
    feed: &'static str,
    table: &'static str,
    row: &TickRow,
    exchange_ts: Option<i64>,
    labels: &CandleContractLabels,
) -> Result<()> {
    crate::tick_persistence::append_within_marker(out, |b| {
        crate::tick_persistence::write_tick_line_with_labels(
            b,
            table,
            feed,
            row,
            exchange_ts,
            labels,
        )
    })
}

/// The side-row filler [`prefault`] writes: never read (the length is never
/// changed), only there to touch the page.
const EXTRA_FILLER: ExtraRow = ExtraRow::Aux(AuxPacketRow {
    kind: AuxPacketKind::MarketStatus,
    security_id: 0,
    segment: "",
    ts_ist_nanos: 0,
    received_at_ist_nanos: None,
    capture_seq: 0,
    oi: None,
    prev_close: None,
    prev_oi: None,
    ltp: None,
    exchange_ts_ist_nanos: None,
    volume: None,
    day_open: None,
    day_high: None,
    day_low: None,
    day_close: None,
    oi_day_high: None,
    oi_day_low: None,
    reason_code: None,
});

/// Touches each page of `rows`' spare capacity once, so the first fill takes
/// no page faults inside the frames that stage. Safe: writes go through
/// `MaybeUninit`, `T` is `Copy` (nothing to drop), and the length is never
/// changed. O(capacity / rows per page).
fn prefault<T: Copy>(rows: &mut Vec<T>, filler: T) {
    const PAGE_BYTES: usize = 4_096;
    let stride = (PAGE_BYTES / std::mem::size_of::<T>().max(1)).max(1);
    let spare = rows.spare_capacity_mut();
    let mut i = 0;
    while let Some(slot) = spare.get_mut(i) {
        slot.write(filler);
        i += stride;
    }
    if let Some(last) = spare.last_mut() {
        last.write(filler);
    }
    std::hint::black_box(rows);
}

/// `source` on the encode-failure line.
pub const TICK_STAGE_ENCODE_FAILED_SOURCE: &str = "stage_encode_failed";

/// Rows the encoder refused, process-wide (drives the log throttle only).
static ENCODE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// The encode-failure backstop, in the order the drain used to run it: the
/// frame is marked unapplied, the alarmed drop counter and the ledger's
/// not-landed total rise by one, then a coded line at powers of two. Runs on
/// the encoding thread BEFORE that batch is acknowledged or its rescue floor
/// released. O(1).
fn note_encode_failure(feed: Feed, capture_seq: i64, err: &anyhow::Error) {
    crate::wal_applied_watermark::applied_watermark()
        .note_unapplied(u64::try_from(capture_seq).unwrap_or(0));
    metrics::counter!("tv_ticks_dropped_total", "feed" => feed.as_str()).increment(1);
    if feed == Feed::Dhan {
        crate::tick_conservation_ledger::ledger().note_rows_not_landed(1);
    }
    let failures = ENCODE_FAILURES
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if failures.is_power_of_two() {
        error!(
            code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
            feed = feed.as_str(),
            capture_seq,
            source = TICK_STAGE_ENCODE_FAILED_SOURCE,
            failures_so_far = failures,
            error = %err,
            "a staged tick row could not be written as ILP text — LOST from this batch, \
             counted on tv_ticks_dropped_total and the frame marked unapplied so the next \
             replay re-offers it. Logged at powers of two; failures_so_far is the exact count."
        );
    }
}

/// The current number of encode failures, process-wide (tests).
#[cfg(test)]
pub(crate) fn encode_failures_so_far() -> u64 {
    ENCODE_FAILURES.load(Ordering::Relaxed)
}

#[cfg(test)]
#[path = "tick_stage_tests.rs"]
mod tick_stage_tests;
