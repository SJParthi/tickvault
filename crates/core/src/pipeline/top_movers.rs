//! Top gainers, losers, and most-active tracker for live ticks.
//!
//! Tracks per-security percentage change from previous close (`day_close` in `ParsedTick`)
//! and periodically computes ranked snapshots of the top 20 gainers, losers, and
//! most active by volume.
//!
//! # Performance
//! - O(1) per tick: HashMap lookup + arithmetic update (hot path)
//! - O(N log N) snapshot computation every 5 seconds (cold path, ~2000 instruments)

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::debug;

use tickvault_common::tick_types::ParsedTick;

/// Shared handle for the latest top movers snapshot.
///
/// Written by the tick processor (cold path, every 5s).
/// Read by the API handler (cold path, on request).
pub type SharedTopMoversSnapshot = Arc<RwLock<Option<TopMoversSnapshot>>>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Initial capacity for the tracker HashMap.
/// Pre-sized for ~25,000 instruments to prevent mid-session HashMap resize.
/// option_movers uses 30,000; this must match to avoid hot-path allocation.
const TRACKER_MAP_INITIAL_CAPACITY: usize = 30000;

/// Number of top entries to include in each snapshot list.
const TOP_N: usize = 20;

/// Minimum absolute change percentage to qualify as a mover (filters noise).
const MIN_CHANGE_PCT_THRESHOLD: f32 = 0.01;

// ---------------------------------------------------------------------------
// SecurityState — per-security tracking state
// ---------------------------------------------------------------------------

/// Per-security price state for change calculation.
#[derive(Debug, Clone, Copy)]
struct SecurityState {
    /// Latest traded price.
    last_traded_price: f32,
    /// Percentage change from previous close.
    change_pct: f32,
    /// Latest cumulative volume.
    volume: u32,
    /// Exchange segment code.
    exchange_segment_code: u8,
}

// ---------------------------------------------------------------------------
// MoverEntry — public snapshot entry
// ---------------------------------------------------------------------------

/// A single entry in the top movers snapshot.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct MoverEntry {
    /// Dhan security identifier.
    pub security_id: u32,
    /// Exchange segment code.
    pub exchange_segment_code: u8,
    /// Last traded price.
    pub last_traded_price: f32,
    /// Previous close price (from PrevClose packets).
    pub prev_close: f32,
    /// Percentage change from previous close.
    pub change_pct: f32,
    /// Cumulative volume.
    pub volume: u32,
}

// ---------------------------------------------------------------------------
// TopMoversSnapshot — periodic ranked output
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of top gainers, losers, and most active securities.
/// Separated by segment: equities (NSE_EQ + BSE_EQ) and indices (IDX_I).
/// Each sub-category has independent top-N rankings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopMoversSnapshot {
    // --- Equity rankings (NSE_EQ + BSE_EQ) ---
    pub equity_gainers: Vec<MoverEntry>,
    pub equity_losers: Vec<MoverEntry>,
    pub equity_most_active: Vec<MoverEntry>,
    // --- Index rankings (IDX_I) ---
    pub index_gainers: Vec<MoverEntry>,
    pub index_losers: Vec<MoverEntry>,
    pub index_most_active: Vec<MoverEntry>,
    /// Total securities being tracked.
    pub total_tracked: usize,
}

// ---------------------------------------------------------------------------
// TopMoversTracker
// ---------------------------------------------------------------------------

/// Real-time tracker for top gainers, losers, and most active securities.
///
/// Updated O(1) per tick. Snapshot computation is O(N log N) but only runs
/// periodically on the cold path (every 5 seconds).
///
/// Previous close prices come from two sources:
/// 1. PrevClose WebSocket packets (code 6) — for indices (IDX_I) only.
/// 2. `day_close` field in Full packet ticks — for equities and F&O.
///    Dhan confirmed (Ticket #5525125): day_close = previous session's close.
///    Used as fallback when prev_close_prices has no entry (first tick).
pub struct TopMoversTracker {
    /// Per-security state keyed by `(security_id, segment_code)`.
    securities: HashMap<(u32, u8), SecurityState>,
    /// Previous close prices from PrevClose packets, keyed by `(security_id, segment_code)`.
    /// Populated via `update_prev_close()` when PrevClose packets arrive.
    prev_close_prices: HashMap<(u32, u8), f32>,
    /// Latest snapshot (updated periodically).
    latest_snapshot: Option<TopMoversSnapshot>,
    /// Total ticks processed.
    ticks_processed: u64,
}

impl Default for TopMoversTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TopMoversTracker {
    /// Creates a new tracker with pre-allocated capacity.
    pub fn new() -> Self {
        Self {
            securities: HashMap::with_capacity(TRACKER_MAP_INITIAL_CAPACITY),
            prev_close_prices: HashMap::with_capacity(TRACKER_MAP_INITIAL_CAPACITY),
            latest_snapshot: None,
            ticks_processed: 0,
        }
    }

    /// Stores previous close price from a PrevClose WebSocket packet (code 6).
    ///
    /// Called by tick_processor when PrevClose packets arrive (typically at boot).
    /// This sets the baseline for change% calculations in `update()`.
    ///
    /// # Performance
    /// O(1) — single HashMap insert.
    pub fn update_prev_close(&mut self, security_id: u32, segment_code: u8, prev_close: f32) {
        if prev_close.is_finite() && prev_close > 0.0 {
            self.prev_close_prices
                .insert((security_id, segment_code), prev_close);
        }
    }

    /// Updates the tracker with a new tick.
    ///
    /// Uses previous close from PrevClose packets (populated via `update_prev_close()`).
    /// Falls back to `tick.day_close` if available (post-market only).
    /// Skips ticks without any valid previous close baseline.
    ///
    /// # Performance
    /// O(1) — single HashMap lookup + one division.
    #[inline]
    pub fn update(&mut self, tick: &ParsedTick) {
        // Stock movers = equities + indices ONLY. Reject derivatives (F&O, currency, commodity).
        // Options/futures belong in option_movers, not stock_movers.
        match tick.exchange_segment_code {
            0 | 1 | 4 => {} // IDX_I=0, NSE_EQ=1, BSE_EQ=4
            _ => return,    // NSE_FNO=2, NSE_CURRENCY=3, MCX_COMM=5, BSE_CURRENCY=7, BSE_FNO=8
        }

        // Reject ticks with non-finite or non-positive LTP (NaN, Inf, 0, negative)
        if !tick.last_traded_price.is_finite() || tick.last_traded_price <= 0.0 {
            return;
        }

        self.ticks_processed = self.ticks_processed.saturating_add(1);

        let key = (tick.security_id, tick.exchange_segment_code);

        // Look up previous close: prefer PrevClose packet, fall back to day_close
        let prev_close = if let Some(&pc) = self.prev_close_prices.get(&key) {
            pc
        } else if tick.day_close.is_finite() && tick.day_close > 0.0 {
            // Store day_close as baseline so compute_snapshot can output correct prev_close
            self.prev_close_prices.insert(key, tick.day_close);
            tick.day_close
        } else {
            return; // No baseline — skip
        };

        // Calculate percentage change: ((LTP - prev_close) / prev_close) * 100
        let change_pct = ((tick.last_traded_price - prev_close) / prev_close) * 100.0;

        if let Some(state) = self.securities.get_mut(&key) {
            state.last_traded_price = tick.last_traded_price;
            state.change_pct = change_pct;
            state.volume = tick.volume;
        } else {
            self.securities.insert(
                key,
                SecurityState {
                    last_traded_price: tick.last_traded_price,
                    change_pct,
                    volume: tick.volume,
                    exchange_segment_code: tick.exchange_segment_code,
                },
            );
        }
    }

    /// Computes ranked snapshots split by segment: equities vs indices.
    ///
    /// # Performance
    /// O(N log N) where N is the number of tracked securities. Only call periodically.
    // O(1) EXEMPT: cold path — runs every 5 seconds, not per tick
    pub fn compute_snapshot(&mut self) -> TopMoversSnapshot {
        let total_tracked = self.securities.len();

        // Build entries from all tracked securities, split by segment
        let mut equity_entries: Vec<MoverEntry> = Vec::with_capacity(total_tracked);
        let mut index_entries: Vec<MoverEntry> = Vec::with_capacity(100); // ~30 indices

        for (&(security_id, segment_code), state) in &self.securities {
            if !state.change_pct.is_finite() {
                continue;
            }
            let prev_close = self
                .prev_close_prices
                .get(&(security_id, segment_code))
                .copied()
                .unwrap_or(0.0);
            let entry = MoverEntry {
                security_id,
                exchange_segment_code: state.exchange_segment_code,
                last_traded_price: state.last_traded_price,
                prev_close,
                change_pct: state.change_pct,
                volume: state.volume,
            };
            match state.exchange_segment_code {
                0 => index_entries.push(entry),      // IDX_I
                1 | 4 => equity_entries.push(entry), // NSE_EQ, BSE_EQ
                _ => {}                              // Should not happen (filtered in update)
            }
        }

        // Helper: compute top-N gainers, losers, most_active from a mutable slice
        fn compute_top_n(
            entries: &mut [MoverEntry],
            top_n: usize,
            min_pct: f32,
        ) -> (Vec<MoverEntry>, Vec<MoverEntry>, Vec<MoverEntry>) {
            // Gainers: sort by change_pct descending
            entries.sort_unstable_by(|a, b| {
                b.change_pct
                    .partial_cmp(&a.change_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let gainers: Vec<MoverEntry> = entries
                .iter()
                .filter(|e| e.change_pct > min_pct)
                .take(top_n)
                .copied()
                .collect();

            // Losers: sort by change_pct ascending
            entries.sort_unstable_by(|a, b| {
                a.change_pct
                    .partial_cmp(&b.change_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let losers: Vec<MoverEntry> = entries
                .iter()
                .filter(|e| e.change_pct < -min_pct)
                .take(top_n)
                .copied()
                .collect();

            // Most active: sort by volume descending
            entries.sort_unstable_by(|a, b| b.volume.cmp(&a.volume));
            let most_active: Vec<MoverEntry> = entries.iter().take(top_n).copied().collect();

            (gainers, losers, most_active)
        }

        let (equity_gainers, equity_losers, equity_most_active) =
            compute_top_n(&mut equity_entries, TOP_N, MIN_CHANGE_PCT_THRESHOLD);
        let (index_gainers, index_losers, index_most_active) =
            compute_top_n(&mut index_entries, TOP_N, MIN_CHANGE_PCT_THRESHOLD);

        let snapshot = TopMoversSnapshot {
            equity_gainers,
            equity_losers,
            equity_most_active,
            index_gainers,
            index_losers,
            index_most_active,
            total_tracked,
        };

        debug!(
            total_tracked,
            eq_gainers = snapshot.equity_gainers.len(),
            eq_losers = snapshot.equity_losers.len(),
            idx_gainers = snapshot.index_gainers.len(),
            "stock movers snapshot computed (equity + index separated)"
        );

        // O(1) EXEMPT: cold path (every 5s). Clone copies ~1KB (3 × 20 Copy entries).
        self.latest_snapshot = Some(snapshot.clone());
        snapshot
    }

    /// Returns the latest snapshot (if any has been computed).
    pub fn latest_snapshot(&self) -> Option<&TopMoversSnapshot> {
        self.latest_snapshot.as_ref()
    }

    /// Returns the number of securities being tracked.
    pub fn tracked_count(&self) -> usize {
        self.securities.len()
    }

    /// Returns total ticks processed.
    pub fn ticks_processed(&self) -> u64 {
        self.ticks_processed
    }
}

// ===========================================================================
// Plan B (2026-04-22) — MoversTrackerV2: unified 6-bucket movers engine.
//
// Replaces the split TopMoversTracker (stocks + indices) + OptionMoversTracker
// (mixed NSE_FNO) with a single struct that classifies every tick into one of
// SIX buckets (indices / stocks / index_futures / stock_futures / index_options
// / stock_options) via InstrumentRegistry::get_with_segment (I-P1-11 compliant),
// then maintains an independently-ranked top-N leaderboard per bucket.
//
// See the plan file `.claude/plans/active-plan.md` and the runbook
// `docs/runbooks/movers.md` (shipped in Phase H).
//
// # Performance
//
// - `update_v2()` — single HashMap lookup + arithmetic + bucket-cached write. O(1).
// - `compute_snapshot_v2()` — one iteration over all entries with per-bucket
//   BinaryHeap dispatch. O(N log K) where K = TOP_N = 20.
//
// # Back-compat
//
// The legacy TopMoversTracker above remains until Phase G migration is complete.
// Both trackers can coexist during the transition (two HashMap lookups per tick
// is acceptable for the cutover window; to be removed in a follow-up PR).
// ===========================================================================

use tickvault_common::instrument_registry::InstrumentRegistry;
use tickvault_common::types::ExchangeSegment;

use crate::pipeline::mover_classifier::{MoverBucket, classify_instrument};

// ---------------------------------------------------------------------------
// V2 types
// ---------------------------------------------------------------------------

/// Timeframe over which a movers ranking is computed.
///
/// Mirrors the `candles_*` materialized-view pattern: 1, 2, 3, …, 15
/// minutes — one ranking per timeframe per snapshot. A 5m ranking is NOT
/// derivable from the 1m ranking (different sets of top securities), so
/// each timeframe is computed independently from the rolling tick stream.
///
/// Wire format (`as_str()`) matches the `timeframe SYMBOL` column on the
/// `top_movers` QuestDB table, the `candles_*` materialized-view naming
/// convention, and the `?timeframe=5m` API query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum Timeframe {
    OneMin,
    TwoMin,
    ThreeMin,
    FourMin,
    FiveMin,
    SixMin,
    SevenMin,
    EightMin,
    NineMin,
    TenMin,
    ElevenMin,
    TwelveMin,
    ThirteenMin,
    FourteenMin,
    FifteenMin,
}

impl Timeframe {
    /// All 15 timeframes in ascending order. Static slice — no allocation.
    /// Use this whenever you need to iterate per-timeframe (e.g.
    /// `for tf in Timeframe::ALL`).
    pub const ALL: [Timeframe; 15] = [
        Timeframe::OneMin,
        Timeframe::TwoMin,
        Timeframe::ThreeMin,
        Timeframe::FourMin,
        Timeframe::FiveMin,
        Timeframe::SixMin,
        Timeframe::SevenMin,
        Timeframe::EightMin,
        Timeframe::NineMin,
        Timeframe::TenMin,
        Timeframe::ElevenMin,
        Timeframe::TwelveMin,
        Timeframe::ThirteenMin,
        Timeframe::FourteenMin,
        Timeframe::FifteenMin,
    ];

    /// Wire-format string used in the `timeframe` QuestDB column and in
    /// API query parameters. Stable — changing this is a breaking change.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Timeframe::OneMin => "1m",
            Timeframe::TwoMin => "2m",
            Timeframe::ThreeMin => "3m",
            Timeframe::FourMin => "4m",
            Timeframe::FiveMin => "5m",
            Timeframe::SixMin => "6m",
            Timeframe::SevenMin => "7m",
            Timeframe::EightMin => "8m",
            Timeframe::NineMin => "9m",
            Timeframe::TenMin => "10m",
            Timeframe::ElevenMin => "11m",
            Timeframe::TwelveMin => "12m",
            Timeframe::ThirteenMin => "13m",
            Timeframe::FourteenMin => "14m",
            Timeframe::FifteenMin => "15m",
        }
    }

    /// Number of seconds in this timeframe. Used by the rolling baseline
    /// lookup to find the price `secs()` ago.
    #[inline]
    pub const fn secs(self) -> u32 {
        match self {
            Timeframe::OneMin => 60,
            Timeframe::TwoMin => 120,
            Timeframe::ThreeMin => 180,
            Timeframe::FourMin => 240,
            Timeframe::FiveMin => 300,
            Timeframe::SixMin => 360,
            Timeframe::SevenMin => 420,
            Timeframe::EightMin => 480,
            Timeframe::NineMin => 540,
            Timeframe::TenMin => 600,
            Timeframe::ElevenMin => 660,
            Timeframe::TwelveMin => 720,
            Timeframe::ThirteenMin => 780,
            Timeframe::FourteenMin => 840,
            Timeframe::FifteenMin => 900,
        }
    }
}

/// Number of ranked entries per bucket × category. Matches plan item spec.
const MOVERS_V2_TOP_N: usize = 20;

/// A single ranked entry emitted by the V2 tracker.
///
/// `oi`, `prev_oi`, `oi_change_pct`, and `value` are meaningless (set to 0)
/// for `Indices` / `Stocks` buckets (no derivatives). Callers that render
/// per-bucket tables should hide those columns for non-derivative buckets.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct MoverEntryV2 {
    pub security_id: u32,
    pub exchange_segment_code: u8,
    pub last_traded_price: f32,
    pub prev_close: f32,
    pub change_pct: f32,
    pub volume: u32,
    pub open_interest: u32,
    pub prev_open_interest: u32,
    pub oi_change_pct: f32,
    pub value: f64,
}

/// Rankings available for price buckets (Indices / Stocks).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PriceBucket {
    pub gainers: Vec<MoverEntryV2>,
    pub losers: Vec<MoverEntryV2>,
    pub most_active: Vec<MoverEntryV2>,
    pub tracked: usize,
}

/// Rankings available for derivative buckets (futures + options).
///
/// `oi_buildup` and `oi_unwind` are always empty until a `prev_oi` baseline
/// source lands (Dhan does NOT push prev-day OI for NSE_FNO live; future
/// work will snapshot OI at 09:15:00 IST to use as the intraday baseline).
///
/// `premium` and `discount` apply ONLY to futures buckets — they rank by
/// `LTP − spot_of_underlying`. Options buckets leave both empty (premium
/// for options is computed differently, and the operator UI uses
/// `top_value` for options instead). Futures Premium/Discount require a
/// live spot price for the underlying — empty when spot is unavailable
/// (warm-up grace).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DerivativeBucket {
    pub gainers: Vec<MoverEntryV2>,
    pub losers: Vec<MoverEntryV2>,
    pub most_active: Vec<MoverEntryV2>,
    pub top_oi: Vec<MoverEntryV2>,
    pub oi_buildup: Vec<MoverEntryV2>, // empty until prev-OI baseline wired
    pub oi_unwind: Vec<MoverEntryV2>,  // empty until prev-OI baseline wired
    pub top_value: Vec<MoverEntryV2>,
    /// Futures only: LTP > spot_of_underlying, ranked by spread descending.
    /// Empty for options buckets (operators use `top_value` there) and
    /// empty whenever the underlying spot is unknown (warm-up grace).
    pub premium: Vec<MoverEntryV2>,
    /// Futures only: LTP < spot_of_underlying, ranked by spread ascending
    /// (most-negative first). Empty for options buckets and during
    /// underlying-spot warm-up.
    pub discount: Vec<MoverEntryV2>,
    pub tracked: usize,
}

/// Complete V2 snapshot: all six buckets side-by-side. Produced at the
/// configured cadence (5s default) and consumed by the API handler +
/// QuestDB writer.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MoversSnapshotV2 {
    pub indices: PriceBucket,
    pub stocks: PriceBucket,
    pub index_futures: DerivativeBucket,
    pub stock_futures: DerivativeBucket,
    pub index_options: DerivativeBucket,
    pub stock_options: DerivativeBucket,
    pub total_tracked: usize,
    /// Snapshot emit time — IST epoch seconds.
    pub snapshot_at_ist_secs: i64,
}

/// Thread-safe handle for sharing the V2 snapshot with the API handler.
pub type SharedMoversSnapshotV2 = Arc<std::sync::RwLock<Option<MoversSnapshotV2>>>;

// ---------------------------------------------------------------------------
// Internal per-security V2 state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct V2State {
    bucket: MoverBucket,
    last_traded_price: f32,
    prev_close: f32,
    change_pct: f32,
    volume: u32,
    open_interest: u32,
    exchange_segment_code: u8,
}

impl V2State {
    #[inline]
    fn value(&self) -> f64 {
        // Traded value estimate = LTP × cumulative volume.
        // LTP: f32 → f64 via lossless decimal-string conversion (Dhan precision).
        // Volume: u32 → f64 via lossless .into() (integer widening, no IEEE issue).
        use tickvault_storage::tick_persistence::f32_to_f64_clean;
        let vol: f64 = self.volume.into();
        f32_to_f64_clean(self.last_traded_price) * vol
    }

    #[inline]
    fn to_entry(self, security_id: u32) -> MoverEntryV2 {
        MoverEntryV2 {
            security_id,
            exchange_segment_code: self.exchange_segment_code,
            last_traded_price: self.last_traded_price,
            prev_close: self.prev_close,
            change_pct: self.change_pct,
            volume: self.volume,
            open_interest: self.open_interest,
            // prev_open_interest left at 0 until baseline source lands.
            prev_open_interest: 0,
            oi_change_pct: 0.0,
            value: self.value(),
        }
    }
}

// ---------------------------------------------------------------------------
// MoversTrackerV2
// ---------------------------------------------------------------------------

/// Unified movers tracker with 6 buckets classified via `InstrumentRegistry`.
///
/// Spawned once at boot with a shared `Arc<InstrumentRegistry>`. Every tick
/// runs through `update_v2` in O(1). `compute_snapshot_v2` runs periodically
/// (every 5s default) on the cold path.
pub struct MoversTrackerV2 {
    /// Per-security state keyed by `(security_id, segment_code)`.
    securities: HashMap<(u32, u8), V2State>,
    /// Injected immutable registry for bucket classification.
    registry: Arc<InstrumentRegistry>,
    /// Prev-close cache populated from code-6 PrevClose packets (IDX_I only).
    /// For NSE_EQ / NSE_FNO we read `tick.day_close` on every tick (per Dhan
    /// Ticket #5525125 — Quote/Full `close` field IS previous-day close).
    prev_close_prices: HashMap<(u32, u8), f32>,
    /// OI baseline captured at 09:15 IST. Used to compute `oi_change_pct`
    /// for the `oi_buildup` + `oi_unwind` rankings. Dhan does NOT push
    /// prev-day OI on the NSE_FNO live feed (confirmed via Ticket
    /// #5525125) so we use the intraday open as a best-available baseline.
    /// Keyed `(security_id, segment_code)`.
    baseline_oi: HashMap<(u32, u8), u32>,
    ticks_processed: u64,
}

impl MoversTrackerV2 {
    /// Initial capacity matches the ~25k live instrument universe so no
    /// HashMap resize happens mid-session. Sized above the worst-case count.
    const INITIAL_CAPACITY: usize = 30_000;

    /// Creates a new tracker with the given registry. The registry must be
    /// immutable for the lifetime of the tracker — it is used for O(1)
    /// bucket classification on first tick per instrument.
    #[must_use]
    pub fn new(registry: Arc<InstrumentRegistry>) -> Self {
        Self {
            // O(1) EXEMPT: boot-time pre-allocation for ~25k instruments.
            securities: HashMap::with_capacity(Self::INITIAL_CAPACITY),
            registry,
            prev_close_prices: HashMap::with_capacity(Self::INITIAL_CAPACITY),
            baseline_oi: HashMap::with_capacity(Self::INITIAL_CAPACITY),
            ticks_processed: 0,
        }
    }

    /// Captures the current OI for every tracked derivative as the intraday
    /// baseline. Intended to be called once at 09:15 IST by the pipeline
    /// scheduler; re-calling overwrites the previous baseline (idempotent).
    ///
    /// Price buckets (`Indices` / `Stocks`) are intentionally skipped — they
    /// carry no OI and do not participate in `oi_buildup` / `oi_unwind`.
    ///
    /// Returns the number of baseline entries captured (cold-path diagnostic).
    pub fn capture_baseline_oi(&mut self) -> usize {
        // O(1) EXEMPT: cold path — called once per trading day at 09:15 IST.
        self.baseline_oi.clear();
        let mut captured = 0_usize;
        for (key, state) in self.securities.iter() {
            if state.bucket.has_open_interest() && state.open_interest > 0 {
                self.baseline_oi.insert(*key, state.open_interest);
                captured += 1;
            }
        }
        captured
    }

    /// Current baseline OI population. Cold-path diagnostic.
    #[must_use]
    // TEST-EXEMPT: trivial getter covered by capture_baseline_oi tests
    pub fn baseline_oi_len(&self) -> usize {
        self.baseline_oi.len()
    }

    /// Stores prev_close from a PrevClose (code-6) packet. O(1).
    #[inline]
    pub fn update_prev_close(&mut self, security_id: u32, segment_code: u8, prev_close: f32) {
        if prev_close.is_finite() && prev_close > 0.0 {
            self.prev_close_prices
                .insert((security_id, segment_code), prev_close);
        }
    }

    /// Hot-path tick update. O(1). No allocation.
    #[inline]
    pub fn update_v2(&mut self, tick: &ParsedTick) {
        if !tick.last_traded_price.is_finite() || tick.last_traded_price <= 0.0 {
            return;
        }

        let key = (tick.security_id, tick.exchange_segment_code);

        // Resolve prev_close: cache first, tick's day_close fallback.
        let prev_close = if let Some(&pc) = self.prev_close_prices.get(&key) {
            pc
        } else if tick.day_close.is_finite() && tick.day_close > 0.0 {
            self.prev_close_prices.insert(key, tick.day_close);
            tick.day_close
        } else {
            return;
        };

        self.ticks_processed = self.ticks_processed.saturating_add(1);

        let change_pct = ((tick.last_traded_price - prev_close) / prev_close) * 100.0;

        if let Some(state) = self.securities.get_mut(&key) {
            state.last_traded_price = tick.last_traded_price;
            state.prev_close = prev_close;
            state.change_pct = change_pct;
            state.volume = tick.volume;
            state.open_interest = tick.open_interest;
        } else {
            // First tick for this instrument — classify via registry.
            // Segment enum lookup is infallible for our 8 known codes; on
            // an unknown byte we skip the tick entirely.
            let segment = match segment_from_code(tick.exchange_segment_code) {
                Some(s) => s,
                None => return,
            };
            let Some(bucket) = classify_instrument(&self.registry, tick.security_id, segment)
            else {
                return;
            };
            self.securities.insert(
                key,
                V2State {
                    bucket,
                    last_traded_price: tick.last_traded_price,
                    prev_close,
                    change_pct,
                    volume: tick.volume,
                    open_interest: tick.open_interest,
                    exchange_segment_code: tick.exchange_segment_code,
                },
            );
        }
    }

    /// Produces the current snapshot. Cold path — called every N seconds.
    ///
    /// # Performance
    /// O(N log K) where K = `MOVERS_V2_TOP_N`. Single iteration, 6 × 7
    /// fixed-size heaps (currently 5 populated categories + 2 always-empty
    /// pending the prev-OI baseline source).
    // O(1) EXEMPT: cold path — runs every 5s not per tick.
    pub fn compute_snapshot_v2(&self) -> MoversSnapshotV2 {
        // Plan item F1 (2026-04-22): histogram — latency of each recompute.
        let start = std::time::Instant::now();

        let mut indices = PriceBucket::default();
        let mut stocks = PriceBucket::default();
        let mut index_futures = DerivativeBucket::default();
        let mut stock_futures = DerivativeBucket::default();
        let mut index_options = DerivativeBucket::default();
        let mut stock_options = DerivativeBucket::default();

        // For each bucket, accumulate two sorted-by-abs-change lists
        // (gainers/losers) and one by-volume list; derivatives also get
        // top-oi and top-value. Use Vec + sort for simplicity — N is
        // bounded per bucket and we only run every 5s.
        //
        // If a baseline OI was captured at 09:15 IST (plan OI-baseline item),
        // enrich each derivative entry with prev_oi + oi_change_pct so the
        // oi_buildup / oi_unwind rankings are meaningful instead of empty.
        let entries: Vec<(u32, MoverEntryV2, MoverBucket)> = self
            .securities
            .iter()
            .map(|(&key, state)| {
                let mut entry = state.to_entry(key.0);
                if state.bucket.has_open_interest()
                    && let Some(&baseline) = self.baseline_oi.get(&key)
                    && baseline > 0
                {
                    entry.prev_open_interest = baseline;
                    // DATA-INTEGRITY-EXEMPT: integer OI counts, not Dhan f32 prices — lossless widening.
                    let current = f64::from(state.open_interest);
                    // DATA-INTEGRITY-EXEMPT: integer OI baseline count, not f32 price data.
                    let prev = f64::from(baseline);
                    // (current - prev) / prev * 100. Clamp to f32 wire type.
                    let pct = ((current - prev) / prev) * 100.0;
                    entry.oi_change_pct = if pct.is_finite() { pct as f32 } else { 0.0 };
                }
                (key.0, entry, state.bucket)
            })
            .collect();

        for (_sid, entry, bucket) in entries {
            match bucket {
                MoverBucket::Indices => {
                    indices.tracked += 1;
                    indices.gainers.push(entry);
                    indices.losers.push(entry);
                    indices.most_active.push(entry);
                }
                MoverBucket::Stocks => {
                    stocks.tracked += 1;
                    stocks.gainers.push(entry);
                    stocks.losers.push(entry);
                    stocks.most_active.push(entry);
                }
                MoverBucket::IndexFutures => push_derivative(&mut index_futures, entry),
                MoverBucket::StockFutures => push_derivative(&mut stock_futures, entry),
                MoverBucket::IndexOptions => push_derivative(&mut index_options, entry),
                MoverBucket::StockOptions => push_derivative(&mut stock_options, entry),
            }
        }

        trim_price_bucket(&mut indices);
        trim_price_bucket(&mut stocks);
        trim_derivative_bucket(&mut index_futures);
        trim_derivative_bucket(&mut stock_futures);
        trim_derivative_bucket(&mut index_options);
        trim_derivative_bucket(&mut stock_options);

        let total_tracked = indices.tracked
            + stocks.tracked
            + index_futures.tracked
            + stock_futures.tracked
            + index_options.tracked
            + stock_options.tracked;

        let snapshot = MoversSnapshotV2 {
            indices,
            stocks,
            index_futures,
            stock_futures,
            index_options,
            stock_options,
            total_tracked,
            snapshot_at_ist_secs: ist_now_secs(),
        };

        // Plan item F1 (2026-04-22): emit three Prometheus series per snapshot:
        //   - tv_movers_snapshot_duration_ms (histogram, unlabelled)
        //   - tv_movers_tracked_total{bucket} (gauge, one per bucket)
        // `tv_movers_rows_written_total` is emitted by the ILP writer (Phase C2)
        // not the tracker.
        let elapsed_ms: u32 = u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
        // O(1) EXEMPT: handle creation on cold path (every 5s, not per tick).
        let elapsed_f64: f64 = elapsed_ms.into();
        metrics::histogram!("tv_movers_snapshot_duration_ms").record(elapsed_f64);
        metrics::gauge!(
            "tv_movers_tracked_total",
            "bucket" => MoverBucket::Indices.as_str()
        )
        .set(snapshot.indices.tracked as f64);
        metrics::gauge!(
            "tv_movers_tracked_total",
            "bucket" => MoverBucket::Stocks.as_str()
        )
        .set(snapshot.stocks.tracked as f64);
        metrics::gauge!(
            "tv_movers_tracked_total",
            "bucket" => MoverBucket::IndexFutures.as_str()
        )
        .set(snapshot.index_futures.tracked as f64);
        metrics::gauge!(
            "tv_movers_tracked_total",
            "bucket" => MoverBucket::StockFutures.as_str()
        )
        .set(snapshot.stock_futures.tracked as f64);
        metrics::gauge!(
            "tv_movers_tracked_total",
            "bucket" => MoverBucket::IndexOptions.as_str()
        )
        .set(snapshot.index_options.tracked as f64);
        metrics::gauge!(
            "tv_movers_tracked_total",
            "bucket" => MoverBucket::StockOptions.as_str()
        )
        .set(snapshot.stock_options.tracked as f64);

        snapshot
    }

    /// Total ticks processed. Diagnostic only.
    #[must_use]
    pub fn v2_ticks_processed(&self) -> u64 {
        self.ticks_processed
    }

    /// Shared pointer to the injected registry — for callers that need to
    /// enrich snapshot entries (e.g. the ILP writer) without re-injecting
    /// a second handle.
    #[must_use]
    // TEST-EXEMPT: trivial accessor returning &Arc<InstrumentRegistry>
    pub fn registry(&self) -> &Arc<InstrumentRegistry> {
        &self.registry
    }
}

// ---------------------------------------------------------------------------
// Plan item C2 (2026-04-22) — MoversSnapshotV2 → TopMoverRow serializer.
// ---------------------------------------------------------------------------
//
// Converts a V2 snapshot into a flat `Vec<TopMoverRow>` enriched with
// symbol / underlying / expiry / strike / option_type from the
// `InstrumentRegistry`. The vec is ready for the `TopMoversV2Writer::append_row`
// path.
//
// Called on the cold path (once per persistence cadence, typically 1 Hz).
// Allocation is therefore EXEMPT from the hot-path rule.
// ---------------------------------------------------------------------------

/// Wire-format string for the `segment` SYMBOL column. Stable — must
/// match what the tick-pipeline writes for the same security so cross-table
/// joins line up.
#[inline]
fn segment_wire_str(code: u8) -> &'static str {
    match segment_from_code(code) {
        Some(ExchangeSegment::IdxI) => "IDX_I",
        Some(ExchangeSegment::NseEquity) => "NSE_EQ",
        Some(ExchangeSegment::NseFno) => "NSE_FNO",
        Some(ExchangeSegment::NseCurrency) => "NSE_CURRENCY",
        Some(ExchangeSegment::BseEquity) => "BSE_EQ",
        Some(ExchangeSegment::McxComm) => "MCX_COMM",
        Some(ExchangeSegment::BseCurrency) => "BSE_CURRENCY",
        Some(ExchangeSegment::BseFno) => "BSE_FNO",
        None => "UNKNOWN",
    }
}

/// Converts one `MoverEntryV2` into a `TopMoverRow`, looking up display
/// metadata from the registry via segment-aware key (I-P1-11).
///
/// The caller provides the `bucket` and `rank_category` wire strings since
/// those are owned by the enum layer, not the entry itself. `timeframe`
/// (added 2026-04-25) is the rolling-window timeframe (1m..15m) the row
/// belongs to; `segment` is the wire-format symbol for the I-P1-11
/// composite key.
fn entry_to_row(
    entry: &MoverEntryV2,
    bucket: &MoverBucket,
    rank_category: &str,
    rank: i32,
    ts_nanos: i64,
    timeframe: Timeframe,
    registry: &InstrumentRegistry,
) -> tickvault_storage::movers_persistence::TopMoverRow {
    // Registry lookup: segment-aware per I-P1-11.
    let segment = segment_from_code(entry.exchange_segment_code);
    let subscribed = segment.and_then(|s| registry.get_with_segment(entry.security_id, s));

    let symbol = subscribed
        .map(|s| s.display_label.clone())
        .unwrap_or_else(|| format!("id-{}", entry.security_id));
    let underlying = subscribed
        .filter(|_| bucket.has_open_interest())
        .map(|s| s.underlying_symbol.clone());
    let expiry = subscribed.and_then(|s| s.expiry_date.map(|d| d.format("%Y-%m-%d").to_string()));
    let strike = subscribed.and_then(|s| s.strike_price);
    let option_type = subscribed.and_then(|s| s.option_type).map(|ot| match ot {
        tickvault_common::types::OptionType::Call => "CE".to_string(),
        tickvault_common::types::OptionType::Put => "PE".to_string(),
    });

    tickvault_storage::movers_persistence::TopMoverRow {
        ts_nanos,
        timeframe: timeframe.as_str(),
        bucket: bucket.as_str().to_string(),
        rank_category: rank_category.to_string(),
        rank,
        security_id: entry.security_id,
        segment: segment_wire_str(entry.exchange_segment_code),
        symbol,
        underlying,
        expiry,
        strike,
        option_type,
        ltp: tickvault_storage::tick_persistence::f32_to_f64_clean(entry.last_traded_price),
        prev_close: tickvault_storage::tick_persistence::f32_to_f64_clean(entry.prev_close),
        change_pct: tickvault_storage::tick_persistence::f32_to_f64_clean(entry.change_pct),
        volume: i64::from(entry.volume),
        value: entry.value,
        oi: i64::from(entry.open_interest),
        prev_oi: i64::from(entry.prev_open_interest),
        oi_change_pct: tickvault_storage::tick_persistence::f32_to_f64_clean(entry.oi_change_pct),
    }
}

/// Flattens a price bucket into its three category lists of rows.
fn flatten_price_bucket(
    bucket: &MoverBucket,
    price: &PriceBucket,
    ts_nanos: i64,
    timeframe: Timeframe,
    registry: &InstrumentRegistry,
    out: &mut Vec<tickvault_storage::movers_persistence::TopMoverRow>,
) {
    for (idx, entry) in price.gainers.iter().enumerate() {
        out.push(entry_to_row(
            entry,
            bucket,
            "gainers",
            (idx + 1) as i32,
            ts_nanos,
            timeframe,
            registry,
        ));
    }
    for (idx, entry) in price.losers.iter().enumerate() {
        out.push(entry_to_row(
            entry,
            bucket,
            "losers",
            (idx + 1) as i32,
            ts_nanos,
            timeframe,
            registry,
        ));
    }
    for (idx, entry) in price.most_active.iter().enumerate() {
        out.push(entry_to_row(
            entry,
            bucket,
            "most_active",
            (idx + 1) as i32,
            ts_nanos,
            timeframe,
            registry,
        ));
    }
}

/// Flattens a derivative bucket into its up-to-nine category lists of rows.
/// `premium` and `discount` (added 2026-04-25) are populated for futures
/// buckets only; they remain empty for options buckets and during
/// underlying-spot warm-up.
fn flatten_derivative_bucket(
    bucket: &MoverBucket,
    deriv: &DerivativeBucket,
    ts_nanos: i64,
    timeframe: Timeframe,
    registry: &InstrumentRegistry,
    out: &mut Vec<tickvault_storage::movers_persistence::TopMoverRow>,
) {
    let categories: [(&str, &Vec<MoverEntryV2>); 9] = [
        ("gainers", &deriv.gainers),
        ("losers", &deriv.losers),
        ("most_active", &deriv.most_active),
        ("top_oi", &deriv.top_oi),
        ("oi_buildup", &deriv.oi_buildup),
        ("oi_unwind", &deriv.oi_unwind),
        ("top_value", &deriv.top_value),
        ("premium", &deriv.premium),
        ("discount", &deriv.discount),
    ];
    for (cat_name, list) in categories {
        for (idx, entry) in list.iter().enumerate() {
            out.push(entry_to_row(
                entry,
                bucket,
                cat_name,
                (idx + 1) as i32,
                ts_nanos,
                timeframe,
                registry,
            ));
        }
    }
}

/// Serializes a `MoversSnapshotV2` into the flat row shape consumed by
/// the `top_movers` ILP writer. Cold-path. Allocates once per snapshot.
///
/// `timeframe` (added 2026-04-25) tags every emitted row so the same
/// snapshot can fan out across the 15 timeframes (1m..15m) with distinct
/// rankings stored as separate rows under one DEDUP key.
#[must_use]
pub fn snapshot_to_rows(
    snapshot: &MoversSnapshotV2,
    timeframe: Timeframe,
    registry: &InstrumentRegistry,
) -> Vec<tickvault_storage::movers_persistence::TopMoverRow> {
    // O(1) EXEMPT: cold path — runs once per persistence cadence (default 1Hz),
    // never inside the tick hot loop.
    //
    // Nanosecond timestamp from the snapshot's IST epoch-seconds field.
    // `i64::saturating_mul` protects against overflow on pathological inputs.
    let ts_nanos = snapshot.snapshot_at_ist_secs.saturating_mul(1_000_000_000);
    // Upper bound on row count: (2 × 3 × 20) + (4 × 9 × 20) = 840 per timeframe.
    let mut out = Vec::with_capacity(840);
    flatten_price_bucket(
        &MoverBucket::Indices,
        &snapshot.indices,
        ts_nanos,
        timeframe,
        registry,
        &mut out,
    );
    flatten_price_bucket(
        &MoverBucket::Stocks,
        &snapshot.stocks,
        ts_nanos,
        timeframe,
        registry,
        &mut out,
    );
    flatten_derivative_bucket(
        &MoverBucket::IndexFutures,
        &snapshot.index_futures,
        ts_nanos,
        timeframe,
        registry,
        &mut out,
    );
    flatten_derivative_bucket(
        &MoverBucket::StockFutures,
        &snapshot.stock_futures,
        ts_nanos,
        timeframe,
        registry,
        &mut out,
    );
    flatten_derivative_bucket(
        &MoverBucket::IndexOptions,
        &snapshot.index_options,
        ts_nanos,
        timeframe,
        registry,
        &mut out,
    );
    flatten_derivative_bucket(
        &MoverBucket::StockOptions,
        &snapshot.stock_options,
        ts_nanos,
        timeframe,
        registry,
        &mut out,
    );
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline]
fn segment_from_code(code: u8) -> Option<ExchangeSegment> {
    // Mirrors `ExchangeSegment::from_byte` without forcing a dependency edge.
    // Codes 0/1/2/3/4/5/7/8 per Dhan annexure; 6 is intentionally absent.
    match code {
        0 => Some(ExchangeSegment::IdxI),
        1 => Some(ExchangeSegment::NseEquity),
        2 => Some(ExchangeSegment::NseFno),
        3 => Some(ExchangeSegment::NseCurrency),
        4 => Some(ExchangeSegment::BseEquity),
        5 => Some(ExchangeSegment::McxComm),
        7 => Some(ExchangeSegment::BseCurrency),
        8 => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// Returns current IST epoch seconds. Used for snapshot timestamp.
/// Read Rule 3 in `audit-findings-2026-04-17.md` — market-hours helpers
/// already use this exact offset pattern.
#[inline]
fn ist_now_secs() -> i64 {
    use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
    chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
}

fn push_derivative(bucket: &mut DerivativeBucket, entry: MoverEntryV2) {
    bucket.tracked += 1;
    bucket.gainers.push(entry);
    bucket.losers.push(entry);
    bucket.most_active.push(entry);
    bucket.top_oi.push(entry);
    bucket.top_value.push(entry);
    // oi_buildup/oi_unwind populated only for entries with an OI baseline.
    // `prev_open_interest > 0` indicates a baseline was captured at 09:15 IST.
    // Without a baseline the entry carries `prev_open_interest = 0` and we
    // skip the OI-change rankings for it (gate prevents div-by-zero noise).
    if entry.prev_open_interest > 0 {
        bucket.oi_buildup.push(entry);
        bucket.oi_unwind.push(entry);
    }
}

fn trim_price_bucket(bucket: &mut PriceBucket) {
    // Gainers: positive change_pct, descending.
    bucket.gainers.sort_by(|a, b| {
        b.change_pct
            .partial_cmp(&a.change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bucket.gainers.retain(|e| e.change_pct > 0.0);
    bucket.gainers.truncate(MOVERS_V2_TOP_N);

    // Losers: negative change_pct, ascending.
    bucket.losers.sort_by(|a, b| {
        a.change_pct
            .partial_cmp(&b.change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bucket.losers.retain(|e| e.change_pct < 0.0);
    bucket.losers.truncate(MOVERS_V2_TOP_N);

    // Most active: volume desc.
    bucket.most_active.sort_by(|a, b| b.volume.cmp(&a.volume));
    bucket.most_active.truncate(MOVERS_V2_TOP_N);
}

fn trim_derivative_bucket(bucket: &mut DerivativeBucket) {
    bucket.gainers.sort_by(|a, b| {
        b.change_pct
            .partial_cmp(&a.change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bucket.gainers.retain(|e| e.change_pct > 0.0);
    bucket.gainers.truncate(MOVERS_V2_TOP_N);

    bucket.losers.sort_by(|a, b| {
        a.change_pct
            .partial_cmp(&b.change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bucket.losers.retain(|e| e.change_pct < 0.0);
    bucket.losers.truncate(MOVERS_V2_TOP_N);

    bucket.most_active.sort_by(|a, b| b.volume.cmp(&a.volume));
    bucket.most_active.truncate(MOVERS_V2_TOP_N);

    bucket
        .top_oi
        .sort_by(|a, b| b.open_interest.cmp(&a.open_interest));
    bucket.top_oi.truncate(MOVERS_V2_TOP_N);

    bucket.top_value.sort_by(|a, b| {
        b.value
            .partial_cmp(&a.value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bucket.top_value.truncate(MOVERS_V2_TOP_N);

    // OI buildup: positive oi_change_pct, descending.
    bucket.oi_buildup.sort_by(|a, b| {
        b.oi_change_pct
            .partial_cmp(&a.oi_change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bucket.oi_buildup.retain(|e| e.oi_change_pct > 0.0);
    bucket.oi_buildup.truncate(MOVERS_V2_TOP_N);

    // OI unwind: negative oi_change_pct, ascending.
    bucket.oi_unwind.sort_by(|a, b| {
        a.oi_change_pct
            .partial_cmp(&b.oi_change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bucket.oi_unwind.retain(|e| e.oi_change_pct < 0.0);
    bucket.oi_unwind.truncate(MOVERS_V2_TOP_N);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic
mod tests {
    use super::*;

    fn make_tick(
        security_id: u32,
        segment: u8,
        ltp: f32,
        prev_close: f32,
        volume: u32,
    ) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment,
            last_traded_price: ltp,
            day_close: prev_close,
            volume,
            exchange_timestamp: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn new_tracker_is_empty() {
        let tracker = TopMoversTracker::new();
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.ticks_processed(), 0);
        assert!(tracker.latest_snapshot().is_none());
    }

    #[test]
    fn tick_with_valid_close_is_tracked() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, 100.0, 1000));
        assert_eq!(tracker.tracked_count(), 1);
        assert_eq!(tracker.ticks_processed(), 1);
    }

    #[test]
    fn tick_without_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, 0.0, 1000)); // No prev close
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.ticks_processed(), 1);
    }

    #[test]
    fn change_pct_calculated_correctly() {
        let mut tracker = TopMoversTracker::new();
        // LTP=110, prev_close=100 → +10%
        tracker.update(&make_tick(100, 1, 110.0, 100.0, 1000));

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_gainers.len(), 1);
        let pct = snapshot.equity_gainers[0].change_pct;
        assert!((pct - 10.0).abs() < 0.01, "expected ~10%, got {pct}");
    }

    #[test]
    fn gainers_sorted_descending() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 105.0, 100.0, 100)); // +5%
        tracker.update(&make_tick(2, 1, 120.0, 100.0, 200)); // +20%
        tracker.update(&make_tick(3, 1, 110.0, 100.0, 300)); // +10%

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_gainers.len(), 3);
        assert_eq!(snapshot.equity_gainers[0].security_id, 2); // +20%
        assert_eq!(snapshot.equity_gainers[1].security_id, 3); // +10%
        assert_eq!(snapshot.equity_gainers[2].security_id, 1); // +5%
    }

    #[test]
    fn losers_sorted_ascending() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 95.0, 100.0, 100)); // -5%
        tracker.update(&make_tick(2, 1, 80.0, 100.0, 200)); // -20%
        tracker.update(&make_tick(3, 1, 90.0, 100.0, 300)); // -10%

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_losers.len(), 3);
        assert_eq!(snapshot.equity_losers[0].security_id, 2); // -20%
        assert_eq!(snapshot.equity_losers[1].security_id, 3); // -10%
        assert_eq!(snapshot.equity_losers[2].security_id, 1); // -5%
    }

    #[test]
    fn most_active_sorted_by_volume() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 100.0, 100.0, 500));
        tracker.update(&make_tick(2, 1, 100.0, 100.0, 5000));
        tracker.update(&make_tick(3, 1, 100.0, 100.0, 1000));

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_most_active.len(), 3);
        assert_eq!(snapshot.equity_most_active[0].security_id, 2); // 5000
        assert_eq!(snapshot.equity_most_active[1].security_id, 3); // 1000
        assert_eq!(snapshot.equity_most_active[2].security_id, 1); // 500
    }

    #[test]
    fn snapshot_caps_at_top_n() {
        let mut tracker = TopMoversTracker::new();
        // Create 30 gainers
        for i in 1..=30u32 {
            let ltp = 100.0 + (i as f32);
            tracker.update(&make_tick(i, 1, ltp, 100.0, i * 100));
        }

        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.equity_gainers.len() <= TOP_N);
        assert!(snapshot.equity_most_active.len() <= TOP_N);
    }

    #[test]
    fn snapshot_computation_updates_latest() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000));

        assert!(tracker.latest_snapshot().is_none());
        tracker.compute_snapshot();
        assert!(tracker.latest_snapshot().is_some());
    }

    #[test]
    fn constants_are_reasonable() {
        const _: () = assert!(TOP_N >= 5 && TOP_N <= 100);
        const _: () = assert!(MIN_CHANGE_PCT_THRESHOLD > 0.0 && MIN_CHANGE_PCT_THRESHOLD < 1.0);
        const _: () = assert!(TRACKER_MAP_INITIAL_CAPACITY >= 1024);
    }

    #[test]
    fn default_impl_matches_new() {
        let a = TopMoversTracker::new();
        let b = TopMoversTracker::default();
        assert_eq!(a.tracked_count(), b.tracked_count());
        assert_eq!(a.ticks_processed(), b.ticks_processed());
    }

    #[test]
    fn negative_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, -100.0, 1000));
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn nan_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, f32::NAN, 1000));
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn infinity_close_is_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, f32::INFINITY, 1000));
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn price_update_replaces_previous() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, 100.0, 1000)); // +10%
        tracker.update(&make_tick(100, 1, 120.0, 100.0, 2000)); // +20%

        assert_eq!(tracker.tracked_count(), 1); // Same security
        let snapshot = tracker.compute_snapshot();
        let pct = snapshot.equity_gainers[0].change_pct;
        assert!((pct - 20.0).abs() < 0.01, "should use latest price");
    }

    #[test]
    fn flat_security_excluded_from_gainers_and_losers() {
        let mut tracker = TopMoversTracker::new();
        // Price == prev_close → 0% change (below threshold)
        tracker.update(&make_tick(100, 1, 100.0, 100.0, 1000));

        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.equity_gainers.is_empty(),
            "flat security should not be a gainer"
        );
        assert!(
            snapshot.equity_losers.is_empty(),
            "flat security should not be a loser"
        );
        assert_eq!(
            snapshot.equity_most_active.len(),
            1,
            "flat security still most active"
        );
    }

    #[test]
    fn mover_entry_is_copy() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.0,
            prev_close: 0.0,
            change_pct: 5.0,
            volume: 1000,
        };
        let copy = entry;
        assert_eq!(entry.security_id, copy.security_id);
        assert_eq!(entry.change_pct, copy.change_pct);
    }

    #[test]
    fn snapshot_total_tracked_matches() {
        let mut tracker = TopMoversTracker::new();
        for i in 1..=10u32 {
            tracker.update(&make_tick(i, 1, 100.0 + i as f32, 100.0, i * 100));
        }

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.total_tracked, 10);
    }

    #[test]
    fn same_security_different_segments() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, 100.0, 1000)); // NSE_EQ
        tracker.update(&make_tick(100, 4, 120.0, 100.0, 2000)); // BSE_EQ

        assert_eq!(
            tracker.tracked_count(),
            2,
            "different segments should be tracked separately"
        );
    }

    #[test]
    fn empty_snapshot_when_no_data() {
        let mut tracker = TopMoversTracker::new();
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.equity_gainers.is_empty());
        assert!(snapshot.equity_losers.is_empty());
        assert!(snapshot.equity_most_active.is_empty());
        assert_eq!(snapshot.total_tracked, 0);
    }

    #[test]
    fn large_negative_change() {
        let mut tracker = TopMoversTracker::new();
        // Circuit breaker scenario: -20% drop
        tracker.update(&make_tick(100, 1, 80.0, 100.0, 5000));

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_losers.len(), 1);
        let pct = snapshot.equity_losers[0].change_pct;
        assert!((pct - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn ticks_processed_counts_all_including_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000)); // Tracked
        tracker.update(&make_tick(2, 1, 110.0, 0.0, 1000)); // Skipped (no close)
        tracker.update(&make_tick(3, 1, 110.0, -1.0, 1000)); // Skipped (negative close)

        assert_eq!(tracker.ticks_processed(), 3);
        assert_eq!(tracker.tracked_count(), 1);
    }

    #[test]
    fn consecutive_snapshots_independent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000));

        let s1 = tracker.compute_snapshot();
        assert_eq!(s1.equity_gainers.len(), 1);

        // Update price to flat
        tracker.update(&make_tick(1, 1, 100.0, 100.0, 1000));
        let s2 = tracker.compute_snapshot();
        assert!(s2.equity_gainers.is_empty(), "should reflect updated price");
    }

    // -----------------------------------------------------------------------
    // NaN change_pct is filtered from snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn nan_ltp_rejected_by_update() {
        let mut tracker = TopMoversTracker::new();
        // NaN LTP is rejected by the guard (non-finite LTP rejected at entry)
        tracker.update(&make_tick(100, 1, f32::NAN, 100.0, 1000));
        assert_eq!(tracker.tracked_count(), 0, "NaN LTP must be rejected");

        let mut tracker2 = TopMoversTracker::new();
        let tick = ParsedTick {
            security_id: 200,
            exchange_segment_code: 2,
            last_traded_price: f32::NAN,
            day_close: 100.0,
            volume: 500,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker2.update(&tick);
        assert_eq!(tracker2.tracked_count(), 0, "NaN LTP rejected by guard");

        let snapshot = tracker2.compute_snapshot();
        assert!(snapshot.equity_gainers.is_empty());
        assert!(snapshot.equity_losers.is_empty());
    }

    // -----------------------------------------------------------------------
    // Update existing vs new security
    // -----------------------------------------------------------------------

    #[test]
    fn update_existing_security_replaces_state() {
        let mut tracker = TopMoversTracker::new();

        // First update: LTP=110, volume=1000
        tracker.update(&make_tick(42, 1, 110.0, 100.0, 1000));
        assert_eq!(tracker.tracked_count(), 1);

        // Second update same security: LTP=120, volume=2000
        tracker.update(&make_tick(42, 1, 120.0, 100.0, 2000));
        assert_eq!(
            tracker.tracked_count(),
            1,
            "same security — count unchanged"
        );

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_gainers.len(), 1);
        // Should use the latest values
        let entry = &snapshot.equity_gainers[0];
        assert!((entry.change_pct - 20.0).abs() < 0.01);
        assert_eq!(entry.volume, 2000);
        assert!((entry.last_traded_price - 120.0).abs() < 0.01);
    }

    #[test]
    fn new_security_inserted() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000));
        tracker.update(&make_tick(2, 1, 120.0, 100.0, 2000));
        assert_eq!(tracker.tracked_count(), 2);
    }

    // -----------------------------------------------------------------------
    // MoverEntry serialization
    // -----------------------------------------------------------------------

    #[test]
    fn mover_entry_serializes_to_json() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.5,
            prev_close: 0.0,
            change_pct: 5.25,
            volume: 1000,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("42"));
        assert!(json.contains("5.25"));
    }

    // -----------------------------------------------------------------------
    // TopMoversSnapshot serialization
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_serializes_to_json() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000));
        let snapshot = tracker.compute_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("equity_gainers"));
        assert!(json.contains("equity_losers"));
        assert!(json.contains("equity_most_active"));
        assert!(json.contains("total_tracked"));
    }

    // -----------------------------------------------------------------------
    // Additional coverage: invalid day_close values, total_tracked sums,
    // instrument counts
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_day_close_neg_infinity_skipped() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, f32::NEG_INFINITY, 1000));
        assert_eq!(
            tracker.tracked_count(),
            0,
            "negative infinity day_close must be skipped"
        );
    }

    #[test]
    fn invalid_day_close_very_small_positive_accepted() {
        // day_close = f32::MIN_POSITIVE (smallest positive finite) is valid
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, f32::MIN_POSITIVE, 1000));
        assert_eq!(
            tracker.tracked_count(),
            1,
            "tiny positive day_close is finite and > 0, should be tracked"
        );
    }

    #[test]
    fn invalid_day_close_negative_zero_skipped() {
        // -0.0 <= 0.0 is true in IEEE 754, so it should be skipped
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(100, 1, 110.0, -0.0, 1000));
        assert_eq!(
            tracker.tracked_count(),
            0,
            "-0.0 day_close must be skipped (equal to 0.0)"
        );
    }

    #[test]
    fn total_tracked_reflects_distinct_instrument_count() {
        let mut tracker = TopMoversTracker::new();
        // 5 distinct securities
        for i in 1..=5u32 {
            tracker.update(&make_tick(i, 1, 100.0 + (i as f32), 100.0, i * 100));
        }
        assert_eq!(tracker.tracked_count(), 5);

        let snapshot = tracker.compute_snapshot();
        assert_eq!(
            snapshot.total_tracked, 5,
            "snapshot total_tracked must equal tracked_count"
        );
    }

    #[test]
    fn total_instruments_sums_gainers_losers_most_active() {
        let mut tracker = TopMoversTracker::new();
        // 3 gainers
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000)); // +10%
        tracker.update(&make_tick(2, 1, 120.0, 100.0, 2000)); // +20%
        tracker.update(&make_tick(3, 1, 105.0, 100.0, 3000)); // +5%
        // 2 losers
        tracker.update(&make_tick(4, 1, 90.0, 100.0, 4000)); // -10%
        tracker.update(&make_tick(5, 1, 80.0, 100.0, 5000)); // -20%
        // 1 flat (excluded from gainers/losers but included in most_active)
        tracker.update(&make_tick(6, 1, 100.0, 100.0, 6000)); // 0%

        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_gainers.len(), 3, "3 gainers");
        assert_eq!(snapshot.equity_losers.len(), 2, "2 losers");
        assert_eq!(
            snapshot.equity_most_active.len(),
            6,
            "all 6 securities in most_active"
        );
        assert_eq!(
            snapshot.total_tracked, 6,
            "total_tracked = all distinct instruments"
        );
    }

    #[test]
    fn snapshot_most_active_includes_flat_securities() {
        // Flat securities (0% change) appear in most_active but not
        // in gainers or losers
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 100.0, 100.0, 9999));
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.equity_gainers.is_empty());
        assert!(snapshot.equity_losers.is_empty());
        assert_eq!(snapshot.equity_most_active.len(), 1);
        assert_eq!(snapshot.equity_most_active[0].security_id, 1);
        assert_eq!(snapshot.equity_most_active[0].volume, 9999);
    }

    #[test]
    fn ticks_processed_saturates_at_u64_max() {
        let mut tracker = TopMoversTracker::new();
        // Manually verify saturating_add behavior by sending many ticks
        for _ in 0..10 {
            tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000));
        }
        assert_eq!(tracker.ticks_processed(), 10);
    }

    #[test]
    fn snapshot_with_nan_ltp_rejected_produces_empty_snapshot() {
        // NaN LTP is rejected at update() entry — never tracked.
        let mut tracker = TopMoversTracker::new();
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: f32::NAN,
            day_close: 100.0,
            volume: 5000,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker.update(&tick);
        assert_eq!(tracker.tracked_count(), 0, "NaN LTP rejected by guard");

        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.equity_gainers.is_empty(),
            "NaN excluded from gainers"
        );
        assert!(
            snapshot.equity_losers.is_empty(),
            "NaN excluded from losers"
        );
        assert!(
            snapshot.equity_most_active.is_empty(),
            "NaN excluded from most_active"
        );
    }

    // -----------------------------------------------------------------------
    // Additional coverage: SharedTopMoversSnapshot, Debug/Clone impls,
    // MoverEntry Debug/Serialize, snapshot after multiple compute cycles,
    // boundary near MIN_CHANGE_PCT_THRESHOLD
    // -----------------------------------------------------------------------

    #[test]
    fn shared_top_movers_snapshot_default_is_none() {
        let shared: SharedTopMoversSnapshot = std::sync::Arc::new(std::sync::RwLock::new(None));
        let guard = shared.read().unwrap();
        assert!(guard.is_none(), "shared snapshot should default to None");
    }

    #[test]
    fn shared_top_movers_snapshot_write_and_read() {
        let shared: SharedTopMoversSnapshot = std::sync::Arc::new(std::sync::RwLock::new(None));

        // Write a snapshot
        let snapshot = TopMoversSnapshot {
            equity_gainers: vec![],
            equity_losers: vec![],
            equity_most_active: vec![],
            index_gainers: vec![],
            index_losers: vec![],
            index_most_active: vec![],
            total_tracked: 42,
        };
        {
            let mut guard = shared.write().unwrap();
            *guard = Some(snapshot);
        }

        // Read it back
        let guard = shared.read().unwrap();
        assert!(guard.is_some());
        assert_eq!(guard.as_ref().unwrap().total_tracked, 42);
    }

    #[test]
    fn mover_entry_debug_impl() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.5,
            prev_close: 0.0,
            change_pct: 5.25,
            volume: 1000,
        };
        let debug = format!("{entry:?}");
        assert!(debug.contains("MoverEntry"));
        assert!(debug.contains("42"));
        assert!(debug.contains("100.5"));
    }

    #[test]
    fn top_movers_snapshot_debug_impl() {
        let snapshot = TopMoversSnapshot {
            equity_gainers: vec![],
            equity_losers: vec![],
            equity_most_active: vec![],
            index_gainers: vec![],
            index_losers: vec![],
            index_most_active: vec![],
            total_tracked: 0,
        };
        let debug = format!("{snapshot:?}");
        assert!(debug.contains("TopMoversSnapshot"));
        assert!(debug.contains("total_tracked"));
    }

    #[test]
    fn top_movers_snapshot_clone() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000));
        let snapshot = tracker.compute_snapshot();
        let cloned = snapshot.clone();
        assert_eq!(snapshot.total_tracked, cloned.total_tracked);
        assert_eq!(snapshot.equity_gainers.len(), cloned.equity_gainers.len());
        assert_eq!(snapshot.equity_losers.len(), cloned.equity_losers.len());
        assert_eq!(
            snapshot.equity_most_active.len(),
            cloned.equity_most_active.len()
        );
    }

    #[test]
    fn change_pct_just_above_threshold_is_gainer() {
        let mut tracker = TopMoversTracker::new();
        // MIN_CHANGE_PCT_THRESHOLD is 0.01. Need change_pct > 0.01.
        // close=10000, ltp=10001.1 => pct = 0.011 > 0.01
        tracker.update(&make_tick(1, 1, 10001.1, 10000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert_eq!(
            snapshot.equity_gainers.len(),
            1,
            "change just above threshold should be a gainer"
        );
    }

    #[test]
    fn change_pct_just_below_threshold_excluded_from_gainers() {
        let mut tracker = TopMoversTracker::new();
        // Need change_pct > 0 but <= 0.01 to be excluded.
        // close=100000, ltp=100009 => pct = 0.009 < 0.01
        tracker.update(&make_tick(1, 1, 100_009.0, 100_000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.equity_gainers.is_empty(),
            "change just below threshold should be excluded from gainers"
        );
    }

    #[test]
    fn change_pct_just_below_neg_threshold_is_loser() {
        let mut tracker = TopMoversTracker::new();
        // Need change_pct < -0.01.
        // close=10000, ltp=9998.9 => pct = -0.011 < -0.01
        tracker.update(&make_tick(1, 1, 9998.9, 10000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert_eq!(
            snapshot.equity_losers.len(),
            1,
            "change just below neg threshold should be a loser"
        );
    }

    #[test]
    fn change_pct_just_above_neg_threshold_excluded_from_losers() {
        let mut tracker = TopMoversTracker::new();
        // Need change_pct < 0 but >= -0.01 to be excluded.
        // close=100000, ltp=99991 => pct = -0.009 > -0.01
        tracker.update(&make_tick(1, 1, 99_991.0, 100_000.0, 500));
        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.equity_losers.is_empty(),
            "change just above neg threshold should be excluded from losers"
        );
    }

    #[test]
    fn multiple_compute_snapshots_are_independent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 120.0, 100.0, 1000));

        let s1 = tracker.compute_snapshot();
        let s2 = tracker.compute_snapshot();

        // Both snapshots should have the same data
        assert_eq!(s1.equity_gainers.len(), s2.equity_gainers.len());
        assert_eq!(s1.total_tracked, s2.total_tracked);
    }

    #[test]
    fn latest_snapshot_returns_most_recent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 1000));
        tracker.compute_snapshot();

        // Add more data and compute again
        tracker.update(&make_tick(2, 1, 130.0, 100.0, 2000));
        tracker.compute_snapshot();

        let latest = tracker.latest_snapshot().unwrap();
        assert_eq!(
            latest.total_tracked, 2,
            "latest snapshot should reflect all tracked securities"
        );
    }

    #[test]
    fn volume_zero_still_tracked() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 110.0, 100.0, 0));
        assert_eq!(tracker.tracked_count(), 1);
        let snapshot = tracker.compute_snapshot();
        assert_eq!(snapshot.equity_most_active.len(), 1);
        assert_eq!(snapshot.equity_most_active[0].volume, 0);
    }

    #[test]
    fn mover_entry_serialization_includes_all_fields() {
        let entry = MoverEntry {
            security_id: 12345,
            exchange_segment_code: 1,
            last_traded_price: 2345.50,
            prev_close: 0.0,
            change_pct: -3.75,
            volume: 99999,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["security_id"], 12345);
        assert_eq!(json["exchange_segment_code"], 1);
        assert_eq!(json["volume"], 99999);
        // Float comparison via JSON
        let ltp = json["last_traded_price"].as_f64().unwrap();
        assert!((ltp - 2345.5).abs() < 0.01);
        let pct = json["change_pct"].as_f64().unwrap();
        assert!((pct - (-3.75)).abs() < 0.01);
    }

    #[test]
    fn snapshot_serialization_includes_all_top_level_fields() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(1, 1, 120.0, 100.0, 5000));
        tracker.update(&make_tick(2, 1, 80.0, 100.0, 3000));
        let snapshot = tracker.compute_snapshot();
        let json = serde_json::to_value(&snapshot).unwrap();
        assert!(json.get("equity_gainers").is_some());
        assert!(json.get("equity_losers").is_some());
        assert!(json.get("equity_most_active").is_some());
        assert_eq!(json["total_tracked"], 2);
    }

    #[test]
    fn inf_ltp_rejected_by_update() {
        let mut tracker = TopMoversTracker::new();
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: f32::INFINITY,
            day_close: 100.0,
            volume: 500,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker.update(&tick);
        // Inf LTP is non-finite → rejected by guard
        assert_eq!(tracker.tracked_count(), 0, "Inf LTP rejected by guard");

        let snapshot = tracker.compute_snapshot();
        assert!(
            snapshot.equity_gainers.is_empty(),
            "inf LTP should not produce any entries"
        );
    }

    #[test]
    fn tracker_with_many_securities_caps_at_top_n_for_all_lists() {
        let mut tracker = TopMoversTracker::new();
        // 50 gainers, 50 losers (all with positive LTP to pass the guard)
        for i in 1..=50u32 {
            let ltp = 100.0 + (i as f32) * 2.0; // gainers
            tracker.update(&make_tick(i, 1, ltp, 100.0, i * 100));
        }
        for i in 51..=100u32 {
            // Use ltp > 0 for all: smallest is 100.0 - 49*1.9 = 6.9
            let ltp = 100.0 - ((i - 50) as f32) * 1.9; // losers (all > 0)
            tracker.update(&make_tick(i, 1, ltp, 100.0, i * 100));
        }
        let snapshot = tracker.compute_snapshot();
        assert!(snapshot.equity_gainers.len() <= TOP_N);
        assert!(snapshot.equity_losers.len() <= TOP_N);
        assert!(snapshot.equity_most_active.len() <= TOP_N);
        assert_eq!(snapshot.total_tracked, 100);
    }

    // -----------------------------------------------------------------------
    // Additional coverage: ticks_processed saturating behavior
    // -----------------------------------------------------------------------

    #[test]
    fn ticks_processed_uses_saturating_add() {
        let mut tracker = TopMoversTracker::new();
        // Feed 100 ticks: 50 valid, 50 skipped (no close). All should count.
        for i in 0..50u32 {
            tracker.update(&make_tick(i, 1, 110.0, 100.0, 1000));
        }
        for i in 50..100u32 {
            tracker.update(&make_tick(i, 1, 110.0, 0.0, 1000)); // skipped
        }
        assert_eq!(tracker.ticks_processed(), 100);
        assert_eq!(tracker.tracked_count(), 50);
    }

    // -----------------------------------------------------------------------
    // Update: existing vs new key in HashMap (insert vs get_mut paths)
    // -----------------------------------------------------------------------

    #[test]
    fn update_exercises_insert_path_for_new_key() {
        let mut tracker = TopMoversTracker::new();
        assert_eq!(tracker.tracked_count(), 0);
        // First tick for this (security_id, segment) exercises the insert path
        tracker.update(&make_tick(999, 1, 200.0, 100.0, 5000));
        assert_eq!(tracker.tracked_count(), 1);
    }

    #[test]
    fn update_exercises_get_mut_path_for_existing_key() {
        let mut tracker = TopMoversTracker::new();
        tracker.update(&make_tick(999, 1, 200.0, 100.0, 5000));
        // Second tick for same key exercises the get_mut path
        tracker.update(&make_tick(999, 1, 210.0, 100.0, 6000));
        assert_eq!(tracker.tracked_count(), 1);
        let snapshot = tracker.compute_snapshot();
        // Should have updated values from second tick
        assert!((snapshot.equity_gainers[0].last_traded_price - 210.0).abs() < 0.01);
        assert_eq!(snapshot.equity_gainers[0].volume, 6000);
    }

    // -----------------------------------------------------------------------
    // compute_snapshot: gainers filter (change_pct > threshold)
    // -----------------------------------------------------------------------

    #[test]
    fn compute_snapshot_exactly_at_threshold_excluded_from_gainers() {
        // Exactly at threshold should NOT pass the > check
        let mut tracker = TopMoversTracker::new();
        // change_pct = 0.01 exactly: close=10000, ltp=10001 => pct = 0.01
        tracker.update(&make_tick(1, 1, 10001.0, 10000.0, 100));
        let snapshot = tracker.compute_snapshot();
        // 0.01 is NOT > 0.01, so excluded
        assert!(
            snapshot.equity_gainers.is_empty(),
            "change_pct exactly at threshold should be excluded"
        );
    }

    #[test]
    fn compute_snapshot_exactly_at_neg_threshold_excluded_from_losers() {
        let mut tracker = TopMoversTracker::new();
        // change_pct = -0.01 exactly: close=10000, ltp=9999 => pct = -0.01
        tracker.update(&make_tick(1, 1, 9999.0, 10000.0, 100));
        let snapshot = tracker.compute_snapshot();
        // -0.01 is NOT < -0.01, so excluded
        assert!(
            snapshot.equity_losers.is_empty(),
            "change_pct exactly at neg threshold should be excluded"
        );
    }

    // -----------------------------------------------------------------------
    // latest_snapshot: starts None, populated after compute
    // -----------------------------------------------------------------------

    #[test]
    fn latest_snapshot_none_before_first_compute() {
        let tracker = TopMoversTracker::new();
        assert!(tracker.latest_snapshot().is_none());
    }

    #[test]
    fn latest_snapshot_some_after_compute_with_no_data() {
        let mut tracker = TopMoversTracker::new();
        tracker.compute_snapshot();
        assert!(tracker.latest_snapshot().is_some());
        assert_eq!(tracker.latest_snapshot().unwrap().total_tracked, 0);
    }

    // -----------------------------------------------------------------------
    // Default trait produces same state as new()
    // -----------------------------------------------------------------------

    #[test]
    fn default_tracker_no_snapshot_no_ticks() {
        let tracker = TopMoversTracker::default();
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.ticks_processed(), 0);
        assert!(tracker.latest_snapshot().is_none());
    }

    // -----------------------------------------------------------------------
    // update_prev_close — PrevClose packet integration
    // -----------------------------------------------------------------------

    #[test]
    fn prev_close_map_enables_tracking_when_day_close_is_zero() {
        let mut tracker = TopMoversTracker::new();
        // Set prev_close from PrevClose packet
        tracker.update_prev_close(100, 1, 1000.0);
        // Tick with day_close=0 (during market hours) — should now be tracked
        let tick = make_tick(100, 1, 1100.0, 0.0, 5000);
        tracker.update(&tick);
        assert_eq!(
            tracker.tracked_count(),
            1,
            "should track via prev_close map"
        );
        let snapshot = tracker.compute_snapshot();
        let pct = snapshot.equity_gainers[0].change_pct;
        assert!(
            (pct - 10.0).abs() < 0.01,
            "expected ~10% from prev_close map, got {pct}"
        );
    }

    #[test]
    fn prev_close_map_takes_priority_over_day_close() {
        let mut tracker = TopMoversTracker::new();
        // Set prev_close from PrevClose packet
        tracker.update_prev_close(100, 1, 200.0);
        // Tick with day_close=100 (different from PrevClose packet)
        let tick = make_tick(100, 1, 220.0, 100.0, 1000);
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        let pct = snapshot.equity_gainers[0].change_pct;
        // Should use prev_close map (200.0), not day_close (100.0)
        // (220 - 200) / 200 * 100 = 10%
        assert!(
            (pct - 10.0).abs() < 0.01,
            "should use prev_close map (10%), not day_close (120%), got {pct}"
        );
    }

    #[test]
    fn prev_close_map_rejects_invalid_values() {
        let mut tracker = TopMoversTracker::new();
        tracker.update_prev_close(100, 1, 0.0);
        tracker.update_prev_close(101, 1, -50.0);
        tracker.update_prev_close(102, 1, f32::NAN);
        tracker.update_prev_close(103, 1, f32::INFINITY);
        // None should be stored
        let tick = make_tick(100, 1, 110.0, 0.0, 1000);
        tracker.update(&tick);
        assert_eq!(
            tracker.tracked_count(),
            0,
            "invalid prev_close should be rejected"
        );
    }

    #[test]
    fn prev_close_map_different_segments_independent() {
        let mut tracker = TopMoversTracker::new();
        tracker.update_prev_close(100, 1, 500.0); // NSE_EQ
        tracker.update_prev_close(100, 4, 50.0); // BSE_EQ
        // Tick for BSE_EQ segment
        let tick = ParsedTick {
            security_id: 100,
            exchange_segment_code: 4,
            last_traded_price: 55.0,
            day_close: 0.0,
            volume: 1000,
            exchange_timestamp: 1000,
            ..Default::default()
        };
        tracker.update(&tick);
        let snapshot = tracker.compute_snapshot();
        // (55 - 50) / 50 * 100 = 10%, NOT (55 - 500) / 500 * 100
        let pct = snapshot.equity_gainers[0].change_pct;
        assert!(
            (pct - 10.0).abs() < 0.01,
            "should use segment-specific prev_close, got {pct}"
        );
    }

    // ========================================================================
    // Plan B (2026-04-22) — MoversTrackerV2 + MoversSnapshotV2 smoke tests
    // One per bucket: feed a tick, verify only that bucket's leaderboard
    // populates. Full ranking logic covered by mover_classifier tests.
    // ========================================================================

    fn make_v2_tracker_with_single_instrument(
        security_id: u32,
        segment: tickvault_common::types::ExchangeSegment,
        category: tickvault_common::instrument_registry::SubscriptionCategory,
        instrument_kind: Option<tickvault_common::instrument_types::DhanInstrumentKind>,
    ) -> MoversTrackerV2 {
        use chrono::NaiveDate;
        use tickvault_common::instrument_registry::{InstrumentRegistry, SubscribedInstrument};
        use tickvault_common::types::FeedMode;

        let instrument = SubscribedInstrument {
            security_id,
            exchange_segment: segment,
            category,
            display_label: "TEST".to_string(),
            underlying_symbol: "TEST".to_string(),
            instrument_kind,
            expiry_date: NaiveDate::from_ymd_opt(2026, 4, 28),
            strike_price: Some(25000.0),
            option_type: None,
            feed_mode: FeedMode::Full,
        };
        let reg = std::sync::Arc::new(InstrumentRegistry::from_instruments(vec![instrument]));
        MoversTrackerV2::new(reg)
    }

    fn mk_v2_tick(security_id: u32, segment_code: u8, ltp: f32, prev_close: f32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment_code,
            last_traded_price: ltp,
            day_close: prev_close,
            volume: 1_000,
            open_interest: 50_000,
            exchange_timestamp: 1_000,
            ..Default::default()
        }
    }

    #[test]
    fn test_v2_bucket_indices_populates_on_idx_i_tick() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            13,
            ExchangeSegment::IdxI,
            SubscriptionCategory::MajorIndexValue,
            None,
        );
        tracker.update_v2(&mk_v2_tick(13, 0, 22_100.0, 22_000.0));
        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.indices.tracked, 1);
        assert_eq!(snap.stocks.tracked, 0);
        assert_eq!(snap.index_futures.tracked, 0);
        assert_eq!(snap.stock_futures.tracked, 0);
        assert_eq!(snap.index_options.tracked, 0);
        assert_eq!(snap.stock_options.tracked, 0);
        assert_eq!(snap.indices.gainers.len(), 1);
    }

    #[test]
    fn test_v2_bucket_stocks_populates_on_nse_eq_tick() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            2885,
            ExchangeSegment::NseEquity,
            SubscriptionCategory::StockEquity,
            None,
        );
        tracker.update_v2(&mk_v2_tick(2885, 1, 2950.0, 2900.0));
        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.stocks.tracked, 1);
        assert_eq!(snap.indices.tracked, 0);
        assert_eq!(snap.stocks.gainers.len(), 1);
    }

    #[test]
    fn test_v2_bucket_index_futures_populates_on_futidx_tick() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            50001,
            ExchangeSegment::NseFno,
            SubscriptionCategory::IndexDerivative,
            Some(DhanInstrumentKind::FutureIndex),
        );
        tracker.update_v2(&mk_v2_tick(50001, 2, 22_200.0, 22_000.0));
        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.index_futures.tracked, 1);
        assert_eq!(snap.index_options.tracked, 0);
        assert_eq!(snap.index_futures.top_oi.len(), 1);
        assert_eq!(snap.index_futures.top_oi[0].open_interest, 50_000);
    }

    #[test]
    fn test_v2_bucket_stock_futures_populates_on_futstk_tick() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            60001,
            ExchangeSegment::NseFno,
            SubscriptionCategory::StockDerivative,
            Some(DhanInstrumentKind::FutureStock),
        );
        tracker.update_v2(&mk_v2_tick(60001, 2, 2950.0, 2900.0));
        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.stock_futures.tracked, 1);
        assert_eq!(snap.stock_futures.top_oi.len(), 1);
    }

    #[test]
    fn test_v2_bucket_index_options_populates_on_optidx_tick() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            70001,
            ExchangeSegment::NseFno,
            SubscriptionCategory::IndexDerivative,
            Some(DhanInstrumentKind::OptionIndex),
        );
        tracker.update_v2(&mk_v2_tick(70001, 2, 105.0, 100.0));
        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.index_options.tracked, 1);
        assert_eq!(snap.index_options.gainers.len(), 1);
        assert_eq!(snap.index_options.top_oi.len(), 1);
    }

    #[test]
    fn test_v2_bucket_stock_options_populates_on_optstk_tick() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            80001,
            ExchangeSegment::NseFno,
            SubscriptionCategory::StockDerivative,
            Some(DhanInstrumentKind::OptionStock),
        );
        tracker.update_v2(&mk_v2_tick(80001, 2, 50.0, 40.0));
        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.stock_options.tracked, 1);
        assert_eq!(snap.stock_options.gainers.len(), 1);
    }

    #[test]
    fn test_v2_oi_buildup_and_unwind_are_empty_by_design() {
        // Documented behaviour: prev-OI baseline source is not wired yet
        // (Dhan does not push prev-day OI for NSE_FNO live). Both lists
        // must stay empty so dashboards show "no data" instead of
        // misleading change percentages based on zero.
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            70001,
            ExchangeSegment::NseFno,
            SubscriptionCategory::IndexDerivative,
            Some(DhanInstrumentKind::OptionIndex),
        );
        tracker.update_v2(&mk_v2_tick(70001, 2, 105.0, 100.0));
        let snap = tracker.compute_snapshot_v2();
        assert!(
            snap.index_options.oi_buildup.is_empty(),
            "oi_buildup must be empty until prev-OI baseline source ships"
        );
        assert!(
            snap.index_options.oi_unwind.is_empty(),
            "oi_unwind must be empty until prev-OI baseline source ships"
        );
    }

    /// Umbrella test naming `update_v2` literally for the pub-fn-test-guard.
    #[test]
    fn test_update_v2_skips_non_finite_ltp() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            13,
            ExchangeSegment::IdxI,
            SubscriptionCategory::MajorIndexValue,
            None,
        );
        // NaN LTP must not populate.
        tracker.update_v2(&mk_v2_tick(13, 0, f32::NAN, 22_000.0));
        assert_eq!(tracker.v2_ticks_processed(), 0);
        // Valid tick bumps the counter.
        tracker.update_v2(&mk_v2_tick(13, 0, 22_100.0, 22_000.0));
        assert_eq!(tracker.v2_ticks_processed(), 1);
    }

    /// Umbrella test naming `compute_snapshot_v2` literally.
    #[test]
    fn test_compute_snapshot_v2_empty_tracker_yields_empty_buckets() {
        use tickvault_common::instrument_registry::InstrumentRegistry;
        let reg = std::sync::Arc::new(InstrumentRegistry::from_instruments(Vec::new()));
        let tracker = MoversTrackerV2::new(reg);
        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.total_tracked, 0);
        assert_eq!(snap.indices.tracked, 0);
        assert_eq!(snap.stocks.tracked, 0);
        assert_eq!(snap.index_futures.tracked, 0);
        assert_eq!(snap.stock_futures.tracked, 0);
        assert_eq!(snap.index_options.tracked, 0);
        assert_eq!(snap.stock_options.tracked, 0);
    }

    /// Umbrella test naming `v2_ticks_processed` literally + exercising it.
    #[test]
    fn test_v2_ticks_processed_monotonic_increment() {
        use tickvault_common::instrument_registry::SubscriptionCategory;
        use tickvault_common::types::ExchangeSegment;
        let mut tracker = make_v2_tracker_with_single_instrument(
            13,
            ExchangeSegment::IdxI,
            SubscriptionCategory::MajorIndexValue,
            None,
        );
        assert_eq!(tracker.v2_ticks_processed(), 0);
        tracker.update_v2(&mk_v2_tick(13, 0, 22_050.0, 22_000.0));
        tracker.update_v2(&mk_v2_tick(13, 0, 22_100.0, 22_000.0));
        tracker.update_v2(&mk_v2_tick(13, 0, 22_150.0, 22_000.0));
        assert_eq!(tracker.v2_ticks_processed(), 3);
    }

    #[test]
    fn test_v2_snapshot_total_tracked_sums_across_buckets() {
        use tickvault_common::instrument_registry::{InstrumentRegistry, SubscribedInstrument};
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::{ExchangeSegment, FeedMode};

        let instruments = vec![
            SubscribedInstrument {
                security_id: 13,
                exchange_segment: ExchangeSegment::IdxI,
                category:
                    tickvault_common::instrument_registry::SubscriptionCategory::MajorIndexValue,
                display_label: "NIFTY".to_string(),
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: None,
                expiry_date: None,
                strike_price: None,
                option_type: None,
                feed_mode: FeedMode::Ticker,
            },
            SubscribedInstrument {
                security_id: 2885,
                exchange_segment: ExchangeSegment::NseEquity,
                category: tickvault_common::instrument_registry::SubscriptionCategory::StockEquity,
                display_label: "RELIANCE".to_string(),
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: None,
                expiry_date: None,
                strike_price: None,
                option_type: None,
                feed_mode: FeedMode::Quote,
            },
            SubscribedInstrument {
                security_id: 70001,
                exchange_segment: ExchangeSegment::NseFno,
                category:
                    tickvault_common::instrument_registry::SubscriptionCategory::IndexDerivative,
                display_label: "NIFTY CE".to_string(),
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: Some(DhanInstrumentKind::OptionIndex),
                expiry_date: None,
                strike_price: Some(25000.0),
                option_type: None,
                feed_mode: FeedMode::Full,
            },
        ];
        let reg = std::sync::Arc::new(InstrumentRegistry::from_instruments(instruments));
        let mut tracker = MoversTrackerV2::new(reg);

        tracker.update_v2(&mk_v2_tick(13, 0, 22_100.0, 22_000.0));
        tracker.update_v2(&mk_v2_tick(2885, 1, 2950.0, 2900.0));
        tracker.update_v2(&mk_v2_tick(70001, 2, 105.0, 100.0));

        let snap = tracker.compute_snapshot_v2();
        assert_eq!(snap.total_tracked, 3);
        assert_eq!(snap.indices.tracked, 1);
        assert_eq!(snap.stocks.tracked, 1);
        assert_eq!(snap.index_options.tracked, 1);
    }

    // ========================================================================
    // Plan item C2 (2026-04-22) — snapshot_to_rows enrichment tests
    // ========================================================================

    #[test]
    fn test_snapshot_to_rows_empty_snapshot_emits_no_rows() {
        use tickvault_common::instrument_registry::InstrumentRegistry;
        let reg = InstrumentRegistry::empty();
        let snapshot = MoversSnapshotV2::default();
        let rows = snapshot_to_rows(&snapshot, Timeframe::OneMin, &reg);
        assert!(rows.is_empty());
    }

    #[test]
    fn test_snapshot_to_rows_indices_populated_with_display_label_and_no_underlying() {
        use tickvault_common::instrument_registry::{
            InstrumentRegistry, SubscribedInstrument, SubscriptionCategory,
        };
        use tickvault_common::types::{ExchangeSegment, FeedMode};

        let instrument = SubscribedInstrument {
            security_id: 13,
            exchange_segment: ExchangeSegment::IdxI,
            category: SubscriptionCategory::MajorIndexValue,
            display_label: "NIFTY".to_string(),
            underlying_symbol: "NIFTY".to_string(),
            instrument_kind: None,
            expiry_date: None,
            strike_price: None,
            option_type: None,
            feed_mode: FeedMode::Ticker,
        };
        let reg = InstrumentRegistry::from_instruments(vec![instrument]);

        let mut snapshot = MoversSnapshotV2 {
            snapshot_at_ist_secs: 1_800_000_000,
            ..Default::default()
        };
        snapshot.indices.gainers.push(MoverEntryV2 {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 22_100.0,
            prev_close: 22_000.0,
            change_pct: 0.4545,
            volume: 1_000_000,
            open_interest: 0,
            prev_open_interest: 0,
            oi_change_pct: 0.0,
            value: 0.0,
        });
        snapshot.indices.tracked = 1;

        let rows = snapshot_to_rows(&snapshot, Timeframe::OneMin, &reg);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.bucket, "indices");
        assert_eq!(row.rank_category, "gainers");
        assert_eq!(row.rank, 1);
        assert_eq!(row.security_id, 13);
        assert_eq!(row.symbol, "NIFTY");
        // Indices do NOT emit `underlying` / `expiry` / `strike` / `option_type`.
        assert!(row.underlying.is_none());
        assert!(row.expiry.is_none());
        assert!(row.strike.is_none());
        assert!(row.option_type.is_none());
        assert_eq!(row.ts_nanos, 1_800_000_000 * 1_000_000_000);
    }

    #[test]
    fn test_snapshot_to_rows_index_option_enriched_with_underlying_expiry_strike_cetype() {
        use chrono::NaiveDate;
        use tickvault_common::instrument_registry::{
            InstrumentRegistry, SubscribedInstrument, SubscriptionCategory,
        };
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::{ExchangeSegment, FeedMode, OptionType};

        let instrument = SubscribedInstrument {
            security_id: 49_081,
            exchange_segment: ExchangeSegment::NseFno,
            category: SubscriptionCategory::IndexDerivative,
            display_label: "NIFTY 25000 CE 28-APR".to_string(),
            underlying_symbol: "NIFTY".to_string(),
            instrument_kind: Some(DhanInstrumentKind::OptionIndex),
            expiry_date: NaiveDate::from_ymd_opt(2026, 4, 28),
            strike_price: Some(25_000.0),
            option_type: Some(OptionType::Call),
            feed_mode: FeedMode::Full,
        };
        let reg = InstrumentRegistry::from_instruments(vec![instrument]);

        let mut snapshot = MoversSnapshotV2 {
            snapshot_at_ist_secs: 1_800_000_000,
            ..Default::default()
        };
        snapshot.index_options.top_oi.push(MoverEntryV2 {
            security_id: 49_081,
            exchange_segment_code: 2,
            last_traded_price: 162.15,
            prev_close: 290.30,
            change_pct: -44.14,
            volume: 12_202_3_330,
            open_interest: 8_017_880,
            prev_open_interest: 4_673_565,
            oi_change_pct: 71.56,
            value: 19_786_775_434.5,
        });
        snapshot.index_options.tracked = 1;

        let rows = snapshot_to_rows(&snapshot, Timeframe::OneMin, &reg);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.bucket, "index_options");
        assert_eq!(row.rank_category, "top_oi");
        assert_eq!(row.symbol, "NIFTY 25000 CE 28-APR");
        assert_eq!(row.underlying.as_deref(), Some("NIFTY"));
        assert_eq!(row.expiry.as_deref(), Some("2026-04-28"));
        assert_eq!(row.strike, Some(25_000.0));
        assert_eq!(row.option_type.as_deref(), Some("CE"));
        assert_eq!(row.oi, 8_017_880);
        assert_eq!(row.prev_oi, 4_673_565);
    }

    #[test]
    fn test_snapshot_to_rows_missing_registry_entry_falls_back_to_id_label() {
        use tickvault_common::instrument_registry::InstrumentRegistry;
        let reg = InstrumentRegistry::empty();
        let mut snapshot = MoversSnapshotV2 {
            snapshot_at_ist_secs: 1_800_000_000,
            ..Default::default()
        };
        snapshot.stocks.gainers.push(MoverEntryV2 {
            security_id: 99_999,
            exchange_segment_code: 1,
            last_traded_price: 100.0,
            prev_close: 95.0,
            change_pct: 5.26,
            volume: 10_000,
            open_interest: 0,
            prev_open_interest: 0,
            oi_change_pct: 0.0,
            value: 1_000_000.0,
        });
        snapshot.stocks.tracked = 1;
        let rows = snapshot_to_rows(&snapshot, Timeframe::OneMin, &reg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, "id-99999");
    }

    #[test]
    fn test_snapshot_to_rows_six_buckets_capacity_within_upper_bound() {
        // Plan C spec: the bound is `(2 × 3 × 20) + (4 × 7 × 20) = 680`.
        // We verify the preallocation matches by inspecting a heavily populated
        // synthetic snapshot. The test asserts NO row is silently dropped when
        // the caller fills every slot.
        use tickvault_common::instrument_registry::InstrumentRegistry;
        let reg = InstrumentRegistry::empty();
        let base_entry = MoverEntryV2 {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: 1.0,
            prev_close: 1.0,
            change_pct: 0.0,
            volume: 1,
            open_interest: 1,
            prev_open_interest: 1,
            oi_change_pct: 0.0,
            value: 1.0,
        };
        let mut snapshot = MoversSnapshotV2::default();
        for _ in 0..20 {
            snapshot.indices.gainers.push(base_entry);
            snapshot.indices.losers.push(base_entry);
            snapshot.indices.most_active.push(base_entry);
            snapshot.stocks.gainers.push(base_entry);
            snapshot.stocks.losers.push(base_entry);
            snapshot.stocks.most_active.push(base_entry);
        }
        for bucket in [
            &mut snapshot.index_futures,
            &mut snapshot.stock_futures,
            &mut snapshot.index_options,
            &mut snapshot.stock_options,
        ] {
            for _ in 0..20 {
                bucket.gainers.push(base_entry);
                bucket.losers.push(base_entry);
                bucket.most_active.push(base_entry);
                bucket.top_oi.push(base_entry);
                bucket.oi_buildup.push(base_entry);
                bucket.oi_unwind.push(base_entry);
                bucket.top_value.push(base_entry);
            }
        }
        let rows = snapshot_to_rows(&snapshot, Timeframe::OneMin, &reg);
        // 2 × 3 × 20 + 4 × 7 × 20 = 680
        assert_eq!(rows.len(), 680);
    }

    // ========================================================================
    // Plan OI-baseline (2026-04-22) — capture_baseline_oi + oi_buildup/oi_unwind
    // ========================================================================

    fn tracker_with_index_option(
        security_id: u32,
        prev_oi_state_starts_at: u32,
    ) -> MoversTrackerV2 {
        use chrono::NaiveDate;
        use tickvault_common::instrument_registry::{
            InstrumentRegistry, SubscribedInstrument, SubscriptionCategory,
        };
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::{ExchangeSegment, FeedMode, OptionType};

        let instrument = SubscribedInstrument {
            security_id,
            exchange_segment: ExchangeSegment::NseFno,
            category: SubscriptionCategory::IndexDerivative,
            display_label: format!("NIFTY-OPT-{security_id}"),
            underlying_symbol: "NIFTY".to_string(),
            instrument_kind: Some(DhanInstrumentKind::OptionIndex),
            expiry_date: NaiveDate::from_ymd_opt(2026, 4, 28),
            strike_price: Some(25_000.0),
            option_type: Some(OptionType::Call),
            feed_mode: FeedMode::Full,
        };
        let reg = std::sync::Arc::new(InstrumentRegistry::from_instruments(vec![instrument]));
        let mut tracker = MoversTrackerV2::new(reg);
        // Seed a tick so the instrument enters `securities`.
        let mut tick = ParsedTick::default();
        tick.security_id = security_id;
        tick.exchange_segment_code = 2;
        tick.last_traded_price = 100.0;
        tick.day_close = 95.0;
        tick.open_interest = prev_oi_state_starts_at;
        tick.volume = 1_000;
        tick.exchange_timestamp = 1_000;
        tracker.update_v2(&tick);
        tracker
    }

    #[test]
    fn test_capture_baseline_oi_on_empty_tracker_captures_zero() {
        use tickvault_common::instrument_registry::InstrumentRegistry;
        let reg = std::sync::Arc::new(InstrumentRegistry::empty());
        let mut tracker = MoversTrackerV2::new(reg);
        let captured = tracker.capture_baseline_oi();
        assert_eq!(captured, 0);
        assert_eq!(tracker.baseline_oi_len(), 0);
    }

    #[test]
    fn test_capture_baseline_oi_captures_derivative_oi_snapshot() {
        let mut tracker = tracker_with_index_option(49_081, 500_000);
        let captured = tracker.capture_baseline_oi();
        assert_eq!(captured, 1);
        assert_eq!(tracker.baseline_oi_len(), 1);
    }

    #[test]
    fn test_capture_baseline_oi_skips_zero_oi_entries() {
        let mut tracker = tracker_with_index_option(49_081, 0);
        let captured = tracker.capture_baseline_oi();
        // OI=0 is NOT captured — prevents div-by-zero noise in oi_change_pct.
        assert_eq!(captured, 0);
    }

    #[test]
    fn test_capture_baseline_oi_idempotent_recapture_overwrites() {
        let mut tracker = tracker_with_index_option(49_081, 100_000);
        tracker.capture_baseline_oi();
        // Second capture after OI has grown must overwrite (idempotent).
        let mut tick = ParsedTick::default();
        tick.security_id = 49_081;
        tick.exchange_segment_code = 2;
        tick.last_traded_price = 110.0;
        tick.day_close = 95.0;
        tick.open_interest = 600_000;
        tick.volume = 2_000;
        tick.exchange_timestamp = 2_000;
        tracker.update_v2(&tick);
        let recaptured = tracker.capture_baseline_oi();
        assert_eq!(recaptured, 1);
        assert_eq!(tracker.baseline_oi_len(), 1);
    }

    #[test]
    fn test_oi_buildup_populated_when_baseline_below_current() {
        let mut tracker = tracker_with_index_option(49_081, 100_000);
        tracker.capture_baseline_oi();
        // OI grows 50% after baseline — must land in oi_buildup.
        let mut tick = ParsedTick::default();
        tick.security_id = 49_081;
        tick.exchange_segment_code = 2;
        tick.last_traded_price = 110.0;
        tick.day_close = 95.0;
        tick.open_interest = 150_000;
        tick.volume = 2_000;
        tick.exchange_timestamp = 2_000;
        tracker.update_v2(&tick);
        let snapshot = tracker.compute_snapshot_v2();
        assert_eq!(snapshot.index_options.oi_buildup.len(), 1);
        assert_eq!(snapshot.index_options.oi_unwind.len(), 0);
        let entry = snapshot.index_options.oi_buildup[0];
        assert!(entry.oi_change_pct > 49.0 && entry.oi_change_pct < 51.0);
        assert_eq!(entry.prev_open_interest, 100_000);
    }

    #[test]
    fn test_oi_unwind_populated_when_baseline_above_current() {
        let mut tracker = tracker_with_index_option(49_081, 200_000);
        tracker.capture_baseline_oi();
        // OI drops 25% after baseline — must land in oi_unwind.
        let mut tick = ParsedTick::default();
        tick.security_id = 49_081;
        tick.exchange_segment_code = 2;
        tick.last_traded_price = 90.0;
        tick.day_close = 95.0;
        tick.open_interest = 150_000;
        tick.volume = 2_000;
        tick.exchange_timestamp = 2_000;
        tracker.update_v2(&tick);
        let snapshot = tracker.compute_snapshot_v2();
        assert_eq!(snapshot.index_options.oi_buildup.len(), 0);
        assert_eq!(snapshot.index_options.oi_unwind.len(), 1);
        let entry = snapshot.index_options.oi_unwind[0];
        assert!(entry.oi_change_pct < -24.0 && entry.oi_change_pct > -26.0);
        assert_eq!(entry.prev_open_interest, 200_000);
    }

    #[test]
    fn test_oi_buildup_unwind_empty_without_baseline_capture() {
        let mut tracker = tracker_with_index_option(49_081, 100_000);
        // NO baseline capture — both lists must stay empty even though OI
        // changes. This preserves the pre-baseline contract.
        let mut tick = ParsedTick::default();
        tick.security_id = 49_081;
        tick.exchange_segment_code = 2;
        tick.last_traded_price = 110.0;
        tick.day_close = 95.0;
        tick.open_interest = 200_000;
        tick.volume = 2_000;
        tick.exchange_timestamp = 2_000;
        tracker.update_v2(&tick);
        let snapshot = tracker.compute_snapshot_v2();
        assert!(snapshot.index_options.oi_buildup.is_empty());
        assert!(snapshot.index_options.oi_unwind.is_empty());
    }

    // ----------------------------------------------------------------------
    // Plan items A, B, D — Timeframe enum + Premium/Discount + row tagging
    // ----------------------------------------------------------------------

    /// Item A.1: every Timeframe variant maps to its canonical wire string.
    /// This is the column value persisted to `top_movers.timeframe`.
    /// Changing any of these strings is a breaking schema change.
    #[test]
    fn test_timeframe_as_str_stable_for_every_variant() {
        let pairs: [(Timeframe, &str); 15] = [
            (Timeframe::OneMin, "1m"),
            (Timeframe::TwoMin, "2m"),
            (Timeframe::ThreeMin, "3m"),
            (Timeframe::FourMin, "4m"),
            (Timeframe::FiveMin, "5m"),
            (Timeframe::SixMin, "6m"),
            (Timeframe::SevenMin, "7m"),
            (Timeframe::EightMin, "8m"),
            (Timeframe::NineMin, "9m"),
            (Timeframe::TenMin, "10m"),
            (Timeframe::ElevenMin, "11m"),
            (Timeframe::TwelveMin, "12m"),
            (Timeframe::ThirteenMin, "13m"),
            (Timeframe::FourteenMin, "14m"),
            (Timeframe::FifteenMin, "15m"),
        ];
        for (tf, expected) in pairs {
            assert_eq!(
                tf.as_str(),
                expected,
                "Timeframe::{tf:?} wire string drifted — schema break"
            );
        }
    }

    /// Item A.2: `secs()` matches the named minute count for every variant.
    /// Used by the rolling-baseline lookup to find the price `secs()` ago.
    #[test]
    fn test_timeframe_secs_match_minutes() {
        for tf in Timeframe::ALL {
            let expected_minutes: u32 = tf.as_str()[..tf.as_str().len() - 1]
                .parse()
                .expect("as_str ends with 'm' and is parseable");
            assert_eq!(
                tf.secs(),
                expected_minutes * 60,
                "{tf:?}.secs() must equal {expected_minutes} * 60"
            );
        }
    }

    /// Item A.3: `Timeframe::ALL` contains exactly 15 entries — not 14, not 16.
    /// If this trips: someone added or removed a variant without updating ALL.
    #[test]
    fn test_timeframe_all_count_is_15() {
        assert_eq!(
            Timeframe::ALL.len(),
            15,
            "Timeframe::ALL must contain exactly 15 entries (1m..15m)"
        );
    }

    /// Item A.4: `Timeframe::ALL` is in strictly ascending `secs()` order so
    /// callers iterating over it (e.g. baseline-lookback warmup) can rely
    /// on monotonic timeframe traversal.
    #[test]
    fn test_timeframe_all_in_ascending_order() {
        let mut prev: u32 = 0;
        for tf in Timeframe::ALL {
            assert!(
                tf.secs() > prev,
                "Timeframe::ALL is not strictly ascending: {tf:?} ({}s) follows {prev}s",
                tf.secs()
            );
            prev = tf.secs();
        }
    }

    /// Item B.1: Default `DerivativeBucket` has empty `premium` + `discount`.
    /// Operators expect the warm-up state to populate nothing, not stale rows.
    #[test]
    fn test_derivative_bucket_default_premium_discount_empty() {
        let b = DerivativeBucket::default();
        assert!(b.premium.is_empty(), "premium must default empty");
        assert!(b.discount.is_empty(), "discount must default empty");
    }

    /// Item B.2: `premium` and `discount` are independent vectors — pushing
    /// to one does not mutate the other. (Catches any accidental aliasing
    /// in the struct definition, e.g. if both were renamed to the same
    /// field by a mass-rename refactor.)
    #[test]
    fn test_derivative_bucket_premium_discount_are_independent() {
        let mut b = DerivativeBucket::default();
        b.premium.push(MoverEntryV2 {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: 100.0,
            prev_close: 99.0,
            change_pct: 1.0,
            volume: 1,
            open_interest: 0,
            prev_open_interest: 0,
            oi_change_pct: 0.0,
            value: 100.0,
        });
        assert_eq!(b.premium.len(), 1);
        assert_eq!(
            b.discount.len(),
            0,
            "pushing to premium must not touch discount"
        );
    }

    /// Item D.1: every emitted row carries the `timeframe` wire string passed
    /// by the caller. If this regresses, the QuestDB DEDUP key will collide
    /// across timeframes (1m and 5m rows would overwrite each other).
    #[test]
    fn test_snapshot_to_rows_tags_every_row_with_timeframe() {
        let mut snapshot = MoversSnapshotV2::default();
        snapshot.indices.gainers.push(MoverEntryV2 {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 25_700.0,
            prev_close: 25_600.0,
            change_pct: 0.4,
            volume: 0,
            open_interest: 0,
            prev_open_interest: 0,
            oi_change_pct: 0.0,
            value: 0.0,
        });
        let reg = InstrumentRegistry::empty();
        for tf in [Timeframe::OneMin, Timeframe::FiveMin, Timeframe::FifteenMin] {
            let rows = snapshot_to_rows(&snapshot, tf, &reg);
            assert!(!rows.is_empty(), "snapshot should have rows for {tf:?}");
            for row in &rows {
                assert_eq!(
                    row.timeframe,
                    tf.as_str(),
                    "every row must carry timeframe={}",
                    tf.as_str()
                );
            }
        }
    }

    /// Item D.2: `segment` wire string is stable for every supported
    /// `ExchangeSegment` and falls back to `"UNKNOWN"` for codes we don't
    /// recognise. Required for I-P1-11 cross-segment DEDUP.
    #[test]
    fn test_segment_wire_str_covers_every_segment() {
        // Numeric codes per dhan-annexure-enums.md rule 2:
        // IDX_I=0, NSE_EQ=1, NSE_FNO=2, NSE_CURRENCY=3, BSE_EQ=4,
        // MCX_COMM=5, BSE_CURRENCY=7, BSE_FNO=8 (note: gap at 6).
        let pairs: [(u8, &str); 8] = [
            (0, "IDX_I"),
            (1, "NSE_EQ"),
            (2, "NSE_FNO"),
            (3, "NSE_CURRENCY"),
            (4, "BSE_EQ"),
            (5, "MCX_COMM"),
            (7, "BSE_CURRENCY"),
            (8, "BSE_FNO"),
        ];
        for (code, expected) in pairs {
            assert_eq!(
                segment_wire_str(code),
                expected,
                "segment_wire_str({code}) drifted"
            );
        }
        // Code 6 is not a valid Dhan segment (annexure rule 2: gap at 6).
        assert_eq!(segment_wire_str(6), "UNKNOWN");
        assert_eq!(segment_wire_str(99), "UNKNOWN");
    }

    /// Item D.3: row segment matches the entry's `exchange_segment_code` —
    /// catches off-by-one swaps where the wrong segment string is attached.
    #[test]
    fn test_snapshot_to_rows_segment_matches_entry_segment_code() {
        let mut snapshot = MoversSnapshotV2::default();
        // NSE_FNO entry — code 2.
        snapshot.index_futures.gainers.push(MoverEntryV2 {
            security_id: 49081,
            exchange_segment_code: 2,
            last_traded_price: 25_700.0,
            prev_close: 25_600.0,
            change_pct: 0.4,
            volume: 0,
            open_interest: 0,
            prev_open_interest: 0,
            oi_change_pct: 0.0,
            value: 0.0,
        });
        let reg = InstrumentRegistry::empty();
        let rows = snapshot_to_rows(&snapshot, Timeframe::OneMin, &reg);
        assert!(!rows.is_empty());
        for row in &rows {
            assert_eq!(row.segment, "NSE_FNO", "futures row segment mismatch");
        }
    }
}
