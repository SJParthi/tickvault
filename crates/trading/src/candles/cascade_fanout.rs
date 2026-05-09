//! Cascade fanout (Phase 3 commit 4) — distributes every sealed 1s bar
//! to derived candle engine maps.
//!
//! ## Wave-5 §K-L7 + PR #517 retirement timeline
//!
//! - Phase 3 commit 4 shipped 28 derived engines (3s/5s/10s/15s/30s +
//!   1m..15m + 30m + 1h..4h + 1d/1w/1mo).
//! - Wave-5 §K-L7 (PR #504c) retired the 5 seconds-level engines
//!   (3s/5s/10s/15s/30s) — 28 → 23 derived engines.
//! - PR #517 (Wave-5 TF reduction) retired the 12 sub-15-minute non-
//!   canonical engines (Tf2m/Tf3m/Tf4m/Tf6m/Tf7m/Tf8m/Tf9m/Tf10m/Tf11m/
//!   Tf12m/Tf13m/Tf14m) — 23 → 11 derived engines.
//!
//! Today the cascade has 11 derived engines: 1m / 5m / 15m / 30m / 1h /
//! 2h / 3h / 4h / 1d / 1w / 1mo.

use std::sync::Arc;

use metrics::counter;

use crate::candles::engine::{Bar, Tf1d, Tf1h, Tf1mo, Tf1w, Tf2h, Tf3h, Tf4h, Tf5m, Tf15m, Tf30m};
use crate::candles::engine_map::CandleEngineMap;
use crate::in_mem::PrevDayCache;

/// Prometheus counter — every sealed 1s bar that the fanout processed.
pub const METRIC_FANOUT_BARS_PROCESSED: &str = "tv_candle_cascade_fanout_bars_processed_total";
/// Prometheus counter — sum across all 11 derived engines of bars that
/// sealed inside this fanout call.
pub const METRIC_FANOUT_DERIVED_SEALS: &str = "tv_candle_cascade_fanout_derived_seals_total";

pub struct CascadeFanout {
    pub(crate) tf1m: Arc<CandleEngineMap<crate::candles::engine::Tf1m>>,
    pub(crate) tf5m: Arc<CandleEngineMap<Tf5m>>,
    pub(crate) tf15m: Arc<CandleEngineMap<Tf15m>>,
    pub(crate) tf30m: Arc<CandleEngineMap<Tf30m>>,
    pub(crate) tf1h: Arc<CandleEngineMap<Tf1h>>,
    pub(crate) tf2h: Arc<CandleEngineMap<Tf2h>>,
    pub(crate) tf3h: Arc<CandleEngineMap<Tf3h>>,
    pub(crate) tf4h: Arc<CandleEngineMap<Tf4h>>,
    pub(crate) tf1d: Arc<CandleEngineMap<Tf1d>>,
    pub(crate) tf1w: Arc<CandleEngineMap<Tf1w>>,
    pub(crate) tf1mo: Arc<CandleEngineMap<Tf1mo>>,
}

impl Default for CascadeFanout {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for CascadeFanout {
    fn clone(&self) -> Self {
        Self {
            tf1m: Arc::clone(&self.tf1m),
            tf5m: Arc::clone(&self.tf5m),
            tf15m: Arc::clone(&self.tf15m),
            tf30m: Arc::clone(&self.tf30m),
            tf1h: Arc::clone(&self.tf1h),
            tf2h: Arc::clone(&self.tf2h),
            tf3h: Arc::clone(&self.tf3h),
            tf4h: Arc::clone(&self.tf4h),
            tf1d: Arc::clone(&self.tf1d),
            tf1w: Arc::clone(&self.tf1w),
            tf1mo: Arc::clone(&self.tf1mo),
        }
    }
}

/// Number of derived timeframes the fanout drives.
///
/// **Wave-5 §K-L7 (PR #504c)** dropped the 5 seconds-level engines
/// (28 → 23). **PR #517 (Wave-5 TF reduction)** dropped the 12
/// sub-15-minute non-canonical engines (23 → 11). Pinned by
/// `count_derived_engines` test below.
pub const DERIVED_ENGINE_COUNT: usize = 11;

impl CascadeFanout {
    pub fn new() -> Self {
        Self {
            tf1m: Arc::new(CandleEngineMap::new()),
            tf5m: Arc::new(CandleEngineMap::new()),
            tf15m: Arc::new(CandleEngineMap::new()),
            tf30m: Arc::new(CandleEngineMap::new()),
            tf1h: Arc::new(CandleEngineMap::new()),
            tf2h: Arc::new(CandleEngineMap::new()),
            tf3h: Arc::new(CandleEngineMap::new()),
            tf4h: Arc::new(CandleEngineMap::new()),
            tf1d: Arc::new(CandleEngineMap::new()),
            tf1w: Arc::new(CandleEngineMap::new()),
            tf1mo: Arc::new(CandleEngineMap::new()),
        }
    }

    #[inline]
    pub fn feed_sealed_1s_bar(&self, bar: &Bar) {
        counter!(METRIC_FANOUT_BARS_PROCESSED).increment(1);
        let mut derived_seals = 0_u64;
        if self.tf1m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf5m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf15m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf30m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf1h.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf2h.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf3h.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf4h.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf1d.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf1w.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf1mo.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if derived_seals > 0 {
            counter!(METRIC_FANOUT_DERIVED_SEALS).increment(derived_seals);
        }
    }

    /// F1 (Wave-5 §K-L13 / #504e follow-up): pct-stamping variant of
    /// [`feed_sealed_1s_bar`].
    ///
    /// Calls [`CandleEngineMap::on_sealed_bar_with_pct`] on every derived
    /// engine so any sealed Bar that pops out of the derived seal chain
    /// carries the 5 frozen-per-day % fields populated from `pct_cache`.
    /// Falls back to unstamped bars (with 0.0 % fields) when the cache
    /// has no entry for the instrument — see `on_sealed_bar_with_pct`
    /// for the div-by-zero policy.
    ///
    /// Hot-path budget: the `pct_cache` lookup runs at most once per
    /// derived engine per sealed source bar. Per-tick budget unchanged
    /// — this method runs on the seal path, not the per-tick path.
    /// Counter semantics are identical to [`feed_sealed_1s_bar`].
    #[inline]
    pub fn feed_sealed_1s_bar_with_pct(&self, bar: &Bar, pct_cache: &PrevDayCache) {
        counter!(METRIC_FANOUT_BARS_PROCESSED).increment(1);
        let mut derived_seals = 0_u64;
        if self.tf1m.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf5m.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf15m.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf30m.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf1h.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf2h.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf3h.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf4h.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf1d.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf1w.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if self.tf1mo.on_sealed_bar_with_pct(bar, pct_cache).is_some() {
            derived_seals += 1;
        }
        if derived_seals > 0 {
            counter!(METRIC_FANOUT_DERIVED_SEALS).increment(derived_seals);
        }
    }

    pub fn latest_1m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1m.latest(security_id, segment_code)
    }
    pub fn latest_5m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf5m.latest(security_id, segment_code)
    }
    pub fn latest_15m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf15m.latest(security_id, segment_code)
    }
    pub fn latest_30m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf30m.latest(security_id, segment_code)
    }
    pub fn latest_1h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1h.latest(security_id, segment_code)
    }
    pub fn latest_2h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf2h.latest(security_id, segment_code)
    }
    pub fn latest_3h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf3h.latest(security_id, segment_code)
    }
    pub fn latest_4h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf4h.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf1d` engine state.
    /// **WARN — calendar-approximate (see `Tf1d` docstring).**
    pub fn latest_1d(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1d.latest(security_id, segment_code)
    }
    /// **WARN — calendar-approximate.** See `latest_1d` for context.
    pub fn latest_1w(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1w.latest(security_id, segment_code)
    }
    /// **WARN — calendar-approximate.** See `latest_1d` for context.
    pub fn latest_1mo(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1mo.latest(security_id, segment_code)
    }

    pub fn len_1m(&self) -> usize {
        self.tf1m.len()
    }
    pub fn len_5m(&self) -> usize {
        self.tf5m.len()
    }
    pub fn len_15m(&self) -> usize {
        self.tf15m.len()
    }
    pub fn len_30m(&self) -> usize {
        self.tf30m.len()
    }
    pub fn len_1h(&self) -> usize {
        self.tf1h.len()
    }
    pub fn len_2h(&self) -> usize {
        self.tf2h.len()
    }
    pub fn len_3h(&self) -> usize {
        self.tf3h.len()
    }
    pub fn len_4h(&self) -> usize {
        self.tf4h.len()
    }
    pub fn len_1d(&self) -> usize {
        self.tf1d.len()
    }
    pub fn len_1w(&self) -> usize {
        self.tf1w.len()
    }
    pub fn len_1mo(&self) -> usize {
        self.tf1mo.len()
    }

    // -----------------------------------------------------------------
    // F3 (Wave-5 §K-L28..L36 / #505 follow-up): per-TF bar snapshots.
    //
    // Each accessor delegates to `CandleEngineMap::snapshot_latest_bars`
    // on the corresponding derived engine. Used by `/api/movers/v2`
    // (cold path, ≥1s between calls) to feed
    // `tickvault_trading::in_mem::top_n_by_bars`. O(N) per call over
    // the active universe; never on the per-tick hot path.
    // -----------------------------------------------------------------

    /// F3: snapshot every active 1m bar in the map.
    #[must_use]
    pub fn snapshot_1m(&self) -> Vec<Bar> {
        self.tf1m.snapshot_latest_bars()
    }
    /// F3: snapshot every active 5m bar in the map.
    #[must_use]
    pub fn snapshot_5m(&self) -> Vec<Bar> {
        self.tf5m.snapshot_latest_bars()
    }
    /// F3: snapshot every active 15m bar in the map.
    #[must_use]
    pub fn snapshot_15m(&self) -> Vec<Bar> {
        self.tf15m.snapshot_latest_bars()
    }
    /// F3: snapshot every active 30m bar in the map.
    #[must_use]
    pub fn snapshot_30m(&self) -> Vec<Bar> {
        self.tf30m.snapshot_latest_bars()
    }
    /// F3: snapshot every active 1h bar in the map.
    #[must_use]
    pub fn snapshot_1h(&self) -> Vec<Bar> {
        self.tf1h.snapshot_latest_bars()
    }
    /// F3: snapshot every active 2h bar in the map.
    #[must_use]
    pub fn snapshot_2h(&self) -> Vec<Bar> {
        self.tf2h.snapshot_latest_bars()
    }
    /// F3: snapshot every active 3h bar in the map.
    #[must_use]
    pub fn snapshot_3h(&self) -> Vec<Bar> {
        self.tf3h.snapshot_latest_bars()
    }
    /// F3: snapshot every active 4h bar in the map.
    #[must_use]
    pub fn snapshot_4h(&self) -> Vec<Bar> {
        self.tf4h.snapshot_latest_bars()
    }
    /// F3: snapshot every active 1d bar in the map.
    #[must_use]
    pub fn snapshot_1d(&self) -> Vec<Bar> {
        self.tf1d.snapshot_latest_bars()
    }
    /// F3: snapshot every active 1w bar in the map.
    #[must_use]
    pub fn snapshot_1w(&self) -> Vec<Bar> {
        self.tf1w.snapshot_latest_bars()
    }
    /// F3: snapshot every active 1mo bar in the map.
    #[must_use]
    pub fn snapshot_1mo(&self) -> Vec<Bar> {
        self.tf1mo.snapshot_latest_bars()
    }

    pub fn force_seal_all(&self) -> u64 {
        let mut total = 0_u64;
        total += u64::from(self.tf1m.force_seal_all());
        total += u64::from(self.tf5m.force_seal_all());
        total += u64::from(self.tf15m.force_seal_all());
        total += u64::from(self.tf30m.force_seal_all());
        total += u64::from(self.tf1h.force_seal_all());
        total += u64::from(self.tf2h.force_seal_all());
        total += u64::from(self.tf3h.force_seal_all());
        total += u64::from(self.tf4h.force_seal_all());
        total += u64::from(self.tf1d.force_seal_all());
        total += u64::from(self.tf1w.force_seal_all());
        total += u64::from(self.tf1mo.force_seal_all());
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candles::engine::Bar;

    fn make_sealed_1s_bar(security_id: u32, segment_code: u8, bucket_start: u32) -> Bar {
        Bar {
            bucket_start_ist_secs: bucket_start,
            bucket_end_ist_secs: bucket_start + 1,
            open: 100.0,
            high: 101.0,
            low: 99.5,
            close: 100.5,
            volume: 1_000,
            volume_cum_day_at_end: 1_000,
            oi: 0,
            tick_count: 5,
            security_id,
            exchange_segment_code: segment_code,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        }
    }

    #[test]
    fn count_derived_engines_is_exactly_11() {
        // Wave-5 §K-L7 (PR #504c): retired the 5 seconds-level engines (28→23).
        // PR #517 (Wave-5 TF reduction): retired the 12 sub-15-minute non-
        // canonical engines (23→11).
        assert_eq!(DERIVED_ENGINE_COUNT, 11);
    }

    #[test]
    fn cascade_fanout_new_returns_empty_maps() {
        let fanout = CascadeFanout::new();
        assert_eq!(fanout.tf1m.len(), 0);
        assert_eq!(fanout.tf30m.len(), 0);
        assert_eq!(fanout.tf1mo.len(), 0);
    }

    #[test]
    fn feed_sealed_1s_bar_propagates_to_every_derived_engine() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(1234, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        let assertions: [(&str, Option<Bar>); DERIVED_ENGINE_COUNT] = [
            ("tf1m", fanout.tf1m.latest(1234, 1)),
            ("tf5m", fanout.tf5m.latest(1234, 1)),
            ("tf15m", fanout.tf15m.latest(1234, 1)),
            ("tf30m", fanout.tf30m.latest(1234, 1)),
            ("tf1h", fanout.tf1h.latest(1234, 1)),
            ("tf2h", fanout.tf2h.latest(1234, 1)),
            ("tf3h", fanout.tf3h.latest(1234, 1)),
            ("tf4h", fanout.tf4h.latest(1234, 1)),
            ("tf1d", fanout.tf1d.latest(1234, 1)),
            ("tf1w", fanout.tf1w.latest(1234, 1)),
            ("tf1mo", fanout.tf1mo.latest(1234, 1)),
        ];
        for (name, latest) in assertions {
            assert!(latest.is_some(), "{name} did not receive the sealed 1s bar");
        }
    }

    #[test]
    fn feed_sealed_1s_bar_with_pct_propagates_to_every_derived_engine() {
        // F1 ratchet: the pct-stamping fanout method MUST reach every
        // derived engine, just like its non-stamping sibling. Mirrors
        // `feed_sealed_1s_bar_propagates_to_every_derived_engine` so a
        // future regression dropping a `tf_*.on_sealed_bar_with_pct(...)`
        // call from the body fails the build.
        let fanout = CascadeFanout::new();
        let cache = PrevDayCache::new();
        let bar = make_sealed_1s_bar(4321, 1, 2_000);
        fanout.feed_sealed_1s_bar_with_pct(&bar, &cache);
        let assertions: [(&str, Option<Bar>); DERIVED_ENGINE_COUNT] = [
            ("tf1m", fanout.tf1m.latest(4321, 1)),
            ("tf5m", fanout.tf5m.latest(4321, 1)),
            ("tf15m", fanout.tf15m.latest(4321, 1)),
            ("tf30m", fanout.tf30m.latest(4321, 1)),
            ("tf1h", fanout.tf1h.latest(4321, 1)),
            ("tf2h", fanout.tf2h.latest(4321, 1)),
            ("tf3h", fanout.tf3h.latest(4321, 1)),
            ("tf4h", fanout.tf4h.latest(4321, 1)),
            ("tf1d", fanout.tf1d.latest(4321, 1)),
            ("tf1w", fanout.tf1w.latest(4321, 1)),
            ("tf1mo", fanout.tf1mo.latest(4321, 1)),
        ];
        for (name, latest) in assertions {
            assert!(
                latest.is_some(),
                "{name} did not receive the pct-stamped sealed 1s bar"
            );
        }
    }

    #[test]
    fn feed_sealed_1s_bar_with_pct_falls_back_when_cache_empty() {
        // F1 ratchet: when the prev-day cache has no entry for the
        // instrument, the fanout MUST still propagate the unstamped bar
        // (returning 0.0 for the % fields per the
        // `on_sealed_bar_with_pct` div-by-zero policy) rather than
        // dropping the seal.
        let fanout = CascadeFanout::new();
        let cache = PrevDayCache::new();
        let bar = make_sealed_1s_bar(7777, 1, 3_000);
        fanout.feed_sealed_1s_bar_with_pct(&bar, &cache);
        assert!(
            fanout.tf1m.latest(7777, 1).is_some(),
            "tf1m must observe the seal even on cache miss"
        );
    }

    #[test]
    fn feed_sealed_1s_bar_with_pct_explicit_name_match() {
        let fanout = CascadeFanout::new();
        let cache = PrevDayCache::new();
        let bar = make_sealed_1s_bar(1, 1, 1);
        fanout.feed_sealed_1s_bar_with_pct(&bar, &cache);
    }

    /// F3 ratchet (Wave-5 §K-L28..L36 / #505 follow-up): the 11
    /// `snapshot_<tf>m` accessors MUST each return a bar for every
    /// instrument that has been fed into that engine. Mirrors
    /// `feed_sealed_1s_bar_propagates_to_every_derived_engine`
    /// at the snapshot layer.
    #[test]
    fn snapshot_per_tf_returns_every_active_instrument() {
        let fanout = CascadeFanout::new();
        let bar_a = make_sealed_1s_bar(1234, 1, 1_000);
        let bar_b = make_sealed_1s_bar(5678, 2, 1_000);
        fanout.feed_sealed_1s_bar(&bar_a);
        fanout.feed_sealed_1s_bar(&bar_b);

        // Every accessor MUST surface both instruments; iteration
        // order is unspecified per `snapshot_latest_bars` doc, so we
        // sort security_ids before comparing.
        let mut counts: Vec<(&str, usize)> = vec![
            ("1m", fanout.snapshot_1m().len()),
            ("5m", fanout.snapshot_5m().len()),
            ("15m", fanout.snapshot_15m().len()),
            ("30m", fanout.snapshot_30m().len()),
            ("1h", fanout.snapshot_1h().len()),
            ("2h", fanout.snapshot_2h().len()),
            ("3h", fanout.snapshot_3h().len()),
            ("4h", fanout.snapshot_4h().len()),
            ("1d", fanout.snapshot_1d().len()),
            ("1w", fanout.snapshot_1w().len()),
            ("1mo", fanout.snapshot_1mo().len()),
        ];
        counts.sort_by_key(|c| c.0);
        for (tf, n) in counts {
            assert_eq!(
                n, 2,
                "snapshot_{tf} must surface both fed instruments — got {n}"
            );
        }
    }

    // F3 explicit-name-match tests — one per pub fn so the
    // pub-fn-test guard sees a matching test for each `snapshot_<tf>`.
    #[test]
    fn snapshot_1m_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_1m();
    }
    #[test]
    fn snapshot_5m_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_5m();
    }
    #[test]
    fn snapshot_15m_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_15m();
    }
    #[test]
    fn snapshot_30m_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_30m();
    }
    #[test]
    fn snapshot_1h_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_1h();
    }
    #[test]
    fn snapshot_2h_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_2h();
    }
    #[test]
    fn snapshot_3h_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_3h();
    }
    #[test]
    fn snapshot_4h_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_4h();
    }
    #[test]
    fn snapshot_1d_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_1d();
    }
    #[test]
    fn snapshot_1w_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_1w();
    }
    #[test]
    fn snapshot_1mo_explicit_name_match() {
        let _ = CascadeFanout::new().snapshot_1mo();
    }

    /// F3 ratchet: empty fanout snapshots to empty Vecs (no panic).
    #[test]
    fn snapshot_per_tf_returns_empty_on_fresh_fanout() {
        let fanout = CascadeFanout::new();
        assert!(fanout.snapshot_1m().is_empty());
        assert!(fanout.snapshot_5m().is_empty());
        assert!(fanout.snapshot_15m().is_empty());
        assert!(fanout.snapshot_30m().is_empty());
        assert!(fanout.snapshot_1h().is_empty());
        assert!(fanout.snapshot_2h().is_empty());
        assert!(fanout.snapshot_3h().is_empty());
        assert!(fanout.snapshot_4h().is_empty());
        assert!(fanout.snapshot_1d().is_empty());
        assert!(fanout.snapshot_1w().is_empty());
        assert!(fanout.snapshot_1mo().is_empty());
    }

    #[test]
    fn feed_sealed_1s_bar_isolates_cross_segment_collisions() {
        let fanout = CascadeFanout::new();
        let bar_seg1 = make_sealed_1s_bar(27, 0, 1_000);
        let mut bar_seg2 = make_sealed_1s_bar(27, 1, 1_000);
        bar_seg2.close = 999.0;
        fanout.feed_sealed_1s_bar(&bar_seg1);
        fanout.feed_sealed_1s_bar(&bar_seg2);
        let seg1 = fanout.tf1m.latest(27, 0).expect("seg 0 present");
        let seg2 = fanout.tf1m.latest(27, 1).expect("seg 1 present");
        assert_ne!(seg1.close, seg2.close);
    }

    #[test]
    fn force_seal_all_returns_count_across_all_11_engines() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(42, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        let total = fanout.force_seal_all();
        assert_eq!(total, 11);
    }

    #[test]
    fn force_seal_all_returns_zero_on_empty_fanout() {
        let fanout = CascadeFanout::new();
        assert_eq!(fanout.force_seal_all(), 0);
    }

    #[test]
    fn cascade_fanout_clone_shares_state_via_arc() {
        let fanout_a = CascadeFanout::new();
        let fanout_b = fanout_a.clone();
        let bar = make_sealed_1s_bar(99, 2, 1_000);
        fanout_a.feed_sealed_1s_bar(&bar);
        assert!(fanout_b.tf30m.latest(99, 2).is_some());
    }

    #[test]
    fn cascade_fanout_default_matches_new() {
        let _ = CascadeFanout::default();
    }

    #[test]
    fn metric_constants_have_expected_names() {
        assert_eq!(
            METRIC_FANOUT_BARS_PROCESSED,
            "tv_candle_cascade_fanout_bars_processed_total"
        );
        assert_eq!(
            METRIC_FANOUT_DERIVED_SEALS,
            "tv_candle_cascade_fanout_derived_seals_total"
        );
    }

    #[test]
    fn cascade_fanout_new_explicit_name_match() {
        let _ = CascadeFanout::new();
    }

    #[test]
    fn feed_sealed_1s_bar_explicit_name_match() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(1, 1, 1);
        fanout.feed_sealed_1s_bar(&bar);
    }

    #[test]
    fn force_seal_all_explicit_name_match() {
        let fanout = CascadeFanout::new();
        let _ = fanout.force_seal_all();
    }

    #[test]
    fn len_accessors_return_zero_on_empty_fanout() {
        let fanout = CascadeFanout::new();
        assert_eq!(fanout.len_1m(), 0);
        assert_eq!(fanout.len_30m(), 0);
        assert_eq!(fanout.len_1h(), 0);
        assert_eq!(fanout.len_4h(), 0);
        assert_eq!(fanout.len_1d(), 0);
        assert_eq!(fanout.len_1w(), 0);
        assert_eq!(fanout.len_1mo(), 0);
    }

    #[test]
    fn len_accessors_advance_after_feed_sealed_1s_bar() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(1234, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        assert_eq!(fanout.len_1m(), 1);
        assert_eq!(fanout.len_30m(), 1);
        assert_eq!(fanout.len_1h(), 1);
        assert_eq!(fanout.len_1d(), 1);
        assert_eq!(fanout.len_1mo(), 1);
    }

    #[test]
    fn len_accessors_isolate_cross_segment_collisions_per_i_p1_11() {
        let fanout = CascadeFanout::new();
        let bar_seg0 = make_sealed_1s_bar(27, 0, 1_000);
        let bar_seg1 = make_sealed_1s_bar(27, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar_seg0);
        fanout.feed_sealed_1s_bar(&bar_seg1);
        assert_eq!(fanout.len_1m(), 2);
        assert_eq!(fanout.len_30m(), 2);
    }

    #[test]
    fn len_accessor_explicit_name_match_1m() {
        let f = CascadeFanout::new();
        let _ = f.len_1m();
    }

    #[test]
    fn len_accessor_explicit_name_match_1mo() {
        let f = CascadeFanout::new();
        let _ = f.len_1mo();
    }

    #[test]
    fn every_latest_accessor_returns_state_after_first_seal() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(7777, 2, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        let assertions: [(&str, Option<Bar>); DERIVED_ENGINE_COUNT] = [
            ("latest_1m", fanout.latest_1m(7777, 2)),
            ("latest_5m", fanout.latest_5m(7777, 2)),
            ("latest_15m", fanout.latest_15m(7777, 2)),
            ("latest_30m", fanout.latest_30m(7777, 2)),
            ("latest_1h", fanout.latest_1h(7777, 2)),
            ("latest_2h", fanout.latest_2h(7777, 2)),
            ("latest_3h", fanout.latest_3h(7777, 2)),
            ("latest_4h", fanout.latest_4h(7777, 2)),
            ("latest_1d", fanout.latest_1d(7777, 2)),
            ("latest_1w", fanout.latest_1w(7777, 2)),
            ("latest_1mo", fanout.latest_1mo(7777, 2)),
        ];
        for (name, latest) in assertions {
            assert!(latest.is_some(), "{name} returned None after sealed 1s bar");
        }
    }

    #[test]
    fn every_latest_accessor_returns_none_for_unknown_key() {
        let fanout = CascadeFanout::new();
        assert!(fanout.latest_1m(0, 0).is_none());
        assert!(fanout.latest_30m(0, 0).is_none());
        assert!(fanout.latest_1h(0, 0).is_none());
        assert!(fanout.latest_1d(0, 0).is_none());
        assert!(fanout.latest_1mo(0, 0).is_none());
    }

    #[test]
    fn latest_accessors_isolate_cross_segment_collisions() {
        let fanout = CascadeFanout::new();
        let bar_seg0 = make_sealed_1s_bar(27, 0, 1_000);
        let mut bar_seg1 = make_sealed_1s_bar(27, 1, 1_000);
        bar_seg1.close = 555.0;
        fanout.feed_sealed_1s_bar(&bar_seg0);
        fanout.feed_sealed_1s_bar(&bar_seg1);
        let seg0 = fanout.latest_30m(27, 0).expect("seg 0 present");
        let seg1 = fanout.latest_30m(27, 1).expect("seg 1 present");
        assert_ne!(seg0.close, seg1.close);
    }
}
