//! DHAT zero-allocation ratchet for the per-frame stage telemetry
//! (2026-10-26, plan item K5).
//!
//! The module header has claimed since 2026-10-02 that "the DHAT test
//! `dhat_hot_path_telemetry` proves it". No such test existed: the claim
//! named a file that was never written, which is exactly the reassuring-
//! direction staleness the O(1) table records. This is that test.
//!
//! `record_stage_nanos` runs on the socket read task for every frame and on
//! the frame drain; `beat_at` runs beside it. Both must stay atomics only.
//! The percentile work added by K5 runs on the publisher thread, never here,
//! so the per-frame budget is unchanged at zero.
//!
//! `stage_quantiles` is measured in its own window: it is not on the frame
//! path, but it is called once a second per stage forever, so an allocation
//! in it would be a slow leak on a thread nobody watches.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::time::Instant;

use tickvault_storage::hot_path_telemetry::{
    ALL_STAGES, HotTask, RollingMax, STAGE_BUCKETS, Stage, beat_at, record_stage_nanos,
    stage_quantiles,
};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

#[test]
fn dhat_record_stage_nanos_and_stage_quantiles_are_zero_allocation() {
    // Warm up: fix the time anchor and touch every static before measuring.
    record_stage_nanos(Stage::SocketToWal, 1);
    beat_at(HotTask::FrameDrain, Instant::now());
    let warm = [1u64; STAGE_BUCKETS];
    assert!(stage_quantiles(&warm).is_some());

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..50_000_u64 {
                let stage = ALL_STAGES[(i as usize) % ALL_STAGES.len()];
                // A spread of fast samples and the occasional slow one, so
                // the over-budget arm is measured too.
                let nanos = if i % 1_000 == 0 {
                    500_000
                } else {
                    900 + i % 64
                };
                record_stage_nanos(stage, nanos);
                beat_at(HotTask::FrameDrain, Instant::now());
            }
        },
    );
    assert_eq!(
        (bytes, blocks),
        (0, 0),
        "record_stage_nanos / beat_at allocated on the frame path"
    );

    // The publisher-thread quantile maths, in its own window.
    let mut window = [0u64; STAGE_BUCKETS];
    window[0] = 9_000;
    window[5] = 900;
    window[10] = 90;
    window[20] = 10;
    let (q_bytes, q_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u64 {
                window[(i as usize) % STAGE_BUCKETS] += 1;
                let q = stage_quantiles(&window).expect("samples");
                assert!(q.samples > 0);
            }
        },
    );
    assert_eq!(
        (q_bytes, q_blocks),
        (0, 0),
        "stage_quantiles allocated on the publisher thread"
    );

    // The rolling 75-publish maximum (live audit HP-2, 2026-10-29): one push
    // per stage per second on the publisher thread, a fixed ring.
    let mut rolling = [RollingMax::new(); ALL_STAGES.len()];
    let (r_bytes, r_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u64 {
                let slot = &mut rolling[(i as usize) % ALL_STAGES.len()];
                assert!(slot.push(i % 977) <= 976);
            }
        },
    );
    assert_eq!(
        (r_bytes, r_blocks),
        (0, 0),
        "RollingMax::push allocated on the publisher thread"
    );
}
