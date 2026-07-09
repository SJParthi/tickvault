//! STAGE-D P7.5 — DHAT zero-allocation test for the WS reader hot path.
//!
//! Verifies Principle #1 ("Zero allocation on hot path") for the THREE
//! parts of the WS read loop tail:
//!
//!   1. `AtomicU64::fetch_add(1, Ordering::Relaxed)` — the activity
//!      watchdog counter bump executed on every inbound frame. MUST
//!      allocate zero bytes.
//!
//!   2. The WAL hand-off — zero-tick-loss PR-8a (H1) replaced the
//!      per-frame `data.to_vec()` malloc with `data.clone()` on the
//!      `Bytes` the read loop already holds. A `Bytes` clone is an O(1)
//!      Arc refcount bump, NOT a copy, so the durable-WAL hand-off now
//!      allocates zero bytes. This test pins that — a regression back to
//!      `to_vec()` would make `allocs_during` non-zero and fail here.
//!
//!   3. `dispatch_frame()` — the binary parser called after the WAL
//!      append to route the frame into the tick processor. MUST
//!      allocate zero bytes.
//!
//! With H1, the ENTIRE read-loop tail (counter + WAL hand-off + dispatch)
//! is zero-allocation — no per-frame malloc remains on the hottest path.
//!
//! DHAT allows only one profiler per process, so all hot-path
//! assertions are consolidated into a single test.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_FNO, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
};
use tickvault_core::parser::dispatch_frame;

mod dhat_support;

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

/// Simulate the full WS read loop per-frame tail: counter bump + WAL
/// hand-off (`Bytes::clone`, the H1 zero-alloc replacement for `to_vec`)
/// + dispatch. Returns the parsed frame wrapped in `black_box` so the
/// compiler cannot elide the work.
#[inline(always)]
fn ws_reader_tail(counter: &AtomicU64, frame: &Bytes) -> bool {
    counter.fetch_add(1, Ordering::Relaxed);
    // H1: hand the durable WAL an O(1) Arc-refcount clone (NOT a copy).
    // This is exactly what `spill.append(WsType::LiveFeed, data.clone())`
    // does on the live read loop.
    let wal_handoff = black_box(frame.clone());
    let parsed = dispatch_frame(black_box(wal_handoff.as_ref()), black_box(0));
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

    // Pre-allocate inputs BEFORE the measurement window, in the SAME SHARED
    // representation tokio-tungstenite delivers. tungstenite 0.29 builds
    // `Message::Binary`'s `Bytes` via `in_buffer.split_to(len).freeze()`
    // (src/protocol/frame/mod.rs:199 + :227) — a `split_to` promotes the
    // buffer to the shared (Arc-backed) kind, so the delivered `Bytes` is
    // already shared and its `clone()` is a pure refcount bump (alloc-free).
    //
    // A freshly-built `Bytes` (from `Vec`/`BytesMut::freeze`) is instead
    // *promotable*: its FIRST clone allocates a one-time control block. We
    // reproduce production's shared state by forcing that promotion HERE,
    // off-clock (the throwaway clone's promotion alloc happens before the
    // measurement window), so the in-window clone measures exactly the live
    // hot path's zero-alloc refcount bump.
    let to_shared_bytes = |v: Vec<u8>| -> Bytes {
        let b = Bytes::from(v);
        let _promoted = std::hint::black_box(b.clone()); // off-clock: KIND_VEC -> shared
        b
    };
    let counter = AtomicU64::new(0);
    let ticker_pkt = to_shared_bytes(make_ticker_bytes(52432, 245.50, 1_700_000_000));
    let quote_pkt = to_shared_bytes(make_quote_bytes(52432));
    let ticker_burst: Vec<Bytes> = (0..100)
        .map(|i| {
            to_shared_bytes(make_ticker_bytes(
                50000 + i,
                100.0 + i as f32,
                1_700_000_000 + i,
            ))
        })
        .collect();

    // ---- Measurement window (bounded phantom-retry, 2026-07-09 — roaming
    // 4-block cross-thread flake on 2-core CI runners; see
    // dhat_support/mod.rs). Budget UNCHANGED: exactly 0 blocks. The counter
    // sanity check is a per-attempt DELTA (the counter accumulates across
    // retry attempts).
    let (_, allocs_during) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            let counter_start = counter.load(Ordering::Relaxed);

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
            assert_eq!(counter.load(Ordering::Relaxed) - counter_start, 102);
        },
    );

    assert_eq!(
        allocs_during, 0,
        "WS reader tail (counter + WAL Bytes-clone hand-off + dispatch) allocated {} \
         blocks — PRINCIPLE #1 VIOLATED.\n\
         After H1 the ENTIRE tail must be zero-allocation. A non-zero count almost \
         certainly means the WAL hand-off regressed from `data.clone()` back to \
         `data.to_vec()`, or hidden String/Vec/Box creation crept into dispatch_frame \
         or the counter bump.",
        allocs_during
    );
}
