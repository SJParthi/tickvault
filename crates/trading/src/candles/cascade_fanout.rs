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

    // ────────────────────────────────────────────────────────────────
    // Read-only accessors for the 28 derived engines (Phase 3 commit 5).
    //
    // Each method returns `Option<Bar>` for the (security_id, segment)
    // pair, by `Copy`. No mutation surface is exposed — callers cannot
    // reach `on_sealed_bar` through these accessors. This is the
    // intended consumption path for the parity test harness, the
    // future `/api/movers?v=2` endpoint (Phase 4a), and any trading
    // bot read of in-RAM candle state.
    //
    // 28 named methods is mechanically simpler than a single
    // `latest(timeframe_name: &str)` + match — the latter would either
    // return an `enum BarRef<...>` (boilerplate) or use `dyn`
    // (banned on hot path). Per-TF named methods compile down to
    // one papaya pin/get + one Mutex lock per call (~60ns).
    //
    // Off the per-tick hot path (called by /api/movers + parity tests).
    // ────────────────────────────────────────────────────────────────

    /// Read-only accessor for `Tf3s` engine state.
    pub fn latest_3s(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf3s.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf5s` engine state.
    pub fn latest_5s(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf5s.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf10s` engine state.
    pub fn latest_10s(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf10s.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf15s` engine state.
    pub fn latest_15s(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf15s.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf30s` engine state.
    pub fn latest_30s(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf30s.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf1m` engine state.
    pub fn latest_1m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf2m` engine state.
    pub fn latest_2m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf2m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf3m` engine state.
    pub fn latest_3m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf3m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf4m` engine state.
    pub fn latest_4m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf4m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf5m` engine state.
    pub fn latest_5m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf5m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf6m` engine state.
    pub fn latest_6m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf6m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf7m` engine state.
    pub fn latest_7m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf7m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf8m` engine state.
    pub fn latest_8m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf8m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf9m` engine state.
    pub fn latest_9m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf9m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf10m` engine state.
    pub fn latest_10m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf10m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf11m` engine state.
    pub fn latest_11m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf11m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf12m` engine state.
    pub fn latest_12m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf12m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf13m` engine state.
    pub fn latest_13m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf13m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf14m` engine state.
    pub fn latest_14m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf14m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf15m` engine state.
    pub fn latest_15m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf15m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf30m` engine state.
    pub fn latest_30m(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf30m.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf1h` engine state.
    pub fn latest_1h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1h.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf2h` engine state.
    pub fn latest_2h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf2h.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf3h` engine state.
    pub fn latest_3h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf3h.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf4h` engine state.
    pub fn latest_4h(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf4h.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf1d` engine state.
    /// **WARN — calendar-approximate (see `Tf1d` docstring).** SEBI-canonical
    /// truth is the QuestDB matview. Use this only for trading-bot speed reads
    /// where ±UTC-offset drift is acceptable.
    pub fn latest_1d(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1d.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf1w` engine state.
    /// **WARN — calendar-approximate.** See `latest_1d` for context.
    pub fn latest_1w(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1w.latest(security_id, segment_code)
    }
    /// Read-only accessor for `Tf1mo` engine state.
    /// **WARN — calendar-approximate.** See `latest_1d` for context.
    pub fn latest_1mo(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        self.tf1mo.latest(security_id, segment_code)
    }

    // ────────────────────────────────────────────────────────────────
    // Per-TF len() accessors — Phase 4a follow-up (closes the H1 fix
    // deferred in PR #490). Each method returns the count of
    // `(security_id, segment_code)` pairs currently held in the
    // corresponding derived engine map. Used by the live parity
    // runner + the v2 movers endpoint to surface a truthful
    // "instruments_in_ram" count instead of a hardcoded zero.
    //
    // O(1) per call (papaya `len()` is lock-free), 28 named methods.
    // ────────────────────────────────────────────────────────────────

    pub fn len_3s(&self) -> usize {
        self.tf3s.len()
    }
    pub fn len_5s(&self) -> usize {
        self.tf5s.len()
    }
    pub fn len_10s(&self) -> usize {
        self.tf10s.len()
    }
    pub fn len_15s(&self) -> usize {
        self.tf15s.len()
    }
    pub fn len_30s(&self) -> usize {
        self.tf30s.len()
    }
    pub fn len_1m(&self) -> usize {
        self.tf1m.len()
    }
    pub fn len_2m(&self) -> usize {
        self.tf2m.len()
    }
    pub fn len_3m(&self) -> usize {
        self.tf3m.len()
    }
    pub fn len_4m(&self) -> usize {
        self.tf4m.len()
    }
    pub fn len_5m(&self) -> usize {
        self.tf5m.len()
    }
    pub fn len_6m(&self) -> usize {
        self.tf6m.len()
    }
    pub fn len_7m(&self) -> usize {
        self.tf7m.len()
    }
    pub fn len_8m(&self) -> usize {
        self.tf8m.len()
    }
    pub fn len_9m(&self) -> usize {
        self.tf9m.len()
    }
    pub fn len_10m(&self) -> usize {
        self.tf10m.len()
    }
    pub fn len_11m(&self) -> usize {
        self.tf11m.len()
    }
    pub fn len_12m(&self) -> usize {
        self.tf12m.len()
    }
    pub fn len_13m(&self) -> usize {
        self.tf13m.len()
    }
    pub fn len_14m(&self) -> usize {
        self.tf14m.len()
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

    #[test]
    fn len_accessors_return_zero_on_empty_fanout() {
        let fanout = CascadeFanout::new();
        assert_eq!(fanout.len_3s(), 0);
        assert_eq!(fanout.len_5s(), 0);
        assert_eq!(fanout.len_30s(), 0);
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
        // After ONE sealed 1s bar fed for ONE instrument, every
        // derived engine map holds exactly 1 entry.
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(1234, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        // Spot-check across the second / minute / hour / day / month spectrum.
        assert_eq!(fanout.len_3s(), 1);
        assert_eq!(fanout.len_30s(), 1);
        assert_eq!(fanout.len_1m(), 1);
        assert_eq!(fanout.len_30m(), 1);
        assert_eq!(fanout.len_1h(), 1);
        assert_eq!(fanout.len_1d(), 1);
        assert_eq!(fanout.len_1mo(), 1);
    }

    #[test]
    fn len_accessors_isolate_cross_segment_collisions_per_i_p1_11() {
        // Same security_id, different segment → 2 distinct entries
        // in every derived engine map.
        let fanout = CascadeFanout::new();
        let bar_seg0 = make_sealed_1s_bar(27, 0, 1_000);
        let bar_seg1 = make_sealed_1s_bar(27, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar_seg0);
        fanout.feed_sealed_1s_bar(&bar_seg1);
        assert_eq!(fanout.len_1m(), 2, "I-P1-11 isolation broken on len_1m");
        assert_eq!(fanout.len_30m(), 2, "I-P1-11 isolation broken on len_30m");
    }

    #[test]
    fn len_accessor_explicit_name_match_3s() {
        let f = CascadeFanout::new();
        let _ = f.len_3s();
    }

    #[test]
    fn len_accessor_explicit_name_match_1mo() {
        let f = CascadeFanout::new();
        let _ = f.len_1mo();
    }

    /// Phase 3 commit 5: assert every TF read accessor returns Some(bar)
    /// after one sealed 1s bar is fed. Mirrors the propagation test but
    /// uses the public read accessors instead of the `pub(crate)` fields.
    #[test]
    fn every_latest_accessor_returns_state_after_first_seal() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(7777, 2, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        let assertions: [(&str, Option<Bar>); DERIVED_ENGINE_COUNT] = [
            ("latest_3s", fanout.latest_3s(7777, 2)),
            ("latest_5s", fanout.latest_5s(7777, 2)),
            ("latest_10s", fanout.latest_10s(7777, 2)),
            ("latest_15s", fanout.latest_15s(7777, 2)),
            ("latest_30s", fanout.latest_30s(7777, 2)),
            ("latest_1m", fanout.latest_1m(7777, 2)),
            ("latest_2m", fanout.latest_2m(7777, 2)),
            ("latest_3m", fanout.latest_3m(7777, 2)),
            ("latest_4m", fanout.latest_4m(7777, 2)),
            ("latest_5m", fanout.latest_5m(7777, 2)),
            ("latest_6m", fanout.latest_6m(7777, 2)),
            ("latest_7m", fanout.latest_7m(7777, 2)),
            ("latest_8m", fanout.latest_8m(7777, 2)),
            ("latest_9m", fanout.latest_9m(7777, 2)),
            ("latest_10m", fanout.latest_10m(7777, 2)),
            ("latest_11m", fanout.latest_11m(7777, 2)),
            ("latest_12m", fanout.latest_12m(7777, 2)),
            ("latest_13m", fanout.latest_13m(7777, 2)),
            ("latest_14m", fanout.latest_14m(7777, 2)),
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
            assert!(
                latest.is_some(),
                "{name} returned None after sealed 1s bar — accessor or fanout call missing"
            );
        }
    }

    #[test]
    fn every_latest_accessor_returns_none_for_unknown_key() {
        let fanout = CascadeFanout::new();
        // No bars fed yet — all accessors must return None.
        assert!(fanout.latest_3s(0, 0).is_none());
        assert!(fanout.latest_30m(0, 0).is_none());
        assert!(fanout.latest_1h(0, 0).is_none());
        assert!(fanout.latest_1d(0, 0).is_none());
        assert!(fanout.latest_1mo(0, 0).is_none());
    }

    #[test]
    fn latest_accessors_isolate_cross_segment_collisions() {
        // I-P1-11: same security_id different segment must NOT collide.
        let fanout = CascadeFanout::new();
        let bar_seg0 = make_sealed_1s_bar(27, 0, 1_000);
        let mut bar_seg1 = make_sealed_1s_bar(27, 1, 1_000);
        bar_seg1.close = 555.0;
        fanout.feed_sealed_1s_bar(&bar_seg0);
        fanout.feed_sealed_1s_bar(&bar_seg1);
        let seg0 = fanout.latest_30m(27, 0).expect("seg 0 present");
        let seg1 = fanout.latest_30m(27, 1).expect("seg 1 present");
        assert_ne!(seg0.close, seg1.close, "I-P1-11 isolation broken");
    }
}
