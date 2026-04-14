//! DHAT allocation tests — enforce zero-allocation on the hot path.
//!
//! These tests verify Principle #1: "Zero allocation on hot path".
//! They use DHAT (Dynamic Heap Analysis Tool) to count heap allocations
//! during critical parsing and dispatch operations.
//!
//! If any hot-path function allocates, these tests fail — mechanically
//! enforcing the zero-allocation invariant.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_FNO, FULL_QUOTE_PACKET_SIZE, MARKET_DEPTH_PACKET_SIZE, QUOTE_PACKET_SIZE,
    TICKER_PACKET_SIZE,
};

// ---------------------------------------------------------------------------
// Helpers — build valid binary packets for each type
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_ticker_bytes(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let mut buf = vec![0u8; TICKER_PACKET_SIZE];
    buf[0] = 2; // RESPONSE_CODE_TICKER
    buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[12..16].copy_from_slice(&ltt.to_le_bytes());
    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_quote_bytes(security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = 4; // RESPONSE_CODE_QUOTE
    buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    // LTP at offset 8
    buf[8..12].copy_from_slice(&100.0_f32.to_le_bytes());
    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_depth_bytes(security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; MARKET_DEPTH_PACKET_SIZE];
    buf[0] = 3; // RESPONSE_CODE_MARKET_DEPTH
    buf[1..3].copy_from_slice(&(MARKET_DEPTH_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    // LTP at offset 8
    buf[8..12].copy_from_slice(&250.5_f32.to_le_bytes());
    // Depth levels: fill with valid finite f32 prices
    for level in 0..5 {
        let base = 12 + level * 20;
        // bid_qty, ask_qty
        buf[base..base + 4].copy_from_slice(&100_u32.to_le_bytes());
        buf[base + 4..base + 8].copy_from_slice(&100_u32.to_le_bytes());
        // bid_orders, ask_orders
        buf[base + 8..base + 10].copy_from_slice(&10_u16.to_le_bytes());
        buf[base + 10..base + 12].copy_from_slice(&10_u16.to_le_bytes());
        // bid_price, ask_price
        let bid = 249.0_f32 + level as f32;
        let ask = 251.0_f32 + level as f32;
        buf[base + 12..base + 16].copy_from_slice(&bid.to_le_bytes());
        buf[base + 16..base + 20].copy_from_slice(&ask.to_le_bytes());
    }
    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_full_bytes(security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
    buf[0] = 8; // RESPONSE_CODE_FULL
    buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    // LTP at offset 8
    buf[8..12].copy_from_slice(&300.0_f32.to_le_bytes());
    // Depth starts at offset 62 for full packet
    for level in 0..5 {
        let base = 62 + level * 20;
        if base + 20 <= buf.len() {
            buf[base..base + 4].copy_from_slice(&100_u32.to_le_bytes());
            buf[base + 4..base + 8].copy_from_slice(&100_u32.to_le_bytes());
            buf[base + 8..base + 10].copy_from_slice(&10_u16.to_le_bytes());
            buf[base + 10..base + 12].copy_from_slice(&10_u16.to_le_bytes());
            let bid = 299.0_f32 + level as f32;
            let ask = 301.0_f32 + level as f32;
            buf[base + 12..base + 16].copy_from_slice(&bid.to_le_bytes());
            buf[base + 16..base + 20].copy_from_slice(&ask.to_le_bytes());
        }
    }
    buf
}

// ---------------------------------------------------------------------------
// DHAT Test — single profiler, all checks in one test (DHAT allows one profiler per process)
// ---------------------------------------------------------------------------

/// Verify zero allocations across all hot-path parser functions.
///
/// DHAT only allows a single profiler per process, so we consolidate all
/// allocation checks into one test. Each section pre-allocates its data,
/// then measures the dispatch_frame call in isolation.
#[test]
fn dhat_all_parsers_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Pre-allocate ALL packets before measuring anything
    let ticker_pkt = make_ticker_bytes(52432, 245.50, 1_700_000_000);
    let quote_pkt = make_quote_bytes(52432);
    let depth_pkt = make_depth_bytes(52432);
    let full_pkt = make_full_bytes(52432);
    let burst_pkts: Vec<Vec<u8>> = (0..100)
        .map(|i| make_ticker_bytes(50000 + i, 100.0 + i as f32, 1_700_000_000 + i))
        .collect();
    let empty: &[u8] = &[];

    // ---- Measure: all allocations from here must be zero ----
    let stats_before = dhat::HeapStats::get();

    // 1. Ticker parse
    assert!(tickvault_core::parser::dispatch_frame(&ticker_pkt, 0).is_ok());

    // 2. Quote parse
    assert!(tickvault_core::parser::dispatch_frame(&quote_pkt, 0).is_ok());

    // 3. Market depth parse
    assert!(tickvault_core::parser::dispatch_frame(&depth_pkt, 0).is_ok());

    // 4. Full packet parse
    assert!(tickvault_core::parser::dispatch_frame(&full_pkt, 0).is_ok());

    // 5. Burst: 100 ticker parses
    for pkt in &burst_pkts {
        assert!(tickvault_core::parser::dispatch_frame(pkt, 0).is_ok());
    }

    // 6. Error path: empty frame
    assert!(tickvault_core::parser::dispatch_frame(empty, 0).is_err());

    // ---- End measurement ----
    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert_eq!(
        allocs_during, 0,
        "hot-path parsers allocated {} blocks — PRINCIPLE #1 VIOLATED.\n\
         Breakdown: ticker + quote + depth + full + 100-burst + error path\n\
         All dispatch_frame calls must be zero-allocation.",
        allocs_during
    );
}
