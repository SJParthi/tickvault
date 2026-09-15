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
//! Steady state: one `HashMap::get` on a `Copy` key, one `Vec` index, `TF_COUNT`
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

use tickvault_common::constants::{
    EXCHANGE_SEGMENT_BSE_EQ, EXCHANGE_SEGMENT_BSE_FNO, EXCHANGE_SEGMENT_NSE_EQ,
    EXCHANGE_SEGMENT_NSE_FNO, MAX_PLAUSIBLE_LTP,
};
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::{ParsedTick, VolumeCounter, VolumeObservation, VolumeQuality};

use crate::candles::aggregator_cell::{AggregatorCell, ConsumeOutcome, FeedStrategy, TickPrices};
use crate::candles::tf_index::{
    CANDLE_SESSION_OPEN_SECS_OF_DAY_IST, MARKET_CLOSE_SECS_OF_DAY_IST, MARKET_OPEN_SECS_OF_DAY_IST,
    candle_bucket_clock_ist_secs, fold_clock_ist_secs,
};
use crate::candles::volume_update::{
    VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN, volume_quality_bits, volume_quality_from_bits,
};
use crate::candles::{
    BufferOutcome, BufferedSeal, CandleMetadata, CandleVolumeUpdate, LiveCandleState, SealRing,
    TF_COUNT, TfIndex,
};

#[cfg(test)]
#[path = "multi_tf_aggregator/closure_tests.rs"]
mod observation_window_closure_tests;

#[cfg(test)]
#[path = "multi_tf_aggregator/signed_bar_tests.rs"]
mod signed_bar_tests;

#[cfg(test)]
#[path = "multi_tf_aggregator/dhan_clock_open_tests.rs"]
mod dhan_clock_open_tests;

#[cfg(test)]
#[path = "multi_tf_aggregator/counter_attribution_tests.rs"]
mod counter_attribution_tests;

#[cfg(test)]
#[path = "multi_tf_aggregator/ten_minute_tests.rs"]
mod ten_minute_tests;

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

/// The first present counter, with the evidence needed to judge a bucket's
/// opening boundary. A positive observation carries no quantity for the
/// unobserved prefix. Its second is rounded down by the fold clock, so even
/// equality with a bucket start does not prove a pre-bucket baseline. A real
/// zero counter at that boundary does prove that prefix contained no volume.
#[derive(Clone, Copy, Debug)]
struct VolumeBaseline {
    observed_secs: u32,
    known_zero: bool,
}

impl VolumeBaseline {
    #[inline]
    fn covers_bucket_start(self, bucket_start_secs: u32) -> bool {
        self.observed_secs < bucket_start_secs
            || (self.observed_secs == bucket_start_secs && self.known_zero)
    }
}

/// Evidence for the narrowly admitted first regular-session Dhan trade.
/// A snapshot arriving later, an absent quantity field or a segment with a
/// different regular opening time supplies no proof of a zero origin.
#[inline]
fn dhan_opening_trade_quantity(
    feed: Feed,
    tick: &ParsedTick,
    observation: VolumeObservation,
) -> Option<u16> {
    if feed != Feed::Dhan
        || !matches!(
            tick.exchange_segment_code,
            EXCHANGE_SEGMENT_NSE_EQ
                | EXCHANGE_SEGMENT_NSE_FNO
                | EXCHANGE_SEGMENT_BSE_EQ
                | EXCHANGE_SEGMENT_BSE_FNO
        )
        || !tick.volume_present
        || tick.last_trade_quantity == 0
        || tick.exchange_timestamp % 86_400 != MARKET_OPEN_SECS_OF_DAY_IST
        || tick.received_at_nanos <= 0
        || observation != VolumeObservation::Cumulative(u64::from(tick.last_trade_quantity))
    {
        return None;
    }
    let receipt_ist_secs =
        tick.received_at_nanos / 1_000_000_000 + crate::candles::tf_index::IST_UTC_OFFSET_SECS;
    (receipt_ist_secs == i64::from(tick.exchange_timestamp)).then_some(tick.last_trade_quantity)
}

/// One instrument's fold state.
#[derive(Clone, Debug)]
struct InstrumentSlot {
    /// The composite identity this slot belongs to — carried so seal
    /// emissions can name the instrument without a reverse lookup.
    key: CompositeKey,
    /// Per-timeframe candle state.
    cell: AggregatorCell,
    /// Shared presence/session-aware policy used to derive every frame's input.
    volume_counter: VolumeCounter,
    /// First present counter observation in this session. A positive baseline
    /// in the bucket's opening second is still partial: subsecond receipt
    /// rounding must never turn an unobserved prefix into complete coverage.
    volume_baseline: Option<VolumeBaseline>,
    /// LTT of the last accepted present counter, including an unchanged
    /// counter. Price-only packets and rejected lower counters cannot shorten
    /// the interval whose intervening quantity has no individual trade times.
    last_counter_observed_secs: Option<u32>,
    volume_revision: u64,
    /// Bounded trusted receipt clock for observation freshness, not bucket placement.
    last_observed_secs: u32,
    /// Highest accepted source event second. Late same-bucket Dhan observations
    /// may contribute quantity but cannot silently refresh the last trade price.
    last_trade_secs: u32,
    /// Exact last bucket whose admitted window was qualified as expired,
    /// matching the cell's single amendable last-sealed bucket per frame.
    /// A cutoff high-water mark would incorrectly certify an older partial
    /// administrative seal. Zero is outside every admitted bucket grid.
    qualified_closed_bucket_start: [u32; TF_COUNT],
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
}

impl InstrumentSlot {
    fn annotate_volume(
        &mut self,
        tf: TfIndex,
        state: LiveCandleState,
        metadata: CandleMetadata,
        volume_missing: bool,
        attribution_uncertain: bool,
    ) -> LiveCandleState {
        let quality = VolumeQuality {
            baseline_known: self
                .volume_baseline
                .is_some_and(|baseline| baseline.covers_bucket_start(state.bucket_start_ist_secs)),
            counter_ambiguous: self.volume_counter.is_ambiguous(),
            volume_missing,
            attribution_uncertain,
        };
        self.cell.stamp_volume_metadata(
            tf,
            state,
            metadata,
            volume_quality_bits(quality),
            self.volume_revision,
        )
    }

    fn volume_update(
        &mut self,
        tf: TfIndex,
        state: LiveCandleState,
        closed_through_secs: Option<u32>,
    ) -> CandleVolumeUpdate {
        let start = state.bucket_start_ist_secs;
        let window_end = tf.observation_window_end(start);
        if window_end.is_some_and(|end| closed_through_secs.is_some_and(|cutoff| cutoff >= end)) {
            self.qualified_closed_bucket_start[tf.as_ordinal()] = start;
        }
        let closed =
            window_end.is_some() && self.qualified_closed_bucket_start[tf.as_ordinal()] == start;
        CandleVolumeUpdate {
            feed: self.key.0,
            security_id: self.key.1,
            segment_code: self.key.2,
            tf,
            session_day: start / 86_400,
            bucket_start_secs: start,
            bucket_end_secs: tf.bucket_end(start),
            gross_volume: state.volume,
            estimated_net_volume: state.signed_bar_volume(),
            metadata: state.metadata,
            volume_quality: state.volume_quality,
            revision: state.bucket_revision,
            last_observed_secs: self.last_observed_secs,
            quality: volume_quality_from_bits(state.volume_quality),
            closed,
        }
    }
}

/// Per-tick outcome, coalesced across all [`TF_COUNT`](crate::candles::TF_COUNT)
/// timeframes so the caller emits one log line / counter set per tick
/// rather than one per timeframe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConsumeStats {
    /// Timeframes that sealed a bucket and emitted it. `0..=TF_COUNT`.
    pub sealed_count: u8,
    /// Seal amendments emitted for this tick, including pending carry settled
    /// before a counter restart. At most two amendments per timeframe.
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
    /// `true` when the vendor stamp is ahead of the receipt beyond the allowed
    /// clock lead, including a later IST day. Nothing folds or advances the
    /// watermark. The field name is retained for caller compatibility.
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
    ///    of the OPEN path, so the day-open arm never fires — across all active frames
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

impl MultiTfAggregator {
    /// An untraded snapshot carries no supported current-session quantity
    /// clock. Its receipt says when it arrived, not when its counter was
    /// measured. It must not open a slot, seed a zero baseline, reset a
    /// session or refresh quantity freshness. In particular, a later positive
    /// counter must remain a baseline rather than counting its unseen prefix.
    ///
    /// A present zero may still contradict an already accepted same-session
    /// counter. Publish that uncertainty from the existing candle state so an
    /// old eligible winner cannot survive a counter regression. This path
    /// creates no candle, observation clock or qualified complete-window seal.
    fn observe_untraded_zero<F, V>(
        &mut self,
        feed: Feed,
        tick: &ParsedTick,
        wider_counter: Option<u64>,
        metadata: CandleMetadata,
        on_seal: &mut F,
        on_volume: &mut V,
    ) -> u8
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
        V: FnMut(CandleVolumeUpdate),
    {
        if tick.last_traded_price != 0.0
            || tick.received_at_nanos <= 0
            || tick.volume_observation(wider_counter) != VolumeObservation::Cumulative(0)
        {
            return 0;
        }
        let receipt =
            tick.received_at_nanos / 1_000_000_000 + crate::candles::tf_index::IST_UTC_OFFSET_SECS;
        let Ok(at) = u32::try_from(receipt) else {
            return 0;
        };
        if !(MIN_PLAUSIBLE_EXCHANGE_TS_SECS..=MAX_PLAUSIBLE_EXCHANGE_TS_SECS).contains(&at)
            || at / 86_400 < self.watermark_secs / 86_400
            || at % 86_400 < CANDLE_SESSION_OPEN_SECS_OF_DAY_IST
            || at % 86_400 >= MARKET_CLOSE_SECS_OF_DAY_IST
        {
            return 0;
        }
        let key = (feed, tick.security_id, tick.exchange_segment_code);
        let Some(index) = self.lookup(key.0, key.1, key.2) else {
            return 0;
        };
        let Some(slot) = self.slots.get_mut(index) else {
            return 0;
        };
        if slot.volume_counter.session_day() != Some(at / 86_400)
            || slot.volume_counter.cumulative().is_none()
        {
            return 0;
        }
        slot.volume_revision = slot.volume_revision.saturating_add(1);
        slot.volume_counter
            .observe(at / 86_400, VolumeObservation::Cumulative(0));
        let mut amended_count = 0_u8;
        for tf in TfIndex::ALL {
            let state = slot.cell.snapshot(tf);
            if !state.is_uninitialised() {
                let state = slot.annotate_volume(tf, state, metadata, false, true);
                on_volume(slot.volume_update(tf, state, None));
            }
            // Catch-up may have drained the price candle before the source
            // anomaly arrived. Revoke its retained ranking too; an empty live
            // slot must not shield the last published closed winner.
            if let Some(previous) = slot.cell.last_sealed_snapshot(tf) {
                let previous = slot.annotate_volume(tf, previous, metadata, false, true);
                on_volume(slot.volume_update(tf, previous, None));
                on_seal(key.0, key.1, key.2, tf, previous);
                amended_count = amended_count.saturating_add(1);
            }
        }
        amended_count
    }

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
            volume_counter: VolumeCounter::default(),
            volume_baseline: None,
            last_counter_observed_secs: None,
            volume_revision: 0,
            last_observed_secs: 0,
            last_trade_secs: 0,
            qualified_closed_bucket_start: [0; TF_COUNT],
            last_ltp: f64::NAN,
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
    /// Expected O(F) work on an already allocated instrument, where F is the
    /// fixed [`TF_COUNT`](crate::candles::TF_COUNT): one expected-constant hash
    /// lookup and one scalar fold per active frame. First-sight slot growth
    /// and caller callbacks have separate costs; this is not a worst-case
    /// end-to-end latency bound.
    pub fn consume_tick<F>(
        &mut self,
        feed: Feed,
        tick: &ParsedTick,
        cumulative_volume_override: Option<u64>,
        on_seal: F,
    ) -> ConsumeStats
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        self.consume_tick_with_volume_updates(
            feed,
            tick,
            cumulative_volume_override,
            on_seal,
            |_| {},
        )
    }

    /// Fold once and expose the exact same per-bucket gross/signed quantities to
    /// consumers such as Top Volume. At most a fixed number of publications per
    /// timeframe of this instrument are emitted. O(F) steady-state work; no
    /// instrument-population scan, database read, allocation or timer delta.
    pub fn consume_tick_with_volume_updates<F, V>(
        &mut self,
        feed: Feed,
        tick: &ParsedTick,
        cumulative_volume_override: Option<u64>,
        on_seal: F,
        on_volume: V,
    ) -> ConsumeStats
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
        V: FnMut(CandleVolumeUpdate),
    {
        self.consume_tick_with_context(
            feed,
            tick,
            cumulative_volume_override,
            CandleMetadata::UNKNOWN,
            on_seal,
            on_volume,
        )
    }

    /// Fold with instrument metadata resolved once by the owner. Each new bucket
    /// pins that definition; a mid-bucket change taints the existing bucket and
    /// never re-prices its historical signed quantity under a different lot size.
    pub fn consume_tick_with_context<F, V>(
        &mut self,
        feed: Feed,
        tick: &ParsedTick,
        cumulative_volume_override: Option<u64>,
        metadata: CandleMetadata,
        mut on_seal: F,
        mut on_volume: V,
    ) -> ConsumeStats
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
        V: FnMut(CandleVolumeUpdate),
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
            let amended_count = self.observe_untraded_zero(
                feed,
                tick,
                cumulative_volume_override,
                metadata,
                &mut on_seal,
                &mut on_volume,
            );
            crate::candles::fold_counters::fold_counters()
                .tick_untraded_timestamp
                .increment(1);
            return ConsumeStats {
                untraded_timestamp: true,
                amended_count,
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
            let amended_count = self.observe_untraded_zero(
                feed,
                tick,
                cumulative_volume_override,
                metadata,
                &mut on_seal,
                &mut on_volume,
            );
            crate::candles::fold_counters::fold_counters()
                .tick_refused_untraded_sentinel
                .increment(1);
            return ConsumeStats {
                untraded_sentinel: true,
                amended_count,
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
        // Dhan bucket placement is validated LTT; its observation clock stays
        // on the bounded receipt policy used by freshness consumers. Other
        // feeds retain their existing bucket-clock contract.
        let fold_secs =
            candle_bucket_clock_ist_secs(feed, tick.exchange_timestamp, tick.received_at_nanos);
        let observed_secs = fold_clock_ist_secs(tick.exchange_timestamp, tick.received_at_nanos);

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
        // gate below: all ten active timeframes stop folding, for every instrument,
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
            if fold_day > receipt_day
                || i64::from(tick.exchange_timestamp)
                    > receipt_ist_secs + crate::candles::tf_index::MAX_PLAUSIBLE_RECEIPT_LEAD_SECS
            {
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

        // Clockless legacy replay can still fold local buckets with uncertain
        // attribution. It cannot advance a shared watermark from an unverified
        // vendor stamp and thereby force-seal every other instrument.
        if tick.received_at_nanos > 0 && fold_secs > self.watermark_secs {
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

        let observation = tick.volume_observation(cumulative_volume_override);
        let volume_missing = matches!(observation, VolumeObservation::Missing);
        let previous_counter = slot.volume_counter.cumulative();
        let assessment = slot.volume_counter.observe_with_opening_trade(
            fold_secs / 86_400,
            observation,
            dhan_opening_trade_quantity(feed, tick, observation),
        );
        let previous_counter_secs = if assessment.session_changed {
            None
        } else {
            slot.last_counter_observed_secs
        };
        let cumulative_volume = assessment.cumulative;
        let baseline = assessment.baseline;
        let trade_time_regressed =
            feed == Feed::Dhan && tick.exchange_timestamp < slot.last_trade_secs;
        let mut stats = ConsumeStats::default();
        slot.volume_revision = slot.volume_revision.saturating_add(1);
        slot.last_observed_secs = slot.last_observed_secs.max(observed_secs);
        slot.last_trade_secs = slot.last_trade_secs.max(tick.exchange_timestamp);

        // A new validated session is an explicit axis change. A same-session
        // lower observation is not: drop magnitude and packet count cannot
        // distinguish a reset from delayed old data. Keep the old high-water
        // quantity and invalidate signed/eligible decisions for the session.
        if assessment.session_changed {
            for tf in TfIndex::ALL {
                if let Some(state) = slot.cell.force_seal(tf) {
                    stats.sealed_count = stats.sealed_count.saturating_add(1);
                    let state = slot.annotate_volume(tf, state, state.metadata, false, true);
                    on_volume(slot.volume_update(tf, state, Some(fold_secs)));
                    on_seal(key.0, key.1, key.2, tf, state);
                }
            }
            slot.volume_baseline = None;
            slot.last_counter_observed_secs = None;
        }
        let decreased = matches!(observation, VolumeObservation::Cumulative(value)
            if previous_counter.is_some_and(|previous| value < previous)
                && !assessment.session_changed);
        if decreased {
            crate::candles::fold_counters::fold_counters()
                .cumulative_regression
                .increment(1);
        }
        if !volume_missing && !decreased {
            slot.last_counter_observed_secs = Some(fold_secs);
        }
        if !decreased && !trade_time_regressed {
            slot.last_ltp = prices.last_traded_price;
        }
        if assessment.baseline_seeded {
            slot.volume_baseline = Some(VolumeBaseline {
                observed_secs: fold_secs,
                known_zero: baseline == 0,
            });
            // Price-only packets may have opened buckets before any counter
            // arrived. Rebase those zero/partial buckets before folding the
            // first present counter, so a missing Ticker field can never turn
            // into a baseline of zero for the entire day so far.
            // A proved opening trade already has a zero origin and a positive
            // first increment. Rebasing empty cells would mark their chains
            // broken and replace that origin with the live counter, losing
            // the very first trade again. Such a proof requires no accepted
            // counter in the current session; any price-only cells have a
            // zero quantity axis, and prior-session cells were cleared above.
            let mut amended = [None; TF_COUNT];
            if assessment.increment == 0 {
                slot.cell.rebase_open_buckets(baseline, |tf, state| {
                    if let Some(target) = amended.get_mut(tf.as_ordinal()) {
                        *target = Some(state);
                    }
                });
            }
            for tf in TfIndex::ALL {
                if let Some(Some(state)) = amended.get(tf.as_ordinal()) {
                    stats.amended_count = stats.amended_count.saturating_add(1);
                    let state = slot.annotate_volume(tf, *state, state.metadata, false, true);
                    on_volume(slot.volume_update(tf, state, Some(fold_secs)));
                    on_seal(key.0, key.1, key.2, tf, state);
                }
            }
            crate::candles::fold_counters::fold_counters()
                .slot_volume_baseline_seeded
                .increment(1);
        }
        let extremes = slot.cell.observe_session_extremes(tick, fold_secs);
        // Compatibility argument carries observation availability only. The
        // whole-bar sign is derived inside the cell from its frozen baseline.
        let signed_tick_volume = if assessment.quality.counter_ambiguous || volume_missing {
            None
        } else {
            Some(0)
        };

        // A Dhan packet supplies one last-trade time/quantity and a cumulative
        // counter. A larger (or inconsistent) increment does not locate the
        // intervening trades within that interval. A missing counter crossing
        // a boundary is flagged now, before its outgoing bucket can leave the
        // cell's bounded retention. Do not manufacture a split of the gross
        // quantity. Other feeds retain their existing clock/counter contract.
        let uncertain_counter_interval = (feed == Feed::Dhan
            && (volume_missing
                || (assessment.increment > 0
                    && (trade_time_regressed
                        || tick.last_trade_quantity == 0
                        || assessment.increment != u64::from(tick.last_trade_quantity)))))
        .then_some(previous_counter_secs)
        .flatten();
        // A rejected counter regression also revokes a retained closed row;
        // a timer drain must not shield the previous eligible publication.
        let counter_axis_rejected = feed == Feed::Dhan && decreased;

        for tf in TfIndex::ALL {
            let uncertain_bucket_range = uncertain_counter_interval.and_then(|previous| {
                let from = tf.bucket_start(previous);
                let to = tf.bucket_start(fold_secs);
                (from != to).then_some((from.min(to), from.max(to)))
            });
            let affected = |state: LiveCandleState| {
                !state.is_uninitialised()
                    && (counter_axis_rejected
                        || uncertain_bucket_range.is_some_and(|(first, last)| {
                            (first..=last).contains(&state.bucket_start_ist_secs)
                        }))
            };
            let before = slot.cell.snapshot(tf);
            if affected(before) {
                // The ordinary rollover callback will publish this stamped
                // outgoing state; no intermediate same-revision publication.
                slot.annotate_volume(tf, before, before.metadata, false, true);
            }
            let boundary_amendment = slot.cell.last_sealed_snapshot(tf).and_then(|previous| {
                (affected(previous)
                    && previous.volume_quality & VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN == 0)
                    .then(|| slot.annotate_volume(tf, previous, previous.metadata, false, true))
            });
            let mut attribution_uncertain = tick.received_at_nanos <= 0
                || trade_time_regressed
                || counter_axis_rejected
                || uncertain_bucket_range.is_some();
            let outcome = slot.cell.consume_tick_with_extremes(
                tf,
                tick,
                prices,
                baseline,
                strategy,
                cumulative_volume,
                extremes,
                // Availability only; numeric tick-direction payloads are gone.
                signed_tick_volume,
                // Derived ONCE at :748, above this loop — the same hoisting
                // contract as `prices` and `cumulative_volume`. Passing it
                // down rather than recomputing it saves 48 conversions per
                // tick (TF_COUNT timeframes × the bucket site and one fold arm).
                fold_secs,
            );
            if let Some(previous) = boundary_amendment {
                let emitted_by_fold = match outcome {
                    ConsumeOutcome::Sealed { sealed_state } => {
                        sealed_state.bucket_start_ist_secs == previous.bucket_start_ist_secs
                    }
                    ConsumeOutcome::AmendedLate { amended_state } => {
                        amended_state.bucket_start_ist_secs == previous.bucket_start_ist_secs
                    }
                    ConsumeOutcome::Updated | ConsumeOutcome::DiscardLate => false,
                };
                if !emitted_by_fold {
                    stats.amended_count = stats.amended_count.saturating_add(1);
                    // Preserve the existing closure qualification. Discovering
                    // uncertainty is not a new clock or a new volume sample.
                    on_volume(slot.volume_update(tf, previous, None));
                    on_seal(key.0, key.1, key.2, tf, previous);
                }
            }
            match outcome {
                ConsumeOutcome::Updated => {}
                ConsumeOutcome::Sealed { sealed_state } => {
                    stats.sealed_count = stats.sealed_count.saturating_add(1);
                    let sealed_state =
                        slot.annotate_volume(tf, sealed_state, sealed_state.metadata, false, false);
                    on_volume(slot.volume_update(tf, sealed_state, Some(fold_secs)));
                    on_seal(key.0, key.1, key.2, tf, sealed_state);
                }
                ConsumeOutcome::AmendedLate { amended_state } => {
                    stats.amended_count = stats.amended_count.saturating_add(1);
                    attribution_uncertain = true;
                    let amended_state = slot.annotate_volume(
                        tf,
                        amended_state,
                        amended_state.metadata,
                        false,
                        true,
                    );
                    on_volume(slot.volume_update(tf, amended_state, Some(fold_secs)));
                    on_seal(key.0, key.1, key.2, tf, amended_state);
                }
                ConsumeOutcome::DiscardLate => {
                    attribution_uncertain = true;
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
                    // existing: this arm sits inside the fixed-timeframe loop on
                    // the per-tick path, which is the one place a bare
                    // `counter!` macro must never appear.
                    crate::candles::fold_counters::fold_counters()
                        .tick_discarded_late
                        .increment(1);
                }
            }
            let current = slot.cell.snapshot(tf);
            if !current.is_uninitialised() {
                let current = slot.annotate_volume(
                    tf,
                    current,
                    metadata,
                    volume_missing,
                    attribution_uncertain,
                );
                on_volume(slot.volume_update(tf, current, None));
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
    /// The count derives from the registry, so retired frames add neither
    /// fold state nor a seal visit.
    pub fn force_seal_all<F>(&mut self, on_seal: F) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        self.force_seal_all_with_volume_updates(on_seal, |_| {})
    }

    /// Administrative flush with the same canonical quantity callback as live
    /// folds. Existing state is emitted before resetting its volume baseline
    /// and quality. With no expiry cutoff, these updates do not certify a
    /// complete observation window. Use the explicit expiry method before an
    /// orderly after-close drain. O(N × F), not an O(1) tick operation.
    pub fn force_seal_all_with_volume_updates<F, V>(
        &mut self,
        mut on_seal: F,
        mut on_volume: V,
    ) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
        V: FnMut(CandleVolumeUpdate),
    {
        let mut emitted = 0_usize;
        for slot in &mut self.slots {
            let (feed, sid, seg) = slot.key;
            slot.volume_revision = slot.volume_revision.saturating_add(1);
            for tf in TfIndex::ALL {
                if let Some(state) = slot.cell.force_seal(tf) {
                    emitted = emitted.saturating_add(1);
                    let state = slot.annotate_volume(tf, state, state.metadata, false, false);
                    on_volume(slot.volume_update(tf, state, None));
                    on_seal(feed, sid, seg, tf, state);
                }
            }
            slot.volume_counter.reset_baseline();
            slot.volume_baseline = None;
            slot.last_counter_observed_secs = None;
        }
        emitted
    }

    /// Watermark-aware intraday catch-up seal across every instrument: seals
    /// only the buckets whose admitted observation-window end is at or before
    /// `cutoff_secs`. The final-tail nominal timestamp is preserved.
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
    /// work that happens beside it. The ignored
    /// `catch_up_seal_all_sweep_cost_at_the_authorized_ceiling` harness below
    /// measures the selected source/runtime. Earlier 24-frame timings do not
    /// measure this ten-frame candidate and must not be relabeled as such.
    pub fn catch_up_seal_all<F>(&mut self, cutoff_secs: u32, on_seal: F) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        self.catch_up_seal_all_with_volume_updates(cutoff_secs, on_seal, |_| {})
    }

    /// Publish timer-sealed candles to the same canonical stream. A timer does
    /// not create a new observation timestamp or infer missing volume. The
    /// existing sweep visits N instruments × F frames; only actual seals emit.
    pub fn catch_up_seal_all_with_volume_updates<F, V>(
        &mut self,
        cutoff_secs: u32,
        mut on_seal: F,
        mut on_volume: V,
    ) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
        V: FnMut(CandleVolumeUpdate),
    {
        let mut emitted = 0_usize;
        for slot in &mut self.slots {
            let (feed, sid, seg) = slot.key;
            slot.volume_revision = slot.volume_revision.saturating_add(1);
            for tf in TfIndex::ALL {
                if let Some(state) = slot.cell.catch_up_seal(tf, cutoff_secs) {
                    emitted = emitted.saturating_add(1);
                    let state = slot.annotate_volume(tf, state, state.metadata, false, false);
                    on_volume(slot.volume_update(tf, state, Some(cutoff_secs)));
                    on_seal(feed, sid, seg, tf, state);
                }
            }
        }
        emitted
    }

    /// Explicit local-clock expiry of the regular capture observation windows.
    /// This closes aggregation state, not the provider's delivery history.
    /// The caller supplies its trusted IST wall-clock second; source freshness,
    /// quantity quality, and nominal bucket identities remain unchanged.
    /// Early calls leave unexpired windows open; repeated calls do not invent
    /// zero candles or duplicate unchanged seals. O(N × F), on maintenance.
    pub fn seal_expired_observation_windows_with_volume_updates<F, V>(
        &mut self,
        now_ist_secs: u32,
        on_seal: F,
        on_volume: V,
    ) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
        V: FnMut(CandleVolumeUpdate),
    {
        self.catch_up_seal_all_with_volume_updates(now_ist_secs, on_seal, on_volume)
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
            received_at_nanos: if (MIN_PLAUSIBLE_EXCHANGE_TS_SECS..=MAX_PLAUSIBLE_EXCHANGE_TS_SECS)
                .contains(&ts)
            {
                (i64::from(ts) - 19_800) * 1_000_000_000
            } else {
                0
            },
            volume: cum,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn preopen_ingest_preserves_the_official_open_in_all_active_frames() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        for (stamp, cumulative) in [
            (CANDLE_OPEN, 100_u32),
            (CANDLE_OPEN + 300, 110),
            (CANDLE_OPEN + 840, 120),
        ] {
            let mut before = tick(77, SEG_IDX, stamp, 100.0, cumulative);
            before.received_at_nanos = (i64::from(stamp) - 19_800) * 1_000_000_000;
            let stats = agg.consume_tick(Feed::Dhan, &before, None, |_, _, _, _, _| {});
            assert!(
                stats.folded(),
                "pre-open packets reach the live candle path"
            );
        }

        let mut opening = tick(77, SEG_IDX, OPEN, 105.0, 130);
        opening.received_at_nanos = (i64::from(OPEN) - 19_800) * 1_000_000_000;
        opening.day_open = 98.0;
        opening.day_high = 110.0;
        opening.day_low = 95.0;
        let stats = agg.consume_tick(Feed::Dhan, &opening, None, |_, _, _, _, _| {});
        assert!(stats.folded());
        for tf in TfIndex::ALL {
            let state = agg
                .snapshot(Feed::Dhan, 77, SEG_IDX, tf)
                .expect("instrument has fold state");
            let period = tf.seconds_per_bucket();
            assert_eq!(
                state.bucket_start_ist_secs,
                CANDLE_OPEN + (900 / period) * period
            );
            assert_eq!(state.open, 98.0, "official open for {tf:?}");
            assert_eq!(state.high, 110.0, "opening high for {tf:?}");
            assert_eq!(state.low, 95.0, "opening low for {tf:?}");
            assert_eq!(state.close, 105.0, "observed close for {tf:?}");
        }
    }

    // Gross-counter uncertainty still refuses signed-bar publication. The
    // dedicated signed_bar_tests module checks the whole-bar direction rule.

    /// A reset quote is current: the next tick compares with its price,
    /// even when the pre-reset price would imply the opposite direction.
    #[test]
    fn an_ambiguous_counter_restart_cannot_establish_a_new_price_axis() {
        for (before, reset, after, _old_inferred_net) in [
            (100.0_f32, 110.0_f32, 105.0_f32, -200_i64),
            (110.0, 100.0, 105.0, 200),
        ] {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            for (offset, price, cumulative) in
                [(0, before, 4_000_000_000), (1, reset, 100), (2, after, 300)]
            {
                let _ = agg.consume_tick(
                    Feed::Dhan,
                    &tick(91, SEG_IDX, OPEN + offset, price, cumulative),
                    None,
                    |_, _, _, _, _| {},
                );
            }
            let bar = agg
                .snapshot(Feed::Dhan, 91, SEG_IDX, TfIndex::M1)
                .expect("open bucket");
            assert_eq!(bar.volume, 0);
            assert_eq!(bar.net_volume(), None);
            assert_ne!(
                bar.volume_quality
                    & crate::candles::volume_update::VOLUME_QUALITY_COUNTER_AMBIGUOUS,
                0
            );
        }
    }

    #[test]
    fn a_restart_does_not_invent_a_direction_for_an_unchanged_price() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        for (offset, price, cumulative) in [
            (0, 100.0, 4_000_000_000),
            (1, 101.0, 4_000_000_100),
            (2, 110.0, 100),
            (3, 110.0, 300),
        ] {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(91, SEG_IDX, OPEN + offset, price, cumulative),
                None,
                |_, _, _, _, _| {},
            );
        }
        let bar = agg
            .snapshot(Feed::Dhan, 91, SEG_IDX, TfIndex::M1)
            .expect("open bucket");
        assert_eq!(bar.volume, 100);
        assert_eq!(bar.net_volume(), None);
    }

    /// A lower counter alone cannot distinguish a reset from a stale snapshot.
    /// Preserve observed pre-decrease quantity and expose uncertainty instead
    /// of inventing a certified counter axis from the drop magnitude.
    #[test]
    fn an_ambiguous_counter_restart_preserves_quantity_and_invalidates_net() {
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
        // A large decrease is ambiguous, not proof of a new counter axis.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(91, SEG_IDX, base + 2, 102.0, 100_000),
            None,
            sink,
        );
        // A recovery below the retained high cannot certify another 50,000.
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
            None,
            "an unresolved counter epoch cannot produce certified signed flow"
        );
        assert_eq!(
            open.volume, 1_000,
            "only the accepted old-axis increment is attributed"
        );
    }

    /// A lower counter adds no accepted quantity and is explicitly uncertain.
    /// A genuine duplicate is different: it leaves a known bar classified.
    #[test]
    fn a_lower_counter_contributes_zero_and_invalidates_certified_net() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let base = OPEN;

        // Give this M1 candle a real same-timeframe predecessor; otherwise
        // its signed value would already be unavailable before the decrease.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(93, SEG_IDX, base - 1, 100.0, 5_000),
            None,
            sink,
        );
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

        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(93, SEG_IDX, base + 1, 101.0, 6_000),
            None,
            sink,
        );
        let duplicate = agg.snapshot(Feed::Dhan, 93, SEG_IDX, TfIndex::M1).unwrap();
        assert_eq!(duplicate.volume, 1_000, "a duplicate cannot count twice");
        assert_eq!(
            duplicate.net_volume(),
            before,
            "a duplicate preserves the known signal"
        );

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
            None,
            "the lower counter cannot certify whether its epoch changed"
        );
        assert_eq!(after.volume, 1_000, "and it added nothing to gross either");
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
        for tf in TfIndex::ALL {
            assert_eq!(
                total(tf),
                expected,
                "every active frame conserves the ledger: {tf:?}"
            );
        }
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
        // Each active frame must independently match the received ledger;
        // agreement between two candles could otherwise hide a shared defect.
        for tf in TfIndex::ALL {
            assert_eq!(
                total(tf),
                expected,
                "one carry settlement per frame: {tf:?}"
            );
        }
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
        // `max` above it would have STAYED pinned there, suppressing new-day
        // volume in every active timeframe until the old counter was exceeded.
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
    /// gate: all ten active timeframes stop folding, for every instrument, with only a
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

    /// Clockless legacy replay may fold locally, but cannot move a shared
    /// watermark from a vendor timestamp nobody independently validated.
    #[test]
    fn a_clockless_future_replay_cannot_advance_the_global_watermark() {
        let today_in_session = DAY + 33_300 + 60;
        let mut agg = MultiTfAggregator::default();
        let mut t = tick(13, SEG_IDX, today_in_session + 86_400, 100.0, 1);
        t.received_at_nanos = 0;
        assert_eq!(t.received_at_nanos, 0, "fixture must exercise the sentinel");
        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
        assert!(
            !stats.future_trading_day,
            "with no second clock the gate must not guess"
        );
        assert_eq!(agg.watermark_secs(), 0);
        let state = agg
            .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
            .expect("local replay");
        assert_ne!(
            state.volume_quality
                & crate::candles::volume_update::VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN,
            0
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

    /// Receipt must not re-date a Dhan trade across IST midnight. The regular
    /// capture window is already closed then; this is a candle-only refusal.
    #[test]
    fn a_dhan_tick_straddling_ist_midnight_keeps_its_event_day() {
        let mut agg = MultiTfAggregator::default();

        let just_before_midnight = DAY - 1;
        let just_after_midnight_utc_secs =
            i64::from(DAY + 1) - crate::candles::tf_index::IST_UTC_OFFSET_SECS;

        let mut t = tick(66_422, SEG_IDX, just_before_midnight, 142.50, 12_000);
        t.received_at_nanos = just_after_midnight_utc_secs * 1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            stats.stale_trading_day,
            "receipt does not re-date a prior-day Dhan LTT into a current-day candle"
        );
        assert!(!stats.folded());
        assert!(agg.is_empty());
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
        // The ten-minute frame crosses exactly one nominal boundary from
        // 09:15 to 09:25 on its 09:00 grid; the hour frame stays in one bucket.
        let m10: Vec<&SealRow> = seals.iter().filter(|r| r.3 == TfIndex::M10).collect();
        assert_eq!(
            m10.len(),
            1,
            "the active 10m frame seals its one populated bucket"
        );
        assert_eq!(m10[0].4, CANDLE_OPEN + 600);
        assert!(
            !seals.iter().any(|r| r.3 == TfIndex::M60),
            "the 60m bucket did not close — it must emit nothing"
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
        for tf in TfIndex::ALL {
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
    /// fixed-timeframe fold + ILP append) has never run"). Memory fitting is not
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

    // ======================================================================
    // HOSTILE REVIEW 2026-09-11 — adversarial probes of the unattributed
    // carry (commit 3ed705a5a). Added by a hostile reviewer; production code
    // untouched.
    // ======================================================================

    /// Sums every emitted bar of one timeframe, last-emission-per-bucket wins
    /// (a Refold amend re-emits its bucket).
    fn hostile_run(seq: &[(u32, u32)]) -> std::collections::HashMap<(TfIndex, u32), u64> {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut bars: std::collections::HashMap<(TfIndex, u32), u64> =
            std::collections::HashMap::new();
        for (off, cum) in seq {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, *cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });
        bars
    }

    fn hostile_total(bars: &std::collections::HashMap<(TfIndex, u32), u64>, want: TfIndex) -> u64 {
        bars.iter()
            .filter(|((tf, _), _)| *tf == want)
            .map(|(_, v)| *v)
            .sum()
    }

    /// FINDING A — a STALE in-bucket packet clears a carry the bucket never
    /// swept, so the carried units reach no bar at all.
    ///
    /// `settle_carry_into_open_bucket` applies only the SIGNED half on the
    /// grounds that "`cumulative - bucket_start` already contains the refused
    /// tick". That is true only while the in-bucket packet's cumulative is at
    /// or above the carried one. A STALE packet carries a SMALLER cumulative
    /// (measured on the live box: security 68407 took 5 regressions before
    /// 09:40 IST on 2026-09-11), the volume-regression guard suppresses the
    /// widening, and the carry is cleared with its gross unswept.
    #[test]
    fn hostile_a_stale_in_bucket_packet_clears_a_carry_it_never_swept() {
        //  off  cum    what it does to the S1 frame
        //   0   1000   seeds the baseline, opens bucket t0 (vol 0)
        //   1   1100   rolls: seals t0(0), opens t1 (vol 100)
        //   2   1300   rolls: seals t1(100), opens t2 (vol 200)
        //   2   1400   in-bucket: t2 vol -> 300, baseline 1400
        //   1   1600   LATE for S1 -> AmendedLate + carry.gross = 200
        //   2   1450   STALE (1450 < 1600) and IN-BUCKET for t2:
        //              vol -> 350 (only 50 of the carry swept)
        //              settle_carry_into_open_bucket CLEARS the other 150
        //   3   1700   rolls: seals t2(350), opens t3 at baseline 1600
        const SEQ: &[(u32, u32)] = &[
            (0, 1_000),
            (1, 1_100),
            (2, 1_300),
            (2, 1_400),
            (1, 1_600),
            (2, 1_450),
            (3, 1_700),
        ];
        let bars = hostile_run(SEQ);
        let expected = 1_700_u64 - 1_000;
        for tf in TfIndex::ALL {
            assert_eq!(
                hostile_total(&bars, tf),
                expected,
                "received ledger for {tf:?}"
            );
        }
        assert_eq!(
            hostile_total(&bars, TfIndex::M1),
            expected,
            "sanity: the minute bar sweeps everything"
        );
        assert_eq!(
            hostile_total(&bars, TfIndex::S1),
            expected,
            "the 1s frames must tile the day exactly -- a carry cleared by a \
             stale in-bucket packet is volume that reached no bar"
        );
    }

    /// FINDING B — a carry outstanding when `catch_up_seal` drains the slot,
    /// with no later tick to open a bucket, is forfeited entirely at the day
    /// boundary. `force_seal` takes the carry BEFORE the uninitialised check
    /// and then returns `None`.
    #[test]
    fn hostile_a_carry_is_forfeited_when_catch_up_seal_drained_the_slot() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut bars: std::collections::HashMap<(TfIndex, u32), u64> =
            std::collections::HashMap::new();
        //   0  1000  seeds, opens S1 t0
        //   1  1100  rolls: seals t0(0), opens t1 (100)
        //   2  1300  rolls: seals t1(100), opens t2 (200); baseline 1300
        //   1  1500  LATE for S1 -> AmendedLate + carry.gross = 200
        for (off, cum) in [(0_u32, 1_000_u32), (1, 1_100), (2, 1_300), (1, 1_500)] {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
        }
        // The watermark-driven sealer drains S1's t2 bucket. The carry stays.
        let _ = agg.catch_up_seal_all(OPEN + 10, |_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });
        // No further tick for this instrument; the day ends.
        agg.force_seal_all(|_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });

        let expected = 1_500_u64 - 1_000;
        for tf in TfIndex::ALL {
            assert_eq!(
                hostile_total(&bars, tf),
                expected,
                "received ledger for {tf:?}"
            );
        }
        assert_eq!(
            hostile_total(&bars, TfIndex::S1),
            expected,
            "a carry outstanding across an intraday catch-up seal must still \
             reach a bar -- force_seal drops it on an uninitialised slot"
        );
    }

    /// FINDING D — a late tick arriving AFTER the watermark drain lost its
    /// units at the day boundary. Found by the fuzz above once its cutoff was
    /// repaired; 796 of 4,000 sequences were losing volume.
    ///
    /// The sibling test
    /// `hostile_a_carry_is_forfeited_when_catch_up_seal_drained_the_slot`
    /// looks like this one and is not: there the late tick arrives BEFORE the
    /// drain, so `catch_up_seal` settles the carry into the bucket it is about
    /// to publish and conservation holds. Reverse the order — drain first,
    /// late tick second — and the carry has no bucket to settle into. The day
    /// then ends, `force_seal` finds an uninitialised slot, and the units are
    /// dropped.
    ///
    /// | off | cum   | effect |
    /// |----:|------:|---|
    /// | 0   | 1_000 | seeds the baseline; opens t0 |
    /// | 1   | 1_100 | rolls: seals t0, opens t1 at the chained endpoint 1_000 |
    /// | —   | —     | `catch_up_seal_all(@2)` drains t1 (100 units); slot uninitialised |
    /// | 1   | 1_500 | LATE for the drained bucket — carried, with no bucket to take it |
    /// | —   | —     | day end: the carry must reach t1, not the floor |
    ///
    /// 1,500 − 1,000 = 500 units traded. t0 is the SEEDING bucket and holds
    /// 0 by construction — with no previous cumulative there is no span for
    /// it to measure — so the whole 500 belongs to t1: the 100 it had already
    /// counted, widened to 500 by the carry the late tick left behind.
    #[test]
    fn hostile_a_late_tick_after_the_drain_must_not_lose_its_units_at_day_end() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut bars: std::collections::HashMap<(TfIndex, u32), u64> =
            std::collections::HashMap::new();

        for (off, cum) in [(0_u32, 1_000_u32), (1, 1_100)] {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
        }
        // The watermark drains t1 — BEFORE the late tick, which is what makes
        // this different from the sibling test.
        let _ = agg.catch_up_seal_all(OPEN + 2, |_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });
        // Now the late packet for the bucket that was just published.
        let late = tick(13, SEG_IDX, OPEN + 1, 100.0, 1_500);
        let _ = agg.consume_tick(Feed::Dhan, &late, None, |_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });
        // No further tick for this instrument; the day ends.
        agg.force_seal_all(|_, _, _, tf, st| {
            bars.insert((tf, st.bucket_start_ist_secs), st.volume);
        });

        let expected = 1_500_u64 - 1_000;
        for tf in TfIndex::ALL {
            assert_eq!(
                hostile_total(&bars, tf),
                expected,
                "received ledger for {tf:?}"
            );
        }
        assert_eq!(
            bars.get(&(TfIndex::S1, OPEN)).copied(),
            Some(0),
            "sanity: the day's first bucket seeds the baseline and measures no \
             span — if this is ever non-zero the arithmetic below moves"
        );
        assert_eq!(
            bars.get(&(TfIndex::S1, OPEN + 1)).copied(),
            Some(500),
            "the drained bucket must be AMENDED from 100 to 500 by the units \
             the late tick left behind — re-emitting it is an UPSERT on the \
             same bucket, exactly what an `AmendedLate` price fix already does. \
             Without the amend it stays at 100 and 400 units reach no bar"
        );
        assert_eq!(
            hostile_total(&bars, TfIndex::S1),
            expected,
            "every unit that traded must land in some S1 bar"
        );
    }

    /// Historical regression C: resetting the cumulative axis must preserve
    /// gross volume and its classification together.
    ///
    /// The old path cleared carry while leaving its signed contribution in
    /// the open candle. Carry now settles before rebasing, so both quantities
    /// remain on the same volume ledger.
    ///
    /// Result: a bar whose `net_volume_signed` exceeds its own `volume`,
    /// which `net_volume()` silently CLAMPS to `±volume` — publishing
    /// "100% of this bar's flow was one-directional" about flow that traded
    /// before the counter restarted.
    #[test]
    fn hostile_a_carried_sign_survives_a_counter_restart_and_exceeds_the_bars_volume() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut s1: Vec<(u32, LiveCandleState)> = Vec::new();
        let push = |tf: TfIndex, st: LiveCandleState, out: &mut Vec<(u32, LiveCandleState)>| {
            if tf == TfIndex::S1 {
                out.push((st.bucket_start_ist_secs, st));
            }
        };

        //  off  cum             price  what it does to the S1 frame
        //   0   3_000_000_000   100    seeds; opens t0
        //   1   3_000_000_500   100    rolls: seals t0(0), opens t1 (500)
        //   2   3_000_001_000   100    rolls: seals t1(500), opens t2 (500)
        //   1   3_000_002_000   101    LATE -> AmendedLate; carry {gross 1000, net +1000}
        //   3   100             101    u32 WRAP: rolls t2, opens t3 seeded with carry.net
        //                              = +1000 and volume 0; restart re-anchor follows
        //   3   300             101    in-bucket: t3 volume -> 200, net -> +1200
        //   4   400             101    rolls t3 and publishes it
        const SEQ: &[(u32, u32, f32)] = &[
            (0, 3_000_000_000, 100.0),
            (1, 3_000_000_500, 100.0),
            (2, 3_000_001_000, 100.0),
            (1, 3_000_002_000, 101.0),
            (3, 100, 101.0),
            (3, 300, 101.0),
            (4, 400, 101.0),
        ];
        for (off, cum, px) in SEQ {
            let t = tick(13, SEG_IDX, OPEN + off, *px, *cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                push(tf, st, &mut s1)
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| push(tf, st, &mut s1));

        let mut by_bucket: std::collections::HashMap<u32, LiveCandleState> =
            std::collections::HashMap::new();
        for (start, st) in s1 {
            by_bucket.insert(start, st);
        }
        let t3 = by_bucket
            .get(&(OPEN + 3))
            .copied()
            .expect("the post-restart bucket must have been published");
        println!(
            "HOSTILE C: post-restart S1 bar volume={} net_volume_signed={} net_volume()={:?}",
            t3.volume,
            t3.net_volume_signed,
            t3.net_volume()
        );
        assert!(
            u64::try_from(t3.net_volume_signed.abs()).unwrap_or(u64::MAX) <= t3.volume,
            "net_volume_signed ({}) exceeds the bar's own volume ({}) — the \
             carried sign crossed a counter restart its gross did not",
            t3.net_volume_signed,
            t3.volume
        );
    }

    /// FINDING C2 — the same leak, tuned so the clamp INVERTS the sign.
    ///
    /// The carried net is a SELL (-1,000) from before the counter restart.
    /// The post-restart bar's own and only classified flow is a BUY (+200).
    /// `net_volume_signed` becomes -800, `net_volume()` clamps it to -200, and
    /// the bar publishes "every unit of this bar's volume was sell-initiated"
    /// about 200 units that were, on this fold's own classification,
    /// buy-initiated.
    #[test]
    fn hostile_a_carried_sign_across_a_restart_inverts_the_published_net() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut s1: Vec<(u32, LiveCandleState)> = Vec::new();
        const SEQ: &[(u32, u32, f32)] = &[
            (0, 3_000_000_000, 100.0),
            (1, 3_000_000_500, 100.0),
            (2, 3_000_001_000, 100.0),
            (1, 3_000_002_000, 99.0), // LATE downtick -> carry.net = -1000
            (3, 100, 99.0),           // u32 wrap
            (3, 300, 105.0),          // post-restart BUY of 200 units
            (4, 400, 106.0),
        ];
        for (off, cum, px) in SEQ {
            let t = tick(13, SEG_IDX, OPEN + off, *px, *cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                if tf == TfIndex::S1 {
                    s1.push((st.bucket_start_ist_secs, st));
                }
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::S1 {
                s1.push((st.bucket_start_ist_secs, st));
            }
        });
        let mut by_bucket: std::collections::HashMap<u32, LiveCandleState> =
            std::collections::HashMap::new();
        for (start, st) in s1 {
            by_bucket.insert(start, st);
        }
        let t3 = by_bucket.get(&(OPEN + 3)).copied().expect("published");
        println!(
            "HOSTILE C2: volume={} net_signed={} net_volume()={:?}",
            t3.volume,
            t3.net_volume_signed,
            t3.net_volume()
        );
        assert_eq!(
            t3.net_volume(),
            None,
            "a drop followed by a recovery cannot certify a new counter axis"
        );
    }

    /// FINDING C3 — the OTHER `chain_broken` consumption site, and the one
    /// that fails SILENTLY for a whole bucket.
    ///
    /// The two restart tests above restart with an open bucket. This one
    /// restarts after the watermark sealer drained it. The retained sealed
    /// predecessor must also be rebased before it can seed a later bucket.
    ///
    /// The original bug kept `last_sealed` on the erased cumulative axis.
    /// Chaining the next bucket to it computed `volume = cumulative − start` against a
    /// number the post-restart counter may not reach for hours, so the bar
    /// publishes **0 volume with a rising tick count** — no error, no
    /// counter, nothing to see. The instrument simply stops reporting flow.
    ///
    /// Sequence (S1 frame, `u32` wrap between step 3 and step 4):
    ///
    /// | off | cum           | effect |
    /// |----:|--------------:|---|
    /// | 0   | 3_000_000_000 | seeds; opens t0 |
    /// | 0   | 3_000_000_500 | in-bucket; t0 volume 500 |
    /// | —   | —             | `catch_up_seal_all` drains t0; slot uninitialised, `last_sealed` endpoint 3_000_000_500 |
    /// | 5   | 200           | WRAP. No open bucket to rebase, so the flag is taken HERE; t5 opens at 200 |
    /// | 5   | 900           | in-bucket; t5 volume 700 — the real post-restart flow |
    /// | 6   | 1_000         | rolls t5 and publishes it |
    ///
    /// Expected: t5 reports **700**. Without the broken chain it reports 0,
    /// because `200 − 3_000_000_500` saturates.
    #[test]
    fn hostile_a_restart_with_no_open_bucket_must_not_chain_to_the_erased_axis() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut s1: Vec<(u32, LiveCandleState)> = Vec::new();
        let push = |tf: TfIndex, st: LiveCandleState, out: &mut Vec<(u32, LiveCandleState)>| {
            if tf == TfIndex::S1 {
                out.push((st.bucket_start_ist_secs, st));
            }
        };

        for (off, cum) in [(0_u32, 3_000_000_000_u32), (0, 3_000_000_500)] {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                push(tf, st, &mut s1)
            });
        }
        // The watermark sealer drains S1's open bucket. `last_sealed` now
        // holds a bar whose right endpoint is 3_000_000_500 — a number the
        // post-wrap counter will never reach.
        let _ = agg.catch_up_seal_all(OPEN + 4, |_, _, _, tf, st| push(tf, st, &mut s1));

        for (off, cum) in [(5_u32, 200_u32), (5, 900), (6, 1_000)] {
            let t = tick(13, SEG_IDX, OPEN + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                push(tf, st, &mut s1)
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| push(tf, st, &mut s1));

        let mut by_bucket: std::collections::HashMap<u32, LiveCandleState> =
            std::collections::HashMap::new();
        for (start, st) in s1 {
            by_bucket.insert(start, st);
        }
        let t5 = by_bucket
            .get(&(OPEN + 5))
            .copied()
            .expect("the first post-restart bucket must have been published");
        println!(
            "HOSTILE C3: post-restart-no-open-bucket S1 bar volume={} tick_count={}",
            t5.volume, t5.tick_count
        );
        assert!(
            t5.tick_count > 0,
            "sanity: the bar must have seen ticks at all"
        );
        assert_eq!(
            t5.volume, 0,
            "same-session decreases remain ambiguous until an explicit session transition"
        );
    }

    /// FUZZ — `|net_volume_signed| <= volume` on EVERY emitted bar. The
    /// doc on `LiveCandleState::net_volume` states this as a structural fact;
    /// `net_volume()` clamps on top of it, so a violation is invisible to a
    /// reader and shows up only here.
    #[test]
    fn hostile_fuzz_net_never_exceeds_gross_on_any_emitted_bar() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut violations: Vec<(Vec<(u32, u32, f32)>, TfIndex, u32, i64, u64)> = Vec::new();
        for _case in 0..3_000 {
            let n = 6 + (next() % 8) as usize;
            let mut seq: Vec<(u32, u32, f32)> = vec![(0, 1_000, 100.0)];
            let mut cum: u32 = 1_000;
            let mut off: u32 = 0;
            let mut px: f32 = 100.0;
            for _ in 0..n {
                off = if next() % 10 < 7 {
                    off + 1 + (next() % 2) as u32
                } else {
                    off.saturating_sub(1 + (next() % 3) as u32)
                };
                cum = if next() % 10 < 8 {
                    cum + 50 + (next() % 200) as u32
                } else {
                    cum.saturating_sub(10 + (next() % 120) as u32)
                };
                px = match next() % 3 {
                    0 => px + 1.0,
                    1 => (px - 1.0).max(1.0),
                    _ => px,
                };
                seq.push((off.min(110), cum, px));
            }

            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            let mut bars: std::collections::HashMap<(TfIndex, u32), LiveCandleState> =
                std::collections::HashMap::new();
            for (off, cum, px) in &seq {
                let t = tick(13, SEG_IDX, OPEN + off, *px, *cum);
                let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st);
                });
            }
            agg.force_seal_all(|_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st);
            });
            for ((tf, start), st) in &bars {
                let mag = u64::try_from(st.net_volume_signed.abs()).unwrap_or(u64::MAX);
                if mag > st.volume {
                    violations.push((seq.clone(), *tf, *start, st.net_volume_signed, st.volume));
                    break;
                }
            }
        }
        // Hoisted, and `first()` rather than `[0]`: the indexed form is valid
        // ONLY because the assert short-circuits, so it reads as a panic
        // waiting for a careless edit. This evaluates on every run and cannot
        // panic on an empty vec.
        let first = violations.first().map_or_else(
            || "<none>".to_string(),
            |(seq, tf, start, net, vol)| {
                format!("tf={tf:?} bucket={start} net={net} volume={vol} seq={seq:?}")
            },
        );
        assert!(
            violations.is_empty(),
            "{} of 3000 sequences published a bar whose |net| exceeds its own \
             volume. First: {first}",
            violations.len()
        );
    }

    /// FINDING D — no counter restart needed. `settle_carry_into_open_bucket`
    /// adds the carried NET on the stated grounds that the GROSS "is already
    /// swept by `cumulative − bucket_start`". When the settling packet is
    /// STALE that sweep is suppressed by the volume-regression guard, so the
    /// bar receives the sign of units it does not hold.
    ///
    /// `net_volume()` then clamps and publishes `±volume` — maximum
    /// conviction — for a bar whose own classified flow was smaller.
    #[test]
    fn hostile_a_stale_settling_packet_gives_a_bar_a_net_it_has_no_volume_for() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut s1: Vec<(u32, LiveCandleState)> = Vec::new();
        //  off  cum    price   S1
        //   0   1000   100     seed; open t0
        //   1   1100   101     roll: seal t0(0), open t1 (vol 100, net +100)
        //   2   1200   102     roll: seal t1, open t2 (vol 100, net +100)
        //   1   1500   103     LATE -> AmendedLate; carry {gross 300, net +300}
        //   2   1250    99     STALE + in-bucket: vol -> 150 only; carry's NET
        //                      (+300) is applied, its GROSS is discarded
        //   3   1600   104     roll: publishes t2
        const SEQ: &[(u32, u32, f32)] = &[
            (0, 1_000, 100.0),
            (1, 1_100, 101.0),
            (2, 1_200, 102.0),
            (1, 1_500, 103.0),
            (2, 1_250, 99.0),
            (3, 1_600, 104.0),
        ];
        for (off, cum, px) in SEQ {
            let t = tick(13, SEG_IDX, OPEN + off, *px, *cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                if tf == TfIndex::S1 {
                    s1.push((st.bucket_start_ist_secs, st));
                }
            });
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::S1 {
                s1.push((st.bucket_start_ist_secs, st));
            }
        });
        let mut by_bucket: std::collections::HashMap<u32, LiveCandleState> =
            std::collections::HashMap::new();
        for (start, st) in s1 {
            by_bucket.insert(start, st);
        }
        let t2 = by_bucket.get(&(OPEN + 2)).copied().expect("published");
        println!(
            "HOSTILE D: bucket t2 volume={} net_signed={} net_volume()={:?}",
            t2.volume,
            t2.net_volume_signed,
            t2.net_volume()
        );
        assert!(
            u64::try_from(t2.net_volume_signed.abs()).unwrap_or(u64::MAX) <= t2.volume,
            "|net_volume_signed| ({}) exceeds the bar's own volume ({}) with no \
             counter restart anywhere — the carry's sign was settled without \
             its gross",
            t2.net_volume_signed,
            t2.volume
        );
    }

    /// FUZZ, CONTROL — the net invariant under a strictly rising cumulative.
    #[test]
    fn hostile_fuzz_control_net_invariant_holds_when_cumulative_is_monotonic() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut violations = 0_usize;
        for _case in 0..3_000 {
            let n = 6 + (next() % 8) as usize;
            let mut seq: Vec<(u32, u32, f32)> = vec![(0, 1_000, 100.0)];
            let mut cum: u32 = 1_000;
            let mut off: u32 = 0;
            let mut px: f32 = 100.0;
            for _ in 0..n {
                off = if next() % 10 < 7 {
                    off + 1 + (next() % 2) as u32
                } else {
                    off.saturating_sub(1 + (next() % 3) as u32)
                };
                let _ = next();
                cum += 50 + (next() % 200) as u32;
                px = match next() % 3 {
                    0 => px + 1.0,
                    1 => (px - 1.0).max(1.0),
                    _ => px,
                };
                seq.push((off.min(110), cum, px));
            }
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            let mut bars: std::collections::HashMap<(TfIndex, u32), LiveCandleState> =
                std::collections::HashMap::new();
            for (off, cum, px) in &seq {
                let t = tick(13, SEG_IDX, OPEN + off, *px, *cum);
                let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st);
                });
            }
            agg.force_seal_all(|_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st);
            });
            if bars
                .values()
                .any(|st| u64::try_from(st.net_volume_signed.abs()).unwrap_or(u64::MAX) > st.volume)
            {
                violations += 1;
            }
        }
        assert_eq!(violations, 0, "monotonic control broke the net invariant");
    }

    /// The conservation fuzzes' violation LEDGER, extracted from the fuzz body
    /// so the reporting path has a test of its own.
    ///
    /// A fuzz that passes never executes its own failure branch, so the report
    /// a future debugger reads at 3am is text nothing has ever run. That is the
    /// same shape as the defect this file's own history records — a cutoff that
    /// ran 4,000 cases and sealed NOTHING, so the invariant it claimed to prove
    /// was never once evaluated. The report is not decoration: it is the whole
    /// output of the fuzz on the one run that matters.
    ///
    /// The over/under split is kept because a DOUBLE COUNT and a LOSS have
    /// opposite causes and want opposite fixes; collapsing them into one
    /// "failures" number would throw away the first thing you need to know.
    #[derive(Default)]
    struct ConservationTally {
        over: usize,
        under: usize,
        first_over: Option<String>,
        first_under: Option<String>,
    }

    impl ConservationTally {
        /// Records one case. Equality is NOT a violation and must leave every
        /// field untouched — the fuzz calls this on all 4,000 cases, so a
        /// mis-handled equal case would count every passing run as a failure.
        fn record(&mut self, s1: u64, expected: u64, seq: &[String]) {
            if s1 == expected {
                return;
            }
            let (count, first) = if s1 > expected {
                (&mut self.over, &mut self.first_over)
            } else {
                (&mut self.under, &mut self.first_under)
            };
            *count += 1;
            // `get_or_insert_with`, so the FIRST violation of each kind is the
            // one retained. The earliest reproducer is the cheapest to debug;
            // overwriting it with the latest would hand back the longest.
            first.get_or_insert_with(|| {
                format!("S1={s1} expected={expected} seq={}", seq.join(" "))
            });
        }

        fn summary(&self) -> String {
            format!(
                "over(DOUBLE COUNT)={} under(LOSS)={} / 4000\n  first over: {:?}\n  \
                 first under: {:?}",
                self.over, self.under, self.first_over, self.first_under
            )
        }
    }

    /// The ledger above is the fuzzes' only output on a failing run, and a
    /// passing fuzz never exercises it. This is that missing test.
    #[test]
    fn the_conservation_ledger_splits_by_direction_and_keeps_the_first() {
        let mut tally = ConservationTally::default();

        // Equal is not a violation: nothing moves. This is the case that runs
        // 4,000 times on a healthy tree.
        tally.record(500, 500, &["(0,1000)".to_string()]);
        assert_eq!((tally.over, tally.under), (0, 0));
        assert!(tally.first_over.is_none() && tally.first_under.is_none());

        // More volume than the ground truth = DOUBLE COUNT.
        tally.record(700, 500, &["(0,1000)".to_string(), "(1,1200)".to_string()]);
        assert_eq!((tally.over, tally.under), (1, 0));
        assert_eq!(
            tally.first_over.as_deref(),
            Some("S1=700 expected=500 seq=(0,1000) (1,1200)")
        );

        // Less than the ground truth = LOSS, and it must land on the OTHER
        // counter. A single shared counter would report a loss as a double
        // count and send the next debugger at the opposite bug.
        tally.record(300, 500, &["(0,1000)".to_string()]);
        assert_eq!((tally.over, tally.under), (1, 1));
        assert_eq!(
            tally.first_under.as_deref(),
            Some("S1=300 expected=500 seq=(0,1000)")
        );

        // A later violation increments the count but must NOT replace the
        // retained reproducer.
        tally.record(900, 500, &["(0,9999)".to_string()]);
        assert_eq!((tally.over, tally.under), (2, 1));
        assert_eq!(
            tally.first_over.as_deref(),
            Some("S1=700 expected=500 seq=(0,1000) (1,1200)"),
            "the FIRST over-report must survive a later one"
        );

        // The summary names both directions and carries both reproducers.
        let summary = tally.summary();
        assert!(summary.contains("over(DOUBLE COUNT)=2"), "{summary}");
        assert!(summary.contains("under(LOSS)=1"), "{summary}");
        assert!(summary.contains("S1=700 expected=500"), "{summary}");
        assert!(summary.contains("S1=300 expected=500"), "{summary}");
    }

    /// FUZZ — conservation with the watermark-driven `catch_up_seal_all`
    /// interleaved, which is how the live drain actually runs. Hunting a
    /// DOUBLE COUNT as hard as a loss: a carry that survives a catch-up drain
    /// and is then settled into a bucket that had already swept it would
    /// INVENT volume.
    #[test]
    fn hostile_fuzz_conservation_with_catch_up_seals_interleaved() {
        let mut state = 0xD1B5_4A32_D192_ED03_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut tally = ConservationTally::default();
        for _case in 0..4_000 {
            let n = 6 + (next() % 10) as usize;
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            let mut bars: std::collections::HashMap<(TfIndex, u32), u64> =
                std::collections::HashMap::new();
            let mut log: Vec<String> = Vec::new();
            let mut cum: u32 = 1_000;
            let mut off: u32 = 0;
            let first_cum = cum;
            let mut max_cum = cum;
            {
                let t = tick(13, SEG_IDX, OPEN, 100.0, cum);
                let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st.volume);
                });
                log.push(format!("({off},{cum})"));
            }
            for _ in 0..n {
                off = if next() % 10 < 7 {
                    off + 1 + (next() % 2) as u32
                } else {
                    off.saturating_sub(1 + (next() % 3) as u32)
                };
                cum = if next() % 10 < 8 {
                    cum + 50 + (next() % 200) as u32
                } else {
                    cum.saturating_sub(10 + (next() % 120) as u32)
                };
                let off = off.min(110);
                max_cum = max_cum.max(cum);
                let t = tick(13, SEG_IDX, OPEN + off, 100.0, cum);
                let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st.volume);
                });
                log.push(format!("({off},{cum})"));
                // The live drain sweeps on a timer; imitate it.
                //
                // CUTOFF `off + 1`, not `off - 1`. The first version of this
                // fuzz used `off - 1` and therefore SEALED NOTHING, ever: an
                // S1 bucket at `OPEN + off` ends at `OPEN + off + 1`, which is
                // never <= `OPEN + off - 1`, and the generator's `off` never
                // climbs far enough (max ~30 over 15 steps) for a MINUTE
                // bucket to end before the cutoff either. The test ran 4,000
                // cases, logged `catchup(@N)` each time, and drained not one
                // bar — a fuzz that passed while testing nothing, found by its
                // own emit closure showing as never-executed under llvm-cov.
                //
                // `off + 1` is the realistic cutoff, not a contrived one: the
                // live drain's watermark is driven by the whole feed, so it is
                // routinely AHEAD of any single instrument's last tick. It
                // drains the bucket this instrument still has open, and the
                // generator then sends ticks at EARLIER offsets 30% of the
                // time — the drained-slot-then-late-tick shape that
                // `hostile_a_carry_is_forfeited_when_catch_up_seal_drained_the_slot`
                // reproduces one case at a time.
                if next() % 3 == 0 {
                    let cutoff = OPEN + off + 1;
                    let _ = agg.catch_up_seal_all(cutoff, |_, _, _, tf, st| {
                        bars.insert((tf, st.bucket_start_ist_secs), st.volume);
                    });
                    log.push(format!("catchup(@{})", off + 1));
                }
            }
            agg.force_seal_all(|_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
            let s1: u64 = bars
                .iter()
                .filter(|((tf, _), _)| *tf == TfIndex::S1)
                .map(|(_, v)| *v)
                .sum();
            // Independent received-counter ledger: a shared candle defect
            // cannot hide behind agreement with another timeframe.
            let expected = u64::from(max_cum - first_cum);
            tally.record(s1, expected, &log);
        }
        println!("HOSTILE CATCHUP FUZZ: {}", tally.summary());
        assert_eq!(
            (tally.over, tally.under),
            (0, 0),
            "conservation broken under interleaved catch-up seals"
        );
    }

    /// FUZZ, CONTROL — identical generator but the cumulative NEVER goes
    /// backwards. If this passes while the unrestricted fuzz fails, the stale
    /// packet is the whole cause.
    #[test]
    fn hostile_fuzz_control_monotonic_cumulative_conserves() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut failures = 0_usize;
        let mut first: Option<(Vec<(u32, u32)>, TfIndex, u64, u64)> = None;
        for _case in 0..4_000 {
            let n = 6 + (next() % 8) as usize;
            let mut seq: Vec<(u32, u32)> = vec![(0, 1_000)];
            let mut cum: u32 = 1_000;
            let mut off: u32 = 0;
            for _ in 0..n {
                let r = next() % 10;
                off = if r < 7 {
                    off + 1 + (next() % 2) as u32
                } else {
                    off.saturating_sub(1 + (next() % 3) as u32)
                };
                let _ = next();
                cum += 50 + (next() % 200) as u32;
                seq.push((off.min(110), cum));
            }
            let bars = hostile_run(&seq);
            let expected = u64::from(cum - 1_000);
            for tf in TfIndex::ALL {
                let actual = hostile_total(&bars, tf);
                if actual != expected {
                    failures += 1;
                    if first.is_none() {
                        first = Some((seq.clone(), tf, actual, expected));
                    }
                }
            }
        }
        assert_eq!(failures, 0, "monotonic control failed: {first:?}");
    }

    /// Unlike the older three-frame comparison, check every registered frame
    /// against the received cumulative ledger, not against another candle.
    #[test]
    fn all_active_frames_preserve_late_carry_across_counter_restarts() {
        use std::collections::HashMap;

        for strategy in [FeedStrategy::REFOLD, FeedStrategy::DISCARD] {
            for catch_up in [false, true] {
                for reset_offset in [10_u32, 60, 61, 120] {
                    for (prices, direction) in [
                        ([100.0_f32, 101.0, 102.0, 103.0, 104.0, 105.0], 1_i64),
                        ([100.0, 99.0, 98.0, 97.0, 96.0, 95.0], -1),
                        ([100.0, 100.0, 100.0, 100.0, 100.0, 100.0], 0),
                    ] {
                        let mut agg = MultiTfAggregator::new(strategy);
                        let mut latest = HashMap::new();
                        for (off, cum, price) in [
                            (0_u32, 3_000_000_000_u32, prices[0]),
                            (60, 3_000_000_100, prices[1]),
                        ] {
                            let stats = agg.consume_tick(
                                Feed::Dhan,
                                &tick(77, SEG_IDX, OPEN + off, price, cum),
                                None,
                                |_, _, _, tf, state| {
                                    latest.insert((tf, state.bucket_start_ist_secs), state);
                                },
                            );
                            assert!(stats.folded());
                        }
                        if catch_up {
                            agg.catch_up_seal_all(OPEN + 120, |_, _, _, tf, state| {
                                latest.insert((tf, state.bucket_start_ist_secs), state);
                            });
                        }
                        let m1 = agg
                            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::M1)
                            .expect("tracked instrument");
                        assert_eq!(m1.is_uninitialised(), catch_up);
                        // Accepted +50 arrives too late for M1. Before the
                        // fix a restart erased its pending gross and sign.
                        for (index, (off, cum)) in [
                            (10_u32, 3_000_000_150_u32),
                            (reset_offset, 10),
                            (reset_offset, 15),
                            (121, 20),
                        ]
                        .into_iter()
                        .enumerate()
                        {
                            let stats = agg.consume_tick(
                                Feed::Dhan,
                                &tick(77, SEG_IDX, OPEN + off, prices[index + 2], cum),
                                None,
                                |_, _, _, tf, state| {
                                    latest.insert((tf, state.bucket_start_ist_secs), state);
                                },
                            );
                            assert!(stats.folded());
                            if index >= 1 {
                                let index = agg.lookup(Feed::Dhan, 77, SEG_IDX).expect("slot");
                                assert!(agg.slots[index].volume_counter.is_ambiguous());
                            }
                        }
                        agg.force_seal_all(|_, _, _, tf, state| {
                            latest.insert((tf, state.bucket_start_ist_secs), state);
                        });
                        for tf in TfIndex::ALL {
                            let mut count = 0;
                            let mut gross = 0_u64;
                            for ((frame, _), state) in &latest {
                                if *frame != tf {
                                    continue;
                                }
                                count += 1;
                                gross += state.volume;
                                if state.volume > 0 {
                                    let expected_net = if state.bucket_open_prev_close <= 0.0
                                        || state.volume_quality & crate::candles::volume_update::VOLUME_QUALITY_COUNTER_AMBIGUOUS != 0
                                    {
                                        None
                                    } else {
                                        Some(
                                            direction
                                                * i64::try_from(state.volume)
                                                    .expect("small volume"),
                                        )
                                    };
                                    assert_eq!(
                                        state.net_volume(),
                                        expected_net,
                                        "frame {tf:?}, catch_up {catch_up}, reset {reset_offset}, direction {direction}"
                                    );
                                }
                            }
                            assert!(count > 0, "missing frame {tf:?}");
                            // Independent conservative ledger: +100 and +50
                            // before an unresolved same-session decrease. A
                            // guessed new axis must not add another ten.
                            assert_eq!(
                                gross, 150,
                                "frame {tf:?}, catch_up {catch_up}, reset {reset_offset}, direction {direction}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn all_active_frames_preserve_late_carry_through_repeated_counter_restarts() {
        use std::collections::HashMap;

        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut latest = HashMap::new();
        for (index, (off, cumulative)) in [
            (0_u32, 3_000_000_000_u32),
            (60, 3_000_000_100),
            (10, 3_000_000_150),
            (61, 10),
            (62, 20),
            (180, 3_000_000_020),
            (121, 3_000_000_050),
            (181, 5),
            (241, 15),
        ]
        .into_iter()
        .enumerate()
        {
            if index == 6 {
                agg.catch_up_seal_all(OPEN + 240, |_, _, _, tf, state| {
                    latest.insert((tf, state.bucket_start_ist_secs), state);
                });
            }
            let price = 100.0 + f32::from(u16::try_from(index).expect("short trace"));
            let stats = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, OPEN + off, price, cumulative),
                None,
                |_, _, _, tf, state| {
                    latest.insert((tf, state.bucket_start_ist_secs), state);
                },
            );
            assert!(stats.folded());
        }
        agg.force_seal_all(|_, _, _, tf, state| {
            latest.insert((tf, state.bucket_start_ist_secs), state);
        });
        for tf in TfIndex::ALL {
            let gross: u64 = latest
                .iter()
                .filter(|((frame, _), _)| *frame == tf)
                .map(|(_, state)| state.volume)
                .sum();
            // Only +100 and +50 exceed the original baseline; none of the
            // ambiguous recoveries reaches its accepted high-water mark.
            assert_eq!(gross, 150, "frame {tf:?}");
        }
    }

    #[test]
    fn all_active_frames_conserve_accepted_volume_under_reorder_and_catch_up() {
        use std::collections::HashMap;
        let mut seed = 0xA11F_24CA_9D1E_2026_u64;
        for case in 0..256 {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            let mut bars = HashMap::new();
            let mut cumulative = 1_000_u32;
            let mut accepted_high = cumulative;
            let mut offset = 0_u32;
            let first = tick(13, SEG_IDX, OPEN, 100.0, cumulative);
            let _ = agg.consume_tick(Feed::Dhan, &first, None, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
            let mut drained = 0;
            for step in 0..64 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                offset = if seed % 4 == 0 {
                    offset.saturating_sub(3)
                } else {
                    offset + 2
                };
                cumulative = if seed % 7 == 0 {
                    cumulative.saturating_sub(17)
                } else {
                    cumulative + 31 + u32::try_from(seed % 100).expect("small delta")
                };
                accepted_high = accepted_high.max(cumulative);
                let current = tick(13, SEG_IDX, OPEN + offset, 100.0, cumulative);
                let _ = agg.consume_tick(Feed::Dhan, &current, None, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st.volume);
                });
                if step % 5 == 0 {
                    drained += agg.catch_up_seal_all(OPEN + offset + 1, |_, _, _, tf, st| {
                        bars.insert((tf, st.bucket_start_ist_secs), st.volume);
                    });
                }
            }
            agg.force_seal_all(|_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st.volume);
            });
            assert!(drained > 0, "fixture must actually execute catch-up seals");
            let expected = u64::from(accepted_high - 1_000);
            assert!(expected > 0);
            for tf in TfIndex::ALL {
                let mut count = 0;
                let total: u64 = bars
                    .iter()
                    .filter_map(|((frame, _), volume)| {
                        if *frame == tf {
                            count += 1;
                            Some(*volume)
                        } else {
                            None
                        }
                    })
                    .sum();
                assert!(count > 0, "missing frame {tf:?}, case {case}");
                assert_eq!(total, expected, "frame {tf:?}, case {case}");
            }
        }
    }

    /// Warm baseline immediately before 09:15, then an uninterrupted full
    /// session. Independent period arithmetic checks EVERY persisted bucket
    /// label, gross delta and signed delta across all ten active frames.
    /// This is a synthetic fold test, not proof of broker or DB completeness.
    #[test]
    fn all_active_frames_match_per_bucket_gross_and_net_through_full_session_boundaries() {
        use std::collections::HashMap;
        const PERIODS: [(TfIndex, u32); 10] = [
            (TfIndex::S1, 1),
            (TfIndex::S3, 3),
            (TfIndex::S5, 5),
            (TfIndex::M1, 60),
            (TfIndex::M3, 180),
            (TfIndex::M5, 300),
            (TfIndex::M10, 600),
            (TfIndex::M15, 900),
            (TfIndex::M30, 1_800),
            (TfIndex::M60, 3_600),
        ];
        assert_eq!(PERIODS.len(), TfIndex::ALL.len());
        for tf in TfIndex::ALL {
            assert_eq!(PERIODS.iter().filter(|(frame, _)| *frame == tf).count(), 1);
        }
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut expected: std::collections::BTreeMap<(TfIndex, u32), (u64, f64)> =
            std::collections::BTreeMap::new();
        let mut actual = HashMap::new();
        let anchor = OPEN - 15 * 60; // 09:00, independent of bucket_start().
        let seed_ts = OPEN - 1;
        let mut cumulative = 10_000_u32;
        let mut first = tick(13, SEG_IDX, seed_ts, 100.0, cumulative);
        first.received_at_nanos = (i64::from(seed_ts) - 19_800) * 1_000_000_000;
        let _ = agg.consume_tick(Feed::Dhan, &first, None, |_, _, _, tf, st| {
            actual.insert((tf, st.bucket_start_ist_secs), (st.volume, st.net_volume()));
        });
        for (tf, period) in PERIODS {
            expected.insert(
                (tf, anchor + (seed_ts - anchor) / period * period),
                (0, 100.0),
            );
        }
        let mut received_total = 0_u64;
        let mut drained = 0;
        // 09:15:00 through 15:39:59 inclusive: 23,100 received deltas.
        for second in 0..23_100_u32 {
            let ts = OPEN + second;
            let delta = 1 + second % 97;
            let buy = second % 2 == 0;
            cumulative += delta;
            received_total += u64::from(delta);
            // Generate every intended event second, with receipt one second
            // later. Dhan LTT owns the bucket; the final valid 15:39:59 print
            // consequently arrives after the 15:40 capture boundary.
            let mut current = tick(13, SEG_IDX, ts, if buy { 101.0 } else { 100.0 }, cumulative);
            current.received_at_nanos = (i64::from(ts + 1) - 19_800) * 1_000_000_000 + 100_000_000;
            for (tf, period) in PERIODS {
                let key = (tf, anchor + (ts - anchor) / period * period);
                let total = expected.entry(key).or_default();
                total.0 += u64::from(delta);
                total.1 = if buy { 101.0 } else { 100.0 };
            }
            let _ = agg.consume_tick(Feed::Dhan, &current, None, |_, _, _, tf, st| {
                actual.insert((tf, st.bucket_start_ist_secs), (st.volume, st.net_volume()));
            });
            if second % 67 == 0 {
                drained += agg.catch_up_seal_all(ts + 1, |_, _, _, tf, st| {
                    actual.insert((tf, st.bucket_start_ist_secs), (st.volume, st.net_volume()));
                });
            }
        }
        agg.force_seal_all(|_, _, _, tf, st| {
            actual.insert((tf, st.bucket_start_ist_secs), (st.volume, st.net_volume()));
        });
        assert!(drained > 0);
        assert_eq!(
            actual.len(),
            expected.len(),
            "no missing or invented bucket labels"
        );
        let mut previous_close: HashMap<TfIndex, f64> = HashMap::new();
        for (key, (gross, close)) in &expected {
            let expected_net = previous_close.insert(key.0, *close).map(|previous| {
                let magnitude = i64::try_from(*gross).expect("bounded fixture");
                if *close > previous {
                    magnitude
                } else if *close < previous {
                    -magnitude
                } else {
                    0
                }
            });
            assert_eq!(
                actual.get(key),
                Some(&(*gross, expected_net)),
                "bucket {key:?}"
            );
        }
        for tf in TfIndex::ALL {
            let total: u64 = actual
                .iter()
                .filter_map(|((frame, _), (gross, _))| (*frame == tf).then_some(*gross))
                .sum();
            assert_eq!(total, received_total, "full-session ledger {tf:?}");
        }
    }

    /// FUZZ — every active frame must match the independently accepted
    /// cumulative ledger over reordered and occasionally stale observations.
    /// No candle timeframe is allowed to certify another candle's arithmetic.
    #[test]
    fn hostile_fuzz_every_frame_tiles_the_day_to_one_total() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut failures: Vec<(Vec<(u32, u32)>, TfIndex, u64, u64)> = Vec::new();
        for _case in 0..4_000 {
            let n = 6 + (next() % 8) as usize;
            let mut seq: Vec<(u32, u32)> = vec![(0, 1_000)];
            let mut cum: u32 = 1_000;
            let mut off: u32 = 0;
            for _ in 0..n {
                // Mostly forward in time, sometimes a late arrival.
                let r = next() % 10;
                off = if r < 7 {
                    off + 1 + (next() % 2) as u32
                } else {
                    off.saturating_sub(1 + (next() % 3) as u32)
                };
                // Mostly rising cumulative, sometimes a stale (smaller) one.
                let s = next() % 10;
                cum = if s < 8 {
                    cum + 50 + (next() % 200) as u32
                } else {
                    cum.saturating_sub(10 + (next() % 120) as u32)
                };
                seq.push((off.min(110), cum));
            }
            let bars = hostile_run(&seq);
            let expected = u64::from(seq.iter().map(|(_, value)| *value).max().unwrap() - 1_000);
            for tf in TfIndex::ALL {
                let actual = hostile_total(&bars, tf);
                if actual != expected {
                    failures.push((seq.clone(), tf, actual, expected));
                }
            }
        }
        let over = failures
            .iter()
            .filter(|(_, _, actual, expected)| actual > expected)
            .count();
        let under = failures
            .iter()
            .filter(|(_, _, actual, expected)| actual < expected)
            .count();
        let stale = failures
            .iter()
            .filter(|(seq, _, _, _)| seq.windows(2).any(|w| w[1].1 < w[0].1))
            .count();
        println!(
            "HOSTILE FUZZ: {} frame failures / {} comparisons; double counts: {over}; \
             losses: {under}; failures with stale cumulative observations: {stale}",
            failures.len(),
            4_000 * TF_COUNT
        );
        let first_three = failures
            .iter()
            .take(3)
            .map(|(seq, tf, actual, expected)| {
                format!("  seq={seq:?}\n    {tf:?}={actual} ledger={expected}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            failures.is_empty(),
            "{} timeframe comparisons failed conservation. First 3:\n{first_three}",
            failures.len()
        );
    }
}

// Nonzero receipts driven through the real fold. Dhan event-time buckets and
// bounded receipt freshness are deliberately separate; TrueData retains the
// previous receipt-based bucket policy.
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

    /// Delivery can cross a minute without changing the trade's candle.
    #[test]
    fn delivery_lag_across_a_minute_boundary_keeps_the_dhan_trade_minute() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        // Traded at 09:29:59, received at 09:30:01.
        let traded = CANDLE_OPEN + 30 * 60 - 1;
        let received = CANDLE_OPEN + 30 * 60 + 1;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(received);

        let _ = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(traded)),
            "a Dhan trade at 09:29:59 belongs to the 09:29 bar"
        );
        assert_ne!(
            TfIndex::M1.bucket_start(received),
            TfIndex::M1.bucket_start(traded),
            "fixture must straddle a minute boundary or it proves nothing"
        );
    }

    #[test]
    fn truedata_retains_receipt_minute_at_the_same_delivery_boundary() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);
        let traded = CANDLE_OPEN + 30 * 60 - 1;
        let received = traded + 2;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(received);
        assert!(
            agg.consume_tick(Feed::Truedata, &t, None, |_, _, _, _, _| {})
                .folded()
        );
        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(received))
        );
    }

    /// A same-day dormant snapshot does not become a current-minute trade.
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
            "the Dhan bucket clock does not re-date a stale snapshot"
        );
    }

    /// A same-day legacy replay cannot use replay wall time as trade time.
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

    /// A late-arriving earlier Dhan event can widen the range, not replace
    /// the close of a later event in the same candle.
    #[test]
    fn the_dhan_close_is_owned_by_the_latest_trade_time() {
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
            (close - 100.0).abs() < 1e-9,
            "the earlier 107.0 event cannot replace the later 100.0 close: {close}"
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
        let mut t = tick(13, SEG_IDX, yesterday, 100.0, 1);
        t.received_at_nanos = 0;
        assert_eq!(t.received_at_nanos, 0, "fixture must exercise the sentinel");

        let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});

        assert!(
            !stats.stale_trading_day,
            "with no second clock the receipt gate must stand down — refusing \
             here would strand every pre-TVW3 boot replay, which is prior-day \
             by construction"
        );
        assert_eq!(agg.watermark_secs(), 0);
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

    /// A last-trade stamp one second before midnight belongs to the prior
    /// day. Receipt never re-dates it; the regular candle window has already
    /// closed, so the refusal affects candle construction only.
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

#[cfg(test)]
mod canonical_volume_contract_tests {
    use super::*;
    use crate::candles::volume_update::{
        VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN, VOLUME_QUALITY_COUNTER_AMBIGUOUS,
        VOLUME_QUALITY_DEFINITION_CHANGED, VOLUME_QUALITY_MISSING_OBSERVATION,
        VOLUME_QUALITY_UNKNOWN_BASELINE,
    };
    use std::collections::BTreeMap;

    const DAY: u32 = 1_779_321_600;
    const OPEN: u32 = DAY + 33_300;

    fn metadata() -> CandleMetadata {
        CandleMetadata {
            lot_size: 100,
            instrument_definition_version: 1,
            underlying_id: 17,
            family_code: 1,
        }
    }

    fn observed(at: u32, price: f32, cumulative: Option<u32>) -> ParsedTick {
        ParsedTick {
            security_id: 77,
            exchange_segment_code: 2,
            exchange_timestamp: at,
            received_at_nanos: (i64::from(at) - 19_800) * 1_000_000_000,
            last_traded_price: price,
            volume: cumulative.unwrap_or(0),
            volume_present: cumulative.is_some(),
            ..ParsedTick::default()
        }
    }

    #[test]
    fn an_untraded_zero_cannot_certify_a_session_or_allocate_the_first_positive_prefix() {
        for exchange_timestamp in [0, OPEN - 86_400, OPEN] {
            let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
            let mut untraded = observed(OPEN, 0.0, Some(0));
            untraded.exchange_timestamp = exchange_timestamp;
            let stats = agg.consume_tick_with_context(
                Feed::Dhan,
                &untraded,
                None,
                metadata(),
                |_, _, _, _, _| panic!("an untraded instrument has no price candle"),
                |_| panic!("an unsupported zero has no canonical ranked candle"),
            );
            assert!(stats.untraded_timestamp || stats.untraded_sentinel);
            assert_eq!(agg.watermark_secs(), 0);
            assert!(
                agg.is_empty(),
                "a sentinel cannot allocate counter authority"
            );
            assert!(agg.last_ltp(Feed::Dhan, 77, 2).is_none());
            assert!(agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M1).is_none());

            for (price, cumulative, measured_delta) in [(100.0, 100, 0), (101.0, 150, 50)] {
                let mut updates = BTreeMap::new();
                agg.consume_tick_with_context(
                    Feed::Dhan,
                    &observed(OPEN + 1, price, Some(cumulative)),
                    None,
                    metadata(),
                    |_, _, _, _, _| {},
                    |update| {
                        updates.insert(update.tf, update);
                    },
                );
                for tf in TfIndex::ALL {
                    let state = agg.snapshot(Feed::Dhan, 77, 2, tf).expect("real candle");
                    let update = updates.get(&tf).expect("canonical candle publication");
                    assert_eq!(
                        state.volume, measured_delta,
                        "the first 100 is only a baseline"
                    );
                    assert_eq!(update.gross_volume, state.volume);
                    assert_ne!(state.volume_quality & VOLUME_QUALITY_UNKNOWN_BASELINE, 0);
                    assert!(!update.quality.is_eligible(), "{tf:?}");
                    assert_eq!(update.last_observed_secs, OPEN + 1);
                }
            }
        }
    }

    #[test]
    fn an_untraded_zero_cannot_fill_a_missing_volume_field_or_refresh_its_clock() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        agg.consume_tick_with_context(
            Feed::Dhan,
            &observed(OPEN, 100.0, None),
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
        let mut untraded = observed(OPEN + 1, 0.0, Some(0));
        untraded.exchange_timestamp = 0;
        agg.consume_tick_with_context(
            Feed::Dhan,
            &untraded,
            None,
            metadata(),
            |_, _, _, _, _| panic!("no new candle or amendment is supported"),
            |_| panic!("no quantity observation clock exists for the sentinel"),
        );
        let index = agg.lookup(Feed::Dhan, 77, 2).expect("existing price slot");
        let slot = &agg.slots[index];
        assert_eq!(slot.volume_counter.cumulative(), None);
        assert!(slot.volume_baseline.is_none());
        assert_eq!(slot.last_observed_secs, OPEN);
        agg.consume_tick_with_context(
            Feed::Dhan,
            &observed(OPEN + 1, 101.0, Some(1_000)),
            None,
            metadata(),
            |_, _, _, _, _| {},
            |update| {
                assert_eq!(update.gross_volume, 0);
                assert_ne!(update.volume_quality & VOLUME_QUALITY_UNKNOWN_BASELINE, 0);
                assert!(!update.quality.is_eligible());
            },
        );
    }

    #[test]
    fn an_untraded_counter_regression_revokes_quantity_without_refreshing_it() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        for (price, counter) in [(100.0, 0), (101.0, 100)] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(OPEN, price, Some(counter)),
                None,
                metadata(),
                |_, _, _, _, _| {},
                |_| {},
            );
        }
        let mut untraded = observed(OPEN + 10, 0.0, Some(0));
        untraded.exchange_timestamp = 0;
        let mut seen = 0;
        agg.consume_tick_with_context(
            Feed::Dhan,
            &untraded,
            None,
            metadata(),
            |_, _, _, _, _| panic!("no new candle is created by the sentinel"),
            |update| {
                seen += 1;
                assert_eq!(update.gross_volume, 100);
                assert_eq!(update.estimated_net_volume, None);
                assert_eq!(update.last_observed_secs, OPEN);
                assert_ne!(update.volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS, 0);
                assert_ne!(
                    update.volume_quality & VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN,
                    0
                );
                assert!(!update.quality.is_eligible());
                assert!(!update.closed);
            },
        );
        assert_eq!(seen, TF_COUNT);
        assert_eq!(agg.watermark_secs(), OPEN);
    }

    #[test]
    fn an_untraded_regression_also_amends_the_retained_closed_candle() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        for (price, counter) in [(100.0, 0), (101.0, 100)] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(OPEN, price, Some(counter)),
                None,
                metadata(),
                |_, _, _, _, _| {},
                |_| {},
            );
        }
        agg.catch_up_seal_all_with_volume_updates(OPEN + 60, |_, _, _, _, _| {}, |_| {});
        assert!(
            agg.snapshot(Feed::Dhan, 77, 2, TfIndex::S5)
                .unwrap()
                .is_uninitialised()
        );
        let mut untraded = observed(OPEN + 61, 0.0, Some(0));
        untraded.exchange_timestamp = 0;
        let mut candles = BTreeMap::new();
        let mut updates = BTreeMap::new();
        let stats = agg.consume_tick_with_context(
            Feed::Dhan,
            &untraded,
            None,
            metadata(),
            |_, _, _, tf, state| {
                candles.insert((tf, state.bucket_start_ist_secs), state);
            },
            |update| {
                updates.insert((update.tf, update.bucket_start_secs), update);
            },
        );
        assert!(stats.amended_count > 0);
        assert_eq!(stats.sealed_count, 0);
        let candle = candles
            .get(&(TfIndex::S5, OPEN))
            .expect("persisted amendment");
        let update = updates
            .get(&(TfIndex::S5, OPEN))
            .expect("winner invalidation");
        assert_eq!(candle.volume, 100);
        assert_eq!(candle.net_volume(), None);
        assert_ne!(candle.volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS, 0);
        assert_eq!(update.volume_quality, candle.volume_quality);
        assert_eq!(update.revision, candle.bucket_revision);
        assert_eq!(update.last_observed_secs, OPEN);
        assert!(!update.quality.is_eligible());
        assert!(
            update.closed,
            "only the earlier catch-up supplied closure authority"
        );
        assert_eq!(agg.watermark_secs(), OPEN);
    }

    #[test]
    fn a_next_day_untraded_zero_does_not_reset_or_seal_the_existing_counter_epoch() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        agg.consume_tick_with_context(
            Feed::Dhan,
            &observed(OPEN, 100.0, Some(1_000)),
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
        let index = agg.lookup(Feed::Dhan, 77, 2).unwrap();
        let revision = agg.slots[index].volume_revision;
        let mut untraded = observed(OPEN + 86_400, 0.0, Some(0));
        untraded.exchange_timestamp = 0;
        let stats = agg.consume_tick_with_context(
            Feed::Dhan,
            &untraded,
            None,
            metadata(),
            |_, _, _, _, _| panic!("receipt cannot close a previous-session candle"),
            |_| panic!("receipt cannot publish a new current-session quantity"),
        );
        assert_eq!(stats.sealed_count, 0);
        assert_eq!(stats.amended_count, 0);
        assert_eq!(agg.slots[index].volume_revision, revision);
        assert_eq!(
            agg.slots[index].volume_counter.session_day(),
            Some(OPEN / 86_400)
        );
        assert_eq!(agg.slots[index].volume_counter.cumulative(), Some(1_000));
        assert_eq!(agg.slots[index].last_observed_secs, OPEN);
        assert_eq!(agg.watermark_secs(), OPEN);
    }

    #[test]
    fn a_valid_price_and_dated_zero_still_establishes_real_quantity_for_all_frames() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let at = DAY + CANDLE_SESSION_OPEN_SECS_OF_DAY_IST;
        for (price, counter) in [(100.0, 0), (101.0, 50)] {
            let mut updates = BTreeMap::new();
            let stats = agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(at, price, Some(counter)),
                None,
                metadata(),
                |_, _, _, _, _| {},
                |update| {
                    updates.insert(update.tf, update);
                },
            );
            assert!(stats.folded());
            for tf in TfIndex::ALL {
                let candle = agg
                    .snapshot(Feed::Dhan, 77, 2, tf)
                    .expect("real price candle");
                let update = updates.get(&tf).expect("same canonical quantity");
                assert_eq!(candle.volume, u64::from(counter), "{tf:?}");
                assert_eq!(
                    candle.net_volume(),
                    None,
                    "first candle has no predecessor: {tf:?}"
                );
                assert!(candle.tick_count > 0);
                assert_eq!(update.gross_volume, candle.volume);
                assert_eq!(update.estimated_net_volume, candle.net_volume());
                assert_eq!(
                    update.volume_quality,
                    crate::candles::volume_update::VOLUME_QUALITY_UNCLASSIFIED_NET,
                    "{tf:?}"
                );
                assert!(update.quality.is_eligible(), "{tf:?}");
                assert_eq!(update.last_observed_secs, at);
            }
        }
    }

    #[test]
    fn every_frame_publishes_the_same_gross_signed_metadata_and_revision_as_its_candle() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let mut candles = BTreeMap::new();
        let mut updates = BTreeMap::new();
        for (at, price, cumulative) in [
            (OPEN, 100.0, 1_000),
            (OPEN, 101.0, 1_100),
            (OPEN, 100.0, 1_150),
        ] {
            let stats = agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(at, price, Some(cumulative)),
                None,
                metadata(),
                |_, _, _, tf, state| {
                    candles.insert((tf, state.bucket_start_ist_secs), state);
                },
                |update| {
                    updates.insert((update.tf, update.bucket_start_secs), update);
                },
            );
            assert!(stats.folded());
        }
        assert_eq!(updates.len(), TF_COUNT);
        for tf in TfIndex::ALL {
            let state = agg
                .snapshot(Feed::Dhan, 77, 2, tf)
                .expect("same instrument");
            let update = updates
                .get(&(tf, state.bucket_start_ist_secs))
                .expect("all24");
            assert_eq!(state.volume, 150);
            assert_eq!(state.net_volume(), None, "first candle has no predecessor");
            assert_eq!(update.gross_volume, state.volume);
            assert_eq!(update.estimated_net_volume, state.net_volume());
            assert_eq!(update.metadata, state.metadata);
            assert_eq!(update.volume_quality, state.volume_quality);
            assert_eq!(update.revision, state.bucket_revision);
            assert_eq!(
                update.bucket_end_secs,
                tf.bucket_end(state.bucket_start_ist_secs)
            );
            assert!(!update.closed);
        }
        agg.force_seal_all_with_volume_updates(
            |_, _, _, tf, state| {
                candles.insert((tf, state.bucket_start_ist_secs), state);
            },
            |update| {
                updates.insert((update.tf, update.bucket_start_secs), update);
            },
        );
        assert_eq!(candles.len(), TF_COUNT);
        for (key, state) in candles {
            let update = updates
                .get(&key)
                .expect("every actual seal emits quantities");
            assert!(
                !update.closed,
                "an administrative flush has no expiry cutoff"
            );
            assert_eq!(
                (update.gross_volume, update.estimated_net_volume),
                (state.volume, state.net_volume())
            );
            assert_eq!(
                (update.metadata, update.volume_quality, update.revision),
                (state.metadata, state.volume_quality, state.bucket_revision)
            );
        }
    }

    #[test]
    fn a_positive_first_counter_at_a_subsecond_boundary_never_certifies_the_opening_prefix() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let mut latest = BTreeMap::new();
        for (price, counter, fractional_nanos) in
            [(100.0, 1_000, 900_000_000), (101.0, 1_100, 950_000_000)]
        {
            let mut tick = observed(OPEN, price, Some(counter));
            tick.received_at_nanos += fractional_nanos;
            let stats = agg.consume_tick_with_context(
                Feed::Dhan,
                &tick,
                None,
                metadata(),
                |_, _, _, _, _| {},
                |update| {
                    latest.insert(update.tf, update);
                },
            );
            assert!(stats.folded());
        }
        for tf in TfIndex::ALL {
            let update = latest.get(&tf).expect("all frames publish canonical state");
            assert_eq!(
                update.gross_volume, 100,
                "the first 1,000 is only a baseline"
            );
            assert_eq!(
                update.estimated_net_volume, None,
                "first candle has no predecessor"
            );
            assert_ne!(
                update.volume_quality & VOLUME_QUALITY_UNKNOWN_BASELINE,
                0,
                "09:15:00.900 is not a pre-bucket observation: {tf:?}"
            );
            assert!(!update.quality.baseline_known, "{tf:?}");
            assert!(!update.quality.is_eligible(), "{tf:?}");
        }
    }

    #[test]
    fn a_genuine_zero_in_the_opening_second_preserves_known_zero_and_signed_quantity() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let aligned_frames = [TfIndex::S1, TfIndex::S5, TfIndex::M1, TfIndex::M15];
        for (price, counter, fractional_nanos) in
            [(100.0, 0, 900_000_000), (101.0, 100, 950_000_000)]
        {
            let mut tick = observed(OPEN, price, Some(counter));
            tick.received_at_nanos += fractional_nanos;
            agg.consume_tick_with_context(
                Feed::Dhan,
                &tick,
                None,
                metadata(),
                |_, _, _, _, _| {},
                |_| {},
            );
            for tf in aligned_frames {
                let state = agg.snapshot(Feed::Dhan, 77, 2, tf).expect("aligned candle");
                assert_eq!(state.bucket_start_ist_secs, OPEN, "{tf:?}");
                assert_eq!(state.volume, u64::from(counter), "{tf:?}");
                assert_eq!(
                    state.net_volume(),
                    None,
                    "first candle has no predecessor: {tf:?}"
                );
                assert_eq!(
                    state.volume_quality,
                    crate::candles::volume_update::VOLUME_QUALITY_UNCLASSIFIED_NET,
                    "known quantity but unknown direction: {tf:?}"
                );
            }
        }
    }

    #[test]
    fn a_partial_positive_baseline_only_certifies_buckets_starting_after_its_observation_second() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let mut tick = observed(OPEN, 100.0, Some(1_000));
        tick.received_at_nanos += 900_000_000;
        agg.consume_tick_with_context(
            Feed::Dhan,
            &tick,
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
        for (at, price, counter, newly_complete) in [
            (OPEN + 1, 101.0, 1_100, TfIndex::S1),
            (OPEN + 5, 102.0, 1_200, TfIndex::S5),
            (OPEN + 60, 103.0, 1_300, TfIndex::M1),
        ] {
            let mut tick = observed(at, price, Some(counter));
            // Each 100-unit increment is one located trade. A cumulative
            // snapshot without LTQ cannot certify a crossed bucket boundary.
            tick.last_trade_quantity = 100;
            agg.consume_tick_with_context(
                Feed::Dhan,
                &tick,
                None,
                metadata(),
                |_, _, _, _, _| {},
                |_| {},
            );
            let complete = agg
                .snapshot(Feed::Dhan, 77, 2, newly_complete)
                .expect("later bucket");
            assert_eq!(complete.bucket_start_ist_secs, at);
            assert_eq!(complete.volume, 100);
            assert_eq!(complete.net_volume(), Some(100));
            assert_eq!(complete.volume_quality, 0, "{newly_complete:?}");
            let longer = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M15).expect("M15");
            assert_eq!(longer.bucket_start_ist_secs, OPEN);
            assert_ne!(longer.volume_quality & VOLUME_QUALITY_UNKNOWN_BASELINE, 0);
        }
    }

    #[test]
    fn absent_then_large_counter_seeds_without_counting_the_day_or_certifying_the_bucket() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let mut latest = BTreeMap::new();
        for (price, counter) in [
            (100.0, None),
            (101.0, Some(3_000_000)),
            (102.0, Some(3_000_010)),
        ] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(OPEN + 1, price, counter),
                None,
                metadata(),
                |_, _, _, _, _| {},
                |update| {
                    latest.insert(update.tf, update);
                },
            );
        }
        for tf in TfIndex::ALL {
            let update = latest.get(&tf).expect("all frames");
            assert_eq!(update.gross_volume, 10, "{tf:?}");
            assert_ne!(
                update.volume_quality & VOLUME_QUALITY_MISSING_OBSERVATION,
                0,
                "{tf:?}"
            );
            assert!(!update.quality.is_eligible());
        }
    }

    #[test]
    fn genuine_zero_is_a_baseline_and_missing_after_a_high_counter_never_resets() {
        for (sequence, expected) in [
            (vec![Some(0), Some(100), Some(300)], 300),
            (
                vec![
                    Some(3_000_000_000),
                    Some(3_000_000_100),
                    None,
                    Some(3_000_000_200),
                ],
                200,
            ),
        ] {
            let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
            for (index, counter) in sequence.into_iter().enumerate() {
                agg.consume_tick_with_context(
                    Feed::Dhan,
                    &observed(OPEN, 100.0 + index as f32, counter),
                    None,
                    metadata(),
                    |_, _, _, _, _| {},
                    |_| {},
                );
            }
            for tf in TfIndex::ALL {
                let state = agg.snapshot(Feed::Dhan, 77, 2, tf).expect("all frames");
                assert_eq!(state.volume, expected, "{tf:?}");
                assert_eq!(state.volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS, 0);
            }
        }
    }

    #[test]
    fn all_frames_keep_decreases_uncertain_across_arbitrarily_large_recoveries() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let mut latest = BTreeMap::new();
        for counter in [
            3_000_000_000,
            3_000_000_100,
            100,
            200,
            2_000_000_000,
            3_000_000_200,
        ] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(OPEN, 101.0, Some(counter)),
                None,
                metadata(),
                |_, _, _, _, _| {},
                |update| {
                    latest.insert(update.tf, update);
                },
            );
        }
        for tf in TfIndex::ALL {
            let update = latest.get(&tf).expect("all frames");
            assert_eq!(update.gross_volume, 200, "no staircase recount: {tf:?}");
            assert_eq!(update.estimated_net_volume, None);
            assert!(update.quality.counter_ambiguous);
            assert_ne!(update.volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS, 0);
        }
    }

    #[test]
    fn a_changed_lot_cannot_reprice_an_open_bucket_and_the_next_bucket_pins_the_new_definition() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let old = metadata();
        let changed = CandleMetadata {
            lot_size: 200,
            instrument_definition_version: 2,
            ..old
        };
        for (at, value, meta) in [
            (OPEN, 0, old),
            (OPEN, 100, changed),
            (OPEN + 60, 200, changed),
        ] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(at, 100.0 + value as f32, Some(value)),
                None,
                meta,
                |_, _, _, _, _| {},
                |_| {},
            );
            let state = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M1).expect("M1");
            if at == OPEN && value == 100 {
                assert_eq!(state.metadata, old);
                assert_ne!(state.volume_quality & VOLUME_QUALITY_DEFINITION_CHANGED, 0);
            }
        }
        let new_bucket = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M1).expect("M1");
        assert_eq!(new_bucket.metadata, changed);
        assert_eq!(
            new_bucket.volume_quality & VOLUME_QUALITY_DEFINITION_CHANGED,
            0
        );
    }

    #[test]
    fn a_partial_baseline_remains_partial_and_a_later_bucket_can_be_eligible() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        for (at, price, value) in [(OPEN + 20, 100.0, 1_000), (OPEN + 21, 101.0, 1_100)] {
            let mut tick = observed(at, price, Some(value));
            tick.last_trade_quantity = 100;
            agg.consume_tick_with_context(
                Feed::Dhan,
                &tick,
                None,
                metadata(),
                |_, _, _, _, _| {},
                |_| {},
            );
        }
        let partial = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M1).expect("M1");
        assert_ne!(partial.volume_quality & VOLUME_QUALITY_UNKNOWN_BASELINE, 0);
        let mut tick = observed(OPEN + 60, 102.0, Some(1_200));
        // The final 100 units belong to this last trade in the new minute.
        tick.last_trade_quantity = 100;
        agg.consume_tick_with_context(
            Feed::Dhan,
            &tick,
            None,
            metadata(),
            |_, _, _, _, _| {},
            |_| {},
        );
        let next = agg
            .snapshot(Feed::Dhan, 77, 2, TfIndex::M1)
            .expect("next M1");
        assert_eq!(next.volume_quality, 0);
        assert_eq!(next.net_volume(), Some(100));
    }

    #[test]
    fn timer_seals_do_not_invent_freshness_or_change_bucket_identity() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        for (price, value) in [(100.0, 0), (101.0, 100)] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(OPEN, price, Some(value)),
                None,
                metadata(),
                |_, _, _, _, _| {},
                |_| {},
            );
        }
        let prior = agg.snapshot(Feed::Dhan, 77, 2, TfIndex::M1).expect("M1");
        let mut sealed = None;
        agg.catch_up_seal_all_with_volume_updates(
            OPEN + 120,
            |_, _, _, _, _| {},
            |update| {
                if update.tf == TfIndex::M1 {
                    sealed = Some(update);
                }
            },
        );
        let update = sealed.expect("timer closes the actual M1");
        assert_eq!(update.bucket_start_secs, OPEN);
        assert_eq!(update.bucket_end_secs, OPEN + 60);
        assert_eq!(update.last_observed_secs, OPEN);
        assert!(update.revision > prior.bucket_revision);
        assert!(update.closed);
    }

    #[test]
    fn a_same_day_future_stamp_cannot_move_the_global_watermark() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 2);
        let good = observed(OPEN, 100.0, Some(0));
        agg.consume_tick(Feed::Dhan, &good, None, |_, _, _, _, _| {});
        let before = agg.watermark_secs();
        let mut poison = good;
        poison.security_id = 88;
        poison.exchange_timestamp = OPEN + 3_600;
        let rejected = agg.consume_tick_with_context(
            Feed::Dhan,
            &poison,
            None,
            metadata(),
            |_, _, _, _, _| panic!("must not seal"),
            |_| panic!("must not publish"),
        );
        assert!(rejected.future_trading_day);
        assert!(!rejected.folded());
        assert_eq!(agg.watermark_secs(), before);
        assert_eq!(agg.lookup(Feed::Dhan, 88, 2), None);
    }

    #[test]
    fn signed_overflow_stays_unavailable_even_after_later_cancellation() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let mut latest = BTreeMap::new();
        for (price, counter) in [
            (100.0, 0_u64),
            (101.0, i64::MAX as u64),
            (102.0, (i64::MAX as u64) + 1),
            (100.0, u64::MAX),
        ] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(OPEN, price, Some(0)),
                Some(counter),
                metadata(),
                |_, _, _, _, _| {},
                |update| {
                    latest.insert(update.tf, update);
                },
            );
        }
        for update in latest.values() {
            assert_eq!(update.gross_volume, u64::MAX);
            assert_eq!(update.estimated_net_volume, None);
            assert_ne!(
                update.volume_quality
                    & crate::candles::volume_update::VOLUME_QUALITY_UNCLASSIFIED_NET,
                0
            );
        }
    }

    #[test]
    fn gross_beyond_sql_long_is_refused_by_the_shared_candle_state_before_ranking() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 1);
        let at = DAY + 32_400;
        let mut latest = BTreeMap::new();
        // Each signed delta and the running net fit i64. Only the accumulated
        // gross crosses the persisted domain, which previously left RAM green.
        for (price, counter) in [
            (100.0, 0_u64),
            (101.0, 1_u64 << 62),
            (100.0, i64::MAX as u64),
        ] {
            agg.consume_tick_with_context(
                Feed::Dhan,
                &observed(at, price, Some(0)),
                Some(counter),
                metadata(),
                |_, _, _, _, _| {},
                |update| {
                    latest.insert(update.tf, update);
                },
            );
        }
        for update in latest.values() {
            assert_eq!(update.gross_volume, i64::MAX as u64);
            assert_eq!(
                update.estimated_net_volume, None,
                "first candle has no predecessor"
            );
            assert_eq!(
                update.volume_quality,
                crate::candles::volume_update::VOLUME_QUALITY_UNCLASSIFIED_NET
            );
            assert!(update.quality.is_eligible());
        }
        let unrepresentable = (i64::MAX as u64) + 1;
        agg.consume_tick_with_context(
            Feed::Dhan,
            &observed(at, 101.0, Some(0)),
            Some(unrepresentable),
            metadata(),
            |_, _, _, _, _| {},
            |update| {
                latest.insert(update.tf, update);
            },
        );
        for tf in TfIndex::ALL {
            let state = agg
                .snapshot(Feed::Dhan, 77, 2, tf)
                .expect("canonical state");
            let update = latest.get(&tf).expect("canonical update");
            assert_eq!(state.volume, unrepresentable, "raw gross is never clamped");
            assert_eq!(
                state.net_volume_signed, 0,
                "unrepresentable magnitude is unavailable"
            );
            assert_eq!(state.net_volume(), None);
            assert_eq!(update.gross_volume, state.volume);
            assert_eq!(update.estimated_net_volume, state.net_volume());
            assert_eq!(update.volume_quality, state.volume_quality);
            assert_ne!(state.volume_quality & VOLUME_QUALITY_COUNTER_AMBIGUOUS, 0);
            assert!(!update.quality.is_eligible());
        }
        let mut seals = BTreeMap::new();
        agg.force_seal_all_with_volume_updates(
            |_, _, _, tf, state| {
                seals.insert(tf, state);
            },
            |update| {
                latest.insert(update.tf, update);
            },
        );
        assert_eq!(seals.len(), TF_COUNT);
        for (tf, state) in seals {
            let update = latest.get(&tf).expect("same sealed revision");
            assert!(
                !update.closed,
                "an administrative flush has no expiry cutoff"
            );
            assert_eq!(state.volume, unrepresentable);
            assert_eq!(update.gross_volume, state.volume);
            assert_eq!(update.estimated_net_volume, state.net_volume());
            assert_eq!(update.volume_quality, state.volume_quality);
            assert_eq!(update.revision, state.bucket_revision);
            assert!(!update.quality.is_eligible());
        }
    }

    #[test]
    fn the_canonical_callback_bound_holds_through_close_late_data_and_session_change() {
        use crate::candles::volume_update::MAX_VOLUME_UPDATES_PER_TICK;
        for strategy in [FeedStrategy::REFOLD, FeedStrategy::DISCARD] {
            let mut agg = MultiTfAggregator::with_capacity(strategy, 1);
            for (index, (at, counter)) in [
                (OPEN, None),
                (OPEN, Some(1_000)),
                (OPEN + 1, Some(1_100)),
                (OPEN + 120, Some(1_200)),
                (OPEN + 1, Some(1_300)),
                (OPEN + 121, Some(100)),
                (OPEN + 86_400, Some(0)),
                (OPEN + 86_401, Some(100)),
            ]
            .into_iter()
            .enumerate()
            {
                if index == 4 {
                    agg.catch_up_seal_all(OPEN + 180, |_, _, _, _, _| {});
                }
                let mut tick = observed(at, 100.0 + index as f32, counter);
                if index == 4 {
                    tick.received_at_nanos = 0;
                }
                let mut count = 0_usize;
                agg.consume_tick_with_context(
                    Feed::Dhan,
                    &tick,
                    None,
                    metadata(),
                    |_, _, _, _, _| {},
                    |_| {
                        count += 1;
                    },
                );
                assert!(
                    count <= MAX_VOLUME_UPDATES_PER_TICK,
                    "{strategy:?}: {index} emitted {count}"
                );
            }
        }
    }
}
