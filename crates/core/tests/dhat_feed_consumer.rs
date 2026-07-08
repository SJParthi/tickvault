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

    let stats_before = dhat::HeapStats::get();

    // 10,000 ticks through the shared core — alternating feeds so BOTH match
    // arms are exercised on the measured path.
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

    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert_eq!(
        allocs_during, 0,
        "consume_feed_tick allocated {allocs_during} blocks over 10,000 ticks — \
         PRINCIPLE #1 VIOLATED. The converged consumer core must be zero-alloc."
    );
    // Sanity: the gate ran as specified — enrich fired for the 5,001 Dhan
    // calls (incl. warm-up), never for the 5,000 Groww calls; persist +
    // aggregate fired for every tick.
    assert_eq!(enriched_count.get(), 5_001);
    assert_eq!(persisted_count.get(), 10_001);
    assert_eq!(aggregated_count.get(), 10_001);
}
