//! Scoreboard PR-C — DHAT zero-alloc test for the Groww exchange-lag fold
//! (`feed_lag_monitor::record_groww_tick`).
//!
//! The hot-path contract: after the ONE-TIME global ring init (two boxed
//! 32,768-slot atomic arrays, allocated OUTSIDE the profiler window below —
//! the day histogram is a const-initialized static, no allocation at all),
//! every `record_groww_tick` call is a pure classify + two relaxed atomic
//! stores + one relaxed head bump + two relaxed day-histogram RMWs — ZERO
//! heap allocation. A regression that adds `.clone()`, `format!()`, or
//! `Vec::new()` to the admitted path would blow the budget immediately
//! across 10K calls.
//!
//! HONEST COVERAGE (the dhat_feed_lag_ring.rs precedent): the profiled loop
//! feeds only fresh-capture non-negative samples — the steady-state
//! admitted-unclamped arm, the ONLY `record_groww_tick` arm with NO metrics
//! call. The `ExcludedNoCapture` / `ExcludedStaleCapture` / clamped arms
//! each call `metrics::counter!` per event and are NOT inside the profiler
//! window — those arms fire on reconcile/re-tail drains and clock skew, not
//! on the steady-state path.
//!
//! Budget: ≤ 1 KiB / ≤ 8 blocks across 10,000 calls — small enough to catch
//! any per-call allocation (even 1 byte/call = 10 KB), with headroom for
//! incidental one-time runtime bookkeeping.
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`) —
//! the same house pattern as `dhat_feed_lag_ring.rs` — so the normal
//! Test (core) nextest lane compiles AND runs it on every PR. The
//! Coverage & Perf lane skips it BY TEST NAME (`-- --skip dhat_` — hence
//! the `dhat_` fn-name prefix below): llvm-cov instrumentation itself
//! allocates, which would blow the exact budget.
//!
//! Run: `cargo test -p tickvault-core --test dhat_feed_lag_groww`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_core::pipeline::feed_lag_monitor::record_groww_tick;

mod dhat_support;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_MS: i64 = 1_000_000;
const IST_UTC_OFFSET_NANOS: i64 = 19_800 * NANOS_PER_SEC;

#[test]
fn dhat_record_groww_tick_hot_path_zero_allocation() {
    // 2026-07-06 ~10:00 IST as IST nanos (2026-07-06 00:00 UTC =
    // 1_783_296_000 epoch secs; verified `date -u -d @1783296000`).
    let t0_utc_secs: i64 = 1_783_296_000 + 4 * 3600 + 1800;
    let t0_ist_nanos: i64 = t0_utc_secs * NANOS_PER_SEC + IST_UTC_OFFSET_NANOS;

    // Pre-init the global Groww ring BEFORE the profiler window so the
    // one-time boxed-array allocation does not count against the hot path
    // (same pattern as dhat_feed_lag_ring's pre-seed).
    record_groww_tick(
        Some(t0_ist_nanos + 150 * NANOS_PER_MS),
        t0_ist_nanos + 150 * NANOS_PER_MS,
        t0_ist_nanos,
        false,
    );

    // Testing-mode profiler (house pattern — dhat_feed_lag_ring.rs):
    // suppresses the dhat-heap.json side-effect file.
    let _profiler = dhat::Profiler::builder().testing().build();

    const BUDGET_BYTES: u64 = 1024;
    const BUDGET_BLOCKS: u64 = 8;

    // Bounded phantom-retry helper (roaming cross-thread flake on 2-core CI
    // runners — see dhat_support/mod.rs). Budgets match the Dhan ratchet.
    let (new_bytes, new_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_i64 {
                // Admitted steady-state samples: fresh capture (ms-precision
                // 150 ms lag), varying exchange + capture instants.
                let exchange = t0_ist_nanos + i * NANOS_PER_MS;
                let capture = exchange + 150 * NANOS_PER_MS;
                record_groww_tick(Some(capture), capture, exchange, false);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        new_bytes <= BUDGET_BYTES,
        "record_groww_tick allocated {new_bytes} bytes across 10K calls — \
         expected ≤ {BUDGET_BYTES}. The fold must be a pure classify + ring \
         write + two histogram RMWs; probable cause: a recent edit added \
         .clone(), format!(), Vec::new(), or per-call metrics Key allocation \
         to the admitted hot path."
    );
    assert!(
        new_blocks <= BUDGET_BLOCKS,
        "record_groww_tick allocated {new_blocks} blocks across 10K calls — \
         expected ≤ {BUDGET_BLOCKS} (zero-alloc hot-path contract)."
    );
}
