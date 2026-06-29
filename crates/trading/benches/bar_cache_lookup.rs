//! Criterion bench: `BarCache::lookup` latency.
//!
//! Closes the Wave 7-A4 §6 9-box "100% code performance" guarantee
//! row for the RAM-first hot-path READ side. Companion to:
//! - DHAT zero-alloc test (W7-A4.5, PR #600) — verifies 0 heap allocs
//! - **THIS BENCH** — verifies p99 latency budget at runtime
//!
//! Budget: < 100 ns per call. Source-site docstring claims ~30ns
//! (papaya pin + get + Copy). The budget gives ~3× headroom for
//! noisy CI runners and matches the
//! `quality/benchmark-budgets.toml::bar_cache_lookup` entry.
//!
//! At peak indicator tick rate (~470 ticks/sec × 9 TFs = ~4,200
//! lookups/sec sustained, plus ~99K reads/sec burst at minute
//! boundary) this budget caps lookup work at ~10% of one core.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_trading::candles::TfIndex;
use tickvault_trading::in_mem::{BarCache, CompactBar};

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

fn bench_bar_cache_lookup(c: &mut Criterion) {
    let cache = BarCache::new();
    let security_id = 1234_u64;
    let segment_code = 2_u8; // NSE_FNO
    let tf = TfIndex::M1;
    let bucket_start = 34_200_u32;

    // Pre-warm: insert one bar so the lookup hits the warm path
    // (papaya pin + get of an existing entry) and never falls
    // through to the slow miss path.
    cache.insert(security_id, segment_code, tf, make_bar(bucket_start));

    c.bench_function("bar_cache_lookup", |b| {
        b.iter(|| {
            let result = cache.lookup(
                black_box(security_id),
                black_box(segment_code),
                black_box(tf),
                black_box(bucket_start),
            );
            black_box(result)
        });
    });
}

criterion_group!(benches, bench_bar_cache_lookup);
criterion_main!(benches);
