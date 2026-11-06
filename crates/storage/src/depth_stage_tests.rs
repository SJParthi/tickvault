//! Unit tests for the depth book stage (plan item PF-4b, 2026-11-06).

use super::*;
use crate::candle_contract_labels::{TEST_PUBLISH_LOCK, publish_candle_contract_labels};
use crate::depth_persistence::{
    DEPTH_KIND_5, DEPTH_KIND_20, DEPTH_KIND_200, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthLayout,
    MAX_DEPTH_BOOK_LEVELS, write_book_line_into,
};
use proptest::prelude::*;

const TS: i64 = 1_793_268_000_000_000_000;

/// Serialises the two tests that read the process-wide encode-failure count.
static ENCODE_FAILURE_COUNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn book_buffer() -> Buffer {
    Buffer::new(DepthLayout::Book.protocol())
}

fn row<'a>(
    id: i64,
    prices: &'a [f64],
    quantities: &'a [f64],
    orders: &'a [f64],
    seq: i64,
) -> DepthBookRow<'a> {
    DepthBookRow {
        security_id: id,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_5,
        side: DEPTH_SIDE_BID,
        prices,
        quantities,
        orders,
        capture_seq: seq,
        ts_nanos: TS,
    }
}

fn labels_with(id: i64, name: &str) -> Arc<CandleContractLabels> {
    let mut labels = CandleContractLabels::default();
    labels.insert((id, "NSE_FNO"), Arc::from(name));
    Arc::new(labels)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// `book_line_len` is the EXACT length the line writer writes, so every
    /// flush and width cut lands where it did on the text buffer.
    #[test]
    fn proptest_book_line_len_is_exact(
        id in any::<i64>(),
        seq in any::<i64>(),
        ts in 0_i64..=i64::MAX,
        levels in 0_usize..=MAX_DEPTH_BOOK_LEVELS,
        kind in 0_usize..3,
        side in any::<bool>(),
        segment in 0_usize..4,
        name in proptest::option::of("[A-Za-z0-9 \\\\\\-ÄÖÜé]{0,64}"),
        bits in any::<u64>(),
    ) {
        let values: Vec<f64> = (0..levels)
            .map(|i| f64::from_bits(bits.rotate_left(i as u32)))
            .collect();
        let row = DepthBookRow {
            security_id: id,
            segment: ["NSE_FNO", "NSE_EQ", "IDX_I", "BSE_EQ"][segment],
            depth_kind: [DEPTH_KIND_5, DEPTH_KIND_20, DEPTH_KIND_200][kind],
            side: if side { DEPTH_SIDE_BID } else { DEPTH_SIDE_ASK },
            prices: &values,
            quantities: &values,
            orders: &values,
            capture_seq: seq,
            ts_nanos: ts,
        };
        let mut buffer = book_buffer();
        write_book_line_into(&mut buffer, Feed::Dhan, &row, levels, name.as_deref())
            .expect("a non-negative timestamp always writes");
        prop_assert_eq!(
            book_line_len(Feed::Dhan, &row, levels, name.as_deref().map(escaped_symbol_len)),
            buffer.len()
        );
    }
}

#[test]
fn test_i64_text_len_matches_itoa_at_the_edges() {
    for v in [
        0_i64,
        1,
        9,
        10,
        -1,
        -9,
        -10,
        99,
        100,
        i64::MAX,
        i64::MIN,
        i64::MIN + 1,
    ] {
        assert_eq!(i64_text_len(v), v.to_string().len(), "{v}");
    }
}

#[test]
fn test_new_batch_allocates_nothing_and_presized_reserves_both_vecs() {
    let empty = DepthStagedBatch::new();
    assert_eq!(empty.ram_bytes(), 0);
    assert!(empty.is_empty());
    let presized = DepthStagedBatch::presized();
    assert!(presized.rows.capacity() >= DEPTH_STAGED_ROWS_PRESIZE);
    assert!(presized.values.capacity() >= DEPTH_STAGED_VALUES_PRESIZE);
    assert_eq!(
        DepthStagedBatch::for_layout(DepthLayout::Levels).ram_bytes(),
        0,
        "a level writer never stages"
    );
    assert!(DepthStagedBatch::for_layout(DepthLayout::Book).ram_bytes() > 0);
}

#[test]
fn test_presize_holds_a_full_batch_of_inline_rows_without_growing() {
    let mut batch = DepthStagedBatch::presized();
    let prices = [100.0, 99.95, 99.9, 99.85, 99.8];
    let mut grew = false;
    let mut seq = 1_i64;
    // Inline five-level rows until the batch holds the book flush size.
    while batch.text_bytes() < crate::depth_persistence::DEPTH_BOOK_FLUSH_BYTES {
        let r = row(52_000, &prices, &prices, &prices, seq);
        let len = book_line_len(Feed::Dhan, &r, 5, None);
        grew |= batch.push(&r, 0, len);
        seq += 1;
    }
    assert!(!grew, "{} rows grew the pre-size", batch.len());
    assert!(batch.len() > 20_000, "{} rows", batch.len());
}

#[test]
fn test_push_then_encode_writes_the_line_writer_bytes() {
    let mut batch = DepthStagedBatch::presized();
    let table = labels_with(7, "NIFTY-25Nov2026-24500-CE");
    let pin = batch.pin_table(Arc::clone(&table)).expect("pin");
    let prices = [100.5, f64::NAN, -0.0];
    let quantities = [1.0, 2.0, 3.0];
    let orders = [4.0, 5.0, 6.0];
    let r = row(7, &prices, &quantities, &orders, 77);
    let len = book_line_len(Feed::Dhan, &r, 3, Some("NIFTY-25Nov2026-24500-CE".len()));
    let _ = batch.push(&r, pin, len);
    let mut want = book_buffer();
    write_book_line_into(
        &mut want,
        Feed::Dhan,
        &r,
        3,
        Some("NIFTY-25Nov2026-24500-CE"),
    )
    .expect("write");
    let mut got = book_buffer();
    assert_eq!(batch.encode_into(Feed::Dhan, &mut got), 1);
    assert_eq!(got.as_bytes(), want.as_bytes());
    assert_eq!(batch.text_bytes(), want.len());
    assert_eq!(batch.pending_levels(), 3);
    assert_eq!(batch.encode_readonly(Feed::Dhan), want.as_bytes());
}

#[test]
fn test_a_zero_level_side_counts_one_level_and_encodes_empty_arrays() {
    let mut batch = DepthStagedBatch::presized();
    let r = row(9, &[], &[], &[], 5);
    let _ = batch.push(&r, 0, book_line_len(Feed::Dhan, &r, 0, None));
    assert_eq!(batch.pending_levels(), 1);
    let mut want = book_buffer();
    write_book_line_into(&mut want, Feed::Dhan, &r, 0, None).expect("write");
    assert_eq!(batch.encode_readonly(Feed::Dhan), want.as_bytes());
}

#[test]
fn test_pins_dedupe_the_last_table_and_refuse_a_ninth() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut batch = DepthStagedBatch::new();
    publish_candle_contract_labels(CandleContractLabels::default());
    let first = batch
        .try_pin(crate::candle_contract_labels::candle_contract_labels())
        .expect("pin");
    let again = batch
        .try_pin(crate::candle_contract_labels::candle_contract_labels())
        .expect("same table");
    assert_eq!(first, again);
    assert_eq!(batch.pin_count(), 1);
    for i in 1..DEPTH_STAGE_LABEL_PINS {
        let pin = batch
            .pin_table(labels_with(i as i64, "X"))
            .expect("free slot");
        assert_eq!(usize::from(pin), i);
    }
    assert!(batch.pin_table(labels_with(99, "X")).is_err());
    assert!(
        batch
            .try_pin(crate::candle_contract_labels::candle_contract_labels())
            .is_err(),
        "the first table is not the LAST pinned, so a ninth slot is needed"
    );
    batch.clear_rows();
    assert_eq!(
        batch.pin_count(),
        DEPTH_STAGE_LABEL_PINS,
        "clear_rows keeps pins"
    );
    batch.clear();
    assert_eq!(batch.pin_count(), 0, "clear drops pins");
}

#[test]
fn test_pinned_table_clones_and_an_unused_slot_reads_none() {
    let mut batch = DepthStagedBatch::new();
    let table = labels_with(1, "A");
    let pin = batch.pin_table(Arc::clone(&table)).expect("pin");
    let got = batch.pinned_table(pin).expect("pinned");
    assert!(Arc::ptr_eq(&got, &table));
    assert!(batch.pinned_table(5).is_none());
}

/// A row the encoder refuses (a negative designated timestamp) is counted
/// once, leaves `out` untouched, and is skipped by every later encode.
#[test]
fn test_encode_failure_is_counted_once_and_skipped_after() {
    let _g = ENCODE_FAILURE_COUNT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut batch = DepthStagedBatch::presized();
    let prices = [1.0];
    let good = row(3, &prices, &prices, &prices, 10);
    let mut bad = row(4, &prices, &prices, &prices, 11);
    bad.ts_nanos = -1;
    let _ = batch.push(&good, 0, book_line_len(Feed::Dhan, &good, 1, None));
    let _ = batch.push(&bad, 0, 0);
    let before = encode_failures_so_far();
    let mut out = book_buffer();
    assert_eq!(batch.encode_into(Feed::Dhan, &mut out), 1);
    assert_eq!(encode_failures_so_far(), before + 1);
    assert_eq!(batch.refused_levels(), 1);
    let mut want = book_buffer();
    write_book_line_into(&mut want, Feed::Dhan, &good, 1, None).expect("write");
    assert_eq!(
        out.as_bytes(),
        want.as_bytes(),
        "the refused line left no byte"
    );
    let mut again = book_buffer();
    assert_eq!(batch.encode_into(Feed::Dhan, &mut again), 1);
    assert_eq!(
        encode_failures_so_far(),
        before + 1,
        "a second encode must not count it again"
    );
    assert_eq!(batch.encode_counted(Feed::Dhan, &mut book_buffer()), 0);
    assert_eq!(encode_failures_so_far(), before + 1);
}

#[test]
fn test_encode_counted_counts_without_marking() {
    let _g = ENCODE_FAILURE_COUNT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut batch = DepthStagedBatch::presized();
    let prices = [1.0, 2.0];
    let mut bad = row(4, &prices, &prices, &prices, 11);
    bad.ts_nanos = -7;
    let _ = batch.push(&bad, 0, 0);
    let before = encode_failures_so_far();
    assert_eq!(batch.encode_counted(Feed::Dhan, &mut book_buffer()), 2);
    assert_eq!(batch.encode_counted(Feed::Dhan, &mut book_buffer()), 2);
    assert_eq!(
        encode_failures_so_far(),
        before + 2,
        "unmarked: each LAST encode counts"
    );
}

/// The body of the first function whose signature contains `sig` in `src`.
fn body_of<'a>(src: &'a str, sig: &str) -> &'a str {
    let found = src.find(sig);
    assert!(found.is_some(), "`{sig}` not found; the scan is stale");
    let start = found.unwrap_or_default();
    let open = start + src[start..].find('{').expect("body opens");
    let mut depth = 0_i32;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[start..open + i + 1];
                }
            }
            _ => {}
        }
    }
    assert_eq!(depth, 0, "`{sig}` body never closes");
    &src[start..]
}

/// PF-4b ratchet: the drain's book append path stages, it never writes ILP
/// text again; and the writer and rescue threads encode BEFORE the POST and
/// the floor release (so a refused row is counted before the batch is
/// acknowledged).
#[test]
fn test_pf4b_drain_book_append_never_writes_ilp_text() {
    let src = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/depth_persistence.rs"),
    )
    .expect("read depth_persistence.rs");
    for sig in [
        "pub fn append_book_row(",
        "fn append_book_row_inner(",
        "fn stage_pin_after_cut(",
    ] {
        let body = body_of(&src, sig);
        for writer in [
            "write_book_line_into",
            "encode_into",
            "encode_counted",
            ".column_arr(",
            ".table(",
            ".at(",
        ] {
            assert!(
                !body.contains(writer),
                "`{sig}` names the ILP writer `{writer}`: the drain must stage, not encode"
            );
        }
    }
    assert!(body_of(&src, "fn append_book_row_inner(").contains("self.staged.push("));
    let write = body_of(&src, "pub fn write(&mut self, batch: &mut DepthFlushBatch)");
    let encode = write.find("self.encode_staged(batch)").expect("encodes");
    let post = write.find("self.write_inner(batch)").expect("posts");
    assert!(
        encode < post,
        "the writer thread must encode before the POST"
    );
    let rescue = body_of(&src, "pub fn rescue(&self, batch: &DepthRescueBatch)");
    let spill = rescue.find("spill_payloads").expect("encodes in the spill");
    let floor = rescue
        .find("release_rescue_floor")
        .expect("releases the floor");
    assert!(
        spill < floor,
        "the rescue thread must encode before the floor release"
    );
}

#[test]
fn test_ram_bytes_counts_capacity_not_length() {
    let mut batch = DepthStagedBatch::presized();
    let ram = batch.ram_bytes();
    assert!(ram >= DEPTH_STAGED_ROWS_PRESIZE * STAGED_BOOK_ROW_BYTES);
    let prices = [1.0];
    let r = row(1, &prices, &prices, &prices, 1);
    let _ = batch.push(&r, 0, 1);
    assert_eq!(
        batch.ram_bytes(),
        ram,
        "a push inside the pre-size adds no RAM"
    );
    batch.clear();
    assert_eq!(batch.ram_bytes(), ram, "a clear keeps the capacity");
}
