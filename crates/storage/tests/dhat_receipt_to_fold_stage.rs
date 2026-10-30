//! DHAT zero-allocation ratchet for the `receipt_to_fold` stage (plan item
//! DT0, 2026-10-29).
//!
//! `record_receipt_to_fold_nanos` runs on the frame drain once per main-feed
//! frame: the stage record plus the fine quarter-octave histogram, five
//! relaxed atomics in all. `register_main_worker_thread` runs from the main
//! runtime's `on_thread_park` hook on every worker park; after its first
//! call on a thread it is one thread-local read. Both must stay
//! allocation-free.
//!
//! What this test does NOT measure: the FIRST call of
//! `register_main_worker_thread` on each thread reads `/proc/thread-self`
//! (one syscall and one small `PathBuf`). That is paid once per worker
//! thread, at its first park (an idle moment, never on the frame path);
//! the warm-up below pays it on the measuring thread before measuring.
//!
//! `fine_quantiles` runs on the publisher thread once a second forever, so it is
//! held to zero as well.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_storage::hot_path_telemetry::{
    FINE_BUCKETS, Stage, fine_quantiles, record_receipt_to_fold_nanos, record_stage_nanos,
};
use tickvault_storage::hot_thread_sched::{
    register_main_worker_thread, release_main_worker_thread,
};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

#[test]
fn dhat_receipt_to_fold_stage_zero_alloc() {
    // Warm up: touch every static and read this thread's id once.
    record_stage_nanos(Stage::ReceiptToFold, 1);
    record_receipt_to_fold_nanos(1);
    let _slot = register_main_worker_thread();
    let warm = [1u64; FINE_BUCKETS];
    assert!(fine_quantiles(&warm).is_some());

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..50_000_u64 {
                // Fast frames, the odd slow one, and one past the top bound,
                // so every arm of the fine index is measured.
                let nanos = match i % 1_000 {
                    0 => 5_000_000,
                    1 => 40_000_000,
                    _ => 900 + (i % 64) * 1_000,
                };
                record_receipt_to_fold_nanos(nanos);
                if i % 100 == 0 {
                    let _slot = register_main_worker_thread();
                }
            }
        },
    );
    assert_eq!(
        (bytes, blocks),
        (0, 0),
        "record_receipt_to_fold_nanos allocated on the drain, or register_main_worker_thread on a worker park"
    );

    let mut window = [0u64; FINE_BUCKETS];
    window[3] = 9_000;
    window[20] = 90;
    window[FINE_BUCKETS - 1] = 1;
    let (q_bytes, q_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u64 {
                window[(i as usize) % FINE_BUCKETS] += 1;
                let q = fine_quantiles(&window).expect("samples");
                assert!(q.samples > 0);
            }
        },
    );
    assert_eq!(
        (q_bytes, q_blocks),
        (0, 0),
        "fine_quantiles allocated on the publisher thread"
    );
    release_main_worker_thread();
}
