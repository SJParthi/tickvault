//! DHAT gate for the depth BOOK hand-off (plan item PF-4b review round 1,
//! 2026-11-06); the depth twin of `dhat_tick_rescue_spare.rs`.
//!
//! Since PF-4b the frame drain stages `market_depth_book` rows in a
//! pre-sized batch and hands the batch to the depth writer thread (a flush)
//! or, during a stall, to the rescue thread (`discard_pending`). Both threads
//! write the ILP text and then hand the spent batch back to the drain through
//! the spare lane, cleared on their side. With batches circulating, the
//! drain's side of every cycle must allocate nothing:
//!
//! - flush cycles: two five-level book rows (a Full packet's bid and ask)
//!   are appended and flushed; the writer thread (`DepthWriterSink::write`,
//!   no QuestDB here, so its own rescue to the spill) runs OUTSIDE the
//!   measured window and recycles the batch;
//! - stall cycles: rows are appended and handed to the rescue queue
//!   (`discard_pending`); the rescue thread (`DepthRescueSink::rescue` and
//!   `recycle`) runs outside the window;
//! - a contract-name table is republished mid-run (outside the window), so a
//!   measured cycle pins a NEW table on its first row.
//!
//! Only the drain's calls are inside the window. Stall rows carry capture
//! sequence 0 (no WAL record), so no hand-off takes a rescue floor.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use tickvault_common::feed::Feed;
use tickvault_storage::candle_contract_labels::{
    CandleContractLabels, publish_candle_contract_labels,
};
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_5, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthBookRow, DepthFlushBatch, DepthRescueBatch,
    DepthRescueSink, DepthWriter, DepthWriterSink,
};

/// Measured cycles per shape.
const CYCLES: usize = 6;

/// Tries per cycle, to absorb a one-time cross-thread phantom (the libtest
/// harness allocates on other threads; see `dhat_support`). Each try is a
/// full, identical cycle, so a real per-cycle allocation recurs every time.
const ATTEMPTS: usize = 3;

/// Rows (sides) appended per cycle: three Full packets.
const SIDES_PER_CYCLE: usize = 6;

/// 2026-10-29 10:00:00 IST as IST-epoch nanos, inside the session window.
const SESSION_TS_NANOS: i64 = 1_793_268_000_000_000_000;

const PRICES: [f64; 5] = [24_500.25, 24_500.2, 24_500.15, 24_500.1, 24_500.05];
const QUANTITIES: [f64; 5] = [750.0, 900.0, 1_050.0, 1_200.0, 1_350.0];
const ORDERS: [f64; 5] = [3.0, 4.0, 5.0, 6.0, 7.0];

fn side(i: usize, seq: i64) -> DepthBookRow<'static> {
    DepthBookRow {
        security_id: 52_000 + (i as i64 / 2) % 8,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_5,
        side: if i.is_multiple_of(2) {
            DEPTH_SIDE_BID
        } else {
            DEPTH_SIDE_ASK
        },
        prices: &PRICES,
        quantities: &QUANTITIES,
        orders: &ORDERS,
        capture_seq: seq,
        ts_nanos: SESSION_TS_NANOS + i as i64,
    }
}

fn publish(name: &str) {
    let mut labels = CandleContractLabels::default();
    for id in 0..8_i64 {
        labels.insert((52_000 + id, "NSE_FNO"), Arc::from(name));
    }
    assert!(publish_candle_contract_labels(labels) > 0);
}

/// (bytes, blocks) allocated by `f`.
fn measure(f: impl FnOnce()) -> (u64, u64) {
    let before = dhat::HeapStats::get();
    f();
    let after = dhat::HeapStats::get();
    (
        after.total_bytes.saturating_sub(before.total_bytes),
        after.total_blocks.saturating_sub(before.total_blocks),
    )
}

struct Lane {
    w: DepthWriter,
    sink: DepthWriterSink,
    rx: std::sync::mpsc::Receiver<DepthFlushBatch>,
    rescue_sink: DepthRescueSink,
    rescue_rx: std::sync::mpsc::Receiver<DepthRescueBatch>,
    i: usize,
    seq: i64,
}

impl Lane {
    /// One flush cycle; returns the drain's allocation.
    fn flush_cycle(&mut self) -> (u64, u64) {
        let (w, i, seq) = (&mut self.w, &mut self.i, &mut self.seq);
        let got = measure(|| {
            for _ in 0..SIDES_PER_CYCLE {
                *i += 1;
                *seq += 1;
                w.append_book_row(&side(*i, *seq)).expect("append");
            }
            w.flush().expect("hand off");
        });
        assert_eq!(self.w.pending(), 0, "the batch left the drain");
        let mut batch = self.rx.try_recv().expect("handed to the writer queue");
        assert_eq!(batch.rows(), SIDES_PER_CYCLE * 5, "every level was staged");
        // The writer thread: writes the text (rescued to the spill, no
        // QuestDB here) and recycles the batch.
        let _landed = self.sink.write(&mut batch);
        got
    }

    /// One stall cycle through the rescue queue; returns the drain's
    /// allocation.
    fn rescue_cycle(&mut self) -> (u64, u64) {
        let (w, i) = (&mut self.w, &mut self.i);
        let got = measure(|| {
            for _ in 0..SIDES_PER_CYCLE {
                *i += 1;
                w.append_book_row(&side(*i, 0)).expect("append");
            }
            assert!(w.discard_pending() > 0, "handed to the rescue queue");
        });
        let batch = self.rescue_rx.try_recv().expect("queued for rescue");
        assert_eq!(batch.rows(), SIDES_PER_CYCLE * 5);
        self.rescue_sink.rescue(&batch);
        self.rescue_sink.recycle(batch);
        got
    }
}

fn retried(lane: &mut Lane, cycle: fn(&mut Lane) -> (u64, u64)) -> (u64, u64) {
    let mut got = (u64::MAX, u64::MAX);
    for _ in 0..ATTEMPTS {
        got = cycle(lane);
        if got == (0, 0) {
            break;
        }
    }
    got
}

#[test]
fn dhat_depth_book_hand_off_allocates_nothing_with_batches_recycled() {
    let _profiler = dhat::Profiler::builder().testing().build();
    publish("NIFTY-25Nov2026-24500-CE");
    let (mut w, sink, rx) = DepthWriter::for_test_book(Feed::Dhan).split_for_offload();
    let (rescue_sink, rescue_rx) = w.split_rescue_offload();
    let mut lane = Lane {
        w,
        sink,
        rx,
        rescue_sink,
        rescue_rx,
        i: 0,
        seq: 1_000,
    };
    // Warm-up outside the measured cycles: batches in circulation, the
    // writer thread's encode scratch reserved, per-thread state touched.
    for _ in 0..3 {
        let _ = lane.flush_cycle();
        let _ = lane.rescue_cycle();
    }
    for n in 0..CYCLES {
        if n == CYCLES / 2 {
            // A republished name table, outside the window: the next
            // measured cycle pins it on its first row.
            publish("BANKNIFTY-27Nov2026-51200-PE");
        }
        let got = retried(&mut lane, Lane::flush_cycle);
        assert_eq!(
            got,
            (0, 0),
            "flush cycle {n}: the drain's book append and hand-off must allocate nothing \
             with a recycled batch waiting (bytes, blocks)"
        );
        let got = retried(&mut lane, Lane::rescue_cycle);
        assert_eq!(
            got,
            (0, 0),
            "rescue cycle {n}: the drain's book append and rescue hand-off must allocate \
             nothing with a recycled batch waiting (bytes, blocks)"
        );
    }
    publish_candle_contract_labels(CandleContractLabels::default());
}
