//! DHAT zero-allocation gate for inline depth book rows below the pre-size.
//!
//! One heap case per integration-test process follows DHAT 0.3.3's documented
//! isolation requirement. A mutex cannot exclude parallel libtest bookkeeping.
//! The original case body, warm-up, 400 frames, 16,000-level/1,600-tick assertions,
//! actual nonempty book-buffer check and exact 0-block/0-byte budget remain.
//! The historical 6-block/389-byte failure is preserved as unattributed because
//! its testing-mode profiler was dropped before the ordinary failing assert.
//!
//! Run: cargo test -p tickvault-app --test dhat_depth_book_row_presize

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
