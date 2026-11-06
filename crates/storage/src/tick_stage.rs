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
use crate::feed_aux_persistence::AuxPacketRow;
use crate::tick_ilp::{TickIlpEncoder, TickRowBuffer};
use crate::tick_persistence::{
    MAX_PRODUCER_BUFFER_BYTES, QUESTDB_MAX_BUF_SIZE_BYTES, TICKS_TABLE, TickRow,
};

/// Distinct contract-name tables one batch may pin. Names are published a few
/// times a day, so a batch normally pins one; the 09:00-09:15 attach can
/// republish while a batch is held across a full queue.
pub const TICK_STAGE_LABEL_PINS: usize = 8;

/// One staged row: exactly what the old text step consumed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum StagedRow {
    /// A `ticks` row.
    Tick {
        /// The row, as the drain built and gated it.
        row: TickRow,
        /// Index into the batch's name pins.
        pin: u8,
    },
    /// A `tick_quotes` row: `row.ts_ist_nanos` is already the arrival time.
    Quote {
        /// The row, its designated `ts` replaced by the arrival time.
        row: TickRow,
        /// The vendor's last trade time (IST nanos), `None` for a never-traded 0.
        exchange_ts: Option<i64>,
        /// Index into the batch's name pins.
        pin: u8,
    },
    /// A `feed_aux_packets` row.
    Aux {
        /// The row, as the drain built it.
        row: AuxPacketRow,
        /// Index into the batch's name pins.
        pin: u8,
    },
    /// A row its encoder refused once (already counted); skipped by every
    /// later encode of the batch.
    Refused {
        /// The refused row's capture sequence.
        capture_seq: i64,
    },
}

impl StagedRow {
    /// The row's capture sequence.
    pub(crate) const fn capture_seq(&self) -> i64 {
        match self {
            Self::Tick { row, .. } | Self::Quote { row, .. } => row.capture_seq,
            Self::Aux { row, .. } => row.capture_seq,
            Self::Refused { capture_seq } => *capture_seq,
        }
    }
}

/// RAM one staged row holds.
pub const STAGED_ROW_BYTES: usize = std::mem::size_of::<StagedRow>();

const _: () = assert!(
    STAGED_ROW_BYTES <= 320,
    "a staged row must stay a small Copy value"
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
    pins: [Option<Arc<CandleContractLabels>>; TICK_STAGE_LABEL_PINS],
    pin_count: u8,
    /// Rows an encode marked `Refused` (each already counted by the encoder).
    refused: usize,
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
            pins: [const { None }; TICK_STAGE_LABEL_PINS],
            pin_count: 0,
            refused: 0,
        }
    }

    /// An empty batch reserved for [`TICK_STAGED_ROWS_PRESIZE`] rows, every
    /// page touched once. One allocation; cold (construction and the split).
    #[must_use]
    pub(crate) fn presized() -> Self {
        let mut batch = Self::new();
        batch.reserve_presize();
        batch
    }

    /// Reserves to the pre-size and touches the new pages, when smaller. One
    /// allocation at most; a batch already that big costs one compare.
    pub(crate) fn reserve_presize(&mut self) {
        if self.rows.capacity() < TICK_STAGED_ROWS_PRESIZE {
            let have = self.rows.len();
            self.rows
                .reserve_exact(TICK_STAGED_ROWS_PRESIZE.saturating_sub(have));
            prefault(&mut self.rows);
        }
    }

    /// Rows staged (refused ones included).
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether nothing is staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
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
        self.pin_count += 1;
        Ok(self.pin_count - 1)
    }

    /// Pushes one row. Returns `true` when the batch had to grow (past its
    /// pre-size: one reallocation, cold, counted by the caller). Never drops.
    pub(crate) fn push(&mut self, row: StagedRow) -> bool {
        let grew = self.rows.len() == self.rows.capacity();
        self.rows.push(row);
        grew
    }

    /// Empties the batch and releases its pins, keeping the capacity.
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.pins = [const { None }; TICK_STAGE_LABEL_PINS];
        self.pin_count = 0;
        self.refused = 0;
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
        self.refused = 0;
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

    /// An empty batch with room for exactly `rows` rows (tests).
    #[cfg(test)]
    pub(crate) fn with_capacity_for_test(rows: usize) -> Self {
        let mut batch = Self::new();
        batch.rows.reserve_exact(rows);
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
        let mut encoded = 0;
        for i in 0..self.rows.len() {
            let Some(row) = self.rows.get(i).copied() else {
                continue;
            };
            match encode_row(&row, self, feed.as_str(), out) {
                Ok(true) => encoded += 1,
                Ok(false) => {}
                Err(err) => {
                    note_encode_failure(feed, row.capture_seq(), &err);
                    if let Some(slot) = self.rows.get_mut(i) {
                        *slot = StagedRow::Refused {
                            capture_seq: row.capture_seq(),
                        };
                        self.refused += 1;
                    }
                }
            }
        }
        encoded
    }

    /// [`Self::encode_into`] for a batch encoded for the LAST time (the rescue
    /// thread's spill): a refused row is counted exactly as there, but the
    /// batch is not marked, since it is dropped right after. O(row bytes) per
    /// row.
    pub(crate) fn encode_counted(&self, feed: Feed, out: &mut TickRowBuffer) -> usize {
        let mut encoded = 0;
        for row in &self.rows {
            match encode_row(row, self, feed.as_str(), out) {
                Ok(true) => encoded += 1,
                Ok(false) => {}
                Err(err) => note_encode_failure(feed, row.capture_seq(), &err),
            }
        }
        encoded
    }

    /// The batch's ILP text in `encoder`, without touching the batch or any
    /// counter; a refused row is left out. Tests and diagnostics only:
    /// allocates the returned text. O(rows).
    #[must_use]
    pub(crate) fn encode_readonly(&self, feed: Feed, encoder: TickIlpEncoder) -> Vec<u8> {
        let mut out = TickRowBuffer::new(encoder);
        for row in &self.rows {
            let _skipped_if_refused = encode_row(row, self, feed.as_str(), &mut out);
        }
        out.as_bytes().to_vec()
    }
}

/// Writes one row through the write function the old text step used, inside
/// a marker (a refused row leaves `out` as it was). `Ok(false)` for a row
/// already refused.
fn encode_row(
    row: &StagedRow,
    batch: &StagedBatch,
    feed: &'static str,
    out: &mut TickRowBuffer,
) -> Result<bool> {
    match row {
        StagedRow::Tick { row, pin } => {
            let labels = batch.labels(*pin);
            crate::tick_persistence::append_within_marker(out, |b| {
                crate::tick_persistence::write_tick_line_with_labels(
                    b,
                    TICKS_TABLE,
                    feed,
                    row,
                    None,
                    labels,
                )
            })?;
        }
        StagedRow::Quote {
            row,
            exchange_ts,
            pin,
        } => {
            let labels = batch.labels(*pin);
            crate::tick_persistence::append_within_marker(out, |b| {
                crate::tick_persistence::write_tick_line_with_labels(
                    b,
                    crate::tick_quotes_persistence::TICK_QUOTES_TABLE,
                    feed,
                    row,
                    *exchange_ts,
                    labels,
                )
            })?;
        }
        StagedRow::Aux { row, pin } => {
            let labels = batch.labels(*pin);
            crate::tick_persistence::append_within_marker(out, |b| match b {
                TickRowBuffer::Library(x) => {
                    crate::feed_aux_persistence::append_aux_row_with_labels(x, feed, row, labels)
                }
                TickRowBuffer::Own(x) => {
                    crate::feed_aux_persistence::append_aux_row_with_labels(x, feed, row, labels)
                }
            })?;
        }
        StagedRow::Refused { .. } => return Ok(false),
    }
    Ok(true)
}

/// Touches each page of `rows`' spare capacity once, so the first fill takes
/// no page faults inside the frames that stage. Safe: writes go through
/// `MaybeUninit`, `StagedRow` is `Copy` (nothing to drop), and the length is
/// never changed. O(capacity / rows per page).
fn prefault(rows: &mut Vec<StagedRow>) {
    const PAGE_BYTES: usize = 4_096;
    let stride = (PAGE_BYTES / STAGED_ROW_BYTES).max(1);
    let filler = StagedRow::Refused { capture_seq: 0 };
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
