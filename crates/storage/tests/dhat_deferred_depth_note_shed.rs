//! DHAT zero-allocation ratchet for `DeferredDepth::note_shed` (plan item 45a,
//! 2026-09-29).
//!
//! `note_shed` runs on the frame drain for every packet the ingest shed skips
//! — on a shed session that is every Full-mode packet of the main feed. It
//! must stay atomics only: an allocation here would be millions per session
//! on the one task that empties the sockets. This pins it at zero, across the
//! common case (bucket already marked), the first mark of a new bucket, and
//! the overflow arm.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_storage::wal_deferred_depth::{
    DEFERRED_BUCKET_SHIFT, DEFERRED_BUCKETS, DeferredDepth, ShedDepth,
};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

#[test]
fn dhat_note_shed_is_zero_allocation_on_every_arm() {
    // A leaked 'static table, built BEFORE the window opens.
    let marks: &'static DeferredDepth = Box::leak(Box::new(DeferredDepth::new_for_tests()));
    let base: u64 = 1_780_000_000_000_000_000;

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u64 {
                // Mostly the same bucket (the common case), a new bucket every
                // 100 calls, and a colliding bucket every 1,000 (overflow arm).
                let seq = if i % 1_000 == 999 {
                    base + ((DEFERRED_BUCKETS as u64) << DEFERRED_BUCKET_SHIFT)
                } else {
                    base + ((i / 100) << DEFERRED_BUCKET_SHIFT) + i
                };
                let kind = if i % 2 == 0 {
                    ShedDepth::Inline
                } else {
                    ShedDepth::Dedicated
                };
                marks.note_shed(seq, kind);
                std::hint::black_box(i);
            }
        },
    );

    assert!(
        bytes == BUDGET_BYTES && blocks == BUDGET_BLOCKS,
        "DeferredDepth::note_shed allocated {bytes} bytes / {blocks} blocks over 10,000 \
         calls. It runs on the frame drain for every shed packet and must stay atomics \
         only — no Vec, no String, no metrics macro."
    );

    // Non-vacuity: the calls really did mark buckets and hit the overflow arm.
    let snap = marks.snapshot();
    assert!(!snap.marked_buckets().is_empty(), "marks were recorded");
    assert!(snap.overflowed, "the collision arm was exercised");
}
