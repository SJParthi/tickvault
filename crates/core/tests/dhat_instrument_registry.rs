//! DHAT allocation test for InstrumentRegistry O(1) lookups.
//!
//! Separate binary from other DHAT tests because DHAT allows only one
//! profiler per process. This test verifies Principle #1 for the
//! instrument registry lookup on the tick-processing hot path.
//!
//! Every incoming tick calls `registry.get(security_id)` to resolve
//! the instrument metadata. This MUST be zero-allocation since HashMap
//! lookups are pointer-based with no heap activity.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn dhat_instrument_registry_lookups_zero_alloc() {
    use dhan_live_trader_common::instrument_registry::{
        InstrumentRegistry, SubscriptionCategory, make_display_index_instrument,
        make_major_index_instrument,
    };
    use dhan_live_trader_common::types::FeedMode;

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
    let _warmup = registry.get(13);

    // ---- Measure: all subsequent lookups must be zero-allocation ----
    let stats_before = dhat::HeapStats::get();

    // Simulate hot-path: 1000 registry lookups (what tick processor does)
    for _ in 0..1000 {
        // Hit: known security_ids
        let r1 = registry.get(13);
        assert!(r1.is_some());
        assert_eq!(
            r1.expect("nifty").category,
            SubscriptionCategory::MajorIndexValue
        );

        let r2 = registry.get(21);
        assert!(r2.is_some());
        assert_eq!(
            r2.expect("vix").category,
            SubscriptionCategory::DisplayIndex
        );

        // Miss: unknown security_id (also must be zero-alloc)
        let r3 = registry.get(99999);
        assert!(r3.is_none());
    }

    // Also verify contains() and len() are zero-alloc
    for _ in 0..1000 {
        assert!(registry.contains(13));
        assert!(!registry.contains(99999));
        assert_eq!(registry.len(), 5);
        assert!(!registry.is_empty());
    }

    // ---- End measurement ----
    let stats_after = dhat::HeapStats::get();
    let allocs_during = stats_after
        .total_blocks
        .saturating_sub(stats_before.total_blocks);

    assert_eq!(
        allocs_during, 0,
        "InstrumentRegistry lookups allocated {} blocks over 4000 operations — PRINCIPLE #1 VIOLATED.\n\
         HashMap get/contains/len must be zero-allocation on the hot path.",
        allocs_during
    );
}
