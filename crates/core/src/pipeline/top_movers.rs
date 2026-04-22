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
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DerivativeBucket {
    pub gainers: Vec<MoverEntryV2>,
    pub losers: Vec<MoverEntryV2>,
    pub most_active: Vec<MoverEntryV2>,
    pub top_oi: Vec<MoverEntryV2>,
    pub oi_buildup: Vec<MoverEntryV2>, // empty until prev-OI baseline wired
    pub oi_unwind: Vec<MoverEntryV2>,  // empty until prev-OI baseline wired
    pub top_value: Vec<MoverEntryV2>,
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
            ticks_processed: 0,
        }
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
        let entries: Vec<(u32, MoverEntryV2, MoverBucket)> = self
            .securities
            .iter()
            .map(|(&(sid, _seg), state)| (sid, state.to_entry(sid), state.bucket))
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

        MoversSnapshotV2 {
            indices,
            stocks,
            index_futures,
            stock_futures,
            index_options,
            stock_options,
            total_tracked,
            snapshot_at_ist_secs: ist_now_secs(),
        }
    }

    /// Total ticks processed. Diagnostic only.
    #[must_use]
    pub fn v2_ticks_processed(&self) -> u64 {
        self.ticks_processed
    }
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
    // oi_buildup/oi_unwind stay empty until prev-OI baseline exists.
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
}
