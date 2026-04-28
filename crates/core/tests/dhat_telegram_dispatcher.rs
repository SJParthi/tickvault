//! Wave 3-B Item 11 — DHAT zero-allocation guard for the Telegram
//! coalescer's BYPASS path.
//!
//! The notification dispatcher is documented as cold path (see
//! `crates/core/src/notification/service.rs:22`), but the coalescer's
//! `observe()` entry point is shared with the `notify()` call which
//! CAN be reached from a hot-path tick processor's `error!` macro
//! (when a flush failure is logged at error level → tracing layer →
//! Telegram fan-out via the alertmanager path). For Severity::Critical
//! and Severity::High events we MUST NOT allocate inside `observe()` —
//! those events bypass coalescing and the only work done is a single
//! `match` arm + return.
//!
//! This test pins that invariant. If a future edit accidentally moves
//! state.lock() / format!() / Vec::new() into the bypass path, DHAT
//! will report new heap blocks during the loop and the test fails.
//!
//! Run: `cargo test -p tickvault-core --features dhat --test dhat_telegram_dispatcher`

#![cfg(feature = "dhat")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_core::notification::{
    CoalesceDecision, CoalescerConfig, Severity, TelegramCoalescer,
};

/// Drives `observe(...)` 10,000 times with Severity::Critical and
/// Severity::High events. Both severities are documented bypasses —
/// the closure that produces the payload string MUST NOT run, and the
/// internal Mutex MUST NOT be touched. DHAT confirms zero new heap
/// blocks during the loop.
#[test]
fn bypass_path_zero_allocation() {
    // 1. Pre-construct the coalescer BEFORE the profiler window so its
    //    one-time `Mutex<HashMap>` allocation is excluded from the budget.
    let coalescer = TelegramCoalescer::new(CoalescerConfig::default());

    // 2. Capture the heap state immediately before the hot loop.
    let _profiler = dhat::Profiler::builder().testing().build();
    let stats_before = dhat::HeapStats::get();

    // 3. Hot loop — 10,000 bypass calls. The closure panics if it ever
    //    runs (it must not for bypass severities), which would fail the
    //    test before DHAT even gets a chance to assert.
    for _ in 0..10_000 {
        let decision = coalescer.observe("RiskHalt", Severity::Critical, || {
            panic!("payload closure must not run on Critical bypass")
        });
        assert_eq!(decision, CoalesceDecision::Bypass);

        let decision = coalescer.observe("WebSocketDisconnected", Severity::High, || {
            panic!("payload closure must not run on High bypass")
        });
        assert_eq!(decision, CoalesceDecision::Bypass);

        let decision = coalescer.observe("Phase2Complete", Severity::Medium, || {
            panic!("payload closure must not run on Medium bypass")
        });
        assert_eq!(decision, CoalesceDecision::Bypass);
    }

    // 4. Capture the heap state after the loop.
    let stats_after = dhat::HeapStats::get();

    let new_blocks = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);
    let new_bytes = stats_after
        .total_bytes
        .saturating_sub(stats_before.total_bytes);

    // 5. Hard assert: zero new heap blocks across 30,000 bypass calls.
    //    A regression that adds even one allocation per call would
    //    push this to 30_000.
    assert_eq!(
        new_blocks, 0,
        "TelegramCoalescer::observe bypass path allocated {new_blocks} new heap blocks \
         across 30k Critical+High+Medium calls (expected 0). \
         New bytes: {new_bytes}. \
         Investigate which match arm in coalescer.rs::observe is now allocating."
    );
}
