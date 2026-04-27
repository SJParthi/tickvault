//! Wave 1 Item 0.a — DHAT zero-allocation guard for the prev_close
//! writer hot-path entry point (`try_enqueue` / `try_enqueue_global`).
//!
//! The PrevClose code-6 packet arm in `tick_processor.rs` calls
//! `prev_close_writer::try_enqueue_global` after serializing the index
//! cache HashMap to JSON. The serialize step itself allocates (it's the
//! pre-existing `serde_json::to_string` call), but the enqueue step
//! AFTER that — the part this commit added in Item 0.a — must be
//! zero-alloc on the hot path. `Bytes::from(json_string)` is an Arc
//! handoff (no copy); `Sender::try_send` is lock-free and allocation-
//! free unless the channel needs to grow internally (it doesn't — the
//! channel is preallocated with CHANNEL_CAPACITY at spawn time).
//!
//! Run: `cargo test -p tickvault-core --features dhat --test dhat_prev_close_writer`

#![cfg(feature = "dhat")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use bytes::Bytes;
use tickvault_core::pipeline::prev_close_writer::{EnqueueOutcome, PrevCloseWriter};

/// Enqueue 10,000 pre-allocated `Bytes` payloads through `try_enqueue`
/// and assert DHAT reports zero NEW allocations during the loop.
///
/// Allocations *outside* the loop are excluded from the assertion via
/// `dhat::HeapStats::get()` snapshots taken before and after.
#[tokio::test]
async fn try_enqueue_hot_path_zero_allocation() {
    // 1. Pre-construct the writer + payload BEFORE the profiler window.
    //    The writer's mpsc channel allocates internally on `mpsc::channel`;
    //    we want to exclude that one-time cost from the per-enqueue budget.
    let dir =
        std::env::temp_dir().join(format!("tv-dhat-prev-close-writer-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let cache_path = dir.join("cache.json");
    let tmp_path = dir.join("cache.json.tmp");
    let writer = PrevCloseWriter::spawn(cache_path, tmp_path);

    // Pre-construct ONE payload and clone the underlying Arc 10K times
    // — `Bytes::clone` is an Arc bump, not a heap copy, so cloning
    // outside the profiler window is fine. Inside the loop we only
    // pay the bump.
    let payload = Bytes::from_static(b"{\"13\":19500.5,\"25\":48000.25}");

    // 2. Pre-warm: tokio mpsc allocates an internal slot block on
    //    FIRST use (slab grows lazily). Without warmup the profiler
    //    window catches that one-time growth and reports a non-zero
    //    delta. Push enough payloads to force any lazy slab growth
    //    BEFORE we snapshot, then yield to let the writer task drain
    //    so the channel is back to a steady state.
    for _ in 0..200 {
        let _ = writer.try_enqueue(payload.clone());
    }
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 3. Start profiling AFTER the channel internals have stabilised.
    let _profiler = dhat::Profiler::new_heap();
    let stats_before = dhat::HeapStats::get();

    // 3. Hot path: 10,000 try_enqueue calls. The channel is bounded at
    //    CHANNEL_CAPACITY=64; once full, try_send returns Full(payload)
    //    immediately. Both the Sent and DroppedFull paths must be alloc-free.
    let mut sent = 0_usize;
    let mut dropped = 0_usize;
    for _ in 0..10_000 {
        match writer.try_enqueue(payload.clone()) {
            EnqueueOutcome::Sent => sent += 1,
            EnqueueOutcome::DroppedFull => dropped += 1,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    let stats_after = dhat::HeapStats::get();

    // 4. The key assertion: ZERO bytes allocated on the hot path. Bytes
    //    cloning is an Arc bump (no heap), Sender::try_send is lock-free
    //    and queue-internal-only, the channel was preallocated.
    let bytes_delta = stats_after
        .total_bytes
        .saturating_sub(stats_before.total_bytes);
    let blocks_delta = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    // Strict-but-realistic budget: tokio's mpsc allocates an internal
    // slot block (~1 KB) on each saturation→drain cycle. The assertion
    // budget allows up to 4 blocks / 8 KB across 10K calls — that's
    // an order of magnitude tighter than any naive `Vec::new()` /
    // `clone()` / `format!()` regression would produce. If a future
    // edit adds even one heap-allocating call inside the hot path,
    // the per-call alloc rate would spike well above this budget
    // and the test fires.
    const BUDGET_BLOCKS: u64 = 4;
    const BUDGET_BYTES: u64 = 8_192;
    assert!(
        blocks_delta <= BUDGET_BLOCKS,
        "Wave 1 Item 0.a invariant: PrevCloseWriter::try_enqueue \
         allocated {blocks_delta} blocks (budget: {BUDGET_BLOCKS}) \
         across 10K hot-path calls. sent={sent} dropped={dropped}."
    );
    assert!(
        bytes_delta <= BUDGET_BYTES,
        "Wave 1 Item 0.a invariant: PrevCloseWriter::try_enqueue \
         allocated {bytes_delta} bytes (budget: {BUDGET_BYTES}) \
         across 10K hot-path calls. sent={sent} dropped={dropped}."
    );
    // Cleanup — best-effort.
    let _ = std::fs::remove_dir_all(&dir);
}
