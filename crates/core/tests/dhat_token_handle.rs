//! DHAT allocation test for TokenHandle (ArcSwap) reads.
//!
//! Separate binary from dhat_allocation.rs because DHAT allows only one
//! profiler per process. This test verifies Principle #1 for auth token
//! reads on the hot path.
//!
//! AUTH-GAP-01: Every tick processor reads the current JWT via `handle.load()`.
//! This must be zero-allocation since ArcSwap uses atomic pointer operations.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn dhat_token_handle_reads_zero_alloc() {
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    let _profiler = dhat::Profiler::builder().testing().build();

    // Pre-allocate: create a TokenHandle with a token stored
    type TokenHandle = Arc<ArcSwap<Option<String>>>;
    let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(Some(
        "test-jwt-token-for-dhat".to_string(),
    ))));

    // Warm up: first load() may initialize ArcSwap's thread-local hazard pointer
    // (one-time per-thread allocation, not per-read). This is acceptable.
    let _warmup = handle.load();

    // ---- Measure: all subsequent reads must be zero-allocation ----
    let stats_before = dhat::HeapStats::get();

    // Simulate hot-path: 1000 token reads (what tick processor does)
    for _ in 0..1000 {
        let guard = handle.load();
        let token = guard.as_ref().as_ref();
        // Use the token to prevent optimization
        assert!(token.is_some());
        assert!(!token.expect("token present").is_empty());
    }

    // ---- End measurement ----
    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert_eq!(
        allocs_during, 0,
        "TokenHandle.load() allocated {} blocks over 1000 reads — PRINCIPLE #1 VIOLATED.\n\
         ArcSwap reads must be zero-allocation on the hot path (after warmup).",
        allocs_during
    );
}
