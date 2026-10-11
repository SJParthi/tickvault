//! DHAT zero-allocation gate for the **depth frame drain** —
//! `dhan_feed_stack::drain_depth_frame`, the function every depth-20 and
//! depth-200 frame passes through on the way from the parser to the writer.
//!
//! # Why this test exists (audit PROOF-10, 2026-10-11)
//!
//! Depth is about 24 times the tick volume. The depth PARSER has a DHAT gate
//! (`crates/core/tests/dhat_depth_packet_zero_alloc.rs`) and the depth WRITER
//! has one (`crates/storage/tests/dhat_depth_append_zero_alloc.rs`), but the
//! app-side seam that joins them was measured by nothing. That is the shape
//! the tick lane had before 2026-08-14, when a per-tick allocation sat in the
//! one function between two gated pieces (`dhat_live_ingest_seam.rs` records
//! it). This gate closes the same gap for depth.
//!
//! # What it proves, and what it does not
//!
//! It proves the walk allocates nothing PER PACKET or PER LEVEL: header
//! split, ghost classification, per-level plausibility checks and the row
//! appends. It does NOT prove O(1): a depth packet is O(levels) by nature
//! (20 or 200), which CLAUDE.md records for `parse_depth_packet`.
//!
//! # Budget — blocks, not bytes
//!
//! Every level becomes a row in the writer's ILP buffer, so bytes are
//! SUPPOSED to grow with the level count, by amortised buffer doublings.
//! Blocks are the honest signal: one allocation per packet would add
//! thousands of blocks, one per level tens of thousands. The ceiling is 256
//! blocks over 10,000 packets (200,000 depth-20 levels), and separately over
//! 1,000 depth-200 packets (200,000 levels).
//!
//! CI: un-gated (no `#![cfg(feature = "dhat")]`), the house pattern, so the
//! Test (app) lane and the DHAT lane both run it, and the Coverage lane skips
//! it by the `dhat_` name prefix. Listed in the ci.yml DHAT drift list.
//!
//! Run: `cargo test -p tickvault-app --test dhat_depth_drain`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_app::dhan_feed_stack::{DepthIngest, counters, drain_depth_frame};
use tickvault_common::constants::{DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID};
use tickvault_core::parser::depth::DepthFeedKind;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;

/// Ceiling on heap blocks inside one measured window. If this goes red, find
/// the allocation; do NOT raise the ceiling.
const MAX_BLOCKS: u64 = 256;

/// A receipt instant inside a trading session (UTC nanos), the same instant
/// the drain's own unit tests use, so the gate measures the write path and
/// not a refusal path.
const RECEIPT_NANOS: i64 = 1_779_355_000_000_000_000;

/// NSE_FNO on the wire.
const SEGMENT_NSE_FNO: u8 = 2;

/// Serialises the profiled tests: `dhat::Profiler` is process-global, so two
/// profiled tests running at once would measure each other.
static DHAT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One depth packet in the wire layout `parse_depth_packet` reads: a 12-byte
/// header (length, feed code, segment, security id, and for depth-200 the row
/// count) followed by `rows` 16-byte levels (f64 price, u32 qty, u32 orders).
fn depth_packet(
    security_id: u32,
    feed_code: u8,
    rows: u32,
    row_count_in_header: bool,
    n: u64,
) -> Vec<u8> {
    let levels = rows as usize;
    let mut p = vec![0u8; 12 + levels * 16];
    let len = u16::try_from(p.len()).unwrap_or(u16::MAX);
    p[0..2].copy_from_slice(&len.to_le_bytes());
    p[2] = feed_code;
    p[3] = SEGMENT_NSE_FNO;
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    if row_count_in_header {
        p[8..12].copy_from_slice(&rows.to_le_bytes());
    }
    for i in 0..levels {
        let base = 12 + i * 16;
        // Vary the book per frame so nothing can be short-circuited.
        let price = 100.0_f64 + i as f64 * 0.05 + (n % 97) as f64 * 0.05;
        let qty = 10 * (i as u32 + 1) + (n % 7) as u32;
        let orders = i as u32 + 1;
        p[base..base + 8].copy_from_slice(&price.to_le_bytes());
        p[base + 8..base + 12].copy_from_slice(&qty.to_le_bytes());
        p[base + 12..base + 16].copy_from_slice(&orders.to_le_bytes());
    }
    p
}

fn frame(bytes: Vec<u8>, endpoint: DhanEndpointType, seq: u64) -> CapturedFrame {
    CapturedFrame {
        seq,
        endpoint,
        connection_index: 5,
        received_at: std::time::Instant::now(),
        received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
            std::time::Instant::now(),
        ),
        wal_backed: true,
        bytes: bytes.into(),
    }
}

/// A depth-20 frame stacking four packets (two contracts, bid and ask each),
/// the real shape: a single-packet frame would not exercise the multi-packet
/// walk.
fn depth20_frame(seq: u64, n: u64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(4 * 332);
    for (sid, code) in [
        (60_001u32, DEEP_DEPTH_FEED_CODE_BID),
        (60_001, DEEP_DEPTH_FEED_CODE_ASK),
        (60_002, DEEP_DEPTH_FEED_CODE_BID),
        (60_002, DEEP_DEPTH_FEED_CODE_ASK),
    ] {
        bytes.extend_from_slice(&depth_packet(sid, code, 20, false, n));
    }
    frame(bytes, DhanEndpointType::Depth20, seq)
}

/// A depth-200 frame: one packet of 200 levels, alternating sides.
fn depth200_frame(seq: u64, n: u64) -> CapturedFrame {
    let code = if n.is_multiple_of(2) {
        DEEP_DEPTH_FEED_CODE_BID
    } else {
        DEEP_DEPTH_FEED_CODE_ASK
    };
    frame(
        depth_packet(70_001, code, 200, true, n),
        DhanEndpointType::Depth200,
        seq,
    )
}

/// Runs `frames` through the drain inside a profiled window and returns
/// `(blocks, bytes, rows)`. Warm-up frames run first, OUTSIDE the window, so
/// one-time costs (metric registration, first buffer sizing) are not
/// mistaken for per-packet ones.
fn measure(
    warm: &[CapturedFrame],
    frames: &[CapturedFrame],
    kind: DepthFeedKind,
) -> (u64, u64, u64) {
    let mut depth = DepthIngest::for_test();
    let c = counters();
    for f in warm {
        let _ = drain_depth_frame(&mut depth, f, RECEIPT_NANOS, kind, c);
    }
    let mut rows = 0_u64;
    let profiler = dhat::Profiler::builder().testing().build();
    for f in frames {
        rows += drain_depth_frame(&mut depth, f, RECEIPT_NANOS, kind, c).rows;
    }
    let stats = dhat::HeapStats::get();
    drop(profiler);
    (stats.total_blocks, stats.total_bytes, rows)
}

#[test]
fn depth20_drain_does_not_allocate_per_packet() {
    let _serial = DHAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    const FRAMES: u64 = 2_500;
    let warm: Vec<CapturedFrame> = (0..4).map(|n| depth20_frame(n, n)).collect();
    let frames: Vec<CapturedFrame> = (0..FRAMES).map(|n| depth20_frame(1_000 + n, n)).collect();
    let (blocks, bytes, rows) = measure(&warm, &frames, DepthFeedKind::Twenty);

    // Non-vacuity: every level of every packet became a row. A refusal path
    // allocates less and would pass the budget while measuring nothing.
    assert_eq!(
        rows,
        FRAMES * 4 * 20,
        "the drain wrote {rows} rows for {FRAMES} frames of 4 x 20 levels; \
         the frames are being refused and the budget below would measure an \
         empty walk"
    );
    assert!(
        blocks <= MAX_BLOCKS,
        "PER-PACKET ALLOCATION ON THE DEPTH-20 DRAIN.\n\n{} packets ({rows} \
         levels) allocated {blocks} blocks ({bytes} bytes); the budget is \
         {MAX_BLOCKS} blocks. Bytes grow with rows by design; blocks must not. \
         Find the allocation, do NOT raise the ceiling.",
        FRAMES * 4
    );
}

#[test]
fn depth200_drain_does_not_allocate_per_packet() {
    let _serial = DHAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    const FRAMES: u64 = 1_000;
    let warm: Vec<CapturedFrame> = (0..4).map(|n| depth200_frame(n, n)).collect();
    let frames: Vec<CapturedFrame> = (0..FRAMES).map(|n| depth200_frame(1_000 + n, n)).collect();
    let (blocks, bytes, rows) = measure(&warm, &frames, DepthFeedKind::TwoHundred);

    assert_eq!(
        rows,
        FRAMES * 200,
        "the drain wrote {rows} rows for {FRAMES} depth-200 packets of 200 \
         levels; the frames are being refused and the budget below would \
         measure an empty walk"
    );
    assert!(
        blocks <= MAX_BLOCKS,
        "PER-PACKET ALLOCATION ON THE DEPTH-200 DRAIN.\n\n{FRAMES} packets \
         ({rows} levels) allocated {blocks} blocks ({bytes} bytes); the budget \
         is {MAX_BLOCKS} blocks. Find the allocation, do NOT raise the ceiling."
    );
}
