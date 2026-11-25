//! PF-4b golden fixtures (plan item PF-4b, 2026-11-06).
//!
//! Recorded on the UNCHANGED depth writer, before the `market_depth_book`
//! ILP text step moved off the frame drain, so the move is proven against
//! bytes the old code wrote rather than against a re-implementation of it.
//! Each fixture is one seeded stream of array-row appends (inline five-level
//! books as bid/ask pairs, depth-20 and depth-200 sides, zero-level sides,
//! NaN and infinite values, malformed and over-long rows the writer refuses,
//! out-of-window rows, sequence-0 rows, unbacked marks and contract-name
//! publishes at random points) through a SPLIT book writer with its rescue
//! split too. Flushes come at random points; in the light streams the
//! writer queue is sometimes left unread, so the producer retains rows
//! (queue full), widens, and past `MAX_DEPTH_RETAINED_FLUSH_SPANS` hands the
//! rows to the rescue thread's queue.
//!
//! Every fixture holds, per batch in the order it was read (rescue queue
//! first, then the writer queue, after every operation), the kind, the row
//! count (in levels), the capture-sequence range, the unbacked flag and the
//! exact ILP bytes. The `light` streams publish at most seven name tables
//! inside one batch and are compared batch by batch; the `heavy` stream
//! publishes far more often, never fills the queue, and is compared as the
//! concatenated text plus the row total and the overall sequence range (a
//! later change may cut a batch early when names change too often, which
//! moves batch boundaries but must not move one byte).
//!
//! Regenerate ONLY on the code the fixtures describe:
//! `TV_PF4B_GOLDEN_WRITE=1 cargo test -p tickvault-storage --lib depth_stage_golden`.

use super::*;
use crate::candle_contract_labels::{
    CandleContractLabels, TEST_PUBLISH_LOCK, publish_candle_contract_labels,
};
use std::sync::Arc;

/// Where the fixtures live, relative to the crate.
const FIXTURE_DIR: &str = "tests/fixtures/pf4b_golden";

/// Set to `1` to (re)write the fixtures instead of comparing against them.
const WRITE_ENV: &str = "TV_PF4B_GOLDEN_WRITE";

/// Ids no other test names, so only this test's publishes name them.
const ID_BASE: i64 = 7_900_000_000;

/// Distinct instruments a stream draws from.
const IDS: i64 = 16;

/// 2026-10-29 10:00:00 IST as IST-epoch nanos, inside the session window.
const SESSION_TS_NANOS: i64 = 1_793_268_000_000_000_000;

/// 2026-10-29 07:00:00 IST, before the window opens.
const PRE_OPEN_TS_NANOS: i64 = SESSION_TS_NANOS - 3 * 3_600 * 1_000_000_000;

/// Operations per stream.
const OPS: usize = 420;

/// Contract names a publish draws from (all at most 64 characters; two need
/// the publish-time sanitiser).
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

/// Segment labels a row draws from.
const SEGMENTS: [&str; 4] = ["NSE_FNO", "NSE_EQ", "IDX_I", "BSE_EQ"];

/// xorshift64* — deterministic, no dependency.
pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// Which queue a batch came off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Flush,
    Rescue,
}

/// One batch as the writer handed it off.
#[derive(Debug, PartialEq, Eq)]
struct Golden {
    kind: Kind,
    rows: usize,
    min_seq: u64,
    max_seq: u64,
    unbacked: bool,
    bytes: Vec<u8>,
}

/// A level value: mostly prices and sizes, sometimes the awkward ones.
fn value(rng: &mut Rng) -> f64 {
    match rng.below(24) {
        0 => f64::NAN,
        1 => f64::INFINITY,
        2 => f64::NEG_INFINITY,
        3 => 0.0,
        4 => -0.0,
        5 => -12.35,
        6 => 131_073.5,
        7 => f64::MAX,
        8 => f64::MIN_POSITIVE,
        _ => 100.0 + rng.below(50_000) as f64 * 0.05,
    }
}

fn capture_seq(rng: &mut Rng, counter: &mut i64) -> i64 {
    *counter += 1 + rng.below(3) as i64;
    if rng.below(25) == 0 { 0 } else { *counter }
}

fn publish_random(rng: &mut Rng) {
    let mut labels = CandleContractLabels::default();
    for i in 0..IDS {
        if rng.below(3) != 0 {
            labels.insert(
                (ID_BASE + i, rng.pick(&SEGMENTS)),
                Arc::from(rng.pick(&NAMES)),
            );
        }
    }
    publish_candle_contract_labels(labels);
}

/// Level arrays for one side: `(prices, quantities, orders)` lengths, which
/// are equal except for the deliberately malformed rows.
fn lengths(rng: &mut Rng, kind: &'static str) -> (usize, usize, usize) {
    let n = match kind {
        DEPTH_KIND_5 => match rng.below(10) {
            0 => 0,
            1 => 1 + rng.below(4) as usize,
            _ => 5,
        },
        DEPTH_KIND_20 => match rng.below(6) {
            0 => rng.below(20) as usize,
            _ => 20,
        },
        _ => match rng.below(10) {
            0 => MAX_DEPTH_BOOK_LEVELS + 1, // refused: too many levels
            1 | 2 => MAX_DEPTH_BOOK_LEVELS,
            _ => rng.below(40) as usize,
        },
    };
    if rng.below(40) == 0 {
        // Refused: the three arrays disagree.
        (n, n + 1, n)
    } else {
        (n, n, n)
    }
}

/// Appends one generated side (or a bid/ask pair for a five-level book).
pub(super) fn append_random_side(
    w: &mut DepthWriter,
    rng: &mut Rng,
    counter: &mut i64,
    prices: &mut [f64],
    quantities: &mut [f64],
    orders: &mut [f64],
) {
    let kind = match rng.below(20) {
        0 | 1 => DEPTH_KIND_20,
        2 => DEPTH_KIND_200,
        _ => DEPTH_KIND_5,
    };
    let id = ID_BASE + rng.below(IDS as u64) as i64;
    let segment = SEGMENTS[(id % SEGMENTS.len() as i64) as usize];
    let seq = capture_seq(rng, counter);
    let ts = match rng.below(30) {
        0 => PRE_OPEN_TS_NANOS,
        1 => tickvault_common::constants::IST_UTC_OFFSET_NANOS,
        _ => SESSION_TS_NANOS + rng.below(1_000_000_000_000) as i64,
    };
    let sides: &[&'static str] = if kind == DEPTH_KIND_5 {
        &[DEPTH_SIDE_BID, DEPTH_SIDE_ASK]
    } else if rng.below(2) == 0 {
        &[DEPTH_SIDE_BID]
    } else {
        &[DEPTH_SIDE_ASK]
    };
    for side in sides {
        let (np, nq, no) = lengths(rng, kind);
        for v in prices.iter_mut().take(np) {
            *v = value(rng);
        }
        for v in quantities.iter_mut().take(nq) {
            *v = value(rng);
        }
        for v in orders.iter_mut().take(no) {
            *v = value(rng);
        }
        let row = DepthBookRow {
            security_id: id,
            segment,
            depth_kind: kind,
            side,
            prices: &prices[..np],
            quantities: &quantities[..nq],
            orders: &orders[..no],
            capture_seq: seq,
            ts_nanos: ts,
        };
        let _handled = w.append_book_row(&row);
    }
}

/// The batch's ILP bytes exactly as the writer thread would send them.
/// (PF-4b: the text buffer, then the staged rows as the writer thread
/// writes them; before PF-4b the text buffer alone.)
fn flush_bytes(batch: &DepthFlushBatch) -> Vec<u8> {
    let mut out = batch.buffer.as_bytes().to_vec();
    out.extend_from_slice(&batch.staged.encode_readonly(Feed::Dhan));
    out
}

/// The rescue batch's ILP bytes exactly as the rescue thread would spill them.
fn rescue_bytes(batch: &DepthRescueBatch) -> Vec<u8> {
    let mut out = batch.buffer.as_bytes().to_vec();
    out.extend_from_slice(&batch.staged.encode_readonly(Feed::Dhan));
    out
}

/// Runs one seeded stream through a split book writer; returns its batches.
fn run_stream(seed: u64, heavy: bool) -> Vec<Golden> {
    let mut rng = Rng(seed);
    publish_candle_contract_labels(CandleContractLabels::default());
    let spill = std::env::temp_dir().join(format!("tv-pf4b-golden-{seed:x}"));
    let (mut w, sink, rx) = DepthWriter::for_test_book(Feed::Dhan)
        .with_spill_dir_for_test(spill)
        // Never defers to the WAL: the rescue queue is always read below, so
        // this is never asked; set so the process-global marks cannot matter.
        .with_wal_defer_check_for_test(|_, _, _| false)
        .split_for_offload();
    // The batches are read off the queues below; nothing is sent or spilled.
    drop(sink);
    let (rescue_sink, rescue_rx) = w.split_rescue_offload();
    drop(rescue_sink);
    let mut out = Vec::new();
    let mut counter = 1_000_i64;
    let mut publishes_in_batch = 0_u32;
    let mut stall = 0_u32;
    let mut prices = vec![0.0; MAX_DEPTH_BOOK_LEVELS + 2];
    let mut quantities = vec![0.0; MAX_DEPTH_BOOK_LEVELS + 2];
    let mut orders = vec![0.0; MAX_DEPTH_BOOK_LEVELS + 2];
    let collect = |rescue_rx: &std::sync::mpsc::Receiver<DepthRescueBatch>,
                   rx: &std::sync::mpsc::Receiver<DepthFlushBatch>,
                   read_flush: bool,
                   out: &mut Vec<Golden>| {
        while let Ok(batch) = rescue_rx.try_recv() {
            out.push(Golden {
                kind: Kind::Rescue,
                rows: batch.rows,
                min_seq: batch.min_seq,
                max_seq: batch.max_seq,
                unbacked: batch.unbacked,
                bytes: rescue_bytes(&batch),
            });
            // Nothing spills here: give back the batch's watermark floor, as
            // the rescue thread would, so the process-wide floor table (16
            // slots) is not exhausted for the tests that run after this one.
            if let Some(floor) = batch.floor {
                crate::wal_applied_watermark::applied_watermark().release_rescue_floor(floor);
            }
        }
        while read_flush && let Ok(batch) = rx.try_recv() {
            out.push(Golden {
                kind: Kind::Flush,
                rows: batch.rows,
                min_seq: batch.min_seq,
                max_seq: batch.max_seq,
                unbacked: batch.unbacked,
                bytes: flush_bytes(&batch),
            });
        }
    };
    // Op thresholds out of 100: side append, unbacked mark, name publish,
    // flush (the rest).
    let [t_side, t_mark, t_publish] = if heavy { [55, 57, 92] } else { [70, 73, 81] };
    for _ in 0..OPS {
        let roll = rng.below(100);
        if roll < t_side {
            append_random_side(
                &mut w,
                &mut rng,
                &mut counter,
                &mut prices,
                &mut quantities,
                &mut orders,
            );
        } else if roll < t_mark {
            w.mark_pending_unbacked();
        } else if roll < t_publish && !heavy && (stall > 0 || w.retained_spans > 0) {
            // No publish while a light stream's queue is stalled or a batch
            // is retained: it would otherwise see more than eight name
            // tables, which a later change may cut early (batch-by-batch
            // comparison).
        } else if roll < t_publish {
            // A light batch sees at most eight name tables: the one its first
            // row read plus seven publishes.
            if !heavy && publishes_in_batch >= 7 {
                let _ = w.flush();
                if w.pending() == 0 {
                    publishes_in_batch = 0;
                }
            }
            if heavy || publishes_in_batch < 7 {
                publish_random(&mut rng);
                publishes_in_batch += 1;
            }
        } else {
            let _ = w.flush();
            // Only a batch that actually left resets the count.
            if w.pending() == 0 {
                publishes_in_batch = 0;
            }
        }
        // The heavy stream reads every batch at once, so its queue never
        // fills; a light stream sometimes leaves the writer queue unread.
        // A stall leaves the writer queue unread for 30-89 operations, long
        // enough to fill it, retain spans and cut to the rescue queue.
        if !heavy && stall == 0 && rng.below(14) == 0 {
            stall = 30 + rng.below(60) as u32;
        }
        let read_flush = heavy || stall == 0;
        stall = stall.saturating_sub(1);
        collect(&rescue_rx, &rx, read_flush, &mut out);
    }
    collect(&rescue_rx, &rx, true, &mut out);
    let _ = w.flush();
    collect(&rescue_rx, &rx, true, &mut out);
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
        let kind = match b.kind {
            Kind::Flush => "flush",
            Kind::Rescue => "rescue",
        };
        out.extend_from_slice(
            format!(
                "#batch kind={kind} rows={} min={} max={} unbacked={} len={}\n",
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
            kind: match field(header, "kind") {
                "rescue" => Kind::Rescue,
                _ => Kind::Flush,
            },
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
    let got = run_stream(seed, heavy);
    publish_candle_contract_labels(CandleContractLabels::default());
    assert!(
        got.iter().filter(|b| b.rows > 0).count() >= 8,
        "{name}: the stream must cut several batches"
    );
    assert!(
        got.iter()
            .any(|b| b.bytes.windows(9).any(|w| w == b"contract=")),
        "{name}: some rows must carry a contract name"
    );
    if !heavy {
        assert!(
            got.iter().any(|b| b.kind == Kind::Rescue),
            "{name}: a light stream must reach the rescue queue"
        );
    }
    let path = fixture_path(name);
    if std::env::var(WRITE_ENV).is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, encode_fixture(&got)).expect("write fixture");
        return;
    }
    let raw = std::fs::read(&path).expect("fixture present (see the module docs)");
    let golden = decode_fixture(&raw);
    if heavy {
        assert_eq!(
            summary(&got),
            summary(&golden),
            "{name}: the stream's bytes, rows or sequence range moved"
        );
    } else {
        assert_eq!(got.len(), golden.len(), "{name}: batch count moved");
        for (i, (g, want)) in got.iter().zip(golden.iter()).enumerate() {
            assert_eq!(g, want, "{name}: batch {i} moved");
        }
    }
}

#[test]
fn pf4b_golden_light_seed_1() {
    assert_matches_golden("light_seed_1", 0x9E37_79B9_7F4A_7C15, false);
}

#[test]
fn pf4b_golden_light_seed_2() {
    assert_matches_golden("light_seed_2", 0xA076_1D64_78BD_642F, false);
}

#[test]
fn pf4b_golden_light_seed_3() {
    assert_matches_golden("light_seed_3", 0x8CB9_2BA7_2F3D_8DD7, false);
}

#[test]
fn pf4b_golden_heavy_publishes() {
    assert_matches_golden("heavy_publishes", 0x2545_F491_4F6C_DD1D, true);
}
