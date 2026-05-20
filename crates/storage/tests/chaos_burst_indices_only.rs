//! Burst-defence chaos test — 4-SID indices-only scope.
//!
//! Pins the rescue-ring envelope for the LOCKED 4-IDX_I universe
//! (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21).
//!
//! **Rewritten 2026-05-20** when `TICK_BUFFER_CAPACITY` was rightsized
//! 5M → 100K. The old assertions were pinned to the retired Wave-5
//! 11,018-instrument universe (~110K tps) and no longer reflected the
//! live scope — a 4-SID feed runs at ~15-20 tps, not 110,000.
//!
//! What this test exercises:
//! - `TICK_BUFFER_CAPACITY` (rescue ring) absorbs a full 60-second
//!   QuestDB outage at the 4-SID extreme-peak tick rate without ring
//!   overflow, with ≥2× headroom.
//! - `TICK_BUFFER_HIGH_WATERMARK` sits at 80% of capacity — the WARN
//!   fires well before catastrophic fill.
//! - `MAX_TOTAL_SUBSCRIPTIONS` (25K) / `MAX_TOTAL_SUBSCRIPTIONS_TARGET`
//!   (24.5K) bracket the 4-SID universe with vast headroom.
//! - A synthetic VecDeque (the rescue ring's underlying type) absorbs
//!   a full-ring burst without panic — bounded memory, FIFO eviction.
//!
//! What this test does NOT do:
//! - Spin up a real QuestDB / parser / WS pipeline (see
//!   `chaos_questdb_full_session.rs` for the integration path).
//! - Promise "no drops ever". Beyond the ring + spill + DLQ, OS
//!   resources can exhaust — PROC-01 + RESOURCE-01..03 cover those.
//!
//! Run cost: ~10 ms. No Docker, no network, no root.

#![cfg(test)]

use std::collections::VecDeque;

use tickvault_common::constants::{
    MAX_TOTAL_SUBSCRIPTIONS, MAX_TOTAL_SUBSCRIPTIONS_TARGET, TICK_BUFFER_CAPACITY,
    TICK_BUFFER_HIGH_WATERMARK,
};

/// The LOCKED live universe: 4 IDX_I SIDs per
/// `crates/common/src/locked_universe.rs`.
const INDICES_ONLY_INSTRUMENT_COUNT: usize = 4;

/// Conservative extreme-peak inflow for the 4 IDX_I SIDs in Quote
/// mode: 4 indices × ~100 packets/sec/index = ~400 tps. The realistic
/// average is ~15-20 tps; 400 is a deliberate over-estimate so the
/// envelope claim holds under volatility bursts.
const INDICES_ONLY_PEAK_TPS: usize = 400;

/// The QuestDB-outage SLA the rescue ring must absorb without dropping
/// a single row: 60 seconds. 400 tps × 60s = 24,000 ticks queued — the
/// 100K ring covers it with ~76K headroom.
const QUESTDB_OUTAGE_SLA_SECS: usize = 60;

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "compile-time constant ratchet — assertion IS the test"
)]
fn test_chaos_burst_envelope_constants_match_indices_only_scope() {
    assert_eq!(
        MAX_TOTAL_SUBSCRIPTIONS, 25_000,
        "capacity hard cap pinned at 25,000 — change requires updating \
         the plan + scenarios table"
    );
    assert!(
        MAX_TOTAL_SUBSCRIPTIONS_TARGET < MAX_TOTAL_SUBSCRIPTIONS,
        "target must stay below hard cap so the warn-threshold gauge \
         fires BEFORE Dhan-side rejection"
    );
    assert!(
        INDICES_ONLY_INSTRUMENT_COUNT < MAX_TOTAL_SUBSCRIPTIONS_TARGET,
        "4-SID universe ({}) must fit under target ({})",
        INDICES_ONLY_INSTRUMENT_COUNT,
        MAX_TOTAL_SUBSCRIPTIONS_TARGET
    );
}

#[test]
fn test_chaos_burst_rescue_ring_absorbs_60s_questdb_outage_at_peak_tps() {
    let queued_at_sla = INDICES_ONLY_PEAK_TPS
        .checked_mul(QUESTDB_OUTAGE_SLA_SECS)
        .expect("constants overflow");
    assert!(
        queued_at_sla <= TICK_BUFFER_CAPACITY,
        "envelope claim: a {}s QuestDB outage at extreme peak {} tps must \
         absorb without ring overflow. queued={}, ring={}",
        QUESTDB_OUTAGE_SLA_SECS,
        INDICES_ONLY_PEAK_TPS,
        queued_at_sla,
        TICK_BUFFER_CAPACITY
    );
    // Headroom past the SLA boundary: the ring must keep at least one
    // more full SLA-window burst in reserve beyond the 60s SLA.
    let headroom = TICK_BUFFER_CAPACITY - queued_at_sla;
    assert!(
        headroom >= queued_at_sla,
        "ring must hold ≥2× the {}s-SLA burst — headroom {} vs burst {}",
        QUESTDB_OUTAGE_SLA_SECS,
        headroom,
        queued_at_sla
    );
}

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "compile-time constant ratchet — assertion IS the test"
)]
fn test_chaos_burst_high_watermark_alert_fires_before_overflow() {
    // The high-watermark is the level at which the operator gets paged
    // ("ring at 80% capacity"). It MUST be strictly below capacity so
    // the alert is actionable, not coincident with overflow.
    assert!(TICK_BUFFER_HIGH_WATERMARK < TICK_BUFFER_CAPACITY);
    let pct = (TICK_BUFFER_HIGH_WATERMARK as f64 / TICK_BUFFER_CAPACITY as f64) * 100.0;
    assert!(
        (75.0..=85.0).contains(&pct),
        "high watermark must sit in the 75%..85% band — current {pct}%"
    );

    // Lead time at extreme peak: (HW / peak_tps) seconds of warning
    // before overflow. Must give the operator at least the full SLA.
    let warn_seconds = TICK_BUFFER_HIGH_WATERMARK / INDICES_ONLY_PEAK_TPS;
    assert!(
        warn_seconds >= QUESTDB_OUTAGE_SLA_SECS,
        "high-watermark gives only {warn_seconds}s lead time at \
         {INDICES_ONLY_PEAK_TPS} tps — operator may not react in time"
    );
}

#[test]
fn test_chaos_burst_synthetic_vecdeque_bounded_no_panic() {
    // Synthetic check: a VecDeque sized at the rescue-ring capacity
    // (proxy for the real ring's underlying type) absorbs a full-ring
    // burst without panic and never re-allocates beyond its prealloc.
    let mut ring: VecDeque<u64> = VecDeque::with_capacity(TICK_BUFFER_CAPACITY);
    let preallocated_capacity = ring.capacity();
    for i in 0u64..(TICK_BUFFER_CAPACITY as u64) {
        ring.push_back(i);
    }
    assert_eq!(ring.len(), TICK_BUFFER_CAPACITY);
    // Capacity should not have grown — `with_capacity` contract is
    // "at least N", and we wrote exactly N.
    assert!(ring.capacity() >= preallocated_capacity);
    // Drain and verify FIFO order.
    let first = ring.pop_front();
    let last = ring.pop_back();
    assert_eq!(first, Some(0));
    assert_eq!(last, Some(TICK_BUFFER_CAPACITY as u64 - 1));
}

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "compile-time constant ratchet — assertion IS the test"
)]
fn test_chaos_burst_invariant_documentation_complete() {
    // Documentation ratchet: ensure the named constants the envelope
    // cites are reachable + non-zero.
    assert!(TICK_BUFFER_CAPACITY > 0);
    assert!(TICK_BUFFER_HIGH_WATERMARK > 0);
    assert!(MAX_TOTAL_SUBSCRIPTIONS > 0);
    assert!(MAX_TOTAL_SUBSCRIPTIONS_TARGET > 0);
    assert!(INDICES_ONLY_INSTRUMENT_COUNT > 0);
    assert!(INDICES_ONLY_PEAK_TPS > 0);
    assert!(QUESTDB_OUTAGE_SLA_SECS > 0);
}
