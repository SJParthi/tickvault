//! Phase 0 Item 10 — gap-fill multi-bar planner (2026-05-13).
//!
//! Pure-function planner that, given a disconnect window and the
//! market-close instant, returns the list of 1-minute bar starts the
//! gap-fill scheduler must request from Dhan REST
//! `/v2/charts/intraday`.
//!
//! Builds on the PR-8 primitives in `tickvault_common::constants`:
//!   * `is_bar_eligible_for_gap_fill` — per-bar eligibility check
//!   * `compute_earliest_legal_fetch_secs` — bar_end + 5s buffer
//!   * `GAP_FILL_ONE_MINUTE_SECS` — bar width constant
//!
//! The actual scheduler task that consumes this plan (HTTP fetch,
//! async retry loop, DEDUP UPSERT writer, `gap_fill_audit` write)
//! lands in a follow-up PR. This module is the pure-logic floor —
//! immune to async/IO flakes, exercised by `proptest`-style enumeration.
//!
//! See `topic-PHASE-0-LEAN-LOCKED.md` §7 for the locked seal-then-fetch
//! invariant table.

use tickvault_common::constants::{GAP_FILL_ONE_MINUTE_SECS, is_bar_eligible_for_gap_fill};

/// Phase 0 Item 10 — given a disconnect window and the trading-day
/// market-close instant (15:30:00 IST in seconds-of-day or epoch-seconds —
/// the caller picks the reference; both inputs MUST share the same
/// reference frame), returns the list of 1m bar starts that the
/// gap-fill scheduler MUST refill, in ascending order.
///
/// Each returned bar start `b` satisfies:
///   * `b >= outage_start_secs` AND `b < outage_end_secs`
///     (bar started during the outage window)
///   * `b + 60 <= market_close_secs`
///     (bar's full 60s window ends at or before market close — the
///     15:29 bar is included because its end is exactly market_close;
///     the 15:30 bar is excluded because its end overshoots)
///   * `b` is aligned to a minute boundary (`b % 60 == 0`)
///
/// Returns the list in ascending bar-start order. Caller pairs each
/// element with `compute_earliest_legal_fetch_secs` to schedule the
/// HTTP fetch.
///
/// Empty list ⇒ no bars to refill (outage too short, outside market
/// hours, or entirely past market close).
///
/// Pure function. Allocates a `Vec` (cold path — runs once per
/// reconnect, never per tick).
///
/// # Boundary semantics (matches plan §7 table)
///
/// | Disconnect window | Bars returned |
/// |---|---|
/// | 09:33:03 → 09:34:00 | `[09:33]` (only the bar that was in flight) |
/// | 09:33:03 → 09:36:00 | `[09:33, 09:34, 09:35]` |
/// | 15:29:00 → 15:31:00 | `[15:29]` (15:30 excluded — market closed mid-bar) |
/// | 09:14:00 → 09:14:30 | `[]` (no full minute in outage) |
///
/// Tested by `tests` module below.
#[must_use]
pub fn plan_gap_fill_bars(
    outage_start_secs: i64,
    outage_end_secs: i64,
    market_close_secs: i64,
) -> Vec<i64> {
    // O(1) EXEMPT: cold path — runs at most once per reconnect cycle
    // (~5 bars typical), never per tick.
    if outage_start_secs >= outage_end_secs {
        return Vec::new();
    }
    let bar_width = GAP_FILL_ONE_MINUTE_SECS as i64;
    if bar_width <= 0 {
        // Defensive: bar_width is a compile-time const > 0, but a
        // future regression that flips the sign should not panic
        // (saturating_div on the loop counter would silently spin).
        return Vec::new();
    }
    // Round outage_start UP to the next minute boundary. A bar that
    // starts STRICTLY BEFORE outage_start was already in flight when
    // the disconnect happened — live ticks captured part of it, so
    // gap-fill must NOT overwrite it. This mirrors the
    // `bar_start_secs >= outage_start_secs` clause inside
    // `is_bar_eligible_for_gap_fill`.
    let remainder = outage_start_secs.rem_euclid(bar_width);
    let first_bar = if remainder == 0 {
        outage_start_secs
    } else {
        outage_start_secs.saturating_add(bar_width - remainder)
    };
    // Cap at ~24h worth of bars to bound the loop in case a buggy
    // caller passes nonsensical inputs (e.g. outage_end_secs in the
    // far future). 24h = 1440 bars; one full trading session is 375
    // bars (09:15–15:30). Anything beyond a single trading day's
    // worth means the operator's app was offline overnight and
    // gap-fill is the wrong tool — historical_fetch handles it.
    const MAX_BARS_PER_PLAN: usize = 1440;
    let mut bars = Vec::with_capacity(MAX_BARS_PER_PLAN.min(64));
    let mut bar = first_bar;
    while bar < outage_end_secs && bars.len() < MAX_BARS_PER_PLAN {
        if is_bar_eligible_for_gap_fill(bar, outage_start_secs, outage_end_secs, market_close_secs)
        {
            bars.push(bar);
        }
        bar = bar.saturating_add(bar_width);
        // Saturating-add can plateau at i64::MAX; break the loop in
        // that case to avoid an infinite spin.
        if bar == i64::MAX {
            break;
        }
    }
    bars
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan §7 row 1: disconnect at 09:33:03, reconnect at 09:34:00.
    /// Only one full bar (09:33) is missing.
    #[test]
    fn test_plan_gap_fill_bars_single_minute_outage() {
        // 09:33:00 IST = 9*3600 + 33*60 = 34_380 secs-of-day.
        // Disconnect at 09:33:03 = 34_383; reconnect at 09:34:00 = 34_440.
        // Market close = 15:30:00 = 55_800.
        let bars = plan_gap_fill_bars(34_383, 34_440, 55_800);
        // 09:33 bar started BEFORE outage_start (at 34_380 < 34_383) →
        // NOT eligible (live ticks already captured part of it).
        // 09:34 bar starts AT outage_end (not before) → not eligible.
        assert_eq!(bars, Vec::<i64>::new());
    }

    /// 3-minute outage entirely within market hours.
    #[test]
    fn test_plan_gap_fill_bars_three_minute_outage() {
        // Outage 09:33:00 → 09:36:00 (aligned boundaries).
        // Eligible bars: 09:33 (34_380), 09:34 (34_440), 09:35 (34_500).
        let bars = plan_gap_fill_bars(34_380, 34_560, 55_800);
        assert_eq!(bars, vec![34_380, 34_440, 34_500]);
    }

    /// Plan §7 row 3: outage spans market close. Only 15:29 bar refilled.
    #[test]
    fn test_plan_gap_fill_bars_excludes_market_close_bar() {
        // Outage 15:29:00 → 15:31:00.
        // 15:29 bar (start 55_740): bar_end = 55_800 = market_close → eligible.
        // 15:30 bar (start 55_800): bar_end = 55_860 > market_close → excluded.
        let bars = plan_gap_fill_bars(55_740, 55_860, 55_800);
        assert_eq!(bars, vec![55_740]);
    }

    /// Outage entirely after market close → no bars.
    #[test]
    fn test_plan_gap_fill_bars_entirely_after_close() {
        let bars = plan_gap_fill_bars(56_000, 56_120, 55_800);
        assert_eq!(bars, Vec::<i64>::new());
    }

    /// Outage entirely before market open → bars returned (caller
    /// gates with a market-hours check before calling us, but the
    /// planner stays pure). Our contract is "bars that started during
    /// the outage window AND whose end is at/before close" — we don't
    /// know market_open, only market_close.
    #[test]
    fn test_plan_gap_fill_bars_before_market_open_returns_eligible_bars() {
        // Outage 08:00:00 → 08:03:00 (way before market open).
        // bar_end of each is well before market close (15:30) → eligible.
        let bars = plan_gap_fill_bars(28_800, 28_980, 55_800);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars, vec![28_800, 28_860, 28_920]);
    }

    /// Sub-minute outage → no full bar inside.
    #[test]
    fn test_plan_gap_fill_bars_sub_minute_outage() {
        // Outage 09:33:10 → 09:33:40 (30 seconds).
        // No full minute bar starts within this window.
        let bars = plan_gap_fill_bars(34_390, 34_420, 55_800);
        assert_eq!(bars, Vec::<i64>::new());
    }

    /// Inverted window (outage_start >= outage_end) → no bars.
    #[test]
    fn test_plan_gap_fill_bars_inverted_window_returns_empty() {
        let bars = plan_gap_fill_bars(34_440, 34_380, 55_800);
        assert_eq!(bars, Vec::<i64>::new());
        // Equal start/end — degenerate but valid input.
        let bars = plan_gap_fill_bars(34_380, 34_380, 55_800);
        assert_eq!(bars, Vec::<i64>::new());
    }

    /// Bar list is always monotonically increasing by 60s.
    #[test]
    fn test_plan_gap_fill_bars_monotonic_increasing_60s_steps() {
        let bars = plan_gap_fill_bars(34_380, 35_580, 55_800); // 20-min outage
        assert_eq!(bars.len(), 20);
        for pair in bars.windows(2) {
            assert_eq!(pair[1] - pair[0], 60);
        }
        // First bar = outage_start, last bar = outage_end - 60.
        assert_eq!(bars[0], 34_380);
        assert_eq!(*bars.last().expect("at least one bar"), 35_520);
    }

    /// Misaligned outage_start (mid-minute) rounds UP to next boundary.
    #[test]
    fn test_plan_gap_fill_bars_misaligned_start_rounds_up() {
        // Outage 09:33:15 → 09:35:30. First eligible bar = 09:34
        // (09:33 started before outage_start; live ticks captured it).
        let bars = plan_gap_fill_bars(34_395, 34_530, 55_800);
        assert_eq!(bars, vec![34_440, 34_500]);
    }

    /// Pathological i64::MAX inputs must not panic + must return empty.
    #[test]
    fn test_plan_gap_fill_bars_extreme_inputs_dont_panic() {
        // outage_start at i64::MAX → no bars (round-up overflows to MAX).
        let bars = plan_gap_fill_bars(i64::MAX, i64::MAX, i64::MAX);
        assert_eq!(bars, Vec::<i64>::new());
    }

    /// Bar count bounded — even on a huge outage we don't allocate
    /// unbounded memory.
    #[test]
    fn test_plan_gap_fill_bars_caps_at_24h_worth() {
        // Outage spanning 48 hours.
        let bars = plan_gap_fill_bars(0, 48 * 3600, i64::MAX);
        // Hard cap = MAX_BARS_PER_PLAN = 1440 (24h worth).
        assert!(bars.len() <= 1440);
    }
}
