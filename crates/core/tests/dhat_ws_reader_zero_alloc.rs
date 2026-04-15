//! STAGE-D P7.5 — DHAT zero-allocation test for the WS reader hot path.
//!
//! Verifies Principle #1 ("Zero allocation on hot path") for the two
//! cheapest parts of the WS read loop:
//!
//!   1. `AtomicU64::fetch_add(1, Ordering::Relaxed)` — the activity
//!      watchdog counter bump executed on every inbound frame. MUST
//!      allocate zero bytes.
//!
//!   2. `dispatch_frame()` — the binary parser called after the WAL
//!      append to route the frame into the tick processor. MUST
//!      allocate zero bytes.
//!
//! The third step in the read loop — `WsFrameSpill::append` — takes
//! an owned `Vec<u8>` (one allocation per frame). That allocation is
//! deliberate and bounded (`O(frame_len)` ≤ 162 B for Full packets),
//! so it is NOT covered by this test. The hot-path-reviewer rule marks
//! it `O(1) EXEMPT` inside the read loop with a justification.
//!
//! DHAT allows only one profiler per process, so all hot-path
//! assertions are consolidated into a single test.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_FNO, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
};
use tickvault_core::parser::dispatch_frame;

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
    buf[8..12].copy_from_slice(&100.0_f32.to_le_bytes());
    buf
}

/// Simulate the WS read loop per-frame "tail" that runs AFTER the
/// durable WAL append: counter bump + dispatch. Returns the parsed
/// frame wrapped in `black_box` so the compiler cannot elide the work.
#[inline(always)]
fn ws_reader_tail(counter: &AtomicU64, packet: &[u8]) -> bool {
    counter.fetch_add(1, Ordering::Relaxed);
    let parsed = dispatch_frame(black_box(packet), black_box(0));
    black_box(parsed).is_ok()
}

/// Zero-allocation verification for the WS reader hot path tail.
///
/// Pre-allocates all packets and the counter before entering the
/// measured section so that only the counter bump + dispatch calls
/// contribute to the delta.
#[test]
fn dhat_ws_reader_tail_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Pre-allocate inputs BEFORE the measurement window.
    let counter = AtomicU64::new(0);
    let ticker_pkt = make_ticker_bytes(52432, 245.50, 1_700_000_000);
    let quote_pkt = make_quote_bytes(52432);
    let ticker_burst: Vec<Vec<u8>> = (0..100)
        .map(|i| make_ticker_bytes(50000 + i, 100.0 + i as f32, 1_700_000_000 + i))
        .collect();

    // ---- Measurement window begins ----
    let before = dhat::HeapStats::get();

    // Single ticker: counter bump + dispatch.
    assert!(ws_reader_tail(&counter, &ticker_pkt));

    // Single quote: counter bump + dispatch.
    assert!(ws_reader_tail(&counter, &quote_pkt));

    // Burst: 100 counter bumps + 100 dispatches (realistic fan-in
    // representing a busy 10k/s frame arrival window).
    for pkt in &ticker_burst {
        assert!(ws_reader_tail(&counter, pkt));
    }

    // Verify the counter advanced as expected (1 + 1 + 100 = 102).
    assert_eq!(counter.load(Ordering::Relaxed), 102);

    let after = dhat::HeapStats::get();
    // ---- Measurement window ends ----

    let allocs_during = after.total_blocks.saturating_sub(before.total_blocks);

    assert_eq!(
        allocs_during, 0,
        "WS reader tail (counter + dispatch) allocated {} blocks — PRINCIPLE #1 VIOLATED.\n\
         Only the bounded WAL `data.to_vec()` is permitted; everything else must be \
         zero-allocation. Check for hidden String/Vec/Box creation in dispatch_frame \
         or the hot-path counter bump.",
        allocs_during
    );
}
