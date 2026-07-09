//! DHAT zero-allocation test for the C2 shared per-tick consumer core.
//!
//! Separate binary because DHAT allows only one profiler per process.
//! Verifies Principle #1: the converged `consume_feed_tick` adds NO heap
//! allocation on the per-tick path — the ordered enrich → persist → aggregate
//! dispatch is a `Copy`-enum match + three monomorphized closure calls that
//! inline to the pre-C2 straight-line code. The closures here mirror the
//! production shape (Copy-field reads/writes + `Cell` hand-off) without the
//! I/O sinks, so the measurement isolates the CORE's own cost.

use std::cell::Cell;

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::pipeline::feed_consumer::consume_feed_tick;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod dhat_support;

fn make_tick(i: u32) -> ParsedTick {
    ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        last_traded_price: 21_000.0 + f32::from(i as u16),
        exchange_timestamp: 1_740_556_500 + i,
        ..ParsedTick::default()
    }
}

#[test]
fn dhat_feed_consumer_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Warm-up (mirrors the production loop reaching steady state).
    let enriched_count = Cell::new(0_u64);
    let persisted_count = Cell::new(0_u64);
    let aggregated_count = Cell::new(0_u64);
    let mut warm = make_tick(0);
    consume_feed_tick(
        Feed::Dhan,
        &mut warm,
        |t| {
            t.iv = 0.18;
            enriched_count.set(enriched_count.get() + 1);
        },
        |_| persisted_count.set(persisted_count.get() + 1),
        |_| aggregated_count.set(aggregated_count.get() + 1),
    );

    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners — see
    // dhat_support/mod.rs). Budget UNCHANGED: exactly 0 blocks. The gate
    // sanity checks moved INSIDE the workload as per-attempt DELTAS (the
    // cells accumulate across retry attempts, so absolute totals would
    // depend on the attempt count).
    let (_, allocs_during) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            let enriched_start = enriched_count.get();
            let persisted_start = persisted_count.get();
            let aggregated_start = aggregated_count.get();

            // 10,000 ticks through the shared core — alternating feeds so BOTH
            // match arms are exercised on the measured path.
            for i in 0..10_000_u32 {
                let mut tick = make_tick(i);
                let feed = if i.is_multiple_of(2) {
                    Feed::Dhan
                } else {
                    Feed::Groww
                };
                consume_feed_tick(
                    feed,
                    &mut tick,
                    |t| {
                        t.iv = 0.18;
                        enriched_count.set(enriched_count.get() + 1);
                    },
                    |t| {
                        // Copy-field read, mirroring the persist closure's access shape.
                        if t.last_traded_price > 0.0 {
                            persisted_count.set(persisted_count.get() + 1);
                        }
                    },
                    |t| {
                        if t.exchange_timestamp > 0 {
                            aggregated_count.set(aggregated_count.get() + 1);
                        }
                    },
                );
            }

            // Sanity (per attempt): enrich fired for the 5,000 Dhan calls,
            // never for the 5,000 Groww calls; persist + aggregate fired for
            // every tick. (The warm-up's +1 enrich sits outside these deltas.)
            assert_eq!(enriched_count.get() - enriched_start, 5_000);
            assert_eq!(persisted_count.get() - persisted_start, 10_000);
            assert_eq!(aggregated_count.get() - aggregated_start, 10_000);
        },
    );

    assert_eq!(
        allocs_during, 0,
        "consume_feed_tick allocated {allocs_during} blocks over 10,000 ticks — \
         PRINCIPLE #1 VIOLATED. The converged consumer core must be zero-alloc."
    );
}
