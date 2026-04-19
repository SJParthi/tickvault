//! PR #288 (#5): DHAT zero-allocation test for `DepthSequenceTracker::observe()`.
//!
//! The module comment claims the hot path is O(1) + zero-alloc. This test
//! enforces that claim mechanically. If any regression adds a `.clone()`,
//! `.to_string()`, `Vec::new()`, `format!()` etc. inside `observe()`, DHAT
//! reports non-zero alloc bytes and the test fails.
//!
//! Run: `cargo test -p tickvault-core --features dhat --test dhat_depth_sequence_tracker`

#![cfg(feature = "dhat")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::types::ExchangeSegment;
use tickvault_core::parser::DepthSide;
use tickvault_core::pipeline::DepthSequenceTracker;

/// Observe 10,000 sequences across 100 different keys and assert DHAT reports
/// zero NEW allocations during the observe loop. The tracker's initial map
/// allocation happens in `new()` and is excluded via a profiler snapshot.
#[test]
fn observe_hot_path_zero_allocation() {
    // Allocate the tracker BEFORE the profiler window so its initial
    // capacity + papaya internals don't count against the hot path.
    let tracker = DepthSequenceTracker::new();
    // Pre-seed the keys so the first call is also hot-path (not first-insert).
    for id in 0..100_u32 {
        tracker.observe(id, ExchangeSegment::NseFno, DepthSide::Bid, 0);
        tracker.observe(id, ExchangeSegment::NseFno, DepthSide::Ask, 0);
    }

    // Now start profiling and run the hot path.
    let _profiler = dhat::Profiler::new_heap();
    let stats_before = dhat::HeapStats::get();

    // 10,000 observes across pre-seeded keys. Every call is a lookup +
    // compare + (possible) overwrite — no new key insertions.
    for seq in 1..=50_u32 {
        for id in 0..100_u32 {
            let _ = tracker.observe(id, ExchangeSegment::NseFno, DepthSide::Bid, seq);
            let _ = tracker.observe(id, ExchangeSegment::NseFno, DepthSide::Ask, seq);
        }
    }

    let stats_after = dhat::HeapStats::get();

    // The key assertion: ZERO bytes allocated during the hot path. If
    // papaya's insert-on-existing-key somehow allocates (e.g. if the
    // value slot needs a new node), this number would be non-zero.
    let bytes_delta = stats_after
        .total_bytes
        .saturating_sub(stats_before.total_bytes);
    let blocks_delta = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert_eq!(
        bytes_delta, 0,
        "DepthSequenceTracker::observe() must be zero-alloc on the hot path, \
         but DHAT reports {bytes_delta} bytes across {blocks_delta} blocks \
         during a 10,000-iteration loop over 200 pre-seeded keys. \
         Check for a newly-introduced `.clone()`, `.to_string()`, \
         `format!()`, or `Vec::new()` inside `observe()`."
    );
}
