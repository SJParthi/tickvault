//! PF-4 golden fixtures (plan item PF-4, 2026-11-06).
//!
//! Recorded on the UNCHANGED tick writer, before the ILP text step moved off
//! the frame drain, so the move can be proven against bytes the old code
//! wrote rather than against a re-implementation of it. Each fixture is one
//! seeded stream of appends (ticks, `tick_quotes` routes, `feed_aux_packets`
//! rows, window and band refusals, sequence-0 rows, unbacked marks and
//! contract-name publishes at random points) cut into batches at random
//! flush points through a split writer, both encoders.
//!
//! Every fixture holds, per batch, the row count, the capture-sequence range,
//! the unbacked flag and the exact ILP bytes. The `light` streams publish at
//! most seven name tables inside one batch and are compared batch by batch;
//! the `heavy` stream publishes far more often and is compared as the whole
//! concatenated text plus the row total and the overall sequence range (a
//! later change may cut a batch early when names change too often, which
//! moves batch boundaries but must not move one byte).
//!
//! Regenerate ONLY on the code the fixtures describe:
//! `TV_PF4_GOLDEN_WRITE=1 cargo test -p tickvault-storage --lib tick_stage_golden`.

use super::*;
use crate::candle_contract_labels::{
    CandleContractLabels, TEST_PUBLISH_LOCK, publish_candle_contract_labels,
};
use std::sync::Arc;

/// Where the fixtures live, relative to the crate.
const FIXTURE_DIR: &str = "tests/fixtures/pf4_golden";

/// Set to `1` to (re)write the fixtures instead of comparing against them.
const WRITE_ENV: &str = "TV_PF4_GOLDEN_WRITE";

/// Ids no other test names, so only this test's publishes name them.
const ID_BASE: u64 = 7_800_000_000;

/// 2026-10-28 00:00 IST, IST epoch seconds.
const DAY_IST_SECS: u32 = 1_793_145_600;

/// 09:00 IST, seconds of day (the window open).
const WINDOW_OPEN_SECS: u32 = 32_400;

/// Seconds of the 09:00-15:40 window.
const WINDOW_SPAN_SECS: u32 = 24_000;

/// IST minus UTC, seconds.
const IST_OFFSET_SECS: i64 = 19_800;

/// Operations per stream.
const OPS: usize = 420;

/// Contract names a publish draws from. All at most 64 characters, so a
/// later 64-character cap on names cannot change these bytes; two need the
/// publish-time sanitiser.
const NAMES: [&str; 8] = [
    "NIFTY-25Nov2026-24500-CE",
    "BANKNIFTY-27Nov2026-51200-PE",
    "RELIANCE",
    "NIFTY",
    "BAD,NAME=X",
    "SPACE NAME",
    "M&M-27Nov2026-3100-CE",
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ-0123456789-ABCDEFGHIJKLMNOPQRSTUVWXYZ",
];

/// Segment codes a row draws from (IDX_I, NSE_EQ, NSE_FNO, BSE_EQ).
const SEGMENTS: [u8; 4] = [0, 1, 2, 4];

/// xorshift64* — deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// One batch as the writer handed it off.
#[derive(Debug, PartialEq, Eq)]
struct Golden {
    rows: usize,
    min_seq: u64,
    max_seq: u64,
    unbacked: bool,
    bytes: Vec<u8>,
}

/// The batch's ILP text, exactly as the writer thread would send it.
///
/// Written in the run's OWN encoder (review round 1, 2026-11-06): both runs
/// used to be written with the library encoder, so the "the two encoders
/// differ" check below compared the library with itself.
fn batch_bytes(batch: &FlushBatch, encoder: TickIlpEncoder) -> Vec<u8> {
    batch.encode_ilp_with_for_test(Feed::Dhan, encoder)
}

fn price(rng: &mut Rng) -> f32 {
    match rng.below(20) {
        0 => f32::NAN,
        1 => f32::INFINITY,
        2 => 0.0,
        3 => -0.0,
        4 => -12.35,
        5 => 131_073.5,
        _ => 100.0 + rng.below(50_000) as f32 * 0.05,
    }
}

fn qty(rng: &mut Rng) -> u32 {
    if rng.below(4) == 0 {
        0
    } else {
        rng.below(5_000_000) as u32
    }
}

fn security_id(rng: &mut Rng) -> u64 {
    if rng.below(60) == 0 {
        // Bit 63 set: refused by the row build, never a row.
        u64::MAX - rng.below(10)
    } else {
        ID_BASE + rng.below(24)
    }
}

/// A receipt (UTC nanos) near `ltt_ist`, sometimes absent or late.
fn receipt(rng: &mut Rng, ltt_ist: u32) -> i64 {
    match rng.below(12) {
        0 => 0,
        1 => {
            // Received after the 15:40 close.
            (i64::from(DAY_IST_SECS + 57_000) - IST_OFFSET_SECS) * 1_000_000_000
        }
        _ => {
            let lag = rng.below(600) as i64;
            (i64::from(ltt_ist) - IST_OFFSET_SECS + lag) * 1_000_000_000
                + rng.below(1_000_000_000) as i64
        }
    }
}

fn tick(rng: &mut Rng) -> ParsedTick {
    let in_session =
        DAY_IST_SECS + WINDOW_OPEN_SECS + rng.below(u64::from(WINDOW_SPAN_SECS)) as u32;
    let ltt = match rng.below(14) {
        0 => 0,                         // never traded: falls back to the receipt
        1 => 2_600_000_000,             // year 2052: out of band
        2 => DAY_IST_SECS + 7 * 3_600,  // 07:00: out of the window
        3 => DAY_IST_SECS + 15 * 3_600, // the 1980-sentinel's neighbour: in session
        _ => in_session,
    };
    let ltt_for_receipt = if ltt == 0 || ltt == 2_600_000_000 {
        in_session
    } else {
        ltt
    };
    ParsedTick {
        security_id: security_id(rng),
        exchange_segment_code: rng.pick(&SEGMENTS),
        last_traded_price: price(rng),
        last_trade_quantity: rng.below(2_000) as u16,
        exchange_timestamp: ltt,
        received_at_nanos: receipt(rng, ltt_for_receipt),
        average_traded_price: price(rng),
        volume: qty(rng),
        total_sell_quantity: qty(rng),
        total_buy_quantity: qty(rng),
        day_open: price(rng),
        day_close: price(rng),
        day_high: price(rng),
        day_low: price(rng),
        open_interest: qty(rng),
        oi_day_high: qty(rng),
        oi_day_low: qty(rng),
        ..ParsedTick::default()
    }
}

fn capture_seq(rng: &mut Rng, counter: &mut i64) -> i64 {
    *counter += 1 + rng.below(3) as i64;
    if rng.below(25) == 0 { 0 } else { *counter }
}

fn aux_row(rng: &mut Rng, seq: i64) -> Option<AuxPacketRow> {
    let kind = rng.pick(&[
        AuxPacketKind::OpenInterest,
        AuxPacketKind::PrevClose,
        AuxPacketKind::MarketStatus,
        AuxPacketKind::Disconnect,
    ]);
    let ltt = DAY_IST_SECS + WINDOW_OPEN_SECS + rng.below(u64::from(WINDOW_SPAN_SECS)) as u32;
    let mut row = AuxPacketRow::from_header(
        kind,
        ID_BASE + rng.below(24),
        rng.pick(&SEGMENTS),
        receipt(rng, ltt),
        seq,
    )?;
    match kind {
        AuxPacketKind::OpenInterest => row.oi = crate::feed_aux_persistence::aux_qty(qty(rng)),
        AuxPacketKind::PrevClose => {
            row.prev_close = crate::feed_aux_persistence::aux_price(price(rng));
            row.prev_oi = crate::feed_aux_persistence::aux_qty(qty(rng));
        }
        AuxPacketKind::Disconnect => row.reason_code = Some(805),
        _ => {}
    }
    Some(row)
}

fn publish_random(rng: &mut Rng) {
    let mut labels = CandleContractLabels::default();
    for i in 0..24 {
        if rng.below(3) != 0 {
            let id = i64::try_from(ID_BASE + i).unwrap_or_default();
            let seg = segment_code_to_str(rng.pick(&SEGMENTS));
            labels.insert((id, seg), Arc::from(rng.pick(&NAMES)));
        }
    }
    publish_candle_contract_labels(labels);
}

/// Runs one seeded stream through a split writer and returns its batches.
fn run_stream(seed: u64, encoder: TickIlpEncoder, heavy: bool) -> Vec<Golden> {
    let mut rng = Rng(seed);
    publish_candle_contract_labels(CandleContractLabels::default());
    let spill = std::env::temp_dir().join(format!("tv-pf4-golden-{seed:x}-{encoder:?}"));
    let (mut w, sink, rx) = TickWriter::for_test_with_encoder(Feed::Dhan, encoder)
        .with_spill_dir_for_test(spill)
        .split_for_offload();
    // The batches are read off the queue below; nothing is sent.
    drop(sink);
    let mut out = Vec::new();
    let mut counter = 1_000_i64;
    let mut publishes_in_batch = 0_u32;
    let collect = |rx: &std::sync::mpsc::Receiver<FlushBatch>, out: &mut Vec<Golden>| {
        while let Ok(batch) = rx.try_recv() {
            out.push(Golden {
                rows: batch.rows,
                min_seq: batch.min_seq,
                max_seq: batch.max_seq,
                unbacked: batch.unbacked,
                bytes: batch_bytes(&batch, encoder),
            });
        }
    };
    // Op thresholds out of 100: tick, quote route, aux row, unbacked mark,
    // name publish, flush (the rest).
    let [t_tick, t_quote, t_aux, t_mark, t_publish] = if heavy {
        [40, 50, 60, 62, 96]
    } else {
        [50, 65, 77, 80, 88]
    };
    for _ in 0..OPS {
        let roll = rng.below(100);
        if roll < t_tick {
            let t = tick(&mut rng);
            let seq = capture_seq(&mut rng, &mut counter);
            let _ = w.append_tick_with_seq(&t, seq);
        } else if roll < t_quote {
            let t = tick(&mut rng);
            let seq = capture_seq(&mut rng, &mut counter);
            let _ = w.append_tick_quote_with_seq(&t, seq);
        } else if roll < t_aux {
            let seq = capture_seq(&mut rng, &mut counter);
            if let Some(row) = aux_row(&mut rng, seq) {
                let _ = w.append_aux(&row);
            }
        } else if roll < t_mark {
            w.mark_pending_unbacked();
        } else if roll < t_publish {
            // A light batch sees at most eight name tables: the one its first
            // row read plus seven publishes.
            if !heavy && publishes_in_batch >= 7 {
                let _ = w.flush();
                publishes_in_batch = 0;
            }
            publish_random(&mut rng);
            publishes_in_batch += 1;
        } else {
            if w.pending() > 0 {
                publishes_in_batch = 0;
            }
            let _ = w.flush();
        }
        // Read every handed-off batch at once, so the queue never fills.
        collect(&rx, &mut out);
    }
    let _ = w.flush();
    collect(&rx, &mut out);
    out
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(format!("{name}.golden"))
}

fn encode_fixture(batches: &[Golden]) -> Vec<u8> {
    let mut out = Vec::new();
    for b in batches {
        out.extend_from_slice(
            format!(
                "#batch rows={} min={} max={} unbacked={} len={}\n",
                b.rows,
                b.min_seq,
                b.max_seq,
                b.unbacked,
                b.bytes.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&b.bytes);
    }
    out
}

fn field<'a>(header: &'a str, key: &str) -> &'a str {
    header
        .split(' ')
        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .expect("fixture header field")
}

fn decode_fixture(mut raw: &[u8]) -> Vec<Golden> {
    let mut out = Vec::new();
    while !raw.is_empty() {
        let nl = raw.iter().position(|b| *b == b'\n').expect("header line");
        let header = std::str::from_utf8(&raw[..nl]).expect("utf8 header");
        let len: usize = field(header, "len").parse().expect("len");
        let bytes = raw[nl + 1..nl + 1 + len].to_vec();
        out.push(Golden {
            rows: field(header, "rows").parse().expect("rows"),
            min_seq: field(header, "min").parse().expect("min"),
            max_seq: field(header, "max").parse().expect("max"),
            unbacked: field(header, "unbacked").parse().expect("unbacked"),
            bytes,
        });
        raw = &raw[nl + 1 + len..];
    }
    out
}

/// The whole stream as one text, its row total and its sequence range.
fn summary(batches: &[Golden]) -> (Vec<u8>, usize, u64, u64) {
    let mut text = Vec::new();
    let mut rows = 0;
    let mut min = 0_u64;
    let mut max = 0_u64;
    for b in batches {
        text.extend_from_slice(&b.bytes);
        rows += b.rows;
        if b.min_seq != 0 && (min == 0 || b.min_seq < min) {
            min = b.min_seq;
        }
        max = max.max(b.max_seq);
    }
    (text, rows, min, max)
}

fn assert_matches_golden(name: &str, seed: u64, heavy: bool) {
    let _g = TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let library = run_stream(seed, TickIlpEncoder::Library, heavy);
    let own = run_stream(seed, TickIlpEncoder::Own, heavy);
    publish_candle_contract_labels(CandleContractLabels::default());
    assert_eq!(library, own, "{name}: the two encoders differ");
    assert!(
        library.iter().filter(|b| b.rows > 0).count() >= 8,
        "{name}: the stream must cut several batches"
    );
    let path = fixture_path(name);
    if std::env::var(WRITE_ENV).is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, encode_fixture(&library)).expect("write fixture");
        return;
    }
    let raw = std::fs::read(&path).expect("fixture present (see the module docs)");
    let golden = decode_fixture(&raw);
    if heavy {
        assert_eq!(
            summary(&library),
            summary(&golden),
            "{name}: the stream's bytes, rows or sequence range moved"
        );
    } else {
        assert_eq!(library.len(), golden.len(), "{name}: batch count moved");
        for (i, (got, want)) in library.iter().zip(golden.iter()).enumerate() {
            assert_eq!(got, want, "{name}: batch {i} moved");
        }
    }
}

#[test]
fn pf4_golden_light_seed_1() {
    assert_matches_golden("light_seed_1", 0x9E37_79B9_7F4A_7C15, false);
}

#[test]
fn pf4_golden_light_seed_2() {
    assert_matches_golden("light_seed_2", 0xD1B5_4A32_D192_ED03, false);
}

#[test]
fn pf4_golden_light_seed_3() {
    assert_matches_golden("light_seed_3", 0x8CB9_2BA7_2F3D_8DD7, false);
}

#[test]
fn pf4_golden_heavy_publishes() {
    assert_matches_golden("heavy_publishes", 0x2545_F491_4F6C_DD1D, true);
}
