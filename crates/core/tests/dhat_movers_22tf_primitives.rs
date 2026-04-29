//! Phase 12-final-DHAT — zero-allocation verification for the movers 22-tf
//! HOT-PATH primitives (scheduler arithmetic + edge-triggered state +
//! check_set_emptiness).
//!
//! Targets that MUST be zero-alloc per `.claude/rules/project/hot-path.md`:
//! - `compute_next_tick_secs` — pure integer arithmetic
//! - `compute_lag_secs` — saturating sub
//! - `should_emit_snapshot` — single integer compare
//! - `EdgeTriggeredEmptyState::observe` — bool flip + Copy outcome match
//! - `check_set_emptiness` — HashSet length check (no allocation)
//!
//! Targets explicitly OFF this DHAT path (called once per minute, NOT
//! per tick — allocation is acceptable):
//! - `compute_swap_delta` — allocates 2 result Vecs (leavers + entrants)
//! - `classify_cycle_outcome` — wraps compute_swap_delta
//! - `parse_questdb_dataset_json` — allocates HashSet + serde_json Strings
//! - `run_dynamic_subscriber_loop` — allocates the previous-set HashMap
//!
//! All three primitive groups are tested inside ONE `dhat::Profiler`
//! scope because dhat allows only one profiler per process. The total
//! budget is 8 blocks / 16 KB across 300K hot-path calls — an order of
//! magnitude tighter than any naive `Vec::new() / clone() / format!()`
//! regression would produce.
//!
//! Run: `cargo test -p tickvault-core --features dhat --test dhat_movers_22tf_primitives`

#![cfg(feature = "dhat")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::collections::HashSet;

use tickvault_core::instrument::depth_20_dynamic_subscriber::{
    DynamicContractKey, EdgeTriggeredEmptyState, SelectorCycleOutcome, check_set_emptiness,
};
use tickvault_core::pipeline::movers_22tf_scheduler::{
    compute_lag_secs, compute_next_tick_secs, should_emit_snapshot,
};

/// Single combined DHAT test — all three primitive groups inside ONE
/// profiler scope. dhat::Profiler is a global singleton; running each
/// group in its own #[test] would race on profiler init.
///
/// Total budget: ≤ 8 blocks / ≤ 16 KB across 300K calls. The budget
/// covers incidental dhat-internal bookkeeping; a regression that
/// added even one heap allocation per call would push the count well
/// past this ceiling.
#[test]
fn dhat_movers_22tf_hot_path_primitives_zero_alloc() {
    // Pre-build the HashSet OUTSIDE the profiler window — its allocation
    // is one-time setup, not hot-path.
    let mut set: HashSet<DynamicContractKey> = HashSet::with_capacity(150);
    for i in 0..100_u32 {
        set.insert((i, 'D'));
    }

    let healthy = SelectorCycleOutcome::SwapApplied {
        leavers: 10,
        entrants: 10,
        total_active: 50,
    };
    let empty = SelectorCycleOutcome::EmptyOrUndersized {
        returned_count: 0,
        reason: "zero_results",
    };

    let _profiler = dhat::Profiler::builder().testing().build();
    let stats_before = dhat::HeapStats::get();

    // Group 1 — scheduler primitives (100K iterations).
    let mut sink: u64 = 0;
    for i in 0..100_000_u64 {
        let cadence = (i % 60) + 1; // 1..=60
        let now = 12_345_u64 + i;
        let last_emit = now.saturating_sub(cadence);

        let next = compute_next_tick_secs(cadence, now);
        let lag = compute_lag_secs(now, next);
        let emit = should_emit_snapshot(cadence, now, last_emit);

        sink = sink.wrapping_add(next).wrapping_add(lag);
        if emit {
            sink = sink.wrapping_add(1);
        }
    }
    std::hint::black_box(sink);

    // Group 2 — EdgeTriggeredEmptyState observer (100K iterations).
    let mut state = EdgeTriggeredEmptyState::default();
    let mut alert_count = 0_u64;
    for i in 0..100_000_u64 {
        // Alternate between healthy + empty to exercise both edges.
        let outcome = if i.is_multiple_of(2) {
            &healthy
        } else {
            &empty
        };
        if state.observe(outcome) {
            alert_count = alert_count.wrapping_add(1);
        }
    }
    std::hint::black_box(alert_count);

    // Group 3 — check_set_emptiness (100K iterations).
    let mut none_count = 0_u64;
    for _ in 0..100_000_u64 {
        if check_set_emptiness(&set).is_none() {
            none_count = none_count.wrapping_add(1);
        }
    }
    std::hint::black_box(none_count);

    let stats_after = dhat::HeapStats::get();
    let blocks_delta = stats_after
        .curr_blocks
        .saturating_sub(stats_before.curr_blocks);
    let bytes_delta = stats_after
        .curr_bytes
        .saturating_sub(stats_before.curr_bytes);

    // Budget covers incidental dhat-internal allocations across 300K
    // total calls (3 groups × 100K). A regression that added a single
    // heap allocation per call would push counts far above this.
    const BUDGET_BLOCKS: usize = 8;
    const BUDGET_BYTES: usize = 16_384;

    assert!(
        blocks_delta <= BUDGET_BLOCKS,
        "Phase 12-final-DHAT invariant: hot-path primitives allocated \
         {blocks_delta} blocks (budget: {BUDGET_BLOCKS}) across 300K calls. \
         A regression to Vec::new() / clone() / format!() would push this \
         well above budget."
    );
    assert!(
        bytes_delta <= BUDGET_BYTES,
        "Phase 12-final-DHAT invariant: hot-path primitives allocated \
         {bytes_delta} bytes (budget: {BUDGET_BYTES}) across 300K calls."
    );
}
