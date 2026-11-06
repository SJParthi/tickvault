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

/// `MIN_BOOK_LINE_BYTES` is a floor under every line the writer can write:
/// no levels, no contract name, one-digit integers, every feed, every segment
/// label the depth path can carry, every kind and side.
#[test]
fn test_min_book_line_bytes_is_a_floor_for_every_label() {
    let mut shortest = usize::MAX;
    for feed in Feed::ALL {
        for code in 0..=u8::MAX {
            let Some(segment) = crate::depth_persistence::depth_segment_label(code) else {
                continue;
            };
            for kind in [DEPTH_KIND_5, DEPTH_KIND_20, DEPTH_KIND_200] {
                for side in [DEPTH_SIDE_BID, DEPTH_SIDE_ASK] {
                    let r = DepthBookRow {
                        security_id: 0,
                        segment,
                        depth_kind: kind,
                        side,
                        prices: &[],
                        quantities: &[],
                        orders: &[],
                        capture_seq: 0,
                        ts_nanos: 0,
                    };
                    shortest = shortest.min(book_line_len(*feed, &r, 0, None));
                }
            }
        }
    }
    assert!(shortest < usize::MAX, "some label was checked");
    assert!(
        shortest >= MIN_BOOK_LINE_BYTES,
        "the shortest line is {shortest} bytes, under the floor {MIN_BOOK_LINE_BYTES}"
    );
}

/// Fills a pre-sized batch with `levels`-level rows (and `name`) until one
/// more row would pass `DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES` of text (the
/// flush trigger plus a crossing frame); returns the batch.
fn fill_to_the_text_bound(levels: usize, name: Option<&str>) -> DepthStagedBatch {
    let mut batch = DepthStagedBatch::presized();
    let values = vec![123.45_f64; levels];
    let mut seq = 0_i64;
    loop {
        let r = DepthBookRow {
            security_id: 1,
            segment: "IDX_I",
            depth_kind: DEPTH_KIND_5,
            side: DEPTH_SIDE_BID,
            prices: &values,
            quantities: &values,
            orders: &values,
            capture_seq: seq % 10,
            ts_nanos: seq % 10,
        };
        let len = book_line_len(Feed::Dhan, &r, levels, name.map(escaped_symbol_len));
        if batch.text_bytes() + len > DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES {
            return batch;
        }
        let _ = batch.push(&r, 0, len);
        seq += 1;
    }
}

/// PF-4b review round 1: a batch that grew only up to the flush trigger (a
/// depth-20 or depth-200 burst, or the shortest possible rows) must still be
/// recycled, or every later hand-off reserves a fresh batch on the drain.
#[test]
fn test_a_batch_grown_to_the_flush_trigger_is_still_recycled() {
    let bound = crate::depth_persistence::MAX_DEPTH_STAGED_SPARE_RAM_BYTES;
    for (levels, name) in [
        (0, None),
        (5, None),
        (5, Some("NIFTY-25Nov2026-24500-CE")),
        (20, None),
        (200, None),
    ] {
        let batch = fill_to_the_text_bound(levels, name);
        assert!(!batch.is_empty());
        assert!(
            batch.ram_bytes() <= bound,
            "{levels} levels: {} bytes over the recycle bound {bound}",
            batch.ram_bytes()
        );
    }
    // (The realistic inline shape never grows at all: pinned by
    // `test_presize_holds_a_full_batch_of_inline_rows_without_growing` and by
    // the depth writer's regrow test. These rows are deliberately shorter.)
    // The depth-200 shape does grow (counted on the drain), within the bound.
    assert!(
        fill_to_the_text_bound(200, None).ram_bytes() > DepthStagedBatch::presized().ram_bytes()
    );
}

// ---- Behaviour of the small batch accessors (pub-fn test guard, 2026-11-06) ----

#[test]
fn test_for_layout_presizes_a_book_batch_and_leaves_a_level_batch_empty() {
    let (rows, values) = DepthStagedBatch::for_layout(DepthLayout::Book).capacities_for_test();
    assert!(rows >= DEPTH_STAGED_ROWS_PRESIZE, "book rows {rows}");
    assert!(
        values >= DEPTH_STAGED_VALUES_PRESIZE,
        "book values {values}"
    );
    assert_eq!(
        DepthStagedBatch::for_layout(DepthLayout::Levels).capacities_for_test(),
        (0, 0),
        "a level writer never stages, so its batch allocates nothing"
    );
}

#[test]
fn test_text_bytes_sums_every_pushed_row_and_clear_rows_resets_it() {
    let mut batch = DepthStagedBatch::presized();
    assert_eq!(batch.text_bytes(), 0);
    let prices = [10.5, 11.0];
    let a = row(1, &prices, &prices, &prices, 1);
    let b = row(2, &prices[..1], &prices[..1], &prices[..1], 2);
    let a_len = book_line_len(Feed::Dhan, &a, 2, None);
    let b_len = book_line_len(Feed::Dhan, &b, 1, None);
    let _ = batch.push(&a, 0, a_len);
    let _ = batch.push(&b, 0, b_len);
    assert_eq!(batch.text_bytes(), a_len + b_len);
    assert_eq!(
        batch.text_bytes(),
        batch.encode_readonly(Feed::Dhan).len(),
        "the counted bytes are the bytes the encoder writes"
    );
    batch.clear_rows();
    assert_eq!(batch.text_bytes(), 0);
}

#[test]
fn test_pending_levels_counts_each_level_and_an_empty_side_as_one() {
    let mut batch = DepthStagedBatch::presized();
    assert_eq!(batch.pending_levels(), 0);
    let prices = [1.0, 2.0, 3.0];
    let three = row(1, &prices, &prices, &prices, 1);
    let empty = row(2, &[], &[], &[], 2);
    let _ = batch.push(&three, 0, book_line_len(Feed::Dhan, &three, 3, None));
    assert_eq!(batch.pending_levels(), 3);
    let _ = batch.push(&empty, 0, book_line_len(Feed::Dhan, &empty, 0, None));
    assert_eq!(batch.pending_levels(), 4, "an empty side counts one level");
    batch.clear();
    assert_eq!(batch.pending_levels(), 0);
}

#[test]
fn test_refused_levels_counts_the_levels_of_refused_rows_and_clear_rows_resets_it() {
    let _g = ENCODE_FAILURE_COUNT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut batch = DepthStagedBatch::presized();
    let prices = [1.0, 2.0];
    let good = row(3, &prices, &prices, &prices, 10);
    let mut bad = row(4, &prices, &prices, &prices, 11);
    bad.ts_nanos = -1;
    let _ = batch.push(&good, 0, book_line_len(Feed::Dhan, &good, 2, None));
    let _ = batch.push(&bad, 0, 0);
    assert_eq!(
        batch.refused_levels(),
        0,
        "nothing refused before an encode"
    );
    assert_eq!(batch.encode_into(Feed::Dhan, &mut book_buffer()), 1);
    assert_eq!(batch.refused_levels(), 2, "the refused row had two levels");
    assert_eq!(
        batch.encode_into(Feed::Dhan, &mut book_buffer()),
        1,
        "a marked row is skipped"
    );
    assert_eq!(batch.refused_levels(), 2, "and is not counted twice");
    batch.clear_rows();
    assert_eq!(batch.refused_levels(), 0);
}

#[test]
fn test_pin_table_reuses_the_last_slot_and_hands_the_table_back_when_full() {
    let mut batch = DepthStagedBatch::new();
    let a = labels_with(1, "A");
    let b = labels_with(2, "B");
    assert_eq!(batch.pin_table(Arc::clone(&a)).expect("pin a"), 0);
    assert_eq!(
        batch.pin_table(Arc::clone(&a)).expect("a again"),
        0,
        "the last pinned table reuses its slot"
    );
    assert_eq!(batch.pin_count(), 1);
    assert_eq!(batch.pin_table(Arc::clone(&b)).expect("pin b"), 1);
    assert_eq!(
        batch.pin_table(Arc::clone(&a)).expect("a after b"),
        2,
        "only the LAST table is deduplicated"
    );
    while batch.pin_count() < DEPTH_STAGE_LABEL_PINS {
        let next = batch.pin_count() as i64;
        let _ = batch.pin_table(labels_with(next, "X")).expect("free slot");
    }
    let extra = labels_with(99, "Z");
    let back = batch
        .pin_table(Arc::clone(&extra))
        .expect_err("every slot holds another table");
    assert!(Arc::ptr_eq(&back, &extra), "the same table is handed back");
    assert_eq!(batch.pin_count(), DEPTH_STAGE_LABEL_PINS);
}

#[test]
fn test_escaped_symbol_len_adds_one_byte_per_escaped_character() {
    assert_eq!(escaped_symbol_len(""), 0);
    assert_eq!(escaped_symbol_len("NIFTY-25Nov2026-24500-CE"), 24);
    // Space, comma, equals, newline, carriage return and backslash each
    // take one backslash before them.
    assert_eq!(escaped_symbol_len("a b,c=d\ne\rf\\g"), 13 + 6);
    assert_eq!(escaped_symbol_len("é"), 2, "bytes, not characters");
    // The questdb-rs writer escapes the same set: a symbol adds `,s=` plus
    // the escaped value to the line.
    for value in ["plain", "with space", "a,b=c", "back\\slash"] {
        let mut plain = book_buffer();
        plain
            .table("t")
            .expect("table")
            .column_i64("x", 1)
            .expect("col")
            .at(questdb::ingress::TimestampNanos::new(TS))
            .expect("at");
        let mut named = book_buffer();
        named
            .table("t")
            .expect("table")
            .symbol("s", value)
            .expect("symbol")
            .column_i64("x", 1)
            .expect("col")
            .at(questdb::ingress::TimestampNanos::new(TS))
            .expect("at");
        assert_eq!(
            named.len() - plain.len(),
            ",s=".len() + escaped_symbol_len(value),
            "{value:?}"
        );
    }
}
