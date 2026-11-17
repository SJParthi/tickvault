//! Unit tests for [`super::StagedBatch`] (plan item PF-4, 2026-11-06).

use super::*;
use crate::candle_contract_labels::{
    TEST_PUBLISH_LOCK, candle_contract_labels, publish_candle_contract_labels,
};
use crate::feed_aux_persistence::AuxPacketKind;
use tickvault_common::tick_types::ParsedTick;

fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn row(seq: i64) -> TickRow {
    TickRow {
        security_id: 7_900_000_001,
        segment: "NSE_FNO",
        ltp: 245.35,
        volume: 1_250_000,
        ts_ist_nanos: 1_793_268_000_000_000_000,
        capture_seq: seq,
        open: Some(238.05),
        high: None,
        low: None,
        close: None,
        oi: Some(4_512_300),
        oi_day_high: None,
        oi_day_low: None,
        avg_price: None,
        last_trade_qty: Some(75),
        total_buy_qty: None,
        total_sell_qty: None,
        received_at_ist_nanos: Some(1_793_268_000_250_000_000),
        payload_hash: None,
    }
}

#[test]
fn test_staged_row_is_small_and_a_presized_batch_holds_the_presize() {
    assert!(STAGED_ROW_BYTES <= 112, "{STAGED_ROW_BYTES}");
    let b = StagedBatch::presized();
    assert!(b.capacity() >= TICK_STAGED_ROWS_PRESIZE);
    assert!(b.is_empty(), "pre-touching never changes the length");
    assert_eq!(MAX_PRODUCER_ROWS, 32_768);
    let empty = StagedBatch::new();
    assert_eq!(empty.capacity(), 0, "new() allocates nothing");
}

#[test]
fn test_try_pin_reuses_the_same_table_and_refuses_a_ninth() {
    let _g = lock();
    let mut b = StagedBatch::presized();
    publish_candle_contract_labels(CandleContractLabels::default());
    assert_eq!(b.try_pin(candle_contract_labels()).ok(), Some(0));
    assert_eq!(
        b.try_pin(candle_contract_labels()).ok(),
        Some(0),
        "the same table reuses its pin"
    );
    for want in 1..TICK_STAGE_LABEL_PINS as u8 {
        publish_candle_contract_labels(CandleContractLabels::default());
        assert_eq!(b.try_pin(candle_contract_labels()).ok(), Some(want));
    }
    publish_candle_contract_labels(CandleContractLabels::default());
    assert!(
        b.try_pin(candle_contract_labels()).is_err(),
        "a ninth table must be handed back, never pinned over another"
    );
    b.clear();
    assert_eq!(
        b.try_pin(candle_contract_labels()).ok(),
        Some(0),
        "clear releases every pin"
    );
}

#[test]
fn test_clear_releases_the_pinned_tables() {
    let _g = lock();
    publish_candle_contract_labels(CandleContractLabels::default());
    let mut b = StagedBatch::presized();
    let pin = b.try_pin(candle_contract_labels()).expect("pin");
    assert_eq!(pin, 0);
    let held = candle_contract_labels();
    let pinned_strong = Arc::strong_count(&held);
    b.clear();
    assert!(
        Arc::strong_count(&held) < pinned_strong,
        "the batch's reference is dropped by clear"
    );
}

#[test]
fn test_push_reports_growth_only_past_capacity() {
    let mut b = StagedBatch::new();
    assert!(
        b.push(StagedRow::Refused { capture_seq: 1 }),
        "zero capacity grows"
    );
    let mut b = StagedBatch::presized();
    for i in 0..TICK_STAGED_ROWS_PRESIZE {
        assert!(!b.push(StagedRow::Refused {
            capture_seq: i as i64
        }));
    }
    assert!(
        b.push(StagedRow::Refused { capture_seq: -1 }),
        "the push past the pre-size grows, and still keeps the row"
    );
    assert_eq!(b.len(), TICK_STAGED_ROWS_PRESIZE + 1);
}

/// Owner decision 2026-11-06: a row the encoder refuses is counted on the
/// encoding thread, once, and the other rows encode.
#[test]
fn test_encode_counts_a_refused_row_once_and_writes_the_rest() {
    let _g = lock();
    publish_candle_contract_labels(CandleContractLabels::default());
    let mut b = StagedBatch::presized();
    let pin = b.try_pin(candle_contract_labels()).expect("pin");
    let mut bad = row(9);
    bad.ts_ist_nanos = -1; // ILP refuses a negative designated timestamp
    b.push_extra(ExtraRow::Tick(row(8)), pin);
    b.push_extra(ExtraRow::Tick(bad), pin);
    b.push_extra(
        ExtraRow::Quote {
            row: row(10),
            exchange_ts: Some(1_793_268_000_000_000_000),
        },
        pin,
    );
    let before = encode_failures_so_far();
    for encoder in [TickIlpEncoder::Library, TickIlpEncoder::Own] {
        let mut out = TickRowBuffer::new(encoder);
        let mut copy = StagedBatch::presized();
        let p = copy.try_pin(candle_contract_labels()).expect("pin");
        for r in [row(8), bad] {
            copy.push_extra(ExtraRow::Tick(r), p);
        }
        assert_eq!(copy.encode_into(Feed::Dhan, &mut out), 1, "{encoder:?}");
        let text = String::from_utf8(out.as_bytes().to_vec()).expect("utf8");
        assert_eq!(text.lines().count(), 1, "{encoder:?}: {text}");
        assert!(text.contains("capture_seq=8i"), "{encoder:?}: {text}");
        let mut again = TickRowBuffer::new(encoder);
        assert_eq!(
            copy.encode_into(Feed::Dhan, &mut again),
            1,
            "a second encode skips the refused row without counting it again"
        );
    }
    let mut out = TickRowBuffer::new(TickIlpEncoder::Own);
    assert_eq!(b.encode_into(Feed::Dhan, &mut out), 2);
    let text = String::from_utf8(out.as_bytes().to_vec()).expect("utf8");
    assert!(text.lines().next().is_some_and(|l| l.starts_with("ticks,")));
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("tick_quotes,"))
    );
    assert!(
        encode_failures_so_far() >= before + 3,
        "each refused row was counted exactly once per batch"
    );
    assert!(matches!(
        b.extras.get(1),
        Some(ExtraRow::Refused { capture_seq: 9 })
    ));
}

#[test]
fn test_encode_readonly_matches_encode_into_and_leaves_the_batch() {
    let _g = lock();
    publish_candle_contract_labels(CandleContractLabels::default());
    let mut b = StagedBatch::presized();
    let pin = b.try_pin(candle_contract_labels()).expect("pin");
    b.push_extra(ExtraRow::Tick(row(1)), pin);
    let aux = AuxPacketRow::from_header(
        AuxPacketKind::MarketStatus,
        13,
        0,
        1_779_951_600_111_000_000,
        2,
    )
    .expect("aux");
    b.push_extra(ExtraRow::Aux(aux), pin);
    let readonly = b.encode_readonly(Feed::Dhan, TickIlpEncoder::Library);
    let mut out = TickRowBuffer::new(TickIlpEncoder::Library);
    assert_eq!(b.encode_into(Feed::Dhan, &mut out), 2);
    assert_eq!(readonly, out.as_bytes());
    assert_eq!(b.len(), 2);
}

/// [`MAX_ILP_LINE_BYTES`] really bounds a row: the longest shape (a
/// `tick_quotes` row, every column, a 64-character name of 4-byte
/// characters, extreme integers, the longest `f64` text, every production
/// segment) and the longest aux row, in both encoders.
#[test]
fn test_max_ilp_line_bytes_covers_a_maximal_row() {
    let name: String = std::iter::repeat_n('\u{1F600}', 64).collect();
    let mut labels = CandleContractLabels::default();
    let mut longest = 0;
    for code in 0..=u8::MAX {
        let seg = tickvault_common::segment::segment_code_to_str(code);
        labels.insert((i64::MIN, seg), Arc::from(name.as_str()));
        let max = TickRow {
            security_id: i64::MIN,
            segment: seg,
            ltp: -2.225_073_858_507_201_4e-308,
            volume: i64::MIN,
            ts_ist_nanos: i64::MAX,
            capture_seq: i64::MIN,
            open: Some(-1.797_693_134_862_315_7e308),
            high: Some(-2.225_073_858_507_201_4e-308),
            low: Some(-1.797_693_134_862_315_7e308),
            close: Some(-2.225_073_858_507_201_4e-308),
            oi: Some(i64::MIN),
            oi_day_high: Some(i64::MIN),
            oi_day_low: Some(i64::MIN),
            avg_price: Some(-2.225_073_858_507_201_4e-308),
            last_trade_qty: Some(i64::MIN),
            total_buy_qty: Some(i64::MIN),
            total_sell_qty: Some(i64::MIN),
            received_at_ist_nanos: Some(i64::MAX),
            payload_hash: Some(i64::MIN),
        };
        let mut aux =
            AuxPacketRow::from_header(AuxPacketKind::Disconnect, 0, code, 1, 1).expect("aux");
        aux.security_id = i64::MIN;
        aux.ts_ist_nanos = i64::MAX;
        aux.received_at_ist_nanos = Some(i64::MAX);
        aux.capture_seq = i64::MIN;
        aux.oi = Some(i64::MIN);
        aux.prev_close = Some(-2.225_073_858_507_201_4e-308);
        aux.prev_oi = Some(i64::MIN);
        aux.ltp = Some(-2.225_073_858_507_201_4e-308);
        aux.exchange_ts_ist_nanos = Some(i64::MAX);
        aux.volume = Some(i64::MIN);
        aux.day_open = Some(-2.225_073_858_507_201_4e-308);
        aux.day_high = Some(-2.225_073_858_507_201_4e-308);
        aux.day_low = Some(-2.225_073_858_507_201_4e-308);
        aux.day_close = Some(-2.225_073_858_507_201_4e-308);
        aux.oi_day_high = Some(i64::MIN);
        aux.oi_day_low = Some(i64::MIN);
        aux.reason_code = Some(i64::MIN);
        for encoder in [TickIlpEncoder::Library, TickIlpEncoder::Own] {
            // Speed fix 6: the raw shapes the drain stages, at their extremes.
            let raw = crate::tick_persistence::raw::RawTick::from_parsed_tick(
                &ParsedTick {
                    security_id: i64::MAX as u64,
                    exchange_segment_code: code,
                    last_traded_price: f32::MIN,
                    last_trade_quantity: u16::MAX,
                    exchange_timestamp: u32::MAX,
                    received_at_nanos: i64::MAX,
                    average_traded_price: -f32::MIN_POSITIVE,
                    volume: u32::MAX,
                    total_sell_quantity: u32::MAX,
                    total_buy_quantity: u32::MAX,
                    day_open: f32::MIN,
                    day_close: -f32::MIN_POSITIVE,
                    day_high: f32::MIN,
                    day_low: -f32::MIN_POSITIVE,
                    open_interest: u32::MAX,
                    oi_day_high: u32::MAX,
                    oi_day_low: u32::MAX,
                    ..ParsedTick::default()
                },
                i64::MIN,
            )
            .expect("raw");
            labels.insert((i64::MAX, seg), Arc::from(name.as_str()));
            for staged in [
                Err(ExtraRow::Tick(max)),
                Err(ExtraRow::Quote {
                    row: max,
                    exchange_ts: Some(i64::MAX),
                }),
                Err(ExtraRow::Aux(aux)),
                Ok(StagedRow::Tick { raw, pin: 0 }),
                Ok(StagedRow::Quote { raw, pin: 0 }),
            ] {
                let mut b = StagedBatch::new();
                b.pins[0] = Some(Arc::new(labels.clone()));
                b.pin_count = 1;
                match staged {
                    Ok(row) => b.push(row),
                    Err(extra) => b.push_extra(extra, 0),
                };
                let mut out = TickRowBuffer::new(encoder);
                assert_eq!(
                    b.encode_into(Feed::Truedata, &mut out),
                    1,
                    "{encoder:?} {seg}"
                );
                assert!(
                    out.as_bytes()
                        .windows(4)
                        .any(|w| w == "\u{1F600}".as_bytes()),
                    "the maximal name is written"
                );
                longest = longest.max(out.len());
            }
        }
    }
    assert!(
        longest <= MAX_ILP_LINE_BYTES,
        "a maximal row is {longest} bytes, over MAX_ILP_LINE_BYTES"
    );
}

#[test]
fn test_len_is_empty_and_capacity_track_the_rows_and_reserve_presize_only_grows() {
    let mut b = StagedBatch::new();
    assert!(b.is_empty());
    assert_eq!(b.len(), 0);
    assert!(b.reserve_presize(), "an empty batch reserves");
    assert!(b.capacity() >= TICK_STAGED_ROWS_PRESIZE);
    let cap = b.capacity();
    b.push(StagedRow::Refused { capture_seq: 1 });
    assert_eq!(b.len(), 1);
    assert!(!b.is_empty());
    assert!(
        !b.reserve_presize(),
        "a pre-sized batch does not reserve again"
    );
    assert_eq!(
        b.capacity(),
        cap,
        "a batch already that big is left as it is"
    );
    b.clear();
    assert!(b.is_empty());
    assert_eq!(b.capacity(), cap, "clear keeps the capacity");
}

/// The rescue thread's encode counts a refused row but leaves the batch
/// unmarked (it is dropped right after).
#[test]
fn test_encode_counted_counts_a_refused_row_without_marking_it() {
    let _g = lock();
    publish_candle_contract_labels(CandleContractLabels::default());
    let mut b = StagedBatch::presized();
    let pin = b.try_pin(candle_contract_labels()).expect("pin");
    let mut bad = row(2);
    bad.ts_ist_nanos = -1;
    b.push_extra(ExtraRow::Tick(row(1)), pin);
    b.push_extra(ExtraRow::Tick(bad), pin);
    let before = encode_failures_so_far();
    let mut out = TickRowBuffer::new(TickIlpEncoder::Own);
    assert_eq!(b.encode_counted(Feed::Dhan, &mut out), 1);
    assert!(encode_failures_so_far() > before);
    assert_eq!(b.refused(), 0, "not marked");
    assert!(
        matches!(b.extras.get(1), Some(ExtraRow::Tick(_))),
        "not marked"
    );
}

/// The two pinned-table writers write the row through the marker and read
/// the table they are given, never the global one.
#[test]
fn test_append_within_marker_write_tick_line_with_labels_and_append_aux_row_with_labels() {
    let mut given = CandleContractLabels::default();
    given.insert((7_900_000_001, "NSE_FNO"), Arc::from("GIVEN-NAME"));
    let mut out = TickRowBuffer::new(TickIlpEncoder::Library);
    crate::tick_persistence::append_within_marker(&mut out, |b| {
        crate::tick_persistence::write_tick_line_with_labels(
            b,
            TICKS_TABLE,
            "dhan",
            &row(5),
            None,
            &given,
        )
    })
    .expect("tick line");
    let mut aux = AuxPacketRow::from_header(AuxPacketKind::OpenInterest, 7_900_000_001, 2, 1, 6)
        .expect("aux");
    aux.ts_ist_nanos = 1_793_268_000_000_000_000;
    if let TickRowBuffer::Library(b) = &mut out {
        crate::feed_aux_persistence::append_aux_row_with_labels(b, "dhan", &aux, &given)
            .expect("aux line");
    }
    let text = String::from_utf8(out.as_bytes().to_vec()).expect("utf8");
    assert_eq!(text.matches("contract=GIVEN-NAME").count(), 2, "{text}");
    let mut bad = row(7);
    bad.ts_ist_nanos = -1;
    let before = out.len();
    assert!(
        crate::tick_persistence::append_within_marker(&mut out, |b| {
            crate::tick_persistence::write_tick_line_with_labels(
                b,
                TICKS_TABLE,
                "dhan",
                &bad,
                None,
                &given,
            )
        })
        .is_err()
    );
    assert_eq!(out.len(), before, "the marker rewound the refused row");
}

/// The test helpers themselves: `with_capacity_for_test`, `as_ptr_for_test`
/// and `encode_failures_so_far`.
#[test]
fn test_helpers_with_capacity_for_test_as_ptr_for_test_and_encode_failures_so_far() {
    let b = StagedBatch::with_capacity_for_test(10);
    assert!(b.capacity() >= 10);
    assert!(!b.as_ptr_for_test().is_null());
    let _seen = encode_failures_so_far();
}

/// `FlushBatch::encode_ilp_for_test` writes the batch's text and changes
/// nothing (the app's tests read batches through it).
#[test]
fn test_encode_ilp_for_test_writes_the_batch_text() {
    let (mut w, sink, rx) =
        crate::tick_persistence::TickWriter::for_test(Feed::Dhan).split_for_offload();
    drop(sink);
    w.append_row(&row(9)).expect("stage");
    w.flush().expect("hand off");
    let batch = rx.try_recv().expect("queued");
    let text = batch.encode_ilp_for_test(Feed::Dhan);
    assert!(text.starts_with(b"ticks,"));
    assert_eq!(batch.encode_ilp_for_test(Feed::Dhan), text, "read-only");
}

/// `FlushBatch::encode_ilp_with_for_test` writes the batch in the encoder it
/// is given; both encoders write the same bytes (review round 1: the golden
/// test now writes each run in its own encoder through this).
#[test]
fn test_encode_ilp_with_for_test_writes_both_encoders_identically() {
    let (mut w, sink, rx) =
        crate::tick_persistence::TickWriter::for_test(Feed::Dhan).split_for_offload();
    drop(sink);
    w.append_row(&row(10)).expect("stage");
    w.flush().expect("hand off");
    let batch = rx.try_recv().expect("queued");
    let library = batch.encode_ilp_with_for_test(Feed::Dhan, TickIlpEncoder::Library);
    let own = batch.encode_ilp_with_for_test(Feed::Dhan, TickIlpEncoder::Own);
    assert!(library.starts_with(b"ticks,"));
    assert_eq!(library, own);
    assert_eq!(batch.encode_ilp_for_test(Feed::Dhan), library);
}

/// `clear_rows` empties the rows (and the refused count) but keeps the pins
/// and the capacity; `clear` drops the pins too (review round 1).
#[test]
fn test_clear_rows_keeps_pins_and_capacity_and_pin_count_tracks_them() {
    let _g = lock();
    publish_candle_contract_labels(CandleContractLabels::default());
    let mut b = StagedBatch::presized();
    let pin = b.try_pin(candle_contract_labels()).expect("pin");
    b.push_extra(ExtraRow::Tick(row(1)), pin);
    let cap = b.capacity();
    b.clear_rows();
    assert!(b.is_empty());
    assert_eq!(b.refused(), 0);
    assert_eq!(b.pin_count(), 1, "pins kept");
    assert_eq!(b.capacity(), cap);
    b.clear();
    assert_eq!(b.pin_count(), 0, "pins dropped");
    assert_eq!(b.capacity(), cap);
}

/// B1 review 2 (2026-11-13), per table since B1 review 3: `encode_tables_into`
/// and `has_rows_for` split a batch by table. The three parts together are
/// the whole batch's text and each table's rows land only in its own part, so
/// the writer thread can post the ticks while `tick_quotes` or
/// `feed_aux_packets` alone is not keyed.
#[test]
fn test_encode_tables_into_and_has_rows_for_split_a_batch_by_table() {
    let _g = lock();
    publish_candle_contract_labels(CandleContractLabels::default());
    let mut b = StagedBatch::presized();
    let pin = b.try_pin(candle_contract_labels()).expect("pin");
    b.push_extra(ExtraRow::Tick(row(1)), pin);
    assert!(b.has_rows_for(GatedTable::Ticks));
    assert!(!b.has_rows_for(GatedTable::FeedAuxPackets), "ticks only");
    assert!(!b.has_rows_for(GatedTable::TickQuotes), "ticks only");
    let aux = AuxPacketRow::from_header(
        AuxPacketKind::MarketStatus,
        13,
        0,
        1_779_951_600_111_000_000,
        2,
    )
    .expect("aux");
    b.push_extra(ExtraRow::Aux(aux), pin);
    b.push_extra(ExtraRow::Tick(row(3)), pin);
    b.push_extra(
        ExtraRow::Quote {
            row: row(4),
            exchange_ts: Some(1_793_268_000_000_000_000),
        },
        pin,
    );
    assert!(b.has_rows_for(GatedTable::FeedAuxPackets));
    assert!(b.has_rows_for(GatedTable::TickQuotes));
    let mut parts = 0;
    let mut bytes = 0;
    for (table, rows, prefix) in [
        (GatedTable::Ticks, 2, "ticks,"),
        (GatedTable::TickQuotes, 1, "tick_quotes,"),
        (GatedTable::FeedAuxPackets, 1, "feed_aux_packets,"),
    ] {
        let mut out = TickRowBuffer::new(TickIlpEncoder::Library);
        assert_eq!(
            b.encode_tables_into(Feed::Dhan, &mut out, |t| t == table),
            rows
        );
        let text = String::from_utf8_lossy(out.as_bytes()).to_string();
        assert!(
            text.lines().all(|l| l.starts_with(prefix)),
            "{table:?}: {text}"
        );
        parts += rows;
        bytes += out.as_bytes().len();
    }
    let mut whole = TickRowBuffer::new(TickIlpEncoder::Library);
    assert_eq!(b.encode_into(Feed::Dhan, &mut whole), parts);
    assert_eq!(whole.as_bytes().len(), bytes);
}

// ---- Speed fix 5 (2026-11-17): the generation-checked pin ----

fn publish_numbered(n: u64) {
    let mut labels = CandleContractLabels::default();
    labels.insert((1, "NSE_FNO"), Arc::from(format!("T{n}").as_str()));
    publish_candle_contract_labels(labels);
}

/// The pin `TickWriter::stage_row` takes: the generation check, else a load.
fn generation_pin(batch: &mut StagedBatch) -> Option<u8> {
    let g = crate::candle_contract_labels::candle_contract_labels_generation();
    match batch.current_pin_at_generation(g) {
        Some(pin) => Some(pin),
        None => batch
            .try_pin_at_generation(candle_contract_labels(), g)
            .ok(),
    }
}

#[test]
fn test_tick_try_pin_at_generation_records_the_generation_only_on_success() {
    let _g = lock();
    let mut batch = StagedBatch::new();
    for n in 0..TICK_STAGE_LABEL_PINS as u64 {
        publish_numbered(n);
        let g = crate::candle_contract_labels::candle_contract_labels_generation();
        let pin = batch
            .try_pin_at_generation(candle_contract_labels(), g)
            .expect("free slot");
        assert_eq!(usize::from(pin), n as usize);
        assert_eq!(batch.current_pin_at_generation(g), Some(pin));
    }
    publish_numbered(99);
    let g = crate::candle_contract_labels::candle_contract_labels_generation();
    assert!(
        batch
            .try_pin_at_generation(candle_contract_labels(), g)
            .is_err()
    );
    assert_eq!(batch.current_pin_at_generation(g), None, "nothing recorded");
    publish_candle_contract_labels(CandleContractLabels::default());
}

#[test]
fn test_tick_current_pin_at_generation_answers_only_the_last_pin_of_that_generation() {
    let _g = lock();
    publish_numbered(0);
    let mut batch = StagedBatch::new();
    let g = crate::candle_contract_labels::candle_contract_labels_generation();
    assert_eq!(batch.current_pin_at_generation(g), None);
    assert_eq!(generation_pin(&mut batch), Some(0));
    assert_eq!(batch.current_pin_at_generation(g), Some(0));
    assert_eq!(batch.current_pin_at_generation(g + 1), None);
    batch.clear_rows();
    assert_eq!(batch.current_pin_at_generation(g), Some(0), "pins kept");
    publish_numbered(1);
    batch.try_pin(candle_contract_labels()).expect("pin");
    assert_eq!(
        batch.current_pin_at_generation(g + 1),
        None,
        "try_pin never records"
    );
    batch.clear();
    assert_eq!(batch.current_pin_at_generation(g), None, "clear forgets");
    publish_candle_contract_labels(CandleContractLabels::default());
}

#[derive(Debug, Clone)]
enum PinOp {
    Publish,
    Pin,
    ClearRows,
    Clear,
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig { cases: 512, ..proptest::prelude::ProptestConfig::default() })]

    /// Speed fix 5: across random publishes (a ninth distinct table
    /// included) and clears, the generation-checked pin gives the SAME pin
    /// index and the SAME table as a fresh load plus `try_pin` every time.
    #[test]
    fn proptest_fix5_tick_generation_pin_equals_a_fresh_load_across_publishes(
        ops in proptest::collection::vec(
            proptest::prop_oneof![
                4 => proptest::strategy::Just(PinOp::Publish),
                8 => proptest::strategy::Just(PinOp::Pin),
                1 => proptest::strategy::Just(PinOp::ClearRows),
                1 => proptest::strategy::Just(PinOp::Clear),
            ],
            1..120,
        )
    ) {
        let _g = lock();
        let mut published = 0_u64;
        publish_numbered(published);
        let mut fast = StagedBatch::new();
        let mut reference = StagedBatch::new();
        for op in &ops {
            match op {
                PinOp::Publish => {
                    published += 1;
                    publish_numbered(published);
                }
                PinOp::Pin => {
                    let got = generation_pin(&mut fast);
                    let want = reference.try_pin(candle_contract_labels()).ok();
                    proptest::prop_assert_eq!(got, want);
                    match want {
                        Some(pin) => proptest::prop_assert!(std::ptr::eq(
                            fast.labels(pin),
                            reference.labels(pin)
                        )),
                        None => {
                            fast.clear();
                            reference.clear();
                        }
                    }
                }
                PinOp::ClearRows => {
                    fast.clear_rows();
                    reference.clear_rows();
                }
                PinOp::Clear => {
                    fast.clear();
                    reference.clear();
                }
            }
            proptest::prop_assert_eq!(fast.pin_count(), reference.pin_count());
        }
        publish_candle_contract_labels(CandleContractLabels::default());
    }
}

/// Speed fix 6: consecutive side rows of one table and pin are ONE staged
/// run; a different table or pin starts a new run; `len` counts every row;
/// `push_extra` reports growth like `push`; a clear empties the side rows.
#[test]
fn test_push_extra_runs_side_rows_and_extra_row_table_and_capture_seq() {
    let aux = AuxPacketRow::from_header(AuxPacketKind::MarketStatus, 13, 0, 1, 42).expect("aux");
    assert_eq!(ExtraRow::Aux(aux).table(), Some(GatedTable::FeedAuxPackets));
    assert_eq!(ExtraRow::Aux(aux).capture_seq(), 42);
    assert_eq!(ExtraRow::Tick(row(5)).table(), Some(GatedTable::Ticks));
    let quote = ExtraRow::Quote {
        row: row(6),
        exchange_ts: None,
    };
    assert_eq!(quote.table(), Some(GatedTable::TickQuotes));
    assert_eq!(quote.capture_seq(), 6);
    let refused = ExtraRow::Refused { capture_seq: 7 };
    assert_eq!(refused.table(), None);
    assert_eq!(refused.capture_seq(), 7);

    let mut b = StagedBatch::new();
    assert!(b.push_extra(ExtraRow::Aux(aux), 0), "zero capacity grows");
    let mut b = StagedBatch::presized();
    assert!(
        !b.push_extra(refused, 0),
        "a refused side row is never staged"
    );
    assert!(b.is_empty());
    for (extra, pin) in [
        (ExtraRow::Aux(aux), 0),
        (ExtraRow::Aux(aux), 0),
        (quote, 0),
        (ExtraRow::Aux(aux), 1),
        (ExtraRow::Aux(aux), 1),
    ] {
        assert!(!b.push_extra(extra, pin), "a pre-sized batch takes it");
    }
    assert_eq!(b.len(), 5, "every side row counts as a staged row");
    assert_eq!(b.extras.len(), 5);
    assert_eq!(b.rows.len(), 3, "three runs: aux pin 0, quote, aux pin 1");
    assert!(matches!(
        b.rows.as_slice(),
        [
            StagedRow::Extra {
                first: 0,
                len: 2,
                pin: 0,
                table: GatedTable::FeedAuxPackets
            },
            StagedRow::Extra {
                first: 2,
                len: 1,
                table: GatedTable::TickQuotes,
                ..
            },
            StagedRow::Extra {
                first: 3,
                len: 2,
                pin: 1,
                ..
            },
        ]
    ));
    assert_eq!(
        b.rows.get(1).and_then(StagedRow::table),
        Some(GatedTable::TickQuotes)
    );
    b.clear_rows();
    assert!(
        b.extras.is_empty() && b.is_empty(),
        "clear_rows empties both"
    );
    assert!(b.extras.capacity() >= TICK_STAGED_ROWS_PRESIZE);
    assert!(STAGED_ROW_BYTES <= 112, "{STAGED_ROW_BYTES}");
    assert_eq!(STAGED_SLOT_BYTES, STAGED_ROW_BYTES + STAGED_EXTRA_ROW_BYTES);
}
