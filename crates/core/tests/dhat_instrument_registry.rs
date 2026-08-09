//! DHAT allocation test for InstrumentRegistry O(1) lookups.
//!
//! Separate binary from other DHAT tests because DHAT allows only one
//! profiler per process. This test verifies Principle #1 for the
//! instrument registry lookup on the tick-processing hot path.
//!
//! Every incoming tick calls `registry.get_with_segment(security_id, segment)`
//! to resolve the instrument metadata — the segment comes from byte 3 of the
//! 8-byte binary header. This MUST be zero-allocation since HashMap lookups are
//! pointer-based with no heap activity.
//!
//! 2026-08-08: migrated from the id-only `registry.get()` / `contains()`, which
//! were DELETED — they read a legacy single-segment index and returned the
//! wrong instrument on a cross-segment id reuse (I-P1-11). This test now
//! measures the SAME path production actually executes, which is what a
//! hot-path allocation budget should guard.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod dhat_support;

#[test]
fn dhat_instrument_registry_lookups_zero_alloc() {
    use tickvault_common::instrument_registry::{
        InstrumentRegistry, SubscriptionCategory, make_display_index_instrument,
        make_major_index_instrument,
    };
    use tickvault_common::types::{ExchangeSegment, FeedMode};

    let _profiler = dhat::Profiler::builder().testing().build();

    // Pre-allocate: build a registry with instruments
    let instruments = vec![
        make_major_index_instrument(13, "NIFTY 50", FeedMode::Ticker),
        make_major_index_instrument(25, "NIFTY BANK", FeedMode::Ticker),
        make_display_index_instrument(21, "INDIA VIX", FeedMode::Ticker),
        make_display_index_instrument(99, "NIFTY IT", FeedMode::Ticker),
        make_display_index_instrument(100, "NIFTY PHARMA", FeedMode::Ticker),
    ];
    let registry = InstrumentRegistry::from_instruments(instruments);

    // Warm up: first lookup may initialize internal state
    let _warmup = registry.get_with_segment(13, ExchangeSegment::IdxI);

    // ---- Measure: all subsequent lookups must be zero-allocation ----
    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners — see
    // dhat_support/mod.rs). Budget UNCHANGED: exactly 0 blocks.
    let (_, allocs_during) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            // Simulate hot-path: 1000 registry lookups (what tick processor does)
            for _ in 0..1000 {
                // Hit: known security_ids
                let r1 = registry.get_with_segment(13, ExchangeSegment::IdxI);
                assert!(r1.is_some());
                assert_eq!(
                    r1.expect("nifty").category,
                    SubscriptionCategory::MajorIndexValue
                );

                let r2 = registry.get_with_segment(21, ExchangeSegment::IdxI);
                assert!(r2.is_some());
                assert_eq!(
                    r2.expect("vix").category,
                    SubscriptionCategory::DisplayIndex
                );

                // Miss: unknown security_id (also must be zero-alloc)
                let r3 = registry.get_with_segment(99999, ExchangeSegment::IdxI);
                assert!(r3.is_none());
            }

            // Also verify contains_with_segment() and len() are zero-alloc
            for _ in 0..1000 {
                assert!(registry.contains_with_segment(13, ExchangeSegment::IdxI));
                assert!(!registry.contains_with_segment(99999, ExchangeSegment::IdxI));
                assert_eq!(registry.len(), 5);
                assert!(!registry.is_empty());
            }
        },
    );

    assert_eq!(
        allocs_during, 0,
        "InstrumentRegistry lookups allocated {} blocks over 4000 operations — PRINCIPLE #1 VIOLATED.\n\
         HashMap get/contains/len must be zero-allocation on the hot path.",
        allocs_during
    );
}
