//! Audit Finding #10 (2026-05-03): the `dispatcher::dispatch_frame`
//! and `dispatcher::dispatch_deep_depth_frame` hot-path entry points
//! had no end-to-end DHAT zero-allocation ratchet. The per-packet
//! parsers (ticker / quote / full / OI / prev_close / depth) are
//! covered by `dhat_allocation.rs` and `dhat_deep_depth.rs`, but the
//! dispatcher itself — which classifies every incoming WS frame and
//! routes to the right parser — was not pinned.
//!
//! Investigation found the 5 alloc sites flagged by the audit
//! (lines 793, 805, 874, 998, 1027 of dispatcher.rs) are all inside
//! the `#[cfg(test)] mod tests` block — test helpers, not production
//! hot-path. The actual hot path is `dispatch_frame` (line 123) and
//! `dispatch_deep_depth_frame` (line 204).
//!
//! This test pins the zero-allocation contract end-to-end across
//! 10,000 dispatch calls per packet type using the same
//! `Profiler::new_heap()` + `HeapStats` before/after pattern as the
//! existing `dhat_depth_sequence_tracker.rs` test.

#![cfg(feature = "dhat")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const ITERATIONS: usize = 10_000;

/// Builds a valid 16-byte ticker packet (response code 2).
fn ticker_packet() -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // response code: ticker
    buf[1..3].copy_from_slice(&16u16.to_le_bytes()); // message length
    buf[3] = 1; // exchange segment: NSE_EQ
    buf[4..8].copy_from_slice(&12345u32.to_le_bytes()); // security id
    buf[8..12].copy_from_slice(&100.5f32.to_le_bytes()); // LTP
    buf[12..16].copy_from_slice(&1700000000u32.to_le_bytes()); // LTT
    buf
}

/// Builds a valid 50-byte quote packet (response code 4).
fn quote_packet() -> Vec<u8> {
    let mut buf = vec![0u8; 50];
    buf[0] = 4;
    buf[1..3].copy_from_slice(&50u16.to_le_bytes());
    buf[3] = 1;
    buf[4..8].copy_from_slice(&12345u32.to_le_bytes());
    buf[8..12].copy_from_slice(&100.5f32.to_le_bytes());
    buf
}

/// Builds a valid 12-byte OI packet (response code 5).
fn oi_packet() -> Vec<u8> {
    let mut buf = vec![0u8; 12];
    buf[0] = 5;
    buf[1..3].copy_from_slice(&12u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&52432u32.to_le_bytes());
    buf[8..12].copy_from_slice(&50000u32.to_le_bytes());
    buf
}

/// Builds a valid 332-byte 20-level depth bid packet (response code 41).
fn depth_bid_packet() -> Vec<u8> {
    let mut buf = vec![0u8; 332];
    buf[0..2].copy_from_slice(&332u16.to_le_bytes()); // depth header: msg_len first
    buf[2] = 41; // response code: depth bid
    buf[3] = 2; // exchange segment: NSE_FNO
    buf[4..8].copy_from_slice(&52432u32.to_le_bytes());
    buf
}

/// Audit Finding #10 — `dispatch_frame` for 3 frame types must allocate
/// zero blocks across 10K dispatches each (ticker / quote / OI).
#[test]
fn dispatch_frame_zero_allocation_across_10k_calls_per_code() {
    let _profiler = dhat::Profiler::new_heap();

    let ticker = ticker_packet();
    let quote = quote_packet();
    let oi = oi_packet();

    // Warm up: discard any lazy-init allocations (Prometheus counter
    // handles, etc.) so the measured window is steady-state.
    let _ = tickvault_core::parser::dispatch_frame(&ticker, 0);
    let _ = tickvault_core::parser::dispatch_frame(&quote, 0);
    let _ = tickvault_core::parser::dispatch_frame(&oi, 0);

    let stats_before = dhat::HeapStats::get();

    for _ in 0..ITERATIONS {
        let _ = tickvault_core::parser::dispatch_frame(&ticker, 0);
        let _ = tickvault_core::parser::dispatch_frame(&quote, 0);
        let _ = tickvault_core::parser::dispatch_frame(&oi, 0);
    }

    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert!(
        allocs_during == 0,
        "dispatch_frame allocated {allocs_during} blocks across \
         {ITERATIONS} iterations × 3 packet types — expected 0. \
         Audit Finding #10 (2026-05-03)."
    );
}

/// Same ratchet for `dispatch_deep_depth_frame` (depth-20/200 hot path).
#[test]
fn dispatch_deep_depth_frame_zero_allocation_across_10k_calls() {
    let _profiler = dhat::Profiler::new_heap();

    let depth = depth_bid_packet();

    // Warm up (one-time lazy init).
    let _ = tickvault_core::parser::dispatcher::dispatch_deep_depth_frame(&depth, 0);

    let stats_before = dhat::HeapStats::get();

    for _ in 0..ITERATIONS {
        let _ = tickvault_core::parser::dispatcher::dispatch_deep_depth_frame(&depth, 0);
    }

    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    // Note: `parse_twenty_depth_packet` allocates a single Vec for the
    // depth levels per call (covered by dhat_deep_depth.rs as O(1)
    // EXEMPT bounded). So we expect ITERATIONS allocs here, not 0 —
    // pin the upper bound at exactly ITERATIONS to catch any
    // additional dispatcher-introduced allocation.
    assert!(
        allocs_during <= ITERATIONS as u64,
        "dispatch_deep_depth_frame allocated {allocs_during} blocks \
         across {ITERATIONS} iterations — expected <= {ITERATIONS} \
         (one bounded Vec per call from parse_twenty_depth_packet). \
         Audit Finding #10."
    );
}
