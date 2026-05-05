//! Criterion bench: 28-TF `CascadeFanout::feed_sealed_1s_bar` latency.
//!
//! Closes the §6 Phase 3 9-box gap (per `per-wave-guarantee-matrix.md`
//! row "100% code performance" + `stream-resilience.md` rule B9).
//!
//! Budget: < 5,000 ns per call (5µs). The cascade docstring claims
//! ~1.7µs at 28 × 60ns/map; the budget gives ~3× headroom for noisy
//! CI runners. Measured warm-path in dev: papaya pin/get + uncontended
//! mutex per derived map = 28 × ~60ns ≈ 1.7µs.
//!
//! At peak ~24K instruments × 1 seal/sec = ~24K calls/sec, this budget
//! caps fanout work at ~120ms/sec on a single core (12% of one core).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_trading::candles::cascade_fanout::CascadeFanout;
use tickvault_trading::candles::engine::Bar;

fn bar_at(bucket_start: u32, security_id: u32) -> Bar {
    Bar {
        bucket_start_ist_secs: bucket_start,
        bucket_end_ist_secs: bucket_start + 1,
        open: 100.0,
        high: 100.5,
        low: 99.5,
        close: 100.25,
        volume: 1000,
        volume_cum_day_at_end: 1000,
        oi: 5000,
        tick_count: 10,
        security_id,
        exchange_segment_code: 2, // NSE_FNO
        sealed: true,
    }
}

fn bench_feed_sealed_1s_bar(c: &mut Criterion) {
    let fanout = CascadeFanout::new();
    let security_id = 1234_u32;

    // Pre-warm: first call inserts Arc<Mutex<CandleEngine<TF>>> into
    // all 28 maps. We measure the steady-state warm path only.
    fanout.feed_sealed_1s_bar(&bar_at(34_200, security_id));

    let mut tick_offset = 1_u32;
    c.bench_function("cascade_fanout_feed_sealed_1s_bar", |b| {
        b.iter(|| {
            let bar = bar_at(34_200 + tick_offset, security_id);
            fanout.feed_sealed_1s_bar(black_box(&bar));
            tick_offset = tick_offset.wrapping_add(1);
        });
    });
}

criterion_group!(benches, bench_feed_sealed_1s_bar);
criterion_main!(benches);
