//! PF-4b oracle tests (plan item PF-4b, 2026-11-06).
//!
//! The pre-PF-4b book append is kept here as [`LegacyBook`]: one text buffer
//! the drain wrote each row into, with the per-run contract-name cache the
//! old `DepthWriter` held (resolved once per `(security_id, segment)` run,
//! cleared at every `flush()` call). Random streams of appends, name
//! publishes, flushes, direct discards, unbacked marks, unread queues and
//! shutdowns run through the STAGED writer and through the oracle side by
//! side; the bytes the writer, rescue and drain-inline paths produce must be
//! the oracle's bytes.
//!
//! * `proptest_pf4b_ordered_bytes_equal_the_legacy_append`: every queue is
//!   read after every operation, so the batches leave in row order and their
//!   concatenation must equal the oracle's text byte for byte.
//! * `proptest_pf4b_stress_lines_equal_the_legacy_append`: the writer queue
//!   is left unread for random stretches (retained spans, width cuts and
//!   rescues) and the stream ends with the lane's shutdown sequence. Batches
//!   can then leave out of row order, so the comparison is the MULTISET of
//!   lines (each line split by a small ILP v2 reader). Whether a random case
//!   also reaches a full rescue queue, a park or an inline spill is NOT
//!   asserted; those arms are pinned by the deterministic tests below
//!   (`test_parked_staged_batch_spills_the_legacy_bytes_at_shutdown`,
//!   `test_inline_spill_of_staged_rows_writes_the_legacy_bytes`). Line ORDER
//!   inside a batch is the ordered test's and the golden fixtures' job.

use super::depth_stage_golden_tests::Rng;
use super::*;
use crate::candle_contract_labels::{
    CandleContractLabels, TEST_PUBLISH_LOCK, publish_candle_contract_labels,
};
use proptest::prelude::*;
use std::sync::Arc;

const ID_BASE: i64 = 7_950_000_000;
const IDS: i64 = 12;
const SESSION_TS_NANOS: i64 = 1_793_268_000_000_000_000;
const SEGMENTS: [&str; 3] = ["NSE_FNO", "NSE_EQ", "IDX_I"];
const NAMES: [&str; 5] = [
    "NIFTY-25Nov2026-24500-CE",
    "BAD,NAME=X",
    "RELIANCE",
    "ÄÖÜ-27Nov2026-100-CE",
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ-0123456789-ABCDEFGHIJKLMNOPQRSTUVWXYZ",
];

/// The pre-PF-4b drain-side append, kept as the oracle.
struct LegacyBook {
    buffer: Buffer,
    cache: Option<(i64, &'static str, Option<Arc<str>>)>,
    feed: Feed,
    levels: usize,
}

impl LegacyBook {
    fn new() -> Self {
        Self {
            buffer: Buffer::new(DepthLayout::Book.protocol()),
            cache: None,
            feed: Feed::Dhan,
            levels: 0,
        }
    }

    /// `DepthWriter::append_book_row` as it was: window gate, layout and
    /// length checks, the per-run name cache, one marked line.
    fn append(&mut self, row: &DepthBookRow<'_>) {
        if depth_arrival_window_refusal(row.ts_nanos).is_some() {
            return;
        }
        let levels = row.prices.len();
        if levels > MAX_DEPTH_BOOK_LEVELS
            || row.quantities.len() != levels
            || row.orders.len() != levels
        {
            return;
        }
        let hit = matches!(
            &self.cache,
            Some((id, seg, _)) if *id == row.security_id && *seg == row.segment
        );
        if !hit {
            let labels = crate::candle_contract_labels::candle_contract_labels();
            let name = labels
                .get(&(row.security_id, row.segment))
                .map(std::sync::Arc::clone);
            self.cache = Some((row.security_id, row.segment, name));
        }
        let name = match &self.cache {
            Some((_, _, Some(n))) => Some(Arc::clone(n)),
            _ => None,
        };
        self.buffer.set_marker().expect("marker");
        match write_book_line_into(&mut self.buffer, self.feed, row, levels, name.as_deref()) {
            Ok(()) => {
                self.buffer.clear_marker();
                self.levels += levels.max(1);
            }
            Err(_) => {
                self.buffer.rewind_to_marker().expect("rewind");
            }
        }
    }

    /// What the old `flush()` did to the cache.
    fn flush_called(&mut self) {
        self.cache = None;
    }
}

/// One operation of a stream.
#[derive(Debug, Clone)]
enum Op {
    /// A side (or a five-level bid/ask pair) appended.
    Append(u64),
    Publish(u64),
    Flush,
    Discard,
    MarkUnbacked,
    /// Leave the writer queue unread for this many operations.
    Stall(u8),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        12 => any::<u64>().prop_map(Op::Append),
        2 => any::<u64>().prop_map(Op::Publish),
        3 => Just(Op::Flush),
        1 => Just(Op::Discard),
        1 => Just(Op::MarkUnbacked),
        1 => (1u8..12).prop_map(Op::Stall),
    ]
}

fn publish(seed: u64) {
    let mut rng = Rng(seed | 1);
    let mut labels = CandleContractLabels::default();
    for i in 0..IDS {
        if rng.below(3) != 0 {
            labels.insert(
                (ID_BASE + i, SEGMENTS[rng.below(3) as usize]),
                Arc::from(NAMES[rng.below(NAMES.len() as u64) as usize]),
            );
        }
    }
    publish_candle_contract_labels(labels);
}

/// Appends one generated side (or bid/ask pair) to both writers.
fn append_both(seed: u64, counter: &mut i64, w: &mut DepthWriter, legacy: &mut LegacyBook) {
    let mut rng = Rng(seed | 1);
    let kind = match rng.below(12) {
        0 => DEPTH_KIND_20,
        1 => DEPTH_KIND_200,
        _ => DEPTH_KIND_5,
    };
    // Few instruments, so runs of the same instrument are common.
    let id = ID_BASE + rng.below(IDS as u64) as i64;
    let segment = SEGMENTS[(id % 3) as usize];
    *counter += 1 + rng.below(2) as i64;
    let seq = if rng.below(20) == 0 { 0 } else { *counter };
    let ts = match rng.below(25) {
        0 => SESSION_TS_NANOS - 3 * 3_600 * 1_000_000_000,
        1 => -5, // the encoder's only refusal, refused by the window first
        _ => SESSION_TS_NANOS + rng.below(1_000_000_000) as i64,
    };
    let sides: &[&'static str] = if kind == DEPTH_KIND_5 {
        &[DEPTH_SIDE_BID, DEPTH_SIDE_ASK]
    } else {
        &[DEPTH_SIDE_ASK]
    };
    let mut prices = [0.0_f64; MAX_DEPTH_BOOK_LEVELS + 1];
    let mut quantities = [0.0_f64; MAX_DEPTH_BOOK_LEVELS + 1];
    let mut orders = [0.0_f64; MAX_DEPTH_BOOK_LEVELS + 1];
    for side in sides {
        let n = match kind {
            DEPTH_KIND_5 => {
                if rng.below(10) == 0 {
                    rng.below(5) as usize
                } else {
                    5
                }
            }
            DEPTH_KIND_20 => 20,
            _ => match rng.below(8) {
                0 => MAX_DEPTH_BOOK_LEVELS + 1,
                1 => MAX_DEPTH_BOOK_LEVELS,
                _ => rng.below(30) as usize,
            },
        };
        let nq = if rng.below(30) == 0 { n + 1 } else { n };
        for i in 0..=MAX_DEPTH_BOOK_LEVELS {
            prices[i] = f64::from_bits(rng.next());
            quantities[i] = (rng.below(10_000)) as f64;
            orders[i] = if rng.below(9) == 0 {
                f64::NAN
            } else {
                rng.below(50) as f64
            };
        }
        let row = DepthBookRow {
            security_id: id,
            segment,
            depth_kind: kind,
            side,
            prices: &prices[..n],
            quantities: &quantities[..nq.min(MAX_DEPTH_BOOK_LEVELS + 1)],
            orders: &orders[..n],
            capture_seq: seq,
            ts_nanos: ts,
        };
        let _ = w.append_book_row(&row);
        legacy.append(&row);
    }
}

/// Splits ILP v2 `market_depth_book` text into its lines: symbols are clean
/// text up to the first space; a column is `key=` then either a text value
/// (up to `,` or ` `) or `=` + a binary f64 array (entity 14, element type,
/// dimension count, `u32` dimensions, 8-byte values); then ` <ts>n\n`.
pub(super) fn split_book_lines(mut bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    while !bytes.is_empty() {
        let mut i = bytes.iter().position(|b| *b == b' ').expect("symbols end") + 1;
        loop {
            let eq = i + bytes[i..].iter().position(|b| *b == b'=').expect("key") + 1;
            i = eq;
            if bytes[i] == b'=' {
                assert_eq!(bytes[i + 1], 14, "a binary array");
                let ndim = bytes[i + 3] as usize;
                let mut count = 1_usize;
                for d in 0..ndim {
                    let at = i + 4 + 4 * d;
                    count *=
                        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("dim")) as usize;
                }
                i += 4 + 4 * ndim + 8 * count;
            } else {
                while bytes[i] != b',' && bytes[i] != b' ' {
                    i += 1;
                }
            }
            match bytes[i] {
                b',' => i += 1,
                _ => break,
            }
        }
        let end = i
            + bytes[i..]
                .iter()
                .position(|b| *b == b'\n')
                .expect("line end")
            + 1;
        lines.push(bytes[..end].to_vec());
        bytes = &bytes[end..];
    }
    lines
}

/// Reads every framed record of the ILP v2 spill files in `dir`, oldest
/// file first.
pub(super) fn read_v2_spill_dir(dir: &Path) -> Vec<u8> {
    use crate::depth_spill_frame::{RecordAt, read_record_at};
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let file = std::fs::File::open(&path).expect("open spill");
        let len = file.metadata().expect("spill metadata").len();
        let mut offset = 0_u64;
        let mut scratch = Vec::new();
        while offset < len {
            let record = read_record_at(&file, offset, len, &mut scratch).expect("spill read");
            assert!(
                matches!(record, RecordAt::Whole { .. }),
                "record at {offset} of {path:?} is not whole: {record:?}"
            );
            if let RecordAt::Whole { version, next } = record {
                assert_eq!(version, crate::depth_spill_frame::ILP_VERSION_2);
                out.extend_from_slice(&scratch);
                offset = next;
            }
        }
    }
    out
}

fn flush_batch_bytes(batch: &DepthFlushBatch) -> Vec<u8> {
    let mut out = batch.buffer.as_bytes().to_vec();
    out.extend_from_slice(&batch.staged.encode_readonly(Feed::Dhan));
    out
}

/// Gives back a rescue batch's watermark floor, as the rescue thread would,
/// for a batch the test reads instead of spilling (the floor table is
/// process-wide and has 16 slots).
fn release_floor(batch: DepthRescueBatch) {
    if let Some(floor) = batch.floor {
        crate::wal_applied_watermark::applied_watermark().release_rescue_floor(floor);
    }
}

fn rescue_batch_bytes(batch: &DepthRescueBatch) -> Vec<u8> {
    let mut out = batch.buffer.as_bytes().to_vec();
    out.extend_from_slice(&batch.staged.encode_readonly(Feed::Dhan));
    out
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!(
        "tv-pf4b-{tag}-{nanos}-{:?}",
        std::thread::current().id()
    ))
}

/// Runs `ops` through a split, rescue-split book writer and the oracle.
/// `ordered`: every queue is read after every operation (no stalls). Returns
/// (the staged path's bytes in read order plus the inline spills, its level
/// total, the oracle's text, the oracle's level total).
fn run(ops: &[Op], ordered: bool) -> (Vec<u8>, usize, Vec<u8>, usize) {
    publish_candle_contract_labels(CandleContractLabels::default());
    let spill = temp_dir("oracle");
    let (mut w, sink, rx) = DepthWriter::for_test_book(Feed::Dhan)
        .with_spill_dir_for_test(spill.clone())
        .with_wal_defer_check_for_test(|_, _, _| false)
        .split_for_offload();
    drop(sink);
    let (rescue_sink, rescue_rx) = w.split_rescue_offload();
    drop(rescue_sink);
    let mut legacy = LegacyBook::new();
    let mut out = Vec::new();
    let mut levels = 0_usize;
    let mut counter = 10_000_i64;
    let mut stall = 0_u8;
    let read = |rx: &std::sync::mpsc::Receiver<DepthFlushBatch>,
                rescue_rx: &std::sync::mpsc::Receiver<DepthRescueBatch>,
                out: &mut Vec<u8>,
                levels: &mut usize,
                read_now: bool| {
        while read_now && let Ok(b) = rx.try_recv() {
            out.extend_from_slice(&flush_batch_bytes(&b));
            *levels += b.rows;
        }
        while read_now && let Ok(b) = rescue_rx.try_recv() {
            out.extend_from_slice(&rescue_batch_bytes(&b));
            *levels += b.rows;
            release_floor(b);
        }
    };
    for op in ops {
        match op {
            Op::Append(seed) => append_both(*seed, &mut counter, &mut w, &mut legacy),
            Op::Publish(seed) => publish(*seed),
            Op::Flush => {
                let _ = w.flush();
                legacy.flush_called();
            }
            Op::Discard => {
                let _ = w.discard_pending();
            }
            Op::MarkUnbacked => w.mark_pending_unbacked(),
            Op::Stall(n) => {
                if !ordered {
                    stall = *n;
                }
            }
        }
        let read_now = stall == 0;
        stall = stall.saturating_sub(1);
        read(&rx, &rescue_rx, &mut out, &mut levels, read_now);
    }
    // The lane's shutdown sequence: close the writer queue, rescue what the
    // producer still holds, then hand the park over; the rescue thread is
    // gone by then (so a parked batch is written inline, not waited on).
    read(&rx, &rescue_rx, &mut out, &mut levels, true);
    w.close_offload();
    let _ = w.discard_pending();
    read(&rx, &rescue_rx, &mut out, &mut levels, true);
    drop(rescue_rx);
    w.close_rescue_offload();
    // Rows the drain wrote inline (a full rescue queue, past the park).
    out.extend_from_slice(&read_v2_spill_dir(&spill));
    let _ = std::fs::remove_dir_all(&spill);
    publish_candle_contract_labels(CandleContractLabels::default());
    (
        out,
        levels,
        legacy.buffer.as_bytes().to_vec(),
        legacy.levels,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn proptest_pf4b_ordered_bytes_equal_the_legacy_append(
        ops in proptest::collection::vec(op_strategy(), 1..160)
    ) {
        let _g = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (got, got_levels, want, want_levels) = run(&ops, true);
        prop_assert_eq!(got_levels, want_levels);
        prop_assert!(got == want, "staged bytes differ from the legacy append");
    }

    #[test]
    fn proptest_pf4b_stress_lines_equal_the_legacy_append(
        ops in proptest::collection::vec(op_strategy(), 1..220)
    ) {
        let _g = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (got, _got_levels, want, _want_levels) = run(&ops, false);
        let mut got_lines = split_book_lines(&got);
        let mut want_lines = split_book_lines(&want);
        got_lines.sort();
        want_lines.sort();
        prop_assert_eq!(got_lines.len(), want_lines.len());
        prop_assert!(got_lines == want_lines, "a line was lost, added or changed");
    }
}

/// The splitter splits what the encoder writes, line for line (the stress
/// test's comparison would be vacuous if it merged lines).
#[test]
fn test_split_book_lines_counts_every_line() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut legacy = LegacyBook::new();
    let mut w = DepthWriter::for_test_book(Feed::Dhan);
    let mut counter = 1_i64;
    for seed in 1..200_u64 {
        append_both(seed * 0x9E37_79B9, &mut counter, &mut w, &mut legacy);
    }
    let lines = split_book_lines(legacy.buffer.as_bytes());
    assert!(lines.len() > 100, "{} lines", lines.len());
    assert_eq!(lines.concat(), legacy.buffer.as_bytes());
    assert_eq!(w.pending_ilp_bytes(), legacy.buffer.as_bytes());
}

/// A batch PARKED in RAM (the rescue queue full, rows with no capture
/// sequence) and written inline at shutdown spills the legacy bytes.
#[test]
fn test_parked_staged_batch_spills_the_legacy_bytes_at_shutdown() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish_candle_contract_labels(CandleContractLabels::default());
    let spill = temp_dir("park");
    let mut w = DepthWriter::for_test_book(Feed::Dhan)
        .with_spill_dir_for_test(spill.clone())
        .with_wal_defer_check_for_test(|_, _, _| false);
    let (rescue_sink, rescue_rx) = w.split_rescue_offload();
    let mut legacy = LegacyBook::new();
    let mut counter = 1_i64;
    let mut queued = Vec::new();
    // Three discards: two fill the rescue queue, the third parks (its rows
    // are marked unbacked, so the WAL cannot defer them).
    for round in 0..3_u64 {
        for seed in 0..6_u64 {
            append_both(round * 100 + seed + 7, &mut counter, &mut w, &mut legacy);
        }
        w.mark_pending_unbacked();
        assert!(w.discard_pending() > 0);
    }
    assert_eq!(w.parked_rescue.len(), 1, "the third batch must park");
    assert!(w.parked_rescue_bytes >= crate::depth_stage::DEPTH_STAGED_ROWS_PRESIZE);
    while let Ok(b) = rescue_rx.try_recv() {
        queued.extend_from_slice(&rescue_batch_bytes(&b));
        release_floor(b);
    }
    // The rescue thread is gone: the park is written on the drain.
    drop(rescue_rx);
    drop(rescue_sink);
    w.close_rescue_offload();
    let mut got = queued;
    got.extend_from_slice(&read_v2_spill_dir(&spill));
    let _ = std::fs::remove_dir_all(&spill);
    assert_eq!(got, legacy.buffer.as_bytes());
}

/// The drain's INLINE spill (the rescue queue full, rows the WAL holds but
/// the deferral check refuses, nothing parkable) writes the staged rows as
/// text first and spills exactly the legacy bytes; the pins stay with the
/// batch (never dropped on the drain).
#[test]
fn test_inline_spill_of_staged_rows_writes_the_legacy_bytes() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish(99);
    let spill = temp_dir("inline");
    let mut w = DepthWriter::for_test_book(Feed::Dhan)
        .with_spill_dir_for_test(spill.clone())
        .with_wal_defer_check_for_test(|_, _, _| false);
    let (_rescue_sink, rescue_rx) = w.split_rescue_offload();
    let mut legacy = LegacyBook::new();
    let prices = [24_500.25, 24_500.2, 24_500.15, 24_500.1, 24_500.05];
    let mut seq = 500_i64;
    let mut queued = Vec::new();
    // Every row carries a capture sequence and nothing is marked unbacked,
    // so a full rescue queue cannot park the third batch: it spills inline.
    for round in 0..3_i64 {
        for k in 0..6_i64 {
            for side in [DEPTH_SIDE_BID, DEPTH_SIDE_ASK] {
                seq += 1;
                let row = DepthBookRow {
                    security_id: ID_BASE + (round + k) % IDS,
                    segment: SEGMENTS[((ID_BASE + (round + k) % IDS) % 3) as usize],
                    depth_kind: DEPTH_KIND_5,
                    side,
                    prices: &prices,
                    quantities: &prices,
                    orders: &prices,
                    capture_seq: seq,
                    ts_nanos: SESSION_TS_NANOS + seq,
                };
                w.append_book_row(&row).expect("append");
                legacy.append(&row);
            }
        }
        assert!(!w.pending_unbacked());
        assert!(w.discard_pending() > 0);
    }
    assert!(w.parked_rescue.is_empty(), "nothing was parkable");
    assert!(w.staged.is_empty(), "the inline arm cleared the rows");
    assert!(w.staged.pin_count() > 0, "the drain kept the pins");
    while let Ok(b) = rescue_rx.try_recv() {
        queued.extend_from_slice(&rescue_batch_bytes(&b));
        release_floor(b);
    }
    let inline = read_v2_spill_dir(&spill);
    let _ = std::fs::remove_dir_all(&spill);
    publish_candle_contract_labels(CandleContractLabels::default());
    assert!(!inline.is_empty(), "the third batch was spilled inline");
    let mut got = queued;
    got.extend_from_slice(&inline);
    assert_eq!(got, legacy.buffer.as_bytes());
}

/// The rescue THREAD spills the legacy bytes, and the spent batch comes
/// back to the drain's spare lane cleared (pins dropped there).
#[test]
fn test_rescue_thread_spills_the_legacy_bytes_and_recycles_the_batch() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish(42);
    let spill = temp_dir("rescue-thread");
    let (mut w, sink, rx) = DepthWriter::for_test_book(Feed::Dhan)
        .with_spill_dir_for_test(spill.clone())
        .split_for_offload();
    drop(sink);
    drop(rx);
    let (rescue_sink, rescue_rx) = w.split_rescue_offload();
    let mut legacy = LegacyBook::new();
    let mut counter = 1_i64;
    for seed in 0..40_u64 {
        append_both(seed + 3, &mut counter, &mut w, &mut legacy);
    }
    let staged_ptr = w.staged.as_ptr_for_test();
    // The writer thread is gone: the flush rescues through the rescue queue.
    assert!(w.flush().is_err());
    let batch = rescue_rx.try_recv().expect("rescued");
    assert!(batch.staged.pin_count() > 0, "the batch carries its pins");
    rescue_sink.rescue(&batch);
    rescue_sink.recycle(batch);
    let spare = w
        .spare_staged
        .as_ref()
        .expect("lane")
        .try_recv()
        .expect("recycled");
    assert_eq!(
        spare.as_ptr_for_test(),
        staged_ptr,
        "the same allocation came back"
    );
    assert_eq!(spare.pin_count(), 0, "cleared on the rescue thread");
    assert!(spare.is_empty());
    let got = read_v2_spill_dir(&spill);
    let _ = std::fs::remove_dir_all(&spill);
    publish_candle_contract_labels(CandleContractLabels::default());
    assert_eq!(got, legacy.buffer.as_bytes());
}

/// The WRITER thread (no QuestDB here, so its own rescue arm) writes the
/// legacy bytes, and hands the staged batch back cleared.
#[test]
fn test_writer_thread_encodes_the_legacy_bytes_and_recycles_the_batch() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish(7);
    let spill = temp_dir("writer-thread");
    let (mut w, mut sink, rx) = DepthWriter::for_test_book(Feed::Dhan)
        .with_spill_dir_for_test(spill.clone())
        .split_for_offload();
    let mut legacy = LegacyBook::new();
    let mut counter = 1_i64;
    for seed in 0..40_u64 {
        append_both(seed + 11, &mut counter, &mut w, &mut legacy);
    }
    assert!(w.flush().is_ok());
    legacy.flush_called();
    let mut batch = rx.try_recv().expect("handed off");
    let rows = batch.rows();
    assert_eq!(rows, legacy.levels);
    assert_eq!(sink.write(&mut batch), 0, "no QuestDB: rescued, not landed");
    let spare = w
        .spare_staged
        .as_ref()
        .expect("lane")
        .try_recv()
        .expect("recycled");
    assert_eq!(spare.pin_count(), 0, "cleared on the writer thread");
    let got = read_v2_spill_dir(&spill);
    let _ = std::fs::remove_dir_all(&spill);
    publish_candle_contract_labels(CandleContractLabels::default());
    assert_eq!(got, legacy.buffer.as_bytes());
}

/// A republish in the MIDDLE of an instrument's run keeps the run's old
/// name, across a direct discard (the hand-off carries the run's table into
/// the next batch), exactly as the old cache did.
#[test]
fn test_run_name_survives_a_mid_run_republish_and_discard() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = ID_BASE + 1;
    let mut old = CandleContractLabels::default();
    old.insert((id, "NSE_EQ"), Arc::from("OLD-NAME"));
    publish_candle_contract_labels(old);
    let spill = temp_dir("carry");
    let mut w = DepthWriter::for_test_book(Feed::Dhan).with_spill_dir_for_test(spill.clone());
    let (_rescue_sink, rescue_rx) = w.split_rescue_offload();
    let mut legacy = LegacyBook::new();
    let prices = [100.0, 99.95];
    let row = |seq: i64| DepthBookRow {
        security_id: id,
        segment: "NSE_EQ",
        depth_kind: DEPTH_KIND_5,
        side: DEPTH_SIDE_BID,
        prices: &prices,
        quantities: &prices,
        orders: &prices,
        capture_seq: seq,
        ts_nanos: SESSION_TS_NANOS,
    };
    w.append_book_row(&row(1)).expect("append");
    legacy.append(&row(1));
    let mut new = CandleContractLabels::default();
    new.insert((id, "NSE_EQ"), Arc::from("NEW-NAME"));
    publish_candle_contract_labels(new);
    assert!(w.discard_pending() > 0);
    w.append_book_row(&row(2)).expect("append");
    legacy.append(&row(2));
    let first = rescue_rx.try_recv().expect("rescued");
    let mut got = rescue_batch_bytes(&first);
    release_floor(first);
    got.extend_from_slice(&w.pending_ilp_bytes());
    publish_candle_contract_labels(CandleContractLabels::default());
    let _ = std::fs::remove_dir_all(&spill);
    let text = String::from_utf8_lossy(&got);
    assert_eq!(text.matches("contract=OLD-NAME").count(), 2, "{text}");
    assert_eq!(got, legacy.buffer.as_bytes());
}

/// A ninth name table inside one batch cuts the batch early (counted) and
/// loses no byte.
#[test]
fn test_ninth_name_table_cuts_the_batch_and_keeps_every_byte() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut w, sink, rx) = DepthWriter::for_test_book(Feed::Dhan).split_for_offload();
    drop(sink);
    let mut legacy = LegacyBook::new();
    let prices = [100.0];
    let mut got = Vec::new();
    for t in 0..20_i64 {
        let mut labels = CandleContractLabels::default();
        labels.insert((ID_BASE + t, "NSE_FNO"), Arc::from("N"));
        publish_candle_contract_labels(labels);
        // A new instrument each time: a cache miss pins the new table.
        let row = DepthBookRow {
            security_id: ID_BASE + t,
            segment: "NSE_FNO",
            depth_kind: DEPTH_KIND_5,
            side: DEPTH_SIDE_ASK,
            prices: &prices,
            quantities: &prices,
            orders: &prices,
            capture_seq: 100 + t,
            ts_nanos: SESSION_TS_NANOS,
        };
        w.append_book_row(&row).expect("append");
        legacy.append(&row);
        assert!(w.staged.pin_count() <= crate::depth_stage::DEPTH_STAGE_LABEL_PINS);
        while let Ok(b) = rx.try_recv() {
            got.extend_from_slice(&flush_batch_bytes(&b));
        }
    }
    got.extend_from_slice(&w.pending_ilp_bytes());
    publish_candle_contract_labels(CandleContractLabels::default());
    assert!(!got.is_empty());
    assert_eq!(got, legacy.buffer.as_bytes());
}
