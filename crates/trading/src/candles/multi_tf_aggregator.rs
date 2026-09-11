//! `MultiTfAggregator` — the tick → multi-timeframe fold container.
//!
//! REBUILT 2026-08-09 (the original was hard-deleted 2026-07-17 in the
//! stage-3 dead-WS sweep; the Dhan live main-feed WS revival authorized by the
//! operator on 2026-08-09 needs it back). See
//! [`crate::candles::aggregator_cell`] for the per-instrument fold and for the
//! full "what changed vs the deleted shape" table.
//!
//! # The composite key — `(feed, security_id, exchange_segment_code)`
//!
//! `security_id` ALONE IS BANNED as an identity
//! (`.claude/rules/project/security-id-uniqueness.md`, I-P1-11): Dhan's
//! instrument master reuses the same numeric id across segments — FINNIFTY is
//! `security_id = 27` on `IDX_I` and a completely different instrument is
//! `27` on `NSE_EQ`. Keying on the number alone silently merges two
//! instruments' ticks into one candle.
//!
//! `feed` joins the key under the 2026-06-19 operator lock ("same tables +
//! feed column"): the shared `candles_*` DEDUP key is
//! `(ts, security_id, segment, feed)`, so two feeds observing the same
//! instrument are two distinct rows and must therefore be two distinct fold
//! states. Merging them here would produce one blended candle that matches
//! neither feed's own record — and it would silently defeat the whole
//! cross-verification design.
//!
//! # Slot allocation — bounded, O(1), fail-CLOSED
//!
//! Ids are NAMESPACE-BANDED (Groww index `[2^62, 2^63)`, GDF `[2^60, 2^62)`,
//! TrueData `[2^59, 2^60)`), so `security_id as usize` as an array index is
//! not merely wrong, it is astronomically out of range — the exact defect that
//! made `IndicatorEngine` a silent total no-op until it was repaired on
//! 2026-08-07 (daily-universe §28.2). This container therefore uses the same
//! repaired shape as [`crate::indicator`] and `rest_candle_fold::FoldSlots`:
//! a `HashMap<CompositeKey, u32>` handing out DENSE indices into a `Vec`, hard
//! capped at [`AGGREGATOR_MAX_SLOTS`]. At capacity it REFUSES the tick — loud
//! and counted — it never grows unbounded and it never reuses another
//! instrument's slot.
//!
//! # Zero allocation on the per-tick path
//!
//! Steady state: one `HashMap::get` on a `Copy` key, one `Vec` index, 21
//! scalar folds. No `Vec::new`, no `String`, no `format!`, no `collect`, no
//! `clone`. The ONLY allocation is on first sight of a new instrument (one map
//! insert + one `Vec::push`) — the cold path, once per instrument per process.
//!
//! # Why `std::collections::HashMap` and not `papaya`
//!
//! `papaya` buys lock-free CONCURRENT reads and pays for epoch reclamation.
//! This table is owned outright by ONE tokio task and is only ever reached
//! through `&mut MultiTfAggregator`, so there is nothing to make lock-free.
//! `DashMap` is banned on hot paths regardless.

use std::collections::HashMap;

use tickvault_common::constants::MAX_PLAUSIBLE_LTP;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;

use crate::candles::aggregator_cell::{AggregatorCell, ConsumeOutcome, FeedStrategy, TickPrices};
use crate::candles::tf_index::{
    CANDLE_SESSION_OPEN_SECS_OF_DAY_IST, MARKET_CLOSE_SECS_OF_DAY_IST, fold_clock_ist_secs,
};
use crate::candles::{BufferOutcome, BufferedSeal, LiveCandleState, SealRing, TfIndex};

/// Hard ceiling on distinct `(feed, security_id, segment)` identities the
/// container will fold. Matches the `rest_candle_fold::FOLD_MAX_SLOTS` /
/// `MAX_INDICATOR_INSTRUMENTS` house ceiling and the ~25,000-instrument target
/// scale in `daily-universe-scope-expansion-2026-05-27.md` §0 Quote 13.
///
/// Worst-case RAM at the ceiling: 25,000 × ~5.4 KB ≈ **135 MB** — budgeted
/// against the 32 GiB r8g.xlarge host. Slots materialise on first sight, so
/// today's handful of live instruments cost a few KB.
pub const AGGREGATOR_MAX_SLOTS: usize = 25_000;

/// Slots pre-allocated by [`MultiTfAggregator::new`].
///
/// 1,000 is the adaptive-universe STARTING size in the 16-connection design
/// (`.claude/plans/proposals/2026-08-09-dhan-16-connection-architecture.md`),
/// chosen there to sit below the measured ingest ceiling. Pre-sizing to it
/// costs ~5.4 MB up front and keeps the slot table realloc-free for the whole
/// range the sizer actually starts in, instead of reallocating and memmoving
/// a multi-kilobyte-per-cell table as the universe fills.
///
/// This is a pre-allocation, NOT a ceiling: growth past it is allowed and
/// reallocs (cold path). The hard ceiling is [`AGGREGATOR_MAX_SLOTS`].
pub const AGGREGATOR_DEFAULT_SLOTS: usize = 1_000;

/// Earliest `exchange_timestamp` (IST epoch seconds) treated as real.
///
/// 2020-09-13. Comfortably before any tickvault data has ever existed, so it
/// can never reject a legitimate tick, while still rejecting the small values
/// (0, 1, a few thousand) that a corrupt or zero-filled packet produces.
pub const MIN_PLAUSIBLE_EXCHANGE_TS_SECS: u32 = 1_600_000_000;

/// Latest `exchange_timestamp` (IST epoch seconds) treated as real.
///
/// 2050-01-01. `exchange_timestamp` is a raw `u32` off the wire that no parser
/// range-validates, and it drives the event-time watermark, which in turn
/// drives `catch_up_seal_all` across every slot. An all-ones LTT is
/// ~4.29 billion (year 2106) and would force-seal the entire live book.
///
/// An ABSOLUTE bound rather than a relative jump cap, deliberately: the
/// watermark starts at 0, so a relative cap cannot distinguish the first
/// honest tick (a ~1.78-billion-second jump from zero) from poison. That
/// exact mistake was made and caught by these tests before it shipped.
pub const MAX_PLAUSIBLE_EXCHANGE_TS_SECS: u32 = 2_524_608_000;

/// The composite identity. `security_id` alone is BANNED (I-P1-11); `feed` is
/// part of it under the 2026-06-19 feed-in-key lock.
type CompositeKey = (Feed, u64, u8);

/// One instrument's fold state.
#[derive(Clone, Debug)]
struct InstrumentSlot {
    /// The composite identity this slot belongs to — carried so seal
    /// emissions can name the instrument without a reverse lookup.
    key: CompositeKey,
    /// Per-timeframe candle state.
    cell: AggregatorCell,
    /// Cumulative day volume as of the END of the last tick folded. On a
    /// boundary crossing this becomes the new bucket's volume baseline.
    ///
    /// MONOTONIC by construction (see `consume_tick`): it may advance, never
    /// regress. `tick.volume` is DAY-CUMULATIVE, so a late tick carries a
    /// SMALLER value than the one already stored; letting that value land
    /// here dragged the NEXT bucket's baseline backwards and inflated its
    /// volume by the whole regression. Measured live 2026-08-24: intraday
    /// frames summed to ~9.2x the day bar.
    last_cumulative: u64,
    /// `false` until the first tick this slot ever folds.
    ///
    /// A slot created MID-SESSION starts with no knowledge of the volume the
    /// instrument already traded, and `0` is not that knowledge — it is the
    /// absence of it. Treating `0` as a baseline made the first bucket report
    /// `cumulative - 0`, i.e. THE ENTIRE DAY SO FAR, in one bar. The first
    /// tick seeds the baseline instead, so the first bar reports `0` and the
    /// unattributable volume is COUNTED
    /// (`tv_aggregator_slot_volume_baseline_seeded_total`) rather than
    /// invented. Under-reporting one bucket is far less wrong than
    /// over-reporting by a whole day, and it must not be silent.
    volume_baseline_seeded: bool,
    /// Last accepted last-traded price, in rupees.
    ///
    /// Stored on the slot that ALREADY EXISTS per instrument rather than in a
    /// second per-instrument map. A parallel map would be one more structure
    /// bounded only by caller convention -- the unbounded-growth shape this
    /// codebase's own O(1) table has recorded and removed repeatedly -- and it
    /// could disagree with the fold about which price was last accepted.
    ///
    /// `f64::NAN` until the first accepted tick, deliberately: a reader must be
    /// able to tell "no price yet" from a real zero, and `0.0` is a live
    /// sentinel on this feed (Ticker-mode packets and pre-open instruments both
    /// carry it). Every consumer of this value already refuses a non-finite.
    last_ltp: f64,
    /// Direction of the last CLASSIFIED tick for this instrument: `+1`
    /// buy-initiated, `-1` sell-initiated, `0` before any classification.
    ///
    /// This is the zero-tick carry of the tick rule. A tick whose price equals
    /// the previous tick's is attributed to the side that last moved the
    /// price — unchanged-price ticks are the MAJORITY on a liquid contract, so
    /// discarding them would under-report a bar's flow by most of its volume,
    /// and splitting them evenly would invent a number the rule does not say.
    ///
    /// Per INSTRUMENT, not per timeframe. All 24 frames see the same tick
    /// sequence, so one carry serves them all; storing it per frame would be 24
    /// copies of one fact and would let them drift.
    ///
    /// `i8` because it holds three values. At `AGGREGATOR_MAX_SLOTS` (25,000)
    /// that is 25 KB across the fleet — a rounding error against the 170 MB
    /// slot table, and the reason this lives here rather than on
    /// `LiveCandleState`, where it would have cost 24 bytes per instrument and
    /// pushed a second budget assert.
    last_tick_sign: i8,
}

/// Per-tick outcome, coalesced across all [`TF_COUNT`](crate::candles::TF_COUNT)
/// timeframes so the caller emits ONE log line / counter set per tick rather
/// than 21.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConsumeStats {
    /// Timeframes that sealed a bucket and emitted it. `0..=TF_COUNT`.
    pub sealed_count: u8,
    /// Timeframes whose most-recently-sealed bucket was AMENDED by this late
    /// tick and re-emitted for UPSERT. `0..=TF_COUNT`.
    pub amended_count: u8,
    /// Timeframes that dropped this tick as too late to place.
    /// `0..=TF_COUNT`.
    pub late_count: u8,
    /// `true` when the tick was refused before any state was touched because
    /// its price was `NaN` / `±Inf` / non-positive. Nothing was folded.
    pub refused_price: bool,
    /// `true` when the tick fell outside the `[09:15, 15:40)` IST candle
    /// window. Nothing was folded.
    pub out_of_session: bool,
    /// `true` when the vendor stamped this tick for a LATER IST day than our
    /// own receipt clock. Nothing was folded, and — crucially — the watermark
    /// was NOT advanced.
    pub future_trading_day: bool,
    /// `true` when the slot table was at [`AGGREGATOR_MAX_SLOTS`] and this
    /// instrument therefore has NO fold state. Fail-closed: nothing was
    /// folded, and the caller must treat it as a real data loss.
    pub slot_exhausted: bool,
    /// `true` when the price is EXACTLY `0.0` — the vendor's documented
    /// "has not traded yet" sentinel, not corruption.
    ///
    /// Added 2026-08-20 after the live box refused ~22,000 ticks a session on
    /// this shape. An option contract that has not traded sends `0.0`, and the
    /// old gate (`p > 0.0`) swept it in with `NaN` and negative prices —
    /// so the whole tick was discarded, row included.
    ///
    /// Those two are not alike. `NaN` is a broken packet and writing it would
    /// put a corrupt row under a garbage timestamp. `0.0` is TRUE: the
    /// instrument has no last traded price, and its packet still carries real
    /// open interest, bid/ask and timestamps. Discarding it loses the ability
    /// to tell "did not trade" from "did not capture" — and the depth path in
    /// the same drain already writes `0.0` levels as exactly this kind of
    /// documented sentinel.
    ///
    /// So this is a CANDLE-only refusal, like `out_of_session`: nothing is
    /// folded (a zero would corrupt the OHLC), and the caller still writes the
    /// tick row.
    pub untraded_sentinel: bool,
    /// `true` when `exchange_timestamp` is EXACTLY `0` — the vendor's "no last
    /// trade time" sentinel, the timestamp twin of [`Self::untraded_sentinel`].
    ///
    /// Added 2026-08-26 after the live box discarded **825,783 ticks in one
    /// session (4.0% of every tick decoded)** on this shape, with **no row at
    /// all** — not a missing candle, but no record the instrument was even
    /// seen.
    ///
    /// The 2026-08-20 fix had already made this decision correctly for the
    /// PRICE sentinel, and its reasoning applies here verbatim: discarding the
    /// row loses the ability to tell "did not trade" from "did not capture",
    /// and costs the packet's open interest and bid/ask with it. An instrument
    /// that has never traded has no last price AND no last trade time — the two
    /// sentinels co-occur — but the timestamp check ran first and hard-refused,
    /// silently defeating that fix for exactly the instruments it was for.
    ///
    /// CANDLE-only, like its price twin: folding a zero timestamp would place
    /// the bar in 1970. The ROW is safe because
    /// `tick_persistence::row_timestamp_ist_nanos` already falls back to
    /// `received_at` for any out-of-band value, so it lands in TODAY's
    /// partition.
    pub untraded_timestamp: bool,
    /// `true` when `exchange_timestamp` fell outside
    /// `[MIN_PLAUSIBLE_EXCHANGE_TS_SECS, MAX_PLAUSIBLE_EXCHANGE_TS_SECS]`
    /// **but a real receipt time is available**, so the writer can stamp the
    /// row safely.
    ///
    /// Added 2026-08-28 after the live box hard-refused **2,008,916 ticks in
    /// one session (2.41% of every tick decoded)** on this shape, writing NO
    /// ROW at all. The third instance of the same class, after the price
    /// sentinel (2026-08-20) and the zero-timestamp sentinel (2026-08-26).
    ///
    /// CANDLE-only, like both of those: the second cannot be bucketed, but
    /// `tick_persistence::row_timestamp_ist_nanos` already falls back to the
    /// receipt for any out-of-band value, so the row lands in TODAY's
    /// partition. Without a receipt the tick stays a HARD refusal
    /// ([`Self::refused_timestamp`]) — no safe stamp exists there.
    pub out_of_band_timestamp: bool,
    /// `true` when `exchange_timestamp` fell outside
    /// `[MIN_PLAUSIBLE_EXCHANGE_TS_SECS, MAX_PLAUSIBLE_EXCHANGE_TS_SECS]`.
    /// Nothing was folded and the watermark was NOT advanced.
    pub refused_timestamp: bool,
    /// `true` when the tick's IST **date** is older than the newest date this
    /// aggregator has seen — a stale last-trade time, not a stale packet.
    ///
    /// # The bug this closes (measured on prod, 2026-08-26)
    ///
    /// Dhan sends the **last trade time**, so a contract that last traded days
    /// or weeks ago is snapshotted NOW carrying a timestamp from THEN. Measured
    /// across all 20.5M rows in one session: mean `received_at - ts` was
    /// **~5 hours**, max **34 days**.
    ///
    /// The session gate above tests `exchange_timestamp % 86_400` — SECONDS OF
    /// DAY ONLY, with no notion of which day. A stale trade time of
    /// *yesterday 15:39:41* yields 56,381, which is inside the
    /// `[09:15:00, 15:40:00)` window, so it passed as "in session" and opened a
    /// candle bucket **dated on the stale day**.
    ///
    /// Two consequences, both verified live:
    ///
    /// 1. **Fabricated history.** `candles_1m` held **8,898 bars on past
    ///    dates** — 8,898 distinct instruments, oldest `2026-07-23T09:39` — in a
    ///    QuestDB volume that was created empty at **08:59:50 that same
    ///    morning**. They cannot be history; they were written that day.
    /// 2. **The day open is destroyed.** With a bucket already open on the
    ///    stale date, today's real 09:15 tick takes the CONTINUE path instead
    ///    of the OPEN path, so the day-open arm never fires — across all 24
    ///    timeframes for that instrument.
    ///
    /// # Why this is a CANDLE-only refusal
    ///
    /// The tick is not corrupt. It is a real last-traded price with a real
    /// (old) trade time, and it carries live open interest and bid/ask. The
    /// row is kept for exactly the reason `untraded_sentinel` is kept:
    /// discarding it would lose the ability to tell "did not trade today" from
    /// "did not capture". Only the FOLD is skipped, because folding it is what
    /// fabricates a bar on a day that already closed.
    pub stale_trading_day: bool,
}

impl ConsumeStats {
    /// `true` when the tick was folded into at least the open buckets (i.e.
    /// it was neither refused, out of session, nor slot-exhausted).
    ///
    /// This is a NEGATIVE predicate — it reports success by the absence of
    /// every known refusal — so ANY new refusal field MUST be added here too.
    /// Miss one and a refused tick reports itself as folded, which is the
    /// false-OK class the charter forbids. The test
    /// `test_every_refusal_field_makes_folded_false` enforces it mechanically
    /// rather than relying on whoever adds the next field remembering.
    #[must_use]
    pub fn folded(&self) -> bool {
        !self.refused_price
            && !self.out_of_session
            && !self.slot_exhausted
            && !self.refused_timestamp
            && !self.untraded_sentinel
            && !self.stale_trading_day
            && !self.future_trading_day
            && !self.untraded_timestamp
            && !self.out_of_band_timestamp
    }
}

/// Multi-instrument, multi-timeframe tick fold.
///
/// Single-owner (`&mut self`). One instance can serve every feed at once
/// because `feed` is part of the key.
#[derive(Debug)]
pub struct MultiTfAggregator {
    /// Dense, index-stable storage. Slots are appended, never removed or
    /// reordered, so an index handed out stays valid for the process life.
    slots: Vec<InstrumentSlot>,
    /// Composite identity → dense index into [`Self::slots`]. O(1) average.
    index: HashMap<CompositeKey, u32>,
    /// Late-tick policy applied to every fold. A PARAMETER — see
    /// [`FeedStrategy`] / [`crate::candles::LatePolicy`] for the documented
    /// default ([`FeedStrategy::DEFAULT`] = `Refold`).
    strategy: FeedStrategy,
    /// Max `exchange_timestamp` ever seen (IST epoch seconds), the event-time
    /// watermark that drives [`Self::catch_up_seal_all`]. Advanced BEFORE the
    /// session gate so a post-close tick still lets the final session bar
    /// close. Never regresses, so a re-delivered duplicate cannot move it.
    watermark_secs: u32,
    /// Lifetime count of ticks refused because the slot table was full.
    slots_exhausted_total: u64,
    /// Coalescing latch — one `error!` per process for capacity exhaustion;
    /// every occurrence is still counted.
    exhausted_logged: bool,
    /// Test-only slot-ceiling override so the fail-closed exhaustion path can
    /// be exercised without allocating 25,000 cells (~135 MB).
    #[cfg(test)]
    test_capacity_override: Option<usize>,
}

impl Default for MultiTfAggregator {
    fn default() -> Self {
        Self::new(FeedStrategy::DEFAULT)
    }
}

/// Classifies one tick's traded volume as buy- or sell-initiated (the tick
/// rule), returning it signed.
///
/// # The rule
///
/// - `price > prev` — an UPTICK. The trade lifted the offer, so the aggressor
///   was a buyer: `+delta`.
/// - `price < prev` — a DOWNTICK. The trade hit the bid: `-delta`.
/// - `price == prev` — a ZERO TICK. Attributed to whichever side last moved
///   the price, via `carry`. This is the case that decides whether the column
///   is useful at all: unchanged-price ticks are the majority on a liquid
///   contract, so discarding them would under-report a bar's flow by most of
///   its volume, and halving them would invent a number the rule does not say.
///
/// `carry` is read AND updated: an up/down tick writes the new direction, a
/// zero tick reads it and leaves it alone.
///
/// # The four refusals, each returning `0`
///
/// - **No delta** — nothing traded since the previous tick, so there is
///   nothing to classify. The common case for a repeated snapshot.
/// - **No previous price** (`prev` non-finite) — the first accepted tick for
///   this instrument, where `last_ltp` is still `NaN`. `NaN` fails BOTH `>`
///   and `<`, so an unguarded comparison would land on the zero-tick arm and
///   attribute the whole first delta to a carry that is itself `0` — silently
///   correct today, and silently wrong the moment the carry is non-zero from a
///   previous day. Refused explicitly instead.
/// - **Non-finite or non-positive current price** — `0.0` is this feed's
///   absent-price sentinel (Ticker-mode packets, pre-open instruments), never
///   a real price, and a poisoned price cannot classify anything.
/// - **Zero tick with no carry** — the price has not moved since the first
///   tick we ever saw for this instrument, so no side has revealed itself.
///   Returning `0` says "unclassified"; guessing would be fabrication.
///
/// # Why the comparison is exact and not a tolerance
///
/// Both prices come from `f32_to_f64_clean`, so an unchanged price is
/// bit-identical on both sides and compares equal. A widening `f32 as f64`
/// would make `10.20` become `10.19999980926514` and report an unchanged price
/// as an UPTICK — systematically, on the majority of ticks, which would turn
/// this column into a near-copy of gross volume.
///
/// # Complexity
/// O(1) — three compares, one negate, one byte written. Zero allocation. Runs
/// ONCE per tick, never once per timeframe.
/// A backwards step in the vendor's day-cumulative volume at or beyond this
/// size is a counter RESTART (a `u32` wrap, or a day rollover), never a stale
/// packet — and the two need opposite remedies. See the call site in
/// [`MultiTfAggregator::consume_tick_with_prices`] for why refusing a restart
/// silently kills the instrument for the rest of the session.
///
/// Half the `u32` range. Chosen because it is the largest floor that cannot
/// produce a false positive — a stale packet is behind by the volume traded
/// between two packets we received, and no plausible gap approaches 2^31 — and
/// the smallest that cannot produce a false negative, since a wrap from just
/// below `u32::MAX` back to just above zero is a drop of nearly the full `u32`
/// range, and a day rollover drops the entire previous day's volume.
const CUMULATIVE_RESTART_DROP_FLOOR: u64 = 1 << 31;

#[inline]
#[must_use]
fn classify_tick_volume(prev: f64, price: f64, delta: u64, carry: &mut i8) -> Option<i64> {
    // ⚠ 2026-09-11: this returned a bare `i64` and answered `0` to FOUR
    // different questions — "nothing traded", "the price is unusable", "no
    // previous price", and "real volume whose side is unknown". The call site
    // then wrapped every one of them in `Some(..)`, so `net_volume_classified`
    // was `true` on every live bar and the `None` arm the fold already carries
    // (`aggregator_cell::fold_in_bucket`) was UNREACHABLE from the live path.
    //
    // The consequence is the one this column exists to prevent: a bar whose
    // volume was entirely unclassifiable published `Some(0)` — "buy and sell
    // flow were perfectly balanced" — about flow nobody measured. `Some(0)` and
    // `None` are now two different answers, which is what the storage layer,
    // the spill format and `net_volume()` were all already built to expect.
    if delta == 0 {
        // GENUINELY ZERO, not unclassifiable: no volume traded between this
        // packet and the last accepted one, so there is no flow to attribute
        // and the bar stays fully classified. Duplicate packets (the same
        // update delivered twice, which this feed does routinely) land here.
        return Some(0);
    }
    if !price.is_finite() || price <= 0.0 {
        // Real volume arrived under a price we cannot read — a Ticker-mode
        // `0.0` sentinel or a corrupt field. Unclassifiable, never zero.
        return None;
    }
    // Saturate BEFORE the sign: `-(u64 as i64)` past `i64::MAX` wraps POSITIVE,
    // which would record a sell as a buy. Same hazard, same handling, as the
    // tick-persistence path.
    let magnitude = i64::try_from(delta).unwrap_or(i64::MAX);
    if !prev.is_finite() || prev <= 0.0 {
        // First classifiable tick for this instrument: there is no previous
        // price to compare against, so the delta is real but unclassifiable.
        // The carry is deliberately NOT written — inventing a direction here
        // would then propagate to every zero tick that follows.
        return None;
    }
    if price > prev {
        *carry = 1;
        Some(magnitude)
    } else if price < prev {
        *carry = -1;
        Some(-magnitude)
    } else {
        match *carry {
            1 => Some(magnitude),
            -1 => Some(-magnitude),
            // Real volume, price unchanged, and no side has EVER revealed
            // itself for this instrument. This is the opening-bar case: the
            // honest answer is "we do not know", never "balanced".
            _ => None,
        }
    }
}

impl MultiTfAggregator {
    /// Aggregator with an explicit late-tick policy, pre-sized to
    /// [`AGGREGATOR_DEFAULT_SLOTS`].
    ///
    /// Deliberately NOT an unsized `Vec::new()` / `HashMap::new()`. Slot
    /// allocation happens on first sight of an instrument, which is a cold
    /// path — but an unsized `Vec` reallocs and memmoves the whole slot table
    /// as the universe fills, and each cell is multiple kilobytes. Pre-sizing
    /// to the design's adaptive-universe starting point makes the common case
    /// realloc-free rather than merely rare-realloc. Growth beyond that still
    /// reallocs (cold path, flagged honestly, never relabelled O(1));
    /// [`Self::with_capacity`] removes it entirely when the universe size is
    /// known at boot.
    #[must_use]
    pub fn new(strategy: FeedStrategy) -> Self {
        Self::with_capacity(strategy, AGGREGATOR_DEFAULT_SLOTS)
    }

    /// Empty aggregator pre-sized for `cap` instruments, so the boot path can
    /// avoid re-hashing / re-allocating mid-session. `cap` is clamped to
    /// [`AGGREGATOR_MAX_SLOTS`].
    #[must_use]
    pub fn with_capacity(strategy: FeedStrategy, cap: usize) -> Self {
        let cap = cap.min(AGGREGATOR_MAX_SLOTS);
        Self {
            slots: Vec::with_capacity(cap),
            index: HashMap::with_capacity(cap),
            strategy,
            watermark_secs: 0,
            slots_exhausted_total: 0,
            exhausted_logged: false,
            #[cfg(test)]
            test_capacity_override: None,
        }
    }

    /// Number of instruments with allocated fold state.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// `true` when no instrument has been seen yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The event-time watermark: the max `exchange_timestamp` (IST epoch
    /// seconds) ever consumed, `0` before the first tick.
    #[must_use]
    pub fn watermark_secs(&self) -> u32 {
        self.watermark_secs
    }

    /// Lifetime count of ticks dropped because the slot table was full.
    #[must_use]
    pub fn slots_exhausted_total(&self) -> u64 {
        self.slots_exhausted_total
    }

    /// Resets the event-time watermark. Called at the day boundary alongside
    /// [`Self::force_seal_all`] so a poisoned future-dated watermark self-heals
    /// within one day instead of disabling catch-up sealing forever.
    pub fn reset_watermark(&mut self) {
        self.watermark_secs = 0;
    }

    /// Raises the event-time watermark to at least `secs`, never lowering it.
    ///
    /// # Why this exists
    ///
    /// The stale-trading-day gate compares a tick's IST day against the
    /// watermark's -- but `consume_tick` ADVANCES the watermark before that
    /// comparison, so on a fresh aggregator (watermark 0) the FIRST tick sets
    /// the very value it is then checked against and always passes. For live
    /// ticks that is harmless: the first tick of a session genuinely is the
    /// newest thing seen.
    ///
    /// It is not harmless for BOOT REPLAY. `ws_frame_spill::replay_all` is not
    /// day-scoped, and a segment left unconfirmed at yesterday's shutdown --
    /// which happens whenever the replay RAM budget defers segments, measured
    /// live as 13 deferred on 2026-08-28 -- replays the next morning into a
    /// fresh aggregator. Its first frame sets the watermark to YESTERDAY, the
    /// gate then compares yesterday against yesterday, and the frame folds
    /// into a bucket on a day that closed hours ago. Because the re-fold seals
    /// through the normal path and `candles_*` dedups on
    /// `(ts, security_id, segment, feed)` with no completeness discriminator,
    /// the PARTIAL bar rebuilt from only the deferred subset upserts over the
    /// COMPLETE bar written live the previous session.
    ///
    /// Seeding the watermark to the current trading day before replay closes
    /// that hole using machinery that already exists: a prior-day frame is
    /// then refused as `stale_trading_day`, which is a CANDLE-ONLY refusal, so
    /// its row is still written to `ticks` and only the bogus bar is skipped.
    /// Recovery keeps everything it could legitimately keep.
    ///
    /// Monotonic by construction: seeding can only ever raise the watermark,
    /// so it can never re-open a day the aggregator has already moved past,
    /// and calling it twice is harmless.
    ///
    /// # Complexity
    ///
    /// O(1) -- one compare and one store, no allocation.
    pub fn seed_watermark_at_least(&mut self, secs: u32) {
        self.watermark_secs = self.watermark_secs.max(secs);
    }

    /// Snapshot of one instrument's open bucket for one timeframe, or `None`
    /// when the instrument has no slot.
    ///
    /// # Complexity
    /// O(1) average — one hash lookup, one index.
    #[must_use]
    pub fn snapshot(
        &self,
        feed: Feed,
        security_id: u64,
        segment_code: u8,
        tf: TfIndex,
    ) -> Option<LiveCandleState> {
        let idx = *self.index.get(&(feed, security_id, segment_code))? as usize;
        self.slots.get(idx).map(|s| s.cell.snapshot(tf))
    }

    /// Read-only slot lookup. A pure query can never consume capacity.
    ///
    /// # Complexity
    /// O(1) average — one hash lookup.
    #[must_use]
    pub fn lookup(&self, feed: Feed, security_id: u64, segment_code: u8) -> Option<usize> {
        self.index
            .get(&(feed, security_id, segment_code))
            .map(|&i| i as usize)
    }

    /// The effective slot ceiling. Always [`AGGREGATOR_MAX_SLOTS`] in a
    /// non-test build; tests may shrink it via `force_capacity_for_test`.
    #[cfg(not(test))]
    #[inline]
    fn effective_capacity(&self) -> usize {
        AGGREGATOR_MAX_SLOTS
    }

    /// See the non-test twin above.
    #[cfg(test)]
    #[inline]
    fn effective_capacity(&self) -> usize {
        self.test_capacity_override.unwrap_or(AGGREGATOR_MAX_SLOTS)
    }

    /// Resolves a composite identity to its dense slot, allocating on first
    /// sight. `None` ONLY at [`AGGREGATOR_MAX_SLOTS`] — fail-closed and loud.
    ///
    /// # Complexity
    /// O(1) average — one hash lookup on every tick after an instrument's
    /// first.
    #[inline]
    fn slot_index(&mut self, key: CompositeKey) -> Option<usize> {
        if let Some(&idx) = self.index.get(&key) {
            return Some(idx as usize);
        }
        let capacity = self.effective_capacity();
        if self.slots.len() >= capacity {
            self.slots_exhausted_total = self.slots_exhausted_total.saturating_add(1);
            crate::candles::fold_counters::fold_counters()
                .slot_exhausted
                .increment(1);
            if !self.exhausted_logged {
                self.exhausted_logged = true;
                tracing::error!(
                    feed = key.0.as_str(),
                    security_id = key.1,
                    segment_code = key.2,
                    capacity,
                    "candle aggregator slot table at capacity — this instrument \
                     derives NO candles for the rest of this process; raise \
                     AGGREGATOR_MAX_SLOTS. Further occurrences coalesce to \
                     tv_aggregator_slot_exhausted_total"
                );
            }
            return None;
        }
        // GROWTH IS NOT O(1) AND IS NOT BOUNDED. Pre-size to avoid it.
        //
        // History, because this was got wrong once and the wrong version is
        // superficially convincing:
        //
        // An adversarial review flagged that `Vec` doubling memmoves the whole
        // ~5.4 KB-per-slot table inside `consume_tick` (the 8,000 -> 16,000
        // step alone moves ~43 MB, ~4-8 ms, 60-120 packets of the 66.7 us
        // budget, during which the reader stops emitting pongs and Dhan drops
        // the socket). The fix attempted here was `reserve_exact` in fixed
        // 1,000-slot chunks, documented as "bounds the worst-case pause to one
        // chunk-sized move regardless of universe size".
        //
        // That claim was FALSE, and a second audit caught it. `reserve_exact`
        // at `len == capacity` allocates a new buffer and copies ALL `len`
        // existing slots — the copy is O(n) and grows with n exactly as
        // doubling does. Worse, fixed chunks make the AGGREGATE quadratic:
        // reaching 24,000 slots in 1,000-slot steps moves ~300,000 slots
        // (~1.6 GB) versus doubling's ~24,000 (~130 MB). It was strictly worse
        // than what it replaced, while reading as an improvement.
        //
        // So: plain `Vec` growth (amortized O(1), aggregate O(n)) is retained
        // as the fallback, and the honest statement is that a single growth
        // step is O(n) and unbounded. The ONLY way to get the guarantee is to
        // not grow at all — `with_capacity` at boot, sized to the real
        // universe. `AGGREGATOR_DEFAULT_SLOTS` covers the adaptive sizer's
        // starting range so the common case never reallocates.
        //
        // GROW ONCE, TO THE CEILING (2026-09-01) — because the boot pre-size
        // is measurably BELOW the session peak and cannot be fixed from here.
        //
        // `dhan_feed_stack` pre-sizes this table from `distinct_fold_slots`
        // over the instrument sets it holds AT BOOT, which is the spot
        // universe only — `dhan_feed_stack`'s own live capture reads
        // "tracked: 865", and its comment says "865 is exactly the spot
        // universe". The ~22,000 option/future contracts
        // attach LATER in the session, and the per-minute ATM re-fit adds more
        // after that — the live measured peak is 22,996 subscribed
        // instruments (pinned in `dhan_live_universe`'s headroom test). The same file already says this in as many words about
        // the DETECTOR, which it deliberately sizes at `AGGREGATOR_MAX_SLOTS`
        // instead: "the universe grows ~26x after boot when contracts attach".
        // The fold was left on the boot count.
        //
        // So plain `Vec` doubling from ~865 to ~23,000 runs five reallocs ON
        // THE DRAIN TASK, and the last of them memmoves ~13,900 slots
        // (~75 MB at ~5.4 KB/slot) — worse than the 8k->16k case the note
        // above was written about.
        //
        // Reserving the FULL remaining ceiling on the first growth makes that
        // exactly one realloc for the process lifetime, taken at the smallest
        // n the table will ever have (the boot pre-size, ~4.7 MB), after which
        // `slots.capacity() == effective_capacity()` and the exhaustion check
        // above refuses before `push` can ever grow again.
        //
        // Reserving at CONSTRUCTION instead was considered and rejected: the
        // ceiling is 25,000 x ~5.4 KB ~= 135 MB, and `with_capacity` is the
        // constructor every unit test uses, so it would put that reservation
        // behind every test in the crate to save one 4.7 MB move in prod.
        //
        // `reserve_exact` and not `reserve`: the amount asked for IS the final
        // size, so the growth-amortisation `reserve` would add on top is pure
        // waste. The aggregate-quadratic trap in the note above came from
        // asking for FIXED CHUNKS repeatedly; asking once for the ceiling has
        // no repeat.
        if self.slots.len() == self.slots.capacity() {
            self.slots
                .reserve_exact(capacity.saturating_sub(self.slots.len()));
            self.index
                .reserve(capacity.saturating_sub(self.index.len()));
        }
        // Exact: len() < capacity <= AGGREGATOR_MAX_SLOTS (25_000) << u32::MAX.
        let idx = self.slots.len();
        self.slots.push(InstrumentSlot {
            key,
            cell: AggregatorCell::empty(),
            last_cumulative: 0,
            last_ltp: f64::NAN,
            last_tick_sign: 0,
            // Deliberately NOT a baseline — see the field doc. The first tick
            // this slot folds replaces it with a real observation.
            volume_baseline_seeded: false,
        });
        self.index
            .insert(key, u32::try_from(idx).unwrap_or(u32::MAX));
        Some(idx)
    }

    /// Folds one tick into every timeframe of one instrument, invoking
    /// `on_seal(feed, security_id, segment_code, tf, sealed_state)` for each
    /// timeframe that sealed (or amended) a bucket.
    ///
    /// `cumulative_volume_override` carries a feed's running cumulative day
    /// volume as a `u64` when it does not fit the `u32` `tick.volume` field.
    /// Passing `None` reads `tick.volume`. Routing it as an explicit `u64` is
    /// what prevents the `i64 → u32` truncation on liquid instruments.
    ///
    /// Nothing is folded when the tick is refused (insane price), out of the
    /// candle session window, or the slot table is exhausted — each is
    /// reported distinctly in the returned [`ConsumeStats`], never silently.
    ///
    /// # Complexity
    /// O(1) per tick: one hash lookup + [`TF_COUNT`](crate::candles::TF_COUNT)
    /// (a compile-time constant — read the symbol, do not quote a number: it
    /// moved 21 → 24 on 2026-08-10) scalar folds. Zero heap allocation in
    /// steady state.
    pub fn consume_tick<F>(
        &mut self,
        feed: Feed,
        tick: &ParsedTick,
        cumulative_volume_override: Option<u64>,
        mut on_seal: F,
    ) -> ConsumeStats
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        // PRICE CLASSIFICATION — corrupt and "not traded yet" are different
        // answers and were being given the same one.
        //
        // 2026-08-20. `tick_price_is_sane` requires `p > 0.0`, so an exact
        // `0.0` was refused alongside NaN and negatives, and the caller
        // discarded the whole tick — row included. On the live box that was
        // ~22,000 ticks a session: option contracts that had not traded yet.
        //
        // Zero is not a broken packet. It is the vendor saying "no last traded
        // price", which is TRUE, and the packet still carries open interest,
        // bid/ask and timestamps. The depth path in the same drain already
        // treats a `0.0` level as a documented sentinel and writes it.
        //
        // Folding a zero WOULD corrupt the candle, so the fold is still
        // skipped — exactly like `out_of_session`. The difference is that the
        // caller now keeps the row, so "did not trade" stays distinguishable
        // from "was not captured".
        let p = tick.last_traded_price;
        // The explicit comparison is the same three compares as the range
        // form, written out so the O(1) pre-commit scanner does not read the
        // range method as a Vec scan — the identical trade the session gate
        // twelve lines below already makes. Using that scanner's
        // `// O(1) EXEMPT:` hatch instead would be a small lie: this is not
        // exempt FROM O(1), it IS O(1).
        //
        // MERGE RESOLUTION 2026-08-25 — two independent hardening fixes
        // landed on the SAME three gates from opposite directions, and both
        // wanted to be first. Neither is dropped; the order below satisfies
        // both, and the reasoning is recorded because a future reader will
        // otherwise "tidy" one of them back.
        //
        // 1. TIMESTAMP BAND runs first (from main). It must precede every
        //    early return that can still produce a PERSISTED row — including
        //    the untraded-sentinel return, which is candle-only and writes
        //    the tick anyway. A packet carrying LTP = 0 AND
        //    LTT = 0xFFFFFFFF used to classify as `untraded_sentinel` and
        //    land in a year-2106 partition that retention and archival, which
        //    key on the trading day, can never reach — while every `max(ts)`
        //    and range query over `ticks` silently included it.
        //
        // 2. UNTRADED SENTINEL runs second (from this branch). It must
        //    precede the strict `p > 0.0` price gate, or a legitimately
        //    untraded instrument is miscounted as a malformed price.
        //
        // 3. REPRESENTABILITY runs last, and tests the WIDENED value.
        //
        // Hoisting the timestamp band above the price gate is a superset of
        // main's position, not a weakening: a tick that is bad in BOTH ways
        // is now attributed to `timestamp` rather than `price`, which is the
        // more actionable of the two — a bad price costs one candle, a bad
        // timestamp poisons a partition.
        // ZERO is the vendor's "no last trade time" sentinel, NOT corruption —
        // separated from the band check on 2026-08-26.
        //
        // Measured on prod that day:
        // `tv_dhan_feed_ingest_refused_total{reason="timestamp"}` = **825,783**
        // in one session — 4.0% of every tick decoded — and the drain treats a
        // timestamp refusal as a HARD refusal, so all 825,783 were discarded
        // with NO ROW AT ALL. Not a missing candle: no record that the
        // instrument was even seen.
        //
        // That is the exact mistake the 2026-08-20 fix removed for the PRICE
        // sentinel, whose reasoning applies here word for word: *"discarding
        // the ROW loses the ability to tell 'did not trade' from 'did not
        // capture', and costs the packet's open interest and bid/ask with it."*
        // An instrument that has never traded has no last price AND no last
        // trade time; the two sentinels co-occur. But the timestamp check ran
        // FIRST and hard-refused, so the price sentinel's careful
        // keep-the-row decision was silently defeated for exactly the
        // instruments it was written for.
        //
        // Safe to keep the row because the stamp is not the sentinel:
        // `tick_persistence::row_timestamp_ist_nanos` falls back to
        // `received_at` for any out-of-band value, so this row lands in
        // TODAY's partition — never a 1970 one. That fallback already existed;
        // this change simply stops discarding the row before it can be used.
        //
        // Deliberately EXACTLY zero. Any other below-floor or above-ceiling
        // value is genuine corruption (`0xFFFFFFFF` is ~year 2106) and stays a
        // hard refusal — writing it would put a row under a garbage designated
        // timestamp, which is worse than losing it.
        //
        // AND deliberately gated on a real receipt time, which is what makes
        // this compatible with the 2026-08-09/08-25 adversarial regressions
        // rather than a reversal of them. Their requirement is that a row must
        // never be written under a garbage designated timestamp — and that
        // holds here BY CONSTRUCTION, not by assertion: `row_timestamp_ist_nanos`
        // substitutes the receipt time for an out-of-band LTT, but only when
        // the caller has one (`(tick.received_at_nanos != 0).then_some(..)`),
        // falling back to the raw value otherwise. Without this guard a ts=0
        // tick that also lacked a receipt time would land in a 1970 partition
        // — the same unreachable-partition defect as year-2106, from the other
        // end of the number line.
        //
        // So the two layers now agree on exactly one condition: the fold keeps
        // the row precisely when the writer can stamp it safely. The
        // persistence side already anticipated this case — its own comment
        // calls the fallback "the fallback designated timestamp for a row whose
        // LTT is the vendor's never-traded sentinel" — but the fold refused
        // those rows before they could reach it, so that path was unreachable.
        if tick.exchange_timestamp == 0 && tick.received_at_nanos != 0 {
            crate::candles::fold_counters::fold_counters()
                .tick_untraded_timestamp
                .increment(1);
            return ConsumeStats {
                untraded_timestamp: true,
                ..ConsumeStats::default()
            };
        }
        if tick.exchange_timestamp < MIN_PLAUSIBLE_EXCHANGE_TS_SECS
            || tick.exchange_timestamp > MAX_PLAUSIBLE_EXCHANGE_TS_SECS
        {
            // MEASURED 2026-08-28 on the production box: this arm hard-refused
            // **2,008,916 ticks in one session — 2.41% of all 83,446,729
            // decoded** — and a hard refusal writes NO ROW AT ALL. Not a
            // missing candle: no record the instrument was even seen.
            //
            // THIS IS THE THIRD TIME THIS EXACT SHAPE HAS BEEN FOUND, and the
            // repetition is the point:
            //
            //   2026-08-20  price sentinel `0.0`      ~22,000 ticks/session
            //   2026-08-26  timestamp sentinel `0`     825,783 ticks/session
            //   2026-08-28  out-of-band timestamp    2,008,916 ticks/session
            //
            // Each time the reasoning was identical and is repeated verbatim
            // here: a tick whose TIMESTAMP we cannot trust is not a tick whose
            // CONTENTS are corrupt. It carries a real last-traded price, real
            // open interest, real bid/ask. Discarding the row loses the ability
            // to tell "did not trade" from "did not capture" — and each fix
            // was silently defeated by a check that ran EARLIER and refused
            // harder.
            //
            // WHY THE ROW IS SAFE, by construction rather than by assertion:
            // `tick_persistence::row_timestamp_ist_nanos` ALREADY substitutes
            // the receipt time for an out-of-band LTT, and says so in its own
            // words — "Out of band falls back to the receipt time, exactly as
            // below-floor does." The writer was built for this case. The fold
            // refused the rows before they could reach it, so that path has
            // been unreachable since it was written.
            //
            // The split is on whether a SAFE STAMP EXISTS, which is the same
            // condition the untraded-sentinel arm above uses:
            //
            //   receipt present -> CANDLE-ONLY. Folding is still refused (an
            //     out-of-band second cannot be bucketed), but the row is kept
            //     and lands in TODAY's partition under the receipt time.
            //   receipt absent  -> HARD refusal, unchanged. This is the WAL
            //     replay path, where no safe stamp exists and writing the row
            //     would put it in a 1970 or year-2106 partition that retention
            //     and archival can never reach.
            //
            // So the 2026-08-25 garbage-partition regression stays closed: the
            // fold keeps the row precisely when the writer can stamp it safely.
            if tick.received_at_nanos != 0 {
                crate::candles::fold_counters::fold_counters()
                    .tick_out_of_band_timestamp
                    .increment(1);
                return ConsumeStats {
                    out_of_band_timestamp: true,
                    ..ConsumeStats::default()
                };
            }
            crate::candles::fold_counters::fold_counters()
                .tick_refused_timestamp
                .increment(1);
            return ConsumeStats {
                refused_timestamp: true,
                ..ConsumeStats::default()
            };
        }
        // A SECOND, byte-identical copy of the timestamp-band check stood here
        // until 2026-08-26 and was PROVABLY UNREACHABLE: the copy above tests
        // the same condition and returns, so this one could never evaluate
        // true. It was a merge artifact — two hardening fixes landed on the
        // same gates from opposite directions on 2026-08-25 and both inserted
        // the check.
        //
        // Deleted rather than left in place because dead code that reads as a
        // live safety check is worse than no comment at all: it invites the
        // next reader to reason about a guard that never runs, and this file's
        // own history records exactly that class of cost. The surviving copy
        // above carries the full reasoning (it must precede every early return
        // that can still produce a PERSISTED row — including the untraded
        // sentinel, which is candle-only and writes the tick anyway).
        if p == 0.0 {
            crate::candles::fold_counters::fold_counters()
                .tick_refused_untraded_sentinel
                .increment(1);
            return ConsumeStats {
                untraded_sentinel: true,
                ..ConsumeStats::default()
            };
        }
        // Widened HERE rather than after the slot lookup (2026-08-25), because
        // the gate below tests the WIDENED value and this is the only way to
        // do that without paying a second conversion. Steady-state cost is
        // unchanged — the same single `TickPrices::from_tick` that always ran,
        // just earlier; a refused tick now pays a conversion it did not,
        // which is ~2% of arrivals against 100% for the alternative.
        let prices = TickPrices::from_tick(tick);
        // APPROVED: lint suppressed for the scanner reason directly above; no behaviour silenced.
        #[allow(clippy::manual_range_contains)]
        let raw_is_representable = p.is_finite() && p > 0.0 && p <= MAX_PLAUSIBLE_LTP;
        // The second half is the real fix, and it is deliberately stated as a
        // property of the OUTPUT rather than a new threshold on the input.
        //
        // `f32_to_f64_clean` formats through a 24-byte buffer, and Rust's f32
        // `Display` never uses scientific notation — so any value whose plain
        // decimal rendering overflows that buffer parses back as `0.0`. That
        // is a WIDER class than subnormals: `f32::MIN_POSITIVE` is a perfectly
        // normal float and still collapses (pinned by
        // `aggregator_cell::tests::test_tick_prices_subnormal_day_field_collapses_to_sentinel`),
        // so an `is_normal()` gate would have looked like a fix and let the
        // headline case straight through. Testing the widened value catches
        // every member of the class without inventing a lower price bound that
        // might refuse a legitimate five-paise option premium.
        let price_is_representable = raw_is_representable && prices.last_traded_price > 0.0;
        if !price_is_representable {
            crate::candles::fold_counters::fold_counters()
                .tick_refused_price
                .increment(1);
            return ConsumeStats {
                refused_price: true,
                ..ConsumeStats::default()
            };
        }

        // Advance the watermark AFTER the price gate but BEFORE the session
        // gate. The session-gate half is deliberate and load-bearing: a
        // post-close tick must still let the final session bar become
        // catch-up-sealable. The price-gate half is a security fix.
        //
        // ADVERSARIAL FINDING (2026-08-09, HIGH): the advance used to run
        // before EVERY gate, on a raw `u32` LTT read straight off the wire
        // with no range validation in any parser. A single malformed or
        // hostile packet carrying LTT = 0xFFFFFFFF (~year 2106) was refused
        // for FOLDING but still set the watermark ~4.29 billion. Because the
        // watermark drives `catch_up_seal_all`, the next catch-up cycle would
        // then satisfy `bucket_end <= cutoff` for essentially every open
        // bucket across all ~25,000 slots and force-seal the entire live book
        // early, with incomplete OHLCV, silently — and the watermark never
        // regresses, so it could not recover. One crafted packet, whole-book
        // corruption. That is worse than a crash, because nothing reports it.
        //
        // Two independent defences, because either alone is insufficient:
        //   1. Only a price-sane tick may advance it at all.
        //   2. The timestamp must fall in an ABSOLUTE plausible epoch range;
        //      an implausible one refuses the whole tick, so it can neither
        //      move the watermark nor fold into a far-future bucket.
        //
        // Defence 2 is absolute rather than a relative jump cap on purpose.
        // A relative cap looks appealing but is WRONG at cold start: the
        // watermark begins at 0, so the first honest tick is itself a
        // ~1.78-billion-second jump and a relative cap clamps it to garbage.
        // That mistake was written, caught by these tests, and replaced.
        //
        // 2026-08-25: defence 2 (the band check) now sits ABOVE the
        // untraded-sentinel return rather than here, because a sentinel tick
        // still produces a PERSISTED row. See the block above the `p == 0.0`
        // arm. The watermark is still advanced only after both gates, so this
        // paragraph's reasoning is unchanged.

        // 2026-08-28: the watermark advances on the FOLD clock, because its
        // consumer is the catch-up sealer, which decides which BUCKETS are
        // closed — and buckets are placed by the fold clock. A watermark on
        // one clock and a bucket grid on the other would seal by an
        // inconsistent cutoff.
        //
        // The stale-trading-day gate below reads this same watermark, and
        // reads it on the SAME clock.
        //
        // CORRECTED 2026-08-28 (found by an adversarial sweep, hours after the
        // first draft): that gate compared `tick.exchange_timestamp` against a
        // watermark that had just been advanced on the FOLD clock. The comment
        // here defended the mismatch as harmless because the two clocks agree
        // within the trusted band — true of the MAGNITUDE and irrelevant to
        // the FAILURE, because the gate does integer division into IST days.
        // A packet near midnight whose receipt crosses the day boundary
        // advances the watermark into day D+1 and is then rejected by its own
        // advance as `stale_trading_day`. Comparing like with like removes the
        // shape entirely rather than arguing it is small.
        let fold_secs = fold_clock_ist_secs(tick.exchange_timestamp, tick.received_at_nanos);

        // FUTURE TRADING DAY gate — BEFORE the advance, and that ordering is
        // the entire point.
        //
        // The advance below is `>`, so a tick from the PAST can never move the
        // watermark; the stale-day gate under it is safe for that reason. A
        // tick from the FUTURE had no such guard, and the asymmetry is not
        // theoretical: `fold_clock_ist_secs` returns the VENDOR's stamp
        // whenever receipt and exchange disagree by more than the trusted band
        // (`tf_index.rs`), and a stamp one day ahead disagrees by ~86,400 s —
        // far outside it. So one clock-fault packet stamped for tomorrow was
        // returned verbatim, advanced the watermark into day D+1, and every
        // honest tick for the REST OF THE SESSION then failed the stale-day
        // gate below: all 24 timeframes stop folding, for every instrument,
        // with no error — only a rising refusal counter.
        //
        // The receipt clock is the right reference and the only one available:
        // it is OUR machine's clock, disciplined by chrony and gated at boot by
        // BOOT-03, whereas the thing under suspicion is the vendor's stamp.
        // `SpotPriceStore` already refuses a future-dated trade for exactly
        // this reason (`spot_price_store.rs`, `FutureTradingDay`); the fold was
        // the half that had the guard on one side only. Found by the
        // 2026-09-09 time-permutation sweep.
        //
        // `received_at_nanos <= 0` is the documented "no receipt" sentinel — a
        // WAL frame written before the TVW3 format carried a receipt. With no
        // second clock there is nothing to compare against, so the gate stands
        // down rather than guessing; those frames are replay, not live.
        //
        // O(1): one compare, one divide, one compare. No allocation.
        if tick.received_at_nanos > 0 {
            let receipt_ist_secs = tick.received_at_nanos / 1_000_000_000
                + crate::candles::tf_index::IST_UTC_OFFSET_SECS;
            let fold_day = i64::from(fold_secs) / 86_400;
            let receipt_day = receipt_ist_secs / 86_400;
            if fold_day > receipt_day {
                crate::candles::fold_counters::fold_counters()
                    .tick_refused_future_trading_day
                    .increment(1);
                return ConsumeStats {
                    future_trading_day: true,
                    ..ConsumeStats::default()
                };
            }
            // STALE, judged against the RECEIPT — added 2026-09-10, and this
            // is the arm that actually catches the operator's row.
            //
            // The watermark gate below cannot: it compares a tick against the
            // HIGHEST fold clock seen so far, and on a clean boot the very
            // first tick of the session IS the stale connect snapshot. It
            // therefore sets the watermark to its own prior-day value, sails
            // through its own comparison, and every later tick then looks
            // fresh by contrast. Ordering, not arithmetic, was the hole: a
            // reference derived from the data cannot judge the first datum.
            //
            // The receipt clock has no such dependence — it is OUR machine's
            // clock, disciplined by chrony and gated at boot by BOOT-03, and
            // it is already the reference the FUTURE arm above trusts for
            // exactly this reason. Using it on both sides makes the rule
            // symmetric and order-independent: the exchange day must BE the
            // receipt day.
            //
            // Measured shape this refuses (operator, 2026-09-10, NSE_FNO
            // 66422): received today 09:15, exchange stamp 15:29 of a previous
            // session. Dhan sends LAST TRADE TIME, so every connect snapshot
            // of a dormant contract carries one — mean 5 hours stale, max 34
            // days, per the measurement recorded below.
            //
            // The watermark gate is KEPT beneath this, not replaced: it is the
            // only day guard available when there is no receipt at all (a
            // pre-TVW3 WAL frame), and it independently catches out-of-order
            // arrivals inside a single day.
            if fold_day < receipt_day {
                crate::candles::fold_counters::fold_counters()
                    .tick_refused_stale_trading_day
                    .increment(1);
                return ConsumeStats {
                    stale_trading_day: true,
                    ..ConsumeStats::default()
                };
            }
        }

        if fold_secs > self.watermark_secs {
            self.watermark_secs = fold_secs;
        }

        // STALE TRADING DAY gate — runs BEFORE the seconds-of-day gate.
        //
        // The gate below tests `exchange_timestamp % 86_400` and therefore
        // cannot see WHICH DAY a tick belongs to. Dhan sends the LAST TRADE
        // TIME, so a dormant contract snapshotted now carries a timestamp from
        // whenever it last traded — measured mean 5 hours, max 34 days. A stale
        // trade time of yesterday 15:39:41 is 56,381 seconds-of-day, inside the
        // [09:15:00, 15:40:00) window, so it passed as "in session" and opened a
        // bucket dated on a day that had already closed.
        //
        // The watermark is the right reference and needs no clock threaded in:
        // it advances ONLY on price-sane, band-checked ticks, and the advance
        // just above is `>` so a stale tick can never move it. Comparing after
        // that advance is therefore safe — an older tick leaves the watermark
        // exactly where it was.
        //
        // Ordered ABOVE the seconds-of-day gate on the same reasoning the
        // timestamp band was hoisted above the price gate: a tick that is bad
        // in both ways is attributed to the MORE actionable cause. "Out of
        // session" reads as a benign pre-open packet; "stale trading day" names
        // the thing that fabricates a bar on a closed day.
        //
        // Integer division on IST epoch seconds gives the IST day directly —
        // both `fold_secs` and the watermark are already IST (never add the
        // offset to an exchange stamp; see `data-integrity.md`), so no
        // timezone arithmetic is needed or wanted.
        if fold_secs / 86_400 < self.watermark_secs / 86_400 {
            crate::candles::fold_counters::fold_counters()
                .tick_refused_stale_trading_day
                .increment(1);
            return ConsumeStats {
                stale_trading_day: true,
                ..ConsumeStats::default()
            };
        }

        // Candle-window gate. The bucket grid is 09:15-ANCHORED
        // (`TfIndex::bucket_start` clamps an earlier timestamp to the first
        // bucket), so a pre-open tick that slipped past this gate would not
        // form a pre-open candle — it would CORRUPT the 09:15 candle.
        // 2026-08-28: gated on the FOLD clock, so the window a tick is
        // admitted to is the same window its bucket will be placed in. Gating
        // on one clock and bucketing on the other admits a tick the grid then
        // has nowhere to put, and refuses one it does.
        let secs_of_day = fold_secs % 86_400;
        // The explicit comparison is the same two integer compares as
        // `Range::contains`, written out so the O(1) pre-commit scanner does
        // not read `.contains(` as a Vec scan.
        // APPROVED: lint suppressed for the scanner reason directly above; no behaviour silenced.
        #[allow(clippy::manual_range_contains)]
        // 2026-08-28: the fold window opens at 09:00, not 09:15. The NSE
        // pre-open call auction (09:00-09:12) is where the day's opening price
        // is actually discovered, and until this gate moved, every tick of it
        // was refused here as `out_of_session` - so the equilibrium print that
        // BECOMES the 09:15 open was never folded into any candle. The comment
        // above ("a pre-open tick that slipped past this gate would CORRUPT the
        // 09:15 candle") described the OLD grid, which clamped everything
        // earlier into the 09:15 bucket; the grid now has real buckets to put
        // those ticks in, so admitting them forms pre-open candles instead of
        // corrupting anything.
        let out_of_session = secs_of_day < CANDLE_SESSION_OPEN_SECS_OF_DAY_IST
            || secs_of_day >= MARKET_CLOSE_SECS_OF_DAY_IST;
        if out_of_session {
            return ConsumeStats {
                out_of_session: true,
                ..ConsumeStats::default()
            };
        }

        let key = (feed, tick.security_id, tick.exchange_segment_code);
        let Some(idx) = self.slot_index(key) else {
            return ConsumeStats {
                slot_exhausted: true,
                ..ConsumeStats::default()
            };
        };
        let strategy = self.strategy;
        let Some(slot) = self.slots.get_mut(idx) else {
            // Unreachable: slot_index either returned an existing index or
            // just pushed one. Fail closed rather than index-panic.
            return ConsumeStats {
                slot_exhausted: true,
                ..ConsumeStats::default()
            };
        };

        let cumulative_volume =
            cumulative_volume_override.unwrap_or_else(|| u64::from(tick.volume));

        // SEED, do not assume zero. A slot allocated mid-session has never
        // seen this instrument, so the volume it traded before we arrived is
        // unattributable to any bucket we own. Anchoring the baseline on this
        // first observation makes the first bar report 0; anchoring it on `0`
        // made the first bar report the whole day.
        // Recorded HERE, on the accepted-tick path, so it can never hold a price
        // the fold itself refused. Every earlier return in this function is a
        // refusal.
        // Captured BEFORE the overwrite below — this is the tick rule's whole
        // input, and it is available at exactly one instant in this function.
        let prev_ltp = slot.last_ltp;
        // STALE-PACKET GATE (2026-09-11). A packet whose day-cumulative is
        // BELOW the previous accepted one is stale — a cumulative counter
        // cannot legitimately go down within a day. Its delta is already
        // neutralised downstream (`saturating_sub` yields 0), but until today
        // its PRICE was still adopted as `last_ltp` on the line below, and
        // that price is the tick rule's entire input for the NEXT packet.
        //
        // The failure it caused: 102 -> [stale 105] -> 103 classified the 103
        // as a DOWNTICK and latched `carry = -1`, inverting that tick's sign
        // and every flat tick after it until the next real move. MEASURED on
        // the live box 2026-09-11: security 68407 took 5 cumulative
        // regressions before 09:40 IST, one of them (09:15:07 -> 09:15:08,
        // 40,820 -> 40,690) carrying a price that moved the opposite way.
        //
        // A stale packet is refused as an INPUT to the rule, not merely
        // discounted in the output.
        let is_stale_packet =
            slot.volume_baseline_seeded && cumulative_volume < slot.last_cumulative;
        if !is_stale_packet {
            slot.last_ltp = prices.last_traded_price;
        }
        if !slot.volume_baseline_seeded {
            slot.volume_baseline_seeded = true;
            slot.last_cumulative = cumulative_volume;
            crate::candles::fold_counters::fold_counters()
                .slot_volume_baseline_seeded
                .increment(1);
        }
        let baseline = slot.last_cumulative;
        let mut stats = ConsumeStats::default();

        // `prices` was widened above the price gate — ONCE per tick, not once
        // per timeframe. The three source fields are identical across all
        // `TF_COUNT` timeframes, and `f32_to_f64_clean` costs a decimal
        // round-trip (~50 ns) rather than a cast, so folding it inside this
        // loop would multiply one tick's conversion cost by `TF_COUNT × 3`
        // for no added information.

        // Same reasoning, and a stronger reason besides: this one is a
        // comparison against the PREVIOUS PACKET, so it is only meaningful
        // once per tick. Running it inside the loop would compare a packet
        // against itself for 23 of the 24 timeframes and silently destroy the
        // delta. It must stay above the loop.
        let extremes = slot.cell.observe_session_extremes(tick, fold_secs);

        // TICK-RULE CLASSIFICATION — derived ONCE per tick, for the same
        // reason `extremes` two lines up is: it is a comparison against the
        // PREVIOUS PACKET, so running it inside the timeframe loop would
        // compare a packet against itself for 23 of the 24 frames and destroy
        // the answer.
        //
        // The delta is `cumulative - baseline`, where `baseline` is the
        // previous ACCEPTED tick's day-cumulative for this instrument. That is
        // the same quantity the fold uses for `volume` at a bucket rollover, so
        // the net and the gross count exactly the same trades — which is what
        // makes `net_volume().abs() <= volume` hold rather than merely be
        // hoped for.
        let signed_tick_volume = if is_stale_packet {
            // A stale packet traded nothing new (its delta off the monotonic
            // baseline is 0) and reveals no direction. `Some(0)` — genuinely
            // nothing — and deliberately NOT `None`, which would poison an
            // otherwise fully-classified bar over a packet that added no
            // volume for the bar to be ignorant of.
            Some(0)
        } else {
            classify_tick_volume(
                prev_ltp,
                prices.last_traded_price,
                cumulative_volume.saturating_sub(baseline),
                &mut slot.last_tick_sign,
            )
        };

        for tf in TfIndex::ALL {
            match slot.cell.consume_tick_with_extremes(
                tf,
                tick,
                prices,
                baseline,
                strategy,
                cumulative_volume,
                extremes,
                // Passed THROUGH, not re-wrapped. Until 2026-09-11 this read
                // `Some(signed_tick_volume)`, which made every live bar
                // "classified" by construction and left the fold's own `None`
                // arm dead code.
                signed_tick_volume,
                // Derived ONCE at :748, above this loop — the same hoisting
                // contract as `prices` and `cumulative_volume`. Passing it
                // down rather than recomputing it saves 48 conversions per
                // tick (24 timeframes × the bucket site and one fold arm).
                fold_secs,
            ) {
                ConsumeOutcome::Updated => {}
                ConsumeOutcome::Sealed { sealed_state } => {
                    stats.sealed_count = stats.sealed_count.saturating_add(1);
                    on_seal(key.0, key.1, key.2, tf, sealed_state);
                }
                ConsumeOutcome::AmendedLate { amended_state } => {
                    stats.amended_count = stats.amended_count.saturating_add(1);
                    on_seal(key.0, key.1, key.2, tf, amended_state);
                }
                ConsumeOutcome::DiscardLate => {
                    stats.late_count = stats.late_count.saturating_add(1);
                    // A tick discarded here is DATA LOSS for this timeframe:
                    // the bar it should have contributed to is now missing a
                    // trade. `late_count` has carried that fact since the
                    // aggregator was written and had ZERO production readers
                    // until 2026-08-26 — every consumer reads `sealed_count`
                    // and `amended_count`, and `IngestOutcome::Folded` carries
                    // only those two, so the drop was computed on every tick
                    // and reached nothing.
                    //
                    // Pre-resolved handle, per this module's whole reason for
                    // existing: this arm sits inside the 24-timeframe loop on
                    // the per-tick path, which is the one place a bare
                    // `counter!` macro must never appear.
                    crate::candles::fold_counters::fold_counters()
                        .tick_discarded_late
                        .increment(1);
                }
            }
        }

        // Store the SAME resolved cumulative the cells folded, so the next
        // bucket's baseline matches what was just written — never the
        // truncated `u32` when a `u64` override was supplied.
        //
        // MERGE RESOLUTION 2026-08-25 — both branches found this same defect
        // independently and fixed it the same way. main's version is kept
        // because it is a strict superset: identical monotonic advance, plus
        // a counter that makes the correction VISIBLE. This branch's version
        // (`slot.last_cumulative.max(cumulative_volume)`) is behaviourally
        // equal and silent, and a silent correction is the weaker of two
        // otherwise-identical fixes.
        //
        // ADVANCE ONLY. This was an UNCONDITIONAL assignment and that was a
        // live data-corruption defect, measured 2026-08-24: the same trading
        // day tiled five ways did not sum to one volume total (1s
        // 40,397,638,853 vs 1d 4,372,993,982 — the intraday frames were ~9.2x
        // the day bar, and 6,088 instruments disagreed with their own 1m sum).
        //
        // Mechanism: `tick.volume` is DAY-CUMULATIVE. `FeedStrategy::DEFAULT`
        // is `Refold`, so late ticks are routine (10.0% of live ticks arrive
        // >1h behind receive time) and every timeframe can return
        // `DiscardLate` — yet the store below still ran, writing that late
        // tick's SMALLER cumulative. The next bucket then opened on a baseline
        // BELOW the volume already traded, and `cumulative - baseline`
        // double-counted the difference. The regression is silently
        // self-amplifying because nothing downstream can see a baseline.
        //
        // Refusing the regression is the only correct answer: a cumulative
        // counter cannot legitimately go down within a day, so a smaller value
        // is stale, never news. It is counted so the correction is visible.
        if cumulative_volume > slot.last_cumulative {
            slot.last_cumulative = cumulative_volume;
        } else if cumulative_volume < slot.last_cumulative {
            // TWO different events reach this arm and they need OPPOSITE
            // remedies. Until 2026-09-11 both were treated as "stale packet",
            // which is correct for one of them and catastrophic for the other.
            //
            //   STALE PACKET — a small backwards step. Refuse it: a cumulative
            //   counter cannot legitimately go down, so a smaller value is
            //   stale, never news.
            //
            //   COUNTER RESTART — an ENORMOUS backwards step. Two causes:
            //     * `ParsedTick.volume` is `u32`, so the vendor's day-cumulative
            //       WRAPS past 4,294,967,295 back to a small number;
            //     * a day rollover restarts the counter near zero in a process
            //       that outlived `force_seal_all`.
            //   Refusing this one freezes `last_cumulative` at the high-water
            //   mark FOREVER. Every later `saturating_sub` then yields 0, so
            //   every bar reports `volume 0` and `net_volume` NULL for the rest
            //   of the session — silently, while `tick_count` keeps rising.
            //   The guard that prevents double-counting becomes the thing that
            //   kills the instrument.
            //
            // MEASURED 2026-09-11, 25 minutes into the session: the busiest
            // instrument on the box (81245, NSE_FNO) had already reached a
            // cumulative volume of 250,519,875 — the same order of magnitude as
            // the `u32` ceiling once extrapolated across a full session. This
            // is a reachable event, not a theoretical one.
            //
            // The two are separated by MAGNITUDE, which is the only signal
            // available: no real stale packet is behind by half the `u32`
            // range, and every wrap and every rollover is.
            let backwards_by = slot.last_cumulative - cumulative_volume;
            if backwards_by >= CUMULATIVE_RESTART_DROP_FLOOR {
                // RE-ANCHOR on the new value rather than refusing it. This
                // costs exactly one tick's delta (the wrapping tick's own
                // volume is unattributable — its true delta spans the wrap and
                // cannot be recovered from a truncated counter) and keeps the
                // instrument alive for the remainder of the session.
                slot.last_cumulative = cumulative_volume;
                // Re-anchoring the SLOT baseline alone is NOT sufficient: every
                // bucket that is already OPEN still holds a
                // `bucket_start_cumulative` from before the restart, so its
                // volume would freeze for the rest of the bucket (up to 59
                // minutes on M60) while `tick_count` kept rising. The cell
                // re-bases those in the same breath, preserving what each has
                // already counted.
                slot.cell.rebase_open_buckets(cumulative_volume);
                crate::candles::fold_counters::fold_counters()
                    .cumulative_reanchored
                    .increment(1);
            } else {
                crate::candles::fold_counters::fold_counters()
                    .cumulative_regression
                    .increment(1);
            }
        }
        stats
    }

    /// [`Self::consume_tick`] wired straight into the existing
    /// [`SealRing`]: every sealed / amended bar is wrapped in a
    /// [`BufferedSeal`] and pushed. When the ring is at capacity the EVICTED
    /// (oldest) seal is handed to `on_evicted` so the caller can route it to
    /// disk spill / DLQ — the ring's contract; it is never dropped here.
    ///
    /// # Complexity
    /// O(1) per tick — the ring push is `VecDeque::push_back`.
    pub fn consume_tick_into_ring<F>(
        &mut self,
        feed: Feed,
        tick: &ParsedTick,
        cumulative_volume_override: Option<u64>,
        ring: &mut SealRing,
        mut on_evicted: F,
    ) -> ConsumeStats
    where
        F: FnMut(BufferedSeal),
    {
        self.consume_tick(
            feed,
            tick,
            cumulative_volume_override,
            |feed, security_id, segment_code, tf, state| {
                let seal = BufferedSeal::new(security_id, segment_code, tf, state, feed);
                if let BufferOutcome::DroppedOldest(evicted) = ring.try_buffer(seal) {
                    on_evicted(evicted);
                }
            },
        )
    }

    /// Force-seals every timeframe of every instrument — the day-boundary
    /// flush. Emits ONLY buckets that were actually opened: an instrument
    /// that never ticked, and a timeframe that never opened, emit NOTHING.
    ///
    /// Returns the number of bars emitted.
    ///
    /// # Complexity
    /// O(N × [`TF_COUNT`]) where N is the number of allocated slots. COLD
    /// path — once per day boundary, never per tick.
    ///
    /// Written as the CONSTANT, not as a literal. This line said `21` while
    /// `TF_COUNT` was 24 — understating the real cost by ~14% — because a
    /// number copied into a doc comment has no way to stay true when the
    /// constant beside it moves. Cite the symbol; let it move on its own.
    pub fn force_seal_all<F>(&mut self, mut on_seal: F) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        let mut emitted = 0_usize;
        for slot in &mut self.slots {
            let (feed, sid, seg) = slot.key;
            // DAY-BOUNDARY RESET — required by the monotonic baseline in
            // `consume_tick`, and wrong to omit. The vendor's cumulative
            // volume restarts at ~0 each session; without this the
            // advance-only rule would read tomorrow's honest small cumulative
            // as a regression, refuse it all day, and publish every bar at
            // volume 0. This is the ONE place a regression is legitimate, so
            // it is the one place the baseline drops — and it drops to
            // UNSEEDED, not to a fabricated `0` baseline.
            slot.last_cumulative = 0;
            slot.volume_baseline_seeded = false;
            // The tick-rule carry resets with the baseline, and for the same
            // reason: a direction learned from yesterday's last print is not
            // evidence about today's first. Carrying it across would attribute
            // the whole of the new session's opening zero-tick volume to
            // whichever side happened to move the price at yesterday's close.
            //
            // `last_ltp` is deliberately LEFT ALONE — it is a published
            // accessor (`MultiTfAggregator::last_ltp`) whose contract is "the
            // last accepted price", and blanking it here would make that
            // reader answer `None` after a force-seal. The carry reset is
            // enough: with `last_tick_sign` at 0, the first zero tick of the
            // new day is refused as unclassified rather than mis-signed.
            slot.last_tick_sign = 0;
            for tf in TfIndex::ALL {
                if let Some(state) = slot.cell.force_seal(tf) {
                    emitted = emitted.saturating_add(1);
                    on_seal(feed, sid, seg, tf, state);
                }
            }
            // MERGE RESOLUTION 2026-08-25 — both branches found this same
            // day-boundary defect and reset the baseline; main's version is
            // kept and this branch's duplicate assignment is removed.
            //
            // The difference was not cosmetic. This branch reset to a
            // baseline of `0`, so day two's first bar owned everything traded
            // since the open. main resets to UNSEEDED, so day two's first
            // PACKET re-seeds and the first bar owns only what traded after
            // it. main's is kept because it is the conservative direction: it
            // can under-attribute the sub-second window before our first
            // packet of the day, but it can never over-attribute volume that
            // was not ours — and the same seeding rule already governs a slot
            // allocated mid-session, so one rule now covers both arrivals.
            //
            // The original reasoning, still true: `force_seal` resets the
            // CELL's day state, but `last_cumulative` lives on the SLOT and
            // nothing touched it, so a process spanning midnight opened day
            // two with YESTERDAY's final cumulative as baseline and
            // `saturating_sub` floored every bucket to 0. D1 is the worst
            // case — one bucket per day, so the whole daily bar read zero.
            // Masked today only because the box stops at 17:30 and restarts
            // with `last_cumulative: 0`; a schedule change would have made it
            // live, silently, with no counter moving.
        }
        emitted
    }

    /// Watermark-aware intraday catch-up seal across every instrument: seals
    /// only the buckets whose exclusive end is at or before `cutoff_secs`.
    ///
    /// This is what closes a bar for an illiquid instrument that stops
    /// ticking mid-session — without it that bar would wait for the next tick
    /// or the day boundary. It never seals a bucket whose final ticks are
    /// still plausibly in flight, because the caller derives `cutoff_secs`
    /// from [`Self::watermark_secs`] minus an allowed-lateness margin.
    ///
    /// Returns the number of bars emitted.
    ///
    /// # Complexity
    /// O(N × [`TF_COUNT`]). Driven at a multi-second cadence — but NOT on a
    /// background task: the caller drives this from the frame drain's own
    /// `tokio::select!`, so a sweep is a periodic PAUSE of the drain, not
    /// work that happens beside it. MEASURED at the 25,000-slot x
    /// [`TF_COUNT`] ceiling by `catch_up_seal_all_sweep_cost_at_the_authorized_ceiling`
    /// in this file: 9.67 ms, 16.1 ns per cell (2026-08-21, release, x86 dev
    /// container), a 0.2% duty cycle at the 5 s cadence — recorded in
    /// CLAUDE.md's O(1) table. (This line read "UNMEASURED" until 2026-09-08,
    /// three weeks after the harness landed.
    /// The literal `21` this line once carried was stale; cite the
    /// constant so it cannot go stale again.)
    pub fn catch_up_seal_all<F>(&mut self, cutoff_secs: u32, mut on_seal: F) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        let mut emitted = 0_usize;
        for slot in &mut self.slots {
            let (feed, sid, seg) = slot.key;
            for tf in TfIndex::ALL {
                if let Some(state) = slot.cell.catch_up_seal(tf, cutoff_secs) {
                    emitted = emitted.saturating_add(1);
                    on_seal(feed, sid, seg, tf, state);
                }
            }
        }
        emitted
    }
}

#[cfg(test)]
impl MultiTfAggregator {
    /// Test-only: shrink the effective slot ceiling so the fail-closed
    /// exhaustion path can be exercised without allocating
    /// [`AGGREGATOR_MAX_SLOTS`] cells (~135 MB).
    fn force_capacity_for_test(&mut self, cap: usize) {
        self.test_capacity_override = Some(cap);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

impl MultiTfAggregator {
    /// The last accepted last-traded price for one instrument, in rupees.
    ///
    /// `None` when the instrument has no slot, or has a slot that has not yet
    /// folded an accepted tick. Both are the same answer to the caller -- there
    /// is no price to reason about -- and neither is an error: a slot is created
    /// on first sight and the authorized universe is far larger than the set
    /// that trades in any given second.
    ///
    /// # Why this lives here rather than in a map of its own
    ///
    /// The fold already keeps one slot per instrument, bounded by
    /// `AGGREGATOR_MAX_SLOTS` and fail-closed at it. A second per-instrument map
    /// would be another structure bounded only by caller convention -- the
    /// unbounded-growth shape this repository's O(1) table has recorded and
    /// removed repeatedly -- and it could disagree with the fold about which
    /// price was last ACCEPTED, which is the only price worth reporting.
    ///
    /// # Complexity
    ///
    /// O(1) average: one hash probe and one indexed read. Takes `&self`, so it
    /// cannot allocate a slot as a side effect of being asked -- a reader must
    /// never grow the table it is reading.
    #[must_use]
    pub fn last_ltp(&self, feed: Feed, security_id: u64, segment_code: u8) -> Option<f64> {
        let idx = *self.index.get(&(feed, security_id, segment_code))?;
        let slot = self.slots.get(usize::try_from(idx).ok()?)?;
        // NAN is the "no accepted tick yet" sentinel, and it must not escape as
        // a price: every downstream gain calculation refuses a non-finite, so
        // returning it would turn "unknown" into "refused" one layer away from
        // where the reason is known.
        slot.last_ltp.is_finite().then_some(slot.last_ltp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candles::{LatePolicy, TF_COUNT};

    /// An exact multiple of 86_400, so `DAY + 33_300` is 09:15:00 IST.
    pub(super) const DAY: u32 = 1_779_321_600;
    /// 09:15:00 IST of [`DAY`].
    const OPEN: u32 = DAY + 33_300;
    /// 2026-08-28: the CANDLE session opens at 09:00, fifteen minutes before
    /// the market. Session-gate tests must use THIS, not `OPEN` - a tick at
    /// 09:14 is now legitimately in-session (it is a pre-open auction tick),
    /// so asserting it is gated would be asserting the bug this change fixed.
    pub(super) const CANDLE_OPEN: u32 = DAY + 32_400;

    pub(super) const SEG_IDX: u8 = 0;
    const SEG_EQ: u8 = 1;

    pub(super) fn tick(sid: u64, seg: u8, ts: u32, price: f32, cum: u32) -> ParsedTick {
        ParsedTick {
            security_id: sid,
            exchange_segment_code: seg,
            last_traded_price: price,
            exchange_timestamp: ts,
            volume: cum,
            ..ParsedTick::default()
        }
    }

    // -- tick-rule net volume (2026-09-10) ----------------------------------
    //
    // The classification lives HERE, not in the cell, because it needs the
    // previous TICK and a cell only has bars. These tests drive the real
    // `consume_tick` path end to end.

    /// THE FIX, in one test: flow and close can DISAGREE, and the old
    /// implementation reported the close.
    ///
    /// A bar that sells 1,000 into the bid and buys 400 on the offer, and
    /// happens to close one tick above where it opened, has net flow of -600.
    /// The pre-2026-09-10 code signed the whole 1,400 by the close direction
    /// and reported +1,400 — wrong magnitude AND wrong sign.
    #[test]
    fn a_bar_that_closes_up_on_selling_flow_reports_negative_net_volume() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        // Establish a price, then a baseline tick so the first delta is real.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base, 100.0, 1_000),
            None,
            sink,
        );
        // DOWNTICK carrying 1,000: sell-initiated.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 1, 99.0, 2_000),
            None,
            sink,
        );
        // UPTICK carrying 400: buy-initiated. Closes ABOVE the first price.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 2, 101.0, 2_400),
            None,
            sink,
        );

        let bar = agg
            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
            .expect("bucket is open");
        assert_eq!(bar.volume, 1_400, "gross is every lot that traded");
        assert!(bar.close > 100.0, "the bar closed UP — that is the trap");
        assert_eq!(
            bar.net_volume(),
            Some(-600),
            "1,000 sold minus 400 bought. The old code signed the whole 1,400 \
             by the close direction and answered +1,400 — wrong sign, wrong size"
        );
    }

    /// The zero-tick carry: unchanged-price ticks keep the last direction.
    ///
    /// This is the case that decides whether the column is useful at all —
    /// unchanged-price ticks are the majority on a liquid contract, so
    /// discarding them would under-report a bar's flow by most of its volume.
    #[test]
    fn an_unchanged_price_carries_the_previous_direction() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base, 100.0, 1_000),
            None,
            sink,
        );
        // DOWNTICK 500 — sets the carry to sell.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 1, 99.0, 1_500),
            None,
            sink,
        );
        // FLAT 300 — same price, so it inherits the sell direction.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 2, 99.0, 1_800),
            None,
            sink,
        );

        assert_eq!(
            agg.snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
                .expect("open")
                .net_volume(),
            Some(-800),
            "500 on the downtick plus 300 carried at the same price"
        );
    }

    /// A cumulative RESTART re-anchors the baseline instead of freezing it.
    ///
    /// The vendor's day-cumulative can restart near zero (a counter wrap, or a
    /// session restart on the exchange side). Without the re-anchor the
    /// advance-only rule reads every later tick as a regression, so the
    /// instrument reports `volume 0` for the rest of the session while its
    /// `tick_count` keeps rising — wrong, and silent.
    ///
    /// The three outcomes are deliberately far apart so this test cannot pass
    /// by accident: 51,000 is the re-anchored answer, 1,000 is the frozen one.
    #[test]
    fn a_cumulative_restart_re_anchors_the_baseline_instead_of_freezing() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        // Seed the baseline near the top of the u32 range.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(91, SEG_IDX, base, 100.0, 4_000_000_000),
            None,
            sink,
        );
        // A normal uptick of 1,000.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(91, SEG_IDX, base + 1, 101.0, 4_000_001_000),
            None,
            sink,
        );
        // THE RESTART: a backwards step of ~4e9, far past the floor. This tick
        // itself adds nothing (it traded nothing new), but it must MOVE the
        // baseline.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(91, SEG_IDX, base + 2, 102.0, 100_000),
            None,
            sink,
        );
        // The proof tick: 50,000 above the RESTARTED counter.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(91, SEG_IDX, base + 3, 103.0, 150_000),
            None,
            sink,
        );

        let open = agg
            .snapshot(Feed::Dhan, 91, SEG_IDX, TfIndex::M1)
            .expect("open bucket");
        assert_eq!(
            open.net_volume(),
            Some(51_000),
            "1,000 before the restart plus 50,000 after it; a frozen baseline \
             would report 1,000 and lose the rest of the session"
        );
        assert_eq!(
            open.volume, 51_000,
            "gross must count exactly the same trades as net"
        );
    }

    /// A small backwards step is a STALE PACKET, not a restart: it must not
    /// re-anchor, and it must not poison the next tick's direction.
    ///
    /// MEASURED on the live box 2026-09-11: security 68407 took five
    /// cumulative regressions before 09:40 IST — 09:15:07 -> 09:15:08 went
    /// 40,820 -> 40,690 while the price moved the OTHER way. An out-of-order
    /// snapshot is an older view, so it is refused as an INPUT to the tick
    /// rule rather than merely discounted in the output.
    ///
    /// Three outcomes discriminate all three branches at once:
    ///   11,000 — correct
    ///    9,000 — the stale price was allowed to set the direction
    ///   12,000 — the stale packet wrongly re-anchored the baseline
    #[test]
    fn a_stale_packet_neither_re_anchors_nor_sets_the_next_ticks_direction() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(92, SEG_IDX, base, 100.0, 10_000),
            None,
            sink,
        );
        // UPTICK 10,000 — the real flow, and it sets the last accepted price
        // to 101.0.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(92, SEG_IDX, base + 1, 101.0, 20_000),
            None,
            sink,
        );
        // THE STALE PACKET: cumulative goes BACKWARDS by 1,000 (far under the
        // restart floor) and carries a HIGHER price. If that price were
        // allowed through, the next tick would read as a downtick.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(92, SEG_IDX, base + 2, 103.0, 19_000),
            None,
            sink,
        );
        // The proof tick: 102.0 is ABOVE the last genuinely accepted price
        // (101.0) and BELOW the stale one (103.0).
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(92, SEG_IDX, base + 3, 102.0, 21_000),
            None,
            sink,
        );

        let open = agg
            .snapshot(Feed::Dhan, 92, SEG_IDX, TfIndex::M1)
            .expect("open bucket");
        assert_eq!(
            open.net_volume(),
            Some(11_000),
            "10,000 up, nothing from the stale packet, 1,000 up off the \
             high-water baseline"
        );
    }

    /// A stale packet leaves the bar CLASSIFIED, and that distinction is the
    /// whole point of `Some(0)` rather than `None`.
    ///
    /// It traded nothing new, so there is no flow for the bar to be ignorant
    /// of. Returning `None` would NULL an otherwise fully-classified bar over
    /// a packet that added no volume — a duplicate or an out-of-order snapshot
    /// would silently erase a good reading.
    #[test]
    fn a_stale_packet_contributes_zero_and_never_unclassifies_the_bar() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(93, SEG_IDX, base, 100.0, 5_000),
            None,
            sink,
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(93, SEG_IDX, base + 1, 101.0, 6_000),
            None,
            sink,
        );
        let before = agg
            .snapshot(Feed::Dhan, 93, SEG_IDX, TfIndex::M1)
            .expect("open bucket")
            .net_volume();
        assert_eq!(before, Some(1_000), "a clean uptick of 1,000");

        // The out-of-order snapshot.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(93, SEG_IDX, base + 2, 99.0, 5_500),
            None,
            sink,
        );

        let after = agg
            .snapshot(Feed::Dhan, 93, SEG_IDX, TfIndex::M1)
            .expect("open bucket");
        assert_eq!(
            after.net_volume(),
            Some(1_000),
            "unchanged — the stale packet added no volume and no direction"
        );
        assert_eq!(after.volume, 1_000, "and it added nothing to gross either");
    }
    /// Before any direction has revealed itself, a flat tick is UNCLASSIFIED.
    ///
    /// Guessing here would propagate: the carry would then sign every
    /// subsequent flat tick on a direction nobody observed.
    #[test]
    fn a_flat_tick_with_no_carry_yet_makes_the_bar_unclassified_not_balanced() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base, 100.0, 1_000),
            None,
            sink,
        );
        // Same price, real volume, and no direction has ever been observed.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 1, 100.0, 1_600),
            None,
            sink,
        );

        let bar = agg
            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
            .expect("open");
        assert_eq!(bar.volume, 600, "the gross still counts it");
        // ⚠ 2026-09-11: this asserted `Some(0)` with the rationale "traded but
        // unclassifiable nets to zero — a real reading". That rationale was the
        // defect written down as a test. `Some(0)` on this column means "buy
        // and sell flow were measured and were equal"; here nothing was
        // measured at all — the price never moved and no direction has ever
        // been observed for this instrument, so the 600 units have no known
        // side. Publishing `0` made an unmeasured bar indistinguishable from a
        // genuinely balanced one.
        assert_eq!(
            bar.net_volume(),
            None,
            "the bar traded 600 units whose side is unknown — that is NULL, \
             never a measured zero"
        );
    }

    /// THE INVARIANT, driven through the real fold rather than asserted on a
    /// hand-built state: the net can never exceed the gross.
    #[test]
    fn net_volume_never_exceeds_gross_volume_through_the_real_fold() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        let mut cum = 1_000u32;
        let mut price = 100.0f32;
        let _ = agg.consume_tick(Feed::Dhan, &tick(77, SEG_IDX, base, price, cum), None, sink);
        for i in 1..40u32 {
            // Alternating direction with irregular sizes, so the net wanders.
            price += if i % 3 == 0 { -0.5 } else { 0.25 };
            cum += 10 * i;
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, base + i, price, cum),
                None,
                sink,
            );
        }

        for tf in TfIndex::ALL {
            let Some(bar) = agg.snapshot(Feed::Dhan, 77, SEG_IDX, tf) else {
                continue;
            };
            if let Some(net) = bar.net_volume() {
                assert!(
                    net.unsigned_abs() <= bar.volume,
                    "{tf:?}: net {net} exceeds gross {} — a row saying a bar \
                     traded N lots of which more than N were buys",
                    bar.volume
                );
            }
        }
    }

    /// **Conservation across timeframes** — the property an operator checks by
    /// eye: if `candles_1m` says a minute was +800, the `candles_1s` rows
    /// underneath it must add up to +800.
    ///
    /// It holds BY CONSTRUCTION — `classify_tick_volume` runs once per tick and
    /// the same signed number is added into every open bar, so the frames are
    /// different WINDOWS over one classification, never different answers. But
    /// "by construction" is a claim, and until this test nothing pinned it:
    /// every other `net_volume` test asserts a property of ONE bar.
    ///
    /// This is also what makes a sub-minute frame legitimately look SPARSE
    /// without being wrong. A second in which no tick arrived opens no bucket
    /// at all — that is an absent ROW, not a missing measurement — and a
    /// second whose only tick carried no new volume reports NULL. Neither
    /// contributes to the sum, so the totals still agree.
    #[test]
    fn every_sub_minute_frame_sums_to_the_same_minute_net_volume() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut sealed: Vec<(TfIndex, i64)> = Vec::new();
        // `OPEN` is minute-aligned, so [base, base+59] is exactly ONE M1 bucket.
        let base = OPEN;

        let mut cum = 1_000u32;
        let mut price = 100.0f32;
        for i in 0..60u32 {
            // Irregular sizes and a flipping direction, including flat ticks
            // that ride the carry — so the net is not trivially +gross and the
            // sum has to do real work.
            price += match i % 4 {
                0 => 0.25,
                1 => -0.50,
                2 => 0.0,
                _ => 0.75,
            };
            cum += 10 + i * 3;
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, base + i, price, cum),
                None,
                |_: Feed, _: u64, _: u8, tf: TfIndex, st: LiveCandleState| {
                    if let Some(net) = st.net_volume() {
                        sealed.push((tf, net));
                    }
                },
            );
        }

        let minute = agg
            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
            .and_then(|b| b.net_volume())
            .expect("the minute bar traded and was classified");

        // ANTI-VACUITY, checked before the loop that does the real asserting.
        // Both of these have failed silently in this repository's history: a
        // filter that excludes every frame makes the loop below assert nothing
        // and the test pass green, and a net that equals the gross would mean
        // the flat and down ticks above never exercised the carry — the sum
        // would then be trivially conserved because every term has one sign.
        let gross = agg
            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
            .map(|b| b.volume)
            .expect("the minute bar exists");
        assert!(
            minute.unsigned_abs() < gross,
            "the fixture never produced offsetting flow: net {minute} vs gross \
             {gross} — conservation would hold trivially, so this test would \
             prove nothing"
        );

        let mut frames_checked = 0usize;
        for tf in TfIndex::ALL {
            // Keep only frames whose buckets TILE this minute: the first starts
            // exactly on the minute and the last still starts inside it. That
            // admits every sub-minute frame and excludes D1, whose bucket began
            // at midnight and holds volume this minute never saw.
            if tf.bucket_start(base) != base || tf.bucket_start(base + 59) >= base + 60 {
                continue;
            }
            frames_checked += 1;
            let closed: i64 = sealed
                .iter()
                .filter(|(t, _)| *t == tf)
                .map(|(_, net)| *net)
                .sum();
            // The frame's LAST bucket has not sealed yet, so it is still open
            // and has to be read from the snapshot or the sum is short by it.
            let still_open = agg
                .snapshot(Feed::Dhan, 77, SEG_IDX, tf)
                .and_then(|b| b.net_volume())
                .unwrap_or(0);
            assert_eq!(
                closed + still_open,
                minute,
                "{tf:?}: its bars sum to {} but the minute they tile reports \
                 {minute} — the frames disagree about the same trades, which \
                 is the one thing a single per-tick classification is supposed \
                 to make impossible",
                closed + still_open
            );
        }

        // The second-scale family alone is 19 frames; if the tiling filter ever
        // stops admitting them, this test goes quiet rather than red.
        assert!(
            frames_checked >= 10,
            "only {frames_checked} frames were compared — the tiling filter is \
             excluding frames it should admit, so this test is no longer \
             checking what it claims"
        );
    }

    /// The first tick for an instrument has no previous price to compare to.
    ///
    /// `last_ltp` is `NaN` until the first accepted tick, and `NaN` fails BOTH
    /// `>` and `<` — so an unguarded comparison would land on the zero-tick arm
    /// and attribute the whole first delta to a carry. Refused instead.
    #[test]
    fn the_first_tick_of_an_instrument_is_never_classified() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base, 100.0, 5_000),
            None,
            sink,
        );
        assert_eq!(
            agg.snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
                .expect("open")
                .net_volume_signed,
            0,
            "the very first tick seeds the baseline; there is no previous \
             price, so its delta is real but unclassifiable"
        );
    }

    /// The classifier itself, exhaustively — the refusals are the interesting
    /// half and each one is a hazard that has bitten this repository before.
    #[test]
    fn classify_tick_volume_refuses_every_input_it_cannot_read() {
        let mut carry = 0i8;

        // NOTHING TRADED is `Some(0)`, not `None`. This is the distinction the
        // 2026-09-11 change exists to make: no volume moved, so the bar is not
        // ignorant of anything and must stay fully classified. A duplicate
        // packet — which this feed delivers routinely — lands here, and
        // returning `None` would let one duplicate NULL an otherwise complete
        // bar.
        assert_eq!(classify_tick_volume(100.0, 101.0, 0, &mut carry), Some(0));
        assert_eq!(carry, 0, "a refused tick must not move the carry");

        // No previous price (the first tick): real delta, unclassifiable.
        assert_eq!(
            classify_tick_volume(f64::NAN, 101.0, 500, &mut carry),
            None,
            "real volume with no previous price is UNKNOWN, never balanced"
        );
        assert_eq!(
            carry, 0,
            "inventing a direction here would sign every following flat tick"
        );

        // Absent-price sentinel and poisoned prices on the current side: real
        // volume arrived under a price we cannot read.
        assert_eq!(classify_tick_volume(100.0, 0.0, 500, &mut carry), None);
        assert_eq!(classify_tick_volume(100.0, f64::NAN, 500, &mut carry), None);
        assert_eq!(
            classify_tick_volume(100.0, f64::INFINITY, 500, &mut carry),
            None
        );

        // Flat with no carry: real volume, and no side has EVER revealed
        // itself. The opening-bar case, and the one that used to publish
        // `Some(0)` = "perfectly balanced" about flow nobody measured.
        assert_eq!(
            classify_tick_volume(100.0, 100.0, 500, &mut carry),
            None,
            "real volume with no known direction must never read as balanced"
        );

        // Now the classifying cases.
        assert_eq!(
            classify_tick_volume(100.0, 101.0, 500, &mut carry),
            Some(500)
        );
        assert_eq!(carry, 1, "an uptick sets the carry to buy");
        assert_eq!(
            classify_tick_volume(101.0, 101.0, 300, &mut carry),
            Some(300)
        );
        assert_eq!(carry, 1, "a flat tick READS the carry, never rewrites it");
        assert_eq!(
            classify_tick_volume(101.0, 99.0, 700, &mut carry),
            Some(-700)
        );
        assert_eq!(carry, -1, "a downtick sets the carry to sell");
        assert_eq!(
            classify_tick_volume(99.0, 99.0, 200, &mut carry),
            Some(-200)
        );
    }

    /// The distinction the `Option` exists for, stated as its own test so it
    /// cannot be collapsed back by a future refactor: a genuinely-zero tick and
    /// an unclassifiable tick must NOT compare equal.
    #[test]
    fn a_balanced_tick_and_an_unclassifiable_tick_are_different_answers() {
        let mut carry = 0i8;
        let nothing_traded = classify_tick_volume(100.0, 101.0, 0, &mut carry);
        let traded_but_unknown = classify_tick_volume(100.0, 100.0, 500, &mut carry);

        assert_eq!(nothing_traded, Some(0));
        assert_eq!(traded_but_unknown, None);
        assert_ne!(
            nothing_traded, traded_but_unknown,
            "collapsing these two is the defect: one means no flow existed, the \
             other means flow existed and we could not read its side"
        );
    }

    /// `-(u64 as i64)` past `i64::MAX` wraps POSITIVE, which would record a
    /// sell as a buy. Same hazard, same handling, as the tick-persistence path.
    #[test]
    fn classify_tick_volume_saturates_instead_of_wrapping_a_sell_into_a_buy() {
        let mut carry = -1i8;
        let signed = classify_tick_volume(100.0, 99.0, u64::MAX, &mut carry)
            .expect("a downtick with a readable price classifies");
        assert!(signed < 0, "a downtick must never classify as buy volume");
        assert_eq!(signed, -i64::MAX);
    }

    /// An exact comparison, never a widened `f32`.
    ///
    /// `10.20_f32 as f64` is `10.19999980926514`. Comparing that against a
    /// decimal-clean `10.2` reports an UNCHANGED price as an UPTICK —
    /// systematically, on the majority of ticks, which would turn this column
    /// into a near-copy of gross volume.
    #[test]
    fn an_unchanged_decimal_clean_price_is_flat_not_an_uptick() {
        let clean = tickvault_common::price_precision::f32_to_f64_clean(10.20_f32);
        let mut carry = -1i8;
        assert_eq!(
            classify_tick_volume(clean, clean, 900, &mut carry),
            Some(-900),
            "identical decimal-clean prices must compare EQUAL and take the \
             carry, not read as a rise"
        );
    }

    /// The day boundary resets the carry, never inherits it.
    ///
    /// A direction learned from yesterday's last print is not evidence about
    /// today's first, and carrying it would attribute the whole of the new
    /// session's opening flat volume to whichever side moved the price at
    /// yesterday's close.
    #[test]
    fn a_force_seal_clears_the_tick_rule_carry() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base, 100.0, 1_000),
            None,
            sink,
        );
        // A downtick sets the carry to sell.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 1, 99.0, 1_500),
            None,
            sink,
        );

        let _ = agg.force_seal_all(sink);

        // New day: a flat tick must NOT inherit yesterday's sell direction.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 86_400, 99.0, 200),
            None,
            sink,
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, base + 86_401, 99.0, 900),
            None,
            sink,
        );

        assert_eq!(
            agg.snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
                .expect("open")
                .net_volume_signed,
            0,
            "yesterday's direction is not evidence about today"
        );
    }
    // -- volume-conservation guards (live defect, measured 2026-08-24) ------
    //
    // The live box tiled ONE trading day five ways and got five different
    // volume totals: 1s 40,397,638,853 / 30s 40,150,925,671 / 1m
    // 40,529,097,793 / 5m 41,219,723,749 / 1d 4,372,993,982. The intraday
    // frames were ~9.2x the day bar and 6,088 instruments disagreed with
    // their own 1m sum. These four tests are the shapes that produced it.

    #[test]
    fn a_late_tick_with_a_smaller_cumulative_must_not_lower_the_next_buckets_baseline() {
        // BITE PROOF: with the pre-fix unconditional
        // `slot.last_cumulative = cumulative_volume` this asserts
        // 1_000 == 4_000 and FAILS.
        //
        // `tick.volume` is DAY-CUMULATIVE, so a late tick carries a SMALLER
        // value. Storing it dragged the NEXT bucket's baseline backwards, and
        // that bucket then re-counted volume the previous bucket had already
        // reported. Silently self-amplifying: nothing downstream sees a
        // baseline.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut m1: Vec<(u32, u64)> = Vec::new();
        let collect = |tf: TfIndex, st: LiveCandleState, out: &mut Vec<(u32, u64)>| {
            if tf == TfIndex::M1 {
                out.push((st.bucket_start_ist_secs, st.volume));
            }
        };

        // Seed, then advance well inside the first minute.
        for (off, cum) in [(0_u32, 1_000_u32), (10, 5_000)] {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                collect(tf, st, &mut m1);
            });
        }

        // A LATE tick: earlier timestamp, therefore smaller cumulative.
        let late = tick(13, SEG_IDX, OPEN + 1, 99.0, 2_000);
        let _ = agg.consume_tick(Feed::Dhan, &late, None, |_, _, _, tf, st| {
            collect(tf, st, &mut m1);
        });

        // Roll into the next minute. Its baseline must be 5_000, not 2_000.
        let next = tick(13, SEG_IDX, OPEN + 70, 101.0, 6_000);
        let _ = agg.consume_tick(Feed::Dhan, &next, None, |_, _, _, tf, st| {
            collect(tf, st, &mut m1);
        });
        agg.force_seal_all(|_, _, _, tf, st| collect(tf, st, &mut m1));

        // Last emission per bucket wins (a Refold amend re-emits its bucket).
        let vol_of = |start: u32| -> u64 {
            m1.iter()
                .rfind(|(b, _)| *b == start)
                .map_or(u64::MAX, |(_, v)| *v)
        };
        assert_eq!(vol_of(OPEN), 4_000, "first minute: 5_000 - seeded 1_000");
        assert_eq!(
            vol_of(OPEN + 60),
            1_000,
            "second minute must baseline on 5_000 (the high-water cumulative), \
             never on the late tick's stale 2_000"
        );
    }

    #[test]
    fn a_mid_session_slot_creation_must_not_put_a_whole_days_volume_in_one_bar() {
        // BITE PROOF: with the pre-fix `last_cumulative: 0` this asserts
        // 0 == 1_000_000 and FAILS.
        //
        // A slot allocated an hour into the session has never seen this
        // instrument. `0` is not a baseline, it is the ABSENCE of one, and
        // `cumulative - 0` published the whole day so far as a single bar.
        // The first tick seeds the baseline instead: the first bar
        // under-reports by the unattributable amount and
        // `tv_aggregator_slot_volume_baseline_seeded_total` counts it.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut m1: Vec<(u32, u64)> = Vec::new();

        let first = tick(13, SEG_IDX, OPEN + 3_600, 100.0, 1_000_000);
        let _ = agg.consume_tick(Feed::Dhan, &first, None, |_, _, _, _, _| {});
        let second = tick(13, SEG_IDX, OPEN + 3_610, 101.0, 1_000_500);
        let _ = agg.consume_tick(Feed::Dhan, &second, None, |_, _, _, _, _| {});
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                m1.push((st.bucket_start_ist_secs, st.volume));
            }
        });

        assert_eq!(m1.len(), 1);
        assert_eq!(
            m1[0].1, 500,
            "the bar reports only what we observed (1_000_500 - 1_000_000); \
             pre-arrival volume is unattributable, never the bar's"
        );
    }

    #[test]
    fn a_new_day_resets_the_baseline_so_the_monotonic_rule_cannot_freeze_volume() {
        // The other half of the monotonic rule, and wrong to omit: the
        // vendor's cumulative restarts at ~0 each session. Without the
        // day-boundary reset in `force_seal_all` the advance-only rule would
        // read tomorrow's honest small cumulative as a regression and publish
        // every bar of the new day at volume 0.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let day1 = tick(13, SEG_IDX, OPEN + 10, 100.0, 900_000);
        let _ = agg.consume_tick(Feed::Dhan, &day1, None, |_, _, _, _, _| {});
        agg.force_seal_all(|_, _, _, _, _| {});

        // Next session: cumulative restarts small.
        let mut m1: Vec<u64> = Vec::new();
        for (off, cum) in [(0_u32, 100_u32), (10, 700)] {
            let t = tick(13, SEG_IDX, OPEN + 86_400 + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                m1.push(st.volume);
            }
        });
        assert_eq!(m1, vec![600], "600 = 700 - the new day's seeded 100");
    }

    #[test]
    fn every_timeframe_of_one_day_must_sum_to_the_same_volume_total() {
        // THE INVARIANT. This is the test that would have caught the live
        // defect: the same day tiled three ways must sum to one total.
        // It FAILS on the pre-fix code (1s and 1m over-report against 1d,
        // exactly as the box did).
        //
        // The sequence is a realistic session slice: several ticks per
        // second, two OUT-OF-ORDER ticks inside an open bucket, and two
        // genuinely LATE ticks arriving after their bucket sealed — the
        // three shapes `FeedStrategy::DEFAULT`'s Refold policy makes routine
        // (10.0% of live ticks arrive >1h behind receive time).
        const SEQ: &[(u32, u32)] = &[
            (0, 1_000), // seeds the baseline
            (1, 1_200),
            (2, 1_500),
            (2, 1_400), // out of order, same second
            (5, 2_000),
            (59, 3_000),
            (60, 3_500), // rolls 1s and 1m
            (30, 2_500), // LATE: its 1m bucket already sealed
            (61, 4_000),
            (120, 5_000),
            (119, 4_800), // LATE again
            (180, 6_000),
            // ADDED 2026-09-11 — the shape this invariant could NOT see.
            //
            // Both LATE ticks above carry a SMALLER cumulative than the tick
            // before them, so they are stale packets: their delta off the
            // monotonic baseline is zero, and a frame that refuses them loses
            // nothing. That made every refusal in this sequence free, and the
            // invariant passed while the fold was losing volume in
            // production.
            //
            // A late tick can equally carry a LARGER cumulative — it is a
            // reading we had not seen, arriving out of order — and then the
            // refusing frame IS told about real units. Before the
            // unattributed carry, the 1s and 1m frames dropped these 1,000
            // units on the floor while the day frame (whose bucket is still
            // open, so `cumulative − bucket_start` sweeps them up) counted
            // them: `left: 6000, right: 5000`, verified by running it.
            //
            // MEASURED on the live box the same day, security 68407:
            // `candles_1s` 1,068,340 against `candles_5s` and `candles_1m`
            // 1,071,330 — short by 2,990 gross and 650 of net, on the same
            // ticks, for exactly this reason.
            (30, 7_000), // LATE, and carrying NEWS
        ];

        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        // (tf, bucket_start) -> volume; a Refold amend re-emits its bucket,
        // so the LAST emission per key is the published bar.
        let mut bars: std::collections::HashMap<(TfIndex, u32), u64> =
            std::collections::HashMap::new();

        for (off, cum) in SEQ {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, *cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });

        let total = |want: TfIndex| -> u64 {
            bars.iter()
                .filter(|((tf, _), _)| *tf == want)
                .map(|(_, v)| *v)
                .sum()
        };

        // Ground truth: the highest cumulative observed minus the first one.
        // Volume traded before our first tick is unattributable to any bucket
        // we own, so it is excluded from BOTH sides — never invented.
        let expected = 7_000_u64 - 1_000;
        assert_eq!(
            total(TfIndex::S1),
            expected,
            "1s frames must tile the day exactly"
        );
        assert_eq!(
            total(TfIndex::M1),
            expected,
            "1m frames must tile the day exactly"
        );
        assert_eq!(total(TfIndex::D1), expected, "the day bar is the same day");
        assert_eq!(total(TfIndex::S1), total(TfIndex::M1));
        assert_eq!(total(TfIndex::M1), total(TfIndex::D1));
    }

    /// The unattributed carry must be settled ONCE — the sharpest edge in the
    /// mechanism, and the one the invariant test above cannot reach.
    ///
    /// A bucket's volume is recomputed on every in-bucket fold as
    /// `cumulative − bucket_start_cumulative`, a span that ALREADY contains
    /// any tick this frame refused since the bucket opened. So the moment a
    /// tick lands in the same bucket as a refusal, the gross is settled by
    /// arithmetic — and a carry left standing would be applied a SECOND time
    /// at the next bucket open, inventing volume that never traded.
    ///
    /// That is the opposite failure from the one the carry exists to fix, it
    /// is silent in exactly the same way, and the invariant sequence above
    /// never produces it: none of its refusals is followed by a tick in the
    /// same bucket of the refusing frame.
    ///
    /// BITE PROOF: making `settle_carry_into_open_bucket` a no-op leaves the
    /// 1m total at 6,000 against a ground truth of 5,000 — the carry counted
    /// twice.
    #[test]
    fn an_unattributed_carry_swept_up_in_bucket_is_never_settled_a_second_time() {
        // (offset from the open, cumulative). Chosen so the 1m frame refuses a
        // tick, then receives one INSIDE the same bucket, then rolls:
        //
        //   0   opens 1m bucket 0
        //   60  rolls  — seals bucket 0, opens bucket 60
        //   10  LATE for 1m (its bucket 0 already sealed) — carries 1,000
        //   70  IN bucket 60 — `cumulative − bucket_start` sweeps the carry up
        //   120 rolls  — must NOT apply the carry again
        const SEQ: &[(u32, u32)] = &[
            (0, 1_000),
            (60, 2_000),
            (10, 3_000), // LATE, carrying NEWS
            (70, 4_000), // same 1m bucket as the roll above
            (120, 5_000),
        ];

        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut bars: std::collections::HashMap<(TfIndex, u32), u64> =
            std::collections::HashMap::new();
        for (off, cum) in SEQ {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, *cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });

        let total = |want: TfIndex| -> u64 {
            bars.iter()
                .filter(|((tf, _), _)| *tf == want)
                .map(|(_, v)| *v)
                .sum()
        };

        // Ground truth, the same rule the invariant above uses: the highest
        // cumulative observed minus the first, because volume traded before
        // our first tick belongs to no bucket we own.
        let expected = 5_000_u64 - 1_000;
        assert_eq!(
            total(TfIndex::M1),
            expected,
            "the carry was swept up in-bucket; applying it again at the next \
             open would invent volume"
        );
        // The day frame never refuses anything, so it is the independent
        // witness: if the 1m total exceeds it, the extra units are fabricated.
        assert_eq!(total(TfIndex::D1), expected);
        assert_eq!(total(TfIndex::S1), expected);
    }

    /// The carried SIGN travels with the carried units, so the bar that
    /// receives them counts the same trades twice over — once gross, once net.
    ///
    /// This is the half a gross-only fix silently leaves broken. `volume` and
    /// `net_volume` are two readings of ONE set of trades, and
    /// `net_volume().abs() <= volume` is a structural fact only while both are
    /// fed from the same deltas. Settling the gross alone would hand the
    /// receiving bar units whose direction it never learned, while
    /// `net_volume_classified` still reported the bar fully classified — a
    /// confident answer over volume nobody signed.
    ///
    /// BITE PROOF: dropping `carry.net` from the bucket-open seed leaves this
    /// bar at `Some(1000)` against a gross of 2,000 — half its flow missing,
    /// and nothing anywhere saying so.
    #[test]
    fn a_settled_carry_brings_its_sign_with_it_not_just_its_units() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};

        // Every tick is an UPTICK, so every delta is buy-initiated and the
        // arithmetic stays readable: net must equal gross throughout.
        for (off, cum, px) in [
            (0_u32, 1_000_u32, 100.0_f32), // seeds the baseline (unclassified)
            (60, 2_000, 101.0),            // rolls 1m: opens bucket 60
            (10, 3_000, 102.0),            // LATE for 1m — carries +1,000
            (120, 4_000, 103.0),           // rolls 1m: opens bucket 120
        ] {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, OPEN + off, px, cum),
                None,
                sink,
            );
        }

        let bar = agg
            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
            .expect("bucket 120 is open");
        assert_eq!(
            bar.volume, 2_000,
            "its own 1,000 plus the 1,000 the late tick brought"
        );
        assert_eq!(
            bar.net_volume(),
            Some(2_000),
            "both deltas were buy-initiated, so the net must account for the \
             carried units too — a net short of the gross here means the \
             carry arrived unsigned"
        );
    }

    /// A bar that settles units nobody could SIGN inherits the ignorance —
    /// it does not report a confident net over volume it never classified.
    ///
    /// The unclassifiable case is reachable on the live path and is not the
    /// first-tick one: `classify_tick_volume` also refuses when real volume
    /// arrives at an UNCHANGED price and no side has ever revealed itself for
    /// that instrument, which is the ordinary state of an instrument whose
    /// opening prints all match. A late tick of that shape carries units and
    /// no direction.
    ///
    /// Settling its gross while leaving `net_volume_classified` true would
    /// publish a net computed from a strict subset of the bar's own volume and
    /// assert it complete — the exact false-OK the column exists to refuse.
    ///
    /// BITE PROOF: dropping `&& !carry.unclassified` from the bucket-open seed
    /// turns the assertion below into `Some(1000)` against a gross of 2,000.
    #[test]
    fn a_bar_that_settles_unsignable_units_refuses_to_report_a_net() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};

        for (off, cum, px) in [
            // Flat opening prints: no side ever reveals itself, so every
            // delta below is real volume with no readable direction.
            (0_u32, 1_000_u32, 100.0_f32), // first tick — unclassifiable
            (60, 2_000, 100.0),            // flat, carry still 0 — unclassifiable
            (10, 3_000, 100.0),            // LATE for 1m, and UNSIGNABLE
            (120, 4_000, 101.0),           // first real move: rolls 1m, classifiable
        ] {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, OPEN + off, px, cum),
                None,
                sink,
            );
        }

        let bar = agg
            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
            .expect("bucket 120 is open");
        assert_eq!(
            bar.volume, 2_000,
            "the units are still counted — ignorance of direction is not a \
             reason to lose the trades"
        );
        assert_eq!(
            bar.net_volume(),
            None,
            "1,000 of this bar's 2,000 units arrived with no readable side, \
             so the honest answer is NULL rather than a net over half of it"
        );
    }

    /// A carry outstanding at the DAY BOUNDARY is settled into that day's
    /// final bar of its timeframe, never carried into tomorrow.
    ///
    /// Tomorrow's slot baseline is re-seeded from tomorrow's first tick, so a
    /// carry measured against today's cumulative would describe a span that no
    /// longer exists — applying it across the boundary is a WRONG answer, not
    /// an imprecise one. Settling it here is the carry's own rule ("the next
    /// bucket this frame touches") reaching its last opportunity of the day.
    #[test]
    fn a_carry_outstanding_at_the_day_boundary_lands_in_todays_final_bar() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut m1: Vec<(u32, u64)> = Vec::new();

        // 0 opens 1m bucket 0; 60 rolls it; 10 is LATE for 1m and carries
        // 1,000 with no further tick to sweep it up. The boundary is next.
        for (off, cum) in [(0_u32, 1_000_u32), (60, 2_000), (10, 3_000)] {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                if tf == TfIndex::M1 {
                    m1.push((st.bucket_start_ist_secs, st.volume));
                }
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                m1.push((st.bucket_start_ist_secs, st.volume));
            }
        });

        // Last emission per bucket wins — a Refold amend re-emits its bucket.
        let mut by_bucket: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
        for (start, vol) in m1 {
            by_bucket.insert(start, vol);
        }
        let total: u64 = by_bucket.values().sum();
        assert_eq!(
            total,
            3_000 - 1_000,
            "the day's 1m bars must still tile the day — the carry lands in \
             the final bar rather than being forfeited at the boundary"
        );
        assert_eq!(
            by_bucket.get(&(OPEN + 60)).copied(),
            Some(2_000),
            "bucket 60 holds its own 1,000 plus the 1,000 the late tick \
             brought and no frame had yet placed"
        );
    }

    /// Collects `(feed, sid, seg, tf, bucket_start, o, h, l, c)` for
    /// order-insensitive comparison.
    type SealRow = (Feed, u64, u8, TfIndex, u32, f64, f64, f64, f64);

    fn row(feed: Feed, sid: u64, seg: u8, tf: TfIndex, s: LiveCandleState) -> SealRow {
        (
            feed,
            sid,
            seg,
            tf,
            s.bucket_start_ist_secs,
            s.open,
            s.high,
            s.low,
            s.close,
        )
    }

    // -- 2026-08-25 volume-baseline + price-gate regressions ----------------

    /// Reads a sealed M1 bar's volume for `bucket_start`, sealing by pushing a
    /// tick well past the day so every open bucket closes.
    fn m1_volumes(agg: &mut MultiTfAggregator, sid: u64) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        agg.force_seal_all(|_, s, _, tf, st| {
            if s == sid && tf == TfIndex::M1 {
                out.push((st.bucket_start_ist_secs, st.volume));
            }
        });
        out.sort_unstable();
        out
    }

    #[test]
    fn test_an_out_of_order_packet_cannot_lower_the_next_buckets_volume_baseline() {
        // The bite test for the 2026-08-25 baseline fix. Cumulative traded
        // volume only rises within a session, so a LOWER arrival is a
        // reordered packet — and this feed reorders, which is why
        // `LatePolicy::Refold` exists. Under last-write-wins the stale packet
        // lowered `last_cumulative`; the next bucket then opened with a
        // baseline below the true figure and DOUBLE-COUNTED the slice already
        // charged to the bucket before it.
        //
        // Revert `slot.last_cumulative.max(cumulative_volume)` back to a plain
        // assignment and minute two's volume reads 900 instead of 500.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DISCARD);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};

        // Minute one: cumulative climbs 100 -> 500.
        let _ = agg.consume_tick(Feed::Dhan, &tick(13, SEG_IDX, OPEN, 100.0, 100), None, sink);
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 30, 101.0, 500),
            None,
            sink,
        );
        // A reordered straggler carrying a STALE cumulative, still inside
        // minute one. Its own bar is protected by the in-bucket `max`; the
        // baseline it leaves behind is what this test is about.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 40, 101.0, 100),
            None,
            sink,
        );
        // Minute two: cumulative reaches 1000, so the true bucket volume is
        // 1000 - 500 = 500.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 65, 102.0, 1000),
            None,
            sink,
        );

        let vols = m1_volumes(&mut agg, 13);
        let minute_two = vols
            .iter()
            .find(|(b, _)| *b == OPEN + 60)
            .expect("minute two sealed");
        assert_eq!(
            minute_two.1, 500,
            "minute two must charge only its own slice (1000-500), never the \
             400 already charged to minute one"
        );
    }

    #[test]
    fn test_the_day_close_seal_clears_the_slot_volume_baseline() {
        // `force_seal` resets the CELL's day state; `last_cumulative` lives on
        // the SLOT and was never reset. A process spanning midnight opened day
        // two's first bucket with yesterday's final cumulative as baseline —
        // `saturating_sub` floored every bucket to 0, and with the monotonic
        // `max` above it would have STAYED pinned there. D1 is the worst case:
        // one bucket per day, so the entire daily bar reads zero volume.
        //
        // Delete the `slot.last_cumulative = 0;` line in `force_seal_all` and
        // day two's volume reads 0 instead of 300.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DISCARD);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 9_000),
            None,
            sink,
        );
        // Session close: this is the production day-boundary seal.
        let _ = agg.force_seal_all(|_, _, _, _, _| {});

        // Day two — cumulative restarts near zero, as the exchange does.
        let open2 = OPEN + 86_400;
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, open2, 100.0, 100),
            None,
            sink,
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, open2 + 30, 101.0, 400),
            None,
            sink,
        );

        let mut day2 = None;
        agg.force_seal_all(|_, s, _, tf, st| {
            if s == 13 && tf == TfIndex::M1 && st.bucket_start_ist_secs == open2 {
                day2 = Some(st.volume);
            }
        });
        // 300, and the number itself is the merge decision (2026-08-25).
        //
        // The day-close drops the baseline to UNSEEDED, so day two's FIRST
        // packet (cumulative 100) re-seeds it, and the first bar owns only
        // what traded after that observation: 400 - 100 = 300.
        //
        // This branch originally asserted 400, arguing that a continuously
        // running process owns everything since the open. That is more precise
        // and less safe: it cannot distinguish "we were running and had not yet
        // received a packet" from "we arrived late", so it can OVER-attribute.
        // Seeding can only ever under-attribute the sub-second window before
        // the day's first packet, and it reuses the rule that already governs
        // a slot allocated mid-session — one rule for both kinds of arrival.
        //
        // What this test still proves is the defect it was written for: delete
        // the reset in `force_seal_all` and this reads 0, not 300, because
        // yesterday's 9,000 baseline floors both packets through
        // `saturating_sub`.
        assert_eq!(
            day2,
            Some(300),
            "day two's first bar must be measured from its own re-seeded \
             baseline, not floored to zero by yesterday's 9,000 cumulative"
        );
    }

    #[test]
    fn test_a_price_that_widens_to_zero_is_refused_rather_than_zeroing_the_bar() {
        // `f32::MIN_POSITIVE` is finite, greater than zero, and inside the
        // ceiling — so the old raw-value gate passed it — and it is not
        // `== 0.0`, so it escaped the untraded-sentinel arm too.
        // `f32_to_f64_clean` then collapsed it to 0.0 (Rust's f32 Display
        // never uses scientific notation, so it overflows the 24-byte format
        // buffer), setting open/high/low/close to zero and PINNING `low` there
        // for the rest of the bucket.
        //
        // Note it is a NORMAL float: an `is_normal()` gate — the obvious fix,
        // and the one first attempted here — lets this exact value through.
        // The gate tests the WIDENED value for that reason.
        //
        // Drop `&& prices.last_traded_price > 0.0` from the gate and the bar's
        // low reads 0.0.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DISCARD);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 24_000.0, 10),
            None,
            sink,
        );
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 5, f32::MIN_POSITIVE, 11),
            None,
            sink,
        );
        assert!(
            stats.refused_price,
            "a subnormal must be refused as an unrepresentable price"
        );
        assert!(
            !stats.untraded_sentinel,
            "it is not the zero sentinel — mislabelling it would hide the class"
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 10, 24_010.0, 12),
            None,
            sink,
        );

        let mut low = None;
        agg.force_seal_all(|_, s, _, tf, st| {
            if s == 13 && tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN {
                low = Some(st.low);
            }
        });
        assert_eq!(
            low,
            Some(24_000.0),
            "the bar's low must be the real low, not a zero left by one \
             mangled packet"
        );
    }

    #[test]
    fn test_the_zero_sentinel_still_classifies_as_untraded_after_the_gate_swap() {
        // The zero check moved AHEAD of the representability gate. Pin that
        // both zeros still land in the sentinel bucket rather than being
        // relabelled as bad prices — `-0.0 == 0.0` is true in IEEE-754, and
        // that equality is what keeps negative zero classified correctly.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DISCARD);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        for price in [0.0_f32, -0.0_f32] {
            let stats =
                agg.consume_tick(Feed::Dhan, &tick(13, SEG_IDX, OPEN, price, 1), None, sink);
            assert!(
                stats.untraded_sentinel,
                "{price} must be the untraded sentinel, not a refused price"
            );
            assert!(!stats.refused_price);
        }
    }

    // -- construction / accessors -------------------------------------------

    #[test]
    fn test_multi_tf_aggregator_new_starts_empty_with_the_given_policy() {
        let agg = MultiTfAggregator::new(FeedStrategy::DISCARD);
        assert!(agg.is_empty());
        assert_eq!(agg.len(), 0);
        assert_eq!(agg.strategy.late_policy, LatePolicy::Discard);
        assert_eq!(agg.watermark_secs(), 0);
        assert_eq!(agg.slots_exhausted_total(), 0);
    }

    #[test]
    fn test_multi_tf_aggregator_with_capacity_clamps_to_the_slot_ceiling() {
        let agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, usize::MAX);
        assert!(agg.is_empty(), "capacity must not pre-populate slots");
        // No panic / no 16-exabyte reservation is the real assertion here.
        assert_eq!(agg.len(), 0);
    }

    #[test]
    fn test_multi_tf_aggregator_len_and_is_empty_track_allocated_slots() {
        let mut agg = MultiTfAggregator::default();
        assert!(agg.is_empty());
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert_eq!(agg.len(), 1);
        assert!(!agg.is_empty());
        // Same identity again — no new slot.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 1, 101.0, 2),
            None,
            |_, _, _, _, _| {},
        );
        assert_eq!(agg.len(), 1);
    }

    #[test]
    fn test_multi_tf_aggregator_lookup_is_read_only_and_never_allocates() {
        let mut agg = MultiTfAggregator::default();
        assert_eq!(agg.lookup(Feed::Dhan, 13, SEG_IDX), None);
        assert_eq!(agg.len(), 0, "a pure query must not consume capacity");
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert_eq!(agg.lookup(Feed::Dhan, 13, SEG_IDX), Some(0));
        assert_eq!(agg.lookup(Feed::Truedata, 13, SEG_IDX), None);
    }

    #[test]
    fn test_multi_tf_aggregator_snapshot_returns_the_open_bucket_per_identity() {
        let mut agg = MultiTfAggregator::default();
        assert_eq!(agg.snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1), None);
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 5),
            None,
            |_, _, _, _, _| {},
        );
        let s = agg
            .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
            .expect("slot exists");
        assert_eq!(s.bucket_start_ist_secs, OPEN);
        assert_eq!(s.close, 100.0);
    }

    #[test]
    fn test_multi_tf_aggregator_watermark_secs_never_regresses() {
        let mut agg = MultiTfAggregator::default();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 100, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert_eq!(agg.watermark_secs(), OPEN + 100);
        // An older (late) tick must not pull the watermark back.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 10, 100.0, 2),
            None,
            |_, _, _, _, _| {},
        );
        assert_eq!(agg.watermark_secs(), OPEN + 100);
        // A post-close tick still advances it (so the last session bar can seal).
        let post_close = DAY + 56_400 + 5;
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, post_close, 100.0, 3),
            None,
            |_, _, _, _, _| {},
        );
        assert!(stats.out_of_session, "post-close must be gated out");
        assert_eq!(agg.watermark_secs(), post_close, "…but must still advance");
    }

    #[test]
    fn test_multi_tf_aggregator_reset_watermark_clears_it() {
        let mut agg = MultiTfAggregator::default();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(agg.watermark_secs() > 0);
        agg.reset_watermark();
        assert_eq!(agg.watermark_secs(), 0);
    }

    /// A seed may only ever RAISE the watermark.
    ///
    /// The monotonic half is what makes seeding safe to call unconditionally:
    /// if it could lower the watermark, seeding after a day had already
    /// advanced would re-open a closed day -- the exact hole it exists to shut.
    #[test]
    fn seeding_the_watermark_raises_it_but_never_lowers_it() {
        let mut agg = MultiTfAggregator::default();
        assert_eq!(agg.watermark_secs(), 0);

        agg.seed_watermark_at_least(DAY);
        assert_eq!(agg.watermark_secs(), DAY, "a seed above 0 must raise it");

        agg.seed_watermark_at_least(DAY - 86_400);
        assert_eq!(
            agg.watermark_secs(),
            DAY,
            "a seed BELOW the current watermark must be ignored, not applied"
        );

        agg.seed_watermark_at_least(DAY);
        assert_eq!(agg.watermark_secs(), DAY, "seeding twice is a no-op");
    }

    /// The defect this whole mechanism exists for, reproduced end to end.
    ///
    /// Boot replay is not day-scoped, so a segment deferred at yesterday's
    /// shutdown reaches a FRESH aggregator the next morning. `consume_tick`
    /// advances the watermark before it checks the stale-day gate, so the
    /// first prior-day frame sets the value it is compared against and folds
    /// into a bucket on a day that closed hours ago -- rebuilt from only the
    /// deferred subset, then upserted over the complete bar by a candle dedup
    /// key with no completeness column.
    ///
    /// Both halves are asserted, because either alone would be a false pass:
    /// unseeded MUST fold (proving the defect is real and the test can see
    /// it), seeded MUST refuse (proving the seed closes it).
    /// ONE vendor packet stamped for tomorrow used to end candles for the day.
    ///
    /// `fold_clock_ist_secs` returns the VENDOR's stamp whenever receipt and
    /// exchange disagree by more than the trusted band, and a stamp one day
    /// ahead disagrees by ~86,400 s. That value then advanced the watermark
    /// into day D+1, and every honest tick afterwards failed the stale-day
    /// gate: all 24 timeframes stop folding, for every instrument, with only a
    /// rising refusal counter to show for it.
    ///
    /// Both halves are asserted because either alone would be a false pass:
    /// the future tick MUST be refused, and the honest tick after it MUST
    /// still fold.
    #[test]
    fn a_future_dated_tick_is_refused_and_never_poisons_the_watermark() {
        let today_in_session = DAY + 33_300 + 60;
        // Receipt is genuinely today: IST secs -> UTC nanos.
        let receipt_now_nanos = (i64::from(today_in_session)
            - crate::candles::tf_index::IST_UTC_OFFSET_SECS)
            * 1_000_000_000;

        let mut agg = MultiTfAggregator::default();

        // A packet the vendor stamped for TOMORROW, received now.
        let mut future = tick(13, SEG_IDX, today_in_session + 86_400, 100.0, 1);
        future.received_at_nanos = receipt_now_nanos;
        let stats = agg.consume_tick(Feed::Dhan, &future, None, |_, _, _, _, _| {
            panic!("a future-dated tick must never seal a bar")
        });
        assert!(
            stats.future_trading_day,
            "a stamp one day ahead of our own receipt clock must be refused"
        );
        assert!(
            agg.lookup(Feed::Dhan, 13, SEG_IDX).is_none(),
            "and must not take a slot — the gate runs before slot allocation"
        );

        // THE POINT: an honest tick right after it still folds.
        let mut honest = tick(13, SEG_IDX, today_in_session, 100.0, 2);
        honest.received_at_nanos = receipt_now_nanos;
        let stats = agg.consume_tick(Feed::Dhan, &honest, None, |_, _, _, _, _| {});
        assert!(
            !stats.stale_trading_day,
            "the future tick must not have advanced the watermark — before this \
             gate, every honest tick for the rest of the session read as stale"
        );
        assert!(
            !stats.future_trading_day,
            "and an honest tick is not itself future-dated"
        );
        assert!(
            agg.lookup(Feed::Dhan, 13, SEG_IDX).is_some(),
            "the honest tick folds normally"
        );
    }

    /// The gate stands down with no receipt to compare against, so a WAL frame
    /// written before the TVW3 format carried one folds exactly as it did
    /// before. `received_at_nanos == 0` is the documented sentinel.
    #[test]
    fn a_future_dated_tick_with_no_receipt_clock_is_judged_as_before() {
        let today_in_session = DAY + 33_300 + 60;
        let mut agg = MultiTfAggregator::default();
        // Default `received_at_nanos` is 0 — the sentinel.
        let t = tick(13, SEG_IDX, today_in_session + 86_400, 100.0, 1);
        assert_eq!(t.received_at_nanos, 0, "fixture must exercise the sentinel");
        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
        assert!(
            !stats.future_trading_day,
            "with no second clock the gate must not guess"
        );
    }

    /// THE OPERATOR'S ROW (2026-09-10, NSE_FNO 66422): received at 09:15
    /// today, exchange stamp 15:29 of a PREVIOUS session.
    ///
    /// This is the FIRST tick the aggregator ever sees, which is what makes it
    /// the sharp case. The watermark gate cannot catch it — the tick sets the
    /// very watermark it would be compared against (pinned one test below, as
    /// the defect it is). The receipt clock has no such dependence, so the
    /// refusal here is order-independent and holds on a cold boot with no WAL
    /// backlog, which is exactly the shape a deploy or a restart produces.
    #[test]
    fn a_prior_day_snapshot_is_refused_on_the_first_tick_of_a_cold_boot() {
        let mut agg = MultiTfAggregator::default();

        // Yesterday 15:29 IST — inside the seconds-of-day window on BOTH ends,
        // which is precisely why every time-of-day gate waved it through.
        let yesterday_1529 = DAY - 86_400 + 15 * 3_600 + 29 * 60;
        // Received today at 09:15 IST. `received_at_nanos` is UTC epoch nanos,
        // so the IST offset comes off before it is stamped.
        let today_0915_utc_secs =
            i64::from(DAY + 9 * 3_600 + 15 * 60) - crate::candles::tf_index::IST_UTC_OFFSET_SECS;

        let mut t = tick(66_422, SEG_IDX, yesterday_1529, 142.50, 12_000);
        t.received_at_nanos = today_0915_utc_secs * 1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.stale_trading_day,
            "an exchange stamp from a previous day must be refused against the \
             RECEIPT, on the very first tick, with no watermark to lean on"
        );
        assert!(
            !stats.future_trading_day,
            "the mirror arm must not also fire — the two are exclusive"
        );
        assert!(
            agg.lookup(Feed::Dhan, 66_422, SEG_IDX).is_none(),
            "and it must not take a slot or open a bucket on a day that closed"
        );
    }

    /// The same instant, judged fresh: same clock, same contract, today's stamp.
    ///
    /// Without this the test above passes just as well against a gate that
    /// refuses everything.
    #[test]
    fn a_same_day_tick_at_the_same_receipt_instant_is_accepted() {
        let mut agg = MultiTfAggregator::default();

        let today_0915 = DAY + 9 * 3_600 + 15 * 60;
        let today_0915_utc_secs =
            i64::from(today_0915) - crate::candles::tf_index::IST_UTC_OFFSET_SECS;

        let mut t = tick(66_422, SEG_IDX, today_0915, 142.50, 12_000);
        t.received_at_nanos = today_0915_utc_secs * 1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            !stats.stale_trading_day,
            "a tick whose exchange day IS the receipt day must fold normally"
        );
        assert!(
            agg.lookup(Feed::Dhan, 66_422, SEG_IDX).is_some(),
            "and it must open its bucket"
        );
    }

    /// A tick received just after IST midnight, stamped just before it, is NOT
    /// stale — it is a boundary crossing, and refusing it would silently drop
    /// the last trades of every session.
    ///
    /// `fold_clock_ist_secs` is what makes this safe: receipt and exchange
    /// agree well inside the trusted band, so the FOLD clock is the receipt,
    /// and both sides of the comparison land on the same day. The gate is
    /// therefore judging a genuine day mismatch, not a clock straddle.
    #[test]
    fn a_tick_straddling_ist_midnight_inside_the_trusted_band_is_not_stale() {
        let mut agg = MultiTfAggregator::default();

        let just_before_midnight = DAY - 1;
        let just_after_midnight_utc_secs =
            i64::from(DAY + 1) - crate::candles::tf_index::IST_UTC_OFFSET_SECS;

        let mut t = tick(66_422, SEG_IDX, just_before_midnight, 142.50, 12_000);
        t.received_at_nanos = just_after_midnight_utc_secs * 1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            !stats.stale_trading_day,
            "two seconds apart is inside the trusted band, so the fold clock \
             takes the receipt and both sides land on the same day — a \
             refusal here would drop real closing trades every session"
        );
    }

    #[test]
    fn a_prior_day_replay_frame_folds_unseeded_and_is_refused_once_seeded() {
        let yesterday_in_session = DAY - 86_400 + 33_300 + 60;
        let today_start = DAY;

        // --- unseeded: the defect ---------------------------------------
        let mut unseeded = MultiTfAggregator::default();
        let stats = unseeded.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, yesterday_in_session, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(
            !stats.stale_trading_day,
            "without a seed the first prior-day frame sets the very watermark \
             it is checked against, so it passes -- this is the defect"
        );
        assert!(
            unseeded.lookup(Feed::Dhan, 13, SEG_IDX).is_some(),
            "and it takes a slot and opens a bucket on a day that already closed"
        );
        assert_eq!(
            unseeded
                .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
                .expect("slot exists")
                .bucket_start_ist_secs,
            yesterday_in_session - (yesterday_in_session % 60),
            "the bucket is dated on the closed day, which is what would upsert \
             over that day's complete bar"
        );

        // --- seeded: the fix ---------------------------------------------
        let mut seeded = MultiTfAggregator::default();
        seeded.seed_watermark_at_least(today_start);
        let stats = seeded.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, yesterday_in_session, 100.0, 1),
            None,
            |_, _, _, _, _| panic!("a prior-day replay frame must never seal a bar"),
        );
        assert!(
            stats.stale_trading_day,
            "seeded, the same frame must be refused as stale_trading_day"
        );
        assert!(
            seeded.lookup(Feed::Dhan, 13, SEG_IDX).is_none(),
            "and must not even take a slot -- the stale-day gate runs before \
             slot allocation, so a prior-day replay cannot burn capacity"
        );
    }

    /// The seed must not break the case boot replay is actually used for.
    ///
    /// The ordinary crash-restart replays SAME-DAY frames, and those must
    /// still fold -- otherwise the fix would trade a rare corruption for a
    /// daily loss of recovered bars, which is a worse bargain.
    #[test]
    fn a_same_day_replay_frame_still_folds_after_seeding() {
        let mut agg = MultiTfAggregator::default();
        agg.seed_watermark_at_least(DAY);

        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, DAY + 33_300 + 60, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(
            !stats.stale_trading_day,
            "a same-day frame must pass the seeded gate"
        );
        assert!(
            stats.folded(),
            "and must fold normally -- the fix must not cost the ordinary \
             crash-restart its recovered bars"
        );
    }

    // -- the load-bearing behaviours ----------------------------------------

    #[test]
    fn test_multi_tf_aggregator_consume_tick_opens_every_timeframe_on_the_first_tick() {
        let mut agg = MultiTfAggregator::default();
        let mut seals = Vec::new();
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 7, 100.0, 42),
            None,
            |f, s, g, tf, st| seals.push(row(f, s, g, tf, st)),
        );
        assert!(stats.folded());
        assert_eq!(stats.sealed_count, 0, "the first tick seals nothing");
        assert_eq!(stats.late_count, 0);
        assert!(seals.is_empty());
        for tf in TfIndex::ALL {
            let s = agg
                .snapshot(Feed::Dhan, 13, SEG_IDX, tf)
                .expect("slot exists");
            assert!(!s.is_uninitialised(), "{tf:?} must be open");
            assert_eq!(s.open, 100.0);
            assert_eq!(s.tick_count, 1);
            assert_eq!(
                s.bucket_start_ist_secs,
                tf.bucket_start(OPEN + 7),
                "{tf:?} bucket must be TF-aligned"
            );
        }
    }

    /// SPARSITY — the single most consequential property in this engine.
    ///
    /// A dense engine would emit one bar per (instrument × TF × elapsed
    /// bucket). Here a 10-minute silence between two ticks must produce
    /// EXACTLY ONE `candles_1m` seal (the bucket that actually had a tick),
    /// never ten, and the untouched buckets must emit nothing at all.
    #[test]
    fn test_multi_tf_aggregator_is_sparse_a_ten_minute_gap_emits_one_bar_per_tf() {
        let mut agg = MultiTfAggregator::default();
        let mut seals: Vec<SealRow> = Vec::new();
        let mut push = |f, s, g, tf, st| seals.push(row(f, s, g, tf, st));
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            &mut push,
        );
        // Ten minutes of total silence, then one tick.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 600, 105.0, 2),
            None,
            &mut push,
        );
        let m1: Vec<&SealRow> = seals.iter().filter(|r| r.3 == TfIndex::M1).collect();
        assert_eq!(
            m1.len(),
            1,
            "exactly ONE 1m bar (the bucket that ticked), not ten empties: {m1:?}"
        );
        assert_eq!(m1[0].4, OPEN);
        // Same for the 1s frame: 600 elapsed 1s buckets, ONE bar.
        let s1: Vec<&SealRow> = seals.iter().filter(|r| r.3 == TfIndex::S1).collect();
        assert_eq!(s1.len(), 1, "600 elapsed 1s buckets must emit ONE bar");
        // And 1d never crossed a boundary at all.
        assert!(
            !seals.iter().any(|r| r.3 == TfIndex::D1),
            "the 1d bucket did not close — it must emit nothing"
        );
    }

    #[test]
    fn test_multi_tf_aggregator_force_seal_all_emits_nothing_for_untouched_state() {
        let mut agg = MultiTfAggregator::default();
        // No instruments at all.
        let mut count = 0;
        assert_eq!(agg.force_seal_all(|_, _, _, _, _| count += 1), 0);
        assert_eq!(count, 0);
        // An instrument that only ever received an OUT-OF-SESSION tick has a
        // slot? No — the gate returns before slot allocation.
        // 2026-08-28: `OPEN - 60` (09:14) is now IN session - it is a
        // pre-open auction minute and folds into a real 09:14 candle. The
        // out-of-session case moved one minute earlier than the CANDLE open.
        let pre_open = CANDLE_OPEN - 60;
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, pre_open, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(stats.out_of_session);
        assert_eq!(agg.len(), 0, "a gated tick must not allocate a slot");
        assert_eq!(agg.force_seal_all(|_, _, _, _, _| count += 1), 0);
    }

    #[test]
    fn test_multi_tf_aggregator_force_seal_all_drains_every_open_timeframe_once() {
        let mut agg = MultiTfAggregator::default();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        let mut seals: Vec<SealRow> = Vec::new();
        let emitted = agg.force_seal_all(|f, s, g, tf, st| seals.push(row(f, s, g, tf, st)));
        assert_eq!(emitted, TF_COUNT, "one bar per opened timeframe");
        assert_eq!(seals.len(), TF_COUNT);
        // Idempotent: a second flush emits NOTHING (never a duplicate row).
        assert_eq!(agg.force_seal_all(|_, _, _, _, _| {}), 0);
    }

    #[test]
    fn test_multi_tf_aggregator_catch_up_seal_all_closes_only_ended_buckets() {
        let mut agg = MultiTfAggregator::default();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 5, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        // Cutoff one second into the session: only the 1s..5s frames whose
        // bucket already ended can close; the 1m frame cannot.
        let mut sealed_tfs: Vec<TfIndex> = Vec::new();
        let n = agg.catch_up_seal_all(OPEN + 6, |_, _, _, tf, _| sealed_tfs.push(tf));
        assert_eq!(n, sealed_tfs.len());
        assert!(
            !sealed_tfs.contains(&TfIndex::M1),
            "a 1m bucket ending at OPEN+60 must NOT seal at cutoff OPEN+6"
        );
        assert!(
            sealed_tfs.contains(&TfIndex::S1),
            "the 1s bucket [OPEN+5, OPEN+6) has ended and must seal"
        );
        // Push the cutoff past the 1m bucket end — now it closes.
        let mut later: Vec<TfIndex> = Vec::new();
        let _ = agg.catch_up_seal_all(OPEN + 60, |_, _, _, tf, _| later.push(tf));
        assert!(later.contains(&TfIndex::M1));
    }

    /// I-P1-11 + the 2026-06-19 feed-in-key lock, in one test.
    ///
    /// `security_id = 27` is the real collision Dhan shipped: FINNIFTY on
    /// `IDX_I` and a different instrument on `NSE_EQ`. Add a second feed
    /// observing the same instrument and there are THREE distinct fold
    /// states. Any two of them merging is silent data corruption.
    #[test]
    fn test_multi_tf_aggregator_composite_key_separates_segment_and_feed_collisions() {
        let mut agg = MultiTfAggregator::default();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(27, SEG_IDX, OPEN, 100.0, 10),
            None,
            |_, _, _, _, _| {},
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(27, SEG_EQ, OPEN, 200.0, 20),
            None,
            |_, _, _, _, _| {},
        );
        let _ = agg.consume_tick(
            Feed::Truedata,
            &tick(27, SEG_IDX, OPEN, 300.0, 30),
            None,
            |_, _, _, _, _| {},
        );

        assert_eq!(agg.len(), 3, "three distinct identities, three slots");
        let idx_dhan = agg
            .snapshot(Feed::Dhan, 27, SEG_IDX, TfIndex::M1)
            .expect("dhan/idx");
        let eq_dhan = agg
            .snapshot(Feed::Dhan, 27, SEG_EQ, TfIndex::M1)
            .expect("dhan/eq");
        let idx_groww = agg
            .snapshot(Feed::Truedata, 27, SEG_IDX, TfIndex::M1)
            .expect("groww/idx");
        assert_eq!(idx_dhan.close, 100.0);
        assert_eq!(eq_dhan.close, 200.0);
        assert_eq!(idx_groww.close, 300.0);
        // Each fold saw exactly ONE tick — nothing bled across identities.
        for s in [idx_dhan, eq_dhan, idx_groww] {
            assert_eq!(s.tick_count, 1);
            assert_eq!(s.high, s.low, "a single tick has high == low");
        }
    }

    /// Dhan LTT is SECOND-granular, so many ticks legitimately share one
    /// timestamp for one instrument. Not one of them may be collapsed.
    #[test]
    fn test_multi_tf_aggregator_folds_every_tick_that_shares_one_second() {
        let mut agg = MultiTfAggregator::default();
        let prices = [100.0_f32, 104.0, 96.0, 101.0, 99.0, 103.0, 97.0];
        for (i, p) in prices.iter().enumerate() {
            let cum = u32::try_from(i + 1).expect("small");
            let stats = agg.consume_tick(
                Feed::Dhan,
                &tick(13, SEG_IDX, OPEN, *p, cum),
                None,
                |_, _, _, _, _| {},
            );
            assert!(stats.folded(), "tick {i} must fold");
            assert_eq!(stats.sealed_count, 0, "same second seals nothing");
            assert_eq!(stats.late_count, 0, "same second is never late");
        }
        // Even the finest frame (1s) keeps them all in ONE bucket.
        for tf in [TfIndex::S1, TfIndex::M1, TfIndex::D1] {
            let s = agg.snapshot(Feed::Dhan, 13, SEG_IDX, tf).expect("slot");
            assert_eq!(
                s.tick_count,
                u32::try_from(prices.len()).expect("small"),
                "{tf:?} lost a same-second tick"
            );
            assert_eq!(s.open, 100.0, "{tf:?} open");
            assert_eq!(s.high, 104.0, "{tf:?} high");
            assert_eq!(s.low, 96.0, "{tf:?} low");
            assert_eq!(s.close, 97.0, "{tf:?} close is the LAST arrival");
        }
    }

    /// Interleaving two instruments must be indistinguishable from running
    /// each alone — the property that proves no state is shared across slots.
    #[test]
    fn test_multi_tf_aggregator_interleaved_instruments_match_isolated_runs() {
        // 40 ticks spanning three 1m buckets, two instruments, distinct prices.
        let script_a: Vec<ParsedTick> = (0..40_u32)
            .map(|i| tick(13, SEG_IDX, OPEN + i * 5, 100.0 + (i % 7) as f32, i + 1))
            .collect();
        let script_b: Vec<ParsedTick> = (0..40_u32)
            .map(|i| {
                tick(
                    25,
                    SEG_EQ,
                    OPEN + i * 5,
                    500.0 - (i % 11) as f32,
                    (i + 1) * 3,
                )
            })
            .collect();

        let run = |ticks: &[&ParsedTick]| -> (Vec<SealRow>, Vec<(TfIndex, LiveCandleState)>) {
            let mut agg = MultiTfAggregator::default();
            let mut seals: Vec<SealRow> = Vec::new();
            for t in ticks {
                let _ = agg.consume_tick(Feed::Dhan, t, None, |f, s, g, tf, st| {
                    seals.push(row(f, s, g, tf, st));
                });
            }
            let mut finals: Vec<(TfIndex, LiveCandleState)> = Vec::new();
            let _ = agg.force_seal_all(|_, _, _, tf, st| finals.push((tf, st)));
            (seals, finals)
        };

        let only_a: Vec<&ParsedTick> = script_a.iter().collect();
        let only_b: Vec<&ParsedTick> = script_b.iter().collect();
        let (seals_a, finals_a) = run(&only_a);
        let (seals_b, finals_b) = run(&only_b);

        // Interleaved: A, B, A, B, …
        let mut mixed: Vec<&ParsedTick> = Vec::new();
        for i in 0..script_a.len() {
            mixed.push(&script_a[i]);
            mixed.push(&script_b[i]);
        }
        let (seals_mixed, finals_mixed) = run(&mixed);

        let mixed_a: Vec<SealRow> = seals_mixed.iter().filter(|r| r.1 == 13).copied().collect();
        let mixed_b: Vec<SealRow> = seals_mixed.iter().filter(|r| r.1 == 25).copied().collect();
        assert_eq!(
            mixed_a, seals_a,
            "instrument A's bars changed when B was interleaved"
        );
        assert_eq!(
            mixed_b, seals_b,
            "instrument B's bars changed when A was interleaved"
        );
        assert_eq!(
            finals_mixed.len(),
            finals_a.len() + finals_b.len(),
            "the flush must cover both instruments"
        );
        assert!(!seals_a.is_empty(), "the script must actually seal bars");
    }

    #[test]
    fn test_multi_tf_aggregator_consume_tick_refuses_nan_and_nonpositive_prices() {
        let mut agg = MultiTfAggregator::default();
        // Open a healthy bucket first.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        let before = agg
            .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
            .expect("slot");
        // 2026-08-20: `0.0` stays in this loop and keeps EVERY guarantee the
        // test was written for — it must not fold, must not seal, and must
        // leave the bucket byte-identical. What changed is only its NAME:
        // corruption (`refused_price`) versus the vendor's documented
        // "has not traded yet" sentinel (`untraded_sentinel`), which the
        // caller keeps a row for. Asserting the split here rather than
        // dropping the case makes this test stronger: it now pins that the
        // two are told apart AND that both are equally harmless to state.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -5.0] {
            let stats = agg.consume_tick(
                Feed::Dhan,
                &tick(13, SEG_IDX, OPEN + 1, bad, 2),
                None,
                |_, _, _, _, _| panic!("a refused tick must never seal"),
            );
            if bad == 0.0 {
                assert!(
                    stats.untraded_sentinel,
                    "0.0 is the not-traded-yet sentinel, not corruption"
                );
                assert!(
                    !stats.refused_price,
                    "0.0 must not be classed as corruption — that discarded the row"
                );
            } else {
                assert!(stats.refused_price, "price {bad} must be refused");
                assert!(
                    !stats.untraded_sentinel,
                    "price {bad} is corruption, not a sentinel"
                );
            }
            assert!(!stats.folded());
        }
        let after = agg
            .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
            .expect("slot");
        assert_eq!(after, before, "a refused tick must leave state untouched");
        assert!(after.high.is_finite() && after.low.is_finite());
    }

    /// The slot table may realloc AT MOST ONCE for the process lifetime.
    ///
    /// # The measured relationship this pins
    ///
    /// `dhan_feed_stack` pre-sizes the fold from `distinct_fold_slots` over
    /// the instrument sets it holds AT BOOT — the spot universe, measured at
    /// ~865 distinct instruments (that file's own live capture line reads
    /// "tracked: 865"). The ~22,000 option/future contracts attach LATER, and the per-minute ATM re-fit adds more after that; the measured
    /// live peak is 22,996 subscribed instruments. The SAME file already
    /// records this about the detector, which it deliberately sizes at
    /// `AGGREGATOR_MAX_SLOTS` instead: "the universe grows ~26x after boot
    /// when contracts attach". The fold was left on the boot count, so plain
    /// `Vec` doubling ran five reallocs mid-session on the frame-drain task,
    /// the last memmoving ~13,900 slots (~75 MB at ~5.4 KB/slot).
    ///
    /// The boot site is not editable from this crate, so the guarantee is made
    /// here: the first growth reserves the whole remaining ceiling, after
    /// which `len == capacity` is unreachable because the exhaustion check
    /// refuses first.
    #[test]
    fn the_slot_table_reallocs_at_most_once_when_the_universe_outgrows_the_boot_pre_size() {
        // Same SHAPE as production, scaled down so the test is fast: a boot
        // pre-size far below the session peak.
        const BOOT_PRE_SIZE: usize = 8;
        const SESSION_PEAK: usize = 500;
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, BOOT_PRE_SIZE);
        agg.force_capacity_for_test(SESSION_PEAK);

        let mut capacity_changes = 0usize;
        let mut last_capacity = agg.slots.capacity();
        for sid in 0..SESSION_PEAK as u64 {
            let stats = agg.consume_tick(
                Feed::Dhan,
                &tick(sid + 1, SEG_IDX, OPEN, 100.0, 1),
                None,
                |_, _, _, _, _| {},
            );
            assert!(
                stats.folded(),
                "sid {sid} must get a slot below the ceiling"
            );
            if agg.slots.capacity() != last_capacity {
                capacity_changes += 1;
                last_capacity = agg.slots.capacity();
            }
        }
        assert_eq!(agg.len(), SESSION_PEAK);
        assert_eq!(
            capacity_changes, 1,
            "the slot table must grow exactly once — to the ceiling — no matter how far the \
             session peak exceeds the boot pre-size; {capacity_changes} reallocs means a \
             multi-megabyte memmove landed on the frame-drain task"
        );
        assert!(
            agg.slots.capacity() >= SESSION_PEAK,
            "the single growth must reach the ceiling, not a doubling step short of it"
        );
    }

    /// The one growth must land at the SMALLEST n the table will ever have.
    ///
    /// Reserving to the ceiling is only cheap because it happens on the very
    /// first slot past the boot pre-size, when there are ~865 slots (~4.7 MB)
    /// to move. If it were deferred, the same single realloc would move an
    /// arbitrarily larger table.
    #[test]
    fn the_single_growth_happens_on_the_first_slot_past_the_pre_size() {
        const BOOT_PRE_SIZE: usize = 4;
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, BOOT_PRE_SIZE);
        agg.force_capacity_for_test(64);
        let pre_size_capacity = agg.slots.capacity();

        for sid in 0..BOOT_PRE_SIZE as u64 {
            agg.consume_tick(
                Feed::Dhan,
                &tick(sid + 1, SEG_IDX, OPEN, 100.0, 1),
                None,
                |_, _, _, _, _| {},
            );
        }
        assert_eq!(
            agg.slots.capacity(),
            pre_size_capacity,
            "filling the pre-size must not realloc at all"
        );

        agg.consume_tick(
            Feed::Dhan,
            &tick(9_999, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(
            agg.slots.capacity() >= 64,
            "the first slot past the pre-size must reserve the whole remaining ceiling"
        );
    }

    #[test]
    fn test_multi_tf_aggregator_gates_ticks_outside_the_candle_session() {
        let mut agg = MultiTfAggregator::default();
        // 2026-08-28: `CANDLE_OPEN - 1` (08:59:59) replaces `OPEN - 1`
        // (09:14:59) as the last gated second - the fifteen pre-open minutes
        // between them are now captured, which is the point of the change.
        for ts in [CANDLE_OPEN - 1, DAY, DAY + 56_400, DAY + 86_399] {
            let stats = agg.consume_tick(
                Feed::Dhan,
                &tick(13, SEG_IDX, ts, 100.0, 1),
                None,
                |_, _, _, _, _| {
                    panic!("an out-of-session tick must never seal");
                },
            );
            assert!(stats.out_of_session, "ts {ts} must be gated");
            assert!(!stats.folded());
        }
        // The first in-session second IS accepted - 09:00:00, the first
        // second of the NSE pre-open call auction.
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, CANDLE_OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(stats.folded());
    }

    /// A price of EXACTLY zero is the vendor's "has not traded yet" sentinel,
    /// not corruption — and the two must not share a verdict.
    ///
    /// The live box refused ~22,000 ticks a session on this shape: option
    /// contracts that had not traded, swept in with NaN by a `p > 0.0` gate.
    /// The candle refusal is right (a zero would corrupt the OHLC). Losing the
    /// ROW is not: the packet still carries open interest, bid/ask and
    /// timestamps, and without it "did not trade" is indistinguishable from
    /// "was not captured".
    #[test]
    fn zero_price_is_a_sentinel_and_nan_is_corruption() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 4);

        let zero = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 0.0, 0),
            None,
            |_, _, _, _, _| {},
        );
        assert!(
            zero.untraded_sentinel,
            "an exact 0.0 must classify as the untraded sentinel"
        );
        assert!(
            !zero.refused_price,
            "0.0 must NOT be lumped in with corruption — that is what discarded the row"
        );
        assert!(!zero.folded(), "and it still must not fold into a candle");
        assert_eq!(zero.sealed_count, 0, "no bucket may be touched by a zero");

        for corrupt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            let s = agg.consume_tick(
                Feed::Dhan,
                &tick(13, SEG_IDX, OPEN, corrupt, 0),
                None,
                |_, _, _, _, _| {},
            );
            assert!(
                s.refused_price,
                "{corrupt} is corruption and must refuse the whole tick"
            );
            assert!(
                !s.untraded_sentinel,
                "{corrupt} is not a 'has not traded' sentinel"
            );
        }

        // A real price still folds — otherwise the two arms above could be
        // passing because nothing folds at all.
        let good = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(good.folded(), "a real price must still fold");
    }
    /// `folded()` reports success by the ABSENCE of every refusal flag, so a
    /// newly-added refusal that is not wired into it makes a refused tick
    /// claim it folded — a false-OK.
    ///
    /// The exhaustive destructure below is the mechanical half: adding a
    /// field to `ConsumeStats` fails to COMPILE here until it is listed,
    /// which forces whoever adds it to decide whether it belongs in
    /// `folded()`. A plain list of assertions would silently stay green.
    #[test]
    fn test_every_refusal_field_makes_folded_false() {
        let ConsumeStats {
            sealed_count: _,
            amended_count: _,
            late_count: _,
            refused_price: _,
            out_of_session: _,
            slot_exhausted: _,
            refused_timestamp: _,
            untraded_sentinel: _,
            stale_trading_day: _,
            future_trading_day: _,
            untraded_timestamp: _,
            out_of_band_timestamp: _,
        } = ConsumeStats::default();

        assert!(
            ConsumeStats::default().folded(),
            "a clean default must count as folded, or the checks below are vacuous"
        );

        for (name, stats) in [
            (
                "refused_price",
                ConsumeStats {
                    refused_price: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "out_of_session",
                ConsumeStats {
                    out_of_session: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "slot_exhausted",
                ConsumeStats {
                    slot_exhausted: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "refused_timestamp",
                ConsumeStats {
                    refused_timestamp: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "untraded_sentinel",
                ConsumeStats {
                    untraded_sentinel: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "stale_trading_day",
                ConsumeStats {
                    stale_trading_day: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "future_trading_day",
                ConsumeStats {
                    future_trading_day: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "untraded_timestamp",
                ConsumeStats {
                    untraded_timestamp: true,
                    ..ConsumeStats::default()
                },
            ),
            (
                "out_of_band_timestamp",
                ConsumeStats {
                    out_of_band_timestamp: true,
                    ..ConsumeStats::default()
                },
            ),
        ] {
            assert!(
                !stats.folded(),
                "{name} is set but folded() still reports success — a refused \
                 tick would be counted as captured"
            );
        }
    }

    /// ADVERSARIAL REGRESSION (2026-08-09, security review, HIGH).
    ///
    /// A hostile or malformed packet carrying an all-ones LTT must not be able
    /// to shove the event-time watermark into the far future. Before the fix
    /// the advance ran ahead of every gate, so one such packet — refused for
    /// folding — still set the watermark to ~4.29 billion, and the next
    /// catch-up cycle force-sealed every open bucket in the entire book with
    /// incomplete OHLCV. Silent, whole-book, and unrecoverable (the watermark

    /// BITE TEST (2026-08-25) — the sentinel bypass of the timestamp band.
    ///
    /// The band check used to sit BELOW the `p == 0.0` untraded-sentinel
    /// return, so a packet carrying LTP = 0 AND a poison timestamp never
    /// reached it. `refused_timestamp` stayed false, and the drain classifies
    /// `untraded_sentinel` as a CANDLE-ONLY refusal — meaning the row was still
    /// written to `ticks`, with the poison value as its DESIGNATED timestamp.
    ///
    /// The sibling test above covers a SANE price with a poison timestamp; this
    /// covers the combination that slipped through. Moving the band check back
    /// below the sentinel return makes this fail.
    #[test]
    fn an_untraded_sentinel_with_a_poison_timestamp_is_refused_outright() {
        let mut agg = MultiTfAggregator::default();
        for poison in [u32::MAX, MAX_PLAUSIBLE_EXCHANGE_TS_SECS + 1, 0, 1] {
            let stats = agg.consume_tick(
                Feed::Dhan,
                // price 0.0 — the documented "untraded" sentinel.
                &tick(13, SEG_IDX, poison, 0.0, 1),
                None,
                |_, _, _, _, _| panic!("an implausible timestamp must never seal"),
            );
            assert!(
                stats.refused_timestamp,
                "ts {poison} with an untraded price must be refused as \
                 IMPLAUSIBLE, not merely as an untraded sentinel — the drain \
                 treats the sentinel as a candle-only refusal and still writes \
                 the row"
            );
            assert!(
                !stats.untraded_sentinel,
                "the timestamp is the more serious defect and must be the \
                 reported reason; classifying it as a sentinel is what let the \
                 row through"
            );
            assert_eq!(
                agg.watermark_secs(),
                0,
                "and it must never move the watermark"
            );
        }
    }
    /// never regresses).
    #[test]
    fn test_watermark_cannot_be_poisoned_by_an_all_ones_timestamp() {
        let mut agg = MultiTfAggregator::default();

        // Establish a normal watermark from a legitimate in-session tick.
        agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 60, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        let honest = agg.watermark_secs();
        assert_eq!(honest, OPEN + 60);

        // The poison packets: sane price, garbage timestamps at both ends.
        for poison in [u32::MAX, MAX_PLAUSIBLE_EXCHANGE_TS_SECS + 1, 0, 1] {
            let stats = agg.consume_tick(
                Feed::Dhan,
                &tick(13, SEG_IDX, poison, 100.0, 2),
                None,
                |_, _, _, _, _| panic!("an implausible timestamp must never seal"),
            );
            assert!(
                stats.refused_timestamp,
                "ts {poison} must be refused as implausible"
            );
            assert!(!stats.folded());
            assert_eq!(
                agg.watermark_secs(),
                honest,
                "ts {poison} moved the watermark — one crafted packet would \
                 then force-seal the entire book on the next catch-up"
            );
        }
    }

    /// A tick refused for an insane price must not move the watermark at all.
    #[test]
    fn test_watermark_does_not_advance_on_a_price_refused_tick() {
        let mut agg = MultiTfAggregator::default();
        agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 60, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        let before = agg.watermark_secs();

        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 120, f32::NAN, 2),
            None,
            |_, _, _, _, _| {},
        );
        assert!(stats.refused_price, "NaN price must be refused");
        assert_eq!(
            agg.watermark_secs(),
            before,
            "a refused tick must not advance the watermark"
        );
    }

    /// The post-close advance is LOAD-BEARING and must survive the fix: the
    /// watermark still moves past the session end so the final bar of the day
    /// becomes catch-up-sealable. Non-vacuity for the two tests above — they
    /// must not have been satisfied by simply never advancing.
    #[test]
    fn test_watermark_still_advances_past_session_close_for_the_final_seal() {
        let mut agg = MultiTfAggregator::default();
        agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 60, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        let post_close = DAY + 56_500; // past the 15:40 session upper bound
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, post_close, 100.0, 2),
            None,
            |_, _, _, _, _| {},
        );
        assert!(stats.out_of_session, "post-close tick is gated for folding");
        assert_eq!(
            agg.watermark_secs(),
            post_close,
            "but it MUST still advance the watermark, or the final session \
             bar never becomes catch-up-sealable"
        );
    }

    #[test]
    fn test_multi_tf_aggregator_slot_exhaustion_fails_closed_and_slots_exhausted_total_counts() {
        // A 2-slot table proves the behaviour without allocating 25,000 cells.
        let mut agg = MultiTfAggregator::default();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(1, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(2, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert_eq!(agg.len(), 2);
        // Simulate the ceiling by asserting the guard's own arithmetic: the
        // real ceiling is a const, so drive it through the private path.
        agg.force_capacity_for_test(2);
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(3, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {
                panic!("an exhausted slot table must never seal");
            },
        );
        assert!(stats.slot_exhausted, "must fail CLOSED");
        assert!(!stats.folded());
        assert_eq!(agg.len(), 2, "the table must not grow past capacity");
        assert_eq!(agg.slots_exhausted_total(), 1, "the drop must be counted");
        // An ALREADY-KNOWN instrument still folds — exhaustion refuses only
        // NEW identities, it never breaks the ones already tracked.
        let ok = agg.consume_tick(
            Feed::Dhan,
            &tick(1, SEG_IDX, OPEN + 1, 101.0, 2),
            None,
            |_, _, _, _, _| {},
        );
        assert!(ok.folded());
        // A second refusal counts again but logs only once (latch).
        let stats2 = agg.consume_tick(
            Feed::Dhan,
            &tick(4, SEG_IDX, OPEN, 100.0, 1),
            None,
            |_, _, _, _, _| {},
        );
        assert!(stats2.slot_exhausted);
        assert_eq!(agg.slots_exhausted_total(), 2);
    }

    #[test]
    fn test_multi_tf_aggregator_consume_tick_into_ring_buffers_every_seal() {
        let mut agg = MultiTfAggregator::default();
        let mut ring = SealRing::with_capacity(64);
        let mut evicted = 0_usize;
        let _ = agg.consume_tick_into_ring(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            &mut ring,
            |_| evicted += 1,
        );
        assert_eq!(ring.len(), 0, "the first tick seals nothing");
        // Cross the 1m boundary: every sub-minute frame plus 1m seals.
        let stats = agg.consume_tick_into_ring(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 60, 105.0, 2),
            None,
            &mut ring,
            |_| evicted += 1,
        );
        assert!(stats.sealed_count > 0);
        assert_eq!(
            ring.len(),
            usize::from(stats.sealed_count),
            "every sealed bar must reach the ring"
        );
        assert_eq!(evicted, 0, "a 64-deep ring must not evict here");
        let seal = ring.pop_oldest().expect("a buffered seal");
        assert_eq!(seal.security_id, 13);
        assert_eq!(seal.exchange_segment_code, SEG_IDX);
        assert_eq!(seal.feed, Feed::Dhan);
    }

    #[test]
    fn test_multi_tf_aggregator_consume_tick_into_ring_hands_back_evictions() {
        let mut agg = MultiTfAggregator::default();
        // Capacity 1 forces the drop-oldest path immediately.
        let mut ring = SealRing::with_capacity(1);
        let mut evicted: Vec<BufferedSeal> = Vec::new();
        let _ = agg.consume_tick_into_ring(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 1),
            None,
            &mut ring,
            |s| evicted.push(s),
        );
        let stats = agg.consume_tick_into_ring(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 60, 105.0, 2),
            None,
            &mut ring,
            |s| evicted.push(s),
        );
        assert!(stats.sealed_count >= 2, "several frames cross at OPEN+60");
        assert_eq!(
            evicted.len(),
            usize::from(stats.sealed_count) - 1,
            "every seal beyond the ring's capacity must be handed back, never dropped"
        );
    }

    #[test]
    fn test_multi_tf_aggregator_cumulative_volume_override_is_not_truncated() {
        let mut agg = MultiTfAggregator::default();
        // A cumulative that overflows u32 — the exact truncation class the
        // explicit u64 argument exists to prevent.
        let big: u64 = u64::from(u32::MAX) + 5_000;
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN, 100.0, 0),
            Some(big),
            |_, _, _, _, _| {},
        );
        let s = agg
            .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
            .expect("slot");
        // AMENDED 2026-08-25. This asserted `s.volume == big` — i.e. that the
        // slot's very first bar publishes the ENTIRE day's cumulative as its
        // own volume. That is the mid-session-slot defect measured live on
        // 2026-08-24 (intraday frames ~9.2x the day bar), and the test was
        // pinning it as correct. The first tick now SEEDS the baseline, so the
        // first bar reports 0 and the unattributable volume is counted by
        // `tv_aggregator_slot_volume_baseline_seeded_total` rather than
        // invented.
        //
        // The test's REAL intent — a u64 cumulative must not be truncated
        // through the `u32` `tick.volume` field — is unchanged and is now
        // carried by the `+ 250` assertion below, which can only hold if the
        // baseline retained all 64 bits of `big`.
        assert_eq!(
            s.volume, 0,
            "the slot's first bar cannot own pre-arrival volume"
        );
        assert!(
            big > u64::from(u32::MAX),
            "fixture must exceed the u32 range"
        );
        // The next bucket baselines off the SAME u64 value.
        let mut sealed: Vec<SealRow> = Vec::new();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 60, 101.0, 0),
            Some(big + 250),
            |f, sid, g, tf, st| sealed.push(row(f, sid, g, tf, st)),
        );
        assert!(!sealed.is_empty(), "the boundary crossing must seal bars");
        let next = agg
            .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
            .expect("slot");
        assert_eq!(next.volume, 250, "incremental volume off the u64 baseline");
    }

    #[test]
    fn test_consume_stats_folded_is_false_for_every_refusal_reason() {
        assert!(ConsumeStats::default().folded());
        for s in [
            ConsumeStats {
                refused_price: true,
                ..ConsumeStats::default()
            },
            ConsumeStats {
                out_of_session: true,
                ..ConsumeStats::default()
            },
            ConsumeStats {
                slot_exhausted: true,
                ..ConsumeStats::default()
            },
        ] {
            assert!(!s.folded(), "{s:?} must not report as folded");
        }
    }
    /// MEASUREMENT (not a CI gate -- `#[ignore]`d, run on demand):
    /// what does the 5-second `catch_up_seal_all` sweep actually cost at the
    /// authorized 25,000-instrument ceiling?
    ///
    /// CLAUDE.md's O(1) table recorded this path as O(slots x TF_COUNT) with no
    /// early exit, running on the frame drain's OWN task, and stated plainly
    /// that it was "UNMEASURED at the 25,000-instrument target". This turns
    /// that into a number. Ignored rather than asserted because a wall-clock
    /// bound on a shared CI runner is a flake, and a flaky gate is worse than
    /// none.
    ///
    /// RESULT, `--release`, x86 dev container, two runs a day apart:
    ///
    /// ```text
    /// 2026-08-21:  600000 cells, 0 sealed,  9.67ms (16.1 ns/cell)
    /// 2026-08-22:  600000 cells, 0 sealed, 10.14ms (16.9 ns/cell)
    /// ```
    ///
    /// Both are recorded rather than one, because a single figure written into
    /// a document invites the next reader to treat run-to-run variance as
    /// drift. CLAUDE.md carries the 2026-08-21 number; this is the same
    /// measurement, ~5% apart on a shared container.
    ///
    /// Against the 5,000 ms cadence that is **0.20% of the interval**, so the
    /// sweep is not a threat to the drain at the authorized ceiling. Two
    /// qualifications keep that from being read as more than it is:
    ///
    /// 1. It is the PURE-TRAVERSAL shape -- cutoff in the past, zero seals --
    ///    which is what ~99% of sweeps do. A sweep that actually seals pays the
    ///    per-bar emit cost on top, and that cost scales with how many buckets
    ///    ended, not with slot count.
    /// 2. It was measured HERE, not on the box. Production is r8g.xlarge
    ///    (Graviton4); this figure is an order-of-magnitude answer, not a
    ///    per-instruction one. Re-run it on the box to claim otherwise.
    ///
    ///     cargo test -p tickvault-trading --release --lib \
    ///       catch_up_seal_all_sweep_cost -- --ignored --nocapture
    #[test]
    #[ignore = "measurement harness, not a gate — see doc comment"]
    fn catch_up_seal_all_sweep_cost_at_the_authorized_ceiling() {
        let cap = crate::candles::AGGREGATOR_MAX_SLOTS;
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, cap);
        // Populate every slot so the sweep visits the real worst case.
        for sid in 0..cap as u64 {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(sid, SEG_EQ, OPEN, 100.0, 1),
                None,
                |_, _, _, _, _| {},
            );
        }
        let slots = agg.len();
        // Cutoff far in the past: every cell is visited, none seals. This is
        // the pure traversal cost — the shape that runs on 99% of sweeps.
        let t0 = std::time::Instant::now();
        let emitted = agg.catch_up_seal_all(OPEN, |_, _, _, _, _| {});
        let elapsed = t0.elapsed();
        println!(
            "catch_up_seal_all: {slots} slots x {TF_COUNT} TF = {} cells, \
             {emitted} sealed, {elapsed:?} ({:.1} ns/cell)",
            slots * TF_COUNT,
            elapsed.as_nanos() as f64 / (slots * TF_COUNT).max(1) as f64
        );
    }

    /// MEASUREMENT (not a CI gate): what does the per-tick FOLD actually cost
    /// at the authorized 25,000-instrument ceiling?
    ///
    /// Every scaling document in this repo sizes MEMORY at 25,000 instruments
    /// and then says CPU is UNMEASURED — `websocket-connection-scope-lock.md`
    /// states it outright ("~12,500 packets/sec at the open × (decode +
    /// 24-timeframe fold + ILP append) has never run"). Memory fitting is not
    /// the same claim as the box keeping up, and the second one is what drops
    /// ticks. This turns the CPU half into a number.
    ///
    /// What it measures: `consume_tick` at FULL slot occupancy, round-robin
    /// across all 25,000 instruments with advancing timestamps, so the slot
    /// hash runs at its real load factor and real seals fire. What it does NOT
    /// measure: packet decode (separately DHAT-gated and fixed-offset), the
    /// ILP append, or the socket read — so the real per-tick budget is LARGER
    /// than this figure, and the headroom printed here is an UPPER bound on
    /// the fold's share, never a claim about the whole pipeline.
    ///
    /// `#[ignore]`d for the same reason as the sweep harness above: a
    /// wall-clock bound on a shared CI runner is a flake, and a flaky gate is
    /// worse than no gate. Run it deliberately, in RELEASE — a debug build
    /// measures the allocator and the bounds checks, not the design:
    ///
    ///     cargo test -p tickvault-trading --release \
    ///       fold_cost_at_the_authorized_ceiling -- --ignored --nocapture
    #[test]
    #[ignore = "measurement harness, not a gate — see doc comment"]
    fn fold_cost_at_the_authorized_ceiling() {
        let cap = crate::candles::AGGREGATOR_MAX_SLOTS;
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, cap);

        // Fill every slot first: a half-empty map is a friendlier hash than
        // the one production actually runs.
        for sid in 0..cap as u64 {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(sid, SEG_EQ, OPEN, 100.0, 1),
                None,
                |_, _, _, _, _| {},
            );
        }
        let slots = agg.len();

        // Drive ticks round-robin with a clock that advances, so buckets close
        // and seals fire — sealing is part of what a tick costs.
        const TICKS: usize = 250_000;
        let mut seals = 0usize;
        let t0 = std::time::Instant::now();
        for i in 0..TICKS {
            let sid = (i % cap) as u64;
            let ts = OPEN + (i / cap) as u32;
            let px = 100.0 + (i % 97) as f32 * 0.05;
            let stats = agg.consume_tick(
                Feed::Dhan,
                &tick(sid, SEG_EQ, ts, px, (i % 1000) as u32 + 1),
                None,
                |_, _, _, _, _| seals += 1,
            );
            std::hint::black_box(stats);
        }
        let elapsed = t0.elapsed();

        let ns_per_tick = elapsed.as_nanos() as f64 / TICKS as f64;
        let ticks_per_sec = 1_000_000_000.0 / ns_per_tick;
        // The open-burst envelope this repo sizes against.
        let envelope = 12_500.0;
        println!(
            "fold cost: {slots} slots, {TICKS} ticks, {seals} seals, {elapsed:?}\n  \
             {ns_per_tick:.1} ns/tick -> {ticks_per_sec:.0} ticks/sec on ONE core\n  \
             headroom vs the {envelope:.0}/sec open burst: {:.1}x (fold only; \
             decode + ILP append are NOT included)",
            ticks_per_sec / envelope
        );
    }
}

// -- the receipt clock, driven END TO END through a real fold ---------------
//
// ADDED 2026-08-28 after an adversarial sweep found the gap and named it
// precisely: `ParsedTick::default().received_at_nanos == 0`, and EVERY candle
// fixture in this workspace leaves it there. `fold_clock_ist_secs` returns
// early on that sentinel, so the entire suite exercised the EXCHANGE-clock
// FALLBACK and passed "by construction, not by agreement" — nothing drove a
// non-zero receipt through a bucket, a close guard, or a seal.
//
// The first draft of these tests ALSO carried a wrong premise, and writing
// them is what exposed it. It used a 100-minute-stale trade stamp on the
// belief that the receipt clock rescues a dormant contract from filing into a
// bar hours in the past. It does not, and cannot: the delta guard rejects any
// receipt more than `MAX_PLAUSIBLE_RECEIPT_LAG_SECS` past the trade, so that
// packet still buckets on its trade stamp. `test_a_stale_snapshot_still_
// buckets_on_its_trade_stamp` below pins that limit deliberately, because a
// limit nobody wrote down is how the next reader inherits the same wrong
// belief. What the receipt clock actually corrects is DELIVERY LAG inside the
// band — measured p50 1.4s, p99 46s on this feed — which is exactly where a
// minute boundary gets crossed on an ordinary day.
#[cfg(test)]
mod receipt_clock_end_to_end_tests {
    use super::tests::{CANDLE_OPEN, SEG_IDX, tick};
    use super::*;

    /// UTC nanos for an IST second — the conversion the fold clock inverts.
    fn receipt_nanos_for_ist(ist_secs: u32) -> i64 {
        (i64::from(ist_secs) - 19_800) * 1_000_000_000
    }

    fn seal_m1_bucket(agg: &mut MultiTfAggregator) -> Option<u32> {
        let mut at: Option<u32> = None;
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                at = Some(st.bucket_start_ist_secs);
            }
        });
        at
    }

    /// The ordinary day, and the reason this change exists: a trade printed in
    /// one minute and delivered in the next. Two seconds of lag — well inside
    /// the measured p50 — decide which bar the packet belongs to.
    #[test]
    fn delivery_lag_across_a_minute_boundary_files_the_bar_by_receipt() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        // Traded at 09:29:59, received at 09:30:01.
        let traded = CANDLE_OPEN + 30 * 60 - 1;
        let received = CANDLE_OPEN + 30 * 60 + 1;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(received);

        let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(received)),
            "a packet received at 09:30:01 belongs to the 09:30 bar"
        );
        assert_ne!(
            TfIndex::M1.bucket_start(received),
            TfIndex::M1.bucket_start(traded),
            "fixture must straddle a minute boundary or it proves nothing"
        );
    }

    /// THE LIMIT, pinned deliberately. A snapshot whose last trade was 100
    /// minutes ago is NOT re-dated to now — the delta guard refuses it and the
    /// exchange stamp wins. This is not a defect: with `received_at` still
    /// re-stamped at WAL replay, a large positive delta is indistinguishable
    /// from a replayed frame, and re-dating a replay to replay-time would
    /// destroy the bars it belongs to. Recorded as a test so the bound is a
    /// fact rather than a belief.
    #[test]
    fn a_stale_snapshot_still_buckets_on_its_trade_stamp() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        let traded = CANDLE_OPEN + 20 * 60;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(traded + 100 * 60);

        let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(traded)),
            "beyond MAX_PLAUSIBLE_RECEIPT_LAG_SECS the fold falls back to the \
             trade stamp — the receipt clock corrects delivery lag, it does \
             not re-date a stale snapshot"
        );
    }

    /// A WAL frame re-stamped at replay: the receipt reads 9 hours after the
    /// trade. That is a perfectly SANE epoch — an absolute plausibility band
    /// would wave it through — so the guard being on the DELTA rather than on
    /// the value is what catches it.
    #[test]
    fn a_replayed_frame_falls_back_to_the_trade_stamp() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        let traded = CANDLE_OPEN + 20 * 60;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(traded + 9 * 3_600);

        let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(traded)),
            "replayed frames must land in the bars they originally belonged to"
        );
    }

    /// Close ownership on the receipt clock: two packets in one minute, the
    /// EARLIER-traded one arriving LAST, both inside the trusted band. On the
    /// exchange clock the order guard would refuse the late arrival and the
    /// bar would keep the first price; on the receipt clock the last-received
    /// packet owns the close. That is the semantic the operator asked for,
    /// written as a test rather than asserted in a comment.
    #[test]
    fn the_close_is_owned_by_the_last_packet_we_received() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);
        let minute = TfIndex::M1.bucket_start(CANDLE_OPEN + 30 * 60);

        let mut first = tick(13, SEG_IDX, minute + 20, 100.0, 10);
        first.received_at_nanos = receipt_nanos_for_ist(minute + 25);
        let mut second = tick(13, SEG_IDX, minute + 5, 107.0, 20);
        second.received_at_nanos = receipt_nanos_for_ist(minute + 45);

        let _ = agg.consume_tick(Feed::Dhan, &first, None, |_, _, _, _, _| {});
        let _ = agg.consume_tick(Feed::Dhan, &second, None, |_, _, _, _, _| {});

        let mut close = f64::NAN;
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                close = st.close;
            }
        });
        assert!(
            (close - 107.0).abs() < 1e-9,
            "the LAST-RECEIVED packet owns the close (got {close}); on the \
             exchange clock the earlier-traded 107.0 would have been refused \
             by the order guard and the bar would have closed at 100.0"
        );
    }
}

// -- the out-of-band timestamp: candle refused, ROW KEPT ---------------------
//
// MEASURED on production 2026-08-27: this population was 2,008,916 ticks in a
// single session — 2.41% of all 83,446,729 decoded — and every one of them was
// HARD-refused, meaning no row was written at all. Not a missing candle: no
// record the instrument was even seen.
//
// It is the THIRD instance of one shape (price sentinel 2026-08-20, zero
// timestamp 2026-08-26, this), and the repetition is why these tests exist as
// behaviour pins rather than as comments.
#[cfg(test)]
mod out_of_band_timestamp_tests {
    use super::tests::{CANDLE_OPEN, SEG_IDX, tick};
    use super::*;

    /// The whole point: a real price with an unusable trade time must still
    /// reach the writer. `folded()` false means "not in a candle"; the drain
    /// reads `out_of_band_timestamp` to know the ROW is still safe to write.
    #[test]
    fn an_out_of_band_stamp_with_a_receipt_is_candle_only_not_a_hard_refusal() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        // Below the 2020 floor, but a real price and a real receipt.
        let mut t = tick(13, SEG_IDX, 1_000_000_000, 100.0, 10);
        t.received_at_nanos = (i64::from(CANDLE_OPEN) - 19_800) * 1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.out_of_band_timestamp,
            "an out-of-band stamp WITH a receipt must be the candle-only class"
        );
        assert!(
            !stats.refused_timestamp,
            "it must NOT be the hard refusal — that is what threw the row away"
        );
        assert!(
            !stats.folded(),
            "it is still not folded: an out-of-band second cannot be bucketed"
        );
    }

    /// The other half, and the reason the split is on the receipt rather than
    /// on the timestamp alone: with NO receipt there is no safe stamp, so the
    /// writer would put the row in a 1970 or year-2106 partition that
    /// retention and archival can never reach. That stays a hard refusal.
    #[test]
    fn an_out_of_band_stamp_without_a_receipt_stays_a_hard_refusal() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        let t = tick(13, SEG_IDX, 1_000_000_000, 100.0, 10);
        assert_eq!(t.received_at_nanos, 0, "fixture models the WAL-replay path");

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.refused_timestamp,
            "no receipt means no safe stamp — the row must NOT be written"
        );
        assert!(!stats.out_of_band_timestamp);
    }

    /// Non-vacuity. A normal in-band tick must be unaffected by either arm —
    /// without this the two tests above would pass on a fold that refused
    /// everything.
    #[test]
    fn an_in_band_stamp_is_untouched_by_the_split() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        let mut t = tick(13, SEG_IDX, CANDLE_OPEN + 60, 100.0, 10);
        t.received_at_nanos = (i64::from(CANDLE_OPEN) + 62 - 19_800) * 1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(!stats.out_of_band_timestamp);
        assert!(!stats.refused_timestamp);
        assert!(stats.folded(), "an ordinary tick must still fold");
    }

    #[test]
    fn last_ltp_is_none_before_any_accepted_tick_and_some_after() {
        // The two states a caller must be able to tell apart. `0.0` is a LIVE
        // sentinel on this feed -- Ticker-mode packets and pre-open instruments
        // both carry it -- so "no price yet" cannot be signalled as a zero.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        assert_eq!(
            agg.last_ltp(Feed::Dhan, 77, 2),
            None,
            "an instrument with no slot has no price"
        );

        let t = tick(77, 2, CANDLE_OPEN + 60, 101.25, 500);
        let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        let got = agg
            .last_ltp(Feed::Dhan, 77, 2)
            .expect("an accepted tick leaves a price");
        assert!(
            (got - 101.25).abs() < 1e-6,
            "expected the accepted price, got {got}"
        );
    }

    #[test]
    fn last_ltp_keys_on_the_composite_so_a_segment_collision_cannot_answer() {
        // I-P1-11: Dhan reuses the same numeric id across segments. Keyed on the
        // bare id, an index would answer with a same-numbered option's price.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let t = tick(27, 2, CANDLE_OPEN + 60, 55.5, 100);
        let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
        assert!(agg.last_ltp(Feed::Dhan, 27, 2).is_some());
        assert_eq!(
            agg.last_ltp(Feed::Dhan, 27, 0),
            None,
            "id 27 on IDX_I is a DIFFERENT instrument"
        );
    }
}

/// The DAY-GATE PERMUTATION SWEEP (2026-09-10).
///
/// The gate added on 2026-09-10 refuses a tick whose EXCHANGE day is not the
/// RECEIPT day. That is two clocks, two directions and a stand-down sentinel,
/// and it sits in the middle of a chain of five earlier refusals — so the
/// behaviour that matters is not one arm but the GRID: which arm wins, what
/// happens at each boundary, and what happens when a clock is not merely
/// wrong but absurd.
///
/// Every test here was written because the grid position was UNPINNED, not
/// because it was known broken. Three of them turned out to matter:
///
///   * the one-nanosecond receipt is exactly what broke seven drain tests on
///     the day the gate shipped — fixtures passing `1_000_000` were handing
///     the gate a 1970 receipt against a 2026 stamp;
///   * the `i64::MAX` receipt is the only input that could PANIC, because the
///     release profile sets `overflow-checks = true` and this arm does
///     arithmetic on a caller-supplied number;
///   * the ORDER tests pin that a corrupt price or an out-of-band second is
///     still judged by the arm that owns it, so a day mismatch can never
///     mask a harder fault or steal its counter.
#[cfg(test)]
mod day_gate_permutation_sweep {
    use super::tests::{DAY, SEG_IDX, tick};
    use super::*;

    /// IST seconds -> the UTC epoch nanos the drain would stamp for them.
    fn receipt_at_ist(ist_secs: i64) -> i64 {
        (ist_secs - crate::candles::tf_index::IST_UTC_OFFSET_SECS) * 1_000_000_000
    }

    const TODAY_0916: u32 = DAY + 9 * 3_600 + 16 * 60;

    // -- the stand-down sentinel, on the STALE side -------------------------

    /// The sentinel case is pinned for the FUTURE arm one module up; this is
    /// its mirror, and it is the one that actually runs in production. A
    /// pre-TVW3 WAL frame carries no receipt, and boot replay of a segment
    /// deferred at yesterday's shutdown is PRIOR-DAY by construction — so if
    /// the sentinel did not stand down here, every such replay would be
    /// refused and the deferred backlog would never fold.
    ///
    /// The watermark gate below it is what judges those frames, exactly as it
    /// did before this change.
    #[test]
    fn a_prior_day_frame_with_no_receipt_is_left_to_the_watermark() {
        let mut agg = MultiTfAggregator::default();
        let yesterday = DAY - 86_400 + 33_400;
        let t = tick(13, SEG_IDX, yesterday, 100.0, 1);
        assert_eq!(t.received_at_nanos, 0, "fixture must exercise the sentinel");

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            !stats.stale_trading_day,
            "with no second clock the receipt gate must stand down — refusing \
             here would strand every pre-TVW3 boot replay, which is prior-day \
             by construction"
        );
    }

    /// A NEGATIVE receipt is garbage, not a clock, and it is treated as the
    /// sentinel rather than as a 1970 instant.
    ///
    /// The distinction is not academic: `receipt_ist_secs / 86_400` truncates
    /// TOWARD ZERO in Rust, so a negative receipt would compute day 0 for
    /// anything inside the first 86,400 seconds before the epoch and slide the
    /// comparison silently. Standing down is the only answer that cannot be
    /// subtly wrong, and the `> 0` guard is what delivers it.
    #[test]
    fn a_negative_receipt_clock_stands_the_gate_down_rather_than_guessing() {
        let mut agg = MultiTfAggregator::default();
        let mut t = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
        t.received_at_nanos = -1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            !stats.stale_trading_day && !stats.future_trading_day,
            "a negative receipt is not a clock; the gate must not judge a day \
             against it in either direction"
        );
    }

    // -- absurd-but-positive clocks ----------------------------------------

    /// THE BUG THAT BROKE SEVEN TESTS on the day this gate shipped.
    ///
    /// Six drain fixtures passed `received_at_nanos = 1_000_000` — one
    /// millisecond after the epoch — as a "don't care" value, because before
    /// the gate nothing read it. Against a live 2026 exchange stamp that is a
    /// receipt fifty-six years in the past, so the stamp is FUTURE-dated by
    /// fifty-six years and refused. `folded` went to 0 and the fixtures failed
    /// on assertions about frame-walk accounting, which is a symptom miles
    /// from the cause.
    ///
    /// Pinned so the next reader who sees `folded: 0` in a drain test has the
    /// answer in a test name instead of an afternoon.
    #[test]
    fn a_one_nanosecond_receipt_reads_a_live_stamp_as_future_dated() {
        let mut agg = MultiTfAggregator::default();
        let mut t = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
        t.received_at_nanos = 1;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.future_trading_day,
            "a 1970 receipt against a 2026 stamp IS a future-dated tick, and \
             the gate is right to say so — the fixture was wrong, not the gate"
        );
    }

    /// The only input that could PANIC: `overflow-checks = true` is set on the
    /// release profile, and this arm divides and ADDS to a caller-supplied
    /// `i64`. `i64::MAX / 1_000_000_000` is 9,223,372,036, and adding the
    /// 19,800-second IST offset stays nine orders of magnitude below the
    /// ceiling — so the arithmetic is safe by construction rather than by
    /// luck, and this test is what keeps it that way if the offset ever moves
    /// or the division is removed.
    #[test]
    fn a_receipt_clock_at_the_i64_ceiling_refuses_without_overflowing() {
        let mut agg = MultiTfAggregator::default();
        let mut t = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
        t.received_at_nanos = i64::MAX;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.stale_trading_day,
            "a receipt at the end of time makes every real stamp stale — the \
             verdict is unhelpful but it must be a VERDICT, never a panic on \
             the per-tick path"
        );
    }

    /// The measured worst case, from the vendor's own behaviour: Dhan sends
    /// LAST TRADE TIME, and a dormant contract's connect snapshot was measured
    /// at a maximum of 34 days stale. That is the top of the envelope this
    /// gate exists to refuse.
    #[test]
    fn the_measured_thirty_four_day_maximum_stale_snapshot_is_refused() {
        let mut agg = MultiTfAggregator::default();
        let thirty_four_days_ago = DAY - 34 * 86_400 + 33_400;
        let mut t = tick(66_422, SEG_IDX, thirty_four_days_ago, 142.50, 12_000);
        t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.stale_trading_day,
            "the measured 34-day maximum must be refused — this is the top of \
             the envelope, not a hypothetical"
        );
        assert!(
            agg.lookup(Feed::Dhan, 66_422, SEG_IDX).is_none(),
            "and it must not take a slot"
        );
    }

    // -- day boundaries -----------------------------------------------------

    /// One SECOND apart across IST midnight, far outside the trusted band, is
    /// a genuine day mismatch and is refused.
    ///
    /// This is the sharp edge of the rule and it is deliberate: the comparison
    /// is on DAYS, so a single second can flip it. It is safe because the
    /// trusted band already collapses a real straddle onto one clock (pinned
    /// by `a_tick_straddling_ist_midnight_inside_the_trusted_band_is_not_stale`)
    /// and because the candle session closes at 15:40 IST — nothing legitimate
    /// trades within a second of midnight.
    #[test]
    fn an_exchange_stamp_one_second_before_midnight_is_stale_against_the_next_day() {
        let mut agg = MultiTfAggregator::default();
        // Receipt is a full working day later, so the trusted band cannot
        // collapse the two onto one clock.
        let mut t = tick(13, SEG_IDX, DAY - 1, 100.0, 1);
        t.received_at_nanos = receipt_at_ist(i64::from(DAY + 33_400));

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.stale_trading_day,
            "23:59:59 of the previous IST day is a different day, and the gate \
             is a DAY comparison — a second is enough"
        );
    }

    /// Exactly IST midnight is the FIRST second of the new day, not the last
    /// second of the old one. Off-by-one here would refuse a whole day's
    /// opening tick on the boundary.
    #[test]
    fn an_exchange_stamp_exactly_at_ist_midnight_belongs_to_the_new_day() {
        let mut agg = MultiTfAggregator::default();
        let mut t = tick(13, SEG_IDX, DAY, 100.0, 1);
        t.received_at_nanos = receipt_at_ist(i64::from(DAY + 33_400));

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            !stats.stale_trading_day,
            "00:00:00 IST is day D, not day D-1 — the floor division must put \
             the boundary second on the new day"
        );
    }

    // -- ORDER: which arm wins when two faults are true at once -------------

    /// A corrupt PRICE outranks a day mismatch, and must: the price arm is a
    /// hard refusal whose remedy is "this packet is unusable", while the day
    /// arm's remedy is "this is a stale snapshot". Booking a NaN price under
    /// `stale_trading_day` would make the stale-snapshot rate — the number the
    /// operator reads to size reconnect noise — silently include corruption.
    #[test]
    fn an_insane_price_is_judged_before_a_day_mismatch() {
        let mut agg = MultiTfAggregator::default();
        let yesterday = DAY - 86_400 + 33_400;
        let mut t = tick(13, SEG_IDX, yesterday, f32::NAN, 1);
        t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(stats.refused_price, "the price arm owns a NaN");
        assert!(
            !stats.stale_trading_day,
            "and the day arm must not also claim it — one tick, one reason, or \
             the stale-snapshot rate stops meaning what it says"
        );
    }

    /// The vendor's never-traded sentinel (`exchange_timestamp == 0`) is
    /// judged by its own arm, which KEEPS THE ROW. Letting it fall to the day
    /// gate would turn it into a hard refusal and lose the ability to tell
    /// "did not trade today" from "did not capture" — the exact false-OK the
    /// 2026-08-26 fix removed.
    #[test]
    fn an_untraded_sentinel_never_reaches_the_day_gate() {
        let mut agg = MultiTfAggregator::default();
        let mut t = tick(13, SEG_IDX, 0, 100.0, 1);
        t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.untraded_timestamp,
            "the sentinel keeps its own arm, which keeps the row"
        );
        assert!(
            !stats.stale_trading_day && !stats.future_trading_day,
            "the day gate must not see it — epoch 0 is a sentinel, not a 1970 \
             trading day, and refusing it hard would lose the row"
        );
    }

    /// An out-of-band second is also judged before the day gate, and also
    /// keeps its row. It is the LARGEST of the candle-only reasons — 2,008,916
    /// ticks in one measured session — so a day gate that swallowed it would
    /// silently convert 2.4% of a session from kept rows to discarded ones.
    #[test]
    fn an_out_of_band_stamp_never_reaches_the_day_gate() {
        let mut agg = MultiTfAggregator::default();
        let mut t = tick(13, SEG_IDX, MIN_PLAUSIBLE_EXCHANGE_TS_SECS - 1, 100.0, 1);
        t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.out_of_band_timestamp,
            "a below-floor second belongs to the band arm, which keeps the row"
        );
        assert!(
            !stats.stale_trading_day && !stats.future_trading_day,
            "the day gate must not re-judge it as a stale day and discard the row"
        );
    }

    // -- the grid -----------------------------------------------------------

    /// STALE and FUTURE can never both be true, across the whole grid.
    ///
    /// They are exclusive by arithmetic (`>` then `<` on the same pair), but
    /// the drain books ONE reason per tick from an `if/else if` chain, so an
    /// overlap would silently drop one counter and mis-attribute the other.
    /// This walks nine day offsets against three receipt days and asserts the
    /// invariant on every cell, rather than trusting the arithmetic to stay
    /// the arithmetic.
    #[test]
    fn stale_and_future_are_mutually_exclusive_across_the_grid() {
        for day_offset in [-34_i64, -2, -1, 0, 1, 2, 34] {
            for receipt_offset in [-1_i64, 0, 1] {
                let mut agg = MultiTfAggregator::default();
                let stamp = i64::from(DAY) + day_offset * 86_400 + 33_400;
                let Ok(stamp_u32) = u32::try_from(stamp) else {
                    continue;
                };
                let mut t = tick(13, SEG_IDX, stamp_u32, 100.0, 1);
                t.received_at_nanos =
                    receipt_at_ist(i64::from(DAY) + receipt_offset * 86_400 + 33_400);

                let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

                assert!(
                    !(stats.stale_trading_day && stats.future_trading_day),
                    "day_offset {day_offset}, receipt_offset {receipt_offset}: \
                     both day flags set. The drain books ONE reason per tick, \
                     so an overlap loses a counter and mis-names the other"
                );
                // And exactly one of the three outcomes holds.
                let same_day = day_offset == receipt_offset;
                assert_eq!(
                    !stats.stale_trading_day && !stats.future_trading_day,
                    same_day,
                    "day_offset {day_offset}, receipt_offset {receipt_offset}: \
                     a tick folds if and only if the two days match"
                );
            }
        }
    }
}
