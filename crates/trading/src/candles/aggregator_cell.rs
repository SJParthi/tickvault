//! Per-instrument multi-timeframe fold cell — REBUILT 2026-08-09.
//!
//! # Why this file exists again
//!
//! The original `aggregator_cell.rs` was HARD-DELETED on 2026-07-17 (stage-3
//! dead-WS sweep, PR #1638) because both live feeds had been retired and the
//! 21-TF tick aggregator had no tick input. The Dhan live main-feed WS is
//! REVIVED by the operator's dated 2026-08-09 authorization
//! (`websocket-connection-scope-lock.md`, "2026-08-09 — DHAN LIVE MAIN-FEED WS
//! REVIVAL AUTHORIZED"), so the tick→timeframe fold is needed again.
//!
//! This is a REBUILD, not a restoration. The differences from the deleted
//! shape are deliberate and each is load-bearing:
//!
//! | Deleted shape | This shape | Why |
//! |---|---|---|
//! | `[parking_lot::Mutex<LiveCandleState>; 21]` per instrument | plain `[LiveCandleState; 21]`, reached through `&mut self` | the fold has exactly ONE owner (the tick-consumer task). A lock that is never contended is pure cost — ~30 ns × 21 TF × every tick. Sharing is re-introduced by the caller (channel / `SealRing`), not by this type. |
//! | `Arc<AggregatorCell>` handed out of a `papaya::HashMap` | owned by value inside the container's slot table | same reason: no cross-thread sharing to make lock-free. |
//! | key `(security_id, segment_code)` | key `(feed, security_id, segment_code)` | I-P1-11 + the 2026-06-19 feed-in-key operator lock. See [`crate::candles::MultiTfAggregator`]. |
//! | ticks folded unconditionally | non-finite / non-positive / absurd LTP REFUSED at ingest | `f32_to_f64_clean` passes `NaN` straight through and the Dhan quote parser is PROVEN to emit `NaN` OHLC. `NaN` is absorbing under `>`/`<`, so ONE such packet poisons `high`/`low` for the rest of the bucket AND every downstream row. The same is true of an absurd-but-FINITE price (`f32::MAX` from a mangled frame) against a running-`max` `high` — bounded by `MAX_PLAUSIBLE_LTP` since 2026-08-15. Fail closed at ingest. |
//!
//! # What is preserved verbatim (do not "simplify" these)
//!
//! - **Sparsity.** [`LiveCandleState::is_uninitialised`] (`bucket_start_ist_secs
//!   == 0`) is the sentinel that makes an untouched bucket emit NOTHING. A
//!   dense engine that emitted an empty bar per (instrument × TF × bucket)
//!   would write ~808 M rows/day (~35,900 rows/s) against a ~5,000 rows/s
//!   ingest envelope. Sparse, it is ~46 M rows/day (~2,050 rows/s). Every
//!   seal path in this file returns `None` / emits nothing for an
//!   uninitialised slot.
//! - **Bucket alignment from `tick.exchange_timestamp`** (the WS LTT field,
//!   IST epoch seconds) — NEVER `Utc::now()` (`data-integrity.md`).
//! - **`f32_to_f64_clean` for every f32→f64 price widening** — `f64::from`
//!   produces IEEE-754 artifacts (`23925.65_f32` → `23925.650390625_f64`)
//!   that landed verbatim in `candles_1m` in 2026-05.
//! - **The volume contract**: `volume` is INCREMENTAL within the bucket,
//!   derived as `cumulative − bucket_start_cumulative`; the storage column
//!   contract (`shadow_seal_columns.rs`) depends on it.
//!
//! # Complexity
//!
//! [`AggregatorCell::consume_tick`] is O(1): one ordinal index into a fixed
//! array, a handful of scalar comparisons, no loop over data, no allocation.

use tickvault_common::constants::MAX_PLAUSIBLE_LTP;
use tickvault_common::price_precision::f32_to_f64_clean;
use tickvault_common::tick_types::ParsedTick;

use crate::candles::tf_index::MARKET_OPEN_SECS_OF_DAY_IST;
use crate::candles::{LiveCandleState, TF_COUNT, TfIndex};

// ---------------------------------------------------------------------------
// Late-tick policy — a PARAMETER, never hardcoded
// ---------------------------------------------------------------------------

/// What the cell does with a tick whose bucket is EARLIER than the open bucket
/// but equal to the MOST-RECENTLY sealed bucket (a 1-bucket-late arrival).
///
/// The pre-2026-07-17 engine carried this as per-feed DATA (one engine, a
/// per-feed value) rather than forked code, because the two feeds genuinely
/// differed. That shape did not survive the sweep, so it is rebuilt here and
/// taken as a parameter — no policy is baked into the fold.
///
/// **Default chosen by this rebuild: [`LatePolicy::Refold`]**
/// ([`FeedStrategy::DEFAULT`]). Rationale: the only feed authorized to
/// produce ticks today is Dhan (2026-08-09 revival), and Dhan's documented
/// behaviour — an LTT-09:15:59 tick physically arriving at 09:16:00 — is
/// exactly the 1-bucket-late case. Under `Discard` that tick's price would be
/// silently lost from the 09:15 bar. Under `Refold` the sealed bar's H/L/C is
/// corrected and re-emitted, and the QuestDB DEDUP key
/// `(ts, security_id, segment, feed)` UPSERTs it in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatePolicy {
    /// Re-fold a 1-bucket-late tick into the just-sealed bucket's H/L/C and
    /// re-emit it for UPSERT. Never touches `open` / `volume` / `oi`.
    Refold,
    /// Drop the out-of-order tick; a sealed bar is immutable once emitted.
    Discard,
}

/// Per-feed fold policy. `Copy`, one enum field — a zero-cost stack argument
/// on the hot path. A future per-feed knob extends THIS struct; it never
/// forks the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedStrategy {
    /// How a 1-bucket-late tick is handled. See [`LatePolicy`].
    pub late_policy: LatePolicy,
}

impl FeedStrategy {
    /// Re-fold 1-bucket-late ticks into their own bucket (see [`LatePolicy`]
    /// for why this is the rebuild's default).
    pub const REFOLD: Self = Self {
        late_policy: LatePolicy::Refold,
    };
    /// Never amend a sealed bar.
    pub const DISCARD: Self = Self {
        late_policy: LatePolicy::Discard,
    };
    /// The documented default of this rebuild — [`Self::REFOLD`].
    pub const DEFAULT: Self = Self::REFOLD;
}

impl Default for FeedStrategy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// ---------------------------------------------------------------------------
// Price sanity — fail CLOSED at ingest
// ---------------------------------------------------------------------------

/// Returns `true` when a tick's last-traded price may be folded.
///
/// Accepts finite, strictly positive, AND below [`MAX_PLAUSIBLE_LTP`].
///
/// `NaN` and `±Inf` reach this point because [`f32_to_f64_clean`] deliberately
/// passes them through, and the Dhan quote parser is proven to emit both `NaN`
/// OHLC and negative LTP. `NaN` is absorbing under comparison, so a single
/// poisoned packet would leave `high`/`low` permanently wrong for the bucket
/// and write that row to QuestDB.
///
/// The CEILING closes the remaining half of that same hole (wired 2026-08-15;
/// `MAX_PLAUSIBLE_LTP` had been declared since Phase 0 with **zero references**
/// anywhere in the workspace — a named limit that enforced nothing). An
/// absurd-but-FINITE price from a mangled frame — `f32::MAX` ≈ 3.4e38 is the
/// worst case — passes `is_finite() && > 0.0` cleanly, and `high` is a running
/// `max`, so ONE such packet pins that bucket's high at 3.4e38 for the rest of
/// the minute and persists it. Refusing is one more comparison.
///
/// The check runs on the RAW `f32`, deliberately BEFORE any widening: passing
/// `f32::MAX` through [`f32_to_f64_clean`] first would yield `3.4028235e23`
/// (the documented buffer-truncation limit on [`TickPrices`]), still absurd but
/// 15 orders of magnitude off — the raw value is the honest one to bound.
///
/// The ceiling can NEVER reject a genuine quote: ₹10 crore is ~500× the
/// highest-priced real NSE instrument (MRF ≈ ₹1.5 lakh, SENSEX ≈ 80k), and it
/// is an absolute ceiling rather than a per-instrument band precisely so a
/// legitimate limit-up move is never dropped — the prime directive is to never
/// miss a real tick.
///
/// # Complexity
/// O(1) — three scalar comparisons, no allocation.
#[inline]
#[must_use]
pub fn tick_price_is_sane(tick: &ParsedTick) -> bool {
    let p = tick.last_traded_price;
    // The widened check is the load-bearing half (2026-08-25). A value can be
    // finite, positive and inside the ceiling and STILL widen to `0.0`:
    // `f32_to_f64_clean` formats through a 24-byte buffer and Rust's f32
    // `Display` never uses scientific notation, so anything whose plain
    // decimal rendering overflows it parses back as zero. `f32::MIN_POSITIVE`
    // is the headline case and it is a perfectly NORMAL float, which is why an
    // `is_normal()` gate would not have caught it. Pinned by
    // `test_tick_prices_subnormal_day_field_collapses_to_sentinel`.
    p.is_finite() && p > 0.0 && p <= MAX_PLAUSIBLE_LTP && f32_to_f64_clean(p) > 0.0
}

// ---------------------------------------------------------------------------
// Hoisted price widening — pay the f32→f64 conversion ONCE per tick
// ---------------------------------------------------------------------------

/// The three `f32`→`f64`-widened prices a fold needs, converted ONCE per tick.
///
/// [`f32_to_f64_clean`] is NOT a free cast — it round-trips through a shortest
/// decimal string (ryu format + parse, ~50 ns) to avoid IEEE-754 widening
/// artifacts. That cost is correct and non-negotiable, but it must be paid per
/// TICK, not per TIMEFRAME: the three source fields (`last_traded_price`,
/// `day_open`, `day_close`) are identical across all `TF_COUNT` timeframes, so
/// converting inside the fold multiplied one tick's conversion cost by
/// `TF_COUNT × 3` (72 at the current `TF_COUNT = 24`; the comment said 63 at
/// 21 until 2026-08-14, stale since the 2026-08-10 raise) — ~3.6 µs against the 100 ns/tick
/// hot-path budget, a ~31× overrun for zero added information.
///
/// Widening is applied here as the folds applied it: `day_open` / `day_close`
/// are widened only when strictly positive and stored as `0.0` otherwise.
/// `> 0.0` is false for `NaN`, so a poisoned field still collapses to the
/// `0.0` sentinel rather than propagating.
///
/// HONEST LIMIT (corrected 2026-08-10 — an earlier version of this comment
/// claimed the change was "bit-identical", which is FALSE). The guard tests
/// the CONVERTED value where the old fold tested the RAW one, and those are
/// NOT equivalent for every `f32`. [`f32_to_f64_clean`] formats through a
/// 24-byte stack buffer; Rust's `f32` `Display` never uses scientific
/// notation, so a positive subnormal (⪅ `1e-23`, e.g. `f32::MIN_POSITIVE`)
/// expands past that buffer, the write fails mid-way, and the truncated
/// decimal parses back to `0.0`. For such a value `raw > 0.0` is true while
/// `converted > 0.0` is false.
///
/// The resulting divergence is confined to that subnormal band and is, if
/// anything, the safer behaviour — a sub-`1e-23` "price" is not a real quote,
/// and treating it as absent beats adopting a garbage session open. It is
/// recorded here rather than smoothed over because the previous wording
/// asserted an identity a future reader would have trusted. Pinned by
/// `test_tick_prices_subnormal_day_field_collapses_to_sentinel`.
///
/// (Pre-existing and unchanged by this refactor: `f32::MAX` widens to
/// `3.4028235e23`, off by 15 orders of magnitude, for the same
/// buffer-truncation reason.)
///
/// # Complexity
/// O(1) — at most three conversions, no allocation, no loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickPrices {
    /// Widened last-traded price. Always converted (the fold always needs it).
    pub last_traded_price: f64,
    /// Widened `day_open`, or `0.0` when absent / non-positive / `NaN`.
    pub day_open: f64,
    /// Widened `day_close`, or `0.0` when absent / non-positive / `NaN`.
    pub day_close: f64,
}

impl TickPrices {
    /// Widens a tick's three fold-relevant prices once.
    ///
    /// # Complexity
    /// O(1) — no allocation, no loop.
    #[inline]
    #[must_use]
    pub fn from_tick(tick: &ParsedTick) -> Self {
        Self {
            last_traded_price: f32_to_f64_clean(tick.last_traded_price),
            // Both day fields go through the SAME gate the day extremes use.
            // `> 0.0` alone was the old test: it rejects NaN, but accepts
            // `+inf`, `f32::MAX` and subnormals — and `day_open` is stamped
            // into a bar's `open` AND `session_open`, `day_close` into
            // `prev_day_close`, so one mangled frame reached four persisted
            // columns. See `usable_exchange_price`.
            day_open: if usable_exchange_price(tick.day_open) {
                f32_to_f64_clean(tick.day_open)
            } else {
                0.0
            },
            day_close: if usable_exchange_price(tick.day_close) {
                f32_to_f64_clean(tick.day_close)
            } else {
                0.0
            },
        }
    }
}

// ---------------------------------------------------------------------------
// ConsumeOutcome
// ---------------------------------------------------------------------------

/// Result of folding one tick into ONE timeframe slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConsumeOutcome {
    /// Folded into the open bucket (or opened the first bucket). Nothing to
    /// seal, nothing to emit.
    Updated,
    /// The tick crossed this timeframe's boundary: the previous bucket was
    /// drained out of the slot and a new bucket was opened holding this tick.
    /// The caller MUST persist `sealed_state`.
    Sealed {
        /// The bucket that just closed.
        sealed_state: LiveCandleState,
    },
    /// A 1-bucket-late tick re-folded the most-recently-sealed bucket's
    /// H/L/C ([`LatePolicy::Refold`] only). The caller MUST route
    /// `amended_state` through the SAME seal path so the writer UPSERTs that
    /// bar in place. Counts as amended, NOT discarded.
    AmendedLate {
        /// The corrected bar to UPSERT.
        amended_state: LiveCandleState,
    },
    /// The tick is ≥ 2 buckets late, or there is no amendable sealed bucket,
    /// or the policy is [`LatePolicy::Discard`]. Dropped; the caller counts it.
    DiscardLate,
}

// ---------------------------------------------------------------------------
// AggregatorCell
// ---------------------------------------------------------------------------

/// Per-instrument multi-timeframe candle state: one open bucket per
/// timeframe, plus the most-recently sealed bucket per timeframe for the
/// [`LatePolicy::Refold`] amend.
///
/// # Layout and RAM
///
/// `[LiveCandleState; TF_COUNT] × 2` + `[bool; TF_COUNT]` =
/// `24 × 128 × 2 + 24` ≈ **6.2 KB per instrument** (pinned by a
/// compile-time assertion below). At the 25,000-instrument slot ceiling that
/// is ~154 MB — real, budgeted against the 32 GiB r8g.xlarge host, and it
/// only materialises for instruments that actually tick (slots are allocated
/// on first sight, never pre-populated).
///
/// # Ownership
///
/// Single-owner by design (`&mut self`). There is no interior mutability and
/// no lock: the tick-consumer task owns the whole container. Cross-thread
/// hand-off happens downstream of the fold, through the `SealRing` / channel.
#[derive(Clone, Debug)]
pub struct AggregatorCell {
    /// Open bucket per timeframe, indexed by [`TfIndex::as_ordinal`].
    slots: [LiveCandleState; TF_COUNT],
    /// Most-recently sealed bucket per timeframe. The empty sentinel
    /// (`bucket_start_ist_secs == 0`) means "nothing amendable". Populated by
    /// an INTRADAY boundary seal and by [`Self::catch_up_seal`]; CLEARED by
    /// [`Self::force_seal`] so a previous-day bar can never be amended across
    /// the day boundary.
    last_sealed: [LiveCandleState; TF_COUNT],
    /// Per-timeframe "the next bucket this slot opens is the day's first bar"
    /// flag. When set, that bucket's `open` is the exchange-published
    /// `tick.day_open` (the official 09:15 equilibrium open) rather than the
    /// first tick's LTP. Set at construction and re-set by
    /// [`Self::force_seal`] (the day boundary); consumed on the open.
    armed_for_day_open: [bool; TF_COUNT],
    /// The highest exchange-published session HIGH this cell has observed
    /// TODAY, in raw wire `f32`. `0.0` = no baseline (boot, or just after a
    /// day-boundary [`Self::force_seal`]). Written ONLY by
    /// [`Self::observe_session_extremes`], once per TICK — never once per
    /// timeframe, because the comparison that gives it meaning is
    /// "did the session high move between two consecutive PACKETS".
    ///
    /// It is a HIGH-WATER MARK, not a last-seen value, and that distinction is
    /// load-bearing — see [`Self::observe_session_extremes`].
    last_seen_day_high: f32,
    /// Session LOW counterpart of [`Self::last_seen_day_high`] — a LOW-water
    /// mark, moving only downward within a day.
    last_seen_day_low: f32,
    /// LTT of the PREVIOUS packet this cell observed. `0` = none yet. This is
    /// the left endpoint of the interval a session-extreme delta describes,
    /// and it is what makes attribution exact rather than assumed.
    last_observed_ts: u32,
}

/// The session extremes that moved between the previous observed packet and
/// this one, pre-widened to `f64`.
///
/// A `Some(v)` means: the exchange's running session high (or low) reached `v`
/// at some instant strictly after the previous packet we saw and at or before
/// this one. It says WHERE the print landed only in combination with the
/// caller's bucket check — see [`AggregatorCell::observe_session_extremes`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SessionExtremeDelta {
    /// New session high, if it rose since the previous observed packet.
    pub new_high: Option<f64>,
    /// New session low, if it fell since the previous observed packet.
    pub new_low: Option<f64>,
    /// LTT of the packet this delta is measured FROM — the OPEN left endpoint
    /// of the interval `(prev_observed_ts, this packet's LTT]` inside which
    /// the exchange set the new extreme. `0` when there was no previous
    /// packet, which no bucket can match, so a first observation attributes to
    /// nothing.
    ///
    /// Carried on the delta rather than read from the cell at fold time
    /// because the fold must ask about the packet the delta CAME FROM, not
    /// whatever the cell has seen since.
    prev_observed_ts: u32,
}

impl SessionExtremeDelta {
    /// `true` when neither extreme moved — the overwhelmingly common case.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.new_high.is_none() && self.new_low.is_none()
    }
}

impl Default for AggregatorCell {
    fn default() -> Self {
        Self::empty()
    }
}

impl AggregatorCell {
    /// Empty cell — every timeframe slot uninitialised (so it emits nothing
    /// until a tick opens it) and every slot armed for day-open.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            slots: [LiveCandleState::empty(); TF_COUNT],
            last_sealed: [LiveCandleState::empty(); TF_COUNT],
            armed_for_day_open: [true; TF_COUNT],
            last_seen_day_high: 0.0,
            last_seen_day_low: 0.0,
            last_observed_ts: 0,
        }
    }

    /// Observes this packet's exchange-published session extremes and records
    /// what MOVED since the previous packet. Call ONCE per tick, BEFORE the
    /// per-timeframe fold.
    ///
    /// # Why a delta and not the level
    ///
    /// `day_high` / `day_low` are RUNNING SESSION extremes computed by the
    /// exchange over the FULL tape — so they are immune both to our sampling
    /// gaps and to Dhan's documented consumer-conflation (a slow consumer is
    /// skipped forward to "the latest available state", silently discarding
    /// the intermediate prints). Adopting the LEVEL into a mid-session bar
    /// would smear the whole day's range across it, which is why
    /// [`adopt_exchange_day_extremes`] is confined to the day's first bucket.
    /// The DELTA carries a strictly narrower, and therefore usable, claim:
    /// *the session high reached `v` somewhere in the interval between the
    /// previous packet and this one.*
    ///
    /// That interval is attributable to exactly one bucket only when BOTH
    /// endpoints sit inside it — which is why the fold applies the delta only
    /// on the in-bucket path of a bucket that has already folded a tick. When
    /// the interval straddles a bucket boundary the print's bucket is
    /// genuinely unknown, and the delta is DROPPED rather than guessed.
    ///
    /// # Why the baseline is a HIGH-WATER MARK and never moves back down
    ///
    /// A session high cannot fall while the session runs, so a packet carrying
    /// a LOWER `day_high` is never news — it is a stale packet, a vendor
    /// reset, or corruption. This method is called before the fold knows
    /// whether the packet is late, and the feed genuinely delivers out of
    /// order (`LatePolicy::Refold` exists for exactly that). So if a fall were
    /// allowed to move the baseline down, a stale packet would lower it and
    /// the very next fresh packet would restore the earlier level as an
    /// apparent RISE — attributing a print from minutes ago to the bucket that
    /// happens to be open now. That is the session-range smearing this whole
    /// design exists to avoid, arriving through the back door.
    ///
    /// Holding the mark instead makes a stale packet cost at worst a MISSED
    /// widening, never a wrong one. Regressions are counted rather than
    /// swallowed, because a persistent stream of them means the feed is
    /// resetting mid-session and that is worth seeing.
    ///
    /// # Complexity
    /// O(1) — two `f32` compares on the common path; the `f32`→`f64` widening
    /// runs only on the rare packet where an extreme actually moved, so a
    /// quiet tick pays two register comparisons and nothing else. No
    /// allocation.
    #[inline]
    pub fn observe_session_extremes(&mut self, tick: &ParsedTick) -> SessionExtremeDelta {
        let mut delta = SessionExtremeDelta {
            prev_observed_ts: self.last_observed_ts,
            ..SessionExtremeDelta::default()
        };

        // The interval's left endpoint may only be moved by a packet that
        // actually REPORTED a session extreme. Advancing it unconditionally
        // was a real defect (found by permutation sweep 2026-08-25): a Ticker
        // packet, or a Quote whose day fields decoded 0.0 / NaN, carries no
        // information about the session extremes at all — yet it narrowed the
        // interval, so the NEXT rise was attributed on evidence that did not
        // exist. A packet that says nothing must move nothing.
        let carries_extremes =
            usable_exchange_price(tick.day_high) || usable_exchange_price(tick.day_low);
        if carries_extremes {
            self.last_observed_ts = tick.exchange_timestamp;
        }

        // A packet claiming a session high BELOW its own session low is
        // internally impossible, so it is evidence the frame is corrupt. The
        // monotone marks below already make it harmless — neither value can
        // win its comparison against a mark on the correct side — but harmless
        // is not the same as seen. Without this the only self-contradictory
        // signal the feed can produce leaves no trace anywhere, and a rising
        // rate of it (a decode drift, a vendor change) would be invisible
        // until something downstream broke for a reason nobody could name.
        if usable_exchange_price(tick.day_high)
            && usable_exchange_price(tick.day_low)
            && tick.day_high < tick.day_low
        {
            crate::candles::fold_counters::fold_counters()
                .session_extremes_inverted
                .increment(1);
        }

        if usable_exchange_price(tick.day_high) {
            if tick.day_high > self.last_seen_day_high {
                // A first observation has `last_seen_day_high == 0.0`, so it
                // establishes the mark and reports NO delta — with no previous
                // packet there is no interval for a delta to describe.
                if self.last_seen_day_high > 0.0 {
                    delta.new_high = Some(f32_to_f64_clean(tick.day_high));
                }
                self.last_seen_day_high = tick.day_high;
            } else if tick.day_high < self.last_seen_day_high {
                crate::candles::fold_counters::fold_counters()
                    .session_extreme_regressed_high
                    .increment(1);
            }
        }
        if usable_exchange_price(tick.day_low) {
            if self.last_seen_day_low == 0.0 || tick.day_low < self.last_seen_day_low {
                if self.last_seen_day_low > 0.0 {
                    delta.new_low = Some(f32_to_f64_clean(tick.day_low));
                }
                self.last_seen_day_low = tick.day_low;
            } else if tick.day_low > self.last_seen_day_low {
                crate::candles::fold_counters::fold_counters()
                    .session_extreme_regressed_low
                    .increment(1);
            }
        }

        delta
    }

    /// Snapshot of the open bucket of one timeframe. Cheap `Copy`.
    ///
    /// # Complexity
    /// O(1) — one array index.
    #[inline]
    #[must_use]
    pub fn snapshot(&self, tf: TfIndex) -> LiveCandleState {
        self.slots[tf.as_ordinal()]
    }

    /// Snapshot of the most-recently sealed bucket of one timeframe, or
    /// `None` when nothing is amendable (boot, or after a day-boundary
    /// [`Self::force_seal`]).
    ///
    /// # Complexity
    /// O(1) — one array index.
    #[inline]
    #[must_use]
    pub fn last_sealed_snapshot(&self, tf: TfIndex) -> Option<LiveCandleState> {
        let s = self.last_sealed[tf.as_ordinal()];
        if s.is_uninitialised() { None } else { Some(s) }
    }

    /// Folds one tick into ONE timeframe slot.
    ///
    /// `bucket_start_cumulative` is the instrument's cumulative day volume as
    /// of the END of the previous tick; it becomes the new bucket's volume
    /// baseline when a boundary is crossed.
    ///
    /// `cumulative_volume` is this tick's cumulative day volume, resolved by
    /// the caller as a `u64`. It is an explicit argument rather than a read of
    /// the `u32` `tick.volume` field precisely so a feed whose cumulative
    /// exceeds `u32` (any liquid instrument late in the day) cannot be
    /// silently truncated.
    ///
    /// The caller is responsible for [`tick_price_is_sane`]; this function
    /// assumes a sane price so the check is paid once per tick, not 21 times.
    ///
    /// Convenience wrapper that widens the tick's prices itself. Callers that
    /// fold ONE tick across MANY timeframes must use
    /// [`AggregatorCell::consume_tick_with_prices`] instead and hoist
    /// [`TickPrices::from_tick`] out of their timeframe loop — otherwise the
    /// widening cost is multiplied by the timeframe count for no added
    /// information (see [`TickPrices`]).
    ///
    /// # Complexity
    /// O(1) — one array index, scalar comparisons, no allocation, no loop.
    pub fn consume_tick(
        &mut self,
        tf: TfIndex,
        tick: &ParsedTick,
        bucket_start_cumulative: u64,
        strategy: FeedStrategy,
        cumulative_volume: u64,
    ) -> ConsumeOutcome {
        self.consume_tick_with_prices(
            tf,
            tick,
            TickPrices::from_tick(tick),
            bucket_start_cumulative,
            strategy,
            cumulative_volume,
        )
    }

    /// [`Self::consume_tick_with_prices`] plus the session extremes that moved
    /// on THIS packet, as returned by [`Self::observe_session_extremes`].
    ///
    /// The delta is an ARGUMENT rather than cell state on purpose. Carrying it
    /// on `self` would make correctness depend on an unenforceable 1:1 pairing
    /// between observe and fold: any caller that folded two packets through
    /// one observation would re-apply the same delta to two different buckets.
    /// Passing it makes the pairing structural, and makes every existing
    /// caller of [`Self::consume_tick_with_prices`] fail CLOSED (empty delta,
    /// today's exact LTP-only behaviour) rather than fail stale.
    ///
    /// # Complexity
    /// O(1) — [`Self::consume_tick_with_prices`] plus at most two compares.
    pub fn consume_tick_with_extremes(
        &mut self,
        tf: TfIndex,
        tick: &ParsedTick,
        prices: TickPrices,
        bucket_start_cumulative: u64,
        strategy: FeedStrategy,
        cumulative_volume: u64,
        extremes: SessionExtremeDelta,
    ) -> ConsumeOutcome {
        self.fold(
            tf,
            tick,
            prices,
            bucket_start_cumulative,
            strategy,
            cumulative_volume,
            extremes,
        )
    }

    /// Folds one tick into ONE timeframe slot using PRE-WIDENED prices.
    ///
    /// Identical to [`AggregatorCell::consume_tick`] except the caller supplies
    /// the widened prices, so a multi-timeframe fold pays the `f32`→`f64`
    /// conversion once per TICK rather than once per (tick × timeframe). See
    /// [`TickPrices`] for why that distinction is load-bearing on the hot path.
    ///
    /// `prices` MUST have been derived from `tick` via [`TickPrices::from_tick`];
    /// passing another tick's prices silently folds the wrong price.
    ///
    /// # Complexity
    /// O(1) — one array index, scalar comparisons, no allocation, no loop.
    pub fn consume_tick_with_prices(
        &mut self,
        tf: TfIndex,
        tick: &ParsedTick,
        prices: TickPrices,
        bucket_start_cumulative: u64,
        strategy: FeedStrategy,
        cumulative_volume: u64,
    ) -> ConsumeOutcome {
        self.fold(
            tf,
            tick,
            prices,
            bucket_start_cumulative,
            strategy,
            cumulative_volume,
            SessionExtremeDelta::default(),
        )
    }

    /// The single fold implementation behind all three public entry points.
    // The fold is the single implementation behind three public entry points
    // whose argument lists are fixed by the hot-path contract: prices and
    // cumulative volume are widened once per TICK and passed down, never
    // recomputed per timeframe. Bundling them into a struct would either
    // reintroduce that per-timeframe cost or add an indirection on the
    // per-tick path, for no behavioural gain.
    // APPROVED: argument count is the hot-path contract, see above.
    #[allow(clippy::too_many_arguments)]
    fn fold(
        &mut self,
        tf: TfIndex,
        tick: &ParsedTick,
        prices: TickPrices,
        bucket_start_cumulative: u64,
        strategy: FeedStrategy,
        cumulative_volume: u64,
        extremes: SessionExtremeDelta,
    ) -> ConsumeOutcome {
        let ord = tf.as_ordinal();
        let bucket_start = tf.bucket_start(tick.exchange_timestamp);

        if self.slots[ord].is_uninitialised() {
            // The slot is empty, but `last_sealed` may still hold a bucket
            // drained INTRADAY by `catch_up_seal`. Re-opening a bucket at or
            // before that one would lose its open/high/low and re-baseline
            // volume, then UPSERT a corrupted bar over the sealed one. Route
            // through the same late semantics as the open-slot path instead.
            let last = self.last_sealed[ord];
            if !last.is_uninitialised() && bucket_start <= last.bucket_start_ist_secs {
                if matches!(strategy.late_policy, LatePolicy::Refold)
                    && bucket_start == last.bucket_start_ist_secs
                {
                    fold_late_hlc(&mut self.last_sealed[ord], tick, prices);
                    return ConsumeOutcome::AmendedLate {
                        amended_state: self.last_sealed[ord],
                    };
                }
                return ConsumeOutcome::DiscardLate;
            }
            // 2026-08-19 — THE ARM IS CONSUMED ONLY WHEN IT IS ACTUALLY USED.
            //
            // This used to read `let use = armed[ord]; armed[ord] = false;` —
            // consuming the arm UNCONDITIONALLY, before anyone checked whether
            // `day_open` was usable. `open_bucket` then falls back to the
            // tick's own price when `day_open <= 0.0`, so the two lines
            // together burned the day's one chance at the official open on a
            // tick that could not supply it.
            //
            // That fired every trading day for as long as pre-open ticks could
            // reach this fold: during the pre-open the vendor has no session
            // open to report, so `day_open` is 0. The first such tick burned
            // the arm; every later bucket then saw `use_day_open == false`;
            // and the real 09:15 open was never stamped into ANY candle.
            //
            // **Corrected 2026-08-25:** this said "the persistence window
            // opens at 09:00, so pre-open ticks reach this fold". They no
            // longer do — `MultiTfAggregator::consume_tick` refuses anything
            // with `secs_of_day < MARKET_OPEN_SECS_OF_DAY_IST` before it gets
            // here. The consume-on-use rule below is still right and is kept:
            // it is what makes the arm robust to ANY tick that cannot supply
            // an open, and a comment describing a reachable path that is no
            // longer reachable would send the next reader hunting a defect
            // that has already been closed upstream.
            //
            // Consuming it only on use means a pre-open tick leaves the arm
            // intact, and the first tick that actually carries a positive
            // `day_open` — in practice the first tick at or after the official
            // open — is the one that spends it.
            //
            // HONEST RESIDUAL: this makes the arm wait for a usable value, not
            // for a particular clock time. If the vendor were to withhold
            // `day_open` past 09:15, the first bucket that DOES see it would
            // open at the session open rather than at its own first trade.
            // That is a bounded, one-bucket inaccuracy and strictly better
            // than losing the official open for the whole day; pinning it to a
            // session-time window would put trading-calendar knowledge inside
            // this cell, which is the wrong place for it.
            let use_day_open = self.armed_for_day_open[ord]
                && prices.day_open > 0.0
                && is_days_first_session_bucket(tf, bucket_start);
            if use_day_open {
                self.armed_for_day_open[ord] = false;
            }
            // "The day's first bucket" is DERIVED, not tracked: nothing has
            // sealed for this timeframe yet today. `force_seal` (the day
            // boundary) clears `last_sealed` and re-arms, so the signal resets
            // across days on its own. Chosen over a new `[bool; TF_COUNT]`
            // array because it costs 0 bytes and cannot drift out of sync with
            // the seal path it is read from.
            let first_bucket_of_day = self.last_sealed[ord].is_uninitialised()
                && is_days_first_session_bucket(tf, bucket_start);
            self.slots[ord] = open_bucket(
                tick,
                prices,
                bucket_start,
                bucket_start_cumulative,
                use_day_open,
                first_bucket_of_day,
                cumulative_volume,
            );
            return ConsumeOutcome::Updated;
        }

        let open_start = self.slots[ord].bucket_start_ist_secs;

        // In-bucket — the common case, and the one that must survive many
        // ticks sharing one second.
        if bucket_start == open_start {
            // A day_open that arrives AFTER the bucket opened still belongs to
            // it. The arm is only ever live inside the day's first bucket (the
            // roll below disarms on the way out), so this cannot stamp a later
            // bar. Without it, an instrument whose first in-session tick
            // carried no session open would lose the official open for the
            // whole day — the bucket is already open, and `fold_in_bucket`
            // deliberately never touches `open`.
            if self.armed_for_day_open[ord]
                && prices.day_open > 0.0
                && is_days_first_session_bucket(tf, open_start)
            {
                self.armed_for_day_open[ord] = false;
                self.slots[ord].open = prices.day_open;
                // The MORE dangerous of the two `day_open` stamp sites: this
                // bucket has already folded ticks, so its range can be far
                // from the official open by now. Without this widening the
                // bar publishes an `open` outside its own `[low, high]`.
                widen_range_to_include(&mut self.slots[ord], prices.day_open);
            }
            // THE ATTRIBUTION TEST, and the only thing that makes a session-
            // extreme delta usable: did the PREVIOUS observed packet also fall
            // in THIS bucket? If it did, the interval the delta describes lies
            // wholly inside this bucket, so the extreme was set here. If it
            // did not — the packet before was late-routed, or opened an
            // earlier bucket — the interval straddles a boundary and the
            // extreme's bucket is genuinely unknown. Refuse rather than guess.
            //
            // Deliberately NOT `tick_count > 0`: `open_bucket` stamps
            // `tick_count: 1`, so that test is always true on this path and
            // proves nothing. This one asks the real question.
            //
            // Guarded by `is_empty()` so a quiet packet — the overwhelming
            // majority — never pays the bucket arithmetic.
            let attributable =
                !extremes.is_empty() && tf.bucket_start(extremes.prev_observed_ts) == bucket_start;
            fold_in_bucket(&mut self.slots[ord], tick, prices, cumulative_volume);
            // Session extremes keep arriving through the first bucket's life,
            // so re-adopt on every tick of it — `day_high` at the bucket's LAST
            // tick is the one that matters, and max/min converge to it.
            if self.last_sealed[ord].is_uninitialised()
                && is_days_first_session_bucket(tf, open_start)
            {
                adopt_exchange_day_extremes(&mut self.slots[ord], tick);
            } else if attributable {
                adopt_session_extreme_delta(&mut self.slots[ord], extremes);
            }
            return ConsumeOutcome::Updated;
        }

        // Strictly newer bucket — seal the open one and open the new one at
        // the LTP (an intraday crossing is never the day's first bar).
        //
        // 2026-08-19 — the `false` is unchanged, but the DISARM beside it is
        // new. Leaving here means the day's first bucket is behind us, so the
        // official open can never legitimately be stamped again; disarming
        // makes that structural rather than incidental, and is what lets the
        // in-bucket path below stamp a late-arriving `day_open` without any
        // risk of a later bucket claiming it.
        if bucket_start > open_start {
            self.armed_for_day_open[ord] = false;
            let sealed_state = std::mem::replace(
                &mut self.slots[ord],
                open_bucket(
                    tick,
                    prices,
                    bucket_start,
                    bucket_start_cumulative,
                    false,
                    // An intraday crossing is never the day's first bar, so
                    // the running session extremes must NOT be adopted here —
                    // this is the scope guarantee of plan Item 6.
                    false,
                    cumulative_volume,
                ),
            );
            self.last_sealed[ord] = sealed_state;
            return ConsumeOutcome::Sealed { sealed_state };
        }

        // Older bucket — late arrival.
        if matches!(strategy.late_policy, LatePolicy::Refold) {
            let last = self.last_sealed[ord];
            if !last.is_uninitialised() && bucket_start == last.bucket_start_ist_secs {
                fold_late_hlc(&mut self.last_sealed[ord], tick, prices);
                return ConsumeOutcome::AmendedLate {
                    amended_state: self.last_sealed[ord],
                };
            }
        }
        ConsumeOutcome::DiscardLate
    }

    /// Day-boundary force-seal of one timeframe slot.
    ///
    /// Returns `Some(sealed)` only when the slot actually held an opened
    /// bucket — an untouched slot returns `None` and emits NOTHING. That is
    /// the sparsity guarantee at the seal path.
    ///
    /// Re-arms the slot for day-open, and CLEARS `last_sealed` so a stray
    /// late tick can never UPSERT a previous-day bar.
    ///
    /// # Complexity
    /// O(1) — one array index.
    pub fn force_seal(&mut self, tf: TfIndex) -> Option<LiveCandleState> {
        let ord = tf.as_ordinal();
        self.armed_for_day_open[ord] = true;
        self.last_sealed[ord] = LiveCandleState::empty();
        // The session-extreme baseline is a DAY-scoped quantity, so the day
        // boundary must drop it: carrying yesterday's high across midnight
        // would make today's genuinely-lower session high look like a fall and
        // suppress every rise until it exceeded yesterday's. Clearing it here
        // is idempotent across the 24 per-timeframe calls the boundary makes,
        // and a partial (single-timeframe) force-seal only ever costs a missed
        // widening — never a wrong one.
        self.last_seen_day_high = 0.0;
        self.last_seen_day_low = 0.0;
        self.last_observed_ts = 0;
        if self.slots[ord].is_uninitialised() {
            return None;
        }
        Some(std::mem::replace(
            &mut self.slots[ord],
            LiveCandleState::empty(),
        ))
    }

    /// Watermark-aware INTRADAY catch-up seal: seals the open bucket ONLY
    /// when its exclusive end is at or before `cutoff_secs`.
    ///
    /// Without this, an illiquid instrument's bar seals only when its NEXT
    /// tick arrives — which can be minutes later, or never before the day
    /// boundary. With it, the caller (driven by the feed's event-time
    /// watermark minus an allowed-lateness margin) closes bars on time
    /// without ever sealing one whose final ticks are still plausibly in
    /// flight.
    ///
    /// Unlike [`Self::force_seal`] this does NOT re-arm day-open (an
    /// intraday re-arm would open a mid-session bucket at `day_open`) and it
    /// POPULATES `last_sealed`, so the late-tick amend keeps working and the
    /// uninitialised-slot guard in [`Self::consume_tick`] can refuse to
    /// re-open the sealed bucket.
    ///
    /// # Complexity
    /// O(1) — one array index.
    pub fn catch_up_seal(&mut self, tf: TfIndex, cutoff_secs: u32) -> Option<LiveCandleState> {
        let ord = tf.as_ordinal();
        if self.slots[ord].is_uninitialised() {
            return None;
        }
        // Saturating: a bucket_start near u32::MAX (reachable only from a
        // poisoned timestamp) must not overflow-panic under the release
        // profile's `overflow-checks = true`. Saturation pins the end past
        // every real cutoff, so the bucket simply never seals — fail-safe.
        if self.slots[ord]
            .bucket_start_ist_secs
            .saturating_add(tf.seconds_per_bucket())
            > cutoff_secs
        {
            return None;
        }
        let sealed = std::mem::replace(&mut self.slots[ord], LiveCandleState::empty());
        self.last_sealed[ord] = sealed;
        Some(sealed)
    }
}

// ---------------------------------------------------------------------------
// Fold primitives (free functions — the cell's slots are plain values)
// ---------------------------------------------------------------------------
/// Widens `[low, high]` so it contains `price`.
///
/// Every use of this in the fold is MONOTONE WIDENING: it can only ever
/// enlarge a bar's range, never shrink it. That property is the entire safety
/// argument for adopting exchange-published values — no input, however stale
/// or corrupt, can narrow a range or discard a price we actually observed.
#[inline]
fn widen_range_to_include(state: &mut LiveCandleState, price: f64) {
    if price > state.high {
        state.high = price;
    }
    if price < state.low {
        state.low = price;
    }
}

/// True when `bucket_start` is the bucket that CONTAINS the day's 09:15 open,
/// for this timeframe.
///
/// The session-open stamp and the session-extreme LEVEL adoption are both
/// correct for exactly one bucket per timeframe per day, and both used to be
/// gated on `last_sealed[ord].is_uninitialised()` — "nothing has sealed yet".
/// A permutation sweep on 2026-08-25 showed that is a different question with
/// the same answer only on a clean 09:15 start. It is ALSO true for:
///
/// - an instrument first seen MID-SESSION (a contract attached at 11:00, a
///   process restart), whose 11:00 bar then adopted the whole running session
///   range — an observed single tick at 100 producing `high 180 / low 60`;
/// - the bucket after an intraday `catch_up_seal`, which clears the slot but
///   deliberately does not re-arm, so a later bucket could open at the 09:15
///   official price and widen its own `low` down to it — a fabricated open
///   and low on a mid-session candle, which then increments
///   `tv_candle_open_clamped_total` and reads as a legitimate gap-open.
///
/// Deriving the answer from the CLOCK instead of from seal history closes all
/// three. It is exact for every timeframe and including D1, whose bucket is
/// the whole day.
///
/// **Corrected 2026-08-25.** This paragraph used to say M30 and M60 "resolve to
/// the 09:00 bucket, which is the one that contains the open". They do not, and
/// no timeframe has a 09:00 bucket: [`TfIndex::bucket_start`] is anchored at
/// 09:15, not at the epoch, so M30's first bucket is `[09:15, 09:45)`. A reader
/// trusting the old wording would conclude this function needs a pre-open
/// carve-out it does not need. The 09:15 anchoring is also what makes the test
/// reduce to `bucket_start == day_start + 33_300` for every timeframe.
///
/// # Complexity
/// O(1) — one remainder, one bucket alignment, one compare.
#[inline]
fn is_days_first_session_bucket(tf: TfIndex, bucket_start: u32) -> bool {
    let day_start = bucket_start - (bucket_start % 86_400);
    // `saturating_add`, not `+` (2026-08-25). The release profile is
    // `overflow-checks = true, panic = "abort"`, so an overflowing add here
    // does not return a wrong answer — it kills the process, taking all 16
    // sockets with it, for one tick. `day_start` can reach 4_294_944_000, and
    // `+ 33_300` exceeds `u32::MAX`.
    //
    // Unreachable through today's only caller (the plausibility bounds and the
    // session gate both block it), and hardened anyway for exactly the reason
    // `TfIndex::bucket_start` and `catch_up_seal` were: a second call site
    // would silently reintroduce a remote abort, and the cost of preventing
    // that is one word.
    bucket_start == tf.bucket_start(day_start.saturating_add(MARKET_OPEN_SECS_OF_DAY_IST))
}

/// True when `raw` is a usable exchange-published price.
///
/// `> 0.0` alone is NOT sufficient and the difference is load-bearing:
/// - it correctly rejects `NaN` (every comparison with `NaN` is false), but
/// - it ACCEPTS `+∞`, which would set a bar's `high` to infinity permanently.
///
/// `0.0` is the documented ABSENT sentinel — Ticker-mode packets carry no day
/// fields at all, so a bare `0.0` must never be read as a real price of zero.
///
/// Two further rejections were added 2026-08-25 after a permutation sweep
/// showed the LTP guard's protections were never extended to the day fields,
/// even though both come off the wire the same way (`read_f32_le`):
///
/// - **`MAX_PLAUSIBLE_LTP` ceiling.** `tick_price_is_sane` has bounded the LTP
///   since 2026-08-15, but a mangled frame decoding `f32::MAX` in `day_high`
///   went straight into a bar's `high` — and, through `TickPrices`, into
///   `open`, `session_open` and `prev_day_close` as well. One packet, four
///   poisoned columns.
/// - **`is_normal()`.** A positive SUBNORMAL passes every finiteness and sign
///   test, but `f32_to_f64_clean` round-trips through a fixed 24-byte decimal
///   buffer that a subnormal overruns, so it parses back as `0.0` — which then
///   wins the `<` comparison and sets a bar's `low` to zero. `is_normal()`
///   excludes exactly the subnormals (and `NaN`, `±∞`, `0.0`) and excludes no
///   price any exchange can quote: the smallest normal `f32` is ~1.2e-38.
#[inline]
fn usable_exchange_price(raw: f32) -> bool {
    raw.is_normal() && raw > 0.0 && raw <= MAX_PLAUSIBLE_LTP
}

/// Builds the state of a bucket being opened by `tick`.
///
/// `use_day_open` makes the bar open at the exchange-published `day_open`
/// (the day's FIRST bar of each timeframe); it falls back to the LTP when
/// `day_open` is absent (`0.0` pre-open, or a malformed packet).
///
/// `first_bucket_of_day` additionally adopts the exchange-published
/// `day_high` / `day_low` as this bar's extremes (operator 2026-08-19, plan
/// Item 6). It is correct for the day's FIRST bucket ONLY, because those
/// fields are running SESSION extremes — for any later bucket they describe
/// the whole day rather than that bucket, which is why every other bucket
/// keeps tracking the LTP alone.
///
/// # The invariant this function is responsible for
///
/// `low <= open <= high` on the returned state. Before 2026-08-19 it was NOT
/// upheld: `open` was stamped from `day_open` while `high`/`low` were seeded
/// from the LTP, so a gap-open morning (`day_open = 100`, first trade `105`)
/// published `open=100, high=105, low=105` — an open below its own low. See
/// plan Item 6 / C1 for the consumers that corrupts.
#[inline]
fn open_bucket(
    tick: &ParsedTick,
    prices: TickPrices,
    bucket_start: u32,
    bucket_start_cumulative: u64,
    use_day_open: bool,
    first_bucket_of_day: bool,
    cumulative_volume: u64,
) -> LiveCandleState {
    let price = prices.last_traded_price;
    // `day_open` is already `0.0` unless the raw field was strictly positive
    // (NaN included), so this test carries the original `> 0.0` semantics.
    let open = if use_day_open && prices.day_open > 0.0 {
        prices.day_open
    } else {
        price
    };
    let mut state = LiveCandleState {
        bucket_start_ist_secs: bucket_start,
        open,
        high: price,
        low: price,
        close: price,
        volume: cumulative_volume.saturating_sub(bucket_start_cumulative),
        bucket_start_cumulative,
        oi: i64::from(tick.open_interest),
        tick_count: 1,
        close_ts_ist_secs: tick.exchange_timestamp,
        // Same reasoning as `session_open` below: the gated widened value,
        // never the raw wire field.
        prev_day_close: prices.day_close,
        close_pct_from_prev_day: 0.0,
        oi_pct_from_prev_day: 0.0,
        volume_pct_from_prev_day: 0.0,
        // Uses the ALREADY-GATED widened value, not the raw wire field. The
        // raw read here was the last hole through which an absurd or
        // subnormal `day_open` reached a persisted column after every other
        // site had been closed (permutation sweep 2026-08-25).
        session_open: prices.day_open,
        open_pct: 0.0,
        open_gap_pct: 0.0,
    };
    // The official open is a REAL matched trade (the pre-open call auction
    // equilibrium), so it genuinely belongs inside this bar's range. Widen
    // rather than clamp `open`: clamping would discard exchange truth to
    // satisfy an invariant, which is backwards.
    //
    // The counter is the honest evidence of how real the defect was: every
    // increment is a bar that WOULD have published an open outside its own
    // range before 2026-08-19. On a calm morning it stays 0; on a gap-open
    // it fires once per instrument per timeframe.
    if open < state.low || open > state.high {
        crate::candles::fold_counters::fold_counters()
            .open_clamped
            .increment(1);
    }
    widen_range_to_include(&mut state, open);
    if first_bucket_of_day {
        adopt_exchange_day_extremes(&mut state, tick);
    }
    state
}

/// Adopts the exchange-published session extremes into a bar's range.
///
/// **Only ever call this for the day's FIRST bucket.** `day_high` / `day_low`
/// are RUNNING SESSION extremes: during the first bucket they describe
/// exactly that bucket (nothing earlier exists to have set them), but from the
/// second bucket onward they describe the whole day and would smear the
/// session range across every bar.
///
/// Both fields are read raw off `&ParsedTick` and widened inline here rather
/// than being carried on [`TickPrices`]. That is deliberate: [`TickPrices`] is
/// built once per tick for the whole session, so adding these two would pay
/// ~100 ns of `f32_to_f64_clean` on EVERY tick to serve one bucket per day.
/// Here the conversion runs during the first bucket only — roughly one minute
/// out of 375 — and costs exactly zero for the rest of the session.
///
/// # Complexity
/// O(1) — two validity checks, at most two widenings. No allocation.
#[inline]
fn adopt_exchange_day_extremes(state: &mut LiveCandleState, tick: &ParsedTick) {
    if usable_exchange_price(tick.day_high) {
        let dh = f32_to_f64_clean(tick.day_high);
        if dh > state.high {
            state.high = dh;
            crate::candles::fold_counters::fold_counters()
                .day_high_adopted
                .increment(1);
        }
    }
    if usable_exchange_price(tick.day_low) {
        let dl = f32_to_f64_clean(tick.day_low);
        if dl < state.low {
            state.low = dl;
            crate::candles::fold_counters::fold_counters()
                .day_low_adopted
                .increment(1);
        }
    }
}

/// Widens a bar to include a session extreme that the exchange set INSIDE it.
///
/// The caller must have established attribution first — both the previous and
/// the current packet inside this bucket (see
/// [`AggregatorCell::observe_session_extremes`]). Given that, this is not an
/// estimate: `day_high` is the exchange's own figure over the full tape, so
/// the widening replaces a value we sampled with the value that actually
/// printed.
///
/// Widening only ever expands `[low, high]`, so it cannot invalidate
/// `low <= open,close <= high`; and a delta that the LTP already covered is a
/// no-op rather than a double count.
///
/// # Complexity
/// O(1) — at most two compares and two stores. No allocation.
#[inline]
fn adopt_session_extreme_delta(state: &mut LiveCandleState, delta: SessionExtremeDelta) {
    if let Some(new_high) = delta.new_high
        && new_high > state.high
    {
        state.high = new_high;
        crate::candles::fold_counters::fold_counters()
            .session_high_recovered
            .increment(1);
    }
    if let Some(new_low) = delta.new_low
        && new_low < state.low
    {
        state.low = new_low;
        crate::candles::fold_counters::fold_counters()
            .session_low_recovered
            .increment(1);
    }
}

/// Folds an in-bucket tick. The caller has already established that the tick
/// belongs to THIS bucket.
#[inline]
fn fold_in_bucket(
    state: &mut LiveCandleState,
    tick: &ParsedTick,
    prices: TickPrices,
    cumulative_volume: u64,
) {
    let price = prices.last_traded_price;
    if price > state.high {
        state.high = price;
    }
    if price < state.low {
        state.low = price;
    }
    // ORDER GUARD (2026-08-25, permutation sweep). `fold_late_hlc` — the
    // SEALED-bucket path — has always had this test; the OPEN-bucket path did
    // not, so an out-of-order packet arriving inside a still-open bucket
    // overwrote `close` with an EARLIER price and moved `close_ts` backwards.
    // Two paths, two policies, and only one of them was right.
    //
    // The damage scaled with the bucket: on a 1-minute bar the window is 60
    // seconds, but on the daily bar it is the whole session, so ANY reordered
    // packet could rewrite the day's close — making it whichever packet
    // arrived last rather than the one that traded last. `>=` is deliberate:
    // many packets share one LTT second, and within a second last-write-wins
    // is the pre-existing, correct behaviour.
    if tick.exchange_timestamp >= state.close_ts_ist_secs {
        state.close = price;
        state.close_ts_ist_secs = tick.exchange_timestamp;
        // MERGE 2026-08-25: the order guard above and the non-zero guard here
        // fix DIFFERENT halves of the same field, and OI needs BOTH.
        //
        // The order guard answers "is this packet newer?". It cannot answer
        // "does this packet carry an OI reading at all". `0` is the ABSENT
        // sentinel — a Ticker-mode packet has no OI field and an equity never
        // has one — so a NEWER blank packet passes the order guard and would
        // still erase a real OI that an earlier tick in the SAME bucket had
        // established. Order alone does not make a blank field into news.
        //
        // Last NON-ZERO wins, exactly like `prev_day_close` / `session_open`
        // below, which carry the same reasoning for the same reason.
        if tick.open_interest != 0 {
            state.oi = i64::from(tick.open_interest);
        } else if state.oi != 0 {
            crate::candles::fold_counters::fold_counters()
                .oi_zero_ignored
                .increment(1);
        }
    }
    // Exchange cumulative volume only ever rises, so a bucket's traded volume
    // is monotone too. `saturating_sub` bounded the ARITHMETIC against
    // underflow, which is not the same thing as bounding the BAR: a stale
    // packet carries a smaller day-cumulative, so the difference is smaller
    // too, and the assignment dragged an already-correct volume back down.
    // The bar then sealed under-reporting while the NEXT bar reported the gap
    // — the volume moved between buckets instead of staying put.
    //
    // Widening only, kept as an explicit compare rather than `.max()` so the
    // suppressed case is COUNTED. A silent `.max()` is correct and tells you
    // nothing: the regression RATE is what says whether this guard is
    // load-bearing today or dormant, and that is worth a counter.
    let bucket_volume = cumulative_volume.saturating_sub(state.bucket_start_cumulative);
    if bucket_volume > state.volume {
        state.volume = bucket_volume;
    } else if bucket_volume < state.volume {
        crate::candles::fold_counters::fold_counters()
            .volume_regression_suppressed
            .increment(1);
    }
    state.tick_count = state.tick_count.saturating_add(1);
    // Last NON-ZERO wins: a blank pre-market 0 must never clobber a real
    // baseline captured earlier in the session. The widened fields are `0.0`
    // unless the raw field was strictly positive, so testing the CONVERTED
    // value preserves the original `tick.day_* > 0.0` semantics exactly.
    if prices.day_close > 0.0 {
        state.prev_day_close = prices.day_close;
    }
    if prices.day_open > 0.0 {
        state.session_open = prices.day_open;
    }
}

/// Folds a LATE tick into an already-sealed bucket's high / low / close.
///
/// `close` is overwritten ONLY when the late tick is genuinely the bucket's
/// last tick (`exchange_timestamp >= close_ts_ist_secs`), so an out-of-order
/// EARLIER late tick can never clobber a truly-later close. `open` /
/// `volume` / `oi` are untouched: `open` belongs to the first tick, and the
/// cumulative snapshots are order-dependent and ambiguous for a latecomer.
#[inline]
fn fold_late_hlc(state: &mut LiveCandleState, tick: &ParsedTick, prices: TickPrices) {
    let price = prices.last_traded_price;
    if price > state.high {
        state.high = price;
    }
    if price < state.low {
        state.low = price;
    }
    if tick.exchange_timestamp >= state.close_ts_ist_secs {
        state.close = price;
        state.close_ts_ist_secs = tick.exchange_timestamp;
    }
    state.tick_count = state.tick_count.saturating_add(1);
}

// Per-instrument RAM pin. TF_COUNT × 128 B × 2 arrays (`slots` +
// `last_sealed`) + TF_COUNT flags, padded.
//
// RAISED 2026-08-10: 5_632 → 6_400 for TF_COUNT 21 → 24 (M2/M30/M60
// appended to complete operator Quote 13's thirteen frames). The bound is
// deliberately derived from TF_COUNT rather than re-frozen as a literal, so
// the next frame change moves it arithmetically instead of tripping a
// mystery const-assert.
//
// Fleet cost at the slot ceiling, stated because this constant multiplies:
//   24 TF × 128 B × 2 = 6_144 B, padded ≤ 6_400 B per instrument
//   × AGGREGATOR_MAX_SLOTS (25,000) = ~160 MB
// against the r8g.xlarge 32 GiB host (operator Quote 13) that is 0.49% —
// up from ~141 MB at 21 frames. On the retired 4 GiB t4g.medium the same
// table would have been ~3.9% of the entire machine, which is the sort of
// number that used to make "just add three timeframes" a real decision.
const MAX_AGGREGATOR_CELL_BYTES: usize = TF_COUNT * 128 * 2 + TF_COUNT * 4 + 160;
const _: () = assert!(
    std::mem::size_of::<AggregatorCell>() <= MAX_AGGREGATOR_CELL_BYTES,
    "AggregatorCell exceeded its per-instrument budget — this multiplies by AGGREGATOR_MAX_SLOTS (25,000); update aws-budget.md before raising."
);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// An exact multiple of 86_400 so `DAY + 33_300` is the 09:15 open.
    pub(super) const DAY: u32 = 1_779_321_600;
    /// 09:15:00 IST of [`DAY`].
    pub(super) const OPEN: u32 = DAY + 33_300;

    // -- volume / oi regression guards (live defect, measured 2026-08-24) ----

    #[test]
    fn an_out_of_order_tick_inside_an_open_bucket_must_not_lower_the_bars_volume() {
        // BITE PROOF: on the pre-fix `state.volume = cumulative - baseline`
        // (last-write-wins) this asserts 500 == 900 and FAILS.
        //
        // `tick.volume` is DAY-CUMULATIVE. Two ticks land in the same minute
        // and the second is the EARLIER one (out of order on the wire), so it
        // carries a smaller cumulative. Its arrival must not un-count volume
        // the bucket has already legitimately observed.
        let mut cell = AggregatorCell::empty();
        let strategy = FeedStrategy::DEFAULT;

        cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN, 100.0, 1_000),
            100,
            strategy,
            1_000,
        );
        assert_eq!(cell.snapshot(TfIndex::M1).volume, 900);

        // Same bucket, out of order: earlier cumulative, later arrival.
        cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 5, 101.0, 600),
            100,
            strategy,
            600,
        );
        assert_eq!(
            cell.snapshot(TfIndex::M1).volume,
            900,
            "in-bucket volume widens only; a stale cumulative is not news"
        );

        // A genuinely larger cumulative still advances it.
        cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 9, 102.0, 1_500),
            100,
            strategy,
            1_500,
        );
        assert_eq!(cell.snapshot(TfIndex::M1).volume, 1_400);
    }

    #[test]
    fn an_open_interest_of_zero_must_not_clobber_a_previously_non_zero_oi() {
        // BITE PROOF: on the pre-fix unconditional `state.oi = ...` this
        // asserts 4_200 == 0 and FAILS.
        //
        // `0` is the ABSENT sentinel for open interest — Ticker-mode packets
        // carry no OI field at all — exactly as it is for `day_close` /
        // `day_open` three lines below in the same function, which already
        // use last-NON-ZERO-wins.
        let mut cell = AggregatorCell::empty();
        let strategy = FeedStrategy::DEFAULT;

        let mut with_oi = tick_at(OPEN, 100.0, 10);
        with_oi.open_interest = 4_200;
        cell.consume_tick(TfIndex::M1, &with_oi, 0, strategy, 10);
        assert_eq!(cell.snapshot(TfIndex::M1).oi, 4_200);

        // Same bucket, a packet with no OI field populated.
        let blank = tick_at(OPEN + 5, 101.0, 20);
        assert_eq!(blank.open_interest, 0, "fixture models the absent case");
        cell.consume_tick(TfIndex::M1, &blank, 0, strategy, 20);
        assert_eq!(
            cell.snapshot(TfIndex::M1).oi,
            4_200,
            "an absent OI must not erase a real one captured earlier in the bar"
        );

        // A real later value still wins.
        let mut newer = tick_at(OPEN + 9, 102.0, 30);
        newer.open_interest = 4_500;
        cell.consume_tick(TfIndex::M1, &newer, 0, strategy, 30);
        assert_eq!(cell.snapshot(TfIndex::M1).oi, 4_500);
    }

    pub(super) fn tick_at(ts: u32, price: f32, cum_volume: u32) -> ParsedTick {
        ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: price,
            exchange_timestamp: ts,
            volume: cum_volume,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn test_a_late_arriving_day_open_still_reaches_the_days_first_bucket() {
        // WHAT THIS ACTUALLY PINS (operator asked 2026-08-19 whether the
        // candle opens at the pre-market price; investigation below).
        //
        // I first believed pre-open ticks reached this fold and stamped the
        // 09:15 candle with a pre-market price. THEY DO NOT — the aggregator
        // refuses anything with `secs_of_day < MARKET_OPEN_SECS_OF_DAY_IST`
        // one layer up, so `bucket_start`'s clamp of pre-open timestamps into
        // the 09:15 bucket is defence-in-depth for a caller that does not
        // exist. The reported bug was not real, and this test exists for the
        // NARROWER case that is.
        //
        // The real risk: the day's first IN-SESSION tick can carry
        // `day_open == 0` — a thin instrument whose session open the vendor
        // has not populated yet. The bucket opens at that tick's price, and
        // `fold_in_bucket` never touches `open`, so without this path the
        // official open would never reach any candle that day.
        let mut cell = AggregatorCell::empty();
        let strategy = FeedStrategy::DEFAULT;

        // First in-session tick, but no session open published yet.
        let first = tick_at(OPEN, 23_990.00, 5);
        assert_eq!(first.day_open, 0.0, "fixture models the unpopulated case");
        cell.consume_tick(TfIndex::M1, &first, 0, strategy, 5);
        assert_eq!(
            cell.snapshot(TfIndex::M1).open,
            f32_to_f64_clean(23_990.00),
            "with nothing better available the bucket opens at the traded price"
        );

        // Same bucket, seconds later: the official open arrives.
        let mut later = tick_at(OPEN + 10, 24_010.00, 12);
        later.day_open = 24_000.25;
        cell.consume_tick(TfIndex::M1, &later, 0, strategy, 12);
        assert_eq!(
            cell.snapshot(TfIndex::M1).open,
            f32_to_f64_clean(24_000.25),
            "a day_open arriving after the bucket opened still belongs to it"
        );
    }

    #[test]
    fn test_the_day_open_arm_cannot_be_spent_by_any_bucket_after_the_first() {
        // The other half, and the reason the roll disarms: once the day's
        // first bucket is behind us the official open must never be stamped
        // again. A permanently-live arm would open every later bar at the
        // session open — the opposite error, and equally wrong.
        let mut cell = AggregatorCell::empty();
        let strategy = FeedStrategy::DEFAULT;

        // First bucket, no day_open available — arm stays live.
        let first = tick_at(OPEN, 23_990.00, 5);
        cell.consume_tick(TfIndex::M1, &first, 0, strategy, 5);

        // Roll into the next minute; day_open is on the wire now, but this
        // bucket is NOT the day's first and must open at its own price.
        let mut second = tick_at(OPEN + 60, 24_055.5, 20);
        second.day_open = 24_000.25;
        cell.consume_tick(TfIndex::M1, &second, 0, strategy, 20);
        assert_eq!(
            cell.snapshot(TfIndex::M1).open,
            f32_to_f64_clean(24_055.5),
            "every bucket after the first opens at its OWN first traded price"
        );
    }

    #[test]
    fn test_tick_prices_from_tick_widening_is_pure_and_matches_the_inline_fold() {
        // Widening must be a pure function of the tick — that is precisely
        // what makes hoisting it out of the timeframe loop safe. If this ever
        // stops holding, one derivation per tick is NOT interchangeable with
        // TF_COUNT of them and `consume_tick_with_prices` becomes unsound.
        let t = tick_at(OPEN, 24_000.05, 10);
        assert_eq!(TickPrices::from_tick(&t), TickPrices::from_tick(&t));

        // And it must agree, bit for bit, with the conversion the fold used to
        // perform inline — this is the equivalence the hoist relies on.
        let mut full = tick_at(OPEN, 24_000.05, 10);
        full.day_open = 24_000.25;
        full.day_close = 23_950.75;
        let p = TickPrices::from_tick(&full);
        assert_eq!(
            p.last_traded_price,
            f32_to_f64_clean(full.last_traded_price)
        );
        assert_eq!(p.day_open, f32_to_f64_clean(full.day_open));
        assert_eq!(p.day_close, f32_to_f64_clean(full.day_close));
    }

    #[test]
    fn test_tick_prices_subnormal_day_field_collapses_to_sentinel() {
        // Pins the HONEST LIMIT documented on `TickPrices`: the widening is
        // NOT bit-identical to the old raw-value guard for subnormal f32.
        // `f32_to_f64_clean` formats through a 24-byte buffer and Rust's f32
        // Display never uses scientific notation, so a positive subnormal
        // overflows it and parses back to 0.0.
        let mut t = tick_at(OPEN, 24_000.05, 10);
        t.day_open = f32::MIN_POSITIVE;
        let p = TickPrices::from_tick(&t);
        assert!(
            t.day_open > 0.0,
            "precondition: the RAW value is strictly positive"
        );
        assert_eq!(
            p.day_open, 0.0,
            "a subnormal day_open widens to the 0.0 sentinel — the documented \
             divergence from the old raw-value guard, not an identity"
        );
    }

    #[test]
    fn test_tick_prices_collapses_absent_and_nan_day_fields_to_sentinel() {
        // `> 0.0` is false for NaN, so a poisoned day field must land on the
        // 0.0 sentinel rather than propagate — otherwise NaN would be
        // absorbing under the fold's comparisons and poison the bar. Callers
        // test the CONVERTED value, so this preserves the original semantics.
        let mut blank = tick_at(OPEN, 24_000.05, 10);
        blank.day_open = 0.0;
        blank.day_close = f32::NAN;
        let p = TickPrices::from_tick(&blank);
        assert_eq!(
            p.day_open, 0.0,
            "absent day_open must widen to the sentinel"
        );
        assert_eq!(
            p.day_close, 0.0,
            "NaN day_close must collapse to the sentinel, never propagate"
        );
        assert!(
            p.last_traded_price > 0.0,
            "a sane LTP still widens normally"
        );

        // A NEGATIVE day field is equally non-adoptable.
        let mut neg = tick_at(OPEN, 24_000.05, 10);
        neg.day_open = -1.0;
        assert_eq!(TickPrices::from_tick(&neg).day_open, 0.0);
    }

    #[test]
    fn test_consume_tick_with_prices_matches_the_convenience_wrapper() {
        // The wrapper must be a pure delegation: same outcome, same state.
        // If these ever diverge, the hot loop and every test call site are
        // exercising different code.
        let t = tick_at(OPEN, 24_000.05, 10);
        let mut a = AggregatorCell::empty();
        let mut b = AggregatorCell::empty();

        let out_a = a.consume_tick(TfIndex::M1, &t, 0, FeedStrategy::DEFAULT, 10);
        let out_b = b.consume_tick_with_prices(
            TfIndex::M1,
            &t,
            TickPrices::from_tick(&t),
            0,
            FeedStrategy::DEFAULT,
            10,
        );

        assert_eq!(
            format!("{out_a:?}"),
            format!("{out_b:?}"),
            "wrapper and hoisted-price path must return the same outcome"
        );
        assert_eq!(
            a.snapshot(TfIndex::M1),
            b.snapshot(TfIndex::M1),
            "wrapper and hoisted-price path must leave identical bar state"
        );
    }

    #[test]
    fn test_tick_price_is_sane_rejects_nan_inf_and_nonpositive() {
        assert!(tick_price_is_sane(&tick_at(OPEN, 100.0, 0)));
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            assert!(
                !tick_price_is_sane(&tick_at(OPEN, bad, 0)),
                "price {bad} must be refused"
            );
        }
    }

    /// The half the finite/positive gate never covered: an absurd-but-FINITE
    /// price. `f32::MAX` passes `is_finite() && > 0.0` cleanly, and before the
    /// ceiling was wired it pinned the bucket's running-max `high` at 3.4e38
    /// for the rest of the minute. Bite-proven: delete the `<= MAX_PLAUSIBLE_LTP`
    /// term and the `f32::MAX` / `1e30` rows below flip to accepted.
    #[test]
    fn test_tick_price_is_sane_rejects_absurd_but_finite_prices() {
        for absurd in [f32::MAX, 1e30, 1e20, MAX_PLAUSIBLE_LTP * 1.5] {
            assert!(
                !tick_price_is_sane(&tick_at(OPEN, absurd, 0)),
                "price {absurd} is finite and positive but 500x+ above any real \
                 NSE instrument — it must be refused before it poisons a high/low"
            );
        }
    }

    /// The ceiling must never cost a real quote. MRF (~1.5 lakh) is the most
    /// expensive real NSE instrument; the boundary itself is inclusive.
    #[test]
    fn test_tick_price_is_sane_accepts_every_real_instrument_price() {
        for real in [0.05, 1.0, 52_000.0, 82_000.0, 150_000.0, MAX_PLAUSIBLE_LTP] {
            assert!(
                tick_price_is_sane(&tick_at(OPEN, real, 0)),
                "price {real} is a plausible quote and must be folded"
            );
        }
    }

    /// The ceiling is checked on the RAW `f32`, deliberately before widening.
    /// This pins WHY: `f32_to_f64_clean(f32::MAX)` is `3.4028235e23`, not
    /// `3.4e38` (the buffer-truncation limit documented on `TickPrices`), so a
    /// post-widening check would bound a value 15 orders of magnitude off from
    /// what the wire actually carried.
    #[test]
    fn test_tick_price_ceiling_is_checked_before_widening_not_after() {
        let widened = f32_to_f64_clean(f32::MAX);
        assert!(
            widened < f64::from(f32::MAX) / 1e10,
            "premise: widening f32::MAX truncates to ~3.4e23, got {widened}"
        );
        assert!(
            !tick_price_is_sane(&tick_at(OPEN, f32::MAX, 0)),
            "the raw value is what gets bounded"
        );
    }

    #[test]
    fn test_aggregator_cell_empty_starts_uninitialised_on_every_timeframe() {
        let cell = AggregatorCell::empty();
        for tf in TfIndex::ALL {
            assert!(
                cell.snapshot(tf).is_uninitialised(),
                "{tf:?} must start uninitialised (sparsity sentinel)"
            );
        }
    }

    #[test]
    fn test_aggregator_cell_snapshot_reflects_the_open_bucket() {
        let mut cell = AggregatorCell::empty();
        let out = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 5, 100.5, 10),
            0,
            FeedStrategy::DEFAULT,
            10,
        );
        assert_eq!(out, ConsumeOutcome::Updated);
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.bucket_start_ist_secs, OPEN);
        assert_eq!(s.open, 100.5);
        assert_eq!(s.high, 100.5);
        assert_eq!(s.low, 100.5);
        assert_eq!(s.close, 100.5);
        assert_eq!(s.tick_count, 1);
        assert_eq!(s.volume, 10);
        // Untouched timeframes stay empty — sparsity.
        assert!(
            cell.snapshot(TfIndex::M15).is_uninitialised(),
            "an untouched timeframe must stay empty"
        );
    }

    #[test]
    fn test_aggregator_cell_consume_tick_folds_ohlcv_within_one_bucket() {
        let mut cell = AggregatorCell::empty();
        let prices = [100.0_f32, 103.0, 98.0, 101.0];
        for (i, p) in prices.iter().enumerate() {
            let ts = OPEN + u32::try_from(i).expect("small");
            let cum = u32::try_from((i + 1) * 10).expect("small");
            let out = cell.consume_tick(
                TfIndex::M1,
                &tick_at(ts, *p, cum),
                0,
                FeedStrategy::DEFAULT,
                u64::from(cum),
            );
            assert_eq!(out, ConsumeOutcome::Updated, "tick {i} must not seal");
        }
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.open, 100.0);
        assert_eq!(s.high, 103.0);
        assert_eq!(s.low, 98.0);
        assert_eq!(s.close, 101.0);
        assert_eq!(s.tick_count, 4);
        assert_eq!(s.volume, 40, "incremental volume = cumulative - baseline");
    }

    #[test]
    fn test_aggregator_cell_consume_tick_seals_exactly_once_at_the_boundary() {
        let mut cell = AggregatorCell::empty();
        // Fill the first minute.
        for i in 0..3_u32 {
            let _ = cell.consume_tick(
                TfIndex::M1,
                &tick_at(OPEN + i, 100.0 + i as f32, 10),
                0,
                FeedStrategy::DEFAULT,
                10,
            );
        }
        // Cross into the next minute.
        let out = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 60, 110.0, 20),
            10,
            FeedStrategy::DEFAULT,
            20,
        );
        let ConsumeOutcome::Sealed { sealed_state } = out else {
            panic!("boundary crossing must seal, got {out:?}");
        };
        assert_eq!(sealed_state.bucket_start_ist_secs, OPEN);
        assert_eq!(sealed_state.close, 102.0);
        assert_eq!(sealed_state.tick_count, 3);
        // The very next tick in the NEW bucket must NOT seal again.
        let out2 = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 61, 111.0, 21),
            10,
            FeedStrategy::DEFAULT,
            21,
        );
        assert_eq!(
            out2,
            ConsumeOutcome::Updated,
            "second seal of the same bucket"
        );
        assert_eq!(cell.snapshot(TfIndex::M1).bucket_start_ist_secs, OPEN + 60);
    }

    #[test]
    fn test_aggregator_cell_consume_tick_refold_amends_the_last_sealed_bucket() {
        let mut cell = AggregatorCell::empty();
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN, 100.0, 10),
            0,
            FeedStrategy::REFOLD,
            10,
        );
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 60, 105.0, 20),
            10,
            FeedStrategy::REFOLD,
            20,
        );
        // A tick whose LTT is inside the SEALED minute arrives late.
        let out = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 59, 130.0, 21),
            10,
            FeedStrategy::REFOLD,
            21,
        );
        let ConsumeOutcome::AmendedLate { amended_state } = out else {
            panic!("1-bucket-late tick under Refold must amend, got {out:?}");
        };
        assert_eq!(amended_state.bucket_start_ist_secs, OPEN);
        assert_eq!(amended_state.high, 130.0, "late high must win");
        assert_eq!(amended_state.close, 130.0, "later LTT sets close");
        assert_eq!(amended_state.open, 100.0, "open is never amended");
        assert_eq!(amended_state.volume, 10, "volume is never amended");
    }

    #[test]
    fn test_aggregator_cell_consume_tick_discard_policy_never_amends() {
        let mut cell = AggregatorCell::empty();
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN, 100.0, 10),
            0,
            FeedStrategy::DISCARD,
            10,
        );
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 60, 105.0, 20),
            10,
            FeedStrategy::DISCARD,
            20,
        );
        let out = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 59, 130.0, 21),
            10,
            FeedStrategy::DISCARD,
            21,
        );
        assert_eq!(out, ConsumeOutcome::DiscardLate);
        let last = cell
            .last_sealed_snapshot(TfIndex::M1)
            .expect("intraday seal is amendable-tracked");
        assert_eq!(last.high, 100.0, "Discard must leave the sealed bar alone");
    }

    #[test]
    fn test_aggregator_cell_consume_tick_discards_a_two_bucket_late_tick() {
        let mut cell = AggregatorCell::empty();
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN, 100.0, 10),
            0,
            FeedStrategy::REFOLD,
            10,
        );
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 60, 105.0, 20),
            10,
            FeedStrategy::REFOLD,
            20,
        );
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 120, 106.0, 30),
            20,
            FeedStrategy::REFOLD,
            30,
        );
        // Now OPEN's bucket is 2 buckets behind — not amendable.
        let out = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 1, 999.0, 31),
            20,
            FeedStrategy::REFOLD,
            31,
        );
        assert_eq!(out, ConsumeOutcome::DiscardLate);
    }

    #[test]
    fn test_aggregator_cell_last_sealed_snapshot_is_none_until_an_intraday_seal() {
        let mut cell = AggregatorCell::empty();
        assert_eq!(cell.last_sealed_snapshot(TfIndex::M1), None);
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN, 100.0, 10),
            0,
            FeedStrategy::DEFAULT,
            10,
        );
        assert_eq!(cell.last_sealed_snapshot(TfIndex::M1), None);
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 60, 105.0, 20),
            10,
            FeedStrategy::DEFAULT,
            20,
        );
        assert!(cell.last_sealed_snapshot(TfIndex::M1).is_some());
    }

    #[test]
    fn test_aggregator_cell_force_seal_emits_nothing_for_an_untouched_slot() {
        let mut cell = AggregatorCell::empty();
        for tf in TfIndex::ALL {
            assert_eq!(
                cell.force_seal(tf),
                None,
                "{tf:?} never opened — force_seal must emit NOTHING (sparsity)"
            );
        }
    }

    #[test]
    fn test_aggregator_cell_force_seal_drains_and_clears_the_amendable_bucket() {
        let mut cell = AggregatorCell::empty();
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN, 100.0, 10),
            0,
            FeedStrategy::DEFAULT,
            10,
        );
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 60, 105.0, 20),
            10,
            FeedStrategy::DEFAULT,
            20,
        );
        assert!(cell.last_sealed_snapshot(TfIndex::M1).is_some());
        let sealed = cell.force_seal(TfIndex::M1).expect("open bucket drained");
        assert_eq!(sealed.bucket_start_ist_secs, OPEN + 60);
        assert!(cell.snapshot(TfIndex::M1).is_uninitialised());
        assert_eq!(
            cell.last_sealed_snapshot(TfIndex::M1),
            None,
            "day-boundary seal must make yesterday un-amendable"
        );
        // Second force_seal is a no-op — never a duplicate emission.
        assert_eq!(cell.force_seal(TfIndex::M1), None);
    }

    #[test]
    fn test_aggregator_cell_catch_up_seal_respects_the_cutoff_and_blocks_reopen() {
        let mut cell = AggregatorCell::empty();
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 5, 100.0, 10),
            0,
            FeedStrategy::DEFAULT,
            10,
        );
        // Cutoff before the bucket end -> must NOT seal.
        assert_eq!(cell.catch_up_seal(TfIndex::M1, OPEN + 59), None);
        // Cutoff at/after the exclusive end -> seals.
        let sealed = cell
            .catch_up_seal(TfIndex::M1, OPEN + 60)
            .expect("bucket end reached");
        assert_eq!(sealed.bucket_start_ist_secs, OPEN);
        assert!(cell.snapshot(TfIndex::M1).is_uninitialised());
        // An uninitialised slot never catch-up-seals.
        assert_eq!(cell.catch_up_seal(TfIndex::M1, OPEN + 600), None);
        // A tick for the just-sealed bucket must NOT re-open it (that would
        // lose open/high/low and UPSERT a corrupted bar).
        let out = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 30, 101.0, 11),
            10,
            FeedStrategy::REFOLD,
            11,
        );
        assert!(
            matches!(out, ConsumeOutcome::AmendedLate { .. }),
            "must amend, never re-open; got {out:?}"
        );
        assert!(
            cell.snapshot(TfIndex::M1).is_uninitialised(),
            "the open slot must stay empty"
        );
    }

    #[test]
    fn test_open_bucket_uses_day_open_only_for_the_days_first_bar() {
        let mut cell = AggregatorCell::empty();
        let mut t = tick_at(OPEN, 100.0, 10);
        t.day_open = 99.0;
        let _ = cell.consume_tick(TfIndex::M1, &t, 0, FeedStrategy::DEFAULT, 10);
        assert_eq!(
            cell.snapshot(TfIndex::M1).open,
            99.0,
            "day's first bar opens at the exchange day-open"
        );
        let mut t2 = tick_at(OPEN + 60, 107.0, 20);
        t2.day_open = 99.0;
        let _ = cell.consume_tick(TfIndex::M1, &t2, 10, FeedStrategy::DEFAULT, 20);
        assert_eq!(
            cell.snapshot(TfIndex::M1).open,
            107.0,
            "an intraday crossing opens at the LTP, never day_open"
        );
    }

    #[test]
    fn test_fold_in_bucket_keeps_last_non_zero_prev_day_close() {
        let mut cell = AggregatorCell::empty();
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN, 100.0, 10),
            0,
            FeedStrategy::DEFAULT,
            10,
        );
        let mut t = tick_at(OPEN + 1, 101.0, 11);
        t.day_close = 95.0;
        let _ = cell.consume_tick(TfIndex::M1, &t, 0, FeedStrategy::DEFAULT, 11);
        assert_eq!(cell.snapshot(TfIndex::M1).prev_day_close, 95.0);
        // A later blank (0.0) must NOT clobber it.
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 2, 102.0, 12),
            0,
            FeedStrategy::DEFAULT,
            12,
        );
        assert_eq!(cell.snapshot(TfIndex::M1).prev_day_close, 95.0);
    }

    #[test]
    fn test_fold_late_hlc_never_lets_an_earlier_tick_clobber_the_close() {
        let mut cell = AggregatorCell::empty();
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 50, 100.0, 10),
            0,
            FeedStrategy::REFOLD,
            10,
        );
        let _ = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 60, 105.0, 20),
            10,
            FeedStrategy::REFOLD,
            20,
        );
        // Late tick with an EARLIER LTT than the sealed close.
        let out = cell.consume_tick(
            TfIndex::M1,
            &tick_at(OPEN + 40, 90.0, 21),
            10,
            FeedStrategy::REFOLD,
            21,
        );
        let ConsumeOutcome::AmendedLate { amended_state } = out else {
            panic!("expected amend, got {out:?}");
        };
        assert_eq!(amended_state.low, 90.0, "low still takes the extreme");
        assert_eq!(
            amended_state.close, 100.0,
            "an EARLIER late tick must not clobber the close"
        );
    }

    #[test]
    fn test_feed_strategy_default_is_refold_and_is_documented() {
        assert_eq!(FeedStrategy::default(), FeedStrategy::REFOLD);
        assert_eq!(FeedStrategy::DEFAULT.late_policy, LatePolicy::Refold);
        assert_eq!(FeedStrategy::DISCARD.late_policy, LatePolicy::Discard);
    }
}

// ---------------------------------------------------------------------------
// Item 6 (operator 2026-08-19) — 09:15 first-bucket open / high / low
// ---------------------------------------------------------------------------

#[cfg(test)]
mod first_bucket_ohlc_tests {
    use super::tests::{DAY, OPEN, tick_at};
    use super::*;

    /// Builds a tick carrying exchange-published session fields.
    fn tick_with_day(
        ts: u32,
        price: f32,
        day_open: f32,
        day_high: f32,
        day_low: f32,
    ) -> ParsedTick {
        ParsedTick {
            day_open,
            day_high,
            day_low,
            ..tick_at(ts, price, 0)
        }
    }

    fn fold(cell: &mut AggregatorCell, tick: &ParsedTick) -> ConsumeOutcome {
        let prices = TickPrices::from_tick(tick);
        cell.consume_tick_with_prices(TfIndex::M1, tick, prices, 0, FeedStrategy::DEFAULT, 0)
    }

    fn m1(cell: &AggregatorCell) -> LiveCandleState {
        cell.snapshot(TfIndex::M1)
    }

    /// The invariant this whole item exists for.
    fn assert_ohlc_valid(s: &LiveCandleState, label: &str) {
        assert!(
            s.low <= s.open,
            "{label}: open {} is BELOW low {} — invalid candle",
            s.open,
            s.low
        );
        assert!(
            s.open <= s.high,
            "{label}: open {} is ABOVE high {} — invalid candle",
            s.open,
            s.high
        );
        assert!(
            s.low <= s.close && s.close <= s.high,
            "{label}: close outside range"
        );
        assert!(s.low <= s.high, "{label}: low above high");
    }

    /// THE BUG. Gap-up: official open 100, first trade 105. Before this item
    /// the bar published `open=100, high=105, low=105` — open below its low.
    #[test]
    fn first_bucket_open_below_ltp_still_yields_valid_ohlc() {
        let mut cell = AggregatorCell::empty();
        fold(&mut cell, &tick_with_day(OPEN, 105.0, 100.0, 0.0, 0.0));
        let s = m1(&cell);
        assert_eq!(
            s.open, 100.0,
            "official open must be preserved, not clamped away"
        );
        assert_ohlc_valid(&s, "gap-up");
        assert_eq!(
            s.low, 100.0,
            "range must widen DOWN to contain the official open"
        );
        assert_eq!(s.high, 105.0);
    }

    /// Gap-down mirror: official open 105, first trade 100.
    #[test]
    fn first_bucket_open_above_ltp_still_yields_valid_ohlc() {
        let mut cell = AggregatorCell::empty();
        fold(&mut cell, &tick_with_day(OPEN, 100.0, 105.0, 0.0, 0.0));
        let s = m1(&cell);
        assert_eq!(s.open, 105.0);
        assert_ohlc_valid(&s, "gap-down");
        assert_eq!(
            s.high, 105.0,
            "range must widen UP to contain the official open"
        );
        assert_eq!(s.low, 100.0);
    }

    /// The second, more dangerous stamp site: `day_open` arrives AFTER the
    /// bucket has already folded ticks, so the range can be far from it.
    #[test]
    fn late_day_open_into_folded_bucket_keeps_ohlc_valid() {
        let mut cell = AggregatorCell::empty();
        // Bucket opens with NO day_open, then folds a long way from it.
        fold(&mut cell, &tick_with_day(OPEN, 200.0, 0.0, 0.0, 0.0));
        fold(&mut cell, &tick_with_day(OPEN + 5, 210.0, 0.0, 0.0, 0.0));
        // Now the official open finally arrives, far below the folded range.
        fold(&mut cell, &tick_with_day(OPEN + 10, 208.0, 150.0, 0.0, 0.0));
        let s = m1(&cell);
        assert_eq!(s.open, 150.0, "late official open must still be adopted");
        assert_ohlc_valid(&s, "late day_open");
        assert_eq!(s.low, 150.0, "range must widen to contain the late open");
    }

    /// Operator idea #2: the first bucket adopts the exchange's true extremes,
    /// which include trades our sampled feed never saw.
    #[test]
    fn first_bucket_adopts_exchange_day_high_and_day_low() {
        let mut cell = AggregatorCell::empty();
        // We only ever OBSERVE 100..101, but the exchange saw 98..107.
        fold(&mut cell, &tick_with_day(OPEN, 100.0, 100.0, 107.0, 98.0));
        fold(
            &mut cell,
            &tick_with_day(OPEN + 30, 101.0, 100.0, 107.0, 98.0),
        );
        let s = m1(&cell);
        assert_eq!(s.high, 107.0, "exchange day_high must be adopted");
        assert_eq!(s.low, 98.0, "exchange day_low must be adopted");
        assert_eq!(s.close, 101.0, "close still tracks the LTP");
        assert_ohlc_valid(&s, "day extremes");
    }

    /// Ticker mode carries NO day fields — `0.0` is the documented ABSENT
    /// sentinel. Adopting it would drag a low to zero.
    #[test]
    fn zero_day_high_low_sentinel_is_never_adopted() {
        let mut cell = AggregatorCell::empty();
        fold(&mut cell, &tick_with_day(OPEN, 100.0, 0.0, 0.0, 0.0));
        let s = m1(&cell);
        assert_eq!(s.low, 100.0, "sentinel 0.0 must NEVER become a price");
        assert_eq!(s.high, 100.0);
        assert_ohlc_valid(&s, "ticker-mode sentinel");
    }

    /// NaN AND +infinity. `> 0.0` alone rejects NaN but ACCEPTS +inf, which
    /// would pin `high` at infinity permanently.
    #[test]
    fn non_finite_day_high_low_is_never_adopted() {
        for (dh, dl, label) in [
            (f32::NAN, f32::NAN, "NaN"),
            (f32::INFINITY, f32::NEG_INFINITY, "infinity"),
            (f32::INFINITY, 0.0, "+inf high only"),
        ] {
            let mut cell = AggregatorCell::empty();
            fold(&mut cell, &tick_with_day(OPEN, 100.0, 0.0, dh, dl));
            let s = m1(&cell);
            assert!(s.high.is_finite(), "{label}: high went non-finite");
            assert!(s.low.is_finite(), "{label}: low went non-finite");
            assert_eq!(s.high, 100.0, "{label}: high must stay at the observed LTP");
            assert_eq!(s.low, 100.0, "{label}: low must stay at the observed LTP");
            assert_ohlc_valid(&s, label);
        }
    }

    /// THE SCOPE GUARANTEE. `day_high`/`day_low` are RUNNING SESSION extremes;
    /// applying them to a later bucket would smear the whole day's range
    /// across every bar. The operator's scope is the 09:15 bucket alone.
    #[test]
    fn second_bucket_ignores_running_day_extremes() {
        let mut cell = AggregatorCell::empty();
        fold(&mut cell, &tick_with_day(OPEN, 100.0, 100.0, 100.0, 100.0));
        // Roll into 09:16. The session has since ranged 90..150, but this
        // bucket only traded at 120.
        let out = fold(
            &mut cell,
            &tick_with_day(OPEN + 60, 120.0, 100.0, 150.0, 90.0),
        );
        assert!(
            matches!(out, ConsumeOutcome::Sealed { .. }),
            "should have sealed 09:15"
        );
        let s = m1(&cell);
        assert_eq!(
            s.high, 120.0,
            "second bucket must NOT adopt the session high"
        );
        assert_eq!(s.low, 120.0, "second bucket must NOT adopt the session low");
        assert_ohlc_valid(&s, "second bucket");
    }

    /// Monotone widening: an exchange value BELOW our observed high can never
    /// shrink the range, and one ABOVE our observed low can never raise it.
    #[test]
    fn widening_never_narrows_an_observed_range() {
        let mut cell = AggregatorCell::empty();
        fold(&mut cell, &tick_with_day(OPEN, 100.0, 0.0, 0.0, 0.0));
        fold(&mut cell, &tick_with_day(OPEN + 10, 130.0, 0.0, 0.0, 0.0));
        fold(&mut cell, &tick_with_day(OPEN + 20, 90.0, 0.0, 0.0, 0.0));
        // Exchange reports a NARROWER range than we observed (stale field).
        fold(
            &mut cell,
            &tick_with_day(OPEN + 30, 110.0, 0.0, 120.0, 95.0),
        );
        let s = m1(&cell);
        assert_eq!(s.high, 130.0, "observed high must survive a lower day_high");
        assert_eq!(s.low, 90.0, "observed low must survive a higher day_low");
        assert_ohlc_valid(&s, "monotone widening");
    }

    /// The invariant must hold for EVERY bar of a folded session, not just the
    /// first — including the sealed ones.
    #[test]
    fn ohlc_invariant_holds_across_a_folded_session() {
        let mut cell = AggregatorCell::empty();
        let mut sealed = Vec::new();
        // 30 minutes, prices sweeping up and down, day extremes always present.
        for i in 0..1_800_u32 {
            let price = 100.0 + ((i % 97) as f32) * 0.35 - ((i % 53) as f32) * 0.4;
            let t = tick_with_day(OPEN + i, price, 100.0, 140.0, 70.0);
            if let ConsumeOutcome::Sealed { sealed_state } = fold(&mut cell, &t) {
                sealed.push(sealed_state);
            }
        }
        assert!(
            sealed.len() >= 25,
            "expected ~29 sealed bars, got {}",
            sealed.len()
        );
        for (n, s) in sealed.iter().enumerate() {
            assert_ohlc_valid(s, &format!("sealed bar {n}"));
        }
        assert_ohlc_valid(&m1(&cell), "still-open bar");
        // Only the FIRST sealed bar may carry the session extremes.
        assert_eq!(sealed[0].high, 140.0, "first bar adopts the session high");
        assert_eq!(sealed[0].low, 70.0, "first bar adopts the session low");
        for (n, s) in sealed.iter().enumerate().skip(1) {
            assert!(
                s.high < 140.0,
                "sealed bar {n} wrongly adopted the session high"
            );
            assert!(
                s.low > 70.0,
                "sealed bar {n} wrongly adopted the session low"
            );
        }
    }

    /// The derived first-bucket signal must reset across the day boundary,
    /// otherwise day 2's 09:15 bar silently loses the treatment.
    #[test]
    fn day_boundary_rearms_the_first_bucket_treatment() {
        let mut cell = AggregatorCell::empty();
        fold(&mut cell, &tick_with_day(OPEN, 100.0, 100.0, 110.0, 95.0));
        fold(
            &mut cell,
            &tick_with_day(OPEN + 60, 101.0, 100.0, 110.0, 95.0),
        );
        assert!(
            cell.force_seal(TfIndex::M1).is_some(),
            "day boundary seals the open bar"
        );

        // Day 2.
        let open2 = OPEN + 86_400;
        fold(&mut cell, &tick_with_day(open2, 200.0, 205.0, 215.0, 195.0));
        let s = m1(&cell);
        assert_eq!(s.open, 205.0, "day 2 must re-arm the official open");
        assert_eq!(
            s.high, 215.0,
            "day 2 first bucket must adopt day_high again"
        );
        assert_eq!(s.low, 195.0, "day 2 first bucket must adopt day_low again");
        assert_ohlc_valid(&s, "day 2 first bucket");
        let _ = DAY;
    }
}

// ---------------------------------------------------------------------------
// Session-extreme DELTA adoption (2026-08-25)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod session_extreme_delta_tests {
    use super::tests::{OPEN, tick_at};
    use super::*;

    /// Drives the cell exactly as the real fan-out does: observe the packet's
    /// session extremes ONCE, then fold it into one timeframe.
    fn feed(cell: &mut AggregatorCell, tf: TfIndex, tick: &ParsedTick, cum: u32) {
        let delta = cell.observe_session_extremes(tick);
        cell.consume_tick_with_extremes(
            tf,
            tick,
            TickPrices::from_tick(tick),
            0,
            FeedStrategy::DEFAULT,
            u64::from(cum),
            delta,
        );
    }

    /// Puts the cell past the day's FIRST bucket, so the level-based
    /// `adopt_exchange_day_extremes` path is no longer live and the delta path
    /// is the one under test. Returns the timestamp of the first tick of the
    /// second bucket.
    fn advance_past_first_bucket(cell: &mut AggregatorCell, day_high: f32, day_low: f32) -> u32 {
        let mut t = tick_at(OPEN, 100.0, 10);
        t.day_high = day_high;
        t.day_low = day_low;
        feed(cell, TfIndex::M1, &t, 10);

        let mut roll = tick_at(OPEN + 60, 100.0, 20);
        roll.day_high = day_high;
        roll.day_low = day_low;
        feed(cell, TfIndex::M1, &roll, 20);
        assert!(
            cell.last_sealed_snapshot(TfIndex::M1).is_some(),
            "fixture must be past the day's first bucket"
        );
        OPEN + 60
    }

    #[test]
    fn a_session_high_set_between_two_ticks_of_one_bucket_is_recovered() {
        // The whole point. Both observations sit inside the 09:16 bucket, so
        // the print that lifted the session high to 104.5 necessarily landed
        // inside it — even though our sampled LTPs never saw a price above
        // 101.0. This is the trade Dhan's conflation dropped on the floor.
        let mut cell = AggregatorCell::empty();
        let second = advance_past_first_bucket(&mut cell, 100.0, 100.0);

        let mut t = tick_at(second + 20, 101.0, 30);
        t.day_high = 104.5;
        t.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &t, 30);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(
            s.high,
            f32_to_f64_clean(104.5),
            "the exchange's own session high must widen the bucket it printed in"
        );
        assert_eq!(s.close, f32_to_f64_clean(101.0), "close stays the real LTP");
        assert!(s.low <= s.high && s.close <= s.high, "range stays valid");
    }

    #[test]
    fn a_session_low_set_between_two_ticks_of_one_bucket_is_recovered() {
        let mut cell = AggregatorCell::empty();
        let second = advance_past_first_bucket(&mut cell, 100.0, 100.0);

        let mut t = tick_at(second + 20, 99.5, 30);
        t.day_high = 100.0;
        t.day_low = 95.25;
        feed(&mut cell, TfIndex::M1, &t, 30);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.low, f32_to_f64_clean(95.25), "session low must widen it");
        assert!(s.low <= s.high, "range stays valid");
    }

    #[test]
    fn a_delta_whose_interval_straddles_a_bucket_boundary_is_dropped_not_guessed() {
        // THE SAFETY PROPERTY. The previous packet was in the 09:15 bucket and
        // this one opens 09:16, so the exchange could have printed the new high
        // on either side of the boundary. We do not know which bucket owns it,
        // so neither bucket gets it. Attributing it would be a fabricated bar.
        let mut cell = AggregatorCell::empty();

        let mut first = tick_at(OPEN, 100.0, 10);
        first.day_high = 100.0;
        first.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &first, 10);

        // Roll into the second bucket, carrying a RISEN session high.
        let mut roll = tick_at(OPEN + 60, 100.5, 20);
        roll.day_high = 108.0;
        roll.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &roll, 20);

        let opened = cell.snapshot(TfIndex::M1);
        assert_eq!(
            opened.high,
            f32_to_f64_clean(100.5),
            "the straddling delta must NOT widen the freshly opened bucket"
        );
        let sealed = cell
            .last_sealed_snapshot(TfIndex::M1)
            .expect("first bucket sealed");
        assert_eq!(
            sealed.high,
            f32_to_f64_clean(100.0),
            "nor may it retro-widen the bucket that just sealed"
        );
    }

    #[test]
    fn a_late_packet_between_two_in_bucket_packets_breaks_attribution_and_is_refused() {
        // THE TEST THAT PINS THE ATTRIBUTION CHECK. Everything else here is
        // satisfied structurally by the roll path never adopting; this is the
        // one shape that reaches the IN-BUCKET path with a predecessor that
        // was somewhere else, and it is reachable because the feed genuinely
        // delivers out of order.
        //
        // A: 09:16:10 opens the 09:16 bucket.
        // B: 09:15:55 arrives late and is discarded — but it is still the
        //    previous packet OBSERVED, and it sits in the 09:15 bucket.
        // C: 09:16:20 folds in-bucket carrying a risen session high.
        //
        // The interval B→C spans 09:15:55 → 09:16:20, so the print could have
        // landed in either minute. Attributing it to 09:16 would invent a high
        // for a minute that may never have traded there.
        let mut cell = AggregatorCell::empty();
        let first_bucket = advance_past_first_bucket(&mut cell, 100.0, 100.0);
        let second_bucket = first_bucket + 60;

        let mut a = tick_at(second_bucket + 10, 100.0, 30);
        a.day_high = 100.0;
        a.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &a, 30);

        let mut b = tick_at(second_bucket - 5, 100.0, 28);
        b.day_high = 100.0;
        b.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &b, 28);

        let mut c = tick_at(second_bucket + 20, 100.0, 35);
        c.day_high = 140.0;
        c.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &c, 35);

        assert_eq!(
            cell.snapshot(TfIndex::M1).high,
            f32_to_f64_clean(100.0),
            "a delta measured from a late packet in an earlier bucket must be refused"
        );
    }

    #[test]
    fn the_first_tick_of_a_bucket_never_carries_a_delta_even_mid_session() {
        // Same rule stated from the other side: `tick_count == 0` means the
        // previous packet was somewhere else, so nothing is attributable yet.
        let mut cell = AggregatorCell::empty();
        let second = advance_past_first_bucket(&mut cell, 100.0, 100.0);

        // Force a third bucket whose FIRST tick carries a risen high.
        let mut third = tick_at(second + 60, 100.0, 30);
        third.day_high = 130.0;
        third.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &third, 30);

        assert_eq!(
            cell.snapshot(TfIndex::M1).high,
            f32_to_f64_clean(100.0),
            "a bucket's first tick has no in-bucket predecessor to bound the interval"
        );
    }

    #[test]
    fn the_very_first_observation_of_the_day_establishes_a_baseline_and_widens_nothing() {
        // With no previous packet there is no interval, so there is no delta —
        // only a baseline. The day's first bucket still gets the LEVEL through
        // the pre-existing `adopt_exchange_day_extremes` path, which is correct
        // there and is deliberately left untouched.
        let mut cell = AggregatorCell::empty();
        let mut t = tick_at(OPEN, 100.0, 10);
        t.day_high = 250.0;
        t.day_low = 90.0;
        let d = cell.observe_session_extremes(&t);
        assert!(d.is_empty(), "no previous packet means no delta");
    }

    #[test]
    fn a_stale_packet_cannot_lower_the_baseline_and_manufacture_a_false_rise() {
        // THE OUT-OF-ORDER ATTACK, and the reason the baseline is a high-water
        // mark. The feed delivers packets out of order — `LatePolicy::Refold`
        // exists for exactly that — and `observe_session_extremes` runs before
        // the fold knows a packet is late.
        //
        // If a fall re-baselined, this sequence would smear: a stale packet
        // carrying an OLD, lower session high drops the mark, and the next
        // fresh packet restores the level we already knew about as an apparent
        // RISE — attributing a print from minutes ago to whichever bucket
        // happens to be open now. Holding the mark makes that impossible.
        let mut cell = AggregatorCell::empty();
        let second = advance_past_first_bucket(&mut cell, 100.0, 100.0);

        // Fresh packet: the session high genuinely rises to 130.
        let mut fresh = tick_at(second + 10, 100.0, 25);
        fresh.day_high = 130.0;
        fresh.day_low = 100.0;
        let d = cell.observe_session_extremes(&fresh);
        assert_eq!(d.new_high, Some(f32_to_f64_clean(130.0)), "a real rise");

        // A LATE packet from before that print, carrying the older high.
        let mut stale = tick_at(second + 3, 100.0, 22);
        stale.day_high = 100.0;
        stale.day_low = 100.0;
        let d = cell.observe_session_extremes(&stale);
        assert!(d.new_high.is_none(), "a fall is never a delta");

        // The next fresh packet re-states 130. It must NOT read as a rise.
        let mut echo = tick_at(second + 20, 100.0, 30);
        echo.day_high = 130.0;
        echo.day_low = 100.0;
        let d = cell.observe_session_extremes(&echo);
        assert!(
            d.new_high.is_none(),
            "restoring a level we already recorded is not a new print"
        );

        // A genuine rise ABOVE the mark still fires, so holding the mark costs
        // nothing real.
        let mut higher = tick_at(second + 30, 100.0, 35);
        higher.day_high = 131.0;
        higher.day_low = 100.0;
        let d = cell.observe_session_extremes(&higher);
        assert_eq!(
            d.new_high,
            Some(f32_to_f64_clean(131.0)),
            "a true new session high must still be recovered"
        );
    }

    #[test]
    fn absent_and_malformed_session_extremes_are_ignored_entirely() {
        // Ticker-mode packets carry no session extremes at all (the field
        // defaults to 0.0), and a malformed packet can decode to NaN. Neither
        // may move the baseline, or the next real value would read as a delta
        // against garbage.
        let mut cell = AggregatorCell::empty();
        let second = advance_past_first_bucket(&mut cell, 100.0, 100.0);

        for bad in [0.0_f32, f32::NAN, -1.0_f32] {
            let mut t = tick_at(second + 5, 100.0, 25);
            t.day_high = bad;
            t.day_low = bad;
            let d = cell.observe_session_extremes(&t);
            assert!(
                d.is_empty(),
                "an unusable session extreme must produce no delta"
            );
        }

        // ...and the baseline survived, so a real rise still fires.
        let mut good = tick_at(second + 10, 100.0, 30);
        good.day_high = 101.0;
        good.day_low = 100.0;
        let d = cell.observe_session_extremes(&good);
        assert_eq!(
            d.new_high,
            Some(f32_to_f64_clean(101.0)),
            "the pre-existing baseline must be intact"
        );
    }

    #[test]
    fn a_delta_already_covered_by_the_traded_price_is_a_no_op() {
        let mut cell = AggregatorCell::empty();
        let second = advance_past_first_bucket(&mut cell, 100.0, 100.0);

        let mut high_trade = tick_at(second + 5, 120.0, 25);
        high_trade.day_high = 120.0;
        high_trade.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &high_trade, 25);

        let mut echo = tick_at(second + 10, 110.0, 30);
        echo.day_high = 120.0;
        echo.day_low = 100.0;
        feed(&mut cell, TfIndex::M1, &echo, 30);

        assert_eq!(
            cell.snapshot(TfIndex::M1).high,
            f32_to_f64_clean(120.0),
            "no double counting; the LTP already reached the session high"
        );
    }

    #[test]
    fn the_day_boundary_drops_the_baseline_so_yesterday_cannot_suppress_today() {
        // Without this, an instrument that closed at 500 yesterday and trades
        // at 90 today would need to exceed 500 before ANY rise registered.
        let mut cell = AggregatorCell::empty();
        let mut yesterday = tick_at(OPEN, 500.0, 10);
        yesterday.day_high = 500.0;
        yesterday.day_low = 400.0;
        feed(&mut cell, TfIndex::M1, &yesterday, 10);

        for tf in TfIndex::ALL {
            let _ = cell.force_seal(tf);
        }

        let mut today = tick_at(OPEN + 86_400, 90.0, 5);
        today.day_high = 90.0;
        today.day_low = 90.0;
        let d = cell.observe_session_extremes(&today);
        assert!(
            d.is_empty(),
            "the first packet of a new day is a baseline, not a fall"
        );

        let mut later = tick_at(OPEN + 86_400 + 10, 91.0, 8);
        later.day_high = 91.0;
        later.day_low = 90.0;
        let d = cell.observe_session_extremes(&later);
        assert_eq!(
            d.new_high,
            Some(f32_to_f64_clean(91.0)),
            "today's rises must register against today's baseline"
        );
    }

    #[test]
    fn a_caller_that_never_observes_extremes_gets_exactly_the_old_behaviour() {
        // The contract is fail-CLOSED: `consume_tick` on its own must never
        // widen from a stale delta. Every pre-existing caller and test depends
        // on this.
        let mut cell = AggregatorCell::empty();
        let strategy = FeedStrategy::DEFAULT;

        let mut first = tick_at(OPEN, 100.0, 10);
        first.day_high = 100.0;
        first.day_low = 100.0;
        cell.consume_tick(TfIndex::M1, &first, 0, strategy, 10);
        let mut roll = tick_at(OPEN + 60, 100.0, 20);
        roll.day_high = 100.0;
        roll.day_low = 100.0;
        cell.consume_tick(TfIndex::M1, &roll, 0, strategy, 20);

        let mut spike = tick_at(OPEN + 80, 101.0, 30);
        spike.day_high = 900.0;
        spike.day_low = 1.0;
        cell.consume_tick(TfIndex::M1, &spike, 0, strategy, 30);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.high, f32_to_f64_clean(101.0), "no observe, no widening");
        assert_eq!(s.low, f32_to_f64_clean(100.0), "no observe, no widening");
    }

    #[test]
    fn every_timeframe_sees_the_same_delta_from_one_observation() {
        // `observe_session_extremes` runs ONCE per packet and all 24 folds read
        // it. If it were ever moved inside the fan-out loop it would compare a
        // packet against itself for 23 of them and the delta would vanish.
        let mut cell = AggregatorCell::empty();
        let strategy = FeedStrategy::DEFAULT;

        for (ts, cum) in [(OPEN, 10_u32), (OPEN + 60, 20)] {
            let mut t = tick_at(ts, 100.0, cum);
            t.day_high = 100.0;
            t.day_low = 100.0;
            let delta = cell.observe_session_extremes(&t);
            let prices = TickPrices::from_tick(&t);
            for tf in TfIndex::ALL {
                cell.consume_tick_with_extremes(tf, &t, prices, 0, strategy, u64::from(cum), delta);
            }
        }

        let mut spike = tick_at(OPEN + 80, 100.0, 30);
        spike.day_high = 107.0;
        spike.day_low = 100.0;
        let delta = cell.observe_session_extremes(&spike);
        let prices = TickPrices::from_tick(&spike);
        for tf in TfIndex::ALL {
            cell.consume_tick_with_extremes(tf, &spike, prices, 0, strategy, 30, delta);
        }

        // M1 and M3 both have a prior in-bucket tick, so both recover it.
        for tf in [TfIndex::M1, TfIndex::M3] {
            assert_eq!(
                cell.snapshot(tf).high,
                f32_to_f64_clean(107.0),
                "{tf:?} must recover the session high from the single observation"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Permutation-sweep regressions (2026-08-25)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod permutation_regression_tests {
    use super::tests::{DAY, OPEN, tick_at};
    use super::*;

    fn strategy() -> FeedStrategy {
        FeedStrategy::DEFAULT
    }

    #[test]
    fn a_mid_session_first_bucket_must_not_adopt_the_whole_running_session_range() {
        // Row #2 of the sweep. "The day's first bucket" was derived as
        // "nothing has sealed yet", which is ALSO true for an instrument first
        // seen at 11:00 — a contract attached after the open, or a process
        // restart. That bar adopted the entire session's high and low, so a
        // single tick at 100 published `high 180 / low 60`: a bar whose range
        // is 120 points wide from one observed price.
        let mut cell = AggregatorCell::empty();
        let mid_session = OPEN + 105 * 60; // 11:00 IST

        let mut t = tick_at(mid_session, 100.0, 10);
        t.day_high = 180.0;
        t.day_low = 60.0;
        t.day_open = 95.0;
        cell.consume_tick(TfIndex::M1, &t, 0, strategy(), 10);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(
            s.high,
            f32_to_f64_clean(100.0),
            "high must be the observed price, not the session high"
        );
        assert_eq!(
            s.low,
            f32_to_f64_clean(100.0),
            "low must be the observed price, not the session low"
        );
        assert_eq!(
            s.open,
            f32_to_f64_clean(100.0),
            "a mid-session bar must not open at the 09:15 price"
        );
    }

    #[test]
    fn the_days_real_first_bucket_still_adopts_everything_it_should() {
        // The other half — the fix must NARROW the predicate, not disable it.
        let mut cell = AggregatorCell::empty();
        let mut t = tick_at(OPEN, 100.0, 10);
        t.day_high = 104.0;
        t.day_low = 96.0;
        t.day_open = 98.0;
        cell.consume_tick(TfIndex::M1, &t, 0, strategy(), 10);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(
            s.open,
            f32_to_f64_clean(98.0),
            "the day's first bar opens at the official open"
        );
        assert_eq!(
            s.high,
            f32_to_f64_clean(104.0),
            "and adopts the session high"
        );
        assert_eq!(s.low, f32_to_f64_clean(96.0), "and the session low");
    }

    #[test]
    fn a_bucket_opened_after_an_intraday_catch_up_seal_must_not_use_the_session_open() {
        // Row #1, the sweep's worst finding. `catch_up_seal` clears the slot
        // but deliberately does not re-arm day-open — and only the ROLL path
        // disarmed. So a thin instrument whose first tick lacked `day_open`
        // could open an 11:00 bucket at the 09:15 official price and widen
        // that bar's `low` down to it: a fabricated open AND low on a
        // mid-session candle, which then reads as a legitimate gap-open.
        let mut cell = AggregatorCell::empty();

        // Day's first bucket, no session open published yet.
        let first = tick_at(OPEN, 100.0, 10);
        assert_eq!(first.day_open, 0.0, "fixture models the unpopulated case");
        cell.consume_tick(TfIndex::M1, &first, 0, strategy(), 10);
        assert!(
            cell.catch_up_seal(TfIndex::M1, OPEN + 600).is_some(),
            "fixture must drain the slot"
        );

        // Much later, the official open finally arrives on a packet.
        let mid_session = OPEN + 105 * 60;
        let mut later = tick_at(mid_session, 120.0, 40);
        later.day_open = 95.0;
        cell.consume_tick(TfIndex::M1, &later, 0, strategy(), 40);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(
            s.open,
            f32_to_f64_clean(120.0),
            "an 11:00 bar must open at its own first traded price"
        );
        assert_eq!(
            s.low,
            f32_to_f64_clean(120.0),
            "and must not be widened down to the 09:15 price"
        );
    }

    #[test]
    fn an_absurd_but_finite_session_extreme_is_refused_everywhere_the_ltp_would_be() {
        // Rows #4-#6. `tick_price_is_sane` has bounded the LTP by
        // MAX_PLAUSIBLE_LTP since 2026-08-15; the day fields come off the wire
        // the same way and were never bounded. One mangled frame reached a
        // bar's `high`, `open`, `session_open` and `prev_day_close`.
        for absurd in [f32::MAX, 1e30, MAX_PLAUSIBLE_LTP * 1.5] {
            let mut cell = AggregatorCell::empty();
            let mut t = tick_at(OPEN, 100.0, 10);
            t.day_high = absurd;
            t.day_open = absurd;
            t.day_close = absurd;
            cell.consume_tick(TfIndex::M1, &t, 0, strategy(), 10);

            let s = cell.snapshot(TfIndex::M1);
            assert_eq!(
                s.high,
                f32_to_f64_clean(100.0),
                "{absurd} must never become a bar high"
            );
            assert_eq!(
                s.open,
                f32_to_f64_clean(100.0),
                "{absurd} must never become a bar open"
            );
            assert_eq!(
                s.session_open, 0.0,
                "{absurd} must never become the session open"
            );
            assert_eq!(
                s.prev_day_close, 0.0,
                "{absurd} must never become the previous close"
            );
        }
    }

    #[test]
    fn a_subnormal_session_low_cannot_zero_a_bars_low() {
        // Row #11. A positive subnormal passes every finiteness and sign test,
        // but `f32_to_f64_clean` round-trips through a fixed decimal buffer
        // that a subnormal overruns, so it parses back as 0.0 -- which then
        // wins the `<` comparison and sets the bar's low to zero.
        let mut cell = AggregatorCell::empty();
        let mut t = tick_at(OPEN, 100.0, 10);
        t.day_low = f32::MIN_POSITIVE / 2.0;
        assert!(
            t.day_low > 0.0 && t.day_low.is_finite(),
            "fixture is positive and finite"
        );
        cell.consume_tick(TfIndex::M1, &t, 0, strategy(), 10);

        assert_eq!(
            cell.snapshot(TfIndex::M1).low,
            f32_to_f64_clean(100.0),
            "a subnormal must never reach a bar's low"
        );
    }

    #[test]
    fn a_packet_carrying_no_session_extremes_must_not_narrow_the_delta_interval() {
        // Rows #7-#8, and the one defect in the delta mechanism itself. A
        // Ticker packet -- or a Quote whose day fields decoded 0.0/NaN --
        // reports nothing about the session extremes, yet it was advancing the
        // interval's left endpoint. The next rise was then attributed on
        // evidence that did not exist.
        let mut cell = AggregatorCell::empty();

        // 09:15 bucket: a real Quote establishes the mark.
        let mut a = tick_at(OPEN, 100.0, 10);
        a.day_high = 100.0;
        a.day_low = 100.0;
        let d = cell.observe_session_extremes(&a);
        cell.consume_tick_with_extremes(
            TfIndex::M1,
            &a,
            TickPrices::from_tick(&a),
            0,
            strategy(),
            10,
            d,
        );

        // 09:16 bucket: a Ticker packet -- no day fields at all.
        let silent = tick_at(OPEN + 70, 100.0, 20);
        assert_eq!(silent.day_high, 0.0, "fixture models a Ticker packet");
        let d = cell.observe_session_extremes(&silent);
        cell.consume_tick_with_extremes(
            TfIndex::M1,
            &silent,
            TickPrices::from_tick(&silent),
            0,
            strategy(),
            20,
            d,
        );

        // 09:16 again: a Quote with a risen high. The rise could have printed
        // any time since the LAST PACKET THAT REPORTED AN EXTREME -- which was
        // in the 09:15 bucket -- so it straddles and must be refused.
        let mut c = tick_at(OPEN + 80, 100.0, 30);
        c.day_high = 105.0;
        c.day_low = 100.0;
        let d = cell.observe_session_extremes(&c);
        cell.consume_tick_with_extremes(
            TfIndex::M1,
            &c,
            TickPrices::from_tick(&c),
            0,
            strategy(),
            30,
            d,
        );

        assert_eq!(
            cell.snapshot(TfIndex::M1).high,
            f32_to_f64_clean(100.0),
            "a silent packet must not license attribution it cannot support"
        );
        let _ = DAY;
    }
}

// ---------------------------------------------------------------------------
// Open-bucket ordering (2026-08-25)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod open_bucket_ordering_tests {
    use super::tests::{OPEN, tick_at};
    use super::*;

    /// Feeds a tick straight into one timeframe, as a caller with no session
    /// extremes to report would.
    fn fold(cell: &mut AggregatorCell, tf: TfIndex, tick: &ParsedTick, cum: u64) {
        cell.consume_tick(tf, tick, 0, FeedStrategy::DEFAULT, cum);
    }

    #[test]
    fn an_out_of_order_packet_inside_an_open_bucket_cannot_rewrite_the_close() {
        // `fold_late_hlc` — the SEALED-bucket path — has always refused to let
        // an earlier packet clobber a later close. The OPEN-bucket path did
        // not, so a reordered packet arriving before its bucket sealed
        // overwrote `close` with a stale price and moved `close_ts` backwards.
        let mut cell = AggregatorCell::empty();

        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 10, 100.0, 10), 10);
        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 40, 110.0, 30), 30);
        // Reordered: an EARLIER tick arrives after the later one.
        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 20, 101.0, 15), 15);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(
            s.close,
            f32_to_f64_clean(110.0),
            "the close must be the LATEST traded price, not the last packet to arrive"
        );
        assert_eq!(
            s.close_ts_ist_secs,
            OPEN + 40,
            "and its timestamp must never move backwards"
        );
        assert_eq!(
            s.high,
            f32_to_f64_clean(110.0),
            "the stale packet's price still counts toward the range"
        );
    }

    #[test]
    fn the_daily_bars_close_survives_reordering_across_the_whole_session() {
        // The same defect, at the scale where it hurt most. A 1-minute bucket
        // gives a reordered packet a 60-second window to do damage; the daily
        // bucket gives it the entire session, so ANY reordered packet could
        // rewrite the day's close.
        let mut cell = AggregatorCell::empty();

        fold(&mut cell, TfIndex::D1, &tick_at(OPEN, 100.0, 10), 10);
        fold(
            &mut cell,
            TfIndex::D1,
            &tick_at(OPEN + 5 * 3600, 250.0, 900),
            900,
        );
        // Fifty minutes stale, arriving last.
        fold(
            &mut cell,
            TfIndex::D1,
            &tick_at(OPEN + 4 * 3600, 180.0, 700),
            700,
        );

        let s = cell.snapshot(TfIndex::D1);
        assert_eq!(
            s.close,
            f32_to_f64_clean(250.0),
            "the day's close must be the last TRADE, not the last delivery"
        );
    }

    #[test]
    fn a_stale_packet_cannot_shrink_a_bars_volume() {
        // Exchange cumulative volume only rises, so a bar's traded volume is
        // monotone. `saturating_sub` bounded the arithmetic but still let a
        // stale packet overwrite the bar with a SMALLER figure than one we had
        // already observed.
        let mut cell = AggregatorCell::empty();

        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 10, 100.0, 10), 10);
        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 40, 110.0, 90), 90);
        assert_eq!(cell.snapshot(TfIndex::M1).volume, 90, "fixture baseline");

        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 20, 101.0, 40), 40);
        assert_eq!(
            cell.snapshot(TfIndex::M1).volume,
            90,
            "a stale cumulative must never reduce volume already observed"
        );
    }

    #[test]
    fn an_in_order_packet_still_advances_everything_it_should() {
        // The positive control: the guard must NARROW behaviour, not freeze it.
        let mut cell = AggregatorCell::empty();

        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 10, 100.0, 10), 10);
        let mut later = tick_at(OPEN + 40, 110.0, 90);
        later.open_interest = 4_242;
        fold(&mut cell, TfIndex::M1, &later, 90);

        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.close, f32_to_f64_clean(110.0), "close advances");
        assert_eq!(s.close_ts_ist_secs, OPEN + 40, "close timestamp advances");
        assert_eq!(s.volume, 90, "volume advances");
        assert_eq!(s.oi, 4_242, "open interest advances");
        assert_eq!(s.tick_count, 2, "both ticks counted");
    }

    #[test]
    fn two_packets_sharing_one_second_keep_last_write_wins() {
        // LTT is whole seconds and many packets share one. `>=` preserves the
        // pre-existing behaviour inside a second; `>` would have silently
        // frozen the close at the first packet of every second.
        let mut cell = AggregatorCell::empty();

        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 10, 100.0, 10), 10);
        fold(&mut cell, TfIndex::M1, &tick_at(OPEN + 10, 103.0, 20), 20);

        assert_eq!(
            cell.snapshot(TfIndex::M1).close,
            f32_to_f64_clean(103.0),
            "within one second the later-arriving packet is the close"
        );
    }

    #[test]
    fn a_self_contradictory_session_range_is_counted_rather_than_ignored() {
        // A packet whose session high sits BELOW its own session low is
        // internally impossible. The monotone marks make it harmless, but
        // harmless is not the same as seen.
        let mut cell = AggregatorCell::empty();

        let mut inverted = tick_at(OPEN + 10, 100.0, 10);
        inverted.day_high = 50.0;
        inverted.day_low = 150.0;
        let delta = cell.observe_session_extremes(&inverted);
        assert!(
            delta.is_empty(),
            "an inverted pair must never produce a widening"
        );
    }
}
