//! DHAT allocation gate for the late-attach seed queue and idle seed steps.
//!
//! The fixture batches are built before profiling, as they are by the attach
//! task in production. The measured window moves them into the bounded queue,
//! exercises its full-queue inline fallback, and drains every bounded step.
//! Nonvacuity checks require all 24,640 instruments and all 48 steps to complete.
//!
//! The budget stays 0 blocks/0 bytes. The original 2026-10-28 mutation removing
//! queue preallocation produced 2 blocks / 384 bytes, so a small allowance would
//! hide the regression this case is intended to catch.
//!
//! Each heap-allocation case now has one integration-test binary. DHAT 0.3.3
//! documents that a mutex between test bodies cannot isolate process-global
//! allocation counts from parallel libtest bookkeeping. The level/book buffer
//! cases retain their bodies and zero budgets in dhat_depth_level_row_presize
//! and dhat_depth_book_row_presize. This fixes that known isolation weakness;
//! the historical 6-block/389-byte failure has no saved allocation stacks and
//! is not attributed to libtest by this source change.
//!
//! Ungated and run by both Test(app) and the explicit DHAT CI target list.
//! Coverage keeps the existing dhat_ exclusion. No budget or oracle is relaxed.
//! Run: cargo test -p tickvault-app --test dhat_idle_seed_step

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_app::dhan_feed_stack::{LiveIngest, SEED_QUEUE_BATCHES, SEED_STEP_INSTRUMENTS};
use tickvault_common::feed::Feed;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;
use tickvault_storage::tick_persistence::TickWriter;

/// Blocks the measured window may allocate: the measured 0 (see the module
/// doc for why not the house 8).
const BUDGET_BLOCKS: u64 = 0;
/// Bytes the measured window may allocate: the measured 0.
const BUDGET_BYTES: u64 = 0;

/// Instruments per queued batch: six full steps of `SEED_STEP_INSTRUMENTS`,
/// so every batch is seeded over several idle steps (the sliced path), never
/// in one.
const BATCH_INSTRUMENTS: usize = SEED_STEP_INSTRUMENTS * 6;

/// Detector slots: the authorized ceiling, as the live lane sizes it, and
/// above the 8 × 3,072 = 24,576 instruments queued here plus the inline
/// batch, so no seed is refused for capacity.
const DETECTOR_CAPACITY: usize = 25_000;

/// Instruments in the batch sent while the queue is full (the inline
/// fallback). Small, so the total stays under `DETECTOR_CAPACITY`.
const INLINE_INSTRUMENTS: usize = 64;

/// Retained local measurement lock. This binary now contains one heap case;
/// the process boundary, not this lock, isolates it from the other cases.
static DHAT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A batch of `n` distinct NSE_FNO contracts starting at `first_id`.
fn seed_batch(first_id: u64, n: usize) -> Vec<SubscribeInstrument> {
    (0..n as u64)
        .map(|i| SubscribeInstrument {
            security_id: first_id + i,
            segment: ExchangeSegment::NseFno,
        })
        .collect()
}

#[test]
fn queue_seed_batch_and_step_idle_work_seed_do_not_allocate_per_step() {
    let _serial = DHAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_detector_capacity(DETECTOR_CAPACITY);
    assert_eq!(
        ingest.tracked_instruments(),
        0,
        "a fresh ingest tracks nothing"
    );
    assert!(
        !ingest.idle_work_pending(),
        "a fresh ingest has no idle work"
    );

    // Every fixture built BEFORE the profiler starts (the attach task builds
    // these in production; the drain only moves them in).
    let mut batches: Vec<Vec<SubscribeInstrument>> = (0..SEED_QUEUE_BATCHES)
        .map(|b| seed_batch(100_000 + (b * BATCH_INSTRUMENTS) as u64, BATCH_INSTRUMENTS))
        .collect();
    let inline_batch = seed_batch(900_000, INLINE_INSTRUMENTS);
    let mut finished: Vec<(usize, usize)> = Vec::with_capacity(SEED_QUEUE_BATCHES);

    let profiler = dhat::Profiler::builder().testing().build();

    let mut queued = 0usize;
    for batch in batches.drain(..) {
        assert!(ingest.seed_queue_has_room());
        assert_eq!(
            ingest.queue_seed_batch(batch, 1_000),
            None,
            "queued, not inline"
        );
        queued += 1;
    }
    // The queue is full: a further call seeds inline (the pre-DT-A1 path)
    // rather than grow the pre-sized queue.
    assert!(!ingest.seed_queue_has_room());
    let inline = ingest.queue_seed_batch(inline_batch, 1_000);

    let mut steps = 0usize;
    while ingest.idle_work_pending() {
        let before = ingest.tracked_instruments();
        let step = ingest.step_idle_work();
        let after = ingest.tracked_instruments();
        assert!(
            after - before <= SEED_STEP_INSTRUMENTS,
            "a step seeded {} instruments, past the {SEED_STEP_INSTRUMENTS} bound",
            after - before
        );
        if let Some(done) = step.seed_done {
            finished.push(done);
        }
        steps += 1;
    }
    // One more call with nothing queued: the empty branch, also free.
    let idle = ingest.step_idle_work();

    let stats = dhat::HeapStats::get();
    drop(profiler);

    // NON-VACUITY FIRST: every queued instrument reached the detector, over
    // the sliced path, and every batch reported its own completion.
    assert_eq!(queued, SEED_QUEUE_BATCHES);
    assert_eq!(inline, Some((INLINE_INSTRUMENTS, INLINE_INSTRUMENTS)));
    assert_eq!(
        steps,
        SEED_QUEUE_BATCHES * BATCH_INSTRUMENTS.div_ceil(SEED_STEP_INSTRUMENTS),
        "each batch must take several bounded steps"
    );
    assert_eq!(finished.len(), SEED_QUEUE_BATCHES);
    assert!(
        finished
            .iter()
            .all(|&d| d == (BATCH_INSTRUMENTS, BATCH_INSTRUMENTS)),
        "every batch must seed all of its instruments: {finished:?}"
    );
    assert_eq!(
        ingest.tracked_instruments(),
        SEED_QUEUE_BATCHES * BATCH_INSTRUMENTS + INLINE_INSTRUMENTS,
        "the detector must hold every seeded instrument"
    );
    assert_eq!(
        ingest.detector_refused(),
        0,
        "no seed may be refused for capacity"
    );
    assert!(idle.seed_done.is_none() && idle.catch_up_done.is_none());

    eprintln!(
        "dhat_idle_seed_step: {steps} idle steps, {} instruments seeded, \
         {} blocks / {} bytes allocated",
        ingest.tracked_instruments(),
        stats.total_blocks,
        stats.total_bytes
    );
    assert!(
        stats.total_blocks == BUDGET_BLOCKS && stats.total_bytes == BUDGET_BYTES,
        "THE SEED QUEUE ALLOCATED ON THE DRAIN.\n\n{steps} idle steps and \
         {queued} queued batches allocated {} blocks ({} bytes); the budget is \
         {BUDGET_BLOCKS} blocks / {BUDGET_BYTES} bytes. The queue is pre-sized \
         to SEED_QUEUE_BATCHES and the detector at the ceiling, so neither may \
         grow here. Find the allocation; do NOT raise the budget.",
        stats.total_blocks,
        stats.total_bytes
    );
}
