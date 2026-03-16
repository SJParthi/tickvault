//! DHAT allocation test for the risk engine hot path.
//!
//! Verifies that `check_order()` — the pre-trade risk check called
//! on every order — does not allocate when the order is approved.
//! Rejected orders allocate for the reason string (cold path).

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use dhan_live_trader_trading::risk::engine::RiskEngine;

#[test]
fn dhat_check_order_approved_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
    // Pre-warm: first fill allocates HashMap entry
    engine.record_fill(1001, 10, 100.0, 25);

    // Measure: approved check_order should be zero-alloc
    let stats_before = dhat::HeapStats::get();
    let result = engine.check_order(1001, 5);
    let stats_after = dhat::HeapStats::get();

    assert!(result.is_approved());
    let allocs = stats_after.total_blocks - stats_before.total_blocks;
    assert_eq!(
        allocs, 0,
        "check_order (approved) must be zero-alloc on hot path, got {allocs} allocations"
    );
}
