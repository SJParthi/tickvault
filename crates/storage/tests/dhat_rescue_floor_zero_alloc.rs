//! DHAT zero-allocation ratchet for the queued-rescue floors (Z7) and the
//! durability-lag ring (Z8b), 2026-10-02.
//!
//! `hold_rescue_floor` runs on the frame drain whenever a batch is handed to
//! a rescue thread, and `release_rescue_floor` on that thread or on the
//! drain when the hand-off is refused. Both must stay atomics only: a
//! fixed table of compare-and-swaps, never a `Vec` or a lock. The lag ring
//! is touched once a second under the persist lock and is a fixed array; it
//! must never allocate after construction either, however many samples it
//! takes or however often it wraps.
//!
//! The full-table fallback (a counter and a range mark) is a cold arm and is
//! deliberately outside the measured window: every batch in the window
//! releases its floor before the next one holds.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_storage::wal_applied_watermark::{
    AppliedSink, DURABILITY_SAMPLE_SLOTS, DurabilityLag, applied_watermark,
};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

#[test]
fn dhat_rescue_floor_and_durability_lag_are_zero_allocation() {
    let wm = applied_watermark();
    let lag: &'static mut DurabilityLag = Box::leak(Box::new(DurabilityLag::new()));
    let base: u64 = 1_780_000_000_000_000_000;
    let mut held = 0u64;

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u64 {
                let sink = if i % 2 == 0 {
                    AppliedSink::Ticks
                } else {
                    AppliedSink::Depth
                };
                // Two batches queued at once, as with a rescue queue of 2.
                let a = wm.hold_rescue_floor(sink, base + i * 10, base + i * 10 + 5);
                let b = wm.hold_rescue_floor(sink, base + i * 10 + 6, base + i * 10 + 9);
                if a.is_some() && b.is_some() {
                    held += 1;
                }
                if let Some(f) = a {
                    wm.release_rescue_floor(f);
                }
                if let Some(f) = b {
                    wm.release_rescue_floor(f);
                }
                // One persist-tick sample per iteration, wrapping the ring
                // ~78 times; the jitter puts some samples inside the spacing
                // window, which refreshes the newest slot instead.
                let now = i * 1_000_000_000 - (i % 7) * 10;
                lag.record(now, i, i);
                std::hint::black_box(lag.durable_at(now, 60_000_000_000));
            }
        },
    );

    assert!(
        bytes == BUDGET_BYTES && blocks == BUDGET_BLOCKS,
        "the rescue floors / durability lag allocated {bytes} bytes / {blocks} blocks over \
         10,000 iterations. hold/release run on the frame drain's rescue arm and must stay \
         atomics only; the lag ring is a fixed array."
    );
    // Non-vacuity: the floors really were taken, and the ring really wrapped.
    assert!(held > 0, "the floors were held");
    assert!(10_000 > 2 * DURABILITY_SAMPLE_SLOTS, "the ring wrapped");
}
