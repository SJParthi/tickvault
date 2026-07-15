//! Silent-feed hardening Item 4 — DHAT zero-alloc test for the Dhan
//! exchange-lag ring write (`feed_lag_monitor::record_dhan_tick`).
//!
//! The hot-path contract: after the ONE-TIME global ring init (two boxed
//! 32,768-slot atomic arrays, allocated OUTSIDE the profiler window below),
//! every `record_dhan_tick` call is two relaxed atomic stores + one relaxed
//! head bump — ZERO heap allocation. A regression that adds `.clone()`,
//! `format!()`, or `Vec::new()` to the admitted path would blow the budget
//! immediately across 10K calls.
//!
//! HONEST COVERAGE (round-1 fix, finding 8): the profiled loop feeds only
//! fresh-capture non-negative samples, i.e. the steady-state
//! `Admitted { clamped: false }` arm — the ONLY `record_dhan_tick` arm with
//! NO metrics call. The `ExcludedReplay` and `Admitted { clamped: true }`
//! arms each call `metrics::counter!` per event and are NOT inside the
//! profiler window, so a per-call allocation introduced THERE (e.g. a
//! future labeled counter Key) is NOT caught by this ratchet — those arms
//! fire only on replay drains / clock skew, not on the steady-state path.
//!
//! Budget: ≤ 1 KiB / ≤ 8 blocks across 10,000 calls — small enough to catch
//! any per-call allocation (even 1 byte/call = 10 KB), with headroom for
//! incidental one-time runtime bookkeeping.
//!
//! CI ENFORCEMENT (round-3 fix, 2026-07-07): this file is deliberately
//! UN-gated (no `#![cfg(feature = "dhat")]`) — the same house pattern as
//! `dhat_ws_reader_zero_alloc.rs` / `dhat_allocation.rs` — so the normal
//! Test (core) nextest lane in ci.yml compiles AND runs it on every PR
//! (a feature-gated variant was CI-inert: no merge gate enables
//! `--features dhat`, so it passed vacuously). The Coverage & Perf lane
//! skips it BY TEST NAME (`-- --skip dhat_` — hence the `dhat_` fn-name
//! prefix below): llvm-cov instrumentation itself allocates, which would
//! blow the exact budget (the documented DHAT-vs-coverage
//! incompatibility, ci.yml Coverage step comment).
//!
//! Run: `cargo test -p tickvault-core --test dhat_feed_lag_ring`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_core::pipeline::feed_lag_monitor::record_dhan_tick;

mod dhat_support;

const NANOS_PER_SEC: i64 = 1_000_000_000;

#[test]
fn dhat_record_dhan_tick_hot_path_zero_allocation() {
    // 2026-07-06 ~10:00 IST as UTC nanos (2026-07-06 00:00 UTC =
    // 1_783_296_000 epoch secs; verified `date -u -d @1783296000`).
    let t0_utc_secs: i64 = 1_783_296_000 + 4 * 3600 + 1800;
    let t0_utc_nanos: i64 = t0_utc_secs * NANOS_PER_SEC;
    let exchange_ist_secs: u32 = u32::try_from(t0_utc_secs + 19_800).unwrap_or(u32::MAX);

    // Pre-init the global ring BEFORE the profiler window so the one-time
    // boxed-array allocation does not count against the hot path (same
    // pre-seed pattern the retired dhat_tick_gap_detector used —
    // that suite was deleted in PR-C3, 2026-07-14, with the detector).
    record_dhan_tick(t0_utc_nanos, t0_utc_nanos - 1_000_000, exchange_ist_secs);

    // Testing-mode profiler (house pattern — dhat_ws_reader_zero_alloc.rs):
    // suppresses the dhat-heap.json side-effect file now that this test
    // runs in the normal Test (core) CI lane on every PR.
    let _profiler = dhat::Profiler::builder().testing().build();

    const BUDGET_BYTES: u64 = 1024;
    const BUDGET_BLOCKS: u64 = 8;

    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners — see
    // dhat_support/mod.rs). Budgets UNCHANGED (1024 B / 8 blocks).
    let (new_bytes, new_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_i64 {
                // Admitted live samples (fresh capture instant, varying receive).
                let recv = t0_utc_nanos + i * 1_000_000;
                record_dhan_tick(recv, recv - 1_000_000, exchange_ist_secs);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        new_bytes <= BUDGET_BYTES,
        "record_dhan_tick allocated {new_bytes} bytes across 10K calls — \
         expected ≤ {BUDGET_BYTES}. The ring write must be two relaxed atomic \
         stores + one head bump; probable cause: a recent edit added \
         .clone(), format!(), Vec::new(), or per-call metrics Key allocation \
         to the hot path."
    );
    assert!(
        new_blocks <= BUDGET_BLOCKS,
        "record_dhan_tick allocated {new_blocks} blocks across 10K calls — \
         expected ≤ {BUDGET_BLOCKS} (zero-alloc hot-path contract)."
    );
}
