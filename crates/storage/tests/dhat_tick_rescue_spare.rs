//! DHAT gate for the rescue hand-off's spare batch (PF-4 review round 1,
//! 2026-11-06).
//!
//! During a QuestDB stall every flush hands its batch to the rescue thread
//! (`TickWriter::discard_pending`), and the drain then needs a fresh batch.
//! Before the review fix the rescue thread DROPPED each spent batch, so the
//! spare lane stayed empty and every rescue reserved and pre-touched a new
//! 16,384-row batch (about 4.5 MiB) on the drain. The rescue thread now hands
//! each spent batch back (`TickRescueSink::recycle`, cleared on that thread).
//!
//! Two measured cases, each over several hand-offs:
//! - spare lane EMPTY: the hand-off allocates exactly one block (the fresh
//!   batch's reserve), and that block is the pre-size. Bounded and visible,
//!   never more; this is the cost the recycle removes.
//! - spare lane holding a recycled batch: the hand-off allocates nothing.
//!
//! Rows are staged with capture sequence 0 (no WAL record), so no hand-off
//! takes a rescue floor: this test plays the rescue thread without running
//! the spill write that would release one.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::tick_persistence::{RescueBatch, TickRescueSink, TickWriter};
use tickvault_storage::tick_stage::{STAGED_ROW_BYTES, TICK_STAGED_ROWS_PRESIZE};

/// Hand-offs measured per case.
const HAND_OFFS: usize = 5;

/// Tries per hand-off, to absorb a one-time cross-thread phantom (the libtest
/// harness allocates on other threads; see `dhat_support`). Each try is a
/// full, identical hand-off, so a real per-hand-off allocation recurs on
/// every try.
const ATTEMPTS: usize = 3;

/// 2026-10-29 10:00:00 IST, IST epoch seconds (how Dhan sends LTT).
const SESSION_IST_SECS: u32 = 1_793_268_000;

fn tick(i: usize) -> ParsedTick {
    let ltt = SESSION_IST_SECS + i as u32;
    ParsedTick {
        security_id: 41_000 + (i as u64 % 16),
        exchange_segment_code: 2,
        last_traded_price: 245.35,
        last_trade_quantity: 75,
        exchange_timestamp: ltt,
        received_at_nanos: (i64::from(ltt) - 19_800) * 1_000_000_000,
        volume: 1_250_000 + i as u32,
        day_open: 238.05,
        day_close: 240.95,
        day_high: 251.2,
        day_low: 236.6,
        ..ParsedTick::default()
    }
}

/// One rescue hand-off, measured: stages one row (outside the window), then
/// measures ONLY `discard_pending`, then plays the rescue thread: the spent
/// batch is recycled through `sink` when given, else dropped. Retried while
/// over `budget`; returns the last measure as (bytes, blocks).
fn hand_off(
    writer: &mut TickWriter,
    rx: &std::sync::mpsc::Receiver<RescueBatch>,
    sink: Option<&TickRescueSink>,
    i: &mut usize,
    budget: (u64, u64),
) -> (u64, u64) {
    let mut got = (u64::MAX, u64::MAX);
    for _ in 0..ATTEMPTS {
        *i += 1;
        writer.append_tick_with_seq(&tick(*i), 0).expect("append");
        let before = dhat::HeapStats::get();
        let handed = writer.discard_pending();
        let after = dhat::HeapStats::get();
        assert_eq!(handed, 1, "the row was handed to the rescue queue");
        let batch = rx.try_recv().expect("queued");
        match sink {
            Some(sink) => sink.recycle(batch),
            None => drop(batch),
        }
        got = (
            after.total_bytes.saturating_sub(before.total_bytes),
            after.total_blocks.saturating_sub(before.total_blocks),
        );
        if got.0 <= budget.0 && got.1 <= budget.1 {
            return got;
        }
    }
    got
}

#[test]
fn dhat_rescue_hand_off_reuses_a_recycled_batch() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let presize_bytes = (TICK_STAGED_ROWS_PRESIZE * STAGED_ROW_BYTES) as u64;
    let mut i = 0_usize;

    // Case 1: nothing comes back (the spent batch is dropped), so the lane
    // stays empty and each hand-off reserves one fresh batch on the drain.
    let mut empty = TickWriter::for_test(Feed::Dhan);
    let (_sink, rx) = empty.split_rescue_offload();
    for _ in 0..HAND_OFFS {
        let got = hand_off(&mut empty, &rx, None, &mut i, (presize_bytes, 1));
        assert_eq!(
            got,
            (presize_bytes, 1),
            "a hand-off with an empty spare lane must reserve exactly one pre-sized batch \
             (bytes, blocks)"
        );
    }

    // Case 2: the rescue thread recycles each batch, so nothing is allocated.
    let mut recycled = TickWriter::for_test(Feed::Dhan);
    let (sink, rx) = recycled.split_rescue_offload();
    // Warm-up outside the measured cases: one batch in circulation.
    let _ = hand_off(
        &mut recycled,
        &rx,
        Some(&sink),
        &mut i,
        (u64::MAX, u64::MAX),
    );
    for _ in 0..HAND_OFFS {
        let got = hand_off(&mut recycled, &rx, Some(&sink), &mut i, (0, 0));
        assert_eq!(
            got,
            (0, 0),
            "a hand-off with a recycled batch waiting must allocate nothing (bytes, blocks)"
        );
    }
}
