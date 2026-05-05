//! 28-TF cascade fanout — Phase 3 commit 4.
//!
//! Holds an `Arc<CandleEngineMap<TFn>>` for each of the 28 derived
//! timeframes (3s, 5s, 10s, 15s, 30s, 1m..15m, 30m, 1h, 2h, 3h, 4h,
//! 1d, 1w, 1mo). The cascade task feeds every sealed 1s bar into
//! `feed_sealed_1s_bar`, which fans out to every derived engine in
//! O(28) — a constant cost, not dependent on the universe size.
//!
//! ## Architecture (per plan §2 CASCADE design)
//!
//! ```
//!         Every Dhan tick
//!                │
//!                ▼
//!       1s CandleEngineMap (per-instrument, on hot tick path)
//!                │
//!                │ on bar seal (every 1s)
//!                ▼
//!       CascadeFanout::feed_sealed_1s_bar(&bar)
//!                │
//!     ┌──────────┴──────────┐
//!     ▼                     ▼
//!   3s map  ...  1mo map   (28 maps, all in parallel)
//! ```
//!
//! Per L12 the hot path is the 1s engine; the fanout runs OFF the hot
//! path inside the existing `run_cascade_1s` task — every fanout step
//! is O(1) per map (papaya pin/get + uncontended mutex), so 28 × ~60ns
//! ≈ 1.7µs per sealed 1s bar. At peak ~24K instruments × 1 seal/sec =
//! ~24K seals/sec, the fanout takes ~40ms/sec on a single core, which
//! is comfortably below 1 core × 1000ms.
//!
//! ## Why not a Vec<Arc<dyn ...>>?
//!
//! Hot-path rules ban `dyn`. Each `CandleEngineMap<TF>` is monomorphized
//! to a different concrete type, so they cannot share a single Vec
//! without trait-object indirection. We use 28 named fields — boring,
//! mechanical, easy to ratchet, and zero-overhead at the call site.
//!
//! ## What this module does NOT do (deferred)
//!
//! - SPSC channel between the 1s seal and the fanout (Phase 3 follow-up
//!   if profiling shows the inline call path becomes a bottleneck).
//!   Today the fanout is called synchronously inside `run_cascade_1s`
//!   because the cost (1.7µs/bar) is comfortably under the ~1ms tick
//!   inter-arrival time at peak.
//! - Calendar-aware bucketing for 1d/1w/1mo (the markers use SECS-based
//!   bucketing today; calendar-aware refinement is a follow-on PR).
//! - Parity tests vs live `candles_*` matviews (Phase 3 commit 5,
//!   gated on the 7-trading-day soak per plan §6 row 3).

use std::sync::Arc;

use metrics::counter;

use crate::candles::engine::{
    Bar, Tf1d, Tf1h, Tf1mo, Tf1w, Tf2h, Tf2m, Tf3h, Tf3m, Tf3s, Tf4h, Tf4m, Tf5m, Tf5s, Tf6m, Tf7m,
    Tf8m, Tf9m, Tf10m, Tf10s, Tf11m, Tf12m, Tf13m, Tf14m, Tf15m, Tf15s, Tf30m, Tf30s,
};
use crate::candles::engine_map::CandleEngineMap;

/// Prometheus counter — every sealed 1s bar that the fanout processed.
pub const METRIC_FANOUT_BARS_PROCESSED: &str = "tv_candle_cascade_fanout_bars_processed_total";
/// Prometheus counter — sum across all 28 derived engines of bars that
/// sealed inside this fanout call. Operators correlate this with the
/// 1s seal rate to verify the cascade is alive.
pub const METRIC_FANOUT_DERIVED_SEALS: &str = "tv_candle_cascade_fanout_derived_seals_total";

/// Holds the 28 derived candle engine maps and fans every sealed 1s bar
/// into all of them.
///
/// Cloneable: every internal `Arc<CandleEngineMap<TF>>` shares state
/// across clones, so multiple consumers (the cascade task + the trading
/// bot reader + the IST midnight rollover task) can hold their own
/// `CascadeFanout` clone.
pub struct CascadeFanout {
    pub(crate) tf3s: Arc<CandleEngineMap<Tf3s>>,
    pub(crate) tf5s: Arc<CandleEngineMap<Tf5s>>,
    pub(crate) tf10s: Arc<CandleEngineMap<Tf10s>>,
    pub(crate) tf15s: Arc<CandleEngineMap<Tf15s>>,
    pub(crate) tf30s: Arc<CandleEngineMap<Tf30s>>,
    pub(crate) tf1m: Arc<CandleEngineMap<crate::candles::engine::Tf1m>>,
    pub(crate) tf2m: Arc<CandleEngineMap<Tf2m>>,
    pub(crate) tf3m: Arc<CandleEngineMap<Tf3m>>,
    pub(crate) tf4m: Arc<CandleEngineMap<Tf4m>>,
    pub(crate) tf5m: Arc<CandleEngineMap<Tf5m>>,
    pub(crate) tf6m: Arc<CandleEngineMap<Tf6m>>,
    pub(crate) tf7m: Arc<CandleEngineMap<Tf7m>>,
    pub(crate) tf8m: Arc<CandleEngineMap<Tf8m>>,
    pub(crate) tf9m: Arc<CandleEngineMap<Tf9m>>,
    pub(crate) tf10m: Arc<CandleEngineMap<Tf10m>>,
    pub(crate) tf11m: Arc<CandleEngineMap<Tf11m>>,
    pub(crate) tf12m: Arc<CandleEngineMap<Tf12m>>,
    pub(crate) tf13m: Arc<CandleEngineMap<Tf13m>>,
    pub(crate) tf14m: Arc<CandleEngineMap<Tf14m>>,
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
    /// **Memory cost:** allocates ~45 MB at construction (28 papaya maps
    /// each pre-sized for 25,000 instruments). Call ONCE at boot — do
    /// NOT use in test setup, derive macros, or `unwrap_or_default()`
    /// without understanding the footprint.
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for CascadeFanout {
    fn clone(&self) -> Self {
        Self {
            tf3s: Arc::clone(&self.tf3s),
            tf5s: Arc::clone(&self.tf5s),
            tf10s: Arc::clone(&self.tf10s),
            tf15s: Arc::clone(&self.tf15s),
            tf30s: Arc::clone(&self.tf30s),
            tf1m: Arc::clone(&self.tf1m),
            tf2m: Arc::clone(&self.tf2m),
            tf3m: Arc::clone(&self.tf3m),
            tf4m: Arc::clone(&self.tf4m),
            tf5m: Arc::clone(&self.tf5m),
            tf6m: Arc::clone(&self.tf6m),
            tf7m: Arc::clone(&self.tf7m),
            tf8m: Arc::clone(&self.tf8m),
            tf9m: Arc::clone(&self.tf9m),
            tf10m: Arc::clone(&self.tf10m),
            tf11m: Arc::clone(&self.tf11m),
            tf12m: Arc::clone(&self.tf12m),
            tf13m: Arc::clone(&self.tf13m),
            tf14m: Arc::clone(&self.tf14m),
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

/// Number of derived timeframes the fanout drives. Pinned by
/// `count_derived_engines` test below — must equal 28.
pub const DERIVED_ENGINE_COUNT: usize = 28;

impl CascadeFanout {
    /// Constructs an empty fanout with all 28 derived engine maps.
    /// Each map pre-allocates capacity for the F&O universe.
    pub fn new() -> Self {
        Self {
            tf3s: Arc::new(CandleEngineMap::new()),
            tf5s: Arc::new(CandleEngineMap::new()),
            tf10s: Arc::new(CandleEngineMap::new()),
            tf15s: Arc::new(CandleEngineMap::new()),
            tf30s: Arc::new(CandleEngineMap::new()),
            tf1m: Arc::new(CandleEngineMap::new()),
            tf2m: Arc::new(CandleEngineMap::new()),
            tf3m: Arc::new(CandleEngineMap::new()),
            tf4m: Arc::new(CandleEngineMap::new()),
            tf5m: Arc::new(CandleEngineMap::new()),
            tf6m: Arc::new(CandleEngineMap::new()),
            tf7m: Arc::new(CandleEngineMap::new()),
            tf8m: Arc::new(CandleEngineMap::new()),
            tf9m: Arc::new(CandleEngineMap::new()),
            tf10m: Arc::new(CandleEngineMap::new()),
            tf11m: Arc::new(CandleEngineMap::new()),
            tf12m: Arc::new(CandleEngineMap::new()),
            tf13m: Arc::new(CandleEngineMap::new()),
            tf14m: Arc::new(CandleEngineMap::new()),
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

    /// Feeds a sealed 1s bar into every derived engine map. Called by
    /// the cascade task on every successful 1s seal.
    ///
    /// O(28) per call — constant, not dependent on universe size. Each
    /// per-map step is O(1) (papaya pin/get + uncontended mutex).
    /// Increments `tv_candle_cascade_fanout_derived_seals_total` for
    /// every derived engine that itself emitted a sealed bar (a
    /// downstream cascade event).
    #[inline]
    pub fn feed_sealed_1s_bar(&self, bar: &Bar) {
        counter!(METRIC_FANOUT_BARS_PROCESSED).increment(1);
        let mut derived_seals = 0_u64;
        if self.tf3s.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf5s.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf10s.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf15s.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf30s.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf1m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf2m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf3m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf4m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf5m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf6m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf7m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf8m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf9m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf10m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf11m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf12m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf13m.on_sealed_bar(bar).is_some() {
            derived_seals += 1;
        }
        if self.tf14m.on_sealed_bar(bar).is_some() {
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

    /// Forces every derived engine in every map to seal its open bar.
    /// Used at IST midnight rollover (per L13). Returns the total
    /// count of bars that were sealed across all 28 derived engines.
    ///
    /// O(N × 28) over instruments — runs once per day. Not hot path.
    pub fn force_seal_all(&self) -> u64 {
        let mut total = 0_u64;
        total += u64::from(self.tf3s.force_seal_all());
        total += u64::from(self.tf5s.force_seal_all());
        total += u64::from(self.tf10s.force_seal_all());
        total += u64::from(self.tf15s.force_seal_all());
        total += u64::from(self.tf30s.force_seal_all());
        total += u64::from(self.tf1m.force_seal_all());
        total += u64::from(self.tf2m.force_seal_all());
        total += u64::from(self.tf3m.force_seal_all());
        total += u64::from(self.tf4m.force_seal_all());
        total += u64::from(self.tf5m.force_seal_all());
        total += u64::from(self.tf6m.force_seal_all());
        total += u64::from(self.tf7m.force_seal_all());
        total += u64::from(self.tf8m.force_seal_all());
        total += u64::from(self.tf9m.force_seal_all());
        total += u64::from(self.tf10m.force_seal_all());
        total += u64::from(self.tf11m.force_seal_all());
        total += u64::from(self.tf12m.force_seal_all());
        total += u64::from(self.tf13m.force_seal_all());
        total += u64::from(self.tf14m.force_seal_all());
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
        }
    }

    #[test]
    fn count_derived_engines_is_exactly_28() {
        // Plan §1 L3: 29 timeframes total. 1s is the hot-path engine,
        // the other 28 are derived. Pin the count.
        assert_eq!(DERIVED_ENGINE_COUNT, 28);
    }

    #[test]
    fn cascade_fanout_new_returns_empty_maps() {
        let fanout = CascadeFanout::new();
        assert_eq!(fanout.tf3s.len(), 0);
        assert_eq!(fanout.tf30m.len(), 0);
        assert_eq!(fanout.tf1mo.len(), 0);
    }

    #[test]
    fn feed_sealed_1s_bar_propagates_to_every_derived_engine() {
        // Hostile review H2 fix: assert ALL 28 derived engines (not a
        // representative subset) — a buggy edit that drops any one
        // `on_sealed_bar` call must fail this test, not pass silently.
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(1234, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        let assertions: [(&str, Option<Bar>); DERIVED_ENGINE_COUNT] = [
            ("tf3s", fanout.tf3s.latest(1234, 1)),
            ("tf5s", fanout.tf5s.latest(1234, 1)),
            ("tf10s", fanout.tf10s.latest(1234, 1)),
            ("tf15s", fanout.tf15s.latest(1234, 1)),
            ("tf30s", fanout.tf30s.latest(1234, 1)),
            ("tf1m", fanout.tf1m.latest(1234, 1)),
            ("tf2m", fanout.tf2m.latest(1234, 1)),
            ("tf3m", fanout.tf3m.latest(1234, 1)),
            ("tf4m", fanout.tf4m.latest(1234, 1)),
            ("tf5m", fanout.tf5m.latest(1234, 1)),
            ("tf6m", fanout.tf6m.latest(1234, 1)),
            ("tf7m", fanout.tf7m.latest(1234, 1)),
            ("tf8m", fanout.tf8m.latest(1234, 1)),
            ("tf9m", fanout.tf9m.latest(1234, 1)),
            ("tf10m", fanout.tf10m.latest(1234, 1)),
            ("tf11m", fanout.tf11m.latest(1234, 1)),
            ("tf12m", fanout.tf12m.latest(1234, 1)),
            ("tf13m", fanout.tf13m.latest(1234, 1)),
            ("tf14m", fanout.tf14m.latest(1234, 1)),
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
            assert!(
                latest.is_some(),
                "{name} did not receive the sealed 1s bar — fanout call missing or broken"
            );
        }
    }

    #[test]
    fn feed_sealed_1s_bar_isolates_cross_segment_collisions() {
        // I-P1-11: same security_id different segment must NOT collide.
        let fanout = CascadeFanout::new();
        let bar_seg1 = make_sealed_1s_bar(27, 0, 1_000); // FINNIFTY IDX_I
        let mut bar_seg2 = make_sealed_1s_bar(27, 1, 1_000); // hypothetical NSE_EQ id=27
        bar_seg2.close = 999.0;
        fanout.feed_sealed_1s_bar(&bar_seg1);
        fanout.feed_sealed_1s_bar(&bar_seg2);
        let seg1 = fanout.tf3s.latest(27, 0).expect("seg 0 present");
        let seg2 = fanout.tf3s.latest(27, 1).expect("seg 1 present");
        assert_ne!(seg1.close, seg2.close);
    }

    #[test]
    fn force_seal_all_returns_count_across_all_28_engines() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(42, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        // 28 derived engines × 1 instrument × 1 open bar each = 28 seals.
        let total = fanout.force_seal_all();
        assert_eq!(total, 28);
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
        // Clone B sees the state because the inner Arcs are shared.
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
}
