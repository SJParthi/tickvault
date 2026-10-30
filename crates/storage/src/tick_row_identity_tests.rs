//! Old-vs-new identity tests for the tick row build (plan ITEM DT,
//! 2026-10-30). The row and its ILP bytes must not change by one bit: the
//! price conversion ([`super::tick_price_f64`]), the segment symbol check
//! ([`super::ilp_segment_symbol`]) and the line writer
//! ([`super::write_tick_line_into`]) are each compared with the code they
//! replaced, kept here verbatim as the oracle.

use super::*;
use proptest::prelude::*;
use questdb::ingress::{Buffer, ProtocolVersion, TimestampNanos};

/// Ids no other test publishes a contract name for, so the global name table
/// (which another test may replace at any moment) never names these rows and
/// both writers omit `contract` identically.
const RESERVED_ID_BASE: u64 = 7_700_000_000;

/// 2026-10-28 00:00 IST, IST epoch seconds.
const DAY_IST_SECS: u32 = 1_793_145_600;

/// The conversion every price column used before 2026-10-30.
fn legacy_price(v: f32) -> f64 {
    round_to_2dp(f32_to_f64_clean(v))
}

fn assert_same_price(v: f32) {
    assert_eq!(
        tick_price_f64(v).to_bits(),
        legacy_price(v).to_bits(),
        "price {v:?} (bits {:#010x})",
        v.to_bits()
    );
}

/// The row construction before 2026-10-30, prices through `legacy_price`.
/// Every other field is the same expression as `TickRow::from_parsed_tick`.
fn legacy_row(tick: &ParsedTick, capture_seq: i64) -> Option<TickRow> {
    let security_id = i64::try_from(tick.security_id).ok()?;
    if !tick.last_traded_price.is_finite() {
        return None;
    }
    let opt_price = |v: f32| {
        if !v.is_finite() {
            return None;
        }
        (v != 0.0).then(|| legacy_price(v))
    };
    let opt_qty = |v: u32| (v != 0).then(|| i64::from(v));
    let received_at_ist_nanos = (tick.received_at_nanos != 0).then_some(
        tick.received_at_nanos
            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
    );
    Some(TickRow {
        security_id,
        segment: segment_code_to_str(tick.exchange_segment_code),
        ltp: legacy_price(tick.last_traded_price),
        volume: i64::from(tick.volume),
        ts_ist_nanos: row_timestamp_ist_nanos(tick.exchange_timestamp, received_at_ist_nanos),
        capture_seq,
        open: opt_price(tick.day_open),
        high: opt_price(tick.day_high),
        low: opt_price(tick.day_low),
        close: opt_price(tick.day_close),
        oi: opt_qty(tick.open_interest),
        oi_day_high: opt_qty(tick.oi_day_high),
        oi_day_low: opt_qty(tick.oi_day_low),
        avg_price: opt_price(tick.average_traded_price),
        last_trade_qty: (tick.last_trade_quantity != 0)
            .then(|| i64::from(tick.last_trade_quantity)),
        total_buy_qty: opt_qty(tick.total_buy_quantity),
        total_sell_qty: opt_qty(tick.total_sell_quantity),
        received_at_ist_nanos,
        payload_hash: None,
    })
}

/// Bit-level row equality (`f64 ==` would call two equal NaNs different and
/// `0.0` equal to `-0.0`).
fn assert_same_row(new: &TickRow, old: &TickRow) {
    let bits = |v: Option<f64>| v.map(f64::to_bits);
    assert_eq!(new.ltp.to_bits(), old.ltp.to_bits(), "ltp");
    assert_eq!(bits(new.open), bits(old.open), "open");
    assert_eq!(bits(new.high), bits(old.high), "high");
    assert_eq!(bits(new.low), bits(old.low), "low");
    assert_eq!(bits(new.close), bits(old.close), "close");
    assert_eq!(bits(new.avg_price), bits(old.avg_price), "avg_price");
    let strip = |r: &TickRow| TickRow {
        ltp: 0.0,
        open: None,
        high: None,
        low: None,
        close: None,
        avg_price: None,
        ..*r
    };
    assert_eq!(strip(new), strip(old), "non-price fields");
}

/// The line writer before 2026-10-30, verbatim except its name and
/// indentation: `sanitize_ilp_symbol` on the segment and an `anyhow` context
/// on every column.
mod legacy {
    use super::*;

    /// The column-name helper the legacy writer used (deleted from the writer
    /// 2026-10-30, kept here so the oracle stays verbatim).
    fn tick_ilp_column(name: &'static str) -> questdb::ingress::ColumnName<'static> {
        questdb::ingress::ColumnName::new_unchecked(name)
    }

    pub(super) fn legacy_write_tick_line_into(
        buffer: &mut Buffer,
        table: &'static str,
        feed: &'static str,
        row: &TickRow,
        exchange_ts_ist_nanos: Option<i64>,
    ) -> Result<()> {
        debug_assert!(tickvault_common::sanitize::ilp_symbol_is_clean(feed));
        buffer
            .table(questdb::ingress::TableName::new_unchecked(table))
            .context("table")?
            .symbol(
                tick_ilp_column("segment"),
                sanitize_ilp_symbol(row.segment).as_ref(),
            )
            .context("segment")?
            .symbol(tick_ilp_column("feed"), feed)
            .context("feed")?;
        // `contract` — the option's NAME (`NIFTY-25Sep2026-24500-CE`), from the
        // SAME table the candle writer reads (operator 2026-09-22: "put either
        // contract or symbol name right in each and every table"). One ArcSwap
        // load + one hash probe per row, zero allocation: the name is borrowed
        // out of the published snapshot, already made ILP-safe once at publish
        // (`candle_contract_labels::sanitize_labels`, J1 2026-10-26; before that
        // every row re-ran `sanitize_ilp_symbol`, a `chars()` scan per tick).
        // Debug builds re-check it per row. Written only when the table knows this exact
        // `(security_id, segment)`; otherwise OMITTED, so the cell reads NULL —
        // never a guessed name. It must sit among the symbols: ILP rejects a
        // symbol after the first column.
        let labels = crate::candle_contract_labels::candle_contract_labels();
        if let Some(name) = labels.get(&(row.security_id, row.segment)) {
            debug_assert!(crate::candle_contract_labels::label_is_ilp_clean(name));
            buffer
                .symbol(tick_ilp_column("contract"), &**name)
                .context("contract")?;
        }
        buffer
            .column_i64(tick_ilp_column("security_id"), row.security_id)
            .context("security_id")?
            .column_f64(tick_ilp_column("ltp"), row.ltp)
            .context("ltp")?;

        // Per-feed OPTIONAL columns — omitted (NULL) when None.
        if let Some(v) = row.open {
            buffer
                .column_f64(tick_ilp_column("open"), v)
                .context("open")?;
        }
        if let Some(v) = row.high {
            buffer
                .column_f64(tick_ilp_column("high"), v)
                .context("high")?;
        }
        if let Some(v) = row.low {
            buffer
                .column_f64(tick_ilp_column("low"), v)
                .context("low")?;
        }
        if let Some(v) = row.close {
            buffer
                .column_f64(tick_ilp_column("close"), v)
                .context("close")?;
        }
        buffer
            .column_i64(tick_ilp_column("volume"), row.volume)
            .context("volume")?;
        if let Some(v) = row.oi {
            buffer.column_i64(tick_ilp_column("oi"), v).context("oi")?;
        }
        if let Some(v) = row.oi_day_high {
            buffer
                .column_i64(tick_ilp_column("oi_day_high"), v)
                .context("oi_day_high")?;
        }
        if let Some(v) = row.oi_day_low {
            buffer
                .column_i64(tick_ilp_column("oi_day_low"), v)
                .context("oi_day_low")?;
        }
        if let Some(v) = row.avg_price {
            buffer
                .column_f64(tick_ilp_column("avg_price"), v)
                .context("avg_price")?;
        }
        if let Some(v) = row.last_trade_qty {
            buffer
                .column_i64(tick_ilp_column("last_trade_qty"), v)
                .context("last_trade_qty")?;
        }
        if let Some(v) = row.total_buy_qty {
            buffer
                .column_i64(tick_ilp_column("total_buy_qty"), v)
                .context("total_buy_qty")?;
        }
        if let Some(v) = row.total_sell_qty {
            buffer
                .column_i64(tick_ilp_column("total_sell_qty"), v)
                .context("total_sell_qty")?;
        }
        if let Some(v) = row.received_at_ist_nanos {
            buffer
                .column_ts(tick_ilp_column("received_at"), TimestampNanos::new(v))
                .context("received_at")?;
        }
        if let Some(v) = row.payload_hash {
            buffer
                .column_i64(tick_ilp_column("payload_hash"), v)
                .context("payload_hash")?;
        }
        if let Some(v) = exchange_ts_ist_nanos {
            buffer
                .column_ts(
                    tick_ilp_column("exchange_timestamp"),
                    TimestampNanos::new(v),
                )
                .context("exchange_timestamp")?;
        }

        // capture_seq is the LAST column before the designated timestamp.
        buffer
            .column_i64(tick_ilp_column("capture_seq"), row.capture_seq)
            .context("capture_seq")?
            .at(TimestampNanos::new(row.ts_ist_nanos))
            .context("designated timestamp")?;
        Ok(())
    }
}

fn new_line(table: &'static str, row: &TickRow, exchange_ts: Option<i64>) -> Vec<u8> {
    encoder_line(TickIlpEncoder::Library, table, row, exchange_ts)
}

/// The same line through our own encoder (`[tick_storage] own_ilp_encoder`).
fn own_line(table: &'static str, row: &TickRow, exchange_ts: Option<i64>) -> Vec<u8> {
    encoder_line(TickIlpEncoder::Own, table, row, exchange_ts)
}

fn encoder_line(
    encoder: TickIlpEncoder,
    table: &'static str,
    row: &TickRow,
    exchange_ts: Option<i64>,
) -> Vec<u8> {
    let mut buffer = TickRowBuffer::new(encoder);
    let result = write_tick_line_into(&mut buffer, table, "dhan", row, exchange_ts);
    assert!(
        result.is_ok(),
        "{encoder:?} writer refused a line: {result:?}"
    );
    buffer.as_bytes().to_vec()
}

fn legacy_line(table: &'static str, row: &TickRow, exchange_ts: Option<i64>) -> Vec<u8> {
    let mut buffer = Buffer::new(ProtocolVersion::V1);
    let result = legacy::legacy_write_tick_line_into(&mut buffer, table, "dhan", row, exchange_ts);
    assert!(result.is_ok(), "legacy writer refused a line: {result:?}");
    buffer.as_bytes().to_vec()
}

/// Segments the line writer may be handed: every real one plus hostile text
/// (`TickRow::segment` is a public field).
const SEGMENTS: &[&str] = &[
    "IDX_I",
    "NSE_EQ",
    "NSE_FNO",
    "NSE_CURRENCY",
    "BSE_EQ",
    "MCX_COMM",
    "BSE_CURRENCY",
    "BSE_FNO",
    "UNKNOWN",
    "IDX,I",
    "IDX=I",
    "IDX I",
    "IDX\nI",
    "IDX\rI",
    "a\\b",
    "\t",
    "\u{7f}",
    "\u{202e}NSE",
    "\u{feff}",
    "Ω_EQ",
    "",
    "~!@#$%^&*()",
];

fn any_price_bits() -> impl Strategy<Value = f32> {
    prop_oneof![
        any::<u32>().prop_map(f32::from_bits),
        // Price-like: whole paise and tick sizes, both signs, every magnitude
        // band up to and past the fast-path limit.
        (-20_000_000_i64..20_000_000).prop_map(|cents| cents as f32 / 100.0),
        (-200_000_i64..200_000).prop_map(|nickels| nickels as f32 * 0.05),
        Just(0.0_f32),
        Just(-0.0_f32),
        Just(f32::NAN),
        Just(f32::INFINITY),
        Just(f32::NEG_INFINITY),
        Just(f32::MAX),
        Just(f32::MIN),
        Just(f32::MIN_POSITIVE),
        Just(131_072.0_f32),
        Just(131_071.99_f32),
        Just(-131_071.99_f32),
    ]
}

fn any_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        any::<u64>().prop_map(f64::from_bits),
        any_price_bits().prop_map(legacy_price),
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(f64::MAX),
        Just(-0.0),
    ]
}

prop_compose! {
    fn any_row()(
        id in 0_i64..i64::MAX,
        seg in 0..SEGMENTS.len(),
        ltp in any_f64(),
        volume in any::<i64>(),
        ts in 0_i64..i64::MAX,
        capture_seq in any::<i64>(),
        prices in proptest::collection::vec(proptest::option::of(any_f64()), 5),
        qtys in proptest::collection::vec(proptest::option::of(any::<i64>()), 6),
        received in proptest::option::of(0_i64..i64::MAX),
        hash in proptest::option::of(any::<i64>()),
    ) -> TickRow {
        // Reserved ids only, so no published contract name applies.
        let security_id = (RESERVED_ID_BASE as i64).saturating_add(id % 1_000_000);
        let segment: &'static str = SEGMENTS[seg];
        TickRow {
            security_id,
            segment,
            ltp,
            volume,
            ts_ist_nanos: ts,
            capture_seq,
            open: prices[0],
            high: prices[1],
            low: prices[2],
            close: prices[3],
            avg_price: prices[4],
            oi: qtys[0],
            oi_day_high: qtys[1],
            oi_day_low: qtys[2],
            last_trade_qty: qtys[3],
            total_buy_qty: qtys[4],
            total_sell_qty: qtys[5],
            received_at_ist_nanos: received,
            payload_hash: hash,
        }
    }
}

prop_compose! {
    /// A tick inside the 09:00-15:40 IST session (most), or anywhere.
    fn any_tick()(
        id in 0_u64..1_000_000,
        seg in prop_oneof![Just(0_u8), Just(1), Just(2), Just(3), Just(4), Just(5), Just(7), Just(8), any::<u8>()],
        in_session in proptest::bool::weighted(0.85),
        secs_into_session in 0_u32..23_400,
        any_ltt in any::<u32>(),
        lag_nanos in 0_i64..30_000_000_000,
        no_receipt in proptest::bool::weighted(0.05),
        p in proptest::collection::vec(any_price_bits(), 6),
        q in proptest::collection::vec(prop_oneof![Just(0_u32), any::<u32>(), Just(u32::MAX)], 7),
        ltq in any::<u16>(),
    ) -> ParsedTick {
        let ltt = if in_session { DAY_IST_SECS + 33_300 + secs_into_session } else { any_ltt };
        let received = if no_receipt {
            0
        } else {
            (i64::from(ltt) - 19_800) * 1_000_000_000 + lag_nanos
        };
        ParsedTick {
            security_id: RESERVED_ID_BASE + id,
            exchange_segment_code: seg,
            last_traded_price: p[0],
            last_trade_quantity: ltq,
            exchange_timestamp: ltt,
            received_at_nanos: received,
            average_traded_price: p[1],
            volume: q[0],
            total_sell_quantity: q[1],
            total_buy_quantity: q[2],
            day_open: p[2],
            day_close: p[3],
            day_high: p[4],
            day_low: p[5],
            open_interest: q[3],
            oi_day_high: q[4],
            oi_day_low: q[5],
            ..ParsedTick::default()
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4_096))]

    #[test]
    fn tick_price_f64_is_bit_identical_to_the_legacy_conversion(v in any_price_bits()) {
        assert_same_price(v);
    }

    #[test]
    fn ilp_segment_symbol_matches_sanitize_ilp_symbol(s in any::<String>()) {
        let (new, old) = (ilp_segment_symbol(&s), sanitize_ilp_symbol(&s));
        prop_assert_eq!(new.as_ref(), old.as_ref());
    }

    #[test]
    fn row_from_any_tick_is_identical_to_the_legacy_row(tick in any_tick(), seq in any::<i64>()) {
        let new = TickRow::from_parsed_tick(&tick, seq).ok();
        let old = legacy_row(&tick, seq);
        prop_assert_eq!(new.is_some(), old.is_some(), "accept/refuse differs");
        if let (Some(new), Some(old)) = (new, old) {
            assert_same_row(&new, &old);
        }
    }

    #[test]
    fn ilp_line_bytes_are_identical_to_the_legacy_writer(
        row in any_row(),
        quotes in any::<bool>(),
        exchange_ts in proptest::option::of(0_i64..i64::MAX),
    ) {
        let (table, ts) = if quotes {
            (crate::tick_quotes_persistence::TICK_QUOTES_TABLE, exchange_ts)
        } else {
            (TICKS_TABLE, None)
        };
        let legacy = legacy_line(table, &row, ts);
        prop_assert_eq!(new_line(table, &row, ts), legacy.clone());
        prop_assert_eq!(own_line(table, &row, ts), legacy);
    }

    /// End to end through the writer: every accepted tick's bytes, `ticks` or
    /// `tick_quotes`, against the legacy row and legacy line.
    #[test]
    fn writer_bytes_are_identical_to_the_legacy_row_and_line(
        tick in any_tick(),
        seq in 1_i64..i64::MAX,
        quotes in any::<bool>(),
    ) {
        let mut writer = TickWriter::for_test(Feed::Dhan);
        let appended = if quotes {
            writer.append_tick_quote_with_seq(&tick, seq)
        } else {
            writer.append_tick_with_seq(&tick, seq)
        };
        let old = legacy_row(&tick, seq);
        prop_assert_eq!(appended.is_ok(), old.is_some(), "accept/refuse differs");
        // Our own encoder writes the same bytes for the same tick, whichever
        // row it became (`ticks`, `tick_quotes` or `feed_aux_packets`).
        let mut own = TickWriter::for_test_with_encoder(Feed::Dhan, TickIlpEncoder::Own);
        let own_appended = if quotes {
            own.append_tick_quote_with_seq(&tick, seq)
        } else {
            own.append_tick_with_seq(&tick, seq)
        };
        prop_assert_eq!(own_appended.is_ok(), appended.is_ok(), "own encoder accept differs");
        prop_assert_eq!(own.buffer.as_bytes(), writer.buffer.as_bytes(), "own encoder bytes differ");
        let Some(old) = old else { return Ok(()) };
        // Only a row the window and band gates accepted is a `ticks` /
        // `tick_quotes` line; a refused one is a `feed_aux_packets` row.
        let bytes = writer.buffer.as_bytes().to_vec();
        let aux = bytes.starts_with(crate::feed_aux_persistence::FEED_AUX_PACKETS_TABLE.as_bytes());
        if aux || bytes.is_empty() {
            return Ok(());
        }
        let expected = match (quotes, old.received_at_ist_nanos) {
            (true, Some(arrival)) => {
                let row = TickRow { ts_ist_nanos: arrival, ..old };
                let ltt = (tick.exchange_timestamp != 0)
                    .then(|| i64::from(tick.exchange_timestamp) * 1_000_000_000);
                legacy_line(crate::tick_quotes_persistence::TICK_QUOTES_TABLE, &row, ltt)
            }
            _ => legacy_line(TICKS_TABLE, &old, None),
        };
        prop_assert_eq!(bytes, expected);
    }
}

#[test]
fn tick_price_f64_edge_values_are_identical() {
    let edges = [
        0.0_f32,
        -0.0,
        f32::NAN,
        -f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::MAX,
        f32::MIN,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        f32::from_bits(1),
        f32::EPSILON,
        0.001,
        0.005,
        0.01,
        0.05,
        0.1,
        1.0,
        10.2,
        21_004.95,
        23_146.45,
        23_937.3,
        99_999.99,
        131_071.99,
        131_072.0,
        131_072.01,
        16_777_215.0,
        16_777_216.0,
        -245.35,
        -0.01,
    ];
    for v in edges {
        assert_same_price(v);
    }
}

/// Every `f32` with `|v| < 2^17` (the whole fast-path domain, both signs):
/// about 2.2 billion values. `#[ignore]`d: minutes on four cores, release.
/// Run: `cargo test -p tickvault-storage --release --lib
/// tick_price_f64_matches_legacy_for_every_fast_path_f32 -- --ignored`
#[test]
#[ignore = "exhaustive over 2^31 values; run by hand in release"]
fn tick_price_f64_matches_legacy_for_every_fast_path_f32() {
    let limit = TICK_PRICE_FAST_PATH_MAX_MAGNITUDE.to_bits();
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    let threads = u32::try_from(threads).unwrap_or(4).max(1);
    let chunk = limit / threads + 1;
    std::thread::scope(|scope| {
        for t in 0..threads {
            scope.spawn(move || {
                let start = t * chunk;
                let end = (start + chunk).min(limit);
                for bits in start..end {
                    for v in [f32::from_bits(bits), -f32::from_bits(bits)] {
                        assert_same_price(v);
                    }
                }
            });
        }
    });
}

#[test]
fn legacy_oracle_is_not_vacuous() {
    // The oracle must actually write lines and produce the published row
    // shape, or every comparison above would pass on two empty buffers.
    let tick = ParsedTick {
        security_id: RESERVED_ID_BASE + 1,
        exchange_segment_code: 2,
        last_traded_price: 245.35,
        exchange_timestamp: DAY_IST_SECS + 36_000,
        received_at_nanos: (i64::from(DAY_IST_SECS + 36_000) - 19_800) * 1_000_000_000,
        day_open: 238.05,
        volume: 1_250_000,
        ..ParsedTick::default()
    };
    let row = legacy_row(&tick, 9).expect("finite LTP");
    let line = String::from_utf8(legacy_line(TICKS_TABLE, &row, None)).expect("utf8");
    assert!(
        line.starts_with("ticks,segment=NSE_FNO,feed=dhan "),
        "{line}"
    );
    assert!(
        line.contains("ltp=245.35,open=238.05,volume=1250000i"),
        "{line}"
    );
    assert!(line.ends_with('\n'));
}

#[test]
fn test_new_with_encoder_own_has_our_sender_and_buffer() {
    let config = tickvault_common::config::QuestDbConfig {
        host: "127.0.0.1".to_owned(),
        http_port: 9,
        pg_port: 8812,
        ilp_port: 9009,
    };
    // Building a sender never dials, for either encoder.
    let own = TickWriter::new_with_encoder(&config, Feed::Dhan, TickIlpEncoder::Own);
    assert!(matches!(own.sender, Some(TickIlpSender::Own(_))));
    assert_eq!(own.buffer.kind(), TickRowBufferKind::Own);
    assert!(own.buffer.capacity() >= TICK_ROW_BUFFER_PRESIZE_BYTES);
    let lib = TickWriter::new(&config, Feed::Dhan);
    assert!(matches!(lib.sender, Some(TickIlpSender::Library(_))));
    assert_eq!(
        lib.buffer.kind(),
        TickRowBufferKind::Library(ProtocolVersion::V1)
    );
}

#[test]
fn test_for_test_with_encoder_is_disconnected_and_presized() {
    for encoder in [TickIlpEncoder::Library, TickIlpEncoder::Own] {
        let w = TickWriter::for_test_with_encoder(Feed::Dhan, encoder);
        assert!(w.sender.is_none());
        assert_eq!(w.buffer.kind(), TickRowBuffer::new(encoder).kind());
        assert!(w.buffer.capacity() >= TICK_ROW_BUFFER_PRESIZE_BYTES);
    }
}

/// The offload path recycles our buffers too, and never installs a spare of
/// the other encoder.
#[test]
fn test_own_encoder_offload_recycles_only_own_spares() {
    let (mut producer, sink, _rx) =
        TickWriter::for_test_with_encoder(Feed::Dhan, TickIlpEncoder::Own).split_for_offload();
    let mut foreign = TickRowBuffer::new(TickIlpEncoder::Library);
    foreign.reserve(4_096);
    assert!(
        sink.spare_return
            .as_ref()
            .is_some_and(|lane| lane.try_send(foreign).is_ok())
    );
    let tick = ParsedTick {
        security_id: RESERVED_ID_BASE + 3,
        exchange_segment_code: 2,
        last_traded_price: 245.35,
        exchange_timestamp: DAY_IST_SECS + 36_000,
        received_at_nanos: (i64::from(DAY_IST_SECS + 36_000) - 19_800) * 1_000_000_000,
        ..ParsedTick::default()
    };
    assert!(producer.append_tick_with_seq(&tick, 1).is_ok());
    assert!(producer.flush().is_ok());
    assert_eq!(producer.buffer.kind(), TickRowBufferKind::Own);
}
