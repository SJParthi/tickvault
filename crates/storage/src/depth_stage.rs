//! The depth book writer's STAGED rows (plan item PF-4b, 2026-11-06).
//!
//! The depth twin of [`crate::tick_stage`]. Until this change the frame
//! drain wrote every `market_depth_book` row as ILP v2 into the depth
//! writer's buffer: about 400 bytes per side, two sides per Full packet, a
//! MEASURED 402 ns per row on the x86 dev container (`depth_row_append/
//! book_row_5` median), so about 0.8 µs per Full tick on the task that
//! empties the sockets. Now the drain keeps the row it already builds (every
//! gate, refusal and counter unchanged and still on the drain) and copies its
//! identity columns into one fixed [`StagedBookRow`] and its level values
//! into a pre-sized, pre-touched value arena. The ILP text is written later,
//! by the thread that needs it, with the UNCHANGED line writer
//! ([`crate::depth_persistence::write_book_line_into`]):
//!
//! * the depth writer thread (`DepthWriterSink::write`), before the POST;
//! * the depth rescue thread (`DepthRescueSink::rescue`), before the spill;
//! * the drain's cold paths that already block (the inline spill, the parked
//!   batches written at shutdown and the unsplit synchronous flush), inside
//!   their existing `off_worker` closures.
//!
//! The `market_depth` LEVEL writer is unchanged: its rows are still written
//! as text on the drain (it is off with the depth sockets, and the inline
//! book rows are the production shape since 2026-10-10).
//!
//! # Byte identity: contract-name pins
//!
//! The line writer adds the contract name, which the old drain resolved ONCE
//! per run of rows for one instrument (cached on `(security_id, segment)`,
//! cleared at every flush). Written later, a row could read a NEWER name
//! table. So the drain keeps the same cache, but it records which TABLE the
//! run's name came from: each batch PINS every table its rows saw (at most
//! [`DEPTH_STAGE_LABEL_PINS`]), and each row carries its pin. The encoder
//! reads the name from that pinned table, which is the value the old cache
//! held. A batch that would see a ninth table is handed off early (the pin
//! cut), never dropped. Pins are released where the batch is cleared: on the
//! writer or rescue thread for every normal batch.
//!
//! # Exact text size
//!
//! The drain still needs the batch's TEXT size: the book writer flushes at
//! `DEPTH_BOOK_FLUSH_BYTES`, the producer cuts at
//! `MAX_DEPTH_PRODUCER_BUFFER_BYTES`, and both were measured on the buffer.
//! [`book_line_len`] computes each line's exact byte length from the row
//! (pinned by a proptest against the real encoder), so every cut lands
//! exactly where it did before.
//!
//! # A row the encoder refuses
//!
//! The encoder refuses only a negative designated timestamp, which the
//! drain's session-window gate already refuses, so this is believed
//! unreachable. The thread that encodes counts it exactly as the drain did:
//! the frame is marked unapplied, `tv_depth_rows_dropped_total` rises by the
//! row's levels, and one coded line is logged at powers of two
//! (`source = "depth_stage_encode_failed"`), all BEFORE the batch's ack or
//! rescue floor release. The row is then marked refused in the batch, so a
//! second encode of the same batch cannot count it twice.
//!
//! # Complexity
//!
//! Staging: O(levels) per side (the value copy), no allocation inside the
//! pre-size. Encoding: O(line bytes) per row on the thread that encodes.
//! SPACE per batch: [`DEPTH_STAGED_ROWS_PRESIZE`] × [`STAGED_BOOK_ROW_BYTES`]
//! plus [`DEPTH_STAGED_VALUES_PRESIZE`] × 8 bytes (about 6.2 MiB, where the
//! text buffer it replaces was reserved at 9 MiB).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use arc_swap::Guard;
use questdb::ingress::Buffer;
use tracing::error;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

use crate::candle_contract_labels::CandleContractLabels;
use crate::depth_persistence::{
    DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES, DepthBookRow, MARKET_DEPTH_BOOK_TABLE,
};

/// Distinct contract-name tables one batch may pin.
pub const DEPTH_STAGE_LABEL_PINS: usize = 8;

/// One staged `market_depth_book` row: its identity columns and where its
/// level values sit in the batch's arena.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StagedBookRow {
    pub(crate) security_id: i64,
    pub(crate) capture_seq: i64,
    pub(crate) ts_nanos: i64,
    pub(crate) segment: &'static str,
    pub(crate) depth_kind: &'static str,
    pub(crate) side: &'static str,
    /// First value of this row in the arena: `levels` prices, then
    /// `levels` quantities, then `levels` orders.
    values_at: u32,
    levels: u16,
    /// Index into the batch's name pins.
    pin: u8,
    /// Refused once by its encoder (already counted); skipped afterwards.
    refused: bool,
}

/// RAM one staged row holds.
pub const STAGED_BOOK_ROW_BYTES: usize = std::mem::size_of::<StagedBookRow>();

const _: () = assert!(
    STAGED_BOOK_ROW_BYTES <= 96,
    "a staged book row must stay a small Copy value"
);

/// The shortest inline book line in production text: a five-level side with
/// no contract name is about 370 bytes. Rows are reserved at a quarter of a
/// kilobyte of text, so a full batch of inline rows never grows them.
const PRESIZE_TEXT_BYTES_PER_ROW: usize = 256;

/// Rows every staged batch is reserved (and pre-touched) for: the old text
/// pre-size (`DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES`, 9 MiB) at 256 bytes a
/// row, 36,864 rows.
pub const DEPTH_STAGED_ROWS_PRESIZE: usize =
    DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES / PRESIZE_TEXT_BYTES_PER_ROW;

/// Level values every staged batch is reserved (and pre-touched) for: 3/8 of
/// the text pre-size in 8-byte values. An inline five-level row stores 15
/// values (120 bytes) for about 370 to 400 bytes of text (under 0.33), so a
/// full batch of inline rows never grows the arena; a depth-200 row stores
/// 600 values for about 5,000 bytes of text (0.96) and a burst of those grows
/// it, counted on `tv_depth_stage_grow_total`, as the text buffer grew before.
pub const DEPTH_STAGED_VALUES_PRESIZE: usize = DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES * 3 / 8 / 8;

/// A batch of staged book rows, their level values, and the name tables they
/// saw.
#[derive(Debug)]
pub struct DepthStagedBatch {
    rows: Vec<StagedBookRow>,
    values: Vec<f64>,
    pins: [Option<Arc<CandleContractLabels>>; DEPTH_STAGE_LABEL_PINS],
    pin_count: u8,
    /// Exact ILP text bytes of every staged row (see [`book_line_len`]).
    text_bytes: usize,
    /// Levels staged, counted as the writer's `pending` counts them (a side
    /// with no levels counts one).
    levels_total: usize,
    /// Levels of rows an encode refused and already counted.
    refused_levels: usize,
}

impl Default for DepthStagedBatch {
    fn default() -> Self {
        Self::new()
    }
}

/// The table a row with no valid pin reads: unreachable (every staged row's
/// pin is set before the row is pushed); empty, so such a row would carry no
/// name rather than a guessed one.
static NO_LABELS: LazyLock<CandleContractLabels> = LazyLock::new(CandleContractLabels::default);

impl DepthStagedBatch {
    /// An empty batch. Allocates nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows: Vec::new(),
            values: Vec::new(),
            pins: [const { None }; DEPTH_STAGE_LABEL_PINS],
            pin_count: 0,
            text_bytes: 0,
            levels_total: 0,
            refused_levels: 0,
        }
    }

    /// An empty batch reserved to the pre-size, every page touched once. Two
    /// allocations; cold (the split, and a spare-less hand-off).
    #[must_use]
    pub(crate) fn presized() -> Self {
        let mut batch = Self::new();
        batch.reserve_presize();
        batch
    }

    /// The producer's batch for a writer of `layout`: pre-sized for a book
    /// writer, empty (no allocation) for a level writer, which never stages.
    #[must_use]
    pub(crate) fn for_layout(layout: crate::depth_persistence::DepthLayout) -> Self {
        match layout {
            crate::depth_persistence::DepthLayout::Book => Self::presized(),
            crate::depth_persistence::DepthLayout::Levels => Self::new(),
        }
    }

    /// Reserves to the pre-size and touches the new pages, when smaller.
    pub(crate) fn reserve_presize(&mut self) {
        if self.rows.capacity() < DEPTH_STAGED_ROWS_PRESIZE {
            let have = self.rows.len();
            self.rows
                .reserve_exact(DEPTH_STAGED_ROWS_PRESIZE.saturating_sub(have));
            prefault(&mut self.rows, StagedBookRow::FILLER);
        }
        if self.values.capacity() < DEPTH_STAGED_VALUES_PRESIZE {
            let have = self.values.len();
            self.values
                .reserve_exact(DEPTH_STAGED_VALUES_PRESIZE.saturating_sub(have));
            prefault(&mut self.values, 0.0);
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

    /// Exact ILP text bytes the staged rows encode to.
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        self.text_bytes
    }

    /// RAM the batch holds, by CAPACITY (what a parked batch pins).
    #[must_use]
    pub fn ram_bytes(&self) -> usize {
        self.rows
            .capacity()
            .saturating_mul(STAGED_BOOK_ROW_BYTES)
            .saturating_add(self.values.capacity().saturating_mul(8))
    }

    /// Levels of rows an encode refused and already counted.
    #[must_use]
    pub(crate) fn refused_levels(&self) -> usize {
        self.refused_levels
    }

    /// Levels staged, in the writer's `pending` unit.
    #[must_use]
    pub(crate) fn pending_levels(&self) -> usize {
        self.levels_total
    }

    /// A clone of the table pinned at `pin` (one `Arc` increment), `None`
    /// for an unused slot.
    pub(crate) fn pinned_table(&self, pin: u8) -> Option<Arc<CandleContractLabels>> {
        self.pins.get(usize::from(pin)).and_then(Option::clone)
    }

    /// Pins an already-held table (the carried run table after a hand-off).
    /// Same rules as [`Self::try_pin`]; the table is handed back when every
    /// slot holds another table. O(1).
    pub(crate) fn pin_table(
        &mut self,
        table: Arc<CandleContractLabels>,
    ) -> std::result::Result<u8, Arc<CandleContractLabels>> {
        if let Some(last) = self
            .pin_count
            .checked_sub(1)
            .and_then(|i| self.pins.get(usize::from(i)))
            .and_then(Option::as_ref)
            && Arc::ptr_eq(last, &table)
        {
            return Ok(self.pin_count - 1);
        }
        let Some(slot) = self.pins.get_mut(usize::from(self.pin_count)) else {
            return Err(table);
        };
        *slot = Some(table);
        self.pin_count += 1;
        Ok(self.pin_count - 1)
    }

    /// The pin index for `labels` (a fresh table load), or the load handed
    /// back when all [`DEPTH_STAGE_LABEL_PINS`] slots hold other tables.
    /// O(1): one pointer compare; on a new table one `Arc` increment.
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

    /// Stages one side whose three slices the caller has already checked
    /// (equal length, at most `MAX_DEPTH_BOOK_LEVELS`). `text_len` is the
    /// row's [`book_line_len`]. Returns `true` when the batch had to grow
    /// (past its pre-size: a reallocation, counted by the caller). Never
    /// drops. O(levels).
    pub(crate) fn push(&mut self, row: &DepthBookRow<'_>, pin: u8, text_len: usize) -> bool {
        let levels = row.prices.len();
        let grew = self.rows.len() == self.rows.capacity()
            || self.values.len().saturating_add(3 * levels) > self.values.capacity();
        let values_at = u32::try_from(self.values.len()).unwrap_or(u32::MAX);
        self.values.extend_from_slice(row.prices);
        self.values.extend_from_slice(row.quantities);
        self.values.extend_from_slice(row.orders);
        self.rows.push(StagedBookRow {
            security_id: row.security_id,
            capture_seq: row.capture_seq,
            ts_nanos: row.ts_nanos,
            segment: row.segment,
            depth_kind: row.depth_kind,
            side: row.side,
            values_at,
            levels: u16::try_from(levels).unwrap_or(u16::MAX),
            pin,
            refused: false,
        });
        self.text_bytes = self.text_bytes.saturating_add(text_len);
        self.levels_total = self.levels_total.saturating_add(levels.max(1));
        grew
    }

    /// Empties the batch and releases its pins, keeping the capacity. For
    /// the writer and rescue threads (dropping a pin can free a whole name
    /// table, which must not happen on the drain).
    pub(crate) fn clear(&mut self) {
        self.clear_rows();
        self.pins = [const { None }; DEPTH_STAGE_LABEL_PINS];
        self.pin_count = 0;
    }

    /// Empties the rows but KEEPS the name pins and the capacity: the clear
    /// a split writer uses on the drain (see [`crate::tick_stage`]). O(1).
    pub(crate) fn clear_rows(&mut self) {
        self.rows.clear();
        self.values.clear();
        self.text_bytes = 0;
        self.levels_total = 0;
        self.refused_levels = 0;
    }

    /// Name tables this batch pins (tests).
    #[cfg(test)]
    pub(crate) fn pin_count(&self) -> usize {
        usize::from(self.pin_count)
    }

    /// The rows' allocation, to prove a batch was reused (tests).
    #[cfg(test)]
    pub(crate) fn as_ptr_for_test(&self) -> *const StagedBookRow {
        self.rows.as_ptr()
    }

    /// The name table pinned at `pin`.
    fn labels(&self, pin: u8) -> &CandleContractLabels {
        self.pins
            .get(usize::from(pin))
            .and_then(Option::as_deref)
            .unwrap_or(&NO_LABELS)
    }

    /// The row's three value slices.
    fn slices(&self, row: &StagedBookRow) -> (&[f64], &[f64], &[f64]) {
        let n = usize::from(row.levels);
        let at = row.values_at as usize;
        let all = self.values.get(at..at.saturating_add(3 * n)).unwrap_or(&[]);
        if all.len() != 3 * n {
            return (&[], &[], &[]);
        }
        let (prices, rest) = all.split_at(n);
        let (quantities, orders) = rest.split_at(n);
        (prices, quantities, orders)
    }

    /// Encodes every staged row, in order, into `out` through the unchanged
    /// line writer; returns the rows written. A refused row is counted on
    /// THIS thread (see the module docs) and marked. O(line bytes) per row;
    /// allocation-free while `out` has room.
    pub(crate) fn encode_into(&mut self, feed: Feed, out: &mut Buffer) -> usize {
        let mut encoded = 0;
        for i in 0..self.rows.len() {
            let Some(row) = self.rows.get(i).copied() else {
                continue;
            };
            if row.refused {
                continue;
            }
            match encode_row(&row, self, feed, out) {
                Ok(()) => encoded += 1,
                Err(err) => {
                    note_encode_failure(feed, &row, &err);
                    if let Some(slot) = self.rows.get_mut(i) {
                        slot.refused = true;
                    }
                    self.refused_levels = self
                        .refused_levels
                        .saturating_add(usize::from(row.levels).max(1));
                }
            }
        }
        encoded
    }

    /// [`Self::encode_into`] for a batch encoded for the LAST time (the
    /// rescue thread's spill, a parked batch at shutdown): a refused row is
    /// counted exactly as there, but the batch is not marked, since it is
    /// cleared right after. Returns the levels refused. O(line bytes) per row.
    pub(crate) fn encode_counted(&self, feed: Feed, out: &mut Buffer) -> usize {
        let mut refused = 0_usize;
        for row in &self.rows {
            if row.refused {
                continue;
            }
            if let Err(err) = encode_row(row, self, feed, out) {
                note_encode_failure(feed, row, &err);
                refused = refused.saturating_add(usize::from(row.levels).max(1));
            }
        }
        refused
    }

    /// The batch's ILP bytes, without touching the batch or any counter; a
    /// refused row is left out. Tests and diagnostics: allocates. O(rows).
    #[must_use]
    pub(crate) fn encode_readonly(&self, feed: Feed) -> Vec<u8> {
        let mut out = Buffer::new(crate::depth_persistence::DepthLayout::Book.protocol());
        for row in &self.rows {
            if !row.refused {
                let _skipped_if_refused = encode_row(row, self, feed, &mut out);
            }
        }
        out.as_bytes().to_vec()
    }
}

impl StagedBookRow {
    /// A value used only to touch pages; never read as a row.
    const FILLER: Self = Self {
        security_id: 0,
        capture_seq: 0,
        ts_nanos: 0,
        segment: "",
        depth_kind: "",
        side: "",
        values_at: 0,
        levels: 0,
        pin: 0,
        refused: true,
    };
}

/// Writes one row through the line writer the old drain used, inside a
/// marker, so a refused row leaves `out` exactly as it was.
fn encode_row(
    row: &StagedBookRow,
    batch: &DepthStagedBatch,
    feed: Feed,
    out: &mut Buffer,
) -> Result<()> {
    let (prices, quantities, orders) = batch.slices(row);
    let book = DepthBookRow {
        security_id: row.security_id,
        segment: row.segment,
        depth_kind: row.depth_kind,
        side: row.side,
        prices,
        quantities,
        orders,
        capture_seq: row.capture_seq,
        ts_nanos: row.ts_nanos,
    };
    let name = batch
        .labels(row.pin)
        .get(&(row.security_id, row.segment))
        .map(|n| &**n);
    out.set_marker()
        .map_err(|e| anyhow::anyhow!("marker: {e}"))?;
    if let Err(err) =
        crate::depth_persistence::write_book_line_into(out, feed, &book, prices.len(), name)
    {
        return Err(match out.rewind_to_marker() {
            Ok(()) => err,
            Err(rewind_err) => err.context(rewind_err),
        });
    }
    out.clear_marker();
    Ok(())
}

/// Decimal digits `itoa` prints for `v`, sign included. O(1).
const fn i64_text_len(v: i64) -> usize {
    let sign = if v < 0 { 1 } else { 0 };
    let magnitude = v.unsigned_abs();
    let digits = match magnitude.checked_ilog10() {
        Some(d) => d as usize + 1,
        None => 1,
    };
    sign + digits
}

/// Bytes a SYMBOL value takes once the ILP writer escapes it: one backslash
/// before each space, comma, equals sign, newline, carriage return or
/// backslash (the questdb-rs unquoted rule). A publish-sanitised contract
/// name can still hold a space or a backslash. O(name bytes), once per
/// instrument run on the drain (the old line writer scanned it per row).
#[must_use]
pub(crate) fn escaped_symbol_len(value: &str) -> usize {
    value.len()
        + value
            .bytes()
            .filter(|b| matches!(b, b' ' | b',' | b'=' | b'\n' | b'\r' | b'\\'))
            .count()
}

/// `,name=value` for a symbol whose name needs no escaping; `value_len` is
/// the value's escaped length.
const fn symbol_len(name: &str, value_len: usize) -> usize {
    1 + name.len() + 1 + value_len
}

/// `,name=` (or ` name=`) plus an integer and its `i` suffix.
const fn i64_column_len(name: &str, v: i64) -> usize {
    1 + name.len() + 1 + i64_text_len(v) + 1
}

/// `,name=` plus a one-dimensional binary f64 array of `n` values: `=`, the
/// entity type, the element type, the dimension count, one `u32` dimension,
/// then the values.
const fn array_column_len(name: &str, n: usize) -> usize {
    1 + name.len() + 1 + 4 + 4 + 8 * n
}

/// The exact byte length of one `market_depth_book` line as
/// [`crate::depth_persistence::write_book_line_into`] writes it (ILP v2),
/// for a row whose symbols are the closed, clean sets; `name_len` is the
/// contract name's [`escaped_symbol_len`]. O(1). Pinned against the real
/// encoder by `proptest_book_line_len_is_exact`.
#[must_use]
pub(crate) fn book_line_len(
    feed: Feed,
    row: &DepthBookRow<'_>,
    levels: usize,
    name_len: Option<usize>,
) -> usize {
    let mut len = MARKET_DEPTH_BOOK_TABLE.len()
        + symbol_len("segment", row.segment.len())
        + symbol_len("depth_kind", row.depth_kind.len())
        + symbol_len("side", row.side.len())
        + symbol_len("feed", feed.as_str().len());
    if let Some(n) = name_len {
        len += symbol_len("contract", n);
    }
    len + i64_column_len("security_id", row.security_id)
        + i64_column_len("levels", i64::try_from(levels).unwrap_or(i64::MAX))
        + array_column_len("price", row.prices.len())
        + array_column_len("quantity", row.quantities.len())
        + array_column_len("orders", row.orders.len())
        + i64_column_len("capture_seq", row.capture_seq)
        // ` <nanos>n\n`
        + 1
        + i64_text_len(row.ts_nanos)
        + 2
}

/// Touches each page of `v`'s spare capacity once, so the first fill takes
/// no page faults inside the frames that stage. Writes go through
/// `MaybeUninit` and the length is never changed. O(capacity / items per
/// page).
fn prefault<T: Copy>(v: &mut Vec<T>, filler: T) {
    const PAGE_BYTES: usize = 4_096;
    let stride = (PAGE_BYTES / std::mem::size_of::<T>().max(1)).max(1);
    let spare = v.spare_capacity_mut();
    let mut i = 0;
    while let Some(slot) = spare.get_mut(i) {
        slot.write(filler);
        i += stride;
    }
    if let Some(last) = spare.last_mut() {
        last.write(filler);
    }
    std::hint::black_box(v);
}

/// `source` on the encode-failure line.
pub const DEPTH_STAGE_ENCODE_FAILED_SOURCE: &str = "depth_stage_encode_failed";

/// Rows the encoder refused, process-wide (drives the log throttle only).
static ENCODE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// The encode-failure backstop, in the order the drain used to run it: the
/// frame is marked unapplied, the alarmed drop counter rises by the row's
/// levels, then a coded line at powers of two. Runs on the encoding thread
/// BEFORE that batch is acknowledged or its rescue floor released. O(1).
fn note_encode_failure(feed: Feed, row: &StagedBookRow, err: &anyhow::Error) {
    crate::wal_applied_watermark::applied_watermark()
        .note_unapplied(u64::try_from(row.capture_seq).unwrap_or(0));
    let levels = u64::from(row.levels).max(1);
    metrics::counter!("tv_depth_rows_dropped_total", "feed" => feed.as_str()).increment(levels);
    let failures = ENCODE_FAILURES
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if failures.is_power_of_two() {
        error!(
            code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
            feed = feed.as_str(),
            capture_seq = row.capture_seq,
            security_id = row.security_id,
            source = DEPTH_STAGE_ENCODE_FAILED_SOURCE,
            failures_so_far = failures,
            error = %err,
            "a staged market_depth_book row could not be written as ILP — LOST from this \
             batch, counted on tv_depth_rows_dropped_total and the frame marked unapplied \
             so the next replay re-offers it. Logged at powers of two; failures_so_far is \
             the exact count."
        );
    }
}

/// The current number of encode failures, process-wide (tests).
#[cfg(test)]
pub(crate) fn encode_failures_so_far() -> u64 {
    ENCODE_FAILURES.load(Ordering::Relaxed)
}

#[cfg(test)]
#[path = "depth_stage_tests.rs"]
mod depth_stage_tests;
