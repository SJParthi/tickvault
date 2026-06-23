//! Wave 6 Sub-PR #1 item 1.1 — per-instrument multi-TF cell type.
//!
//! Each subscribed `(security_id, exchange_segment)` instrument owns
//! exactly one [`AggregatorCell`], which contains 21 `Mutex<LiveCandleState>`
//! slots indexed by [`TfIndex::as_ordinal`]. Hot-path tick consumption
//! locks ONE slot at a time; the boundary timer's seal path locks the
//! same slot to drain + reset.
//!
//! Per the locked design decisions in
//! `.claude/plans/active-plan-aggregator-direct-flush-rehydrate.md`:
//!
//! - **L-C2** — `parking_lot::Mutex` per TF slot (operator-approved
//!   alternative to `crossbeam_utils::AtomicCell` because
//!   `LiveCandleState` is ~64 bytes which exceeds AtomicCell's 16-byte
//!   lock-free threshold; AtomicCell would spin-lock internally
//!   anyway). `parking_lot::Mutex` is ~30 ns uncontended on Linux.
//! - **L-C3 (REVISED — operator lock 2026-06-05, Option B):** a late tick
//!   (`exchange_timestamp` before the open bucket) whose minute is the
//!   MOST-RECENTLY sealed bucket re-folds its OWN minute's high/low/close
//!   ([`ConsumeOutcome::AmendedLate`]) and the caller re-emits → the writer
//!   UPSERTs that candle in place (the tick's timestamp decides its minute, so
//!   this is NOT a cross-bucket merge). Only a ≥2-bucket-late tick, or one with
//!   no amendable sealed bucket (post-`force_seal`), is [`ConsumeOutcome::DiscardLate`]
//!   + `error!(code = AGGREGATOR-LATE-01, ...)`. Supersedes the pre-2026-06-05
//!   "always discard / no silent merge" rule.
//! - **L-H7** — bucket alignment derived from `tick.exchange_timestamp`
//!   (IST epoch seconds, NEVER `Utc::now()`).
//! - **L-H12** — caller propagates `Option<exchange_segment_code>`
//!   for I-P1-11 safety; this module does NOT do segment lookups.
//! - **L-M19** — eager pre-population at boot via
//!   [`AggregatorCell::empty`] from the `InstrumentRegistry` composite
//!   key iter so first-tick has zero allocation.
//!
//! ## What this module ships
//!
//! - [`LiveCandleState`] — `Copy` data struct (≤ 96 bytes) with full
//!   Wave-5 OHLCV+OI+tick_count + 3 pct-stamping fields.
//! - [`AggregatorCell`] — per-instrument wrapper with 21
//!   `Mutex<LiveCandleState>` slots.
//! - [`ConsumeOutcome`] — explicit enum result for `consume_tick`:
//!   `Updated` (folded into open bucket), `Sealed { sealed_state }`
//!   (tick crossed boundary; previous bucket sealed AND new bucket
//!   opened with this tick), `DiscardLate` (tick belongs to a bucket
//!   already sealed; caller logs AGGREGATOR-LATE-01 + counter).
//!
//! ## What this module does NOT yet ship
//!
//! - The `papaya::HashMap<(u32, u8), Arc<AggregatorCell>>` aggregator
//!   container — Sub-PR #1 next commit.
//! - The `ShadowBarWriter` ring → spill → DLQ writer — item 1.2.
//! - The boundary-timer task — item 1.3.
//! - The tick-processor fan-in — item 1.4.
//! - The Wave-5 prev-day pct cache lookup at seal — item 1.5
//!   (this module exposes the 3 pct fields on `LiveCandleState`; the
//!   STAMPING happens in the seal-time writer that reads
//!   `Arc<HashMap<(u32,u8), PrevDayRefs>>`).
//! - DHAT zero-alloc + Criterion bench — item 1.6.
//! - Per-minute heartbeat + audit-table writer — items 1.8, 1.9.

use std::sync::Arc;

use parking_lot::Mutex;

use tickvault_common::tick_types::ParsedTick;

use crate::candles::{TF_COUNT, TfIndex};

// ---------------------------------------------------------------------------
// FeedStrategy — per-feed BEHAVIOUR as a typed parameter (NOT forked code)
// ---------------------------------------------------------------------------

/// What the cell does with a tick whose minute is EARLIER than the open bucket
/// but equal to the MOST-RECENTLY sealed bucket (a 1-bucket-late arrival).
///
/// The 3-agent adversarial review (2026-06-22, recorded in
/// `active-plan-groww-live-backtest-parity.md` §48) proved the two feeds differ
/// in EXACTLY this policy. It is DATA, not forked code — one engine, a per-feed
/// value, so Groww can join the same 21-TF `MultiTfAggregator` as Dhan:
///
/// - **Dhan** [`LatePolicy::Refold`] — re-fold the late tick into the just-sealed
///   minute's H/L/C and re-emit it ([`ConsumeOutcome::AmendedLate`]) so the writer
///   UPSERTs that minute in place (operator lock 2026-06-05, Option B).
/// - **Groww** [`LatePolicy::Discard`] — drop the out-of-order tick (count it via
///   [`ConsumeOutcome::DiscardLate`]), never touching a sealed candle. Groww's live
///   feed never re-folds; forcing it onto Refold would CHANGE its candle vs the
///   Groww backtest (the parity goal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatePolicy {
    /// Re-fold a 1-bucket-late tick into the just-sealed minute (Dhan).
    Refold,
    /// Discard the late tick, never amending a sealed candle (Groww).
    Discard,
}

/// Per-feed fold policy threaded through [`AggregatorCell::consume_tick`] and
/// [`crate::candles::MultiTfAggregator::consume_tick`]. `Copy` (a single enum
/// field) so it is a zero-cost stack argument on the hot path. Adding a future
/// per-feed knob (e.g. a volume convention flag) extends THIS struct — never a
/// second engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedStrategy {
    /// How a 1-bucket-late tick is handled. See [`LatePolicy`].
    pub late_policy: LatePolicy,
}

impl FeedStrategy {
    /// Dhan's strategy: re-fold 1-bucket-late ticks (Option B `AmendedLate`).
    /// This is the BEHAVIOUR the Dhan path has had unconditionally — passing it
    /// keeps the Dhan candle byte-identical to the pre-`FeedStrategy` code.
    pub const DHAN: Self = Self {
        late_policy: LatePolicy::Refold,
    };
    /// Groww's strategy: discard out-of-order ticks (never amend a sealed
    /// candle) — matches the legacy `Groww1mAggregator` semantics exactly.
    pub const GROWW: Self = Self {
        late_policy: LatePolicy::Discard,
    };
}

// ---------------------------------------------------------------------------
// LiveCandleState — Copy data struct
// ---------------------------------------------------------------------------

/// In-memory OHLCV+OI accumulator state for a single (instrument, TF)
/// open bucket.
///
/// `Copy` so the seal path can drain the slot via
/// `mem::replace(&mut *guard, LiveCandleState::empty(new_bucket_start))`
/// without allocating. Tests below pin the size to ≤ 96 bytes so a
/// future field addition that bloats the struct flags up immediately.
///
/// Per locked decision **L-H7** the `bucket_start_ist_secs` field is
/// derived from `tick.exchange_timestamp` (the WS LTT field — already
/// IST epoch seconds), NEVER from `Utc::now()`. The caller computes
/// it via `TfIndex::bucket_start(tick.exchange_timestamp)` before
/// calling [`AggregatorCell::consume_tick`].
///
/// Field semantics match the existing
/// `crates/trading/src/candles/engine.rs::Bar`. The 3 Wave-5 pct
/// fields stay 0.0 until the seal-time writer stamps them from the
/// in-memory `prev_day_cache` (locked decision L-H6).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveCandleState {
    /// Bucket-open IST epoch second (aligned to TF boundary).
    /// `0` means "slot never opened" — the empty/initial state.
    pub bucket_start_ist_secs: u32,
    /// Open price of this bucket (first tick's `last_traded_price`).
    pub open: f64,
    /// Running high.
    pub high: f64,
    /// Running low.
    pub low: f64,
    /// Close (last tick's `last_traded_price`).
    pub close: f64,
    /// **Incremental** volume within this bucket — `tick.volume - bucket_start_cumulative`.
    pub volume: u64,
    /// Cumulative-volume snapshot at bucket-open. Set ONCE per bucket
    /// open, reused on every subsequent tick to derive incremental
    /// `volume`. Item 28 (Wave 5) pattern carried over.
    pub bucket_start_cumulative: u64,
    /// Open Interest snapshot from the latest tick.
    pub oi: i64,
    /// Number of ticks folded into this bucket.
    pub tick_count: u32,
    /// `exchange_timestamp` (IST epoch secs) of the tick that set the current
    /// `close`. Used by the Option B late-tick amend (`fold_late_hlc`) to decide
    /// whether a late tick is genuinely the minute's LAST tick — close is
    /// overwritten ONLY when `late_tick.exchange_timestamp >= close_ts_ist_secs`,
    /// so an out-of-order EARLIER late tick can never clobber a truly-later close.
    pub close_ts_ist_secs: u32,
    /// Previous-day close baseline captured live from the tick's
    /// `day_close` field (Dhan's Quote-packet prev-day close — static per
    /// trading day, blank pre-market, populated from 09:15 IST). Last
    /// non-zero value seen wins, so a pre-market `0` never clobbers a real
    /// baseline. Feeds `close_pct_from_prev_day` at seal — no QuestDB /
    /// PrevDayCache dependency (operator decision 2026-05-28: take the
    /// prev-day close straight from the ticks `close` column).
    pub prev_day_close: f64,
    /// `close - prev_day.close` / `prev_day.close` * 100.0. Stamped at
    /// seal time per locked decision L-H6.
    pub close_pct_from_prev_day: f64,
    /// `oi - prev_day.oi` / `prev_day.oi` * 100.0. Stamped at seal time.
    pub oi_pct_from_prev_day: f64,
    /// `volume / prev_day.volume * 100.0`. Stamped at seal time.
    pub volume_pct_from_prev_day: f64,
    /// Today's SESSION open = the exchange-published `day_open` (Dhan
    /// Quote-packet bytes 34-37 = the NSE pre-open auction result, i.e.
    /// the official 09:15 open). Static per trading day, blank pre-market;
    /// last non-zero value wins so a pre-open `0` never clobbers it.
    /// Feeds `open_pct` at seal (operator lock 2026-06-01 §31 / Option 2).
    pub session_open: f64,
    /// `(close - session_open) / session_open * 100.0` — % change vs the
    /// official 09:15 open. Stamped at seal time. `0.0` if session_open is
    /// `0.0` (div-by-zero guard).
    pub open_pct: f64,
    /// `(session_open - prev_day_close) / prev_day_close * 100.0` — the
    /// OPENING GAP %: today's official 09:15 open vs yesterday's close
    /// (gap-up positive, gap-down negative). Operator request 2026-06-02.
    /// Stamped at seal time. `0.0` if `prev_day_close` is `0.0`
    /// (div-by-zero guard).
    pub open_gap_pct: f64,
}

impl LiveCandleState {
    /// Empty/initial state — `bucket_start_ist_secs == 0` flags the
    /// "never opened" sentinel. Used by [`AggregatorCell::empty`] for
    /// eager pre-population at boot per locked decision L-M19.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            bucket_start_ist_secs: 0,
            open: 0.0,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
            close: 0.0,
            volume: 0,
            bucket_start_cumulative: 0,
            oi: 0,
            tick_count: 0,
            close_ts_ist_secs: 0,
            prev_day_close: 0.0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
            session_open: 0.0,
            open_pct: 0.0,
            open_gap_pct: 0.0,
        }
    }

    /// Returns `true` if this slot has never been ticked (the boot
    /// state). [`Self::bucket_start_ist_secs`] is the cheap check.
    #[inline]
    #[must_use]
    pub const fn is_uninitialised(&self) -> bool {
        self.bucket_start_ist_secs == 0
    }

    /// Constructs the initial state from a tick that just opened a
    /// new bucket. `bucket_start` MUST equal
    /// `TfIndex::bucket_start(tick.exchange_timestamp)`; the caller
    /// owns that math because the cell type is TF-agnostic.
    ///
    /// `bucket_start_cumulative` is the cumulative volume snapshot at
    /// the start of THIS bucket (carried over from the previously
    /// sealed bucket so inter-bucket volume is preserved). On the
    /// session's very first tick, pass `0`.
    ///
    /// Candle-engine re-architecture #T1b — Task 5 (09:15 day-open):
    /// when `use_day_open` is `true` the bucket's `open` is set to
    /// `tick.day_open` (the exchange-published day-open price from the
    /// Quote packet) instead of the first tick's LTP. The caller passes
    /// `true` only for the FIRST bucket a slot opens after an
    /// IST-midnight reset, so the day's first bar of every timeframe
    /// opens at the NSE equilibrium open, not the first traded price.
    /// `high` / `low` / `close` ALWAYS track the LTP — never
    /// `day_high` / `day_low` (Dhan mislabels `day_close`, so it is
    /// never used for a candle's close per Ticket #5525125).
    ///
    /// `cumulative_volume` is the instrument's running cumulative day volume for
    /// THIS tick. The caller resolves it: the Dhan path passes
    /// `u64::from(tick.volume)` (the `u32` Quote-packet field); a feed whose
    /// cumulative exceeds `u32` (Groww `cum_volume`, an `i64`) passes the widened
    /// value via [`AggregatorCell::consume_tick`]'s `cumulative_volume_override`.
    /// Routing it as an explicit `u64` arg — rather than re-reading the `u32`
    /// `tick.volume` here — is what prevents the i64→u32 truncation the adversarial
    /// review flagged CRITICAL.
    #[inline]
    fn from_first_tick(
        tick: &ParsedTick,
        bucket_start: u32,
        bucket_start_cumulative: u64,
        use_day_open: bool,
        cumulative_volume: u64,
    ) -> Self {
        // Operator-spotted 2026-05-25: f64::from(f32) on price fields
        // produces IEEE-754 widening artifacts (e.g. 23925.65_f32 →
        // 23925.650390625_f64) that landed verbatim in candles_1m.
        // data-integrity.md "Price Precision Preservation" mandates
        // f32_to_f64_clean for ALL f32→f64 price conversions written
        // to QuestDB.
        let price = tickvault_common::price_precision::f32_to_f64_clean(tick.last_traded_price);
        // Day-open override: the day's first bar opens at the exchange
        // day-open. Fall back to the LTP if the Quote packet carried a
        // zero `day_open` (e.g. pre-open or a malformed packet) so the
        // candle still has a sensible open.
        let open = if use_day_open && tick.day_open > 0.0 {
            tickvault_common::price_precision::f32_to_f64_clean(tick.day_open)
        } else {
            price
        };
        let cumulative = cumulative_volume;
        Self {
            bucket_start_ist_secs: bucket_start,
            open,
            high: price,
            low: price,
            close: price,
            volume: cumulative.saturating_sub(bucket_start_cumulative),
            bucket_start_cumulative,
            oi: i64::from(tick.open_interest),
            tick_count: 1,
            close_ts_ist_secs: tick.exchange_timestamp,
            // Prev-day close baseline from the live Quote `close` field.
            // `0.0` pre-market (Dhan ships it blank until 09:15) — the
            // first 09:15+ tick supplies the real value via fold_in_bucket.
            prev_day_close: if tick.day_close > 0.0 {
                tickvault_common::price_precision::f32_to_f64_clean(tick.day_close)
            } else {
                0.0
            },
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
            // Session open from the live Quote `day_open` field (official
            // 09:15 open). `0.0` pre-market; first 09:15+ tick supplies the
            // real value via fold_in_bucket. Feeds `open_pct` at seal.
            session_open: if tick.day_open > 0.0 {
                tickvault_common::price_precision::f32_to_f64_clean(tick.day_open)
            } else {
                0.0
            },
            open_pct: 0.0,
            open_gap_pct: 0.0,
        }
    }

    /// Folds an in-bucket tick into the running state. Caller has
    /// already verified the tick belongs to THIS bucket (i.e.
    /// `TfIndex::bucket_start(tick.exchange_timestamp) == self.bucket_start_ist_secs`).
    ///
    /// `cumulative_volume` — see [`Self::from_first_tick`] for why the caller
    /// resolves it as a `u64` rather than re-reading `tick.volume`.
    #[inline]
    fn fold_in_bucket(&mut self, tick: &ParsedTick, cumulative_volume: u64) {
        // 2026-05-25: f32_to_f64_clean (not f64::from) — see
        // from_first_tick docs above for the IEEE-754 widening rationale.
        let price = tickvault_common::price_precision::f32_to_f64_clean(tick.last_traded_price);
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
        // In-order ticks have monotonically non-decreasing LTT, so this records
        // the latest close time (used by the Option B late-tick amend).
        self.close_ts_ist_secs = tick.exchange_timestamp;
        // Item 28 carry-over: incremental volume from bucket start.
        // saturating_sub guards against rare out-of-order tick where
        // current cumulative < bucket-start.
        self.volume = cumulative_volume.saturating_sub(self.bucket_start_cumulative);
        self.oi = i64::from(tick.open_interest);
        self.tick_count = self.tick_count.saturating_add(1);
        // Keep the last non-zero prev-day close (Quote `close`). A blank
        // pre-market 0 must never clobber a real baseline captured earlier.
        if tick.day_close > 0.0 {
            self.prev_day_close =
                tickvault_common::price_precision::f32_to_f64_clean(tick.day_close);
        }
        // Keep the last non-zero session open (Quote `day_open`). Mirrors
        // the prev_day_close guard above — a pre-open 0 must not clobber it.
        if tick.day_open > 0.0 {
            self.session_open = tickvault_common::price_precision::f32_to_f64_clean(tick.day_open);
        }
    }

    /// Option B (operator lock 2026-06-05): folds a LATE tick — one whose
    /// `exchange_timestamp` belongs to THIS (already-sealed) bucket but which
    /// physically arrived after the bucket was sealed — into the bucket's
    /// **high, low, and close** (H/L/C).
    ///
    /// - `high` (max) / `low` (min): always safe — order-independent extremes.
    /// - `close`: overwritten ONLY when this late tick is genuinely the minute's
    ///   LAST tick, i.e. `tick.exchange_timestamp >= close_ts_ist_secs`. This
    ///   prevents an out-of-order EARLIER late tick from clobbering a truly-later
    ///   close. For the operator's case (LTT 9:15:59 arriving at 9:16:00.000) the
    ///   tick IS the last second of the minute, so close updates.
    /// - `open` / `volume` / `oi`: untouched. `open` is the FIRST tick (a
    ///   latecomer can't be first); `volume`/`oi` are order-dependent cumulative
    ///   snapshots whose late value is ambiguous.
    #[inline]
    fn fold_late_hlc(&mut self, tick: &ParsedTick) {
        let price = tickvault_common::price_precision::f32_to_f64_clean(tick.last_traded_price);
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        if tick.exchange_timestamp >= self.close_ts_ist_secs {
            self.close = price;
            self.close_ts_ist_secs = tick.exchange_timestamp;
        }
        self.tick_count = self.tick_count.saturating_add(1);
    }
}

// ---------------------------------------------------------------------------
// ConsumeOutcome — explicit result of consume_tick
// ---------------------------------------------------------------------------

/// Result of folding one tick into one TF slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConsumeOutcome {
    /// Tick was folded into the existing open bucket; nothing to seal.
    Updated,
    /// Tick crossed the TF boundary: the previous bucket has been
    /// drained out of the slot (returned in `sealed_state`) and the
    /// slot has been re-initialised to a new bucket containing this
    /// tick.
    ///
    /// Caller MUST persist `sealed_state` to the corresponding shadow
    /// table. Direct-flush at seal per item 1.4.
    Sealed {
        /// The state of the bucket that just closed.
        sealed_state: LiveCandleState,
    },
    /// Option B (operator lock 2026-06-05): a LATE tick whose
    /// `exchange_timestamp` floors to the MOST-RECENTLY sealed bucket
    /// (`bucket_start == last_sealed[tf].bucket_start_ist_secs`) was re-folded
    /// into that bucket's high/low. `amended_state` is the corrected candle;
    /// the caller MUST route it through the SAME `on_seal` path so the writer
    /// UPSERTs it — replacing that minute's candle row in place (DEDUP
    /// `(ts, security_id, segment)`). Counts as `amended`, NOT `discarded`.
    AmendedLate {
        /// The just-amended (re-sealed) state to UPSERT.
        amended_state: LiveCandleState,
    },
    /// Tick belongs to a bucket STRICTLY OLDER than BOTH the open bucket AND
    /// the most-recently sealed bucket (≥ 2 buckets late), OR there is no
    /// amendable last-sealed bucket (e.g. just after `force_seal` cleared it
    /// across the day boundary). Only THEN is the tick dropped. Caller emits
    /// `error!(code = AGGREGATOR-LATE-01, ...)` + increments the
    /// late-discarded counter. (Supersedes the pre-2026-06-05 L-C3 "no silent
    /// merge" — a 1-bucket-late tick now amends its own minute instead.)
    DiscardLate,
}

// ---------------------------------------------------------------------------
// AggregatorCell — per-instrument 21-TF wrapper
// ---------------------------------------------------------------------------

/// Per-instrument multi-timeframe candle state.
///
/// Layout: `[Mutex<LiveCandleState>; 21]` indexed by
/// [`TfIndex::as_ordinal`]. The `Arc` is exposed to callers because
/// the planned aggregator container is
/// `papaya::HashMap<(u32, u8), Arc<AggregatorCell>>` per locked
/// decision L-C2 — papaya is read-mostly so we share `Arc`s into
/// every `pin().get()` site.
///
/// Each slot is independently lock-able; the hot-path tick consume
/// path locks ONE slot per tick, the boundary timer's seal path
/// locks the same slot to seal it. Contention is naturally bounded
/// to (instrument × TF), and each (instrument × TF × second) sees
/// at most a handful of ticks.
///
/// Candle-engine re-architecture #T1b — Task 5 (09:15 day-open):
/// `armed_for_day_open` carries one `AtomicBool` per TF slot. When a
/// slot is "armed", the NEXT tick that opens its first bucket sets the
/// bucket `open` to `tick.day_open` (the exchange day-open) rather than
/// the first tick's LTP. Slots are armed at construction
/// ([`Self::empty`]) and re-armed by [`Self::force_seal`] (the
/// IST-midnight reset), and disarmed the moment the day's first bucket
/// opens. Subsequent intraday boundary crossings open with the LTP.
#[derive(Debug)]
pub struct AggregatorCell {
    slots: [Mutex<LiveCandleState>; TF_COUNT],
    /// Per-TF "next opened bucket is the day's first bar" flag. See the
    /// struct docstring. `true` ⇒ next first-bucket-open uses
    /// `tick.day_open`; consumed (set `false`) on that open.
    armed_for_day_open: [std::sync::atomic::AtomicBool; TF_COUNT],
    /// Per-TF copy of the MOST-RECENTLY sealed bucket (Option B, operator lock
    /// 2026-06-05). A late tick whose `exchange_timestamp` floors to this bucket
    /// re-folds its high/low here and the caller re-emits → QuestDB UPSERT
    /// replaces that minute's candle in place. Stored only on an INTRADAY
    /// boundary-crossing seal; CLEARED by `force_seal` so a previous-day bucket
    /// is never amendable across the IST-midnight / 15:30 day boundary. Empty
    /// sentinel (`bucket_start_ist_secs == 0`) means "nothing amendable".
    last_sealed: [Mutex<LiveCandleState>; TF_COUNT],
}

impl AggregatorCell {
    /// Empty/uninitialised cell — every slot is
    /// `LiveCandleState::empty()`. Boot path eagerly pre-populates
    /// one of these per `(security_id, exchange_segment)` from the
    /// `InstrumentRegistry::iter()` composite-key iter per locked
    /// decision L-M19.
    ///
    /// Every slot starts **armed for day-open** (Task 5) — the first
    /// bucket each TF opens after boot uses `tick.day_open`.
    #[must_use]
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            slots: std::array::from_fn(|_| Mutex::new(LiveCandleState::empty())),
            armed_for_day_open: std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(true)),
            last_sealed: std::array::from_fn(|_| Mutex::new(LiveCandleState::empty())),
        })
    }

    /// Reads the current open state of the given TF slot.
    /// Lock-acquire-then-copy; the returned value is a snapshot — the
    /// underlying slot may change concurrently after this call.
    /// Used by the read-side trading bot snapshots and by tests.
    #[must_use]
    pub fn snapshot(&self, tf: TfIndex) -> LiveCandleState {
        *self.slots[tf.as_ordinal()].lock()
    }

    /// Folds a single tick into the given TF's open bucket. Returns
    /// the [`ConsumeOutcome`] explaining what happened.
    ///
    /// `bucket_start_cumulative` is the cumulative volume snapshot at
    /// the start of the NEW bucket if we cross a boundary. The caller
    /// computes this from the per-instrument cumulative tracker (the
    /// last close-out cumulative becomes the next bucket's start).
    /// Pass `0` on the session's very first tick.
    ///
    /// `strategy` is the per-feed [`FeedStrategy`]: Dhan passes [`FeedStrategy::DHAN`]
    /// ([`LatePolicy::Refold`] — the unconditional pre-`FeedStrategy` behaviour);
    /// Groww passes [`FeedStrategy::GROWW`] ([`LatePolicy::Discard`] — never amend a
    /// sealed candle). Under `Discard` the 1-bucket-late re-fold branch is skipped
    /// and a late tick always returns [`ConsumeOutcome::DiscardLate`].
    ///
    /// `cumulative_volume_override` carries a feed's running cumulative day volume
    /// as a `u64` when it does NOT fit the `u32` `tick.volume` field. The Dhan path
    /// passes `None` (the cell reads `u64::from(tick.volume)`); the Groww path
    /// passes `Some(cum_volume as u64)` — Groww `cum_volume` is an `i64` that
    /// exceeds `u32` intraday for liquid stocks, so funnelling it through
    /// `tick.volume: u32` would TRUNCATE (the adversarial-review CRITICAL). A `u64`
    /// arg avoids that with zero allocation.
    pub fn consume_tick(
        &self,
        tf: TfIndex,
        tick: &ParsedTick,
        bucket_start_cumulative: u64,
        strategy: FeedStrategy,
        cumulative_volume_override: Option<u64>,
    ) -> ConsumeOutcome {
        // Resolve the running cumulative ONCE: the override (Groww's widened i64→u64)
        // when present, else the Dhan `u32` Quote-packet field. No truncation.
        let cumulative_volume =
            cumulative_volume_override.unwrap_or_else(|| u64::from(tick.volume));
        let bucket_start = tf.bucket_start(tick.exchange_timestamp);
        let mut guard = self.slots[tf.as_ordinal()].lock();

        // First-ever tick for this slot — open the bucket. Task 5: if
        // this slot is armed (boot or post-IST-midnight reset) the
        // day's first bar opens at `tick.day_open`. Consume the flag so
        // every later bucket of the day opens at the LTP.
        if guard.is_uninitialised() {
            let use_day_open = self.armed_for_day_open[tf.as_ordinal()]
                .swap(false, std::sync::atomic::Ordering::Relaxed);
            *guard = LiveCandleState::from_first_tick(
                tick,
                bucket_start,
                bucket_start_cumulative,
                use_day_open,
                cumulative_volume,
            );
            return ConsumeOutcome::Updated;
        }

        // In-bucket tick — fold and return.
        if bucket_start == guard.bucket_start_ist_secs {
            guard.fold_in_bucket(tick, cumulative_volume);
            return ConsumeOutcome::Updated;
        }

        // Tick belongs to a strictly newer bucket — seal the current
        // one and open the new bucket. This is an intraday boundary
        // crossing, NOT the day's first bar, so the new bucket opens at
        // the LTP (`use_day_open = false`).
        if bucket_start > guard.bucket_start_ist_secs {
            let sealed_state = std::mem::replace(
                &mut *guard,
                LiveCandleState::from_first_tick(
                    tick,
                    bucket_start,
                    bucket_start_cumulative,
                    false,
                    cumulative_volume,
                ),
            );
            // Option B: remember this just-sealed INTRADAY bucket so a late tick
            // (LTT in it, arrived after the seal) can re-fold its high/low. Lock
            // order is ALWAYS slot-guard → last_sealed (here, the late arm, and
            // force_seal) → no deadlock.
            *self.last_sealed[tf.as_ordinal()].lock() = sealed_state;
            return ConsumeOutcome::Sealed { sealed_state };
        }

        // Tick belongs to an OLDER bucket — late arrival. Per-feed `late_policy`:
        // - Dhan ([`LatePolicy::Refold`], operator lock 2026-06-05): if its LTT
        //   floors to the most-recently sealed bucket, re-fold its high/low into
        //   THAT minute and re-emit (the writer UPSERTs, replacing the row in
        //   place). Only a ≥2-bucket-late tick, or one with no amendable
        //   last-sealed bucket (post-force_seal), is dropped.
        // - Groww ([`LatePolicy::Discard`]): never amend a sealed candle — always
        //   discard, matching the legacy `Groww1mAggregator` semantics exactly.
        if matches!(strategy.late_policy, LatePolicy::Refold) {
            let mut last = self.last_sealed[tf.as_ordinal()].lock();
            if !last.is_uninitialised() && bucket_start == last.bucket_start_ist_secs {
                last.fold_late_hlc(tick);
                return ConsumeOutcome::AmendedLate {
                    amended_state: *last,
                };
            }
        }
        ConsumeOutcome::DiscardLate
    }

    /// Force-seals the open bucket regardless of whether a new tick
    /// arrived. Used by the boundary-timer task at IST midnight to
    /// flush all 21 TFs even if no tick crosses the boundary in the
    /// next minute (e.g. illiquid contracts post-15:30).
    ///
    /// Returns `Some(sealed_state)` if the slot held a non-empty
    /// bucket; `None` if the slot was uninitialised. Caller persists
    /// the returned state to the corresponding shadow table.
    ///
    /// Task 5: force-sealing **re-arms** the slot for day-open — the
    /// IST-midnight force-seal empties every slot, and the first tick
    /// of the next trading day must open that day's first bar at
    /// `tick.day_open`. Re-arming unconditionally (even for an
    /// already-empty slot) is harmless and keeps the invariant simple.
    pub fn force_seal(&self, tf: TfIndex) -> Option<LiveCandleState> {
        let mut guard = self.slots[tf.as_ordinal()].lock();
        self.armed_for_day_open[tf.as_ordinal()].store(true, std::sync::atomic::Ordering::Relaxed);
        // Option B §4b (cross-day safety): a force-seal is the IST-midnight /
        // post-15:30 day boundary. CLEAR the amendable last-sealed bucket so a
        // stray in-session late tick can NEVER UPSERT a previous-day candle
        // across the day boundary. Only an INTRADAY boundary-crossing seal
        // (consume_tick) leaves an amendable bucket. Lock order: slot → last_sealed.
        *self.last_sealed[tf.as_ordinal()].lock() = LiveCandleState::empty();
        if guard.is_uninitialised() {
            return None;
        }
        let sealed_state = std::mem::replace(&mut *guard, LiveCandleState::empty());
        Some(sealed_state)
    }
}

// Compile-time size assertion — bumping this requires updating the
// per-instrument RAM budget in `aws-budget.md`.
// 2026-06-01 §31 Option 2: bumped 96 → 112 to carry `session_open` +
// `open_pct` (the official-09:15-open % column, mirrors the existing
// prev_day_close/close_pct per-slot pattern). RAM cost: +16 B × 21 TF ×
// ~250 SIDs ≈ 84 KB total — negligible on the 8 GiB host.
// 2026-06-02 (operator request): bumped 112 → 120 to carry `open_gap_pct`
// (the opening-gap % column). The companion `change_pct` column is DERIVED
// from `close_pct_from_prev_day` at the seal-row extractor, so it costs zero
// per-instrument RAM. RAM cost of `open_gap_pct`: +8 B × 21 TF × ~250 SIDs
// ≈ 42 KB total — negligible on the 8 GiB host (see aws-budget.md Tier 1).
// 2026-06-05 (operator lock, Option B late-tick amend): bumped 120 → 128 to
// carry `close_ts_ist_secs` (the close tick's LTT, so a late tick only
// overwrites close when it is genuinely the minute's last tick). RAM cost:
// +8 B × 21 TF × ~250 SIDs × 2 (slots + last_sealed) ≈ 84 KB total — negligible.
const _: () = assert!(
    std::mem::size_of::<LiveCandleState>() <= 128,
    "LiveCandleState exceeded 128-byte budget — every new field bloats per-instrument RAM by 21× (one slot per TF). Either shrink the new field or update aws-budget.md and bump this assertion."
);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::tick_types::ParsedTick;

    fn mk_tick(secs: u32, price: f32, volume: u32, oi: u32) -> ParsedTick {
        let mut t = ParsedTick::default();
        t.exchange_timestamp = secs;
        t.last_traded_price = price;
        t.volume = volume;
        t.open_interest = oi;
        t
    }

    #[test]
    fn test_live_candle_state_size_is_within_budget() {
        // 128-byte budget is pinned by the const _ assert above (bumped
        // from 96 for §31 session_open + open_pct, then 112 → 120 for the
        // 2026-06-02 open_gap_pct column, then 120 → 128 for the 2026-06-05
        // Option B close_ts_ist_secs); this runtime test makes the contract
        // grep-able.
        assert!(std::mem::size_of::<LiveCandleState>() <= 128);
    }

    #[test]
    fn test_live_candle_state_empty_is_uninitialised() {
        let s = LiveCandleState::empty();
        assert!(s.is_uninitialised());
        assert_eq!(s.bucket_start_ist_secs, 0);
        assert_eq!(s.open, 0.0);
        assert_eq!(s.tick_count, 0);
        // §31 Option 2: new fields default to 0.0.
        assert_eq!(s.session_open, 0.0);
        assert_eq!(s.open_pct, 0.0);
        // 2026-06-02: opening-gap % defaults to 0.0.
        assert_eq!(s.open_gap_pct, 0.0);
    }

    #[test]
    fn test_session_open_captured_from_day_open() {
        // §31 Option 2: the official 09:15 open is captured live from the
        // Quote `day_open` field (last non-zero wins, like prev_day_close).
        let cell = AggregatorCell::empty();
        let mut t1 = mk_tick(1_779_354_960, 100.0, 50, 1_000);
        t1.day_open = 100.0;
        cell.consume_tick(TfIndex::M1, &t1, 0, FeedStrategy::DHAN, None);
        assert_eq!(cell.snapshot(TfIndex::M1).session_open, 100.0);

        // A later in-bucket tick keeps session_open + updates close.
        let mut t2 = mk_tick(1_779_354_975, 103.0, 60, 1_010);
        t2.day_open = 100.0;
        cell.consume_tick(TfIndex::M1, &t2, 0, FeedStrategy::DHAN, None);
        let snap = cell.snapshot(TfIndex::M1);
        assert_eq!(snap.session_open, 100.0, "session_open preserved");
        assert_eq!(snap.close, 103.0);

        // A pre-open tick (day_open == 0) must NOT clobber the captured open.
        let mut t3 = mk_tick(1_779_354_990, 104.0, 70, 1_020);
        t3.day_open = 0.0;
        cell.consume_tick(TfIndex::M1, &t3, 0, FeedStrategy::DHAN, None);
        assert_eq!(
            cell.snapshot(TfIndex::M1).session_open,
            100.0,
            "day_open=0 must not clobber the real session open"
        );
    }

    #[test]
    fn test_live_candle_state_empty_high_low_sentinels_neg_inf_pos_inf() {
        // High starts at -inf, low at +inf, so the first real tick
        // always sets both correctly via `>`/`<` comparisons in
        // fold_in_bucket without a special-case.
        let s = LiveCandleState::empty();
        assert_eq!(s.high, f64::NEG_INFINITY);
        assert_eq!(s.low, f64::INFINITY);
    }

    #[test]
    fn test_snapshot_returns_independent_copy_per_tf() {
        // The snapshot accessor must (1) return the value of the
        // requested TF slot ONLY, (2) return a `Copy` so callers can
        // hold it across other operations without keeping the lock,
        // (3) be safe to call repeatedly. Pinned here in addition to
        // the implicit coverage in every test_consume_*/test_force_seal_*
        // assertion below.
        let cell = AggregatorCell::empty();
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 50, 1_000),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let snap_a = cell.snapshot(TfIndex::M1);
        let snap_b = cell.snapshot(TfIndex::M1);
        assert_eq!(snap_a, snap_b);
        // Distinct TFs must NOT share state.
        let snap_m5 = cell.snapshot(TfIndex::M5);
        assert!(snap_m5.is_uninitialised());
        // Mutating the cell after the first snapshot must NOT mutate
        // the previously-returned snapshot (Copy semantics).
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_975, 105.0, 60, 1_010),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let snap_after = cell.snapshot(TfIndex::M1);
        assert_eq!(snap_a.close, 100.0); // snap_a is a stale copy by design
        assert_eq!(snap_after.close, 105.0);
        assert_ne!(snap_a, snap_after);
    }

    #[test]
    fn test_aggregator_cell_empty_all_slots_uninitialised() {
        let cell = AggregatorCell::empty();
        for tf in TfIndex::ALL {
            assert!(
                cell.snapshot(tf).is_uninitialised(),
                "{} not empty",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_consume_tick_first_tick_opens_bucket() {
        let cell = AggregatorCell::empty();
        let tick = mk_tick(1_779_354_960, 100.0, 50, 1_000);
        let outcome = cell.consume_tick(TfIndex::M1, &tick, 0, FeedStrategy::DHAN, None);
        assert_eq!(outcome, ConsumeOutcome::Updated);
        let s = cell.snapshot(TfIndex::M1);
        assert!(!s.is_uninitialised());
        assert_eq!(s.bucket_start_ist_secs, 1_779_354_960); // already aligned
        assert_eq!(s.open, 100.0);
        assert_eq!(s.high, 100.0);
        assert_eq!(s.low, 100.0);
        assert_eq!(s.close, 100.0);
        assert_eq!(s.volume, 50);
        assert_eq!(s.oi, 1_000);
        assert_eq!(s.tick_count, 1);
    }

    #[test]
    fn test_prev_day_close_captured_from_tick_and_not_clobbered_by_premarket_zero() {
        // PR-4b: prev_day_close comes live from tick.day_close (Dhan Quote
        // `close` field). A later pre-market tick with day_close=0 must NOT
        // wipe a real baseline captured earlier.
        let cell = AggregatorCell::empty();
        // First tick carries a real prev-day close (e.g. 09:15+).
        let mut t1 = mk_tick(1_779_354_960, 100.0, 50, 1_000);
        t1.day_close = 95.5;
        cell.consume_tick(TfIndex::M1, &t1, 0, FeedStrategy::DHAN, None);
        assert_eq!(cell.snapshot(TfIndex::M1).prev_day_close, 95.5);
        // Subsequent in-bucket tick with day_close=0 must keep 95.5.
        let mut t2 = mk_tick(1_779_354_975, 101.0, 70, 1_010);
        t2.day_close = 0.0;
        cell.consume_tick(TfIndex::M1, &t2, 0, FeedStrategy::DHAN, None);
        assert_eq!(
            cell.snapshot(TfIndex::M1).prev_day_close,
            95.5,
            "pre-market 0 must not clobber the captured baseline"
        );
    }

    #[test]
    fn test_consume_tick_in_bucket_tick_folds_high_low_close_volume() {
        let cell = AggregatorCell::empty();
        // Bucket-aligned t=1716000900 (divisible by 60).
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 50, 1_000),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // Same bucket: t=1716000915 still floors to 1716000900.
        let outcome = cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_975, 105.0, 75, 1_010),
            0,
            FeedStrategy::DHAN,
            None,
        );
        assert_eq!(outcome, ConsumeOutcome::Updated);
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.bucket_start_ist_secs, 1_779_354_960);
        assert_eq!(s.open, 100.0); // unchanged
        assert_eq!(s.high, 105.0);
        assert_eq!(s.low, 100.0);
        assert_eq!(s.close, 105.0);
        assert_eq!(s.volume, 75); // 75 - 0 (bucket_start_cumulative)
        assert_eq!(s.oi, 1_010);
        assert_eq!(s.tick_count, 2);
    }

    #[test]
    fn test_consume_tick_low_updates_when_tick_below_open() {
        let cell = AggregatorCell::empty();
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let _ = cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_975, 95.0, 60, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.low, 95.0);
        assert_eq!(s.high, 100.0); // unchanged
    }

    #[test]
    fn test_consume_tick_boundary_crossing_seals_previous_bucket() {
        let cell = AggregatorCell::empty();
        // Bucket A: 1716000900..1716000960
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 50, 1_000),
            0,
            FeedStrategy::DHAN,
            None,
        );
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_990, 105.0, 70, 1_005),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // Bucket B: 1716000960..1716001020 — first tick crosses.
        // Pass bucket_start_cumulative=70 (the last cumulative from bucket A).
        let outcome = cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_355_021, 102.0, 80, 1_010),
            70,
            FeedStrategy::DHAN,
            None,
        );
        let sealed_state = match outcome {
            ConsumeOutcome::Sealed { sealed_state } => sealed_state,
            other => panic!("expected Sealed, got {other:?}"),
        };
        // Bucket A as sealed:
        assert_eq!(sealed_state.bucket_start_ist_secs, 1_779_354_960);
        assert_eq!(sealed_state.open, 100.0);
        assert_eq!(sealed_state.high, 105.0);
        assert_eq!(sealed_state.low, 100.0);
        assert_eq!(sealed_state.close, 105.0);
        assert_eq!(sealed_state.volume, 70);
        assert_eq!(sealed_state.tick_count, 2);

        // Slot now holds bucket B with this tick:
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.bucket_start_ist_secs, 1_779_355_020);
        assert_eq!(s.open, 102.0);
        assert_eq!(s.close, 102.0);
        assert_eq!(s.tick_count, 1);
        assert_eq!(s.volume, 10); // 80 - 70 (bucket_start_cumulative)
    }

    #[test]
    fn test_consume_tick_late_tick_returns_discard_late() {
        let cell = AggregatorCell::empty();
        // Open bucket at t=1716001000 (aligned to 60s boundary).
        let aligned_now = 1_779_355_500_u32; // 2026-05-21 09:25:00 IST (M1-aligned)
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(aligned_now + 30, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // Late tick belongs to the PREVIOUS bucket. NO bucket has been sealed
        // yet (only one bucket was ever opened), so there is no amendable
        // last-sealed bucket → DiscardLate (Option B falls through to discard).
        let late = mk_tick(aligned_now - 5, 99.0, 40, 0);
        let outcome = cell.consume_tick(TfIndex::M1, &late, 0, FeedStrategy::DHAN, None);
        assert_eq!(outcome, ConsumeOutcome::DiscardLate);
        // Slot state unchanged after the discard.
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.tick_count, 1);
        assert_eq!(s.close, 100.0);
    }

    // ---- Option B: late-tick re-folds its OWN minute (operator lock 2026-06-05) ----

    #[test]
    fn test_late_tick_into_just_sealed_bucket_amends_high_low_close() {
        let cell = AggregatorCell::empty();
        let b1 = 1_779_355_500_u32; // 09:25:00 IST, M1-aligned
        // Open bucket1: open=high=low=close=100 @ 09:25:30, volume 50.
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(b1 + 30, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // Cross the boundary @ 09:26:10 → seal bucket1, open bucket2.
        let sealed = cell.consume_tick(
            TfIndex::M1,
            &mk_tick(b1 + 70, 105.0, 60, 0),
            50,
            FeedStrategy::DHAN,
            None,
        );
        assert!(matches!(sealed, ConsumeOutcome::Sealed { .. }));
        // Late tick: LTT 09:25:50 belongs to the JUST-SEALED bucket1, price 110
        // (a new high), arriving out of order. The operator's exact case.
        let late = mk_tick(b1 + 50, 110.0, 55, 0);
        match cell.consume_tick(TfIndex::M1, &late, 50, FeedStrategy::DHAN, None) {
            ConsumeOutcome::AmendedLate { amended_state } => {
                assert_eq!(amended_state.bucket_start_ist_secs, b1);
                assert_eq!(amended_state.high, 110.0); // raised
                assert_eq!(amended_state.low, 100.0); // unchanged
                assert_eq!(amended_state.close, 110.0); // LTT 09:25:50 >= close_ts 09:25:30
                assert_eq!(amended_state.open, 100.0); // open NEVER changes
                assert_eq!(amended_state.volume, 50); // volume NEVER changes
            }
            other => panic!("expected AmendedLate, got {other:?}"),
        }
    }

    #[test]
    fn test_late_amend_keeps_close_when_late_tick_is_earlier_in_minute() {
        let cell = AggregatorCell::empty();
        let b1 = 1_779_355_500_u32;
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(b1 + 40, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        ); // close_ts=b1+40
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(b1 + 70, 105.0, 60, 0),
            50,
            FeedStrategy::DHAN,
            None,
        ); // seal bucket1
        // Late tick from EARLIER in bucket1 (LTT b1+10 < close_ts b1+40),
        // price 90 (a new low). low updates; close must NOT regress.
        let late = mk_tick(b1 + 10, 90.0, 55, 0);
        match cell.consume_tick(TfIndex::M1, &late, 50, FeedStrategy::DHAN, None) {
            ConsumeOutcome::AmendedLate { amended_state } => {
                assert_eq!(amended_state.low, 90.0); // order-independent → updated
                assert_eq!(amended_state.close, 100.0); // EARLIER tick → close unchanged
            }
            other => panic!("expected AmendedLate, got {other:?}"),
        }
    }

    #[test]
    fn test_force_seal_clears_last_sealed_no_cross_day_amend() {
        let cell = AggregatorCell::empty();
        let b1 = 1_779_355_500_u32; // 09:25:00
        let b2 = b1 + 120; // 09:27:00
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(b1 + 30, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(b1 + 70, 105.0, 60, 0),
            50,
            FeedStrategy::DHAN,
            None,
        ); // seal bucket1
        // §4b: a force-seal (IST-midnight / post-15:30) clears the amendable
        // bucket + empties the slot, so a previous-day bar can never be amended.
        let _ = cell.force_seal(TfIndex::M1);
        // A new bucket opens (next session).
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(b2 + 10, 200.0, 70, 60),
            60,
            FeedStrategy::DHAN,
            None,
        );
        // A stray late tick whose LTT floors to the OLD (force-sealed) bucket1
        // must NOT amend a cross-day candle — it is dropped.
        let late = mk_tick(b1 + 50, 999.0, 55, 0);
        assert_eq!(
            cell.consume_tick(TfIndex::M1, &late, 60, FeedStrategy::DHAN, None),
            ConsumeOutcome::DiscardLate
        );
    }

    #[test]
    fn test_consume_tick_distinct_tfs_isolated() {
        // A tick that crosses the M1 boundary should NOT cross the H1
        // boundary (because 60 boundaries fit in one hour). The cell
        // must independently track per-TF state.
        let cell = AggregatorCell::empty();
        let aligned = 1_779_358_500_u32; // 2026-05-21 10:15:00 IST (H1-aligned)
        // Open M1 + H1 at t=aligned+30:
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(aligned + 30, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        cell.consume_tick(
            TfIndex::H1,
            &mk_tick(aligned + 30, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // Cross M1 boundary at t=aligned+90 (still inside the same H1):
        let outcome_m1 = cell.consume_tick(
            TfIndex::M1,
            &mk_tick(aligned + 90, 102.0, 60, 0),
            50,
            FeedStrategy::DHAN,
            None,
        );
        let outcome_h1 = cell.consume_tick(
            TfIndex::H1,
            &mk_tick(aligned + 90, 102.0, 60, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        assert!(matches!(outcome_m1, ConsumeOutcome::Sealed { .. }));
        assert_eq!(outcome_h1, ConsumeOutcome::Updated);
    }

    #[test]
    fn test_force_seal_returns_some_when_initialised() {
        let cell = AggregatorCell::empty();
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let sealed = cell.force_seal(TfIndex::M1);
        assert!(sealed.is_some());
        let s = sealed.expect("just asserted some");
        assert_eq!(s.bucket_start_ist_secs, 1_779_354_960);
        assert_eq!(s.tick_count, 1);
        // Slot is now empty again.
        assert!(cell.snapshot(TfIndex::M1).is_uninitialised());
    }

    #[test]
    fn test_force_seal_returns_none_when_uninitialised() {
        let cell = AggregatorCell::empty();
        // Nothing consumed.
        assert!(cell.force_seal(TfIndex::M1).is_none());
        assert!(cell.force_seal(TfIndex::D1).is_none());
    }

    #[test]
    fn test_force_seal_only_affects_one_slot() {
        let cell = AggregatorCell::empty();
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        cell.consume_tick(
            TfIndex::M5,
            &mk_tick(1_779_354_960, 100.0, 50, 0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // Seal M1 only.
        cell.force_seal(TfIndex::M1);
        assert!(cell.snapshot(TfIndex::M1).is_uninitialised());
        assert!(!cell.snapshot(TfIndex::M5).is_uninitialised());
    }

    #[test]
    fn test_volume_uses_bucket_start_cumulative() {
        // bucket_start_cumulative is critical for correct incremental
        // volume across bucket boundaries (Item 28 carry-over).
        let cell = AggregatorCell::empty();
        // Open bucket A with cumulative volume = 1000.
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 1_050, 0),
            1_000,
            FeedStrategy::DHAN,
            None,
        );
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.volume, 50, "incremental from 1000 baseline = 1050 - 1000");
        assert_eq!(s.bucket_start_cumulative, 1_000);
    }

    #[test]
    fn test_pct_fields_default_zero_until_seal_time_stamp() {
        // The 3 Wave-5 pct fields stay 0.0 until the seal-time writer
        // stamps them from the prev_day cache (locked decision L-H6).
        let cell = AggregatorCell::empty();
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick(1_779_354_960, 100.0, 50, 1_000),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.close_pct_from_prev_day, 0.0);
        assert_eq!(s.oi_pct_from_prev_day, 0.0);
        assert_eq!(s.volume_pct_from_prev_day, 0.0);
    }

    #[test]
    fn test_aggregator_cell_is_send_sync_for_arc_sharing() {
        // The papaya HashMap stores Arc<AggregatorCell> per locked
        // decision L-C2. parking_lot::Mutex<T> requires T: Send for
        // the lock guards to cross threads; the array itself must be
        // Send + Sync. This test pins those bounds at compile time —
        // if a future field reverts the bound (e.g. !Send Rc),
        // compilation fails immediately.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AggregatorCell>();
        assert_send_sync::<Arc<AggregatorCell>>();
    }

    #[test]
    fn test_consume_outcome_is_copy_for_zero_alloc_callers() {
        // Hot-path callers in the next commit will pattern-match on
        // ConsumeOutcome and forward sealed_state to the shadow
        // writer's ring buffer. The Copy bound lets that happen
        // without cloning.
        fn assert_copy<T: Copy>() {}
        assert_copy::<ConsumeOutcome>();
        assert_copy::<LiveCandleState>();
    }

    // -----------------------------------------------------------------------
    // Candle-engine re-architecture #T1b — Task 5: 09:15 day-open tests
    // -----------------------------------------------------------------------

    /// Builds a tick carrying a `day_open` value alongside the LTP.
    fn mk_tick_with_day_open(secs: u32, ltp: f32, day_open: f32) -> ParsedTick {
        let mut t = mk_tick(secs, ltp, 0, 0);
        t.day_open = day_open;
        t
    }

    #[test]
    fn test_day_first_bar_opens_at_day_open_not_ltp() {
        // The first bucket a slot opens (cell is armed at construction)
        // must take `open` from `tick.day_open`, NOT the first tick LTP.
        let cell = AggregatorCell::empty();
        let tick = mk_tick_with_day_open(1_779_354_960, 105.0, 100.0);
        cell.consume_tick(TfIndex::M1, &tick, 0, FeedStrategy::DHAN, None);
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.open, 100.0, "day's first bar opens at tick.day_open");
        // high/low/close track the LTP, never day_open.
        assert_eq!(s.high, 105.0);
        assert_eq!(s.low, 105.0);
        assert_eq!(s.close, 105.0);
    }

    #[test]
    fn test_subsequent_intraday_bars_open_at_ltp_not_day_open() {
        // After the day's first bar, every later bucket opens at the
        // first tick's LTP — the armed flag was consumed.
        let cell = AggregatorCell::empty();
        // Day's first M1 bucket (armed → uses day_open).
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick_with_day_open(1_779_354_960, 105.0, 100.0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // Next M1 bucket — boundary crossing. day_open=100 still on the
        // tick, but the slot is disarmed → open must be the LTP 107.0.
        let outcome = cell.consume_tick(
            TfIndex::M1,
            &mk_tick_with_day_open(1_779_355_021, 107.0, 100.0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        assert!(matches!(outcome, ConsumeOutcome::Sealed { .. }));
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.open, 107.0, "intraday bar opens at LTP, not day_open");
    }

    #[test]
    fn test_force_seal_rearms_slot_for_next_day_open() {
        // The IST-midnight force-seal must re-arm the slot so the next
        // trading day's first bar opens at the new day_open.
        let cell = AggregatorCell::empty();
        // Day 1 first bar consumes the armed flag.
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick_with_day_open(1_779_354_960, 105.0, 100.0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // IST-midnight force-seal empties + re-arms the slot.
        cell.force_seal(TfIndex::M1);
        assert!(cell.snapshot(TfIndex::M1).is_uninitialised());
        // Day 2 first tick — armed again → opens at the new day_open.
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick_with_day_open(1_779_441_360, 210.0, 200.0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.open, 200.0, "day 2's first bar opens at new day_open");
    }

    #[test]
    fn test_day_open_falls_back_to_ltp_when_day_open_is_zero() {
        // A pre-open / malformed tick with day_open == 0.0 must NOT
        // produce a zero-open candle — fall back to the LTP.
        let cell = AggregatorCell::empty();
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick_with_day_open(1_779_354_960, 105.0, 0.0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        let s = cell.snapshot(TfIndex::M1);
        assert_eq!(s.open, 105.0, "zero day_open falls back to LTP");
    }

    #[test]
    fn test_day_open_arming_is_per_tf_independent() {
        // Each TF slot has its own armed flag — consuming M1's flag
        // must not disarm M5.
        let cell = AggregatorCell::empty();
        cell.consume_tick(
            TfIndex::M1,
            &mk_tick_with_day_open(1_779_354_960, 105.0, 100.0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        // M5's first bar must still use day_open.
        cell.consume_tick(
            TfIndex::M5,
            &mk_tick_with_day_open(1_779_354_960, 105.0, 100.0),
            0,
            FeedStrategy::DHAN,
            None,
        );
        assert_eq!(cell.snapshot(TfIndex::M1).open, 100.0);
        assert_eq!(cell.snapshot(TfIndex::M5).open, 100.0);
    }
}
