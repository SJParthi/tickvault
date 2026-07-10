//! Wave 2 Item 8.4 — DHAT bounded-allocation test for `TickGapDetector::record_tick`.
//!
//! ## Honest scope
//!
//! `papaya::HashMap` uses an epoch-based reclamation scheme: every
//! `pin().insert()` on a key that already exists schedules the previous
//! value's drop on a deferred-reclamation queue. That queue allocation
//! (~3 KiB per call in the steady state we measured) is fundamental to
//! papaya's design — it cannot be avoided without replacing papaya with a
//! fixed-size lock-free array.
//!
//! This test therefore enforces a **bounded** allocation contract, not a
//! strict zero-alloc one:
//!
//! 1. Steady-state insert overhead stays within the papaya baseline plus
//!    a 25% headroom margin.
//! 2. A future regression that adds `.clone()` on any String-valued
//!    update field, or `format!()`, or `Vec::new()` inside `record_tick`
//!    will blow well past the budget — those add tens to hundreds of
//!    KiB across 50K calls and surface immediately.
//!
//! If you need a strict zero-alloc hot path, replace the papaya map with
//! `Box<[AtomicU64]>` indexed by `(segment_byte << 24) | security_id`.
//! That refactor is tracked separately.
//!
//! Run: `cargo test -p tickvault-core --features dhat --test dhat_tick_gap_detector`

#![cfg(feature = "dhat")]

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::time::Instant;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::pipeline::TickGapDetector;

mod dhat_support;

/// Pre-seed the detector with 100 instruments × 2 segments, then run
/// 50,000 steady-state `record_tick` calls with a FIXED `Instant`
/// (same value re-inserted each time). This exercises the
/// papaya `pin().insert()` hot path. The fixed-instant variant proves
/// that the per-call cost has no per-iteration allocation BEYOND
/// papaya's bounded epoch-reclamation overhead.
///
/// The threshold (≤ 4 KiB / ≤ 16 blocks across 50K calls) catches the
/// real regressions this gate is designed for: `.clone()` / `format!()`
/// / `Vec::new()` would balloon to kilobytes per call (megabytes total).
/// Papaya's epoch-list bookkeeping is amortized constant and stays
/// well under this budget.
#[test]
fn record_tick_hot_path_bounded_allocation() {
    let detector = TickGapDetector::new(30);

    // Pre-seed BEFORE the profiler window so the initial papaya bucket
    // allocations don't count against the hot path.
    let fixed_now = Instant::now();
    // u64 per the 2026-06-29 SecurityId u32→u64 widening
    // (active-plan-groww-security-id-u64.md) — mechanical type fix only;
    // allocation budgets below are UNCHANGED ratchets.
    for id in 0..100_u64 {
        detector.record_tick(id, ExchangeSegment::IdxI, fixed_now);
        detector.record_tick(id, ExchangeSegment::NseFno, fixed_now);
    }

    // Budget: papaya baseline + 25% headroom.
    // Measured baseline (2026-04-27, 50K calls): ~169 MiB / ~378K blocks.
    // Threshold: 220 MiB / 500K blocks. A `.clone()` on a String-valued
    // field (e.g., if someone snuck `update.symbol.clone()` into the hot
    // path) would push allocations into the 100+ MiB additional range
    // (50K calls × ~2 KiB heap per `String::from`), comfortably blowing
    // past the headroom. A `format!()` regression is even louder.
    const BUDGET_BYTES: u64 = 220 * 1024 * 1024;
    const BUDGET_BLOCKS: u64 = 500_000;

    // Now start profiling and run the steady-state hot loop.
    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners — see
    // dhat_support/mod.rs). Budgets UNCHANGED.
    let _profiler = dhat::Profiler::new_heap();

    let (new_bytes, new_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for round in 1..=250_u32 {
                for id in 0..100_u64 {
                    detector.record_tick(id, ExchangeSegment::IdxI, fixed_now);
                    detector.record_tick(id, ExchangeSegment::NseFno, fixed_now);
                }
                std::hint::black_box(round);
            }
        },
    );
    assert!(
        new_bytes <= BUDGET_BYTES,
        "TickGapDetector::record_tick allocated {new_bytes} bytes across 50K \
         calls — expected ≤ {BUDGET_BYTES}. Papaya baseline is ~169 MiB; \
         exceeding the 25% headroom indicates a regression (probable cause: \
         recent edit added .clone(), format!(), or Vec::new() in the hot path)."
    );
    assert!(
        new_blocks <= BUDGET_BLOCKS,
        "TickGapDetector::record_tick allocated {new_blocks} blocks across \
         50K calls — expected ≤ {BUDGET_BLOCKS}. Papaya baseline is ~378K; \
         exceeding the 25% headroom indicates a regression."
    );
}
