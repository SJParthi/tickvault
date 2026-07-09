//! DHAT allocation test for the risk engine hot path.
//!
//! Verifies that `check_order()` — the pre-trade risk check called
//! on every order — does not allocate when the order is approved.
//! Rejected orders allocate for the reason string (cold path).

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_trading::risk::engine::RiskEngine;

mod dhat_support;

#[test]
fn dhat_check_order_approved_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let mut engine = RiskEngine::new(2.0, 100, 1_000_000.0);
    // Pre-warm: first fill allocates HashMap entry
    engine.record_fill(1001, 10, 100.0, 25);

    // Measure: approved check_order should be zero-alloc.
    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners — see
    // dhat_support/mod.rs). Budget UNCHANGED: exactly 0 blocks. The
    // approved-path check is side-effect-free, so re-running is safe; the
    // approval assert runs per attempt inside the workload.
    let (_, allocs) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            let result = engine.check_order(1001, 5);
            assert!(result.is_approved());
        },
    );
    assert_eq!(
        allocs, 0,
        "check_order (approved) must be zero-alloc on hot path, got {allocs} allocations"
    );
}
