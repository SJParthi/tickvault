//! Sub-PR #1.6 of 2026-05-27 daily-universe expansion — in-RAM
//! cache of live percentage-change values per instrument.
//!
//! Operator-locked design 2026-05-27:
//! - Source of truth for `prev_day_close` is the F2 [`PrevDayCache`]
//!   (boot-loaded from QuestDB `previous_close`), NOT per-tick packet
//!   bytes 38-41. This makes pct_close work during pre-market (08:45+
//!   IST) before any market-open tick has stamped a fresh prev_close
//!   field in the packet stream.
//! - Source of truth for `day_high`, `day_low`, `day_open` is the
//!   Quote-mode packet itself (bytes 34-37 / 42-45 / 46-49 — already
//!   parsed by `crates/core/src/parser/quote.rs` into [`ParsedTick`]).
//!   No app-side `DayOhlcTracker`-style tracking needed for non-IDX_I
//!   SIDs — the exchange already computes + stamps these in every
//!   Quote tick.
//! - 4 IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21)
//!   continue to rely on [`DayOhlcTracker`] auto-arm as fallback if
//!   Dhan returns zero-valued OHLC fields for index segment.
//!
//! See `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//! sections §3 (data source rules) + §28 (operator boundary —
//! `crates/trading/src/in_mem/` is OUTSIDE the indicators/strategies
//! boundary; pct_change is derived data, NOT an indicator).
//!
//! ## Concurrency
//!
//! `papaya::HashMap` for O(1) lock-free reads on the tick hot path.
//! `update_from_tick` is the WRITE site (per-tick); `lookup` is the
//! READ site (API / Telegram / operator query). Both are O(1).
//!
//! ## Memory footprint
//!
//! [`PctChange`] is 56 bytes (6 × f64 + 1 × i64). At 250 SIDs the
//! cache holds ~14 KB of value bytes plus papaya overhead — negligible
//! against the t4g.large 8 GiB budget per `aws-budget.md` rule 6.
//!
//! ## Division-by-zero handling
//!
//! Per `.claude/rules/project/data-integrity.md` (no silent NaN
//! propagation): `pct_open` / `pct_close` return [`Option::None`] when
//! the denominator is 0.0 or non-finite. Callers must treat `None` as
//! "not yet computable" (e.g., pre-market with no day_open, or newly-
//! listed stock with no prev_close).

use std::sync::Arc;

use papaya::HashMap;
use tickvault_common::price_precision::f32_to_f64_clean;
use tickvault_common::tick_types::ParsedTick;

/// Composite key per I-P1-11 — `(security_id, exchange_segment_code)`.
type PctChangeKey = (u64, u8);

/// Live percentage-change snapshot for one instrument.
///
/// Cloneable (`Copy`) — the cache stores by-value, `lookup` returns a
/// snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PctChange {
    /// Last traded price (f64 widened from ParsedTick.ltp f32).
    pub ltp: f64,
    /// Today's session open (Quote packet bytes 34-37).
    pub day_open: f64,
    /// Today's session high (Quote packet bytes 42-45).
    pub day_high: f64,
    /// Today's session low (Quote packet bytes 46-49).
    pub day_low: f64,
    /// Yesterday's close (F2 PrevDayCache, NOT per-tick parsed).
    pub prev_day_close: f64,
    /// `(ltp - day_open) / day_open × 100` — `None` if day_open == 0.0
    /// or non-finite.
    pub pct_open: Option<f64>,
    /// `(ltp - prev_day_close) / prev_day_close × 100` — `None` if
    /// prev_day_close == 0.0 or non-finite.
    pub pct_close: Option<f64>,
    /// Tick exchange_timestamp (IST epoch seconds × 1e9 nanos per
    /// `data-integrity.md` WebSocket timestamp rule — NO +5:30 offset).
    pub ts: i64,
}

/// In-RAM cache of [`PctChange`] keyed by `(security_id, exchange_segment_code)`
/// per I-P1-11.
///
/// Cloneable: `Arc::clone` on the inner papaya map.
#[derive(Clone, Default)]
pub struct PctChangeCache {
    inner: Arc<HashMap<PctChangeKey, PctChange>>,
}

impl PctChangeCache {
    /// Build an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the cache from a parsed tick + the boot-loaded
    /// `prev_day_close`.
    ///
    /// Hot path — called from `tick_processor::on_tick` (once per tick).
    /// Cost: ~30 ns papaya pin + 2 division ops + insert.
    ///
    /// `prev_day_close` is sourced from the F2 [`PrevDayCache`] at the
    /// caller site (typically read once and threaded through, or
    /// looked up per-tick with another `papaya::pin`).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Caller looks up prev_day_close from PrevDayCache, then:
    /// let prev_close = prev_day_cache.lookup(tick.security_id, tick.exchange_segment_code)
    ///     .map(|refs| refs.prev_day_close)
    ///     .unwrap_or(0.0);
    /// pct_change_cache.update_from_tick(&tick, prev_close);
    /// ```
    pub fn update_from_tick(&self, tick: &ParsedTick, prev_day_close: f64) {
        // Per data-integrity.md "Price Precision Preservation" rule:
        // ALWAYS use f32_to_f64_clean() instead of f64::from() to
        // avoid IEEE 754 widening artifacts (e.g. 10.20_f32 -> 10.2
        // instead of 10.19999980926514). Banned-pattern scanner
        // category 3 enforces this.
        let ltp = f32_to_f64_clean(tick.last_traded_price);
        let day_open = f32_to_f64_clean(tick.day_open);
        let day_high = f32_to_f64_clean(tick.day_high);
        let day_low = f32_to_f64_clean(tick.day_low);

        let pct_open = compute_pct(ltp, day_open);
        let pct_close = compute_pct(ltp, prev_day_close);

        let entry = PctChange {
            ltp,
            day_open,
            day_high,
            day_low,
            prev_day_close,
            pct_open,
            pct_close,
            ts: i64::from(tick.exchange_timestamp),
        };

        let key = (tick.security_id, tick.exchange_segment_code);
        let pin = self.inner.pin();
        pin.insert(key, entry);
    }

    /// O(1) lock-free lookup. Returns `None` if no tick has been
    /// observed yet for this instrument.
    ///
    /// Cold/warm path — called by API / Telegram / operator queries,
    /// not per-tick.
    #[must_use]
    pub fn lookup(&self, security_id: u64, exchange_segment_code: u8) -> Option<PctChange> {
        let pin = self.inner.pin();
        pin.get(&(security_id, exchange_segment_code)).copied()
    }

    /// Number of `(security_id, exchange_segment_code)` entries
    /// currently cached.
    /// Cold-path read.
    #[must_use]
    pub fn len_instruments(&self) -> usize {
        let pin = self.inner.pin();
        pin.len()
    }

    /// Estimated value-bytes for the `tv_subsystem_memory_*` gauge
    /// family. Reports `len() × size_of::<PctChange>()`, matching the
    /// lazy-RSS reconciliation contract.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        let len = self.len_instruments() as u64;
        let per_entry = std::mem::size_of::<PctChange>() as u64;
        len.saturating_mul(per_entry)
    }
}

/// Pure helper — `(numerator - denominator) / denominator × 100`.
///
/// Returns `None` if denominator is `0.0` or non-finite (NaN / Inf).
/// Also returns `None` if numerator is non-finite (defensive — should
/// not occur on a validated ParsedTick).
#[inline]
#[must_use]
fn compute_pct(numerator: f64, denominator: f64) -> Option<f64> {
    if !denominator.is_finite() || denominator == 0.0 || !numerator.is_finite() {
        return None;
    }
    Some((numerator - denominator) / denominator * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(
        security_id: u64,
        segment_code: u8,
        ltp: f32,
        day_open: f32,
        day_high: f32,
        day_low: f32,
        ts: u32,
    ) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment_code,
            last_traded_price: ltp,
            day_open,
            day_high,
            day_low,
            exchange_timestamp: ts,
            ..ParsedTick::default()
        }
    }

    /// pct math is the standard NSE display formula:
    /// `(ltp - prev_close) / prev_close × 100`
    #[test]
    fn test_pct_change_positive_when_ltp_above_prev_close() {
        let cache = PctChangeCache::new();
        let tick = make_tick(2885, 1, 2500.0, 2450.0, 2510.0, 2440.0, 1_700_000_000);
        cache.update_from_tick(&tick, 2400.0);
        let entry = cache.lookup(2885, 1).expect("cached after update");
        // pct_close = (2500 - 2400) / 2400 * 100 = 4.1666...
        let pct_close = entry.pct_close.expect("non-zero prev_close yields Some");
        assert!((pct_close - 4.166_666_666_666_667).abs() < 1e-9);
        // pct_open = (2500 - 2450) / 2450 * 100 = 2.040816...
        let pct_open = entry.pct_open.expect("non-zero day_open yields Some");
        assert!((pct_open - 2.040_816_326_530_612).abs() < 1e-9);
    }

    /// LTP below prev_close → negative pct. Standard short-side.
    #[test]
    fn test_pct_change_negative_when_ltp_below_prev_close() {
        let cache = PctChangeCache::new();
        let tick = make_tick(13, 0, 24400.0, 24500.0, 24550.0, 24380.0, 1_700_000_000);
        cache.update_from_tick(&tick, 24600.0);
        let entry = cache.lookup(13, 0).expect("cached");
        // pct_close = (24400 - 24600) / 24600 * 100 ≈ -0.8130...
        let pct_close = entry.pct_close.expect("non-zero prev_close yields Some");
        assert!(pct_close < 0.0);
        assert!((pct_close - (-0.813_008_130_081_3)).abs() < 1e-9);
    }

    /// Division-by-zero on prev_close → None, not NaN.
    /// Operator scenario: newly-listed stock has no prior session.
    #[test]
    fn test_pct_close_is_none_when_prev_close_is_zero() {
        let cache = PctChangeCache::new();
        let tick = make_tick(7777, 1, 100.0, 95.0, 102.0, 94.0, 1_700_000_000);
        cache.update_from_tick(&tick, 0.0);
        let entry = cache.lookup(7777, 1).expect("cached");
        assert!(
            entry.pct_close.is_none(),
            "pct_close must be None when prev_close == 0.0"
        );
        // pct_open still computable
        assert!(entry.pct_open.is_some());
    }

    /// Division-by-zero on day_open → None.
    /// Pre-market scenario: no trade has set today's open yet.
    #[test]
    fn test_pct_open_is_none_when_day_open_is_zero() {
        let cache = PctChangeCache::new();
        let tick = make_tick(2885, 1, 2500.0, 0.0, 0.0, 0.0, 1_700_000_000);
        cache.update_from_tick(&tick, 2400.0);
        let entry = cache.lookup(2885, 1).expect("cached");
        assert!(
            entry.pct_open.is_none(),
            "pct_open must be None when day_open == 0.0"
        );
        // pct_close still computable from prev_close
        assert!(entry.pct_close.is_some());
    }

    /// I-P1-11 composite-uniqueness — two SIDs with the same numeric
    /// value across different segments MUST live as TWO distinct
    /// entries in the cache (the FINNIFTY-27 IDX_I vs synthetic
    /// NSE_EQ-27 case documented in security-id-uniqueness.md).
    #[test]
    fn test_composite_key_distinguishes_segments_per_i_p1_11() {
        let cache = PctChangeCache::new();
        let tick_idx = make_tick(27, 0, 24000.0, 24100.0, 24200.0, 23950.0, 1_700_000_000);
        let tick_eq = make_tick(27, 1, 500.0, 505.0, 510.0, 498.0, 1_700_000_000);
        cache.update_from_tick(&tick_idx, 24050.0);
        cache.update_from_tick(&tick_eq, 502.0);
        let idx = cache.lookup(27, 0).expect("IDX_I entry");
        let eq = cache.lookup(27, 1).expect("NSE_EQ entry");
        assert!((idx.ltp - 24000.0).abs() < 0.01);
        assert!((eq.ltp - 500.0).abs() < 0.01);
        assert_eq!(cache.len_instruments(), 2, "two distinct composite keys");
    }

    /// Cache update is idempotent on the composite key — subsequent
    /// ticks REPLACE rather than accumulate.
    #[test]
    fn test_subsequent_tick_replaces_cached_entry() {
        let cache = PctChangeCache::new();
        let tick1 = make_tick(2885, 1, 2500.0, 2450.0, 2510.0, 2440.0, 1_700_000_000);
        cache.update_from_tick(&tick1, 2400.0);
        let tick2 = make_tick(2885, 1, 2520.0, 2450.0, 2520.0, 2440.0, 1_700_000_001);
        cache.update_from_tick(&tick2, 2400.0);
        let entry = cache.lookup(2885, 1).expect("cached");
        assert!((entry.ltp - 2520.0).abs() < 0.01);
        assert_eq!(entry.ts, 1_700_000_001);
        assert_eq!(cache.len_instruments(), 1);
    }

    /// NaN inputs must not propagate as NaN — return None.
    /// Defensive against upstream parser regression.
    #[test]
    fn test_nan_inputs_yield_none_not_nan_propagation() {
        // compute_pct is pure; test it directly.
        assert!(compute_pct(f64::NAN, 100.0).is_none());
        assert!(compute_pct(100.0, f64::NAN).is_none());
        assert!(compute_pct(f64::INFINITY, 100.0).is_none());
        assert!(compute_pct(100.0, f64::INFINITY).is_none());
        assert!(compute_pct(100.0, 0.0).is_none());
        // Valid case still works
        let v = compute_pct(110.0, 100.0).expect("valid");
        assert!((v - 10.0).abs() < 1e-9);
    }

    /// Empty cache lookup returns None — no panic, no default zero.
    #[test]
    fn test_lookup_on_empty_cache_returns_none() {
        let cache = PctChangeCache::new();
        assert_eq!(cache.lookup(2885, 1), None);
        assert_eq!(cache.len_instruments(), 0);
        assert_eq!(cache.estimated_bytes(), 0);
    }

    /// Memory budget — 250 SIDs × size_of::<PctChange>() must fit
    /// well under the t4g.large 8 GiB host budget allowance for the
    /// `in_mem` component family.
    #[test]
    fn test_estimated_bytes_scales_linearly_with_entries() {
        let cache = PctChangeCache::new();
        for i in 0..250 {
            let tick = make_tick(1000 + i, 1, 100.0, 99.0, 101.0, 98.5, 1_700_000_000);
            cache.update_from_tick(&tick, 99.5);
        }
        assert_eq!(cache.len_instruments(), 250);
        let bytes = cache.estimated_bytes();
        let per_entry = std::mem::size_of::<PctChange>() as u64;
        assert_eq!(bytes, 250 * per_entry);
        // Sanity check: 250 entries should be well under 100 KB total
        // (struct is 56 bytes, total ~14 KB).
        assert!(
            bytes < 100_000,
            "250 SIDs × {} B = {} B should be < 100 KB",
            per_entry,
            bytes
        );
    }
}
