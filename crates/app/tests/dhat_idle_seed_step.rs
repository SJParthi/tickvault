//! DHAT allocation gate for the DT-small drain paths added 2026-10-28:
//! the late-attach seed queue (`LiveIngest::queue_seed_batch` plus the seed
//! half of `LiveIngest::step_idle_work`, plan item DT-A1) and the depth row
//! buffer pre-size (`DepthLayout::row_buffer_presize_bytes`, DT-A2).
//!
//! # Why this file exists
//!
//! Both paths shipped with a claim in their docs: "a step never allocates"
//! and "no append regrows on the drain below the pre-size". Both were
//! REASONED (a `VecDeque` pre-sized to `SEED_QUEUE_BATCHES`, a detector
//! pre-sized at the ceiling, a buffer reserved at construction), not
//! measured: no DHAT gate ran either path. This file is the measurement.
//!
//! # What is measured, and what is not
//!
//! * The seed gate times queueing `SEED_QUEUE_BATCHES` batches, the inline
//!   fallback a call on a full queue takes, and every idle step until the
//!   queue is empty. The batches themselves are built BEFORE the profiler
//!   starts: in production the attach task builds them on its own task and
//!   the drain only MOVES them in, so their allocation is not the drain's.
//!   Freeing a finished batch's `Vec` (once per batch) is a deallocation and
//!   is not counted by `total_blocks`.
//! * The depth gate drains Full packets with inline depth (level rows, the
//!   dev and base layout) for fewer bytes than the level pre-size, so it
//!   measures exactly the claim "below the pre-size, no regrow". A batch past
//!   the pre-size still doubles (the honest limit the CLAUDE.md row states);
//!   this gate does not and cannot cover that.
//! * NOT measured: the drain's `select!` loop around these calls (tokio's
//!   own bookkeeping, the same reason `dhat_live_ingest_seam.rs` measures the
//!   seam rather than `run_frame_drain`).
//!
//! # Budget
//!
//! **0 blocks and 0 bytes** across the whole measured window: the MEASURED
//! figure for both gates (2026-10-28, debug, x86 dev container), pinned as
//! the ceiling.
//!
//! Tighter than the house budget for an allocation-free path (<= 8 blocks /
//! <= 1 KiB, `dhat_mark_forward.rs`, `dhat_wal_ring_sink_accept.rs`) on
//! purpose, because that budget does not bite on the regression this gate
//! exists for. BITE-TESTED 2026-10-28: with the seed queue built by
//! `VecDeque::new()` instead of `with_capacity(SEED_QUEUE_BATCHES)`, the
//! eight pushes grow it twice, **2 blocks / 384 bytes**, which the house
//! budget passes and this one fails. A per-step allocation would land at the
//! step count (48) or the instrument count (24,640); a depth regrow at a
//! doubling of megabytes. If this goes red, find the allocation; do NOT
//! raise the ceiling.
//!
//! CI ENFORCEMENT: un-gated (no `#![cfg(feature = "dhat")]`), the house
//! pattern, so the Test (app) lane runs it on every PR and the Coverage lane
//! skips it by the `dhat_` name prefix.
//!
//! Run: `cargo test -p tickvault-app --test dhat_idle_seed_step`

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

/// `dhat::Profiler` is process-global, so the two gates in this binary run
/// one at a time (the reason `dhat_live_ingest_seam.rs` carries the same lock).
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

/// An exchange timestamp inside a trading session (epoch seconds); a 1970
/// stamp is refused by the fold and would measure the refusal path.
const SESSION_EPOCH_SECS: u32 = 1_786_800_000;

/// A receipt inside the same session (UTC nanos, the IST offset is added
/// downstream): `SESSION_EPOCH_SECS` minus 5h30m, the value
/// `dhat_live_ingest_seam.rs` pins for the same reason.
const SESSION_RECEIPT_NANOS: i64 = (SESSION_EPOCH_SECS as i64 - 19_800) * 1_000_000_000;

/// Frames drained inside the window: 400 × 4 Full packets × 10 level rows
/// = 16,000 `market_depth` rows, under the level pre-size (the storage
/// crate pins it at >= 20,000 rows of 179 bytes), so the claim under test is
/// exactly "below the pre-size, no regrow".
const DEPTH_FRAMES: u64 = 400;

#[test]
fn depth_row_buffer_presize_holds_inline_level_rows_without_regrowing() {
    let _serial = DHAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use tickvault_app::dhan_feed_stack::DepthIngest;
    use tickvault_app::dhan_feed_stack::{counters, drain_main_feed_frame};
    use tickvault_common::constants::{
        FULL_OFFSET_DEPTH_START, FULL_QUOTE_PACKET_SIZE, MARKET_DEPTH_LEVEL_SIZE,
        MARKET_DEPTH_LEVELS, QUOTE_OFFSET_LTP, QUOTE_OFFSET_LTT, RESPONSE_CODE_FULL,
    };
    use tickvault_core::websocket::pool_budget::DhanEndpointType;
    use tickvault_core::websocket::pool_supervisor::CapturedFrame;
    use tickvault_storage::depth_persistence::{DEPTH_LEVEL_ROW_BUFFER_PRESIZE_BYTES, DepthLayout};

    assert_eq!(
        DepthLayout::Levels.row_buffer_presize_bytes(),
        DEPTH_LEVEL_ROW_BUFFER_PRESIZE_BYTES
    );

    fn full_packet(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
        let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
        buf[0] = RESPONSE_CODE_FULL;
        buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = 1; // NSE_EQ
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[QUOTE_OFFSET_LTP..QUOTE_OFFSET_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
        buf[QUOTE_OFFSET_LTT..QUOTE_OFFSET_LTT + 4].copy_from_slice(&ltt.to_le_bytes());
        for level in 0..MARKET_DEPTH_LEVELS {
            let base = FULL_OFFSET_DEPTH_START + level * MARKET_DEPTH_LEVEL_SIZE;
            let step = level as u32 + 1;
            buf[base..base + 4].copy_from_slice(&(100u32 * step).to_le_bytes());
            buf[base + 4..base + 8].copy_from_slice(&(200u32 * step).to_le_bytes());
        }
        buf
    }

    fn frame(seq: u64, n: u64) -> CapturedFrame {
        let mut bytes = Vec::with_capacity(4 * FULL_QUOTE_PACKET_SIZE);
        for i in 0..4u32 {
            bytes.extend_from_slice(&full_packet(
                5000 + i,
                100.0 + (n % 97) as f32 * 0.05,
                SESSION_EPOCH_SECS + (n % 600) as u32,
            ));
        }
        CapturedFrame {
            seq,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: (n % 4) as u8,
            received_at: std::time::Instant::now(),
            received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
                std::time::Instant::now(),
            ),
            wal_backed: true,
            bytes: bytes.into(),
        }
    }

    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_inline_depth(DepthIngest::for_test());
    let c = counters();
    // Warm-up OUTSIDE the window: first touch of an instrument takes its
    // slot and registers its metric keys, a per-universe cost.
    for n in 0..4u64 {
        let _ = drain_main_feed_frame(&mut ingest, &frame(n, n), SESSION_RECEIPT_NANOS, 1_000, c);
    }
    let frames: Vec<CapturedFrame> = (0..DEPTH_FRAMES).map(|n| frame(1_000 + n, n)).collect();

    let profiler = dhat::Profiler::builder().testing().build();
    let mut folded = 0u64;
    let mut depth_rows = 0u64;
    for f in &frames {
        let out = drain_main_feed_frame(&mut ingest, f, SESSION_RECEIPT_NANOS, 1_000, c);
        folded += out.folded;
        depth_rows += out.inline_depth_rows;
    }
    let stats = dhat::HeapStats::get();
    drop(profiler);

    // NON-VACUITY: every packet folded and wrote all ten level rows, so the
    // depth buffer really took 16,000 appends inside the window.
    assert_eq!(folded, DEPTH_FRAMES * 4, "every packet must fold");
    assert_eq!(
        depth_rows,
        DEPTH_FRAMES * 4 * 10,
        "every packet must append ten level rows"
    );

    eprintln!(
        "dhat_idle_seed_step depth: {depth_rows} level rows, {folded} ticks, \
         {} blocks / {} bytes allocated",
        stats.total_blocks, stats.total_bytes
    );
    assert!(
        stats.total_blocks == BUDGET_BLOCKS && stats.total_bytes == BUDGET_BYTES,
        "THE DEPTH ROW BUFFER REGREW BELOW ITS PRE-SIZE.\n\n{depth_rows} level \
         rows and {folded} ticks allocated {} blocks ({} bytes); the budget is \
         {BUDGET_BLOCKS} blocks / {BUDGET_BYTES} bytes. The buffer is reserved \
         to DepthLayout::row_buffer_presize_bytes at construction, so no append \
         below it may grow it.",
        stats.total_blocks,
        stats.total_bytes
    );
}

/// Frames drained by the book gate: 400 × 4 Full packets × 2 inline
/// 5-level `market_depth_book` rows = 3,200 rows, far under the book
/// pre-size (`DEPTH_BOOK_FLUSH_BYTES` + 1 MiB), so the claim under test is
/// the production layout's "below the pre-size, no regrow".
const BOOK_FRAMES: u64 = 400;

/// The book twin of `depth_row_buffer_presize_holds_inline_level_rows_without_regrowing`:
/// production runs the array-row layout (`[depth_storage] array_rows`, on
/// since 2026-10-10), two rows per Full packet, which the level gate above
/// does not drain. Same 0-block budget.
#[test]
fn test_depth_row_buffer_presize_holds_inline_book_rows_without_regrowing() {
    let _serial = DHAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use tickvault_app::dhan_feed_stack::DepthIngest;
    use tickvault_app::dhan_feed_stack::{counters, drain_main_feed_frame};
    use tickvault_common::constants::{
        FULL_OFFSET_DEPTH_START, FULL_QUOTE_PACKET_SIZE, MARKET_DEPTH_LEVEL_SIZE,
        MARKET_DEPTH_LEVELS, QUOTE_OFFSET_LTP, QUOTE_OFFSET_LTT, RESPONSE_CODE_FULL,
    };
    use tickvault_core::websocket::pool_budget::DhanEndpointType;
    use tickvault_core::websocket::pool_supervisor::CapturedFrame;
    use tickvault_storage::depth_persistence::{
        DEPTH_BOOK_FLUSH_BYTES, DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES, DepthLayout,
    };

    assert_eq!(
        DepthLayout::Book.row_buffer_presize_bytes(),
        DEPTH_BOOK_ROW_BUFFER_PRESIZE_BYTES
    );

    fn full_packet(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
        let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
        buf[0] = RESPONSE_CODE_FULL;
        buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = 1; // NSE_EQ
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[QUOTE_OFFSET_LTP..QUOTE_OFFSET_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
        buf[QUOTE_OFFSET_LTT..QUOTE_OFFSET_LTT + 4].copy_from_slice(&ltt.to_le_bytes());
        for level in 0..MARKET_DEPTH_LEVELS {
            let base = FULL_OFFSET_DEPTH_START + level * MARKET_DEPTH_LEVEL_SIZE;
            let step = level as u32 + 1;
            buf[base..base + 4].copy_from_slice(&(100u32 * step).to_le_bytes());
            buf[base + 4..base + 8].copy_from_slice(&(200u32 * step).to_le_bytes());
        }
        buf
    }

    fn frame(seq: u64, n: u64) -> CapturedFrame {
        let mut bytes = Vec::with_capacity(4 * FULL_QUOTE_PACKET_SIZE);
        for i in 0..4u32 {
            bytes.extend_from_slice(&full_packet(
                5000 + i,
                100.0 + (n % 97) as f32 * 0.05,
                SESSION_EPOCH_SECS + (n % 600) as u32,
            ));
        }
        CapturedFrame {
            seq,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: (n % 4) as u8,
            received_at: std::time::Instant::now(),
            received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
                std::time::Instant::now(),
            ),
            wal_backed: true,
            bytes: bytes.into(),
        }
    }

    // `since` = 1 ns: every frame here was received after it, so every
    // packet takes the book path (the `dhat_live_ingest_seam.rs` setup).
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_inline_depth(DepthIngest::for_test_book(1));
    let c = counters();
    for n in 0..4u64 {
        let _ = drain_main_feed_frame(&mut ingest, &frame(n, n), SESSION_RECEIPT_NANOS, 1_000, c);
    }
    let frames: Vec<CapturedFrame> = (0..BOOK_FRAMES).map(|n| frame(1_000 + n, n)).collect();

    let profiler = dhat::Profiler::builder().testing().build();
    let mut folded = 0u64;
    let mut depth_levels = 0u64;
    for f in &frames {
        let out = drain_main_feed_frame(&mut ingest, f, SESSION_RECEIPT_NANOS, 1_000, c);
        folded += out.folded;
        depth_levels += out.inline_depth_rows;
    }
    let stats = dhat::HeapStats::get();
    drop(profiler);

    // NON-VACUITY: every packet folded, both book rows (ten levels) were
    // stored for every packet, and the BOOK buffer holds them.
    assert_eq!(folded, BOOK_FRAMES * 4, "every packet must fold");
    assert_eq!(
        depth_levels,
        BOOK_FRAMES * 4 * 10,
        "the book sink must store all ten levels of every packet"
    );
    let book_bytes = ingest
        .depth_sink()
        .map_or(0, |d| d.pending_book_bytes().len());
    assert!(
        book_bytes > 0,
        "the gate must measure the book path, not the level path"
    );
    assert!(
        book_bytes < DEPTH_BOOK_FLUSH_BYTES,
        "below the cut: {book_bytes} bytes"
    );

    eprintln!(
        "dhat_idle_seed_step book: {depth_levels} levels in {book_bytes} book bytes, \
         {folded} ticks, {} blocks / {} bytes allocated",
        stats.total_blocks, stats.total_bytes
    );
    assert!(
        stats.total_blocks == BUDGET_BLOCKS && stats.total_bytes == BUDGET_BYTES,
        "THE DEPTH BOOK ROW BUFFER REGREW BELOW ITS PRE-SIZE.\n\n{depth_levels} \
         book levels and {folded} ticks allocated {} blocks ({} bytes); the budget \
         is {BUDGET_BLOCKS} blocks / {BUDGET_BYTES} bytes.",
        stats.total_blocks,
        stats.total_bytes
    );
}
