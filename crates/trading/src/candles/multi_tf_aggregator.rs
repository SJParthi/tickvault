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
            && !self.untraded_timestamp
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
        // Exact: len() < capacity <= AGGREGATOR_MAX_SLOTS (25_000) << u32::MAX.
        let idx = self.slots.len();
        self.slots.push(InstrumentSlot {
            key,
            cell: AggregatorCell::empty(),
            last_cumulative: 0,
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

        for tf in TfIndex::ALL {
            match slot.cell.consume_tick_with_extremes(
                tf,
                tick,
                prices,
                baseline,
                strategy,
                cumulative_volume,
                extremes,
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
            crate::candles::fold_counters::fold_counters()
                .cumulative_regression
                .increment(1);
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
    /// work that happens beside it. UNMEASURED at the 25,000-instrument
    /// target. (The literal `21` this line used to carry was stale; cite the
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
        let expected = 6_000_u64 - 1_000;
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
            untraded_timestamp: _,
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
                "untraded_timestamp",
                ConsumeStats {
                    untraded_timestamp: true,
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
