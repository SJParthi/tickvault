//! DHAT zero-allocation ratchet for everything the seal writer does PER SEAL
//! after the fold produces it (owner directive 2026-10-30: every in-memory
//! step under 1 µs at p99, no allocation per seal).
//!
//! One measured window walks 20,000 seals through the same calls the live
//! path makes, in order:
//!
//! 1. the drain's hand-off, `SealSender::try_send`;
//! 2. the writer's take, `try_recv` then `SealAbsorptionPipeline::submit`
//!    (into a prefaulted, production-size ring);
//! 3. `pop_oldest`, then `ShadowCandleWriter::append_seal` (row build plus
//!    the ILP append, with the day's contract names published so the label
//!    probe hits and writes the `contract` symbol, and all three delay
//!    columns rendered);
//! 4. the live-commit ledger probe, `note_live_commits`, on an active ledger
//!    (one bar spilled), for both a tracked and an untracked bar.
//!
//! Every 4,096 seals the ILP buffer is discarded, as a flush clears it. The
//! budget is zero bytes and zero blocks. What it does NOT cover, and could
//! not: the network flush and every disk write (spill, DLQ, mirror), which
//! are not per-seal in-memory work.
//!
//! Bite: the second phase appends rows whose delay columns go through a fresh
//! `String` per row (the shape the reused scratch exists to prevent) and
//! asserts the profiler sees it, so the zero above is a measurement of a
//! case that can fail.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::path::PathBuf;
use std::sync::Arc;

use tickvault_common::feed::Feed;
use tickvault_storage::candle_contract_labels::{
    CandleContractLabels, publish_candle_contract_labels,
};
use tickvault_storage::delay_render::render_delay_into;
use tickvault_storage::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use tickvault_storage::seal_spill::{SealSpillWriter, SerializedSeal};
use tickvault_storage::seal_writer_runner::SEAL_MPSC_CAPACITY;
use tickvault_storage::shadow_candle_writer::ShadowCandleWriter;
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;
/// Seals walked through the path per measured window.
const SEALS: usize = 20_000;
/// Distinct seals cycled through.
const POOL: usize = 1_024;
/// Appends between buffer resets (the live cycle flushes at 4,096).
const BATCH: usize = 4_096;
/// 13:20:00 IST as Dhan's IST-epoch seconds: inside the session.
const BUCKET_SECS: u32 = 1_786_800_000;
const NOW_UNIX_SECS: i64 = BUCKET_SECS as i64 - 19_800;

fn seal(i: usize) -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = BUCKET_SECS;
    state.open = 100.05 + i as f64 * 0.05;
    state.high = state.open + 5.0;
    state.low = state.open - 1.0;
    state.close = state.open + 1.5;
    state.volume = 1_234 + i as u64;
    state.oi = 50_000;
    state.tick_count = 5;
    state.close_pct_from_prev_day = 1.5;
    state.total_buy_qty = 89_600;
    state.total_sell_qty = 4_800;
    let open_nanos = i64::from(BUCKET_SECS) * 1_000_000_000;
    state.first_receipt_ist_nanos = open_nanos + 12_345_678;
    state.last_receipt_ist_nanos = open_nanos + 45_000_000_000;
    BufferedSeal::new(40_000 + i as u64, 2, TfIndex::M1, state, Feed::Dhan)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tv-dhat-seal-per-seal-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

struct Path {
    seals: Vec<BufferedSeal>,
    tx: crossbeam_channel::Sender<BufferedSeal>,
    rx: crossbeam_channel::Receiver<BufferedSeal>,
    pipeline: SealAbsorptionPipeline,
    writer: ShadowCandleWriter,
    ledger: SealSpillWriter,
}

impl Path {
    fn walk(&mut self, from: usize, count: usize) {
        for n in from..from + count {
            let s = self.seals[n % POOL];
            assert!(self.tx.try_send(s).is_ok(), "the hand-off refused a seal");
            let taken = self.rx.try_recv().expect("the seal is queued");
            assert!(matches!(
                self.pipeline.submit(taken, NOW_UNIX_SECS),
                SubmitOutcome::Buffered
            ));
            let popped = self.pipeline.pop_oldest().expect("the ring holds it");
            self.writer.append_seal(&popped).expect("the row appends");
            // Tracked bar (pool 0) and an untracked one.
            std::hint::black_box(
                self.ledger
                    .note_live_commits(&self.seals[..1], NOW_UNIX_SECS),
            );
            let k = 1 + n % (POOL - 1);
            std::hint::black_box(
                self.ledger
                    .note_live_commits(&self.seals[k..k + 1], NOW_UNIX_SECS),
            );
            if n % BATCH == BATCH - 1 {
                self.writer.discard_pending();
            }
        }
    }
}

#[test]
fn dhat_the_per_seal_path_after_the_fold_never_allocates() {
    let mut labels = CandleContractLabels::default();
    for i in 0..POOL {
        labels.insert(
            (40_000 + i as i64, "NSE_FNO"),
            Arc::from(format!("NIFTY-25Nov2026-{}-CE", 20_000 + i).as_str()),
        );
    }
    publish_candle_contract_labels(labels);

    let ledger_dir = temp_dir("ledger");
    let ledger = SealSpillWriter::with_spill_dir_for_test(ledger_dir.clone());
    ledger
        .append_seal(&SerializedSeal::from(&seal(0)), NOW_UNIX_SECS)
        .expect("spill the tracked bar");
    let (spill_dir, dlq_dir) = (temp_dir("spill"), temp_dir("dlq"));
    let mut pipeline =
        SealAbsorptionPipeline::with_dirs_for_test(spill_dir.clone(), dlq_dir.clone());
    assert!(pipeline.prefault_ring() > 0);
    let (tx, rx) = crossbeam_channel::bounded(SEAL_MPSC_CAPACITY);
    let mut path = Path {
        seals: (0..POOL).map(seal).collect(),
        tx,
        rx,
        pipeline,
        writer: ShadowCandleWriter::for_test(),
        ledger,
    };
    // Warm: the ILP buffer grows to a full batch's size once, and every
    // thread-local the label snapshot uses is set up.
    path.walk(0, BATCH - 1);
    let written = path.writer.buffer_bytes();
    assert!(
        written.windows(8).any(|w| w == b"contract"),
        "no contract name was written"
    );
    assert!(
        written.windows(13).any(|w| w == b"open_latency="),
        "no delay text was written"
    );
    path.walk(BATCH - 1, 1);

    let _profiler = dhat::Profiler::builder().testing().build();

    let mut from = BATCH;
    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            path.walk(from, SEALS);
            from += SEALS;
        },
    );
    assert!(
        bytes == BUDGET_BYTES && blocks == BUDGET_BLOCKS,
        "the per-seal path allocated {bytes} bytes / {blocks} blocks over {SEALS} seals \
         (hand-off, take, ring, row build, ILP append, ledger probe). Every one of these \
         runs once per sealed candle and must not allocate."
    );

    // Bite: a fresh String per delay column is what the reused scratch
    // prevents; the profiler must see it.
    let (_, fresh_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for n in 0..1_000_i64 {
                let mut fresh = String::new();
                render_delay_into(&mut fresh, n * 1_000_003);
                std::hint::black_box(fresh);
            }
        },
    );
    assert!(
        fresh_blocks > 0,
        "a fresh String per render allocated nothing, so the zero above would not \
         be measuring a case that can fail"
    );

    drop(path);
    publish_candle_contract_labels(CandleContractLabels::default());
    for dir in [ledger_dir, spill_dir, dlq_dir] {
        let _ = std::fs::remove_dir_all(dir);
    }
}
