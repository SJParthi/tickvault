//! Wave 7-A4 sub-PR #5 (W7-A4.5) — DHAT zero-allocation test for the
//! `BarCache` hot-path lookup.
//!
//! Closes the W7-A4 9-box gap "100% code performance": every
//! indicator/strategy tick read calls `BarCache::lookup(...)` to get
//! today's or yesterday's sealed bar from RAM (vs hitting QuestDB,
//! which is banned by category 10 of `banned-pattern-scanner.sh`).
//! Any per-call allocation regression compounds into ~470 allocs/sec
//! at sustained tick rate and ~99K allocs/min boundary burst — both
//! intolerable on a 10 ns / 50 ns / 100 ns p99 budget per
//! `quality/benchmark-budgets.toml`.
//!
//! Pattern mirrors `dhat_cascade_fanout.rs` (Wave 6 §6 / Phase 3): a
//! single test that pre-warms the cache, snapshots `dhat::HeapStats`,
//! exercises 10,000 lookups in steady state, then asserts `total_blocks`
//! delta is 0.
//!
//! Companion to:
//! - `dhat_cascade_fanout.rs` — pins zero-alloc on the SEAL path
//!   (Wave 6 producer)
//! - **THIS TEST** — pins zero-alloc on the LOOKUP path (Wave 7-A4
//!   consumer)
//!
//! Together they cover the full producer→consumer chain.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_trading::candles::TfIndex;
use tickvault_trading::in_mem::{BarCache, CompactBar};

mod dhat_support;

fn make_bar(bucket_start: u32) -> CompactBar {
    CompactBar {
        bucket_start_ist_secs: bucket_start,
        open: 100.0,
        high: 105.0,
        low: 95.0,
        close: 102.0,
        volume: 1_000,
        oi: 500_000,
        tick_count: 42,
    }
}

#[test]
fn dhat_bar_cache_lookup_steady_state_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let cache = BarCache::new();

    // Pre-warm 100 instruments × 1 TF × 1 bucket = 100 cache entries.
    // The first papaya insert per entry allocates a node; that's
    // acceptable startup cost. The hot-path measurement below
    // exercises ONLY the lookup path, where every read must hit a
    // pre-existing entry and allocate nothing.
    let instruments: [u64; 100] = std::array::from_fn(|i| i as u64 + 1000);
    let bucket_start: u32 = 34_200;
    let tf = TfIndex::M1;
    let segment_code: u8 = 2; // NSE_FNO
    for &security_id in &instruments {
        cache.insert(security_id, segment_code, tf, make_bar(bucket_start));
    }

    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners — see
    // dhat_support/mod.rs). Budget UNCHANGED: exactly 0 blocks. The `found`
    // sanity counter is per-attempt (local to the workload closure).
    let (_, allocs) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            // Steady-state measurement: 100 instruments × 100 lookups each =
            // 10,000 papaya pin+get calls. Every call must return Some(...)
            // without allocating.
            let mut found: u64 = 0;
            for _ in 0..100 {
                for &security_id in &instruments {
                    if cache
                        .lookup(security_id, segment_code, tf, bucket_start)
                        .is_some()
                    {
                        found = found.saturating_add(1);
                    }
                }
            }
            assert_eq!(
                found, 10_000,
                "all 10,000 lookups must hit a pre-existing entry"
            );
        },
    );

    // Hard zero — `BarCache::lookup` is the steady-state hot path for
    // every indicator + strategy tick read. Any allocation here
    // compounds linearly with the universe size + tick rate.
    //
    // If this test starts failing, the regression is almost
    // certainly in `papaya::HashMap::pin` (newer papaya releases may
    // change the pin-handle internals) or in the `Copy` semantics of
    // `CompactBar` (an added field that loses `Copy` would force a
    // heap-allocated clone).
    assert_eq!(
        allocs, 0,
        "BarCache::lookup must be zero-alloc on steady-state hot path; got {allocs} allocations across 10,000 lookups (100 instruments × 100 reads)"
    );
}
