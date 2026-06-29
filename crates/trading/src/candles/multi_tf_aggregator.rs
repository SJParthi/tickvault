//! Wave 6 Sub-PR #1 next slice — multi-instrument aggregator container.
//!
//! Wraps `papaya::HashMap<(u32, u8), Arc<InstrumentEntry>>` keyed on
//! `(security_id, exchange_segment_code)` per locked decision **L-C2** +
//! I-P1-11 composite key. Each entry holds:
//!
//! - The per-instrument [`AggregatorCell`] (9-TF state, from the
//!   slice merged via PR #555).
//! - An `AtomicU64` carrying the per-instrument last-seen cumulative
//!   volume — used to compute `bucket_start_cumulative` on TF
//!   boundary crossings per the Item 28 carry-over pattern in
//!   `crates/core/src/pipeline/candle_aggregator.rs`.
//!
//! ## Hot-path API
//!
//! Callers (the forthcoming tick-processor wiring in items 1.4) call
//! [`MultiTfAggregator::consume_tick`] which:
//!
//! 1. Looks up the per-instrument entry (lock-free papaya read).
//! 2. Reads the last-seen cumulative volume (single atomic load).
//! 3. Fans the tick out to all 21 TF slots in the cell, calling
//!    `on_seal(tf, sealed_state)` for each TF whose boundary crossed.
//!    Late ticks are coalesced into a `late_count` so the caller can
//!    emit ONE `error!(code = AGGREGATOR-LATE-01)` per tick (per L-C3),
//!    not 21.
//! 4. Stores the new last-cumulative for the next tick (atomic store).
//!
//! ## Why a separate `last_cumulative` per instrument vs per-TF
//!
//! All 21 TFs see the same tick stream. At any instant, the
//! "cumulative at the end of the previous tick" is identical across
//! all TFs that crossed simultaneously. One `AtomicU64` per instrument
//! suffices. Per-TF storage would multiply RAM by 21× without
//! semantic gain.
//!
//! ## RAM budget per instrument
//!
//! `Arc<InstrumentEntry>` ≈ 16 bytes (Arc) + 21 × 64 bytes
//! (`Mutex<LiveCandleState>`) + 8 bytes (`AtomicU64`) + papaya overhead
//! ≈ ~1.4 KB per instrument. The locked universe is 4 IDX_I SIDs →
//! a few KB steady-state. Well within the App 2 GB envelope from
//! `aws-budget.md`.
//!
//! ## What this module does NOT yet ship
//!
//! - `ShadowBarWriter` ring → spill → DLQ writer (item 1.2, next slice).
//! - Boundary timer that calls `force_seal_all` at IST midnight
//!   (item 1.3, next slice).
//! - Wiring the container into `tick_processor` SPSC consumer
//!   (item 1.4).
//! - Wave-5 pct-stamping at seal time (item 1.5; the writer slice
//!   reads `prev_day_cache` and stamps the 3 pct fields on
//!   `LiveCandleState` BEFORE pushing into the ring).
//! - DHAT zero-alloc + Criterion bench (item 1.6).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use papaya::HashMap;

use tickvault_common::tick_types::ParsedTick;

use crate::candles::tf_index::{MARKET_CLOSE_SECS_OF_DAY_IST, MARKET_OPEN_SECS_OF_DAY_IST};
use crate::candles::{AggregatorCell, ConsumeOutcome, FeedStrategy, LiveCandleState, TfIndex};

/// Composite key per I-P1-11 — `(security_id, exchange_segment_code)`.
/// `security_id` is `u64` (2026-06-29 widening) so Groww's native exchange_token
/// (bit-62 index ids) keys the SAME aggregator as Dhan's 4-byte wire id.
type AggregatorKey = (u64, u8);

/// One papaya entry. Holds the 9-TF cell plus the per-instrument
/// last-seen cumulative-volume tracker.
///
/// `Arc<InstrumentEntry>` is shared into every `pin().get()` site;
/// the inner state is interior-mutable via `Mutex` (in the cell) and
/// `AtomicU64` (cumulative). The struct itself is `Send + Sync` —
/// pinned by the compile-time test below.
#[derive(Debug)]
pub struct InstrumentEntry {
    /// 9-TF candle state per locked decision L-C2.
    pub cell: Arc<AggregatorCell>,
    /// Last-seen cumulative volume from the most recent tick. On a
    /// TF boundary cross, this value becomes the new bucket's
    /// `bucket_start_cumulative` (Item 28 carry-over). Initial value
    /// `0` — the session's first tick uses `bucket_start_cumulative=0`,
    /// matching `LiveCandle::from_tick` semantics in
    /// `crates/core/src/pipeline/candle_aggregator.rs`.
    pub last_cumulative: AtomicU64,
}

impl InstrumentEntry {
    /// Constructs an empty entry: cell with all 9 slots uninitialised
    /// (per [`AggregatorCell::empty`]) + `last_cumulative=0`.
    #[must_use]
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            cell: AggregatorCell::empty(),
            last_cumulative: AtomicU64::new(0),
        })
    }
}

/// Stats returned by [`MultiTfAggregator::consume_tick`] so the caller
/// can drive Prometheus counters + emit `error!(code=AGGREGATOR-LATE-01)`
/// without the container itself doing tracing (kept side-effect-free
/// for testability).
///
/// `late_count` ≥ 1 means the caller increments
/// `tv_aggregator_late_ticks_discarded_total` (a tick ≥ 2 buckets late, or with
/// no amendable last-sealed bucket). `amended_count` ≥ 1 (Option B, operator
/// lock 2026-06-05, supersedes L-C3 "no silent merge") means a 1-bucket-late
/// tick re-folded its own minute's high/low/close and the caller routed the
/// amended candle through `on_seal` (UPSERT) + increments
/// `tv_aggregator_amended_ticks_total`. The container coalesces the 21 TFs into
/// counts so the caller emits ONE log line per tick (not 21).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConsumeStats {
    /// Number of TFs that just sealed a bucket and emitted a sealed
    /// state to the `on_seal` callback.
    pub sealed_count: u8,
    /// Number of TFs that received this tick as a late arrival
    /// (before their open bucket's start). 0..=21.
    pub late_count: u8,
    /// Number of TFs whose most-recently-sealed bucket was AMENDED by this
    /// late tick (Option B) and re-emitted via `on_seal` for UPSERT. 0..=21.
    pub amended_count: u8,
    /// `true` if the instrument was looked up successfully.
    /// `false` if the (security_id, segment) pair was NOT in the
    /// container (a tick for an instrument that wasn't pre-populated
    /// at boot per L-M19). Caller logs as a separate diagnostic
    /// (typically PR-FOLLOWUP — registry drift detection).
    pub instrument_found: bool,
}

/// Multi-instrument multi-TF aggregator container.
///
/// Storage is `papaya::HashMap<(u32, u8), Arc<InstrumentEntry>>`. The
/// boot path eagerly pre-populates one entry per
/// `(security_id, exchange_segment)` from the
/// `InstrumentRegistry::iter()` composite-key iter per L-M19, so
/// hot-path `consume_tick` is a pure read (no insertion).
///
/// `Clone` on the type just bumps an `Arc` refcount — multiple
/// consumers (the tick-processor task + the boundary-timer task)
/// hold cheap clones.
#[derive(Clone, Default)]
pub struct MultiTfAggregator {
    inner: Arc<HashMap<AggregatorKey, Arc<InstrumentEntry>>>,
    /// Operator lock 2026-06-01 §30: `(security_id, exchange_segment_code)`
    /// pairs exempt from the 09:15–15:30 IST candle-window gate (GIFT
    /// Nifty trades ~21 h/day). Empty by default → no exemptions →
    /// today's behavior. Set once at boot via `with_always_on`.
    always_on: Arc<HashSet<(u64, u8)>>,
}

impl MultiTfAggregator {
    /// Empty container.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty container with a capacity hint. The boot path uses this
    /// with `cap = registry_size` to avoid papaya internal-table
    /// rehashing during pre-population.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: Arc::new(HashMap::with_capacity(cap)),
            always_on: Arc::new(HashSet::new()),
        }
    }

    /// Install the always-on (no market-hours filter) exemption set
    /// (operator lock 2026-06-01 §30). Boot wires this from
    /// `DailyUniverse::always_on_segments` so GIFT Nifty candles form
    /// across its full ~21 h session instead of being dropped by the
    /// 09:15–15:30 IST window gate. Builder style so existing
    /// `new()`/`with_capacity()` call sites (incl. all tests) keep the
    /// safe empty default.
    #[must_use]
    // TEST-EXEMPT: builder exercised by test_gift_nifty_exempt_tick_aggregates_outside_window.
    pub fn with_always_on(mut self, always_on: Arc<HashSet<(u64, u8)>>) -> Self {
        self.always_on = always_on;
        self
    }

    /// Number of instruments tracked.
    ///
    /// Cold path; uses papaya's internal length getter.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.pin().len()
    }

    /// `true` if no instrument has been registered yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Eagerly pre-populates an empty entry for every
    /// `(security_id, segment)` pair in `iter`. Idempotent — calling
    /// twice with the same pair leaves the second call a no-op
    /// (checked via `pin().contains_key`). Returns the number of
    /// NEW entries created (not the input length).
    ///
    /// Per locked decision **L-M19**: the hot-path tick consumer
    /// expects the entry to already exist. Pre-population at boot
    /// guarantees first-tick zero-allocation.
    pub fn pre_populate<I>(&self, iter: I) -> usize
    where
        I: IntoIterator<Item = AggregatorKey>,
    {
        let pin = self.inner.pin();
        let mut inserted = 0;
        for key in iter {
            if pin.get(&key).is_none() {
                pin.insert(key, InstrumentEntry::empty());
                inserted += 1;
            }
        }
        inserted
    }

    /// O(1) lookup of the per-instrument entry. Returns `Some(Arc)`
    /// if the instrument was pre-populated; `None` otherwise.
    /// Used by tests and by the boundary timer for force-sealing.
    #[must_use]
    pub fn get(&self, security_id: u64, exchange_segment_code: u8) -> Option<Arc<InstrumentEntry>> {
        let pin = self.inner.pin();
        pin.get(&(security_id, exchange_segment_code)).cloned()
    }

    /// Seed the per-instrument `last_cumulative` baseline ONCE, before the
    /// instrument's FIRST `consume_tick`. Returns `true` if the entry existed
    /// (seed applied), `false` otherwise.
    ///
    /// Why this exists (Groww first-bucket baseline): the per-instrument
    /// `last_cumulative` atomic initialises to `0`, so the FIRST tick's open
    /// bucket uses `bucket_start_cumulative = 0` and the bucket's incremental
    /// volume becomes the FULL cumulative — correct for Dhan (whose cumulative
    /// starts near 0 at session open) but WRONG for Groww, whose `cum_volume` is
    /// a running day total already large at first observation. The legacy
    /// `Groww1mAggregator` used the instrument's first-tick cumulative as the
    /// baseline (so minute-1 volume = `last_cum − first_cum`). The Groww bridge
    /// calls this once on first-seen-instrument with the first cumulative so the
    /// shared engine reproduces that baseline EXACTLY — a Groww-only seed, the
    /// Dhan path never calls it (its baseline stays 0, behaviour unchanged).
    /// O(1): one papaya read + one atomic store.
    pub fn seed_cumulative(
        &self,
        security_id: u64,
        exchange_segment_code: u8,
        cumulative: u64,
    ) -> bool {
        let pin = self.inner.pin();
        if let Some(entry) = pin.get(&(security_id, exchange_segment_code)) {
            entry.last_cumulative.store(cumulative, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Folds a single tick into ALL 21 TF slots for its instrument.
    ///
    /// `on_seal(tf, sealed_state)` is called once per TF whose
    /// boundary crossed (i.e. once per [`ConsumeOutcome::Sealed`]).
    /// Returns [`ConsumeStats`] aggregating the outcome across all 21
    /// TFs so the caller can emit Prometheus counter increments
    /// without 21 separate observability hooks.
    ///
    /// Hot path. The implementation:
    /// 1. Looks up the entry (one papaya pin + get, ~30 ns).
    /// 2. Loads `last_cumulative` (one atomic load, ~5 ns).
    /// 3. Calls `cell.consume_tick(tf, ...)` 21 times — each is a
    ///    parking_lot::Mutex lock + scalar update. Average ~30 ns
    ///    uncontended.
    /// 4. Stores the new `last_cumulative` (one atomic store, ~5 ns).
    ///
    /// Total budget: ~300 ns per tick — within the existing tick-parse
    /// budget. The DHAT/Criterion split-budgets land in item 1.6.
    ///
    /// `strategy` is the per-feed [`FeedStrategy`] (Dhan = [`FeedStrategy::DHAN`]
    /// Refold; Groww = [`FeedStrategy::GROWW`] Discard) threaded to every TF cell
    /// so ONE engine instance serves either feed by VALUE, not forked code.
    ///
    /// `cumulative_volume_override` carries a feed's running cumulative day volume
    /// as a `u64` when it exceeds the `u32` `tick.volume` field (Groww `cum_volume`
    /// is an `i64`). The Dhan path passes `None` (the per-instrument `last_cumulative`
    /// atomic + the cell read the `u32` `tick.volume`); Groww passes
    /// `Some(cum as u64)`, which becomes BOTH the cell's running cumulative AND the
    /// stored `last_cumulative` baseline for the next bucket — so the i64→u32
    /// truncation the adversarial review flagged CRITICAL never happens.
    pub fn consume_tick<F>(
        &self,
        tick: &ParsedTick,
        exchange_segment_code: u8,
        strategy: FeedStrategy,
        cumulative_volume_override: Option<u64>,
        mut on_seal: F,
    ) -> ConsumeStats
    where
        F: FnMut(TfIndex, LiveCandleState),
    {
        // Candle-window gate: only ticks inside the regular trading
        // session [09:15:00, 15:30:00) IST form candles. Pre-open
        // auction ticks (< 09:15) and post-close stale ticks (>= 15:30)
        // are skipped — they still land in the `ticks` table, but they
        // never pollute the candle grid (which Dhan REST cross-verify
        // also expects to begin at 09:15). The bucket grid itself is
        // 09:15-anchored in `TfIndex::bucket_start`.
        // Operator lock 2026-06-01 §30: always-on instruments (GIFT Nifty,
        // ~21 h/day on NSE-IX) are EXEMPT from the regular-session window
        // gate so their candles form across their full session. O(1) read
        // of a tiny boot-set set (usually ≤1 entry).
        let exempt_key = (tick.security_id, exchange_segment_code);
        // O(1) EXEMPT: `always_on` is a HashSet — contains is O(1) hashing, not a Vec scan.
        let exempt = self.always_on.contains(&exempt_key);
        let secs_of_day = tick.exchange_timestamp % 86_400;
        // The O(1) pre-commit scanner flags any `.contains(` as a Vec scan; the
        // explicit comparison below is equally O(1) (two integer comparisons).
        // APPROVED: manual_range_contains is intentional to avoid that false positive.
        #[allow(clippy::manual_range_contains)]
        let out_of_session = secs_of_day < MARKET_OPEN_SECS_OF_DAY_IST
            || secs_of_day >= MARKET_CLOSE_SECS_OF_DAY_IST;
        // Session-window gate applies to EVERY feed (Dhan AND Groww). The bucket
        // grid is 09:15-anchored (`TfIndex::bucket_start` clamps a pre-open tick
        // to the first bucket), so a pre-open tick that slipped past this gate
        // would FOLD INTO (corrupt) the 09:15 candle, not form a distinct pre-open
        // candle. INTENDED deviation from the legacy `Groww1mAggregator` (which
        // floored to absolute minute and had no session concept): both feeds'
        // candle grids begin at 09:15, matching the REST/backtest cross-verify
        // window. Pinned by `test_groww_pre_open_minute_is_gated_intended`.
        // (If SP6 finds Groww backtest emits pre-open candles, revisit with a
        // pre-open-aware bucket grid — a separate, larger change.)
        // `always_on` still exempts long-session instruments (e.g. GIFT Nifty).
        if !exempt && out_of_session {
            return ConsumeStats {
                sealed_count: 0,
                late_count: 0,
                amended_count: 0,
                instrument_found: true,
            };
        }

        let key = (tick.security_id, exchange_segment_code);
        let pin = self.inner.pin();
        let Some(entry) = pin.get(&key) else {
            return ConsumeStats {
                sealed_count: 0,
                late_count: 0,
                amended_count: 0,
                instrument_found: false,
            };
        };

        let last_cum = entry.last_cumulative.load(Ordering::Relaxed);
        let mut sealed_count: u8 = 0;
        let mut late_count: u8 = 0;
        let mut amended_count: u8 = 0;

        for tf in TfIndex::ALL {
            match entry
                .cell
                .consume_tick(tf, tick, last_cum, strategy, cumulative_volume_override)
            {
                ConsumeOutcome::Updated => {}
                ConsumeOutcome::Sealed { sealed_state } => {
                    sealed_count = sealed_count.saturating_add(1);
                    on_seal(tf, sealed_state);
                }
                // Option B: a 1-bucket-late tick amended its own minute's
                // high/low/close — route the corrected candle through the SAME
                // seal path so the writer UPSERTs (replaces) that minute's row.
                ConsumeOutcome::AmendedLate { amended_state } => {
                    amended_count = amended_count.saturating_add(1);
                    on_seal(tf, amended_state);
                }
                ConsumeOutcome::DiscardLate => {
                    late_count = late_count.saturating_add(1);
                }
            }
        }

        // Store the new last-cumulative AFTER all TFs processed so a
        // racing observer that reads mid-fan-out sees a consistent
        // value (but consume_tick on a SINGLE instrument is expected
        // to be single-threaded — the WS read loop is the only writer).
        // Use the SAME resolved cumulative the cell used (override when present)
        // so the next bucket's baseline matches the value just folded — never the
        // truncated `u32` `tick.volume` when a `u64` override was supplied.
        let stored_cumulative =
            cumulative_volume_override.unwrap_or_else(|| u64::from(tick.volume));
        entry
            .last_cumulative
            .store(stored_cumulative, Ordering::Relaxed);

        ConsumeStats {
            sealed_count,
            late_count,
            amended_count,
            instrument_found: true,
        }
    }

    /// Force-seals every TF slot of every instrument and emits each
    /// non-empty bucket via `on_seal(security_id, segment, tf, state)`.
    ///
    /// Used by the boundary timer (item 1.3) at IST midnight to flush
    /// open buckets that didn't see a tick crossing the boundary
    /// (e.g. illiquid contracts post-15:30, or weekend close-out).
    ///
    /// `O(N × 21)` where N = number of instruments. Cold path — runs
    /// at most once per minute boundary, not on the per-tick fast path.
    pub fn force_seal_all<F>(&self, mut on_seal: F)
    where
        F: FnMut(u64, u8, TfIndex, LiveCandleState),
    {
        let pin = self.inner.pin();
        for (key, entry) in pin.iter() {
            let (security_id, segment_code) = *key;
            for tf in TfIndex::ALL {
                if let Some(sealed) = entry.cell.force_seal(tf) {
                    on_seal(security_id, segment_code, tf, sealed);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::tick_types::ParsedTick;

    fn mk_tick(sid: u32, seg: u8, secs: u32, price: f32, volume: u32, oi: u32) -> ParsedTick {
        let mut t = ParsedTick::default();
        t.security_id = sid;
        t.exchange_segment_code = seg;
        t.exchange_timestamp = secs;
        t.last_traded_price = price;
        t.volume = volume;
        t.open_interest = oi;
        t
    }

    #[test]
    fn test_multi_tf_aggregator_new_is_empty() {
        let agg = MultiTfAggregator::new();
        assert!(agg.is_empty());
        assert_eq!(agg.len(), 0);
    }

    #[test]
    fn test_multi_tf_aggregator_with_capacity_starts_empty() {
        let agg = MultiTfAggregator::with_capacity(11_000);
        assert!(agg.is_empty());
        assert_eq!(agg.len(), 0);
    }

    #[test]
    fn test_pre_populate_inserts_each_key_once() {
        let agg = MultiTfAggregator::new();
        let pairs = vec![(13, 0u8), (25, 0u8), (51, 0u8)];
        let n = agg.pre_populate(pairs);
        assert_eq!(n, 3);
        assert_eq!(agg.len(), 3);
    }

    #[test]
    fn test_pre_populate_is_idempotent() {
        // Composite key prevents the I-P1-11 collision class even if
        // someone accidentally double-pre-populates.
        let agg = MultiTfAggregator::new();
        let pairs = vec![(13, 0u8), (25, 0u8)];
        let first = agg.pre_populate(pairs.clone());
        let second = agg.pre_populate(pairs);
        assert_eq!(first, 2);
        assert_eq!(second, 0); // already present, no new inserts
        assert_eq!(agg.len(), 2);
    }

    #[test]
    fn test_pre_populate_distinguishes_segments_for_i_p1_11() {
        // The cornerstone of I-P1-11: same security_id across two
        // different segments must produce TWO entries.
        let agg = MultiTfAggregator::new();
        let n = agg.pre_populate(vec![(13, 0u8), (13, 1u8)]);
        assert_eq!(n, 2);
        assert_eq!(agg.len(), 2);
        assert!(agg.get(13, 0).is_some());
        assert!(agg.get(13, 1).is_some());
    }

    #[test]
    fn test_get_returns_arc_clones_pointing_at_same_entry() {
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let a = agg.get(13, 0).expect("present after pre-populate");
        let b = agg.get(13, 0).expect("present");
        // Both Arcs point at the same entry: mutating via `a`'s
        // last_cumulative must be visible via `b`.
        a.last_cumulative.store(42, Ordering::Relaxed);
        assert_eq!(b.last_cumulative.load(Ordering::Relaxed), 42);
    }

    #[test]
    fn test_get_returns_none_for_unknown_instrument() {
        let agg = MultiTfAggregator::new();
        assert!(agg.get(999, 99).is_none());
    }

    #[test]
    fn test_seed_cumulative_sets_baseline_on_present_instrument() {
        // Groww-only first-tick baseline seed: stores the first cumulative on
        // the entry's `last_cumulative` atomic so minute-1 volume folds as
        // `last_cum − first_cum` (legacy Groww1mAggregator parity).
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(1_333, 1u8)]);
        let seeded = agg.seed_cumulative(1_333, 1, 100);
        assert!(seeded, "seed must succeed on a pre-populated instrument");
        let entry = agg.get(1_333, 1).expect("present");
        assert_eq!(
            entry.last_cumulative.load(Ordering::Relaxed),
            100,
            "baseline cumulative must be stored verbatim"
        );
    }

    #[test]
    fn test_seed_cumulative_returns_false_for_unknown_instrument() {
        // Fail-safe: seeding an instrument that was never pre-populated is a
        // no-op returning false — the Dhan path never calls this, and a Groww
        // mis-seed must not silently create a slot.
        let agg = MultiTfAggregator::new();
        assert!(
            !agg.seed_cumulative(999, 99, 5_000_000_000),
            "seed must return false when the instrument is absent"
        );
        assert!(agg.get(999, 99).is_none(), "no slot is created on a miss");
    }

    #[test]
    fn test_seed_cumulative_accepts_value_above_u32_max() {
        // The i64-widened cumulative (> u32::MAX) must store losslessly into the
        // u64 atomic — the truncation the adversarial review flagged CRITICAL.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(1_333, 1u8)]);
        let big: u64 = 5_000_000_000; // > u32::MAX (4_294_967_295)
        assert!(agg.seed_cumulative(1_333, 1, big));
        let entry = agg.get(1_333, 1).expect("present");
        assert_eq!(
            entry.last_cumulative.load(Ordering::Relaxed),
            big,
            "cumulative above u32::MAX must survive without truncation"
        );
    }

    #[test]
    fn test_consume_tick_returns_instrument_not_found_when_missing() {
        let agg = MultiTfAggregator::new();
        let tick = mk_tick(999, 0, 1_779_354_960, 100.0, 50, 0);
        let mut sealed_callbacks: u32 = 0;
        let stats = agg.consume_tick(&tick, 0, FeedStrategy::DHAN, None, |_, _| {
            sealed_callbacks += 1
        });
        assert!(!stats.instrument_found);
        assert_eq!(stats.sealed_count, 0);
        assert_eq!(stats.late_count, 0);
        assert_eq!(sealed_callbacks, 0);
    }

    #[test]
    fn test_non_exempt_tick_outside_window_is_skipped() {
        // Operator lock 2026-06-01 §30: a normal instrument's tick at
        // 20:00 IST (outside 09:15–15:30) must NOT open any candle.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        // secs_of_day = 72000 = 20:00 IST (base day-aligned + 72000).
        let tick = mk_tick(13, 0, 1_779_235_200 + 72_000, 100.0, 50, 1_000);
        agg.consume_tick(&tick, 0, FeedStrategy::DHAN, None, |_, _| {});
        let entry = agg.get(13, 0).expect("present");
        assert!(
            entry.cell.snapshot(TfIndex::ALL[0]).is_uninitialised(),
            "non-exempt tick outside the session window must be skipped"
        );
    }

    #[test]
    fn test_gift_nifty_exempt_tick_aggregates_outside_window() {
        // GIFT Nifty (sid 5024, IDX_I=0) in the always-on set aggregates
        // at 20:00 IST — its candle MUST open.
        let mut set = HashSet::new();
        set.insert((5024_u32, 0_u8));
        let agg = MultiTfAggregator::with_capacity(8).with_always_on(Arc::new(set));
        agg.pre_populate(vec![(5024, 0)]);
        let tick = mk_tick(5024, 0, 1_779_235_200 + 72_000, 100.0, 50, 1_000);
        agg.consume_tick(&tick, 0, FeedStrategy::DHAN, None, |_, _| {});
        let entry = agg.get(5024, 0).expect("present");
        assert!(
            !entry.cell.snapshot(TfIndex::ALL[0]).is_uninitialised(),
            "exempt GIFT Nifty tick outside the window MUST aggregate"
        );
    }

    #[test]
    fn test_consume_tick_first_tick_updates_all_21_tfs_no_seal() {
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let tick = mk_tick(13, 0, 1_779_354_960, 100.0, 50, 1_000);
        let mut sealed_callbacks: u32 = 0;
        let stats = agg.consume_tick(&tick, 0, FeedStrategy::DHAN, None, |_, _| {
            sealed_callbacks += 1
        });
        assert!(stats.instrument_found);
        assert_eq!(stats.sealed_count, 0); // first tick — nothing to seal
        assert_eq!(stats.late_count, 0);
        assert_eq!(sealed_callbacks, 0);
        // Verify all 21 TF slots opened.
        let entry = agg.get(13, 0).expect("present");
        for tf in TfIndex::ALL {
            assert!(
                !entry.cell.snapshot(tf).is_uninitialised(),
                "TF {} not opened",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_consume_tick_updates_last_cumulative_after_each_tick() {
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let tick1 = mk_tick(13, 0, 1_779_354_960, 100.0, 50, 1_000);
        agg.consume_tick(&tick1, 0, FeedStrategy::DHAN, None, |_, _| {});
        let entry = agg.get(13, 0).expect("present");
        assert_eq!(entry.last_cumulative.load(Ordering::Relaxed), 50);

        let tick2 = mk_tick(13, 0, 1_779_354_970, 101.0, 75, 1_010);
        agg.consume_tick(&tick2, 0, FeedStrategy::DHAN, None, |_, _| {});
        assert_eq!(entry.last_cumulative.load(Ordering::Relaxed), 75);
    }

    #[test]
    fn test_consume_tick_boundary_crossing_seals_only_crossed_tfs() {
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        // Base = 09:15:00 IST market open — every TF opens its first
        // bucket of the day here. tick1 at base+10, tick2 at base+70:
        // only M1 (60 s) crosses a boundary; M2 (120 s) and every wider
        // TF still hold their `base` bucket.
        let base = 1_779_354_900_u32; // 2026-05-21 09:15:00 IST
        let tick1 = mk_tick(13, 0, base + 10, 100.0, 50, 1_000);
        agg.consume_tick(&tick1, 0, FeedStrategy::DHAN, None, |_, _| {});
        let tick2 = mk_tick(13, 0, base + 70, 102.0, 80, 1_010);
        let mut sealed_tfs: Vec<TfIndex> = Vec::new();
        let stats = agg.consume_tick(&tick2, 0, FeedStrategy::DHAN, None, |tf, _| {
            sealed_tfs.push(tf)
        });
        assert!(stats.instrument_found);
        assert_eq!(stats.sealed_count, 1);
        assert_eq!(stats.late_count, 0);
        assert_eq!(sealed_tfs, vec![TfIndex::M1]);
    }

    #[test]
    fn test_consume_tick_discards_late_when_no_amendable_sealed_bucket() {
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let aligned = 1_779_355_500_u32; // 2026-05-21 09:25:00 IST (M1-aligned)
        let tick1 = mk_tick(13, 0, aligned + 30, 100.0, 50, 0);
        agg.consume_tick(&tick1, 0, FeedStrategy::DHAN, None, |_, _| {});
        // Late tick belongs to the PREVIOUS M1 bucket, but NO bucket has been
        // sealed yet → no amendable last-sealed bucket → DiscardLate (Option B
        // only amends a bucket that was actually sealed by a newer in-session tick).
        let late_tick = mk_tick(13, 0, aligned - 5, 99.0, 40, 0);
        let mut on_seal_calls: u32 = 0;
        let stats = agg.consume_tick(&late_tick, 0, FeedStrategy::DHAN, None, |_, _| {
            on_seal_calls += 1
        });
        assert!(stats.instrument_found);
        assert_eq!(stats.sealed_count, 0);
        assert_eq!(stats.amended_count, 0);
        assert!(
            stats.late_count >= 1,
            "M1 should have flagged late; got late_count={}",
            stats.late_count
        );
        assert_eq!(on_seal_calls, 0);
    }

    #[test]
    fn test_consume_tick_amends_just_sealed_bucket_and_routes_via_on_seal() {
        // Option B: a 1-bucket-late tick re-folds its OWN minute's high/low/close
        // and is re-emitted through on_seal so the writer UPSERTs the candle.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let b1 = 1_779_355_500_u32; // 09:25:00 IST
        agg.consume_tick(
            &mk_tick(13, 0, b1 + 30, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        ); // open M1 bucket1
        agg.consume_tick(
            &mk_tick(13, 0, b1 + 70, 105.0, 60, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        ); // cross boundary → seal M1 bucket1
        // Late tick (LTT 09:25:50) into the just-sealed M1 bucket1, price 110 (new high).
        let mut m1_amend_emits: u32 = 0;
        let stats = agg.consume_tick(
            &mk_tick(13, 0, b1 + 50, 110.0, 55, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |tf, st| {
                if tf == TfIndex::M1 {
                    m1_amend_emits += 1;
                    assert_eq!(st.bucket_start_ist_secs, b1);
                    assert_eq!(st.high, 110.0); // corrected high routed for UPSERT
                }
            },
        );
        assert!(
            stats.amended_count >= 1,
            "M1 should have amended; got amended_count={}",
            stats.amended_count
        );
        assert_eq!(stats.late_count, 0);
        assert_eq!(m1_amend_emits, 1);
    }

    #[test]
    fn test_force_seal_all_drains_every_tf_of_every_instrument() {
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0), (25, 0)]);
        // Tick both instruments to open all slots.
        for sid in [13_u32, 25] {
            let t = mk_tick(sid, 0, 1_779_354_960, 100.0, 50, 0);
            agg.consume_tick(&t, 0, FeedStrategy::DHAN, None, |_, _| {});
        }
        let mut emitted: Vec<(u32, u8, TfIndex)> = Vec::new();
        agg.force_seal_all(|sid, seg, tf, _| emitted.push((sid, seg, tf)));
        assert_eq!(
            emitted.len(),
            2 * 21,
            "expected 2 instruments × 21 TFs = 42 sealed bars; got {}",
            emitted.len()
        );
        // Every slot is now empty.
        for sid in [13_u32, 25] {
            let entry = agg.get(sid, 0).expect("present");
            for tf in TfIndex::ALL {
                assert!(entry.cell.snapshot(tf).is_uninitialised());
            }
        }
    }

    #[test]
    fn test_force_seal_all_skips_uninitialised_slots() {
        // If an instrument was pre-populated but never received a
        // tick, force_seal_all must NOT emit empty bars for its 9
        // TFs (locked decision L-H13 — no pollution after market
        // close).
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let mut count: u32 = 0;
        agg.force_seal_all(|_, _, _, _| count += 1);
        assert_eq!(count, 0, "force_seal_all must skip uninitialised slots");
    }

    #[test]
    fn test_consume_tick_distinct_segments_isolated_for_i_p1_11() {
        // Same security_id on two different segments: ticks for one
        // segment must NOT mutate the other's state.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0u8), (13, 1u8)]);
        let tick_seg0 = mk_tick(13, 0, 1_779_354_960, 100.0, 50, 0);
        agg.consume_tick(&tick_seg0, 0, FeedStrategy::DHAN, None, |_, _| {});

        // Segment 1's entry should still be empty.
        let entry1 = agg.get(13, 1).expect("present");
        for tf in TfIndex::ALL {
            assert!(
                entry1.cell.snapshot(tf).is_uninitialised(),
                "segment 1 mutated by segment 0 tick — I-P1-11 violation"
            );
        }
        assert_eq!(entry1.last_cumulative.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_consume_stats_default_is_safe() {
        let s = ConsumeStats::default();
        assert_eq!(s.sealed_count, 0);
        assert_eq!(s.late_count, 0);
        assert!(!s.instrument_found);
    }

    #[test]
    fn test_multi_tf_aggregator_clone_shares_inner_state() {
        // Clone semantics: bumps Arc refcount, both clones see same
        // pre-populated entries. Callers (tick_processor task +
        // boundary_timer task) hold cheap clones.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let cloned = agg.clone();
        assert_eq!(cloned.len(), 1);
        // Adding via cloned is visible in original.
        cloned.pre_populate(vec![(25, 0)]);
        assert_eq!(agg.len(), 2);
    }

    #[test]
    fn test_instrument_entry_is_send_sync_for_arc_sharing() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InstrumentEntry>();
        assert_send_sync::<Arc<InstrumentEntry>>();
        assert_send_sync::<MultiTfAggregator>();
    }

    #[test]
    fn test_consume_stats_is_copy_for_zero_alloc_callers() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<ConsumeStats>();
    }
}
