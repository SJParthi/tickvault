//! DHAT allocation test for deep depth parsing.
//!
//! Unlike the standard feed parser (which is zero-allocation), the deep depth
//! parser intentionally allocates a Vec for depth levels. This is marked as
//! O(1) EXEMPT in the source code because:
//! - The allocation count is bounded (exactly 1 Vec per parse call)
//! - Level count is fixed (20 for twenty-depth, variable but bounded for 200)
//! - This is NOT on the hot tick path — it uses a separate WebSocket
//!
//! This test verifies the allocation count matches expectations:
//! - parse_twenty_depth_packet: exactly 1 allocation per call (the Vec)
//! - parse_two_hundred_depth_packet: exactly 1 allocation per call (the Vec)

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use dhan_live_trader_common::constants::{
    DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID, DEEP_DEPTH_HEADER_SIZE,
    DEEP_DEPTH_LEVEL_SIZE, TWENTY_DEPTH_LEVELS,
};

// ---------------------------------------------------------------------------
// Helpers — build valid deep depth packets
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_twenty_depth_packet(feed_code: u8, security_id: u32) -> Vec<u8> {
    let packet_size = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
    let mut buf = vec![0u8; packet_size];

    // 12-byte header: msg_length(u16) + feed_code(u8) + segment(u8) + security_id(u32) + sequence(u32)
    buf[0..2].copy_from_slice(&(packet_size as u16).to_le_bytes());
    buf[2] = feed_code;
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&1_u32.to_le_bytes());

    // Fill levels with valid data
    for i in 0..TWENTY_DEPTH_LEVELS {
        let base = DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE;
        let price = 24500.0_f64 - (i as f64) * 0.05;
        buf[base..base + 8].copy_from_slice(&price.to_le_bytes());
        buf[base + 8..base + 12].copy_from_slice(&100_u32.to_le_bytes());
        buf[base + 12..base + 16].copy_from_slice(&10_u32.to_le_bytes());
    }

    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_two_hundred_depth_packet(feed_code: u8, security_id: u32, row_count: u32) -> Vec<u8> {
    let packet_size = DEEP_DEPTH_HEADER_SIZE + (row_count as usize) * DEEP_DEPTH_LEVEL_SIZE;
    let mut buf = vec![0u8; packet_size];

    buf[0..2].copy_from_slice(&(packet_size as u16).to_le_bytes());
    buf[2] = feed_code;
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&row_count.to_le_bytes());

    for i in 0..(row_count as usize) {
        let base = DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE;
        let price = 24500.0_f64 - (i as f64) * 0.05;
        if base + 16 <= buf.len() {
            buf[base..base + 8].copy_from_slice(&price.to_le_bytes());
            buf[base + 8..base + 12].copy_from_slice(&100_u32.to_le_bytes());
            buf[base + 12..base + 16].copy_from_slice(&10_u32.to_le_bytes());
        }
    }

    buf
}

// ---------------------------------------------------------------------------
// DHAT Test — verify bounded allocations for deep depth parsing
// ---------------------------------------------------------------------------

/// Verify that deep depth parsing has exactly the expected allocation count.
///
/// O(1) EXEMPT: Deep depth parsing allocates exactly 1 Vec per parse call.
/// This is intentional — the variable level count (20 vs up to 200) makes
/// a fixed-size array impractical. The allocation is bounded and predictable.
///
/// DHAT only allows a single profiler per process, so all checks are in one test.
#[test]
fn dhat_deep_depth_bounded_allocations() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Pre-allocate ALL packets before measuring
    let bid_20 = make_twenty_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 52432);
    let ask_20 = make_twenty_depth_packet(DEEP_DEPTH_FEED_CODE_ASK, 52432);
    let bid_200 = make_two_hundred_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 13, 50);
    let ask_200 = make_two_hundred_depth_packet(DEEP_DEPTH_FEED_CODE_ASK, 13, 100);
    let zero_rows = make_two_hundred_depth_packet(DEEP_DEPTH_FEED_CODE_BID, 13, 0);

    // ---- Measure: count allocations from here ----
    let stats_before = dhat::HeapStats::get();

    // 1. Twenty-depth bid parse (1 Vec allocation expected)
    let result = dhan_live_trader_core::parser::deep_depth::parse_twenty_depth_packet(&bid_20, 0);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().levels.len(), 20);

    // 2. Twenty-depth ask parse (1 Vec allocation expected)
    let result = dhan_live_trader_core::parser::deep_depth::parse_twenty_depth_packet(&ask_20, 0);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().levels.len(), 20);

    // 3. Two-hundred-depth bid parse, 50 rows (1 Vec allocation expected)
    let result =
        dhan_live_trader_core::parser::deep_depth::parse_two_hundred_depth_packet(&bid_200, 0);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().levels.len(), 50);

    // 4. Two-hundred-depth ask parse, 100 rows (1 Vec allocation expected)
    let result =
        dhan_live_trader_core::parser::deep_depth::parse_two_hundred_depth_packet(&ask_200, 0);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().levels.len(), 100);

    // 5. Two-hundred-depth with 0 rows (1 Vec allocation expected, empty Vec)
    let result =
        dhan_live_trader_core::parser::deep_depth::parse_two_hundred_depth_packet(&zero_rows, 0);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().levels.len(), 0);

    // ---- End measurement ----
    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    // O(1) EXEMPT: Each parse_*_depth_packet call allocates 1 Vec when level_count > 0.
    // Vec::with_capacity(0) does NOT allocate on the heap, so the zero-row case
    // contributes 0 allocations.
    // 4 non-empty parses = 4 allocations, 1 zero-row parse = 0 allocations.
    let expected_allocs = 4;
    assert_eq!(
        allocs_during, expected_allocs,
        "deep depth parsing allocated {} blocks, expected exactly {} \
         (1 Vec per non-empty parse call x 4 calls + 0 for empty Vec).\n\
         This is O(1) EXEMPT — bounded Vec allocation for variable-length depth levels.",
        allocs_during, expected_allocs
    );
}
