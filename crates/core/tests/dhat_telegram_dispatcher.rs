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
//! RE-PINNED 2026-07-07 (Telegram UX overhaul, dated operator directive
//! 2026-07-07 — "boilerplate diet + LOW/MEDIUM digest"): the zero-alloc
//! bypass set CONSCIOUSLY narrows from {Critical, High, Medium} to
//! {Critical, High} — Medium now coalesces into the in-market digest
//! (see `classify_dispatch` + the rewritten coalescer routing tests).
//! NEW assertion: `NotificationEvent::episode_key()` — the probe every
//! dispatch now performs BEFORE the bypass branch — is itself a
//! zero-allocation Copy match.
//!
//! This test pins those invariants. If a future edit accidentally moves
//! state.lock() / format!() / Vec::new() into the bypass path or the
//! episode_key probe, DHAT reports new heap blocks and the test fails.
//!
//! Run: `cargo test -p tickvault-core --features dhat --test dhat_telegram_dispatcher`

#![cfg(feature = "dhat")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod dhat_support;

use tickvault_core::notification::{
    CoalesceDecision, CoalescerConfig, NotificationEvent, Severity, TelegramCoalescer,
};

/// Drives `observe(...)` 10,000 times with Severity::Critical and
/// Severity::High events (the two bypass severities post-2026-07-07),
/// plus 10,000 `episode_key()` probes on a pre-built WS lifecycle event.
/// The payload closure MUST NOT run on bypass, the internal Mutex MUST
/// NOT be touched, and the episode probe MUST NOT allocate. DHAT
/// confirms zero new heap blocks during the loop.
#[test]
fn bypass_path_zero_allocation() {
    // 1. Pre-construct the coalescer + the probe event BEFORE the
    //    profiler window so their one-time allocations (Mutex<HashMap>,
    //    the event's String reason) are excluded from the budget.
    let coalescer = TelegramCoalescer::new(CoalescerConfig::default());
    let ws_event = NotificationEvent::WebSocketDisconnected {
        connection_index: 0,
        reason: "reset".to_string(),
    };
    let plain_event = NotificationEvent::TokenRenewed;

    // 2. Start the profiler, then measure via the bounded phantom-retry
    //    helper (2026-07-09 — roaming 4-block cross-thread flake on 2-core
    //    CI runners; see dhat_support/mod.rs). Budget UNCHANGED: 0 blocks.
    let _profiler = dhat::Profiler::builder().testing().build();

    // 3. Hot loop — bypass calls + episode_key probes. The closure panics
    //    if it ever runs (it must not for bypass severities), which would
    //    fail the test before DHAT even gets a chance to assert.
    let (new_bytes, new_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for _ in 0..10_000 {
                let decision = coalescer.observe("RiskHalt", Severity::Critical, || {
                    panic!("payload closure must not run on Critical bypass")
                });
                assert_eq!(decision, CoalesceDecision::Bypass);

                let decision = coalescer.observe("WebSocketDisconnected", Severity::High, || {
                    panic!("payload closure must not run on High bypass")
                });
                assert_eq!(decision, CoalesceDecision::Bypass);

                // 2026-07-07: the episode probe on the dispatch path is a Copy
                // match — zero allocation whether it hits (WS lifecycle) or
                // misses (every other variant).
                assert!(ws_event.episode_key().is_some());
                assert!(plain_event.episode_key().is_none());
            }
        },
    );

    // 5. Hard assert: zero new heap blocks across 20,000 bypass calls +
    //    20,000 episode_key probes. A regression that adds even one
    //    allocation per call would push this to 20_000+.
    assert_eq!(
        new_blocks, 0,
        "TelegramCoalescer::observe bypass path / episode_key() allocated {new_blocks} new \
         heap blocks across 20k Critical+High bypass calls + 20k episode probes (expected 0). \
         New bytes: {new_bytes}. \
         Investigate which match arm in coalescer.rs::observe or events.rs::episode_key \
         is now allocating."
    );
}
