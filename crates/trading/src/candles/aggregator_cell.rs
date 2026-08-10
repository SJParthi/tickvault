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
//! | ticks folded unconditionally | non-finite / non-positive LTP REFUSED at ingest | `f32_to_f64_clean` passes `NaN` straight through and the Dhan quote parser is PROVEN to emit `NaN` OHLC. `NaN` is absorbing under `>`/`<`, so ONE such packet poisons `high`/`low` for the rest of the bucket AND every downstream row. Fail closed at ingest. |
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

use tickvault_common::price_precision::f32_to_f64_clean;
use tickvault_common::tick_types::ParsedTick;

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
/// Accepts finite AND strictly positive only. `NaN` and `±Inf` reach this
/// point because [`f32_to_f64_clean`] deliberately passes them through, and
/// the Dhan quote parser is proven to emit both `NaN` OHLC and negative LTP.
/// `NaN` is absorbing under comparison, so a single poisoned packet would
/// leave `high`/`low` permanently wrong for the bucket and write that row to
/// QuestDB. Refusing costs two comparisons.
///
/// # Complexity
/// O(1) — two scalar comparisons, no allocation.
#[inline]
#[must_use]
pub fn tick_price_is_sane(tick: &ParsedTick) -> bool {
    let p = tick.last_traded_price;
    p.is_finite() && p > 0.0
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
/// `21 × 128 × 2 + 21` ≈ **5.4 KB per instrument** (pinned by a
/// compile-time assertion below). At the 25,000-instrument slot ceiling that
/// is ~135 MB — real, budgeted against the 32 GiB r8g.xlarge host, and it
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
        }
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
                    fold_late_hlc(&mut self.last_sealed[ord], tick);
                    return ConsumeOutcome::AmendedLate {
                        amended_state: self.last_sealed[ord],
                    };
                }
                return ConsumeOutcome::DiscardLate;
            }
            let use_day_open = self.armed_for_day_open[ord];
            self.armed_for_day_open[ord] = false;
            self.slots[ord] = open_bucket(
                tick,
                bucket_start,
                bucket_start_cumulative,
                use_day_open,
                cumulative_volume,
            );
            return ConsumeOutcome::Updated;
        }

        let open_start = self.slots[ord].bucket_start_ist_secs;

        // In-bucket — the common case, and the one that must survive many
        // ticks sharing one second.
        if bucket_start == open_start {
            fold_in_bucket(&mut self.slots[ord], tick, cumulative_volume);
            return ConsumeOutcome::Updated;
        }

        // Strictly newer bucket — seal the open one and open the new one at
        // the LTP (an intraday crossing is never the day's first bar).
        if bucket_start > open_start {
            let sealed_state = std::mem::replace(
                &mut self.slots[ord],
                open_bucket(
                    tick,
                    bucket_start,
                    bucket_start_cumulative,
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
                fold_late_hlc(&mut self.last_sealed[ord], tick);
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

/// Builds the state of a bucket being opened by `tick`.
///
/// `use_day_open` makes the bar open at the exchange-published `day_open`
/// (the day's FIRST bar of each timeframe); it falls back to the LTP when
/// `day_open` is absent (`0.0` pre-open, or a malformed packet). `high` /
/// `low` / `close` ALWAYS track the LTP — never `day_high` / `day_low`.
#[inline]
fn open_bucket(
    tick: &ParsedTick,
    bucket_start: u32,
    bucket_start_cumulative: u64,
    use_day_open: bool,
    cumulative_volume: u64,
) -> LiveCandleState {
    let price = f32_to_f64_clean(tick.last_traded_price);
    // `> 0.0` is false for NaN, so a poisoned day_open can never be adopted.
    let open = if use_day_open && tick.day_open > 0.0 {
        f32_to_f64_clean(tick.day_open)
    } else {
        price
    };
    LiveCandleState {
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
        prev_day_close: if tick.day_close > 0.0 {
            f32_to_f64_clean(tick.day_close)
        } else {
            0.0
        },
        close_pct_from_prev_day: 0.0,
        oi_pct_from_prev_day: 0.0,
        volume_pct_from_prev_day: 0.0,
        session_open: if tick.day_open > 0.0 {
            f32_to_f64_clean(tick.day_open)
        } else {
            0.0
        },
        open_pct: 0.0,
        open_gap_pct: 0.0,
    }
}

/// Folds an in-bucket tick. The caller has already established that the tick
/// belongs to THIS bucket.
#[inline]
fn fold_in_bucket(state: &mut LiveCandleState, tick: &ParsedTick, cumulative_volume: u64) {
    let price = f32_to_f64_clean(tick.last_traded_price);
    if price > state.high {
        state.high = price;
    }
    if price < state.low {
        state.low = price;
    }
    state.close = price;
    state.close_ts_ist_secs = tick.exchange_timestamp;
    // saturating_sub guards the rare out-of-order tick whose cumulative is
    // below the bucket-start snapshot.
    state.volume = cumulative_volume.saturating_sub(state.bucket_start_cumulative);
    state.oi = i64::from(tick.open_interest);
    state.tick_count = state.tick_count.saturating_add(1);
    // Last NON-ZERO wins: a blank pre-market 0 must never clobber a real
    // baseline captured earlier in the session.
    if tick.day_close > 0.0 {
        state.prev_day_close = f32_to_f64_clean(tick.day_close);
    }
    if tick.day_open > 0.0 {
        state.session_open = f32_to_f64_clean(tick.day_open);
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
fn fold_late_hlc(state: &mut LiveCandleState, tick: &ParsedTick) {
    let price = f32_to_f64_clean(tick.last_traded_price);
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

// Per-instrument RAM pin. 21 TF × 128 B × 2 arrays + 21 flags, padded.
// Bumping this multiplies by the slot ceiling (25,000) — update
// `aws-budget.md` before raising it.
const _: () = assert!(
    std::mem::size_of::<AggregatorCell>() <= 5_632,
    "AggregatorCell exceeded its 5.5 KB per-instrument budget — this multiplies by AGGREGATOR_MAX_SLOTS; update aws-budget.md before raising."
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
    fn test_tick_price_is_sane_rejects_nan_inf_and_nonpositive() {
        assert!(tick_price_is_sane(&tick_at(OPEN, 100.0, 0)));
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            assert!(
                !tick_price_is_sane(&tick_at(OPEN, bad, 0)),
                "price {bad} must be refused"
            );
        }
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
