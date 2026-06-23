//! GOLDEN safety gate for the ONE-common-candle-engine convergence (SP3a).
//!
//! Proves that routing Groww ticks through the SAME 21-TF
//! [`tickvault_trading::candles::MultiTfAggregator`] as Dhan — parameterized by
//! the per-feed [`tickvault_trading::candles::FeedStrategy::GROWW`]
//! (Discard late-policy) + an `i64`-widened cumulative-volume override — produces
//! a 1-minute candle BYTE-IDENTICAL to the legacy `Groww1mAggregator`, AND that
//! the other 20 timeframes are now ALSO produced for Groww (they were not before).
//!
//! ## Why the legacy values are LITERALS, not a live call
//!
//! SP3e DELETES `core::feed::groww::aggregator_1m::Groww1mAggregator`. A golden
//! test that called the deleted aggregator would not compile after the delete.
//! Instead we pin the EXACT legacy 1-minute outputs as literals — these are the
//! same values asserted by the legacy aggregator's own (now-deleted) unit tests
//! and by `tickvault_common::candle_fold`'s golden tests
//! (`test_golden_groww_minute_boundary_seal_and_baseline_carry`):
//!   minute-1: open=100, high=110, close=110, volume=50 (last_cum 150 − baseline 100).
//! If the new path yields ANY different 1-minute value, this test FAILS — exactly
//! the "do not bless a silent behaviour change" gate the SP3 plan requires.
//!
//! ## Apples-to-apples timestamp alignment
//!
//! The legacy Groww cell floors an IST-nanos timestamp to its absolute minute.
//! The 21-TF engine anchors buckets at 09:15 IST market-open (`TfIndex::bucket_start`).
//! We therefore drive the new path with second-granular timestamps that ARE
//! minute boundaries inside the regular session, so both engines bucket the same
//! ticks into the same minute and the M1 candle is directly comparable.

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::{
    ConsumeStats, FeedStrategy, LiveCandleState, MultiTfAggregator, TfIndex,
};

/// Builds a Groww-shaped `ParsedTick`: the bridge converts a Groww `Groww1mTick`
/// (i64 cum_volume, f64 ltp, IST-nanos) into a `ParsedTick` (u32 secs, f32 ltp)
/// — Groww has NO day_open / day_close / oi, so those are 0 (the cell falls back
/// to LTP for the day-open and leaves the pct/oi columns at 0, "missing data").
fn groww_tick(ist_secs: u32, ltp: f64) -> ParsedTick {
    let mut t = ParsedTick::default();
    t.security_id = 1_333; // Groww exchange_token, fits u32 (max observed 1_175_236)
    t.exchange_segment_code = 1; // NSE_EQ
    t.exchange_timestamp = ist_secs;
    // The bridge maps Groww's f64 ltp → f32 for ParsedTick; volume comes from the
    // i64 override (NOT this u32 field) so it is left 0 to prove no truncation path.
    t.last_traded_price = ltp as f32;
    t.volume = 0;
    t
}

/// A minute-aligned IST second INSIDE the regular session (09:15–15:30 IST) so
/// the 21-TF market-open-anchored bucket grid lines up minute-for-minute. Base
/// chosen as a real M1-aligned in-session instant.
const M1_BASE: u32 = 1_779_354_960; // in-session, divisible by 60

#[test]
fn test_golden_groww_1m_through_multi_tf_matches_legacy() {
    // Drive the NEW path: Groww feed via the 21-TF engine + GROWW strategy
    // (Discard) + i64 cumulative-volume override (the Groww cumulative).
    let agg = MultiTfAggregator::new();
    agg.pre_populate(std::iter::once((1_333_u32, 1_u8)));
    // Groww bridge seeds the first-tick cumulative as the baseline (legacy parity).
    agg.seed_cumulative(1_333, 1, 100);

    // Minute 1 — baseline cumulative 100; fold to last cum 150 (legacy scenario).
    // tick A (minute 1 open): ltp 100, cum 100
    agg.consume_tick(
        &groww_tick(M1_BASE, 100.0),
        1,
        FeedStrategy::GROWW,
        Some(100),
        |_, _| {},
    );
    // tick B (same minute): ltp 110 (new high), cum 150
    agg.consume_tick(
        &groww_tick(M1_BASE + 30, 110.0),
        1,
        FeedStrategy::GROWW,
        Some(150),
        |_, _| {},
    );

    // Cross into minute 2 — seals minute 1. Capture the sealed M1 candle.
    let mut sealed_m1: Option<LiveCandleState> = None;
    agg.consume_tick(
        &groww_tick(M1_BASE + 60, 111.0),
        1,
        FeedStrategy::GROWW,
        Some(170),
        |tf, state| {
            if tf == TfIndex::M1 {
                sealed_m1 = Some(state);
            }
        },
    );

    let m1 = sealed_m1.expect("minute-1 M1 candle must seal on the boundary cross");

    // GOLDEN: byte-identical to the legacy Groww1mAggregator minute-1 candle.
    assert_eq!(m1.open, 100.0, "open MUST match legacy Groww");
    assert_eq!(m1.high, 110.0, "high MUST match legacy Groww");
    assert_eq!(m1.low, 100.0, "low MUST match legacy Groww");
    assert_eq!(m1.close, 110.0, "close MUST match legacy Groww");
    // Volume = last_cum(150) − baseline(100) = 50 — the i64 cumulative flowed
    // through the shared cell without truncation.
    assert_eq!(
        m1.volume, 50,
        "per-minute volume MUST match legacy Groww (150-100)"
    );
    assert_eq!(
        m1.tick_count, 2,
        "two in-minute ticks before the boundary cross"
    );
    // Seal timestamp is the minute-1 bucket start (the candle's designated ts).
    assert_eq!(
        m1.bucket_start_ist_secs, M1_BASE,
        "M1 seal ts = minute-1 bucket start"
    );
}

#[test]
fn test_groww_through_multi_tf_produces_all_21_tfs() {
    // BEFORE convergence, Groww produced ONLY the 1-minute candle. Through the
    // shared 21-TF engine, a boundary cross that seals EVERY timeframe (a full
    // day-spanning jump) must emit a seal for all 21 TFs — Groww now generates
    // the full timeframe set, not just 1m.
    let agg = MultiTfAggregator::new();
    agg.pre_populate(std::iter::once((1_333_u32, 1_u8)));

    // Open every TF's first bucket of the day at market open.
    agg.consume_tick(
        &groww_tick(M1_BASE, 100.0),
        1,
        FeedStrategy::GROWW,
        Some(100),
        |_, _| {},
    );

    // Jump > 1 day ahead (next session, well past the widest TF's bucket) so
    // EVERY one of the 21 TFs crosses its boundary and seals its open bucket.
    let next_day = M1_BASE + 86_400; // same M1-aligned offset, next day → in-session
    let mut sealed_tfs: std::collections::HashSet<TfIndex> = std::collections::HashSet::new();
    let stats: ConsumeStats = agg.consume_tick(
        &groww_tick(next_day, 105.0),
        1,
        FeedStrategy::GROWW,
        Some(120),
        |tf, _| {
            sealed_tfs.insert(tf);
        },
    );

    assert!(stats.instrument_found);
    assert_eq!(
        sealed_tfs.len(),
        21,
        "Groww through the shared engine must seal ALL 21 timeframes (got {}); \
         before convergence only 1m was produced",
        sealed_tfs.len()
    );
    // Spot-check a few TFs beyond M1 are present.
    assert!(sealed_tfs.contains(&TfIndex::M1));
    assert!(sealed_tfs.contains(&TfIndex::M5));
    assert!(sealed_tfs.contains(&TfIndex::H1));
    assert!(sealed_tfs.contains(&TfIndex::D1));
}

#[test]
fn test_groww_cumulative_volume_above_u32_max_not_truncated() {
    // CRITICAL guard (adversarial review): Groww cum_volume is i64 and exceeds
    // u32::MAX intraday for liquid stocks. Routed via the override it must NOT be
    // truncated (the bug that would happen if it were funnelled through the u32
    // ParsedTick.volume field).
    let agg = MultiTfAggregator::new();
    agg.pre_populate(std::iter::once((1_333_u32, 1_u8)));

    let baseline: u64 = 5_000_000_000; // > u32::MAX (4_294_967_295)
    let last: u64 = 5_000_000_500; // +500 within the SAME minute (minute 1)
    agg.seed_cumulative(1_333, 1, baseline);
    // Minute-1 open tick (cum = baseline).
    agg.consume_tick(
        &groww_tick(M1_BASE, 100.0),
        1,
        FeedStrategy::GROWW,
        Some(baseline),
        |_, _| {},
    );
    // A second tick INSIDE minute 1 advancing the cumulative by 500.
    agg.consume_tick(
        &groww_tick(M1_BASE + 30, 101.0),
        1,
        FeedStrategy::GROWW,
        Some(last),
        |_, _| {},
    );
    // Cross into minute 2 → seals minute 1 (volume = last − baseline = 500).
    let mut sealed_m1: Option<LiveCandleState> = None;
    agg.consume_tick(
        &groww_tick(M1_BASE + 60, 102.0),
        1,
        FeedStrategy::GROWW,
        Some(last + 10),
        |tf, st| {
            if tf == TfIndex::M1 {
                sealed_m1 = Some(st);
            }
        },
    );
    let m1 = sealed_m1.expect("seal");
    assert_eq!(
        m1.volume, 500,
        "i64 cumulative (> u32::MAX) must flow through the shared cell without truncation"
    );
}

#[test]
fn test_groww_discard_late_policy_never_amends() {
    // Groww's FeedStrategy::GROWW (Discard) must NEVER amend a sealed bucket
    // (the legacy Groww1mAggregator dropped out-of-order ticks). A 1-bucket-late
    // tick reports as late, NOT amended — proving the per-feed strategy preserves
    // Groww's exact behaviour (a Dhan Refold would have amended).
    let agg = MultiTfAggregator::new();
    agg.pre_populate(std::iter::once((1_333_u32, 1_u8)));

    agg.consume_tick(
        &groww_tick(M1_BASE, 100.0),
        1,
        FeedStrategy::GROWW,
        Some(100),
        |_, _| {},
    );
    // Boundary cross → seal minute 1.
    agg.consume_tick(
        &groww_tick(M1_BASE + 60, 105.0),
        1,
        FeedStrategy::GROWW,
        Some(150),
        |_, _| {},
    );
    // A 1-bucket-late tick (LTT inside the just-sealed minute) — under Discard it
    // is dropped, never amended.
    let mut amend_emits = 0u32;
    let stats = agg.consume_tick(
        &groww_tick(M1_BASE + 30, 999.0),
        1,
        FeedStrategy::GROWW,
        Some(160),
        |_, _| amend_emits += 1,
    );
    assert_eq!(
        stats.amended_count, 0,
        "Discard policy MUST NOT amend a sealed candle"
    );
    assert!(
        stats.late_count >= 1,
        "the late tick must be counted as discarded-late"
    );
    assert_eq!(amend_emits, 0, "no on_seal re-emit under Discard");
}

#[test]
fn test_dhan_refold_policy_still_amends_one_bucket_late() {
    // The companion guard: the SAME engine under FeedStrategy::DHAN (Refold) DOES
    // amend a 1-bucket-late tick — proving the two feeds' behaviours diverge by
    // VALUE (the strategy), not by forked engine code.
    let agg = MultiTfAggregator::new();
    agg.pre_populate(std::iter::once((1_333_u32, 1_u8)));

    agg.consume_tick(
        &groww_tick(M1_BASE, 100.0),
        1,
        FeedStrategy::DHAN,
        None,
        |_, _| {},
    );
    agg.consume_tick(
        &groww_tick(M1_BASE + 60, 105.0),
        1,
        FeedStrategy::DHAN,
        None,
        |_, _| {},
    );
    let stats = agg.consume_tick(
        &groww_tick(M1_BASE + 30, 110.0),
        1,
        FeedStrategy::DHAN,
        None,
        |_, _| {},
    );
    assert!(
        stats.amended_count >= 1,
        "Dhan Refold MUST amend the just-sealed minute (got {})",
        stats.amended_count
    );
}
