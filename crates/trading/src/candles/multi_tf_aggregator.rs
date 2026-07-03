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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use papaya::HashMap;

use tickvault_common::tick_types::ParsedTick;

use crate::candles::tf_index::{MARKET_CLOSE_SECS_OF_DAY_IST, MARKET_OPEN_SECS_OF_DAY_IST};
use crate::candles::{AggregatorCell, ConsumeOutcome, FeedStrategy, LiveCandleState, TfIndex};

/// Dhan allowed-lateness margin (seconds) subtracted from the Dhan feed's
/// event-time watermark before the catch-up scan (BOUNDARY-01, 2026-07-03):
/// `cutoff = min(watermark − margin, now_ist)`, and
/// [`AggregatorCell::catch_up_seal`] seals ONLY buckets whose end ≤ cutoff.
/// The 5 s margin absorbs sub-second out-of-order delivery around the
/// boundary (the same lateness class Dhan's Option B amend exists for) so a
/// bucket is never sealed while its final ticks are still plausibly in
/// flight. Dhan's WS broadcast delivers in near-capture order, so 5 s is
/// generous there.
pub const CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN: u32 = 5;

/// Groww allowed-lateness margin (seconds) — DELIBERATELY wider than Dhan's
/// (per-feed margins, F4 hostile finding 2026-07-03). Groww's NDJSON path has
/// MEASURED per-subject delivery skew (sidecar snapshot freezes, byte-0
/// re-tail replays, bounded 4 MiB chunk drains) that Dhan's in-process
/// broadcast does not: two subjects' lines can legitimately interleave tens
/// of seconds apart in capture order. Under `LatePolicy::Discard` a 5 s
/// margin converts ">5 s skew = counted discard"; one full minute of allowed
/// lateness converts that into "only >60 s skew discards", while still
/// bounding the Groww seal lag to ~watermark + 65 s instead of unbounded
/// (next tick / midnight only).
pub const CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW: u32 = 60;

/// Future-skew guard (seconds) for the poisoned-watermark defense (F2,
/// 2026-07-03 security review): a LEGITIMATE watermark can lead the IST wall
/// clock only by the host clock skew, which BOOT-03 bounds at ≤ 2 s — so a
/// watermark more than this many seconds AHEAD of `now_ist` is provably
/// poisoned (a garbage future-dated tick advanced the `fetch_max`, which can
/// never regress). [`compute_catchup_cutoff`] returns `None` in that case and
/// the driver logs a coalesced BOUNDARY-01 `error!` with
/// `reason = "watermark_future_skew"` — catch-up sealing stays disabled
/// until the watermark self-heals at the IST-midnight
/// [`MultiTfAggregator::reset_watermark`]. 10 s is generous vs the 2 s bound.
pub const CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS: u32 = 10;

/// Cadence (seconds) of the per-feed catch-up-seal driver tasks. Cold path:
/// each wave is at most one `O(N × 21)` scan, and the drivers self-gate on
/// "watermark advanced since the last scan" so an idle feed (overnight,
/// holiday, dead socket) costs one atomic load per wave and never scans.
pub const CATCHUP_SEAL_POLL_INTERVAL_SECS: u64 = 5;

/// Pure per-scan gate for the catch-up drivers (F2, 2026-07-03 security
/// review) — shared by BOTH the Dhan (`main.rs` Task 4) and Groww
/// (`groww_bridge.rs::spawn_groww_catchup_seal`) driver tasks so the
/// poisoned-watermark defense is unit-testable in one place.
///
/// Returns `None` (skip this wave, do NOT scan) when:
/// - `watermark_secs == 0` — no tick consumed yet;
/// - `watermark_secs == last_scanned_watermark` — self-gate: the watermark
///   did not advance since the last scan (idle feed / overnight / holiday /
///   dead socket — FEED-STALL-01 owns the dead-feed page);
/// - `watermark_secs > now_ist_secs + future_skew_guard_secs` — **POISONED
///   watermark**: a legit watermark can lead the wall clock only by host
///   skew (≤ 2 s per BOOT-03), so a further-future watermark means a
///   garbage-dated tick advanced the never-regressing `fetch_max`. The
///   caller logs a coalesced `error!(code = "BOUNDARY-01",
///   reason = "watermark_future_skew")`, increments
///   `tv_boundary_catchup_skipped_total{feed, reason="future_skew"}`, and
///   MUST NOT update `last_scanned_watermark` — so once the watermark
///   self-heals (IST-midnight [`MultiTfAggregator::reset_watermark`], or a
///   legit tick overtaking it in year-scale cases never), scanning resumes.
///
/// Otherwise returns `Some(min(watermark − margin, now_ist_secs))` — the
/// wall-clock clamp is defense-in-depth: even with a mildly-poisoned
/// watermark inside the guard band, a bucket can never seal before the wall
/// clock passes its end.
///
/// Pure, O(1), no allocation. `margin_secs` is per-feed
/// ([`CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN`] /
/// [`CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW`]);
/// `future_skew_guard_secs` is [`CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS`].
#[must_use]
pub fn compute_catchup_cutoff(
    watermark_secs: u32,
    last_scanned_watermark: u32,
    now_ist_secs: u32,
    margin_secs: u32,
    future_skew_guard_secs: u32,
) -> Option<u32> {
    if watermark_secs == 0 || watermark_secs == last_scanned_watermark {
        return None;
    }
    if watermark_secs > now_ist_secs.saturating_add(future_skew_guard_secs) {
        return None;
    }
    Some(watermark_secs.saturating_sub(margin_secs).min(now_ist_secs))
}

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
    /// Per-INSTANCE event-time watermark (BOUNDARY-01, 2026-07-03): the max
    /// `tick.exchange_timestamp` (IST epoch seconds) EVER consumed by this
    /// aggregator instance. Advanced by a single relaxed `fetch_max` at the
    /// top of [`Self::consume_tick`] — BEFORE the out-of-session early
    /// return, so post-close ticks still advance it (the final session
    /// minute's catch-up seal depends on that). Duplicate-immune by
    /// construction: a frozen-snapshot re-dump (same ts re-delivered) can
    /// never advance a max. `Arc` (not a bare atomic) because the struct is
    /// `Clone` — the catch-up driver task holds a clone and MUST observe the
    /// same watermark the tick-consumer clone advances. Dhan and Groww run
    /// SEPARATE instances, so their watermarks never cross-apply.
    max_seen_exchange_ts_secs: Arc<AtomicU32>,
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
            max_seen_exchange_ts_secs: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Current event-time watermark of this aggregator instance: the max
    /// `exchange_timestamp` (IST epoch seconds) ever seen by
    /// [`Self::consume_tick`], `0` before the first tick. The catch-up
    /// drivers gate + compute the cutoff via [`compute_catchup_cutoff`]
    /// with the per-feed margin ([`CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN`] /
    /// [`CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW`]) and pass it to
    /// [`Self::catch_up_seal_all`]. One relaxed atomic load.
    #[must_use]
    pub fn watermark_secs(&self) -> u32 {
        self.max_seen_exchange_ts_secs.load(Ordering::Relaxed)
    }

    /// Resets the event-time watermark to `0` (F2 self-heal, 2026-07-03
    /// security review). Called by BOTH IST-midnight force-seal tasks right
    /// after `force_seal_all`, so:
    /// - a POISONED watermark (a garbage future-dated tick advanced the
    ///   never-regressing `fetch_max` past the
    ///   [`CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS`] band, disabling
    ///   catch-up) self-heals within one day, and
    /// - each trading day's watermark restarts clean from the day's first
    ///   real tick instead of carrying yesterday's high-water mark.
    /// Cold path (once per day per feed instance); the drivers'
    /// `watermark == 0` gate keeps the scan idle until the next tick
    /// rebuilds it. One relaxed atomic store.
    pub fn reset_watermark(&self) {
        self.max_seen_exchange_ts_secs.store(0, Ordering::Relaxed);
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
        // BOUNDARY-01 watermark (2026-07-03): advance this instance's
        // event-time watermark BEFORE the out-of-session early return —
        // post-close ticks must advance it so the final session minute
        // (e.g. the 15:29 M1 bar) becomes catch-up-sealable once the feed's
        // event time passes 15:30 + margin. One relaxed atomic RMW, zero
        // allocation (DHAT-ratcheted); max never regresses on duplicate /
        // stale timestamps.
        self.max_seen_exchange_ts_secs
            .fetch_max(tick.exchange_timestamp, Ordering::Relaxed);

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

    /// Session-scoped force-seal (close-time force-seal, 2026-07-03).
    ///
    /// Identical to [`Self::force_seal_all`] EXCEPT it SKIPS instruments in
    /// the `always_on` exemption set (operator lock 2026-06-01 §30 — e.g.
    /// GIFT Nifty's ~21h session). Used by the 15:30:05 IST close-time
    /// force-seal task in `crates/app/src/main.rs` (Task 3b) so the final
    /// NSE session buckets (the 15:29 M1 bar and every TF's last bucket)
    /// seal + flush the same day instead of waiting for the IST-midnight
    /// backstop — which the daily 16:30 IST instance auto-stop destroys.
    ///
    /// Why always-on instruments MUST be skipped here: their session
    /// continues past the NSE 15:30 close, so force-sealing them at
    /// 15:30:05 would truncate the open bucket; the later boundary re-seal
    /// of the SAME bucket would then DEDUP-UPSERT a candle missing the
    /// pre-seal ticks. Only the IST-midnight `force_seal_all` (the day
    /// boundary) seals always-on cells.
    ///
    /// Idempotent vs the midnight seal: `force_seal` on an emptied slot
    /// returns `None`, so the midnight pass double-flushes nothing; any
    /// duplicate row is absorbed by the candle tables' DEDUP UPSERT KEYS.
    ///
    /// `O(N × 21)` where N = number of instruments (flagged O(N) honestly —
    /// cold path, runs once per trading day, never per tick).
    pub fn force_seal_all_session_scoped<F>(&self, mut on_seal: F)
    where
        F: FnMut(u64, u8, TfIndex, LiveCandleState),
    {
        let pin = self.inner.pin();
        for (key, entry) in pin.iter() {
            // O(1) EXEMPT: `always_on` is a HashSet — contains is O(1) hashing.
            if self.always_on.contains(key) {
                continue;
            }
            let (security_id, segment_code) = *key;
            for tf in TfIndex::ALL {
                if let Some(sealed) = entry.cell.force_seal(tf) {
                    on_seal(security_id, segment_code, tf, sealed);
                }
            }
        }
    }

    /// Watermark-aware catch-up seal across every instrument × 21 TFs
    /// (BOUNDARY-01, 2026-07-03). Mirrors [`Self::force_seal_all`]'s papaya
    /// iteration but calls [`AggregatorCell::catch_up_seal`] per slot, so a
    /// bucket is sealed ONLY when its end ≤ `cutoff_secs` (the caller's
    /// [`Self::watermark_secs`] minus the per-feed lateness margin)
    /// — never at/past the feed's event-time watermark (the no-mass-discard
    /// contract; see the cell method for the Failure A/B/C rationale).
    /// Unlike `force_seal_all` this does NOT re-arm day-open and does NOT
    /// clear the amendable `last_sealed` — it is an INTRADAY flush, not the
    /// day boundary; the two IST-midnight force-seal tasks keep their
    /// cross-day duties unchanged.
    ///
    /// `on_seal(security_id, segment_code, tf, state)` — the same callback
    /// shape as `force_seal_all`, because the app-side seal routing needs the
    /// instrument identity. Returns the number of buckets sealed so the
    /// driver can emit ONE coalesced BOUNDARY-01 log line per wave.
    ///
    /// `O(N × 21)` where N = number of instruments. Cold path — driven at
    /// the [`CATCHUP_SEAL_POLL_INTERVAL_SECS`] cadence, never per tick; each
    /// slot visit is the same per-slot `parking_lot::Mutex` the hot path
    /// takes (papaya reads are lock-free), so contention is per-(instrument,
    /// TF), never map-wide.
    pub fn catch_up_seal_all<F>(&self, cutoff_secs: u32, mut on_seal: F) -> usize
    where
        F: FnMut(u64, u8, TfIndex, LiveCandleState),
    {
        let mut sealed_count: usize = 0;
        let pin = self.inner.pin();
        for (key, entry) in pin.iter() {
            let (security_id, segment_code) = *key;
            for tf in TfIndex::ALL {
                if let Some(sealed) = entry.cell.catch_up_seal(tf, cutoff_secs) {
                    sealed_count = sealed_count.saturating_add(1);
                    on_seal(security_id, segment_code, tf, sealed);
                }
            }
        }
        sealed_count
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::tick_types::ParsedTick;

    fn mk_tick(sid: u64, seg: u8, secs: u32, price: f32, volume: u32, oi: u32) -> ParsedTick {
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
        set.insert((5024_u64, 0_u8));
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

    // -----------------------------------------------------------------------
    // Session-open purity — exact boundary ratchets (operator directive
    // 2026-07-03): [09:15:00 inclusive, 15:30:00 exclusive) IST. Pre-open
    // data must never seed the 09:15 open candle.
    // -----------------------------------------------------------------------

    /// Day-aligned IST epoch base (divisible by 86_400).
    const DAY_BASE: u32 = 1_779_235_200;

    #[test]
    fn test_0914_59_tick_never_seeds_0915_open() {
        // A 09:14:59 pre-open tick (secs_of_day 33_299) must be skipped —
        // and when the true 09:15:00 tick arrives, the 09:15 candle open
        // must be the 09:15:00 LTP, NOT the pre-open price.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);

        let preopen = mk_tick(13, 0, DAY_BASE + 33_299, 23_100.0, 0, 0);
        agg.consume_tick(&preopen, 0, FeedStrategy::DHAN, None, |_, _| {});
        let entry = agg.get(13, 0).expect("present");
        assert!(
            entry.cell.snapshot(TfIndex::M1).is_uninitialised(),
            "09:14:59 tick must be skipped — no bucket state"
        );

        let open_tick = mk_tick(13, 0, DAY_BASE + 33_300, 23_146.45, 0, 0);
        agg.consume_tick(&open_tick, 0, FeedStrategy::DHAN, None, |_, _| {});
        let snap = entry.cell.snapshot(TfIndex::M1);
        assert!(
            !snap.is_uninitialised(),
            "09:15:00 tick must open the bucket"
        );
        assert!(
            (snap.open - 23_146.45).abs() < 1e-6,
            "09:15 candle open must be the 09:15:00 LTP, not the 09:14:59 pre-open price"
        );
    }

    #[test]
    fn test_0915_00_first_tick_is_the_0915_open() {
        // The FIRST tick at exactly 09:15:00 (secs_of_day 33_300) opens the
        // 09:15 bucket with open == that LTP.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let tick = mk_tick(13, 0, DAY_BASE + 33_300, 100.25, 0, 0);
        let stats = agg.consume_tick(&tick, 0, FeedStrategy::DHAN, None, |_, _| {});
        assert!(stats.instrument_found);
        let entry = agg.get(13, 0).expect("present");
        let snap = entry.cell.snapshot(TfIndex::M1);
        assert!(!snap.is_uninitialised());
        assert!((snap.open - 100.25).abs() < 1e-6);
        assert_eq!(
            snap.bucket_start_ist_secs,
            DAY_BASE + 33_300,
            "09:15:00 tick must land in the 09:15-anchored first bucket"
        );
    }

    #[test]
    fn test_15_29_59_tick_is_consumed() {
        // 15:29:59 (secs_of_day 55_799) is the last in-session second —
        // it MUST form candle state.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let tick = mk_tick(13, 0, DAY_BASE + 55_799, 101.0, 0, 0);
        agg.consume_tick(&tick, 0, FeedStrategy::DHAN, None, |_, _| {});
        let entry = agg.get(13, 0).expect("present");
        assert!(
            !entry.cell.snapshot(TfIndex::M1).is_uninitialised(),
            "15:29:59 tick must be consumed (session close is exclusive at 15:30:00)"
        );
    }

    #[test]
    fn test_15_30_00_tick_is_skipped() {
        // 15:30:00 (secs_of_day 55_800) is EXCLUSIVE — no candle state.
        // (The watermark still advances on out-of-session ticks — pinned
        // separately by test_watermark_advances_on_out_of_session_tick.)
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let tick = mk_tick(13, 0, DAY_BASE + 55_800, 101.0, 0, 0);
        agg.consume_tick(&tick, 0, FeedStrategy::DHAN, None, |_, _| {});
        let entry = agg.get(13, 0).expect("present");
        assert!(
            entry.cell.snapshot(TfIndex::M1).is_uninitialised(),
            "15:30:00 tick must be skipped — the close boundary is exclusive"
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
        for sid in [13_u64, 25] {
            let t = mk_tick(sid, 0, 1_779_354_960, 100.0, 50, 0);
            agg.consume_tick(&t, 0, FeedStrategy::DHAN, None, |_, _| {});
        }
        let mut emitted: Vec<(u64, u8, TfIndex)> = Vec::new();
        agg.force_seal_all(|sid, seg, tf, _| emitted.push((sid, seg, tf)));
        assert_eq!(
            emitted.len(),
            2 * 21,
            "expected 2 instruments × 21 TFs = 42 sealed bars; got {}",
            emitted.len()
        );
        // Every slot is now empty.
        for sid in [13_u64, 25] {
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
    fn test_force_seal_all_session_scoped_skips_always_on() {
        // Close-time force-seal (2026-07-03): GIFT Nifty (always-on, ~21h
        // session) must NOT be sealed at the 15:30:05 IST close-time seal —
        // only regular NSE-session instruments are.
        let mut set = HashSet::new();
        set.insert((5024_u64, 0_u8));
        let agg = MultiTfAggregator::with_capacity(8).with_always_on(Arc::new(set));
        agg.pre_populate(vec![(5024, 0), (13, 0)]);
        for sid in [5024_u64, 13] {
            let t = mk_tick(sid, 0, 1_779_354_960, 100.0, 50, 0);
            agg.consume_tick(&t, 0, FeedStrategy::DHAN, None, |_, _| {});
        }
        let mut emitted: Vec<(u64, u8, TfIndex)> = Vec::new();
        agg.force_seal_all_session_scoped(|sid, seg, tf, _| emitted.push((sid, seg, tf)));
        assert_eq!(
            emitted.len(),
            21,
            "only the regular instrument's 21 TFs seal; got {}",
            emitted.len()
        );
        assert!(
            emitted.iter().all(|(sid, _, _)| *sid == 13),
            "always-on sid 5024 must be skipped by the session-scoped seal"
        );
        // GIFT Nifty's open bucket survives; the regular instrument's is drained.
        let gift = agg.get(5024, 0).expect("present");
        assert!(
            !gift.cell.snapshot(TfIndex::ALL[0]).is_uninitialised(),
            "always-on open bucket must survive the close-time seal"
        );
        let nifty = agg.get(13, 0).expect("present");
        assert!(nifty.cell.snapshot(TfIndex::ALL[0]).is_uninitialised());
    }

    #[test]
    fn test_force_seal_all_session_scoped_seals_regular_instruments() {
        // With an empty always_on set the session-scoped seal behaves
        // exactly like force_seal_all: 2 instruments × 21 TFs = 42 bars.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0), (25, 0)]);
        for sid in [13_u64, 25] {
            let t = mk_tick(sid, 0, 1_779_354_960, 100.0, 50, 0);
            agg.consume_tick(&t, 0, FeedStrategy::DHAN, None, |_, _| {});
        }
        let mut count: u32 = 0;
        agg.force_seal_all_session_scoped(|_, _, _, _| count += 1);
        assert_eq!(count, 2 * 21);
    }

    #[test]
    fn test_close_then_midnight_force_seal_is_idempotent() {
        // The close-time seal (15:30:05) followed by the IST-midnight
        // force_seal_all must NOT double-flush: force_seal on an emptied
        // slot returns None, so the midnight pass emits 0 seals.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let t = mk_tick(13, 0, 1_779_354_960, 100.0, 50, 0);
        agg.consume_tick(&t, 0, FeedStrategy::DHAN, None, |_, _| {});
        let mut first: u32 = 0;
        agg.force_seal_all_session_scoped(|_, _, _, _| first += 1);
        assert_eq!(first, 21, "close-time seal drains the 21 open TF buckets");
        let mut second: u32 = 0;
        agg.force_seal_all(|_, _, _, _| second += 1);
        assert_eq!(
            second, 0,
            "midnight force_seal_all after the close-time seal must emit 0 (idempotent)"
        );
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

    // -----------------------------------------------------------------------
    // BOUNDARY-01 watermark catch-up seal (2026-07-03)
    // -----------------------------------------------------------------------

    /// IST midnight (epoch-in-IST-seconds) of the 2026-05-21 test day the
    /// existing tests anchor on (`1_779_354_900` = that day's 09:15:00).
    const TEST_DAY_START: u32 = 1_779_321_600;

    #[test]
    fn test_watermark_secs_starts_zero_and_is_shared_across_clones() {
        // The getter contract: 0 before any tick; a clone (the catch-up
        // driver task holds one) observes the SAME watermark the
        // tick-consumer clone advances (Arc-shared, per-instance).
        let agg = MultiTfAggregator::new();
        assert_eq!(agg.watermark_secs(), 0);
        agg.pre_populate(vec![(13, 0)]);
        let driver_clone = agg.clone();
        let ts = TEST_DAY_START + 33_330; // 09:15:30 IST
        agg.consume_tick(
            &mk_tick(13, 0, ts, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        assert_eq!(driver_clone.watermark_secs(), ts);
        // A separate INSTANCE has its own watermark (Dhan vs Groww never
        // cross-apply).
        let other_instance = MultiTfAggregator::new();
        assert_eq!(other_instance.watermark_secs(), 0);
    }

    #[test]
    fn test_watermark_advances_and_never_regresses() {
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        assert_eq!(agg.watermark_secs(), 0, "no tick yet → watermark 0");

        let base = TEST_DAY_START + 33_300; // 09:15:00 IST
        agg.consume_tick(
            &mk_tick(13, 0, base + 30, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        assert_eq!(agg.watermark_secs(), base + 30);

        // Advance.
        agg.consume_tick(
            &mk_tick(13, 0, base + 90, 101.0, 60, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        assert_eq!(agg.watermark_secs(), base + 90);

        // A STALE tick (older ts) must never regress the watermark.
        agg.consume_tick(
            &mk_tick(13, 0, base + 10, 99.0, 61, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        assert_eq!(agg.watermark_secs(), base + 90, "stale ts must not regress");

        // A DUPLICATE ts (frozen-snapshot re-dump class) must not move it.
        agg.consume_tick(
            &mk_tick(13, 0, base + 90, 101.0, 61, 0),
            0,
            FeedStrategy::GROWW,
            Some(61),
            |_, _| {},
        );
        assert_eq!(agg.watermark_secs(), base + 90, "duplicate ts is a no-op");

        // Clones share the SAME watermark (the driver task holds a clone).
        let cloned = agg.clone();
        assert_eq!(cloned.watermark_secs(), base + 90);
    }

    #[test]
    fn test_watermark_advances_on_out_of_session_tick() {
        // A post-close tick (>= 15:30 IST) is gated out of the candle fold,
        // but it MUST advance the watermark — the final session minute's
        // catch-up seal depends on the feed's event time crossing 15:30.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let post_close = TEST_DAY_START + 55_807; // 15:30:07 IST
        let stats = agg.consume_tick(
            &mk_tick(13, 0, post_close, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        assert_eq!(stats.sealed_count, 0);
        assert_eq!(
            agg.watermark_secs(),
            post_close,
            "post-close tick must advance"
        );
        // The fold itself stayed gated — no bucket opened.
        let entry = agg.get(13, 0).expect("present");
        assert!(
            entry.cell.snapshot(TfIndex::M1).is_uninitialised(),
            "out-of-session tick must not open a candle"
        );
    }

    #[test]
    fn test_compute_catchup_cutoff_gates() {
        // F2 (2026-07-03 security review): the shared pure per-scan gate.
        let now = TEST_DAY_START + 40_000;
        let margin = CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN;
        let guard = CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS;
        // 1) No tick yet → None.
        assert_eq!(compute_catchup_cutoff(0, 0, now, margin, guard), None);
        // 2) Self-gate: watermark unchanged since the last scan → None.
        let wm = now - 30;
        assert_eq!(compute_catchup_cutoff(wm, wm, now, margin, guard), None);
        // 3) POISONED: watermark further ahead of wall clock than the
        //    future-skew guard → None (caller logs BOUNDARY-01
        //    reason=watermark_future_skew and must NOT update last_scanned).
        let poisoned = now + guard + 1;
        assert_eq!(
            compute_catchup_cutoff(poisoned, 0, now, margin, guard),
            None
        );
        // Boundary: exactly now + guard is still allowed (legit host skew).
        assert_eq!(
            compute_catchup_cutoff(now + guard, 0, now, margin, guard),
            Some(now) // wm − margin > now → clamped to wall clock
        );
        // 4) Normal: Some(min(wm − margin, now)) — here wm − margin < now.
        assert_eq!(
            compute_catchup_cutoff(wm, 0, now, margin, guard),
            Some(wm - margin)
        );
        // Groww's wider margin flows through the same gate.
        assert_eq!(
            compute_catchup_cutoff(wm, 0, now, CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW, guard),
            Some(wm - CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW)
        );
        // Tiny watermark (≤ margin) saturates to 0, never underflows.
        assert_eq!(compute_catchup_cutoff(3, 0, now, margin, guard), Some(0));
    }

    #[test]
    fn test_compute_catchup_cutoff_clamps_to_wall_clock() {
        // F2 defense-in-depth: a watermark inside the skew-guard band but
        // ahead of the wall clock must never let a bucket seal before the
        // wall clock passes its end — cutoff is clamped to now_ist.
        let now = TEST_DAY_START + 40_000;
        let margin = 2; // margin < lead so wm − margin > now
        let guard = CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS;
        let wm = now + 5; // ≤ now + guard → allowed, but ahead of wall clock
        assert_eq!(
            compute_catchup_cutoff(wm, 0, now, margin, guard),
            Some(now),
            "cutoff must clamp to the wall clock, not wm − margin"
        );
        // With margin ≥ lead the min() is wm − margin (behind the clock).
        assert_eq!(
            compute_catchup_cutoff(wm, 0, now, CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW, guard),
            Some(wm - CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW)
        );
    }

    #[test]
    fn test_reset_watermark_zeroes_and_next_tick_rebuilds() {
        // F2 self-heal (2026-07-03): the IST-midnight tasks reset the
        // watermark to 0 right after force_seal_all, so a poisoned watermark
        // self-heals within one day and the next tick rebuilds it clean.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let ts = TEST_DAY_START + 33_310;
        agg.consume_tick(
            &mk_tick(13, 0, ts, 100.0, 10, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        assert_eq!(agg.watermark_secs(), ts);
        agg.reset_watermark();
        assert_eq!(agg.watermark_secs(), 0, "reset must zero the watermark");
        // A clone (the driver task's handle) observes the same reset.
        assert_eq!(agg.clone().watermark_secs(), 0);
        // The next tick rebuilds the watermark from scratch.
        let ts2 = ts + 60;
        agg.consume_tick(
            &mk_tick(13, 0, ts2, 101.0, 20, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        assert_eq!(agg.watermark_secs(), ts2, "next tick rebuilds cleanly");
    }

    #[test]
    fn test_post_catchup_newer_tick_opens_next_bucket_with_volume_continuity() {
        // Item 28 carry-over across a catch-up seal: the next bucket's
        // bucket_start_cumulative must be the per-instrument last_cumulative
        // baseline, so incremental volume is exact — identical to the
        // next-tick boundary-crossing seal path.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let b1 = TEST_DAY_START + 33_300; // 09:15:00 IST (M1 bucket 1)
        agg.consume_tick(
            &mk_tick(13, 0, b1 + 10, 100.0, 100, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        // Catch up: only M1's bucket (end b1+60) is <= cutoff b1+60.
        let sealed = agg.catch_up_seal_all(b1 + 60, |_, _, _, _| {});
        assert_eq!(sealed, 1, "only the M1 bucket ends at/behind the cutoff");
        // Next tick opens M1 bucket 2 with cumulative volume 150.
        agg.consume_tick(
            &mk_tick(13, 0, b1 + 70, 102.0, 150, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        let entry = agg.get(13, 0).expect("present");
        let snap = entry.cell.snapshot(TfIndex::M1);
        assert_eq!(snap.bucket_start_ist_secs, b1 + 60);
        assert_eq!(
            snap.bucket_start_cumulative, 100,
            "baseline must carry the pre-seal cumulative"
        );
        assert_eq!(snap.volume, 50, "incremental volume = 150 - 100");
        assert_eq!(
            snap.open, 102.0,
            "intraday open at LTP (no day-open re-arm)"
        );
    }

    #[test]
    fn test_catch_up_seal_all_never_seals_past_watermark() {
        // THE no-mass-discard ratchet: a backlogged feed (watermark barely
        // past the open buckets' starts) must produce ZERO catch-up seals —
        // every bucket's end is past the cutoff, so the backlog can still
        // fill them. This is the contract that prevents research Failure A.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0), (25, 0)]);
        let base = TEST_DAY_START + 33_300; // 09:15:00 IST
        for sid in [13_u64, 25] {
            agg.consume_tick(
                &mk_tick(sid, 0, base + 30, 100.0, 50, 0),
                0,
                FeedStrategy::DHAN,
                None,
                |_, _| {},
            );
        }
        assert_eq!(agg.watermark_secs(), base + 30);
        let cutoff = agg
            .watermark_secs()
            .saturating_sub(CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN);
        let mut emitted: u32 = 0;
        let sealed = agg.catch_up_seal_all(cutoff, |_, _, _, _| emitted += 1);
        assert_eq!(sealed, 0, "no bucket ends at/behind watermark-margin");
        assert_eq!(emitted, 0);
        // Every open bucket survives untouched for the backlog to fill.
        for sid in [13_u64, 25] {
            let entry = agg.get(sid, 0).expect("present");
            assert!(
                !entry.cell.snapshot(TfIndex::M1).is_uninitialised(),
                "backlogged bucket must stay open"
            );
        }
    }

    #[test]
    fn test_boundary_catchup_final_session_minute_seals_when_watermark_passes_close() {
        // The 2e gap: the 15:29 M1 bar can never be sealed by a next tick
        // (>= 15:30 is out-of-session). Once a post-close tick advances the
        // watermark past 15:30:00 + margin, the catch-up seal flushes it.
        let agg = MultiTfAggregator::new();
        agg.pre_populate(vec![(13, 0)]);
        let last_minute_tick = TEST_DAY_START + 55_770; // 15:29:30 IST
        agg.consume_tick(
            &mk_tick(13, 0, last_minute_tick, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        // Before the watermark passes the close: the 15:29 bucket must wait.
        let pre_cutoff = agg
            .watermark_secs()
            .saturating_sub(CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN);
        assert_eq!(agg.catch_up_seal_all(pre_cutoff, |_, _, _, _| {}), 0);

        // A post-close tick advances the watermark to 15:30:07 (folding is
        // gated, the watermark is not).
        agg.consume_tick(
            &mk_tick(13, 0, TEST_DAY_START + 55_807, 100.5, 51, 0),
            0,
            FeedStrategy::DHAN,
            None,
            |_, _| {},
        );
        let cutoff = agg
            .watermark_secs()
            .saturating_sub(CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN);
        assert!(cutoff >= TEST_DAY_START + 55_800, "cutoff passed 15:30:00");
        let mut m1_sealed: Option<LiveCandleState> = None;
        let sealed = agg.catch_up_seal_all(cutoff, |sid, seg, tf, state| {
            assert_eq!((sid, seg), (13, 0));
            if tf == TfIndex::M1 {
                m1_sealed = Some(state);
            }
        });
        assert!(sealed >= 1, "the final session minute must seal");
        let m1 = m1_sealed.expect("15:29 M1 bar must be catch-up sealed");
        assert_eq!(
            m1.bucket_start_ist_secs,
            TEST_DAY_START + 55_740,
            "sealed bar is the 15:29:00 bucket (bucket-start ts, never wall-clock)"
        );
        assert_eq!(m1.close, 100.0);
        // Wider TFs whose bucket ends past the cutoff stay open for the
        // midnight force-seal (e.g. H1 — the 09:15-anchored grid's last H1
        // bucket ends after 15:30).
        let entry = agg.get(13, 0).expect("present");
        assert!(
            !entry.cell.snapshot(TfIndex::H1).is_uninitialised(),
            "H1 must wait for the IST-midnight force-seal"
        );
    }
}
