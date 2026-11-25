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

/// Appends one generated side (or bid/ask pair) to both writers. A
/// five-level pair goes through `append_book_pair` half the time (speed fix
/// 5), so every oracle stream also proves the pair append's bytes.
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
    // [side][value]; one set per side so a pair can borrow both at once.
    let mut prices = [[0.0_f64; MAX_DEPTH_BOOK_LEVELS + 1]; 2];
    let mut quantities = [[0.0_f64; MAX_DEPTH_BOOK_LEVELS + 1]; 2];
    let mut orders = [[0.0_f64; MAX_DEPTH_BOOK_LEVELS + 1]; 2];
    let mut lens = [(0_usize, 0_usize); 2];
    for (s, _side) in sides.iter().enumerate() {
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
            prices[s][i] = f64::from_bits(rng.next());
            quantities[s][i] = (rng.below(10_000)) as f64;
            orders[s][i] = if rng.below(9) == 0 {
                f64::NAN
            } else {
                rng.below(50) as f64
            };
        }
        lens[s] = (n, nq.min(MAX_DEPTH_BOOK_LEVELS + 1));
    }
    let row = |s: usize, side: &'static str| DepthBookRow {
        security_id: id,
        segment,
        depth_kind: kind,
        side,
        prices: &prices[s][..lens[s].0],
        quantities: &quantities[s][..lens[s].1],
        orders: &orders[s][..lens[s].0],
        capture_seq: seq,
        ts_nanos: ts,
    };
    if sides.len() == 2 && rng.below(2) == 0 {
        let (bid, ask) = (row(0, DEPTH_SIDE_BID), row(1, DEPTH_SIDE_ASK));
        let pair = DepthBookPair {
            security_id: id,
            segment,
            depth_kind: kind,
            bid_prices: bid.prices,
            bid_quantities: bid.quantities,
            bid_orders: bid.orders,
            ask_prices: ask.prices,
            ask_quantities: ask.quantities,
            ask_orders: ask.orders,
            capture_seq: seq,
            ts_nanos: ts,
        };
        let _ = w.append_book_pair(&pair);
        legacy.append(&bid);
        legacy.append(&ask);
        return;
    }
    for (s, side) in sides.iter().enumerate() {
        let r = row(s, side);
        let _ = w.append_book_row(&r);
        legacy.append(&r);
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

// ---- Behaviour of the moved line writer and the park size (pub-fn test guard, 2026-11-06) ----

fn guard_row<'a>(values: &'a [f64], ts_nanos: i64) -> DepthBookRow<'a> {
    DepthBookRow {
        security_id: 42,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_5,
        side: DEPTH_SIDE_BID,
        prices: values,
        quantities: values,
        orders: values,
        capture_seq: 7,
        ts_nanos,
    }
}

#[test]
fn test_write_book_line_into_writes_the_contract_only_when_named_and_refuses_a_negative_ts() {
    let values = [100.5, 101.0];
    let r = guard_row(&values, SESSION_TS_NANOS);
    let mut named = Buffer::new(DepthLayout::Book.protocol());
    write_book_line_into(&mut named, Feed::Dhan, &r, 2, Some("NIFTY-X")).expect("named");
    let mut bare = Buffer::new(DepthLayout::Book.protocol());
    write_book_line_into(&mut bare, Feed::Dhan, &r, 2, None).expect("bare");
    let prefix = format!("{MARKET_DEPTH_BOOK_TABLE},segment=NSE_FNO,depth_kind={DEPTH_KIND_5},");
    assert!(named.as_bytes().starts_with(prefix.as_bytes()));
    assert!(bare.as_bytes().starts_with(prefix.as_bytes()));
    assert!(named.as_bytes().ends_with(b"\n"), "one whole line");
    let contract = b",contract=NIFTY-X";
    assert!(
        named
            .as_bytes()
            .windows(contract.len())
            .any(|w| w == contract),
        "the name is written as the contract symbol"
    );
    assert!(
        !bare.as_bytes().windows(9).any(|w| w == b"contract="),
        "no name, no contract column (NULL)"
    );
    assert_eq!(named.len(), bare.len() + contract.len());
    let mut refused = Buffer::new(DepthLayout::Book.protocol());
    assert!(
        write_book_line_into(&mut refused, Feed::Dhan, &guard_row(&values, -1), 2, None).is_err(),
        "a negative designated timestamp is refused"
    );
}

#[test]
fn test_park_bytes_is_the_text_length_plus_the_staged_capacity() {
    let empty = DepthRescueBatch {
        buffer: Buffer::new(DepthLayout::Book.protocol()),
        staged: crate::depth_stage::DepthStagedBatch::new(),
        rows: 0,
        min_seq: 0,
        max_seq: 0,
        unbacked: false,
        floor: None,
    };
    assert_eq!(empty.park_bytes(), 0, "an empty batch pins nothing");
    let values = [1.0];
    let mut buffer = Buffer::new(DepthLayout::Book.protocol());
    write_book_line_into(
        &mut buffer,
        Feed::Dhan,
        &guard_row(&values, SESSION_TS_NANOS),
        1,
        None,
    )
    .expect("write");
    let text = buffer.len();
    assert!(text > 0);
    let staged = crate::depth_stage::DepthStagedBatch::with_capacity_for_test(4, 12);
    let staged_ram = staged.ram_bytes();
    assert!(staged_ram > 0, "a reserved batch holds RAM while empty");
    let batch = DepthRescueBatch {
        buffer,
        staged,
        rows: 1,
        min_seq: 7,
        max_seq: 7,
        unbacked: false,
        floor: None,
    };
    assert_eq!(
        batch.park_bytes(),
        text + staged_ram,
        "text by length, staged rows by capacity"
    );
}

/// PF-4b review round 2: the writer thread's book scratch never stays above
/// the pre-size. A batch whose text passes it grows the scratch for that one
/// write; the scratch is then dropped, and the next book batch reserves the
/// pre-size again.
#[test]
fn test_writer_sink_scratch_shrinks_back_after_a_batch_above_the_presize() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish_candle_contract_labels(CandleContractLabels::default());
    let spill = temp_dir("scratch");
    let (mut w, mut sink, rx) = DepthWriter::for_test_book(Feed::Dhan)
        .with_spill_dir_for_test(spill.clone())
        .with_wal_defer_check_for_test(|_, _, _| false)
        .split_for_offload();
    let values: Vec<f64> = (0..MAX_DEPTH_BOOK_LEVELS)
        .map(|i| 100.0 + i as f64)
        .collect();
    let mut seq = 1_i64;
    let mut append = |w: &mut DepthWriter, levels: usize| {
        seq += 1;
        w.append_book_row(&DepthBookRow {
            security_id: 52_000,
            segment: "NSE_FNO",
            depth_kind: DEPTH_KIND_200,
            side: DEPTH_SIDE_BID,
            prices: &values[..levels],
            quantities: &values[..levels],
            orders: &values[..levels],
            capture_seq: seq,
            ts_nanos: SESSION_TS_NANOS,
        })
        .expect("append");
    };
    // A small batch: the scratch is reserved to the pre-size and kept.
    append(&mut w, 5);
    w.flush().expect("hand off");
    let mut small = rx.try_recv().expect("small batch queued");
    let _ = sink.write(&mut small);
    assert!(
        sink.scratch.capacity() >= DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES,
        "a normal batch keeps the reserved scratch"
    );
    // A burst: the batch's text passes the pre-size.
    while w.staged.text_bytes() <= DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES {
        append(&mut w, MAX_DEPTH_BOOK_LEVELS);
    }
    w.flush().expect("hand off");
    let mut burst = rx.try_recv().expect("burst batch queued");
    let _ = sink.write(&mut burst);
    assert!(
        sink.scratch.capacity() <= DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES,
        "the burst-sized scratch must not be kept: capacity {}",
        sink.scratch.capacity()
    );
    assert!(
        burst.buffer.as_bytes().is_empty(),
        "the batch keeps the empty buffer it arrived with"
    );
    // The next normal batch reserves the pre-size again.
    append(&mut w, 5);
    w.flush().expect("hand off");
    let mut after = rx.try_recv().expect("next batch queued");
    let _ = sink.write(&mut after);
    assert!(sink.scratch.capacity() >= DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES);
    assert!(sink.scratch.capacity() <= DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES * 2);
    let _ = std::fs::remove_dir_all(&spill);
}

// Regression: 2026-11-11 — PR AA-G review (finding 17). On the after-close
// pass's SYNCHRONOUS writer a ninth name table inside one batch commits the
// batch from inside `append_book_row` (`stage_pin_after_cut`), outside the
// pass's per-flush archive quiesce lock. With
// `hold_archive_quiesce_on_pin_cut` set, that commit waits while an archiver
// holds the exclusive side, and runs as soon as it lets go.
#[test]
fn test_hold_archive_quiesce_on_pin_cut_holds_the_in_append_commit() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let quiesce: &'static crate::archive_quiesce::ArchiveQuiesce =
        Box::leak(Box::new(crate::archive_quiesce::ArchiveQuiesce::new()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let archiver = runtime
        .block_on(quiesce.hold_writers(std::time::Duration::from_secs(5)))
        .expect("the archiver takes the exclusive side");
    let (done_tx, done_rx) = std::sync::mpsc::channel::<i64>();
    let writer = std::thread::spawn(move || {
        let mut w = DepthWriter::for_test_book(Feed::Dhan);
        w.hold_archive_quiesce_on_pin_cut_with(quiesce);
        let prices = [100.0];
        for t in 0..crate::depth_stage::DEPTH_STAGE_LABEL_PINS as i64 + 1 {
            let mut labels = CandleContractLabels::default();
            labels.insert((ID_BASE + t, "NSE_FNO"), Arc::from("N"));
            publish_candle_contract_labels(labels);
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
            let _ = w.append_book_row(&row);
            let _ = done_tx.send(t);
        }
    });
    let pins = crate::depth_stage::DEPTH_STAGE_LABEL_PINS as i64;
    for t in 0..pins {
        assert_eq!(
            done_rx.recv_timeout(std::time::Duration::from_secs(30)),
            Ok(t),
            "appends that stage only never wait for the archiver"
        );
    }
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the ninth table's in-append commit waits while the archiver holds the lock"
    );
    drop(archiver);
    assert_eq!(
        done_rx.recv_timeout(std::time::Duration::from_secs(30)),
        Ok(pins),
        "it commits as soon as the archiver lets go"
    );
    writer.join().expect("writer thread");
    publish_candle_contract_labels(CandleContractLabels::default());
}

/// The production setter points the pin cut at the PROCESS lock (the one the
/// archiver and the after-close pass's flushes use).
#[test]
fn test_hold_archive_quiesce_on_pin_cut_uses_the_process_lock() {
    let mut w = DepthWriter::for_test_book(Feed::Dhan);
    assert!(w.pin_cut_quiesce.is_none(), "off by default");
    w.hold_archive_quiesce_on_pin_cut();
    assert!(
        w.pin_cut_quiesce
            .is_some_and(|q| std::ptr::eq(q, crate::archive_quiesce::process_archive_quiesce())),
        "the process lock"
    );
}

// ---- Speed fix 5 (2026-11-17): the pair append and the generation pin ----

/// One step of the pair-versus-singles stream.
#[derive(Debug, Clone)]
enum PairOp {
    Append(u64),
    Publish(u64),
    Flush,
    Discard,
    MarkUnbacked,
}

fn pair_op_strategy() -> impl Strategy<Value = PairOp> {
    prop_oneof![
        12 => any::<u64>().prop_map(PairOp::Append),
        4 => any::<u64>().prop_map(PairOp::Publish),
        2 => Just(PairOp::Flush),
        1 => Just(PairOp::Discard),
        1 => Just(PairOp::MarkUnbacked),
    ]
}

/// The values of one generated pair: identity, then per side
/// `(prices, quantities, orders)` lengths and values.
struct PairCase {
    id: i64,
    segment: &'static str,
    kind: &'static str,
    seq: i64,
    ts: i64,
    values: [[[f64; MAX_DEPTH_BOOK_LEVELS + 1]; 3]; 2],
    lens: [[usize; 3]; 2],
}

impl PairCase {
    fn new(seed: u64, counter: &mut i64) -> Self {
        let mut rng = Rng(seed | 1);
        let kind = if rng.below(6) == 0 {
            DEPTH_KIND_20
        } else {
            DEPTH_KIND_5
        };
        let id = ID_BASE + rng.below(IDS as u64) as i64;
        let segment = SEGMENTS[(id % 3) as usize];
        *counter += 1 + rng.below(2) as i64;
        let seq = if rng.below(15) == 0 { 0 } else { *counter };
        let ts = match rng.below(12) {
            0 => SESSION_TS_NANOS - 3 * 3_600 * 1_000_000_000,
            1 => -5,
            _ => SESSION_TS_NANOS + rng.below(1_000_000_000) as i64,
        };
        let base = if kind == DEPTH_KIND_5 { 5 } else { 20 };
        let mut values = [[[0.0_f64; MAX_DEPTH_BOOK_LEVELS + 1]; 3]; 2];
        let mut lens = [[0_usize; 3]; 2];
        for side in 0..2 {
            let n = match rng.below(12) {
                0 => rng.below(5) as usize,
                1 => MAX_DEPTH_BOOK_LEVELS + 1,
                _ => base,
            };
            lens[side] = [n; 3];
            // A malformed side now and then: one slice a level longer.
            if rng.below(20) == 0 {
                lens[side][1 + rng.below(2) as usize] = (n + 1).min(MAX_DEPTH_BOOK_LEVELS + 1);
            }
            for column in &mut values[side] {
                for v in column.iter_mut() {
                    *v = match rng.below(15) {
                        0 => f64::NAN,
                        1 => f64::from_bits(rng.next()),
                        _ => rng.below(100_000) as f64 / 20.0,
                    };
                }
            }
        }
        Self {
            id,
            segment,
            kind,
            seq,
            ts,
            values,
            lens,
        }
    }

    fn row(&self, side: usize) -> DepthBookRow<'_> {
        DepthBookRow {
            security_id: self.id,
            segment: self.segment,
            depth_kind: self.kind,
            side: if side == 0 {
                DEPTH_SIDE_BID
            } else {
                DEPTH_SIDE_ASK
            },
            prices: &self.values[side][0][..self.lens[side][0]],
            quantities: &self.values[side][1][..self.lens[side][1]],
            orders: &self.values[side][2][..self.lens[side][2]],
            capture_seq: self.seq,
            ts_nanos: self.ts,
        }
    }

    fn pair(&self) -> DepthBookPair<'_> {
        let (bid, ask) = (self.row(0), self.row(1));
        DepthBookPair {
            security_id: self.id,
            segment: self.segment,
            depth_kind: self.kind,
            bid_prices: bid.prices,
            bid_quantities: bid.quantities,
            bid_orders: bid.orders,
            ask_prices: ask.prices,
            ask_quantities: ask.quantities,
            ask_orders: ask.orders,
            capture_seq: self.seq,
            ts_nanos: self.ts,
        }
    }
}

/// What a writer shows after a step: pending levels, pending text bytes,
/// the encoded pending rows, the unbacked mark and the cached run.
fn writer_state(w: &DepthWriter) -> (usize, usize, Vec<u8>, bool, Option<(i64, &'static str)>) {
    (
        w.pending(),
        w.pending_bytes(),
        w.pending_ilp_bytes(),
        w.pending_unbacked,
        w.book_label.map(|(id, seg, _, _)| (id, seg)),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Speed fix 5: `append_book_pair` is the bid single append then the ask
    /// single append, step for step: the same results, the same pending
    /// levels and text bytes, the same staged rows, the same batches handed
    /// off, across random name publishes (a ninth table included), flushes,
    /// rescues, malformed sides and out-of-window rows.
    #[test]
    fn proptest_fix5_append_book_pair_equals_two_single_appends(
        ops in proptest::collection::vec(pair_op_strategy(), 1..200)
    ) {
        let _g = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish_candle_contract_labels(CandleContractLabels::default());
        let spill_a = temp_dir("pair-a");
        let spill_b = temp_dir("pair-b");
        let make = |spill: PathBuf| {
            let (mut w, sink, rx) = DepthWriter::for_test_book(Feed::Dhan)
                .with_spill_dir_for_test(spill)
                .with_wal_defer_check_for_test(|_, _, _| false)
                .split_for_offload();
            drop(sink);
            let (rescue_sink, rescue_rx) = w.split_rescue_offload();
            drop(rescue_sink);
            (w, rx, rescue_rx)
        };
        let (mut pair_w, pair_rx, pair_rescue) = make(spill_a.clone());
        let (mut single_w, single_rx, single_rescue) = make(spill_b.clone());
        let mut counter = 10_000_i64;
        let drain = |rx: &std::sync::mpsc::Receiver<DepthFlushBatch>,
                     rescue: &std::sync::mpsc::Receiver<DepthRescueBatch>| {
            let mut out = Vec::new();
            let mut rows = 0_usize;
            while let Ok(b) = rx.try_recv() {
                out.extend_from_slice(&flush_batch_bytes(&b));
                rows += b.rows;
            }
            while let Ok(b) = rescue.try_recv() {
                out.extend_from_slice(&rescue_batch_bytes(&b));
                rows += b.rows;
                release_floor(b);
            }
            (out, rows)
        };
        for op in &ops {
            match op {
                PairOp::Append(seed) => {
                    let case = PairCase::new(*seed, &mut counter);
                    let (a_bid, a_ask) = pair_w.append_book_pair(&case.pair());
                    let b_bid = single_w.append_book_row(&case.row(0));
                    let b_ask = single_w.append_book_row(&case.row(1));
                    prop_assert_eq!(a_bid.is_ok(), b_bid.is_ok());
                    prop_assert_eq!(a_ask.is_ok(), b_ask.is_ok());
                }
                PairOp::Publish(seed) => publish(*seed),
                PairOp::Flush => {
                    let a = pair_w.flush().is_ok();
                    let b = single_w.flush().is_ok();
                    prop_assert_eq!(a, b);
                }
                PairOp::Discard => {
                    prop_assert_eq!(pair_w.discard_pending(), single_w.discard_pending());
                }
                PairOp::MarkUnbacked => {
                    pair_w.mark_pending_unbacked();
                    single_w.mark_pending_unbacked();
                }
            }
            prop_assert_eq!(writer_state(&pair_w), writer_state(&single_w), "after {:?}", op);
            prop_assert_eq!(
                drain(&pair_rx, &pair_rescue),
                drain(&single_rx, &single_rescue),
                "handed-off batches differ after {:?}",
                op
            );
        }
        pair_w.close_offload();
        single_w.close_offload();
        let _ = pair_w.discard_pending();
        let _ = single_w.discard_pending();
        prop_assert_eq!(
            drain(&pair_rx, &pair_rescue),
            drain(&single_rx, &single_rescue)
        );
        drop(pair_rescue);
        drop(single_rescue);
        pair_w.close_rescue_offload();
        single_w.close_rescue_offload();
        prop_assert_eq!(read_v2_spill_dir(&spill_a), read_v2_spill_dir(&spill_b));
        let _ = std::fs::remove_dir_all(&spill_a);
        let _ = std::fs::remove_dir_all(&spill_b);
        publish_candle_contract_labels(CandleContractLabels::default());
    }

    /// Speed fix 5: a name-publish-heavy stream (a ninth table inside one
    /// batch is common) through the staged writer, half of its five-level
    /// pairs by `append_book_pair`, still writes the legacy append's bytes.
    #[test]
    fn proptest_fix5_publish_heavy_stream_equals_the_legacy_append(
        ops in proptest::collection::vec(
            prop_oneof![
                10 => any::<u64>().prop_map(Op::Append),
                6 => any::<u64>().prop_map(Op::Publish),
                1 => Just(Op::Flush),
                1 => Just(Op::Discard),
                1 => Just(Op::MarkUnbacked),
            ],
            1..200,
        )
    ) {
        let _g = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (got, got_levels, want, want_levels) = run(&ops, true);
        prop_assert_eq!(got_levels, want_levels);
        prop_assert!(got == want, "staged bytes differ from the legacy append");
    }
}

/// A level writer refuses both sides of a pair, exactly as it refuses two
/// single appends.
#[test]
fn test_append_book_pair_on_a_level_writer_refuses_both_sides() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut counter = 1_i64;
    let mut seed = 3_u64;
    let case = loop {
        let c = PairCase::new(seed, &mut counter);
        if c.ts >= SESSION_TS_NANOS && c.lens == [[5; 3]; 2] && c.seq > 0 {
            break c;
        }
        seed += 2;
    };
    let mut w = DepthWriter::for_test(Feed::Dhan);
    let (bid, ask) = w.append_book_pair(&case.pair());
    assert!(bid.is_err() && ask.is_err());
    assert_eq!(w.pending(), 0);
}

/// A pair whose sides carry different level counts writes each line at its
/// own length (the shared length is taken only for equal counts).
#[test]
fn test_append_book_pair_with_unequal_sides_writes_each_line_at_its_own_length() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut named = CandleContractLabels::default();
    named.insert((ID_BASE, "NSE_FNO"), Arc::from("NIFTY-X"));
    publish_candle_contract_labels(named);
    let bid = [100.0, 99.5, 99.0];
    let ask = [100.5];
    let pair = DepthBookPair {
        security_id: ID_BASE,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_5,
        bid_prices: &bid,
        bid_quantities: &bid,
        bid_orders: &bid,
        ask_prices: &ask,
        ask_quantities: &ask,
        ask_orders: &ask,
        capture_seq: 77,
        ts_nanos: SESSION_TS_NANOS,
    };
    let mut w = DepthWriter::for_test_book(Feed::Dhan);
    let (b, a) = w.append_book_pair(&pair);
    assert!(b.is_ok() && a.is_ok());
    let mut legacy = LegacyBook::new();
    legacy.append(&pair.side(DEPTH_SIDE_BID));
    legacy.append(&pair.side(DEPTH_SIDE_ASK));
    assert_eq!(w.pending_ilp_bytes(), legacy.buffer.as_bytes());
    assert_eq!(w.pending_bytes(), legacy.buffer.len());
    assert_eq!(w.pending(), 4);
    publish_candle_contract_labels(CandleContractLabels::default());
}

/// A ninth name table arriving on a pair's run cuts the batch early and
/// loses no byte (the pair twin of
/// `test_ninth_name_table_cuts_the_batch_and_keeps_every_byte`).
#[test]
fn test_fix5_ninth_name_table_through_append_book_pair_keeps_every_byte() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut w, sink, rx) = DepthWriter::for_test_book(Feed::Dhan).split_for_offload();
    drop(sink);
    let mut legacy = LegacyBook::new();
    let bid = [100.0, 99.95, 99.9, 99.85, 99.8];
    let ask = [100.05, 100.1, 100.15, 100.2, 100.25];
    let mut got = Vec::new();
    for t in 0..20_i64 {
        let mut labels = CandleContractLabels::default();
        labels.insert(
            (ID_BASE + t, "NSE_FNO"),
            Arc::from(format!("N{t}").as_str()),
        );
        publish_candle_contract_labels(labels);
        let pair = DepthBookPair {
            security_id: ID_BASE + t,
            segment: "NSE_FNO",
            depth_kind: DEPTH_KIND_5,
            bid_prices: &bid,
            bid_quantities: &bid,
            bid_orders: &bid,
            ask_prices: &ask,
            ask_quantities: &ask,
            ask_orders: &ask,
            capture_seq: 100 + t,
            ts_nanos: SESSION_TS_NANOS,
        };
        let (b, a) = w.append_book_pair(&pair);
        assert!(b.is_ok() && a.is_ok());
        legacy.append(&pair.side(DEPTH_SIDE_BID));
        legacy.append(&pair.side(DEPTH_SIDE_ASK));
        assert!(w.staged.pin_count() <= crate::depth_stage::DEPTH_STAGE_LABEL_PINS);
        while let Ok(b) = rx.try_recv() {
            got.extend_from_slice(&flush_batch_bytes(&b));
        }
    }
    got.extend_from_slice(&w.pending_ilp_bytes());
    publish_candle_contract_labels(CandleContractLabels::default());
    let text = String::from_utf8_lossy(&got);
    assert!(text.contains("contract=N19"), "{text}");
    assert_eq!(got, legacy.buffer.as_bytes());
}

/// Between publishes, a run change resolves its name from the batch's
/// pinned table (no table load) and still writes the name a fresh load
/// would give; after a publish the new table's name is used.
#[test]
fn test_fix5_run_change_without_a_publish_reads_the_pinned_table() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut first = CandleContractLabels::default();
    for i in 0..3 {
        first.insert(
            (ID_BASE + i, "NSE_FNO"),
            Arc::from(format!("A{i}").as_str()),
        );
    }
    publish_candle_contract_labels(first);
    let mut w = DepthWriter::for_test_book(Feed::Dhan);
    let mut legacy = LegacyBook::new();
    let p = [1.0];
    let row = |id: i64, seq: i64| DepthBookRow {
        security_id: id,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_5,
        side: DEPTH_SIDE_BID,
        prices: &p,
        quantities: &p,
        orders: &p,
        capture_seq: seq,
        ts_nanos: SESSION_TS_NANOS,
    };
    for (i, id) in [0, 1, 2, 0, 1].iter().enumerate() {
        let r = row(ID_BASE + id, 10 + i as i64);
        w.append_book_row(&r).expect("append");
        legacy.append(&r);
    }
    let g = crate::candle_contract_labels::candle_contract_labels_generation();
    assert_eq!(w.staged.current_pin_at_generation(g), Some(0));
    let mut second = CandleContractLabels::default();
    second.insert((ID_BASE + 2, "NSE_FNO"), Arc::from("B2"));
    publish_candle_contract_labels(second);
    for (i, id) in [2, 0].iter().enumerate() {
        let r = row(ID_BASE + id, 20 + i as i64);
        w.append_book_row(&r).expect("append");
        legacy.append(&r);
    }
    assert_eq!(w.staged.pin_count(), 2, "the publish pinned a second table");
    let got = w.pending_ilp_bytes();
    publish_candle_contract_labels(CandleContractLabels::default());
    let text = String::from_utf8_lossy(&got);
    assert_eq!(text.matches("contract=A").count(), 5, "{text}");
    assert_eq!(text.matches("contract=B2").count(), 1, "{text}");
    assert_eq!(got, legacy.buffer.as_bytes());
}
