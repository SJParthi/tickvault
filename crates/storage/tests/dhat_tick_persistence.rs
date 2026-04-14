//! DHAT allocation test for tick persistence ILP buffer building.
//!
//! Separate binary because DHAT allows only one profiler per process.
//! Verifies Principle #1: zero heap allocation for f32→f64 conversion
//! and ILP row building (buffer reuse, stack-based decimal conversion).
//!
//! STORAGE-GAP-02: f32_to_f64_clean uses a stack buffer, not heap.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn dhat_f32_to_f64_clean_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Pre-warm: ensure any one-time setup is done.
    let _ = tickvault_storage::tick_persistence_testing::f32_to_f64_clean_pub(1.0_f32);

    let stats_before = dhat::HeapStats::get();

    // Simulate hot-path: 1000 f32→f64 conversions (what tick processing does).
    for i in 0..1000 {
        let v = (i as f32) * 0.05 + 21000.0;
        let cleaned = tickvault_storage::tick_persistence_testing::f32_to_f64_clean_pub(v);
        // Use the result to prevent optimization.
        assert!(cleaned > 0.0);
    }

    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert_eq!(
        allocs_during, 0,
        "f32_to_f64_clean allocated {} blocks over 1000 conversions — PRINCIPLE #1 VIOLATED.\n\
         Stack-based decimal conversion must be zero-allocation.",
        allocs_during
    );
}
