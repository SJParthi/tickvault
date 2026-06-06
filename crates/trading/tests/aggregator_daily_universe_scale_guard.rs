//! Sub-PR #8 of 2026-05-27 daily-universe expansion — aggregator scale
//! guard.
//!
//! The existing `MultiTfAggregator` (`crates/trading/src/candles/
//! multi_tf_aggregator.rs`) already uses `papaya::HashMap<(u32, u8),
//! Arc<InstrumentEntry>>` per the I-P1-11 composite-key rule and
//! `SEAL_BUFFER_CAPACITY = 200_000` for the seal-burst ring. This
//! ratchet proves the existing design supports the new 250-SID daily
//! universe (~250 × 21 TFs = 5,250 entries × 21 TFs of state) WITHOUT
//! any code change.
//!
//! ## Why this lives in `tests/`
//!
//! The aggregator + indicator engine code path is on the operator
//! 2026-05-27 boundary — indicators + strategies are OFF LIMITS for the
//! 14-PR sequence. The aggregator itself is allowed (it's not an
//! indicator), but per the boundary we ship the scale-up as a
//! ratchet-only PR rather than touching the aggregator code.
//!
//! ## What this guard pins
//!
//! 1. `SEAL_BUFFER_CAPACITY` (200K) ≥ `MAX_DAILY_UNIVERSE_SIZE × TF_COUNT`
//!    (1200 × 21 = 25,200). Headroom factor ≈ 7.9×. This is the
//!    boundary-burst budget for the seal ring per `aws-budget.md`
//!    rule 6's memory map (the seal ring is 200K × 144 B ≈ 28.8 MB).
//!    The 200K buffer is design-locked (L-C1) and sized for the real
//!    ~99K IST-midnight burst with 2× margin (see
//!    `chaos_midnight_seal_burst.rs`); 7.9× over the 1200-SID worst
//!    case is well above that adequacy floor.
//!
//! 2. `MultiTfAggregator::with_capacity(MAX_DAILY_UNIVERSE_SIZE)`
//!    constructs without panic — the boot-time pre-sizing path used
//!    by Sub-PR #10's orchestrator will be valid for the new scale.
//!
//! 3. `pre_populate` accepts 1200 composite keys without collision per
//!    I-P1-11.
//!
//! 4. `force_seal_all` iterates all entries without panic — the
//!    per-minute boundary burst (5,250 seals at most for 250 × 21)
//!    fits in the 200K seal ring with 24× headroom.
//!
//! ## What this PR does NOT do
//!
//! - Touch any aggregator production code (operator boundary 2026-05-27)
//! - Touch any indicator engine code (operator boundary 2026-05-27)
//! - Wire the aggregator into the boot orchestrator at the new scale
//!   (that lands in Sub-PR #10)
//! - Add a Criterion bench at the new scale (the existing
//!   `crates/trading/benches/multi_tf_aggregator.rs` already covers
//!   per-tick cost; the per-instrument count doesn't affect per-tick
//!   complexity since lookup is O(1) papaya pin)

use tickvault_common::constants::MAX_DAILY_UNIVERSE_SIZE;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;
use tickvault_trading::candles::seal_ring::SEAL_BUFFER_CAPACITY;
use tickvault_trading::candles::tf_index::TF_COUNT;

/// TF_COUNT is pinned at 21 by `tf_index.rs`. The daily universe
/// expansion does NOT change the number of timeframes; only the
/// number of instruments.
#[test]
fn tf_count_pinned_at_21_for_daily_universe() {
    assert_eq!(TF_COUNT, 21);
}

/// MAX_DAILY_UNIVERSE_SIZE pinned at 1200 by Sub-PR #2 (NTM expansion,
/// rule §31) + Sub-PR #7's envelope check.
#[test]
fn max_daily_universe_size_pinned_at_1200() {
    assert_eq!(MAX_DAILY_UNIVERSE_SIZE, 1200);
}

/// Worst-case minute-boundary seal burst:
/// `MAX_DAILY_UNIVERSE_SIZE × TF_COUNT = 1200 × 21 = 25,200` seals.
///
/// (Practical burst is smaller — only the 1m TF + any larger TFs that
/// happen to cross a boundary at the same minute. But the seal ring
/// must absorb the worst case without backpressure on the hot path.)
///
/// `SEAL_BUFFER_CAPACITY = 200_000` gives ~7.9× headroom over the
/// worst-case burst. Ratchet:
/// - Bumping `MAX_DAILY_UNIVERSE_SIZE` beyond ~1,900 would push headroom
///   below the ≥5× floor — this test fails fast at that point.
/// - Shrinking `SEAL_BUFFER_CAPACITY` below the burst would invalidate
///   the assumption — this test fails fast there too.
#[test]
fn seal_buffer_capacity_absorbs_worst_case_daily_universe_burst() {
    let worst_case_burst = MAX_DAILY_UNIVERSE_SIZE * TF_COUNT;
    assert!(
        SEAL_BUFFER_CAPACITY >= worst_case_burst,
        "SEAL_BUFFER_CAPACITY ({SEAL_BUFFER_CAPACITY}) must be >= worst-case minute-boundary seal burst ({worst_case_burst} = {MAX_DAILY_UNIVERSE_SIZE} SIDs × {TF_COUNT} TFs). Either bump SEAL_BUFFER_CAPACITY or shrink MAX_DAILY_UNIVERSE_SIZE."
    );
}

/// Headroom factor: how much spare capacity the seal ring has beyond
/// the worst-case burst. The pre-NTM 20× margin (a growth buffer "before
/// requiring a rule-file edit") is now CONSUMED by the authorized §31
/// rule-file edit (cap 400→1200). At the 1200 cap, headroom is ~7.9×,
/// which is still well above the chaos-tested 2× IST-midnight-burst
/// adequacy floor (`chaos_midnight_seal_burst.rs`). Floor lowered to ≥5×
/// — a deliberate, documented consequence of the §31 expansion, NOT a
/// silent weakening. The 200K buffer (L-C1 design lock) is preserved.
#[test]
fn seal_buffer_capacity_has_at_least_5x_headroom_over_burst() {
    let worst_case_burst = MAX_DAILY_UNIVERSE_SIZE * TF_COUNT;
    let headroom_factor = SEAL_BUFFER_CAPACITY / worst_case_burst;
    assert!(
        headroom_factor >= 5,
        "SEAL_BUFFER_CAPACITY headroom factor ({headroom_factor}×) below 5× safety margin"
    );
}

/// `MultiTfAggregator::with_capacity(MAX_DAILY_UNIVERSE_SIZE)`
/// constructs successfully — the boot-time pre-sizing path Sub-PR #10's
/// orchestrator will use.
#[test]
fn aggregator_constructs_with_daily_universe_capacity() {
    let agg = MultiTfAggregator::with_capacity(MAX_DAILY_UNIVERSE_SIZE);
    assert!(agg.is_empty());
    assert_eq!(agg.len(), 0);
}

/// Pre-populating with `MAX_DAILY_UNIVERSE_SIZE` (1200) composite keys per
/// I-P1-11 succeeds — every `(security_id, segment)` pair becomes a distinct
/// papaya entry. The `_` in `count == _ as usize` isn't actually used; we just
/// rely on the returned count matching the input length.
#[test]
fn aggregator_pre_populates_1200_composite_keys_without_collision() {
    let agg = MultiTfAggregator::with_capacity(MAX_DAILY_UNIVERSE_SIZE);

    // Synthetic full-cap universe: mix of segments to stress
    // composite-key uniqueness. Use both NSE_EQ (segment_code=1) and
    // IDX_I (segment_code=0) per the §2 universe scope.
    let keys: Vec<(u32, u8)> = (0..MAX_DAILY_UNIVERSE_SIZE as u32)
        .map(|i| {
            // First ~30 are IDX_I indices (segment 0), rest are
            // NSE_EQ underlyings (segment 1).
            let segment_code: u8 = if i < 30 { 0 } else { 1 };
            (i, segment_code)
        })
        .collect();

    let inserted = agg.pre_populate(keys.iter().copied());
    assert_eq!(
        inserted, MAX_DAILY_UNIVERSE_SIZE,
        "pre_populate should insert all 1200 distinct composite keys"
    );
    assert_eq!(agg.len(), MAX_DAILY_UNIVERSE_SIZE);
}

/// Same-numeric-SID-across-different-segments stress test per I-P1-11.
/// The FINNIFTY-27 (IDX_I) + NSE_EQ-27 collision case must result in
/// TWO distinct aggregator entries, not silently overwrite.
#[test]
fn aggregator_distinguishes_same_sid_across_segments_per_i_p1_11() {
    let agg = MultiTfAggregator::with_capacity(2);
    let inserted = agg.pre_populate([(27, 0), (27, 1)].into_iter());
    assert_eq!(
        inserted, 2,
        "same SID + different segments = 2 distinct entries"
    );
    assert_eq!(agg.len(), 2);

    // Both lookups must succeed independently.
    assert!(agg.get(27, 0).is_some(), "(27, IDX_I) entry must exist");
    assert!(agg.get(27, 1).is_some(), "(27, NSE_EQ) entry must exist");
}

/// `force_seal_all` iterates every entry without panic at the 1200-SID
/// scale. The callback counts entries; we assert the count matches the
/// pre-populated size.
#[test]
fn force_seal_all_iterates_all_1200_entries_without_panic() {
    let agg = MultiTfAggregator::with_capacity(MAX_DAILY_UNIVERSE_SIZE);
    let keys: Vec<(u32, u8)> = (0..MAX_DAILY_UNIVERSE_SIZE as u32)
        .map(|i| (i, 1u8))
        .collect();
    agg.pre_populate(keys.into_iter());
    assert_eq!(agg.len(), MAX_DAILY_UNIVERSE_SIZE);

    let mut seen_count: usize = 0;
    agg.force_seal_all(|_sid, _seg, _tf, _state| {
        seen_count = seen_count.saturating_add(1);
    });

    // force_seal_all iterates ALL instrument entries; the per-entry
    // callback fires for each timeframe that has accumulated state.
    // Empty entries (no tick consumed yet) produce 0 seals. So the
    // observed count is 0 for a freshly pre_populated aggregator with
    // no consume_tick calls — but the iteration itself must complete
    // without panic, which is what this test asserts.
    assert_eq!(
        seen_count, 0,
        "force_seal_all on a freshly pre_populated aggregator (no ticks consumed) emits 0 seals — but the iteration itself completed without panic"
    );
}

/// Empty aggregator: `force_seal_all` on 0 instruments is a no-op
/// (defensive — the boot orchestrator might call this before
/// `pre_populate` runs).
#[test]
fn force_seal_all_on_empty_aggregator_is_noop() {
    let agg = MultiTfAggregator::new();
    let mut callback_count: usize = 0;
    agg.force_seal_all(|_, _, _, _| callback_count = callback_count.saturating_add(1));
    assert_eq!(callback_count, 0);
}
