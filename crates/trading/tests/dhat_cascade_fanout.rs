//! DHAT zero-allocation test for the 28-TF `CascadeFanout` hot path.
//!
//! Closes the §6 Phase 3 9-box gap (per `per-wave-guarantee-matrix.md`
//! row "100% code performance"): every sealed 1s bar fans into 28
//! derived engines via `feed_sealed_1s_bar`. With ~24K instruments
//! sealing once per second at peak, this path is called ~24K × 1/sec.
//! Any per-call allocation regression compounds into ~24K allocs/sec.
//!
//! DHAT's global allocator profiler permits only ONE active profiler
//! per test process, so this file holds a single test that exercises
//! both single- and multi-instrument steady-state scenarios.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

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

#[test]
fn dhat_feed_sealed_1s_bar_steady_state_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let fanout = CascadeFanout::new();

    // Pre-warm 100 instruments × 28 engines. The first call per
    // (security_id, segment) per derived map allocates one
    // `Arc<Mutex<CandleEngine<TF>>>` (acceptable: off steady state,
    // documented in cascade audit).
    let instruments: [u32; 100] = std::array::from_fn(|i| i as u32 + 1000);
    for &security_id in &instruments {
        fanout.feed_sealed_1s_bar(&bar_at(34_200, security_id));
    }

    let stats_before = dhat::HeapStats::get();

    // Steady-state measurement: 100 instruments × 100 sealed bars =
    // 10,000 fanout calls. Every call must hit the warm-path branch
    // in `engine_map::on_sealed_bar` (papaya pin/get of an existing
    // entry) and NOT allocate.
    for tick_offset in 1..=100_u32 {
        for &security_id in &instruments {
            fanout.feed_sealed_1s_bar(&bar_at(34_200 + tick_offset, security_id));
        }
    }

    let stats_after = dhat::HeapStats::get();
    let allocs = stats_after.total_blocks - stats_before.total_blocks;

    // Hard zero — `feed_sealed_1s_bar` is the steady-state hot path
    // for ~24K seals/sec. Any allocation here is a regression that
    // compounds linearly with the universe size.
    assert_eq!(
        allocs, 0,
        "feed_sealed_1s_bar must be zero-alloc on steady-state hot path; got {allocs} allocations across 10,000 calls (100 instruments × 100 sealed bars)"
    );
}
