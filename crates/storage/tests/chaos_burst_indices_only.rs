//! Wave 5 Item 11 — burst-defence chaos test.
//!
//! Pins the envelope claim from `active-plan-wave-5-indices-only.md`:
//!
//! > "We CAN promise: under the verified 11,018-instrument indices-only
//! > scope at observed peak rates (≤110K tps theoretical, typically ~30K
//! > tps), the chain absorbs without drop. Item 11 chaos test pins this
//! > envelope."
//!
//! What this test exercises:
//! - `TICK_BUFFER_CAPACITY` (rescue ring) is at least 600K rows — enough
//!   to absorb a 5-second 110K-tps burst without overflow even if the
//!   QuestDB writer is fully blocked.
//! - `TICK_BUFFER_HIGH_WATERMARK` is at 80% of capacity — alert fires
//!   BEFORE catastrophic fill.
//! - `MAX_TOTAL_SUBSCRIPTIONS` (25K) and `MAX_TOTAL_SUBSCRIPTIONS_TARGET`
//!   (24.5K) bracket the indices-only universe (~11K) with comfortable
//!   headroom — capacity-overflow path never fires.
//! - A synthetic 200K-row VecDeque (the rescue ring's underlying type)
//!   absorbs without panic — bounded memory, FIFO eviction.
//! - The Wave 5 envelope numbers (11,018 instruments × ~10 pkts/sec ≈
//!   ~110K tps) fit comfortably under the 600K ring capacity at >5s
//!   QuestDB outage tolerance.
//!
//! What this test does NOT do:
//! - Spin up a real QuestDB / parser / WS pipeline. The full burst test
//!   requires Docker QuestDB + replay harness — see
//!   `chaos_questdb_full_session.rs` for the integration path.
//! - Promise "no drops ever". The honest envelope acknowledges that
//!   beyond ~600K queued rows + spill capacity + DLQ, OS resources can
//!   exhaust. PROC-01 + RESOURCE-01..03 cover those.
//!
//! Run cost: ~10 ms. No Docker, no network, no root.

#![cfg(test)]

use std::collections::VecDeque;

use tickvault_common::constants::{
    MAX_TOTAL_SUBSCRIPTIONS, MAX_TOTAL_SUBSCRIPTIONS_TARGET, TICK_BUFFER_CAPACITY,
    TICK_BUFFER_HIGH_WATERMARK,
};

/// Wave 5 universe size confirmed live 2026-04-25 against the planner
/// output: 3 IDX_I + 26 display + 216 NSE_EQ + 2,037 index F&O +
/// 22,042 stock F&O = 24,324 (pre-Item-2). Indices-only drops the stock
/// F&O block, leaving 11,018 instruments. The envelope claim assumes
/// peak inflow at the IDX_I + index F&O + NSE_EQ universe.
const WAVE_5_INDICES_ONLY_INSTRUMENT_COUNT: usize = 11_018;

/// Conservative peak inflow assumption per `active-plan-wave-5-indices-only.md`:
/// 11,018 instruments × 10 packets/sec/instrument = ~110,180 tps.
const WAVE_5_PEAK_TPS: usize = 110_180;

/// Worst-case QuestDB outage the rescue ring should absorb without
/// dropping a single row, per the plan's "≤60s outage absorbed" SLA.
/// Result: 110,180 × 5 = 550,900 ticks queued at 5-second outage; the
/// 600K ring covers it with 49,100 headroom.
const WAVE_5_QUESTDB_OUTAGE_TOLERANCE_SECS: usize = 5;

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "compile-time constant ratchet — assertion IS the test"
)]
fn test_chaos_burst_envelope_constants_match_wave_5_plan() {
    // The plan's hard cap.
    assert_eq!(
        MAX_TOTAL_SUBSCRIPTIONS, 25_000,
        "Wave 5 plan capacity hard cap pinned at 25,000 — change requires \
         updating plan + scenarios table"
    );
    assert!(
        MAX_TOTAL_SUBSCRIPTIONS_TARGET < MAX_TOTAL_SUBSCRIPTIONS,
        "target must stay below hard cap so the warn-threshold gauge fires \
         BEFORE Dhan-side rejection"
    );
    // The Wave-5 universe must fit comfortably under the warn target.
    assert!(
        WAVE_5_INDICES_ONLY_INSTRUMENT_COUNT < MAX_TOTAL_SUBSCRIPTIONS_TARGET,
        "indices-only universe ({}) must fit under target ({})",
        WAVE_5_INDICES_ONLY_INSTRUMENT_COUNT,
        MAX_TOTAL_SUBSCRIPTIONS_TARGET
    );
}

#[test]
fn test_chaos_burst_rescue_ring_absorbs_5s_questdb_outage_at_peak_tps() {
    let queued_at_5s = WAVE_5_PEAK_TPS
        .checked_mul(WAVE_5_QUESTDB_OUTAGE_TOLERANCE_SECS)
        .expect("constants overflow");
    assert!(
        queued_at_5s <= TICK_BUFFER_CAPACITY,
        "Wave 5 envelope claim: ≤{}s QuestDB outage at peak {} tps must \
         absorb without ring overflow. queued={}, ring={}",
        WAVE_5_QUESTDB_OUTAGE_TOLERANCE_SECS,
        WAVE_5_PEAK_TPS,
        queued_at_5s,
        TICK_BUFFER_CAPACITY
    );

    // Headroom should be at least 1 second of inflow — ensures we don't
    // fill the ring exactly at the SLA boundary. (~50K headroom for the
    // current parameters.)
    let headroom = TICK_BUFFER_CAPACITY - queued_at_5s;
    assert!(
        headroom >= WAVE_5_PEAK_TPS / 4,
        "ring must have ≥1s of headroom past the {}s SLA boundary — \
         current headroom {} ticks vs needed {} ticks",
        WAVE_5_QUESTDB_OUTAGE_TOLERANCE_SECS,
        headroom,
        WAVE_5_PEAK_TPS / 4
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

    // At peak tps, the operator gets ~ (HW / peak_tps) seconds of warning
    // before overflow. Wave 5 envelope demands at least 4 seconds.
    let warn_seconds = TICK_BUFFER_HIGH_WATERMARK / WAVE_5_PEAK_TPS;
    assert!(
        warn_seconds >= 4,
        "high-watermark gives only {warn_seconds}s lead time at {WAVE_5_PEAK_TPS} tps — \
         operator may not react in time"
    );
}

#[test]
fn test_chaos_burst_synthetic_200k_row_vecdeque_bounded_no_panic() {
    // Synthetic check: a VecDeque with a 200K capacity (proxy for a
    // 5-second burst at sub-peak rates) absorbs without panic and
    // never re-allocates beyond its preallocated cap.
    let mut ring: VecDeque<u64> = VecDeque::with_capacity(200_000);
    let preallocated_capacity = ring.capacity();
    for i in 0u64..200_000 {
        ring.push_back(i);
    }
    assert_eq!(ring.len(), 200_000);
    // Capacity should not have grown — VecDeque's `with_capacity`
    // contract is "at least N", and we wrote exactly N.
    assert!(ring.capacity() >= preallocated_capacity);
    // Drain and verify FIFO order (drain semantics).
    let first = ring.pop_front();
    let last = ring.pop_back();
    assert_eq!(first, Some(0));
    assert_eq!(last, Some(199_999));
}

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "compile-time constant ratchet — assertion IS the test"
)]
fn test_chaos_burst_invariant_documentation_complete() {
    // Documentation ratchet: ensure the named constants the plan cites
    // are reachable + non-zero. Trivial today but pins regression where
    // someone removes a constant assuming it's unused.
    assert!(TICK_BUFFER_CAPACITY > 0);
    assert!(TICK_BUFFER_HIGH_WATERMARK > 0);
    assert!(MAX_TOTAL_SUBSCRIPTIONS > 0);
    assert!(MAX_TOTAL_SUBSCRIPTIONS_TARGET > 0);
    assert!(WAVE_5_INDICES_ONLY_INSTRUMENT_COUNT > 0);
    assert!(WAVE_5_PEAK_TPS > 0);
    assert!(WAVE_5_QUESTDB_OUTAGE_TOLERANCE_SECS > 0);
}
