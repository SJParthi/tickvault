//! DHAT zero-allocation gate for inline depth level rows below the pre-size.
//!
//! One heap case per integration-test process follows DHAT 0.3.3's documented
//! isolation requirement. A mutex cannot exclude parallel libtest bookkeeping.
//! The original case body, warm-up, 400 frames, 16,000-row/1,600-tick assertions
//! and exact 0-block/0-byte budget are retained without weakening.
//! Allocations beyond the configured buffer pre-size are outside this gate.
//!
//! Run: cargo test -p tickvault-app --test dhat_depth_level_row_presize

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_app::dhan_feed_stack::LiveIngest;
use tickvault_common::feed::Feed;
use tickvault_storage::tick_persistence::TickWriter;

/// The measured allocation ceiling remains exactly zero.
const BUDGET_BLOCKS: u64 = 0;
const BUDGET_BYTES: u64 = 0;

/// Retains the original case body; this binary contains only one heap case.
static DHAT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
