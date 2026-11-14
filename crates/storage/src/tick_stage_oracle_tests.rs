//! PF-4 proptest oracle (plan item PF-4, 2026-11-06).
//!
//! [`LegacyAppender`] is the tick writer's append path as it stood BEFORE the
//! ILP text step moved off the frame drain: `append_tick_into`, `append_row`,
//! `append_quote_row`, `append_aux`, `persist_refused_tick` and
//! `note_pending_seq`, copied verbatim (only the struct they live on is
//! renamed; the writer state they touch is the same fields). It writes ILP
//! text on every append, reading the global contract-name table at that
//! moment, exactly as the drain did.
//!
//! The proptests drive it and the real (staging) writer with the same random
//! stream of ticks, `tick_quotes` routes, `feed_aux_packets` rows, unbacked
//! marks, name publishes and flushes, and require the same per-call results,
//! the same pending counts, sequence ranges and flags after every step, and
//! byte-identical ILP text for every batch, in both encoders. Inputs the
//! encoders could refuse (a negative designated timestamp) are left to the
//! dedicated backstop tests: by the owner's 2026-11-06 decision such a row is
//! counted on the encoding thread, not refused on the drain, so the oracle
//! and the new path legitimately disagree on it.

use super::tick_test_writers::{append_aux_row, write_tick_line, write_tick_line_into};
use super::*;
use crate::candle_contract_labels::{
    CandleContractLabels, TEST_PUBLISH_LOCK, publish_candle_contract_labels,
};
use proptest::prelude::*;
use std::sync::Arc;

/// The pre-PF-4 append path, verbatim.
struct LegacyAppender {
    buffer: TickRowBuffer,
    pending: usize,
    pending_min_seq: u64,
    pending_max_seq: u64,
    pending_unbacked: bool,
    feed: Feed,
    out_of_window: OutOfWindowCounters,
    last_capture_seq: i64,
    flush_counters: TickFlushCounters,
    aux_counters: [metrics::Counter; AuxPacketKind::ALL.len()],
    aux_rows_appended: u64,
    quote_rows_appended: u64,
}

impl LegacyAppender {
    fn new(feed: Feed, encoder: TickIlpEncoder) -> Self {
        Self {
            buffer: TickRowBuffer::new(encoder),
            pending: 0,
            pending_min_seq: 0,
            pending_max_seq: 0,
            pending_unbacked: false,
            feed,
            out_of_window: tick_out_of_window_counters(feed),
            last_capture_seq: 0,
            flush_counters: TickFlushCounters::new(feed),
            aux_counters: aux_row_counters(),
            aux_rows_appended: 0,
            quote_rows_appended: 0,
        }
    }

    pub fn mark_pending_unbacked(&mut self) {
        self.pending_unbacked = true;
    }

    pub fn append_tick_with_seq(&mut self, tick: &ParsedTick, capture_seq: i64) -> Result<()> {
        self.append_tick_into(tick, capture_seq, false)
    }

    pub fn append_tick_quote_with_seq(
        &mut self,
        tick: &ParsedTick,
        capture_seq: i64,
    ) -> Result<()> {
        self.append_tick_into(tick, capture_seq, true)
    }

    /// A successful hand-off, as `offload_flush`'s `Sent` arm left the
    /// producer: the batch's text, rows and range leave; the flag clears.
    fn take_batch(&mut self) -> Option<(Vec<u8>, usize, u64, u64, bool)> {
        if self.pending == 0 {
            return None;
        }
        let out = (
            self.buffer.as_bytes().to_vec(),
            self.pending,
            self.pending_min_seq,
            self.pending_max_seq,
            self.pending_unbacked,
        );
        self.buffer.clear();
        self.pending = 0;
        self.pending_min_seq = 0;
        self.pending_max_seq = 0;
        self.pending_unbacked = false;
        Some(out)
    }

    // ---- verbatim from tick_persistence.rs before PF-4 (comments trimmed) ----

    fn append_tick_into(
        &mut self,
        tick: &ParsedTick,
        capture_seq: i64,
        to_quotes: bool,
    ) -> Result<()> {
        let outcome = match TickRow::from_parsed_tick(tick, capture_seq) {
            Ok(row) => {
                {
                    use tickvault_trading::candles::multi_tf_aggregator::{
                        MAX_PLAUSIBLE_EXCHANGE_TS_SECS, MIN_PLAUSIBLE_EXCHANGE_TS_SECS,
                    };
                    let secs = row.ts_ist_nanos.div_euclid(1_000_000_000);
                    let in_band = u32::try_from(secs).is_ok_and(|s| {
                        (MIN_PLAUSIBLE_EXCHANGE_TS_SECS..=MAX_PLAUSIBLE_EXCHANGE_TS_SECS)
                            // O(1) EXEMPT: RangeInclusive::contains on a const range is two integer compares, not a scan
                            .contains(&s)
                    });
                    if !in_band {
                        self.out_of_window.note(TICK_TS_OUT_OF_BAND_REASON);
                        self.persist_refused_tick(tick, capture_seq);
                        return Ok(());
                    }
                }
                let verdict = tickvault_common::session_window::verdict(
                    row.ts_ist_nanos,
                    row.received_at_ist_nanos,
                );
                if verdict.is_noteworthy() {
                    self.out_of_window.note(verdict.reason());
                }
                if verdict.is_refusal() {
                    self.persist_refused_tick(tick, capture_seq);
                    return Ok(());
                }
                if let (true, Some(arrival)) = (to_quotes, row.received_at_ist_nanos) {
                    let appended = self.append_quote_row(&row, arrival, tick.exchange_timestamp);
                    if appended.is_ok() {
                        self.quote_rows_appended = self.quote_rows_appended.saturating_add(1);
                        crate::tick_quotes_persistence::note_row();
                        if self.feed == Feed::Dhan {
                            crate::tick_conservation_ledger::ledger().note_row_appended(arrival);
                        }
                    }
                    appended
                } else {
                    let appended = self.append_row(&row);
                    if appended.is_ok() && self.feed == Feed::Dhan {
                        crate::tick_conservation_ledger::ledger()
                            .note_row_appended(row.ts_ist_nanos);
                        let _placement = crate::tick_ts_placement::note_row(row.ts_ist_nanos);
                    }
                    appended
                }
            }
            Err(err) => Err(err.into()),
        };
        if let Err(err) = &outcome {
            crate::wal_applied_watermark::applied_watermark()
                .note_unapplied(u64::try_from(capture_seq).unwrap_or(0));
            self.flush_counters.append_dropped.increment(1);
            self.flush_counters.append_failures =
                self.flush_counters.append_failures.saturating_add(1);
            if self.flush_counters.append_failures.is_power_of_two() {
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    feed = self.feed.as_str(),
                    security_id = tick.security_id,
                    capture_seq,
                    source = "legacy_oracle",
                    failures_so_far = self.flush_counters.append_failures,
                    error = %err,
                    "legacy oracle append failed"
                );
            }
        }
        outcome
    }

    fn append_row(&mut self, row: &TickRow) -> Result<()> {
        let nonfinite = count_nonfinite_tick_floats(row);
        if nonfinite > 0 {
            metrics::counter!(TICK_NONFINITE_COLUMNS_COUNTER).increment(nonfinite);
        }
        let feed = self.feed.as_str();
        append_within_marker(&mut self.buffer, |buffer| {
            write_tick_line(buffer, feed, row)
        })?;
        self.pending = self.pending.saturating_add(1);
        self.last_capture_seq = self.last_capture_seq.max(row.capture_seq);
        self.note_pending_seq(row.capture_seq);
        Ok(())
    }

    fn append_quote_row(
        &mut self,
        row: &TickRow,
        arrival_ist_nanos: i64,
        ltt_ist_secs: u32,
    ) -> Result<()> {
        let nonfinite = count_nonfinite_tick_floats(row);
        if nonfinite > 0 {
            metrics::counter!(TICK_NONFINITE_COLUMNS_COUNTER).increment(nonfinite);
        }
        let quote = TickRow {
            ts_ist_nanos: arrival_ist_nanos,
            ..*row
        };
        let exchange_ts = (ltt_ist_secs != 0).then(|| i64::from(ltt_ist_secs) * 1_000_000_000);
        let feed = self.feed.as_str();
        append_within_marker(&mut self.buffer, |buffer| {
            write_tick_line_into(
                buffer,
                crate::tick_quotes_persistence::TICK_QUOTES_TABLE,
                feed,
                &quote,
                exchange_ts,
            )
        })?;
        self.pending = self.pending.saturating_add(1);
        self.last_capture_seq = self.last_capture_seq.max(row.capture_seq);
        self.note_pending_seq(row.capture_seq);
        Ok(())
    }

    pub fn append_aux(&mut self, row: &AuxPacketRow) -> Result<()> {
        let feed = self.feed.as_str();
        match append_within_marker(&mut self.buffer, |buffer| match buffer {
            TickRowBuffer::Library(b) => append_aux_row(b, feed, row),
            TickRowBuffer::Own(b) => append_aux_row(b, feed, row),
        }) {
            Ok(()) => {
                self.pending = self.pending.saturating_add(1);
                self.aux_rows_appended = self.aux_rows_appended.saturating_add(1);
                if let Some(c) = self.aux_counters.get(row.kind.index()) {
                    c.increment(1);
                }
                self.note_pending_seq(row.capture_seq);
                Ok(())
            }
            Err(err) => {
                crate::wal_applied_watermark::applied_watermark()
                    .note_unapplied(u64::try_from(row.capture_seq).unwrap_or(0));
                self.flush_counters.append_dropped.increment(1);
                Err(err)
            }
        }
    }

    fn persist_refused_tick(&mut self, tick: &ParsedTick, capture_seq: i64) {
        if let Some(row) =
            AuxPacketRow::from_tick(AuxPacketKind::OutOfWindowTick, tick, capture_seq)
        {
            let _handled = self.append_aux(&row).is_ok();
        }
    }

    fn note_pending_seq(&mut self, capture_seq: i64) {
        let seq = u64::try_from(capture_seq).unwrap_or(0);
        if seq == 0 {
            self.pending_unbacked = true;
            return;
        }
        if self.pending_min_seq == 0 || seq < self.pending_min_seq {
            self.pending_min_seq = seq;
        }
        if seq > self.pending_max_seq {
            self.pending_max_seq = seq;
        }
    }
}

// ---------------------------------------------------------------------------
// The random stream
// ---------------------------------------------------------------------------

/// Ids only this test names.
const ID_BASE: u64 = 7_950_000_000;

/// 2026-10-28 00:00 IST, IST epoch seconds.
const DAY_IST_SECS: u32 = 1_793_145_600;

#[derive(Debug, Clone)]
enum Op {
    Tick(ParsedTick, i64),
    Quote(ParsedTick, i64),
    Aux(u8, u64, u8, i64, i64, Option<u32>),
    Mark,
    Publish(u32, usize),
    Flush,
}

const NAMES: [&str; 5] = [
    "NIFTY-25Nov2026-24500-CE",
    "RELIANCE",
    "BAD,NAME=X",
    "SPACE NAME",
    "BANKNIFTY-27Nov2026-51200-PE",
];

fn price() -> impl Strategy<Value = f32> {
    prop_oneof![
        Just(f32::NAN),
        Just(f32::INFINITY),
        Just(f32::NEG_INFINITY),
        Just(0.0),
        Just(-0.0),
        Just(f32::MIN_POSITIVE),
        any::<f32>(),
        (0.05_f32..200_000.0),
        (1_u32..4_000_000).prop_map(|p| p as f32 * 0.05),
    ]
}

fn ltt() -> impl Strategy<Value = u32> {
    prop_oneof![
        Just(0_u32),
        Just(315_532_800_u32), // the 1980 sentinel
        Just(2_600_000_000_u32),
        Just(u32::MAX),
        (0_u32..86_400).prop_map(|s| DAY_IST_SECS + s),
        (32_400_u32..56_400).prop_map(|s| DAY_IST_SECS + s),
    ]
}

/// Receipts are never negative (the WAL maps an implausible one to 0).
fn receipt() -> impl Strategy<Value = i64> {
    prop_oneof![
        Just(0_i64),
        (0_i64..86_400).prop_map(|s| (i64::from(DAY_IST_SECS) - 19_800 + s) * 1_000_000_000),
        (0_i64..i64::MAX / 2),
    ]
}

fn parsed_tick() -> impl Strategy<Value = ParsedTick> {
    (
        (
            prop_oneof![
                (0_u64..16).prop_map(|i| ID_BASE + i),
                Just(u64::MAX),
                Just(1_u64 << 63)
            ],
            prop_oneof![Just(0_u8), Just(1), Just(2), Just(4), any::<u8>()],
            price(),
            any::<u16>(),
            ltt(),
            receipt(),
        ),
        (price(), any::<u32>(), any::<u32>(), any::<u32>()),
        (price(), price(), price(), price()),
        (any::<u32>(), any::<u32>(), any::<u32>()),
    )
        .prop_map(
            |(
                (id, seg, ltp, ltq, ltt, rcpt),
                (atp, vol, sell, buy),
                (o, c, h, l),
                (oi, oh, ol),
            )| {
                ParsedTick {
                    security_id: id,
                    exchange_segment_code: seg,
                    last_traded_price: ltp,
                    last_trade_quantity: ltq,
                    exchange_timestamp: ltt,
                    received_at_nanos: rcpt,
                    average_traded_price: atp,
                    volume: vol,
                    total_sell_quantity: sell,
                    total_buy_quantity: buy,
                    day_open: o,
                    day_close: c,
                    day_high: h,
                    day_low: l,
                    open_interest: oi,
                    oi_day_high: oh,
                    oi_day_low: ol,
                    ..ParsedTick::default()
                }
            },
        )
}

fn seq() -> impl Strategy<Value = i64> {
    prop_oneof![Just(0_i64), 1_i64..1_000_000, Just(i64::MAX), Just(-5_i64)]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        30 => (parsed_tick(), seq()).prop_map(|(t, s)| Op::Tick(t, s)),
        12 => (parsed_tick(), seq()).prop_map(|(t, s)| Op::Quote(t, s)),
        10 => (0_u8..4, (0_u64..16).prop_map(|i| ID_BASE + i), prop_oneof![Just(0_u8), Just(1), Just(2)],
               (0_i64..86_400).prop_map(|s| (i64::from(DAY_IST_SECS) - 19_800 + s) * 1_000_000_000),
               1_i64..1_000_000, proptest::option::of(any::<u32>()))
            .prop_map(|(k, id, seg, r, s, v)| Op::Aux(k, id, seg, r, s, v)),
        3 => Just(Op::Mark),
        3 => (any::<u32>(), 0_usize..NAMES.len()).prop_map(|(m, n)| Op::Publish(m, n)),
        8 => Just(Op::Flush),
    ]
}

fn aux_row(
    kind: u8,
    id: u64,
    seg: u8,
    receipt: i64,
    seq: i64,
    v: Option<u32>,
) -> Option<AuxPacketRow> {
    let kind = [
        AuxPacketKind::OpenInterest,
        AuxPacketKind::PrevClose,
        AuxPacketKind::MarketStatus,
        AuxPacketKind::Disconnect,
    ][usize::from(kind % 4)];
    let mut row = AuxPacketRow::from_header(kind, id, seg, receipt, seq)?;
    row.oi = v.map(i64::from);
    row.reason_code = v.map(|r| i64::from(r % 1_000));
    Some(row)
}

fn publish(mask: u32, name: usize) {
    let mut labels = CandleContractLabels::default();
    for i in 0..16_u32 {
        if mask & (1 << i) != 0 {
            let id = i64::try_from(ID_BASE + u64::from(i)).unwrap_or_default();
            let seg = segment_code_to_str((i % 3) as u8);
            labels.insert(
                (id, seg),
                Arc::from(NAMES[(name + i as usize) % NAMES.len()]),
            );
        }
    }
    publish_candle_contract_labels(labels);
}

type Snapshot = (usize, u64, u64, bool, u64, u64);

fn legacy_state(l: &LegacyAppender) -> Snapshot {
    (
        l.pending,
        l.pending_min_seq,
        l.pending_max_seq,
        l.pending_unbacked,
        l.quote_rows_appended,
        l.aux_rows_appended,
    )
}

fn new_state(w: &TickWriter) -> Snapshot {
    (
        w.pending,
        w.pending_min_seq,
        w.pending_max_seq,
        w.pending_unbacked,
        w.quote_rows_appended,
        w.aux_rows_appended,
    )
}

/// One case: the oracle and the staging writer, step by step.
fn run_case(encoder: TickIlpEncoder, ops: &[Op]) -> Result<(), TestCaseError> {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish_candle_contract_labels(CandleContractLabels::default());
    let mut legacy = LegacyAppender::new(Feed::Dhan, encoder);
    let (mut w, sink, rx) = TickWriter::for_test_with_encoder(Feed::Dhan, encoder)
        .with_spill_dir_for_test(std::env::temp_dir().join("tv-pf4-oracle"))
        .split_for_offload();
    drop(sink);
    let mut legacy_batches = Vec::new();
    let mut new_batches = Vec::new();
    // At most seven publishes per case: no batch ever needs a ninth name pin,
    // so batch edges match exactly (the pin cut has its own tests).
    let mut publishes = 0;
    for op in ops {
        match op {
            Op::Tick(t, s) => {
                let a = legacy.append_tick_with_seq(t, *s).is_ok();
                let b = w.append_tick_with_seq(t, *s).is_ok();
                prop_assert_eq!(a, b, "append result");
            }
            Op::Quote(t, s) => {
                let a = legacy.append_tick_quote_with_seq(t, *s).is_ok();
                let b = w.append_tick_quote_with_seq(t, *s).is_ok();
                prop_assert_eq!(a, b, "quote append result");
            }
            Op::Aux(k, id, seg, r, s, v) => {
                if let Some(row) = aux_row(*k, *id, *seg, *r, *s, *v) {
                    let a = legacy.append_aux(&row).is_ok();
                    let b = w.append_aux(&row).is_ok();
                    prop_assert_eq!(a, b, "aux append result");
                }
            }
            Op::Mark => {
                legacy.mark_pending_unbacked();
                w.mark_pending_unbacked();
            }
            Op::Publish(m, n) => {
                if publishes < 7 {
                    publish(*m, *n);
                    publishes += 1;
                }
            }
            Op::Flush => {
                if let Some(b) = legacy.take_batch() {
                    legacy_batches.push(b);
                }
                let _ = w.flush();
            }
        }
        prop_assert_eq!(legacy_state(&legacy), new_state(&w), "state after {:?}", op);
        while let Ok(batch) = rx.try_recv() {
            new_batches.push((
                batch.encode_ilp_for_test(Feed::Dhan),
                batch.rows,
                batch.min_seq,
                batch.max_seq,
                batch.unbacked,
            ));
        }
    }
    if let Some(b) = legacy.take_batch() {
        legacy_batches.push(b);
    }
    let _ = w.flush();
    while let Ok(batch) = rx.try_recv() {
        new_batches.push((
            batch.encode_ilp_for_test(Feed::Dhan),
            batch.rows,
            batch.min_seq,
            batch.max_seq,
            batch.unbacked,
        ));
    }
    publish_candle_contract_labels(CandleContractLabels::default());
    prop_assert_eq!(legacy_batches.len(), new_batches.len(), "batch count");
    for (i, (a, b)) in legacy_batches.iter().zip(new_batches.iter()).enumerate() {
        prop_assert_eq!(a, b, "batch {}", i);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1_024, ..ProptestConfig::default() })]

    /// PF-4: the staging writer is byte-identical to the old append path
    /// (library encoder).
    #[test]
    fn proptest_pf4_staged_bytes_equal_the_legacy_append_library(
        ops in proptest::collection::vec(op(), 1..48)
    ) {
        run_case(TickIlpEncoder::Library, &ops)?;
    }

    /// PF-4: the same, our own encoder.
    #[test]
    fn proptest_pf4_staged_bytes_equal_the_legacy_append_own(
        ops in proptest::collection::vec(op(), 1..48)
    ) {
        run_case(TickIlpEncoder::Own, &ops)?;
    }
}
// ---------------------------------------------------------------------------
// Pin cuts, the writer and rescue threads, end to end
// ---------------------------------------------------------------------------

/// An in-session tick for id `ID_BASE + i % 16` on NSE_FNO.
fn session_tick(i: u64) -> ParsedTick {
    let ltt = DAY_IST_SECS + 36_000 + i as u32;
    ParsedTick {
        security_id: ID_BASE + (i % 16),
        exchange_segment_code: 2,
        last_traded_price: 245.35 + i as f32 * 0.05,
        last_trade_quantity: 75,
        exchange_timestamp: ltt,
        received_at_nanos: (i64::from(ltt) - 19_800) * 1_000_000_000 + 250_000_000,
        volume: 1_250_000 + i as u32,
        open_interest: 4_512_300,
        ..ParsedTick::default()
    }
}

/// Publishes a table naming every test id `NAME-<round>`.
fn publish_round(round: usize) {
    let mut labels = CandleContractLabels::default();
    for i in 0..16_u64 {
        let id = i64::try_from(ID_BASE + i).unwrap_or_default();
        labels.insert((id, "NSE_FNO"), Arc::from(format!("NAME-{round}").as_str()));
    }
    publish_candle_contract_labels(labels);
}

fn scratch(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("tv-pf4-{tag}-{nanos}"))
}

fn spill_text(dir: &Path) -> String {
    let mut text = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for p in paths.iter().filter(|p| p.is_file()) {
            text.push_str(&std::fs::read_to_string(p).unwrap_or_default());
        }
    }
    text
}

fn first_lines(text: &[u8], n: usize) -> String {
    String::from_utf8_lossy(text)
        .lines()
        .take(n)
        .map(|l| format!("{l}\n"))
        .collect()
}

/// A ninth contract-name table inside one batch hands the batch off early
/// (split writer, queue has room): eight rows go, every row's name is the
/// one in force when it was appended, and nothing is lost.
#[test]
fn test_pf4_ninth_name_table_hands_the_batch_off_early() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut legacy = LegacyAppender::new(Feed::Dhan, TickIlpEncoder::Own);
    let (mut w, sink, rx) =
        TickWriter::for_test_with_encoder(Feed::Dhan, TickIlpEncoder::Own).split_for_offload();
    drop(sink);
    for round in 0..10_usize {
        publish_round(round);
        let t = session_tick(round as u64);
        legacy
            .append_tick_with_seq(&t, 1 + round as i64)
            .expect("legacy");
        w.append_tick_with_seq(&t, 1 + round as i64)
            .expect("append");
    }
    let first = rx.try_recv().expect("the ninth table cut a batch");
    assert_eq!(first.rows, 8, "eight tables, eight rows");
    assert_eq!(w.pending(), 2);
    w.flush().expect("hand off the rest");
    let second = rx.try_recv().expect("the rest");
    let mut text = first.encode_ilp_for_test(Feed::Dhan);
    text.extend(second.encode_ilp_for_test(Feed::Dhan));
    let (want, rows, ..) = legacy.take_batch().expect("legacy rows");
    assert_eq!(rows, 10);
    assert_eq!(text, want, "byte-identical across the cut");
    for round in 0..10 {
        assert!(
            String::from_utf8_lossy(&text).contains(&format!("contract=NAME-{round} ")),
            "row {round} carries the name in force when it was appended"
        );
    }
    publish_candle_contract_labels(CandleContractLabels::default());
}

/// The same cut with the writer's queue FULL: the batch cannot be handed
/// off, so it is rescued (the durable, counted tier), never dropped, and the
/// rescue thread's spill holds its rows byte for byte.
#[test]
fn test_pf4_ninth_name_table_with_a_full_queue_is_rescued_not_dropped() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = scratch("pin-cut-full");
    let (mut w, sink, rx) = TickWriter::for_test(Feed::Dhan)
        .with_spill_dir_for_test(dir.clone())
        .split_for_offload();
    drop(sink);
    let (rescue, rescue_rx) = w.split_rescue_offload();
    // Fill the hand-off queue.
    for i in 0..FLUSH_QUEUE_DEPTH as u64 {
        w.append_tick_with_seq(&session_tick(100 + i), 1_000 + i as i64)
            .expect("append");
        w.flush().expect("queued");
    }
    let mut legacy = LegacyAppender::new(Feed::Dhan, TickIlpEncoder::Library);
    for round in 0..9_usize {
        publish_round(round);
        let t = session_tick(round as u64);
        legacy
            .append_tick_with_seq(&t, 1 + round as i64)
            .expect("legacy");
        w.append_tick_with_seq(&t, 1 + round as i64)
            .expect("append");
    }
    assert_eq!(w.pending(), 1, "eight rows left in the cut, one staged");
    // Mark the cut batch's rows unbacked? No: they are WAL-shaped, so the
    // full queue PARKS nothing and the rescue thread has room: it is queued.
    let batch = rescue_rx
        .try_recv()
        .expect("the cut batch went to the rescue thread");
    assert_eq!(batch.rows(), 8);
    rescue.rescue(&batch);
    let (want, ..) = legacy.take_batch().expect("legacy");
    assert_eq!(
        spill_text(&dir),
        first_lines(&want, 8),
        "the spill holds the eight rows, byte for byte"
    );
    drop(rx);
    let _ = std::fs::remove_dir_all(&dir);
    publish_candle_contract_labels(CandleContractLabels::default());
}

/// The cut on an UNSPLIT writer with no sender: the synchronous arm rescues
/// the rows to the spill tier, written on the drain's cold path, byte for byte.
#[test]
fn test_pf4_ninth_name_table_on_an_unsplit_writer_spills_inline() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = scratch("pin-cut-unsplit");
    let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
    let mut legacy = LegacyAppender::new(Feed::Dhan, TickIlpEncoder::Library);
    for round in 0..9_usize {
        publish_round(round);
        let t = session_tick(round as u64);
        legacy
            .append_tick_with_seq(&t, 1 + round as i64)
            .expect("legacy");
        w.append_tick_with_seq(&t, 1 + round as i64)
            .expect("append");
    }
    assert_eq!(w.pending(), 1);
    let (want, ..) = legacy.take_batch().expect("legacy");
    assert_eq!(spill_text(&dir), first_lines(&want, 8));
    let _ = std::fs::remove_dir_all(&dir);
    publish_candle_contract_labels(CandleContractLabels::default());
}

/// A local ILP server that answers every request with 204 and records each
/// `/write` body. Returns the port and the bodies.
fn ilp_server() -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut writer = stream;
                loop {
                    let mut request = String::new();
                    if reader.read_line(&mut request).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut length = 0_usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let line = line.trim_end();
                        if line.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; length];
                    if reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    if request.contains("/write") {
                        let _ = tx.send(body);
                    }
                    let reply = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
                    if writer.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, rx)
}

/// End to end through the real writer thread's half, both encoders: the
/// batch's text is written on the sink and POSTed byte-identical to the old
/// append path's text, and a row the encoder refuses mid-batch (owner
/// decision 2026-11-06) is counted there — on the ledger's not-landed total
/// and the encode-failure count — before the batch is acknowledged, while
/// every other row lands.
#[test]
fn test_pf4_writer_thread_posts_the_legacy_bytes_and_counts_a_refused_row() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish_round(7);
    for encoder in [TickIlpEncoder::Library, TickIlpEncoder::Own] {
        let (port, bodies) = ilp_server();
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: port,
            pg_port: 1,
            ilp_port: 1,
        };
        let mut legacy = LegacyAppender::new(Feed::Dhan, encoder);
        let (mut w, mut sink, rx) =
            TickWriter::new_with_encoder(&cfg, Feed::Dhan, encoder).split_for_offload();
        for i in 0..20_u64 {
            let t = session_tick(i);
            legacy
                .append_tick_with_seq(&t, 5_000 + i as i64)
                .expect("legacy");
            w.append_tick_with_seq(&t, 5_000 + i as i64)
                .expect("append");
            if i == 9 {
                // A row the encoder refuses (a negative designated time),
                // staged mid-batch through the raw append.
                let mut bad = TickRow::from_parsed_tick(&session_tick(99), 5_099).expect("row");
                bad.ts_ist_nanos = -1;
                w.append_row(&bad).expect("staging never refuses");
            }
        }
        w.flush().expect("hand off");
        let mut batch = rx.try_recv().expect("queued");
        assert_eq!(batch.rows(), 21);
        let not_landed = crate::tick_conservation_ledger::ledger().rows_not_landed();
        let failures = crate::tick_stage::encode_failures_so_far();
        let landed = sink.write(&mut batch);
        assert_eq!(landed, 20, "{encoder:?}: every good row landed");
        assert!(
            crate::tick_conservation_ledger::ledger().rows_not_landed() > not_landed,
            "{encoder:?}: the refused row is on the ledger's not-landed total"
        );
        assert!(crate::tick_stage::encode_failures_so_far() > failures);
        let body = bodies
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("one POST");
        let (want, ..) = legacy.take_batch().expect("legacy");
        assert_eq!(
            body, want,
            "{encoder:?}: the POST is the old append path's text"
        );
        // The spent batch came back through the spare lane, emptied.
        w.append_tick_with_seq(&session_tick(30), 6_000)
            .expect("append");
        w.flush().expect("hand off");
        let next = rx.try_recv().expect("queued");
        assert_eq!(next.rows(), 1);
    }
    publish_candle_contract_labels(CandleContractLabels::default());
}

/// The rescue thread writes the staged rows' text to the spill, byte for
/// byte, and counts a refused row once (owner decision 2026-11-06).
#[test]
fn test_pf4_rescue_thread_spills_the_legacy_bytes_and_counts_a_refused_row() {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    publish_round(3);
    let dir = scratch("rescue-thread");
    let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
    let (rescue, rescue_rx) = w.split_rescue_offload();
    let mut legacy = LegacyAppender::new(Feed::Dhan, TickIlpEncoder::Library);
    for i in 0..6_u64 {
        let t = session_tick(i);
        legacy
            .append_tick_with_seq(&t, 7_000 + i as i64)
            .expect("legacy");
        w.append_tick_with_seq(&t, 7_000 + i as i64)
            .expect("append");
    }
    let mut bad = TickRow::from_parsed_tick(&session_tick(50), 7_050).expect("row");
    bad.ts_ist_nanos = -1;
    w.append_row(&bad).expect("staging never refuses");
    assert_eq!(w.discard_pending(), 7);
    let batch = rescue_rx.try_recv().expect("queued to the rescue thread");
    let failures = crate::tick_stage::encode_failures_so_far();
    rescue.rescue(&batch);
    assert!(crate::tick_stage::encode_failures_so_far() > failures);
    let (want, ..) = legacy.take_batch().expect("legacy");
    assert_eq!(spill_text(&dir).into_bytes(), want);
    let _ = std::fs::remove_dir_all(&dir);
    publish_candle_contract_labels(CandleContractLabels::default());
}
