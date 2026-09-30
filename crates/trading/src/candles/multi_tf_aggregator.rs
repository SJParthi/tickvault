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
use crate::candles::{BufferOutcome, BufferedSeal, LiveCandleState, SealRing, TF_COUNT, TfIndex};

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
    /// Fold-clock second (the exchange last-trade time) of the last ACCEPTED,
    /// non-stale packet for this instrument. `0` before the first one.
    ///
    /// Half of the repeat-quote test in `consume_tick`: a packet whose trade
    /// time, price AND day-cumulative all equal the previous accepted packet's
    /// describes the SAME trade, re-sent by Dhan because the book or open
    /// interest changed. 4 bytes per instrument, ~100 KB at the 25,000-slot
    /// ceiling, and it lives on the slot that already exists per instrument
    /// for the same reason `last_ltp` does.
    last_trade_ts: u32,
    /// Replay-gap bookkeeping (plan ITEM 47, 2026-09-29). Bit
    /// `tf.as_ordinal()` set = the OPEN bucket of that timeframe may be
    /// missing ticks that a gapped WAL replay skipped, so it is PARTIAL.
    ///
    /// A boot replay folds only the frames the database had not applied.
    /// Before this existed, the fold carried an instrument's volume baseline
    /// across the skipped span and its first tick after the gap took the whole
    /// span's volume (2026-09-28: 733,406 shares in one second for one stock,
    /// ~696 stocks at once). The rebuilt partial bar then overwrote the
    /// complete live row, because the candle DEDUP key is the bucket.
    replay_open_partial: u16,
    /// The same bit for the last SEALED bucket — the one a late amendment
    /// re-emits.
    replay_sealed_partial: u16,
    /// Set when this slot catches up with a replay gap
    /// ([`InstrumentSlot::sync_replay_gap`]), consumed by the next
    /// accepted tick: the cell re-bases its open buckets and breaks every
    /// frame's chain on that tick's cumulative, exactly as a counter restart
    /// does. Without it each frame chains its next bucket to the previous
    /// bar's end, from BEFORE the gap, and the skipped span lands in one bar.
    replay_gap_rebase_pending: bool,
    /// The aggregator's [`MultiTfAggregator::replay_gap_epoch`] this slot last
    /// caught up with. When they differ, one or more gaps were marked since
    /// the slot was last touched, and [`InstrumentSlot::sync_replay_gap`]
    /// applies them before anything reads the slot's baseline or partial bits.
    replay_gap_epoch_seen: u64,
    /// One bit per timeframe: the OPEN bucket was still open, and partial,
    /// when the replay handed over to the live feed
    /// ([`MultiTfAggregator::finish_replay`]). It is suppressed when it seals,
    /// in live mode too: a replay that stopped mid-bucket left it without its
    /// head, and the live process that captured those frames may already have
    /// stored the complete bar under the same key (review, 2026-09-29).
    replay_taint_open: u16,
    /// The same for the last SEALED bucket, so a late tick cannot re-emit a
    /// partial replayed bar through an amendment.
    replay_taint_sealed: u16,
    /// One bit per timeframe: the bucket still open at a CLEAN hand-over,
    /// complete, which [`MultiTfAggregator::finish_replay`] kept (review
    /// round 4: after a crash it exists nowhere else). Live post-gap settling
    /// does not mark it partial, or the capture-start rule would drop it
    /// depending on the next ticks' delivery lag (review round 12). Cleared
    /// when that bucket rolls.
    replay_handover_kept: u16,
    /// `true` from the tick that resolves a replay gap (or a new slot's first
    /// tick) until a later tick has both RAISED the cumulative and left the
    /// tick rule with a direction (plan ITEM 47, review round 5). Until then a
    /// tick that adds volume marks the buckets it lands in partial: the
    /// skipped span's highest cumulative is unknown, so a packet stale against
    /// it would pass as fresh and inflate the bucket; and without a direction
    /// the bucket would publish a null net over the stored signed one.
    replay_settling: bool,
    /// One bit per timeframe: during a replay, the last SEALED bar is held
    /// rather than emitted, because a late tick can still amend it until the
    /// next bar of that timeframe seals — and a late tick in a SKIPPED span
    /// would have amended the stored bar, which an early emission would then
    /// overwrite with the unamended version (review round 6, found by the
    /// mixed-stream property). Released by `release_held`.
    replay_held: u16,
    /// IST fold second up to which a bucket may still be missing SKIPPED
    /// ticks (0 = none). Frames are skipped in CAPTURE order, and a late tick
    /// can carry an older trade time than a skipped one, so a replay can open
    /// the bucket of a skipped tick later, from a later frame, and take it for
    /// complete (review round 6, found by the mixed-stream property). Every
    /// skipped frame was received before this slot's first tick after the
    /// gap, and a trade is not received before it happens, so that receipt
    /// time (plus a small clock-skew margin) bounds them. While set, every
    /// open bucket starting at or before it is marked partial.
    replay_gap_frontier: u32,
    /// One bit per timeframe: the NEXT bucket this slot opens is partial
    /// (review round 7). Set when a replayed tick lands in an open bucket the
    /// live process may already have closed by its periodic catch-up seal:
    /// live then carried that tick's volume into the next bucket, the replay
    /// folds it into the open one, so neither bar can be rebuilt as live
    /// stored it. Consumed when the bucket rolls; cleared at the hand-over and
    /// at the day boundary.
    replay_next_partial: u16,
}

/// Every timeframe's bit in [`InstrumentSlot::replay_open_partial`].
const REPLAY_ALL_TF_MASK: u16 = {
    assert!(TF_COUNT <= 16, "one bit per timeframe must fit a u16");
    // Exact: TF_COUNT <= 16, so the shift fits a u32 and the mask a u16.
    ((1_u32 << TF_COUNT) - 1) as u16
};

/// The bit of `tf` in the replay-partial masks.
const fn replay_tf_bit(tf: TfIndex) -> u16 {
    1_u16 << tf.as_ordinal()
}

/// What the fold holds for one named window of one instrument — returned by
/// [`MultiTfAggregator::window_bar`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowBar {
    /// The bar whose bucket IS the named window: the open bucket, else the
    /// last-sealed one, else `None`.
    pub bar: Option<LiveCandleState>,
    /// Bucket-open IST second of the cell's currently open bucket (`0` when
    /// the frame never opened).
    pub open_bucket_ist_secs: u32,
    /// Bucket-open IST second of the cell's last-sealed bucket (`0` when
    /// nothing has sealed today).
    pub last_sealed_bucket_ist_secs: u32,
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
    /// `true` when the tick fell outside the `[09:00, 15:40)` IST candle
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
    /// QUALIFIER, not a refusal of its own (2026-09-23): `true` when the
    /// `stale_trading_day` or `future_trading_day` flag above was raised by
    /// the RECEIPT-clock comparison — the exchange day is not the day OUR
    /// machine received the tick on. `false` when `stale_trading_day` came
    /// from the WATERMARK comparison instead (an older day than a tick we
    /// already folded).
    ///
    /// The two are different facts and a WAL replay must not confuse them.
    /// A receipt-day mismatch is refused by the operator's 2026-09-10
    /// directive (`websocket-connection-scope-lock.md`, "A TICK WHOSE
    /// EXCHANGE DAY IS NOT THE RECEIPT DAY IS REFUSED OUTRIGHT") — replaying it
    /// reaches exactly the verdict the live feed reached, so it is not loss. A
    /// watermark refusal of a replayed frame can be a genuinely captured tick
    /// from an earlier session.
    ///
    /// ⚠ CORRECTED 2026-09-23 (same day): this said such a tick "IS loss and
    /// must still page". The app's replay path now WRITES that row back to
    /// `ticks` and skips only the candle (the THIRD 2026-09-23 section of
    /// `websocket-connection-scope-lock.md`), so it is neither lost nor paged.
    /// The distinction this flag carries is unchanged — it is exactly what the
    /// write-back keys on.
    ///
    /// Deliberately excluded from [`Self::folded`]: it never occurs without
    /// one of the two refusal flags, which already make `folded()` false.
    pub receipt_day_mismatch: bool,
    /// `true` when this packet repeated the previous accepted TRADE exactly —
    /// same last-trade time, same last-traded price, same day-cumulative
    /// volume — and was therefore applied as a QUOTE REFRESH rather than a
    /// trade: open interest and total buy/sell quantity were updated on the
    /// open bucket, and nothing else moved (see `refresh_repeat_quote`).
    ///
    /// Informational, and deliberately NOT part of [`Self::folded`]: the
    /// packet was accepted and its row is written; it simply carried no new
    /// trade for a candle to count.
    pub repeat_quote: bool,
    /// Timeframes whose seal or late amendment was NOT emitted because a WAL
    /// replay could only partly see that bar (plan ITEM 47). The complete bar
    /// the live process stored survives. `0..=TF_COUNT`.
    pub replay_partial_suppressed: u8,
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
    /// `true` while a WAL replay is folding (plan ITEM 47). A bar the replay
    /// could only partly see is then suppressed rather than emitted, so it
    /// can never overwrite the complete bar the live process stored.
    replay_mode: bool,
    /// Bumped by [`Self::mark_replay_gap`]. Each slot applies the gap lazily
    /// the next time it is touched, so marking a gap is O(1) however many
    /// slots exist and however many gaps a replay has.
    replay_gap_epoch: u64,
    /// Bars suppressed as partial since this aggregator was built, beside the
    /// process-wide `tv_candle_refold_partial_suppressed_total`, so the
    /// hand-over can report the replay's own count in its log line.
    replay_suppressed_total: u64,
    /// IST fold second at which THIS process began capturing live frames (0 =
    /// not set). A PARTIAL bar whose bucket ended at or before it can only be
    /// a fragment of a bar the previous process owned: the first packet after
    /// a subscribe carries the instrument's last trade time, which for a quiet
    /// contract can be an hour old, and it would otherwise open a one-tick,
    /// zero-volume bar for that old bucket and overwrite the stored row
    /// (review, 2026-09-29). Such a bar is suppressed and counted in every
    /// mode.
    live_capture_from_secs: u32,
    /// How far behind the watermark the live catch-up seal closes a quiet
    /// bucket (the app's `CATCHUP_LATENESS_MARGIN_SECS`). A replay uses it
    /// to recognise a tick that live may have seen only AFTER that seal
    /// (review round 7). Defaults to [`DEFAULT_CATCH_UP_MARGIN_SECS`].
    catch_up_margin_secs: u32,
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
            replay_mode: false,
            replay_gap_epoch: 0,
            replay_suppressed_total: 0,
            live_capture_from_secs: 0,
            catch_up_margin_secs: DEFAULT_CATCH_UP_MARGIN_SECS,
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

    /// Snapshot of the bucket that covers ONE NAMED WINDOW, or `None` when
    /// this instrument has no bar for that window.
    ///
    /// [`Self::snapshot`] answers "what is open right now", which is a
    /// different question and the wrong one for a caller that already knows
    /// which window it is describing. A busy contract has usually already
    /// rolled by the time a sweep reads it — so a probe that takes the open
    /// bucket systematically returns the NEXT window for exactly the
    /// instruments a volume leaderboard exists to rank, and returns the
    /// right one only for the quiet tail.
    ///
    /// This resolves the caller's own window instead: the open bucket when it
    /// IS that window, otherwise the last sealed bucket when that is, and
    /// otherwise nothing. It never returns a bar for a different window, so a
    /// consumer cannot silently compare two windows as though they were one.
    ///
    /// `bucket_open_ist_secs` is the window's OPEN, in IST epoch seconds —
    /// the same base and the same grid anchor `TfIndex::bucket_start` uses,
    /// so the comparison is an integer equality and not an approximation.
    ///
    /// Returning `None` is a real answer and the common one at boot, after a
    /// day-boundary `force_seal` (which clears `last_sealed`), and for any
    /// instrument that did not trade in the window.
    ///
    /// # Complexity
    /// O(1) average — one hash lookup and at most two array indexes, no
    /// allocation.
    #[must_use]
    pub fn bar_for_window(
        &self,
        feed: Feed,
        security_id: u64,
        segment_code: u8,
        tf: TfIndex,
        bucket_open_ist_secs: u32,
    ) -> Option<LiveCandleState> {
        let idx = *self.index.get(&(feed, security_id, segment_code))? as usize;
        let cell = &self.slots.get(idx)?.cell;
        let open = cell.snapshot(tf);
        if open.bucket_start_ist_secs == bucket_open_ist_secs {
            return Some(open);
        }
        cell.last_sealed_snapshot(tf)
            .filter(|s| s.bucket_start_ist_secs == bucket_open_ist_secs)
    }

    /// The bar for ONE named window, together with where the cell's open and
    /// last-sealed buckets sit, or `None` when the fold holds no slot for
    /// this instrument.
    ///
    /// [`Self::bar_for_window`] answers only "is there a bar for W". A ranker
    /// that reads the window's volume also needs to know WHY there is none:
    /// the instrument did not trade in W (both buckets are elsewhere and the
    /// sealed one is older than W), or its W bar was overwritten because it
    /// already sealed a LATER bucket — and whether it is trading after W at
    /// all, so the next window's reader still visits it. Both answers come
    /// from the two bucket starts this returns beside the bar.
    ///
    /// # Complexity
    /// O(1) average — one hash lookup and two array indexes, no allocation.
    #[must_use]
    pub fn window_bar(
        &self,
        feed: Feed,
        security_id: u64,
        segment_code: u8,
        tf: TfIndex,
        bucket_open_ist_secs: u32,
    ) -> Option<WindowBar> {
        let idx = *self.index.get(&(feed, security_id, segment_code))? as usize;
        let cell = &self.slots.get(idx)?.cell;
        let open = cell.snapshot(tf);
        let sealed = cell.last_sealed_snapshot(tf);
        let last_sealed_bucket_ist_secs = sealed.map_or(0, |s| s.bucket_start_ist_secs);
        let bar = if open.bucket_start_ist_secs == bucket_open_ist_secs {
            Some(open)
        } else {
            sealed.filter(|s| s.bucket_start_ist_secs == bucket_open_ist_secs)
        };
        Some(WindowBar {
            bar,
            open_bucket_ist_secs: open.bucket_start_ist_secs,
            last_sealed_bucket_ist_secs,
        })
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
            last_trade_ts: 0,
            // Deliberately NOT a baseline — see the field doc. The first tick
            // this slot folds replaces it with a real observation.
            volume_baseline_seeded: false,
            // A new slot's first buckets are partial in every mode: the tick
            // that opens them seeds the baseline and adds none of its own
            // volume, and during a replay the frames before it may have been
            // skipped. Partial alone suppresses nothing live; it matters only
            // with replay mode, a taint, or a bucket that ended before this
            // process began capturing.
            replay_open_partial: REPLAY_ALL_TF_MASK,
            replay_sealed_partial: 0,
            replay_gap_rebase_pending: false,
            // Born after every gap marked so far: nothing to catch up.
            replay_gap_epoch_seen: self.replay_gap_epoch,
            replay_taint_open: 0,
            replay_taint_sealed: 0,
            replay_handover_kept: 0,
            // A new slot knows neither the cumulative nor the direction that
            // came before its first tick (during a replay, those frames may
            // have been skipped), so its first buckets settle like a gap's.
            replay_settling: true,
            replay_held: 0,
            replay_gap_frontier: 0,
            replay_next_partial: 0,
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
        let fold_secs = fold_clock_ist_secs(tick.exchange_timestamp);

        // FUTURE TRADING DAY gate — BEFORE the advance, and that ordering is
        // the entire point.
        //
        // The advance below is `>`, so a tick from the PAST can never move the
        // watermark; the stale-day gate under it is safe for that reason. A
        // tick from the FUTURE had no such guard, and the asymmetry is not
        // theoretical: `fold_clock_ist_secs` returns the VENDOR's stamp.
        // (Until 2026-09-18 it returned it only when receipt and exchange
        // disagreed by more than a trusted band, and a stamp one day ahead
        // disagrees by ~86,400 s — far outside it. Since the ts-bucketing
        // directive there is no band and no exception: the vendor's stamp is
        // ALWAYS what buckets, which makes this gate strictly MORE
        // load-bearing, never less.) So one clock-fault packet stamped for
        // tomorrow was
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
                    receipt_day_mismatch: true,
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
                    receipt_day_mismatch: true,
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

        // Candle-window gate. The bucket grid is anchored at 09:00 (the
        // CANDLE session open, `CANDLE_SESSION_OPEN_SECS_OF_DAY_IST` — so M60
        // runs 09:00/10:00/…, not 09:15/10:15/…), and `TfIndex::bucket_start`
        // clamps anything earlier into the first bucket; this gate is what
        // keeps a tick before 09:00 from corrupting that first bucket.
        // (Until 2026-09-22 this line said "09:15-ANCHORED … would CORRUPT
        // the 09:15 candle", which described the pre-2026-08-28 grid — see
        // the note below.)
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
        // Read before the slot borrow below (plan ITEM 47).
        let replay_mode = self.replay_mode;
        let gap_epoch = self.replay_gap_epoch;
        let live_from = self.live_capture_from_secs;
        // LATE-TICK BOUND (plan ITEM 47, review round 7): during a replay, the
        // latest instant the live process could have reached when it saw this
        // tick — its watermark then, which never exceeds this tick's receipt,
        // and was at least the replay's own watermark. An open bucket that
        // ended `catch_up_margin_secs` before it may already have been closed
        // live by the periodic catch-up seal. 0 outside a replay.
        let late_bound = if replay_mode {
            // Plus the same clock-skew allowance as the gap frontier: live's
            // watermark comes from EXCHANGE time, which can run a little ahead
            // of this box's receipt clock (review round 8).
            receipt_ist_secs(tick.received_at_nanos)
                .map_or(0, |secs| secs.saturating_add(REPLAY_FRONTIER_SKEW_SECS))
                .max(self.watermark_secs)
        } else {
            0
        };
        let catch_up_margin = self.catch_up_margin_secs;
        let Some(slot) = self.slots.get_mut(idx) else {
            // Unreachable: slot_index either returned an existing index or
            // just pushed one. Fail closed rather than index-panic.
            return ConsumeStats {
                slot_exhausted: true,
                ..ConsumeStats::default()
            };
        };
        // Apply any replay gap marked since this slot was last touched, before
        // the baseline or the partial bits are read below. One compare per
        // tick when there is none.
        slot.sync_replay_gap(gap_epoch);

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
        // The first NON-stale tick after a replay gap (plan ITEM 47): its
        // delta spans frames the replay never saw, so it belongs to no bar. A
        // stale packet leaves the gap pending for the next one.
        let gap_now = slot.replay_gap_rebase_pending && !is_stale_packet;
        // REPEAT-QUOTE TEST (2026-09-23). Dhan's Quote/Full packet carries the
        // LAST TRADE TIME, and Dhan re-sends it whenever the order book or open
        // interest changes — so one trade arrives many times, each copy
        // carrying the same trade time, the same price and the same
        // day-cumulative volume. Every copy was folded as a new trade: it
        // raised `tick_count`, widened the receipt stamps (so a bar's close
        // latency measured when the LAST book update arrived, not the last
        // trade), and — once the bar had sealed — re-emitted it as an
        // amendment. MEASURED on the box that day: 64,585 one-minute bars with
        // close latency over 60 s, the worst about 113 minutes.
        //
        // All three must match. A different price at the same second is a
        // different trade (and an index, whose volume is always 0, is told
        // apart by its price alone). A different cumulative is new volume.
        // The price compares BITS: `last_ltp` is NaN until the first accepted
        // tick, and NaN never equals itself, which is exactly right here.
        //
        // Evaluated BEFORE `last_ltp` is overwritten below; it is the only
        // instant both values exist. A stale packet can never match — its
        // cumulative is below the stored one by definition.
        let repeat_candidate = slot.volume_baseline_seeded
            && fold_secs == slot.last_trade_ts
            && cumulative_volume == slot.last_cumulative
            && slot.last_ltp.to_bits() == prices.last_traded_price.to_bits();
        // The fold second of the last tick whose cumulative COUNTED, before
        // this tick replaces it: after a gap, only a bucket that holds it can
        // take the skipped span (plan ITEM 47, review round 4).
        let last_counted_secs = slot.last_trade_ts;
        if !is_stale_packet {
            slot.last_ltp = prices.last_traded_price;
            slot.last_trade_ts = fold_secs;
        }
        // `true` when THIS tick seeds the baseline: the bucket it opens misses
        // this tick's own delta, so it is partial (plan ITEM 47).
        let seeded_now = !slot.volume_baseline_seeded;
        if !slot.volume_baseline_seeded {
            slot.volume_baseline_seeded = true;
            slot.last_cumulative = cumulative_volume;
            crate::candles::fold_counters::fold_counters()
                .slot_volume_baseline_seeded
                .increment(1);
        }
        if gap_now {
            slot.last_cumulative = cumulative_volume;
        }
        if replay_mode && (gap_now || seeded_now) {
            slot.replay_gap_frontier = slot
                .replay_gap_frontier
                .max(replay_gap_frontier_secs(tick.received_at_nanos, fold_secs));
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

        // A repeat that ALSO moved a session extreme is not treated as a
        // repeat: the exchange's own running high or low reports a print we
        // never received, and the fold below is what attributes it. Rare, and
        // folding it costs one tick of `tick_count` — the safe direction.
        if repeat_candidate && extremes.is_empty() {
            // A repeat of the last COUNTED trade resolves a pending gap (plan
            // ITEM 47, review round 13): it carries the same cumulative, so
            // the gap has nothing to discard. Left pending, the gap discarded
            // the NEXT real trade's delta instead — after a restart, where
            // Dhan re-sends the last trade on every order-book change, the
            // first live trade of the instrument lost its volume.
            //
            // ⚠ It still SETTLES, exactly as a gap tick does (review round 14,
            // both reproduced). The gap reset the tick-rule direction, so the
            // next trade at an unchanged price has no sign and its bar would
            // publish a null net over a stored signed one; and a STALE copy of
            // the last pre-gap state is indistinguishable from a true re-send,
            // so the next trade may carry a skipped span. Settling marks those
            // buckets partial: a replay withholds them, as it did before this
            // clear, while live mode (whose partial marks matter only before
            // the capture start) keeps the next trade's volume. O(1).
            if slot.replay_gap_rebase_pending {
                slot.replay_gap_rebase_pending = false;
                slot.replay_settling = true;
            }
            let mut refreshed_open: u16 = 0;
            for tf in TfIndex::ALL {
                // `false` means the bucket this trade belongs to has already
                // sealed (catch-up or rollover). Nothing is reopened and nothing
                // is amended: the sealed bar already holds this trade.
                if slot.cell.refresh_repeat_quote(tf, tick, &prices, fold_secs) {
                    refreshed_open |= replay_tf_bit(tf);
                }
            }
            // A repeat that refreshed an open bucket live may already have
            // closed by catch-up changed quote fields (open interest, the
            // buy/sell totals, possibly the day's open) that live never wrote
            // there. It adds no volume, so only this bucket is partial, not
            // the next (review round 8). `late_bound` is 0 outside a replay,
            // so live mode pays this one branch per repeat, not one per
            // timeframe (review round 11).
            if late_bound != 0 {
                for tf in TfIndex::ALL {
                    if refreshed_open & replay_tf_bit(tf) != 0
                        && slot.open_bucket_may_be_closed_live(tf, late_bound, catch_up_margin)
                    {
                        slot.replay_open_partial |= replay_tf_bit(tf);
                    }
                }
            }
            crate::candles::fold_counters::fold_counters()
                .repeat_quote
                .increment(1);
            return ConsumeStats {
                repeat_quote: true,
                ..ConsumeStats::default()
            };
        }

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

        // COUNTER-RESTART DETECTION — hoisted ABOVE the timeframe loop on
        // 2026-09-11. The ordering was a real defect, not a style point: a
        // frame whose bucket OPENED on this very tick seeded its net from an
        // unattributed carry measured against the PRE-restart counter, and the
        // rebase that clears that carry ran after the loop, too late to stop
        // it. The bar then published a sign for trades whose span no longer
        // existed — reproduced by
        // `hostile_a_carried_sign_across_a_restart_inverts_the_published_net`
        // as `volume 200, net_signed -800`: a fully SELL-initiated bar whose
        // only real flow was a 200-unit BUY. The magnitude form is
        // `hostile_a_carried_sign_survives_a_counter_restart_and_exceeds_the_bars_volume`.
        //
        // Hoisting is behaviour-preserving for everything else. `baseline` is
        // captured above, so this tick's classification still sees the old
        // anchor and its delta still saturates to 0; and `rebase_open_buckets`
        // PRESERVES each open bucket's counted volume while re-anchoring its
        // `bucket_start_cumulative`, so folding after the rebase lands on the
        // same volume as folding before it did.
        //
        // The two-events reasoning — why an ENORMOUS backwards step is a wrap
        // or a day rollover and a small one is a stale packet — is recorded in
        // full at the surviving stale-packet arm below.
        let restarted = cumulative_volume < slot.last_cumulative
            && slot.last_cumulative - cumulative_volume >= CUMULATIVE_RESTART_DROP_FLOOR;
        if restarted {
            // RE-ANCHOR on the new value rather than refusing it. This costs
            // exactly one tick's delta (the wrapping tick's own volume is
            // unattributable — its true delta spans the wrap and cannot be
            // recovered from a truncated counter) and keeps the instrument
            // alive for the remainder of the session.
            slot.last_cumulative = cumulative_volume;
            // Re-anchoring the SLOT baseline alone is NOT sufficient: every
            // bucket that is already OPEN still holds a
            // `bucket_start_cumulative` from before the restart, so its volume
            // would freeze for the rest of the bucket (up to 59 minutes on
            // M60) while `tick_count` kept rising. The cell re-bases those in
            // the same breath, preserving what each has already counted, and
            // drops every unattributed carry with them.
            slot.cell.rebase_open_buckets(cumulative_volume);
            crate::candles::fold_counters::fold_counters()
                .cumulative_reanchored
                .increment(1);
        }
        // REPLAY GAP (plan ITEM 47): the first accepted tick after a gap
        // re-bases every open bucket on this tick's cumulative. Each open
        // bucket keeps what it already counted; the skipped span is attributed
        // to no bar. The chain breaks only for a frame with no open bucket
        // (see `rebase_open_buckets_after_gap`). O(TF_COUNT), once per slot
        // per gap.
        // A counter restart inside the skipped span is handled by the
        // restart's own full re-base above, which also breaks every chain.
        // Any tick that rolls a bucket while a gap is still pending opens a
        // partial one: a stale packet leaves the gap pending, and the bucket
        // it opens chains to the pre-gap end (review round 4).
        let next_bucket_partial = seeded_now || slot.replay_gap_rebase_pending;
        if restarted && slot.replay_gap_rebase_pending {
            slot.replay_settling = true;
            // A counter restart resolves the gap instead of `gap_now` (it is
            // also stale), so it sets the frontier too (review round 7).
            if replay_mode {
                slot.replay_gap_frontier = slot
                    .replay_gap_frontier
                    .max(replay_gap_frontier_secs(tick.received_at_nanos, fold_secs));
            }
        }
        if restarted {
            slot.replay_gap_rebase_pending = false;
        } else if gap_now {
            slot.replay_settling = true;
            slot.replay_gap_rebase_pending = false;
            slot.cell.rebase_open_buckets_after_gap(
                cumulative_volume,
                fold_secs,
                last_counted_secs,
            );
        }

        // A tick that lands in an open bucket live may already have closed by
        // catch-up: live amended the closed bar and carried the volume into
        // the next one, the replay folds it into the open one. Neither bar can
        // be rebuilt as stored, so both are partial (and the last sealed bar
        // too, for a tick older than the open bucket, which live could not
        // have amended). Replay only, and outside the fold loop so live mode
        // pays one branch per tick, not one per timeframe (review rounds 7
        // and 8). Each timeframe reads and writes only its own bits, so doing
        // them all before the fold is the same as doing each before its own.
        if late_bound != 0 {
            for tf in TfIndex::ALL {
                if slot.open_bucket_may_be_closed_live(tf, late_bound, catch_up_margin)
                    && !slot.cell.would_seal(tf, fold_secs)
                {
                    let bit = replay_tf_bit(tf);
                    slot.replay_open_partial |= bit;
                    slot.replay_next_partial |= bit;
                    if fold_secs < slot.cell.open_bucket_start(tf) {
                        slot.replay_sealed_partial |= bit;
                    }
                }
            }
        }

        // Bars are held only during a replay and `finish_replay` releases
        // them all, so in live mode this is `false` and each timeframe below
        // tests one register flag instead of a load and a mask (review round
        // 11). Sampled once: a timeframe only sets or clears its OWN held bit,
        // and only after its own test.
        let any_held = slot.replay_held != 0;
        for tf in TfIndex::ALL {
            // A held bar becomes final the moment this tick seals the bar
            // after it: release it now, before the seal replaces it.
            if any_held
                && slot.replay_held & replay_tf_bit(tf) != 0
                && slot.cell.would_seal(tf, fold_secs)
            {
                match release_held(slot, tf, live_from, &mut on_seal) {
                    Released::Emitted => stats.sealed_count = stats.sealed_count.saturating_add(1),
                    Released::Suppressed => {
                        stats.replay_partial_suppressed =
                            stats.replay_partial_suppressed.saturating_add(1);
                        self.replay_suppressed_total =
                            self.replay_suppressed_total.saturating_add(1);
                    }
                    Released::Nothing => {}
                }
            }
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
                    // Plan ITEM 47: the bucket just sealed inherits the open
                    // bucket's replay bits; the bucket this tick opened is
                    // complete unless this tick seeded the baseline.
                    let before =
                        ended_before_capture(tf, sealed_state.bucket_start_ist_secs, live_from);
                    if slot.roll_replay_bits(tf, next_bucket_partial, replay_mode, before) {
                        stats.replay_partial_suppressed =
                            stats.replay_partial_suppressed.saturating_add(1);
                        self.replay_suppressed_total =
                            self.replay_suppressed_total.saturating_add(1);
                        crate::candles::fold_counters::fold_counters()
                            .refold_partial_suppressed
                            .increment(1);
                    } else if replay_mode {
                        // Held until it can no longer be amended.
                        slot.replay_held |= replay_tf_bit(tf);
                    } else {
                        stats.sealed_count = stats.sealed_count.saturating_add(1);
                        on_seal(key.0, key.1, key.2, tf, sealed_state);
                    }
                }
                ConsumeOutcome::AmendedLate { amended_state } => {
                    // A late tick amends the last SEALED bar. If a replay
                    // could only partly see that bar, re-emitting it would
                    // overwrite the complete live row (plan ITEM 47).
                    let before =
                        ended_before_capture(tf, amended_state.bucket_start_ist_secs, live_from);
                    if slot.sealed_is_suppressed(tf, replay_mode, before) {
                        stats.replay_partial_suppressed =
                            stats.replay_partial_suppressed.saturating_add(1);
                        self.replay_suppressed_total =
                            self.replay_suppressed_total.saturating_add(1);
                        crate::candles::fold_counters::fold_counters()
                            .refold_partial_suppressed
                            .increment(1);
                    } else if replay_mode {
                        // The held bar now carries the amendment; it is
                        // emitted when released.
                        slot.replay_held |= replay_tf_bit(tf);
                    } else {
                        stats.amended_count = stats.amended_count.saturating_add(1);
                        on_seal(key.0, key.1, key.2, tf, amended_state);
                    }
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
        // GAP FRONTIER (plan ITEM 47, review round 6): a bucket that starts at
        // or before it may still be missing a skipped tick. O(TF_COUNT), and
        // only while a frontier is set during a replay: live mode never pays
        // it, and `finish_replay` clears every frontier at the hand-over.
        if replay_mode && slot.replay_gap_frontier != 0 {
            let frontier = slot.replay_gap_frontier;
            for tf in TfIndex::ALL {
                let start = slot.cell.open_bucket_start(tf);
                if start != 0 && start <= frontier {
                    slot.replay_open_partial |= replay_tf_bit(tf);
                }
            }
            if fold_secs > frontier.saturating_add(MAX_BUCKET_SECS) {
                slot.replay_gap_frontier = 0;
            }
        }
        // POST-GAP SETTLING (plan ITEM 47, review round 5). While settling, a
        // tick that ADDS volume is uncertain: its delta is measured from a
        // baseline that may sit below the skipped span's highest cumulative,
        // and its sign may be unknown. Every bucket it landed in is marked
        // partial. A tick that adds nothing is exact and marks nothing, so a
        // zero-volume index never loses a bar. Settling ends on the first
        // such tick that also left the tick rule with a direction.
        if slot.replay_settling && !gap_now && !is_stale_packet && cumulative_volume > baseline {
            // Not a bucket the hand-over kept complete: live mode accepts this
            // uncertainty for every bucket it opens itself, and marking the
            // kept one would only let the capture-start rule drop it (round 12).
            slot.replay_open_partial |= REPLAY_ALL_TF_MASK & !slot.replay_handover_kept;
            if slot.last_tick_sign != 0 {
                slot.replay_settling = false;
            }
        }
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
            //
            // 2026-09-11: the RESTART half of this decision is made ABOVE the
            // timeframe loop (search `let restarted =`). It has to be: a
            // bucket that OPENS on the restarting tick would otherwise seed
            // its net from a carry the rebase had not yet cleared. A restart
            // therefore never reaches this arm — the hoisted branch has
            // already re-anchored `last_cumulative` to `cumulative_volume`, so
            // neither comparison above is true on that tick. What survives
            // here is the STALE-PACKET half, which needs no re-anchor: its
            // delta is already neutralised by `saturating_sub` and its price
            // was refused as an input to the tick rule above.
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
        let replay_mode = self.replay_mode;
        let gap_epoch = self.replay_gap_epoch;
        let live_from = self.live_capture_from_secs;
        for slot in &mut self.slots {
            slot.sync_replay_gap(gap_epoch);
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
            // Nothing carries across the day boundary, so neither does a
            // late tick's mark on the next bucket (review round 7).
            slot.replay_next_partial = 0;
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
            // The repeat-quote test already stands down while the baseline is
            // unseeded; clearing the trade time too means a stale value can
            // never be what makes a new day's first packet read as a repeat.
            slot.last_trade_ts = 0;
            for tf in TfIndex::ALL {
                // With no bucket open, `force_seal` can still return the LAST
                // SEALED bar amended with a settled carry; that bar is judged
                // by the sealed bits, and the open bits are left alone.
                // The day boundary clears the last sealed bar: a held one is
                // final now.
                match release_held(slot, tf, live_from, &mut on_seal) {
                    Released::Emitted => emitted = emitted.saturating_add(1),
                    Released::Suppressed => {
                        self.replay_suppressed_total =
                            self.replay_suppressed_total.saturating_add(1);
                    }
                    Released::Nothing => {}
                }
                let nothing_open = slot.cell.snapshot(tf).is_uninitialised();
                if let Some(state) = slot.cell.force_seal(tf) {
                    let before = ended_before_capture(tf, state.bucket_start_ist_secs, live_from);
                    let suppress = if nothing_open {
                        slot.sealed_is_suppressed(tf, replay_mode, before)
                    } else {
                        slot.take_open_partial(tf, replay_mode, before)
                    };
                    if suppress {
                        count_replay_partial_suppressed();
                        self.replay_suppressed_total =
                            self.replay_suppressed_total.saturating_add(1);
                    } else {
                        emitted = emitted.saturating_add(1);
                        on_seal(feed, sid, seg, tf, state);
                    }
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
    pub fn catch_up_seal_all<F>(&mut self, cutoff_secs: u32, on_seal: F) -> usize
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        self.catch_up_seal_slots(cutoff_secs, 0, usize::MAX, on_seal)
            .0
    }

    /// The same catch-up seal over at most `max_slots` slots starting at
    /// `from_slot` — the resumable form the frame drain runs in bounded steps
    /// from its idle arm (audit PR4b, 2026-09-26), so a frame waits for one
    /// step rather than for the whole book.
    ///
    /// Returns `(emitted, next_slot)`. The sweep is finished when `next_slot`
    /// reaches [`Self::len`]. Slots are only ever appended, never removed or
    /// reordered, so a cursor stays valid across calls: a slot created
    /// mid-sweep lands past the cursor and is visited by this sweep too.
    ///
    /// Sealing one slot's cells is independent of every other slot, so the
    /// bars a sweep produces are the same whether it runs in one call or many
    /// with the same `cutoff_secs` (pinned by
    /// `catch_up_seal_in_slices_seals_the_same_bars`).
    ///
    /// # Complexity
    /// O(`max_slots` × [`TF_COUNT`]) per call.
    pub fn catch_up_seal_slots<F>(
        &mut self,
        cutoff_secs: u32,
        from_slot: usize,
        max_slots: usize,
        mut on_seal: F,
    ) -> (usize, usize)
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        let len = self.slots.len();
        let start = from_slot.min(len);
        let end = start.saturating_add(max_slots).min(len);
        let mut emitted = 0_usize;
        let replay_mode = self.replay_mode;
        let gap_epoch = self.replay_gap_epoch;
        let live_from = self.live_capture_from_secs;
        if let Some(range) = self.slots.get_mut(start..end) {
            for slot in range {
                slot.sync_replay_gap(gap_epoch);
                let (feed, sid, seg) = slot.key;
                for tf in TfIndex::ALL {
                    if slot.replay_held & replay_tf_bit(tf) != 0
                        && slot.cell.would_catch_up_seal(tf, cutoff_secs)
                    {
                        match release_held(slot, tf, live_from, &mut on_seal) {
                            Released::Emitted => emitted = emitted.saturating_add(1),
                            Released::Suppressed => {
                                self.replay_suppressed_total =
                                    self.replay_suppressed_total.saturating_add(1);
                            }
                            Released::Nothing => {}
                        }
                    }
                    if let Some(state) = slot.cell.catch_up_seal(tf, cutoff_secs) {
                        let before =
                            ended_before_capture(tf, state.bucket_start_ist_secs, live_from);
                        if slot.take_open_partial(tf, replay_mode, before) {
                            count_replay_partial_suppressed();
                            self.replay_suppressed_total =
                                self.replay_suppressed_total.saturating_add(1);
                        } else if replay_mode {
                            slot.replay_held |= replay_tf_bit(tf);
                        } else {
                            emitted = emitted.saturating_add(1);
                            on_seal(feed, sid, seg, tf, state);
                        }
                    }
                }
            }
        }
        (emitted, end)
    }

    /// Turns WAL-replay mode on or off (plan ITEM 47).
    ///
    /// While it is on, a bar the replay could only partly see — one that
    /// spans a gap [`Self::mark_replay_gap`] recorded — is suppressed and
    /// counted instead of emitted, so it cannot overwrite the complete bar the
    /// live process already stored under the same key. Complete bars between
    /// gaps are emitted as before. Off (the default), nothing is suppressed.
    ///
    /// # Complexity
    /// O(1).
    pub fn set_replay_mode(&mut self, on: bool) {
        self.replay_mode = on;
    }

    /// Records a gap in a WAL replay: frames between the last folded one and
    /// the next were skipped, because the database had already applied them
    /// (plan ITEM 47).
    ///
    /// The first non-stale tick after the gap discards its own delta instead
    /// of taking the whole skipped span's volume into one bar — the
    /// 2026-09-28 defect, 733,406 shares in one second for one stock. The
    /// baseline stays seeded, so the stale-packet and counter-restart checks
    /// still run on that tick. Every open bucket and last-sealed bar is marked
    /// partial: its tail, or a late amendment to it, may be missing.
    ///
    /// Undercount, never overcount: a bucket that tick opens misses its delta
    /// and is itself marked partial; only a bucket that also holds the last
    /// counted tick takes the span, which the cumulative attributes exactly.
    ///
    /// # Complexity
    /// O(1). The gap is recorded as an epoch bump; each slot applies it the
    /// next time a tick or a seal sweep touches it
    /// ([`InstrumentSlot::sync_replay_gap`]). Consecutive gaps with no tick
    /// between them collapse into one, exactly as repeated eager marks did.
    /// (The first version walked every slot per gap: a replay whose frames
    /// alternate applied / unapplied could mark ~1.5 million gaps against
    /// 25,000 slots at boot. Found by review, 2026-09-29.)
    pub fn mark_replay_gap(&mut self) {
        self.replay_gap_epoch = self.replay_gap_epoch.wrapping_add(1);
    }

    /// Hands a WAL replay over to the live feed (plan ITEM 47). Call ONCE,
    /// after the last replayed frame and before the first live one.
    /// `ended_on_gap` is `true` when frames after the last replayed one were
    /// skipped (applied, or left unread by a stopped drain).
    ///
    /// A bucket still OPEN at this point seals later in live mode, where
    /// partial bars are emitted. So each bucket that cannot be complete is
    /// TAINTED — suppressed and counted when it seals or is amended, in any
    /// mode:
    /// - every open bucket the replay saw only in part (its head is missing);
    /// - when `ended_on_gap`, EVERY open bucket: its tail is missing, and the
    ///   previous process, which lived past the replay's last frame, stored
    ///   the complete bar. This is the 2026-09-28 11:02 shape: the replay's
    ///   last tick left the bar open (review, 2026-09-29);
    /// - every last-sealed bar the replay saw only in part, so a late tick
    ///   cannot re-emit it through an amendment.
    ///
    /// A complete open bucket on a replay that reached the end of the WAL is
    /// left alone: the WAL holds every frame the previous process folded
    /// into it, so it is the same bar the process would have stored, and it
    /// carries on with live ticks.
    ///
    /// Then replay mode ends and one more gap is marked, so the first live
    /// tick re-seeds instead of taking the downtime's volume.
    ///
    /// Every bar still HELD (see `InstrumentSlot::replay_held`) is released
    /// through `on_seal` here: emitted if complete, counted if tainted or
    /// partial.
    ///
    /// Returns `(tainted, suppressed)`: the buckets tainted here, and the bars
    /// suppressed as partial during the replay.
    ///
    /// **Honest limit:** after a crash, a bucket that was open when the
    /// process died and that the replay saw only in part had no stored row;
    /// it gets none now either, counted, rather than a row missing its head.
    /// Telling that case from a process that did store it would need a record
    /// of what the previous process sealed, which the WAL does not hold.
    /// Second limit (review rounds 5 and 6): a packet that is stale against a
    /// skipped span the replay never saw cannot always be recognised. An
    /// emitted bar can then exceed the live bar's volume by at most the
    /// largest backward step of the cumulative, and its net volume and close
    /// can differ too; the randomized property
    /// `test_replay_with_stale_packets_overcounts_at_most_the_largest_regression`
    /// bounds the volume half.
    /// Third limit (review round 7): bars are HELD in memory until this
    /// runs, while each catch-up round archives its segments as it goes. A
    /// process that dies between a round's archive and this hand-over loses
    /// the bars still held then (at most the last sealed bar per timeframe
    /// per instrument); the next boot does not re-read archived segments, so
    /// whatever the previous process stored for those buckets stands.
    ///
    /// # Complexity
    /// O(slots × `TF_COUNT`), once per lane start. A no-op when no replay ran.
    pub fn finish_replay<F>(&mut self, ended_on_gap: bool, mut on_seal: F) -> (u64, u64)
    where
        F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
    {
        if !self.replay_mode {
            return (0, 0);
        }
        let live_from = self.live_capture_from_secs;
        let mut released_suppressed = 0_u64;
        let epoch = self.replay_gap_epoch;
        // The hand-over gap (the downtime before the first live frame) is
        // applied HERE, eagerly, rather than through the lazy epoch sync: the
        // sync would mark every open bucket partial, and a complete one would
        // then be suppressed by the live `ended_before_capture` rule — after a
        // crash, the last bars the replay rebuilt in full, which exist nowhere
        // else (review round 4).
        let hand_over_epoch = epoch.wrapping_add(1);
        let mut tainted = 0_u64;
        // O(1) EXEMPT: begin — once per boot, at the WAL-replay to live hand-over, never per tick
        for slot in &mut self.slots {
            slot.sync_replay_gap(epoch);
            let mut open_mask = 0_u16;
            for tf in TfIndex::ALL {
                if !slot.cell.snapshot(tf).is_uninitialised() {
                    open_mask |= replay_tf_bit(tf);
                }
            }
            slot.replay_taint_open = if ended_on_gap {
                open_mask
            } else {
                slot.replay_open_partial & open_mask
            };
            // Ended on a gap: the skipped tail may also hold late amendments
            // to every last-sealed bar.
            slot.replay_taint_sealed = if ended_on_gap {
                REPLAY_ALL_TF_MASK
            } else {
                slot.replay_sealed_partial
            };
            slot.replay_handover_kept = open_mask & !slot.replay_taint_open;
            tainted = tainted.saturating_add(u64::from(slot.replay_taint_open.count_ones()));
            // Every bar still held is final now: emitted if complete, counted
            // if the taint or a gap marked it.
            for tf in TfIndex::ALL {
                if matches!(
                    release_held(slot, tf, live_from, &mut on_seal),
                    Released::Suppressed
                ) {
                    released_suppressed = released_suppressed.saturating_add(1);
                }
            }
            // The hand-over gap: the next accepted tick discards its delta and
            // re-bases, as after any gap. A frame with nothing open is partial
            // (its next bucket misses that tick); an open bucket keeps its
            // bits: a tainted one is held back by the taint, a complete one
            // stays complete and carries on with live ticks.
            slot.replay_gap_epoch_seen = hand_over_epoch;
            slot.last_tick_sign = 0;
            slot.replay_gap_rebase_pending = true;
            // The frontier only guards buckets a replay could not see in
            // full; each one still open was marked above, so live mode
            // carries none (review round 7).
            slot.replay_gap_frontier = 0;
            // Nor does it carry a late tick's mark on the next bucket: that
            // bucket opens after the hand-over, where the hand-over gap
            // already rules on it, and the previous process never stored
            // it, so a mark could only discard a bar that exists nowhere
            // else (review round 8).
            slot.replay_next_partial = 0;
            slot.replay_open_partial =
                (REPLAY_ALL_TF_MASK & !open_mask) | (slot.replay_open_partial & open_mask);
        }
        // O(1) EXEMPT: end
        self.replay_gap_epoch = hand_over_epoch;
        self.replay_mode = false;
        self.replay_suppressed_total = self
            .replay_suppressed_total
            .saturating_add(released_suppressed);
        (tainted, self.replay_suppressed_total)
    }

    /// Records the IST fold second at which this process began capturing
    /// live frames. From then on a PARTIAL bar whose bucket ended at or
    /// before it is suppressed and counted in every mode: it can only be a
    /// fragment, typically opened by the first packet after a subscribe
    /// carrying an old last-trade time, of a bar the previous process owned
    /// (see [`Self::live_capture_from_secs`]). Call once, before the first
    /// live frame. O(1).
    ///
    /// **Honest limits (review round 4):** a partial bar for a bucket that
    /// ended during a DOWNTIME, when no process owned it, is held back too:
    /// it would be a one-trade, zero-volume fragment either way. And if the
    /// box clock runs a few seconds ahead of the exchange, the first partial
    /// 1/3/5-second bars after a mid-session restart can be held back. At a
    /// pre-market boot the capture start precedes every bucket, so nothing
    /// changes there.
    pub fn set_live_capture_start(&mut self, ist_fold_secs: u32) {
        self.live_capture_from_secs = ist_fold_secs;
    }

    /// Sets how far behind the watermark the live catch-up seal closes a
    /// quiet bucket; the app passes the margin its catch-up uses, so a
    /// replay judges late ticks by the same rule (review round 7). O(1).
    pub fn set_catch_up_margin_secs(&mut self, secs: u32) {
        self.catch_up_margin_secs = secs;
    }
}

/// Fallback for [`MultiTfAggregator::set_catch_up_margin_secs`]: the app's
/// catch-up margin on 2026-09-29 (a measured 199 s delivery lag, rounded up
/// to whole minutes). A SMALLER margin makes a replay more cautious, never
/// less, so a stale default errs toward suppressing bars.
pub const DEFAULT_CATCH_UP_MARGIN_SECS: u32 = 240;

/// The receipt time of a tick in IST fold seconds, or `None` when the tick
/// carries no plausible receipt (legacy WAL records). O(1).
fn receipt_ist_secs(received_at_nanos: i64) -> Option<u32> {
    received_at_nanos
        .checked_div(1_000_000_000)
        .and_then(|secs| secs.checked_add(crate::candles::tf_index::IST_UTC_OFFSET_SECS))
        .and_then(|secs| u32::try_from(secs).ok())
        .filter(|_| received_at_nanos > 0)
}

impl InstrumentSlot {
    /// Applies a replay gap marked since this slot was last touched: the
    /// tick-rule carry is cleared, every open bucket and last-sealed bar is
    /// marked partial (its tail, or a late amendment, may be missing), and the
    /// next non-stale tick discards its own delta and re-bases the cell, so
    /// the skipped span's volume lands in no bar (the 2026-09-28 defect,
    /// 733,406 shares in one second for one stock). The baseline stays seeded.
    /// O(1); one compare when there is nothing to apply.
    #[inline]
    fn sync_replay_gap(&mut self, epoch: u64) {
        if self.replay_gap_epoch_seen == epoch {
            return;
        }
        self.replay_gap_epoch_seen = epoch;
        // The baseline stays SEEDED on purpose: the stale-packet and
        // counter-restart checks both read it, and switching it off let a
        // stale first packet after a gap re-seed LOW and count its span twice
        // (review, 2026-09-29). The gap tick's own delta is discarded in
        // `consume_tick` instead. The direction carry does not survive a gap.
        self.last_tick_sign = 0;
        self.replay_open_partial = REPLAY_ALL_TF_MASK;
        // The skipped span may also have held late amendments to the last
        // sealed bars, which the old process stored; re-emitting one without
        // them would overwrite the stored row (review, 2026-09-29).
        self.replay_sealed_partial = REPLAY_ALL_TF_MASK;
        self.replay_gap_rebase_pending = true;
    }

    /// For a bucket that is being sealed with no successor (catch-up or
    /// close seal): moves the open bucket's replay bits to the sealed bucket
    /// and returns whether the sealed bar must be suppressed.
    ///
    /// The next bucket opens on a later tick. It starts complete if the
    /// baseline is already seeded; if it is not (a gap was just marked, or
    /// the close seal reset the day), the tick that opens it re-seeds and
    /// misses its own delta, so it is marked partial now. O(1).
    fn take_open_partial(
        &mut self,
        tf: TfIndex,
        replay_mode: bool,
        ended_before_capture: bool,
    ) -> bool {
        // A gap still pending means the tick that opens the next bucket will
        // discard its own delta, exactly as a re-seeding tick does.
        let next_open_partial = !self.volume_baseline_seeded || self.replay_gap_rebase_pending;
        self.roll_replay_bits(tf, next_open_partial, replay_mode, ended_before_capture)
    }

    /// Whether `tf`'s open bucket may already have been closed in live mode by
    /// the periodic catch-up seal when a tick with this `late_bound` arrived:
    /// the bucket ended at least `margin` seconds before it (review rounds 7
    /// and 8). `false` with nothing open. O(1).
    #[inline]
    fn open_bucket_may_be_closed_live(&self, tf: TfIndex, late_bound: u32, margin: u32) -> bool {
        let open_start = self.cell.open_bucket_start(tf);
        open_start != 0
            && open_start
                .saturating_add(tf.seconds_per_bucket())
                .saturating_add(margin)
                <= late_bound
    }

    /// Moves `tf`'s open-bucket replay bits to the sealed bucket, sets the
    /// next open bucket's partial bit to `next_open_partial` and clears its
    /// taint (a bucket opened after the hand-over holds live data). Returns
    /// whether the bar being sealed must be suppressed: a partial bar during a
    /// replay, or a bar tainted at the hand-over in any mode. O(1).
    #[inline]
    fn roll_replay_bits(
        &mut self,
        tf: TfIndex,
        next_open_partial: bool,
        replay_mode: bool,
        ended_before_capture: bool,
    ) -> bool {
        let bit = replay_tf_bit(tf);
        let was_partial = self.replay_open_partial & bit != 0;
        let was_tainted = self.replay_taint_open & bit != 0;
        set_bit(&mut self.replay_sealed_partial, bit, was_partial);
        set_bit(&mut self.replay_taint_sealed, bit, was_tainted);
        // The taint belongs to the bucket open at the hand-over and moves to
        // the sealed slot with it; the bucket opening now holds live data.
        // Round 8 removed this clear along with the late-tick taint it used to
        // share a line with, and every later live bar of a tainted timeframe
        // was suppressed for the rest of the process (review round 11).
        self.replay_taint_open &= !bit;
        self.replay_handover_kept &= !bit;
        // A late tick marked the bucket after this one (review round 7).
        let forced = self.replay_next_partial & bit != 0;
        self.replay_next_partial &= !bit;
        set_bit(
            &mut self.replay_open_partial,
            bit,
            next_open_partial || forced,
        );
        (replay_mode && was_partial) || was_tainted || (was_partial && ended_before_capture)
    }

    /// Whether a late amendment of `tf`'s last sealed bar must be suppressed,
    /// by the same rule as [`Self::roll_replay_bits`]. O(1).
    #[inline]
    fn sealed_is_suppressed(
        &self,
        tf: TfIndex,
        replay_mode: bool,
        ended_before_capture: bool,
    ) -> bool {
        let bit = replay_tf_bit(tf);
        let partial = self.replay_sealed_partial & bit != 0;
        (replay_mode && partial)
            || self.replay_taint_sealed & bit != 0
            || (partial && ended_before_capture)
    }
}

/// The longest bucket of any timeframe, in seconds: a gap frontier this far
/// behind the fold can no longer touch an open bucket.
const MAX_BUCKET_SECS: u32 = {
    let mut max = 0;
    let mut i = 0;
    while i < TF_COUNT {
        let secs = TfIndex::ALL[i].seconds_per_bucket();
        if secs > max {
            max = secs;
        }
        i += 1;
    }
    max
};

/// Seconds the exchange clock may run ahead of this box's receipt clock.
/// Widens the gap frontier so a skipped tick stamped a little after its
/// receipt still falls inside it.
const REPLAY_FRONTIER_SKEW_SECS: u32 = 5;

/// The gap frontier for a slot's first tick after a gap: its receipt time in
/// IST fold seconds plus [`REPLAY_FRONTIER_SKEW_SECS`]. A tick with no
/// plausible receipt (legacy WAL records) falls back to its own trade time.
/// O(1).
fn replay_gap_frontier_secs(received_at_nanos: i64, fold_secs: u32) -> u32 {
    receipt_ist_secs(received_at_nanos)
        .unwrap_or(fold_secs)
        .max(fold_secs)
        .saturating_add(REPLAY_FRONTIER_SKEW_SECS)
}

/// What [`release_held`] did with a held bar.
enum Released {
    /// No bar was held for this timeframe.
    Nothing,
    /// The held bar was complete and was emitted.
    Emitted,
    /// The held bar turned out partial (a gap marked it) and was counted.
    Suppressed,
}

/// Releases `tf`'s held bar (plan ITEM 47): the cell's last sealed bar, which
/// carries every amendment the replay saw. Emitted if it is still complete,
/// counted if a later gap or the hand-over marked it partial or tainted. O(1).
fn release_held<F>(
    slot: &mut InstrumentSlot,
    tf: TfIndex,
    live_from: u32,
    on_seal: &mut F,
) -> Released
where
    F: FnMut(Feed, u64, u8, TfIndex, LiveCandleState),
{
    let bit = replay_tf_bit(tf);
    if slot.replay_held & bit == 0 {
        return Released::Nothing;
    }
    slot.replay_held &= !bit;
    let Some(bar) = slot.cell.last_sealed_snapshot(tf) else {
        return Released::Nothing;
    };
    let before = ended_before_capture(tf, bar.bucket_start_ist_secs, live_from);
    if slot.sealed_is_suppressed(tf, true, before) {
        count_replay_partial_suppressed();
        return Released::Suppressed;
    }
    let (feed, sid, seg) = slot.key;
    on_seal(feed, sid, seg, tf, bar);
    Released::Emitted
}

/// `true` when the `tf` bucket starting at `bucket_start` ended at or before
/// `live_from`, the instant this process began capturing (0 = not set). O(1).
#[inline]
fn ended_before_capture(tf: TfIndex, bucket_start: u32, live_from: u32) -> bool {
    live_from != 0 && bucket_start.saturating_add(tf.seconds_per_bucket()) <= live_from
}

/// Sets or clears `bit` in `mask`. O(1).
#[inline]
fn set_bit(mask: &mut u16, bit: u16, on: bool) {
    if on {
        *mask |= bit;
    } else {
        *mask &= !bit;
    }
}

/// Counts one bar a WAL replay suppressed as partial (plan ITEM 47).
fn count_replay_partial_suppressed() {
    crate::candles::fold_counters::fold_counters()
        .refold_partial_suppressed
        .increment(1);
}

/// A seal sink that ignores every bar, for tests that only drive the fold.
/// One shared function rather than a closure per call site: an inline
/// `|_, _, _, _, _| {}` that a test never reaches counted as an uncovered
/// line each time it was written (review round 7).
#[cfg(test)]
fn ignore_seal(_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState) {}

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

    // -- replay gaps (plan ITEM 47, 2026-09-29) ------------------------------
    //
    // A boot replay folds only the frames the database had not applied, so
    // the replayed stream has gaps. Measured on 2026-09-28: one stock's first
    // tick after a gap put 733,406 shares into one second, and the rebuilt
    // partial bars overwrote the complete live ones for ~696 stocks.

    const GAP_SID: u64 = 2_885;

    /// A tick for the gap tests whose price moves with the cumulative, so
    /// the tick rule has a direction as it would on a real contract: after a
    /// gap, a volume-adding tick with no known direction is uncertain and its
    /// bars are held back (review round 5).
    fn gtick(ts: u32, cum: u32) -> ParsedTick {
        tick(
            GAP_SID,
            SEG_EQ,
            ts,
            1_000.0 + (cum % 20_000) as f32 * 0.05,
            cum,
        )
    }

    /// Feeds the 2026-09-28 shape: two ticks, a skipped span, then ten ticks
    /// of 10 shares each and one more tick a minute later. `gap` marks the
    /// skipped span. Returns every emitted `(tf, bucket_start, volume)` and
    /// the total suppressed count.
    fn feed_gap_shape(replay: bool, gap: bool) -> (Vec<(TfIndex, u32, i64)>, u32) {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(replay);
        let mut out: Vec<(TfIndex, u32, i64)> = Vec::new();
        let mut suppressed = 0_u32;
        let mut fold = |agg: &mut MultiTfAggregator,
                        out: &mut Vec<(TfIndex, u32, i64)>,
                        ts: u32,
                        _px: f32,
                        cum: u32| {
            let t = gtick(ts, cum);
            let stats = agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                out.push((tf, st.bucket_start_ist_secs, st.signed_volume()));
            });
            suppressed += u32::from(stats.replay_partial_suppressed);
        };
        fold(&mut agg, &mut out, OPEN, 1_215.0, 1_000);
        fold(&mut agg, &mut out, OPEN + 1, 1_215.0, 1_010);
        if gap {
            agg.mark_replay_gap();
        }
        // After the skipped span: the day total has moved by 733,406.
        let mut cum = 734_416_u32;
        fold(&mut agg, &mut out, OPEN + 300, 1_208.7, cum);
        for s in 1..=10 {
            cum += 10;
            fold(&mut agg, &mut out, OPEN + 300 + s, 1_208.7, cum);
        }
        fold(&mut agg, &mut out, OPEN + 400, 1_208.8, cum + 10);
        (out, suppressed)
    }

    /// The defect, reproduced: with no gap marked, the first tick after the
    /// skipped span carries the whole span's volume in one second.
    #[test]
    fn test_regression_wal_refold_gap_dumps_skipped_volume_into_one_partial_bar() {
        let (unmarked, _) = feed_gap_shape(true, false);
        assert!(
            unmarked.iter().any(|(tf, start, v)| *tf == TfIndex::S1
                && *start == OPEN + 300
                && v.abs() == 733_406),
            "without the gap marker the 1-second bar takes the skipped span: {unmarked:?}"
        );

        // Fixed: the gap is marked during a replay.
        let (marked, suppressed) = feed_gap_shape(true, true);
        assert!(
            marked.iter().all(|(_, _, v)| v.abs() <= 110),
            "no emitted bar may hold more than the 110 shares the replay \
             actually saw after the gap: {marked:?}"
        );
        assert!(suppressed > 0, "the partial bars are counted, never silent");
        assert!(
            !marked
                .iter()
                .any(|(tf, start, _)| *tf == TfIndex::S1 && *start == OPEN + 300),
            "the bar the re-seeding tick opened is partial and not emitted"
        );
        assert!(
            !marked
                .iter()
                .any(|(tf, start, _)| *tf == TfIndex::M1 && *start == OPEN),
            "the minute that was open across the gap lost its tail: not emitted"
        );
    }

    /// Review 2026-09-29: marking a gap is O(1) and lazy. Any number of marks
    /// with no tick between them behaves exactly like one, so a replay with
    /// many gaps cannot cost gaps x slots.
    #[test]
    fn test_mark_replay_gap_repeated_marks_collapse_into_one() {
        let run = |marks: u32| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            let mut out: Vec<(TfIndex, u32, i64)> = Vec::new();
            let mut sink = |_: Feed, _: u64, _: u8, tf: TfIndex, st: LiveCandleState| {
                out.push((tf, st.bucket_start_ist_secs, st.signed_volume()));
            };
            agg.consume_tick(Feed::Dhan, &gtick(OPEN, 1_000), None, &mut sink);
            for _ in 0..marks {
                agg.mark_replay_gap();
            }
            let mut cum = 734_416_u32;
            for s in 0..=10 {
                cum += 10;
                agg.consume_tick(Feed::Dhan, &gtick(OPEN + 300 + s, cum), None, &mut sink);
            }
            out
        };
        let once = run(1);
        assert_eq!(
            run(1_000),
            once,
            "a thousand marks with no tick between are one gap"
        );
        assert!(once.iter().all(|(_, _, v)| v.abs() <= 110), "{once:?}");
    }

    /// Review 2026-09-29: a slot no tick touches after the gap still learns of
    /// it, through the seal sweep. The slot was built outside replay mode, so
    /// its open buckets were complete until the gap; after it they are partial
    /// and the sweep must not emit them.
    #[test]
    fn test_mark_replay_gap_reaches_a_slot_through_the_seal_sweep() {
        let seal_after = |gap: bool| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.consume_tick(Feed::Dhan, &gtick(OPEN, 1_000), None, ignore_seal);
            // The first volume increase settles the new slot; the second
            // second's bucket after it is complete.
            for (ts, cum) in [(OPEN + 1, 1_005), (OPEN + 2, 1_010)] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
            }
            agg.set_replay_mode(true);
            if gap {
                agg.mark_replay_gap();
            }
            // The sweep seals; a replay holds what it seals until the
            // hand-over releases it.
            let mut emitted = agg.catch_up_seal_all(OPEN + 100_000, ignore_seal);
            agg.finish_replay(false, |_, _, _, _, _| emitted += 1);
            emitted
        };
        assert!(
            seal_after(false) > 0,
            "no gap: the complete open buckets seal and emit"
        );
        assert_eq!(
            seal_after(true),
            0,
            "after a gap every open bucket is partial"
        );
    }

    /// Review 2026-09-29, finding 2: after a gap, a LONGER timeframe that did
    /// not roll on the re-seeding tick must chain its next bucket to the
    /// re-based endpoint. Breaking the chain there anchored the next bucket on
    /// its rolling tick and dropped that tick's volume from a bar marked
    /// complete, which then overwrote the complete stored row short.
    #[test]
    fn test_mark_replay_gap_next_longer_bucket_keeps_its_rolling_tick() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let mut out: Vec<(TfIndex, u32, i64)> = Vec::new();
        let mut sink = |_: Feed, _: u64, _: u8, tf: TfIndex, st: LiveCandleState| {
            out.push((tf, st.bucket_start_ist_secs, st.signed_volume()));
        };
        let t0 = OPEN + 60;
        for (ts, cum) in [(t0 + 5, 1_000), (t0 + 10, 1_010)] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, &mut sink);
        }
        agg.mark_replay_gap();
        for (ts, cum) in [
            (t0 + 40, 5_000),
            (t0 + 50, 5_010),
            (t0 + 65, 5_100),
            (t0 + 125, 5_200),
        ] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, &mut sink);
        }
        agg.finish_replay(false, &mut sink);
        assert!(
            out.iter()
                .any(|(tf, start, v)| *tf == TfIndex::M1 && *start == t0 + 60 && v.abs() == 90),
            "the minute after the gap minute holds its own 90 shares: {out:?}"
        );
        assert!(
            !out.iter()
                .any(|(tf, start, _)| *tf == TfIndex::M1 && *start == t0),
            "the minute open across the gap is still suppressed: {out:?}"
        );
    }

    /// Plan ITEM 47, review round 13 (found by the mixed-stream restart
    /// property): after a gap, Dhan's first packet is often a RE-SEND of the
    /// last counted trade (it re-sends on every order-book change). The
    /// repeat path returned early and left the gap pending, so the next real
    /// trade's delta was discarded as the gap tick's: after a restart the
    /// first live trade of the instrument lost its volume (0 instead of 10
    /// here). The re-send carries the same cumulative, which proves nothing
    /// traded in between, so it resolves the gap itself.
    #[test]
    fn test_regression_a_resent_trade_resolves_the_gap_without_losing_the_next_trade() {
        // `mode`: 0 continuous live, 1 restart (gap at the hand-over),
        // 2 a gap inside the replay itself.
        let run = |mode: u8| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            if mode != 0 {
                agg.set_replay_mode(true);
            }
            for (ts, cum) in [(OPEN + 60, 1_000), (OPEN + 61, 1_050)] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
            }
            match mode {
                1 => {
                    agg.set_live_capture_start(OPEN + 100);
                    agg.finish_replay(false, ignore_seal);
                }
                2 => agg.mark_replay_gap(),
                _ => {}
            }
            // The re-send (open interest moved), then a real trade.
            let mut resent = gtick(OPEN + 61, 1_050);
            resent.open_interest = 7;
            let stats = agg.consume_tick(Feed::Dhan, &resent, None, ignore_seal);
            assert!(
                stats.repeat_quote,
                "mode {mode}: the re-send is a repeat quote"
            );
            let mut minute = None;
            agg.consume_tick(Feed::Dhan, &gtick(OPEN + 185, 1_060), None, ignore_seal);
            if mode == 2 {
                agg.finish_replay(false, ignore_seal);
            }
            agg.catch_up_seal_all(OPEN + 100_000, |_, _, _, tf, st| {
                if tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 180 {
                    minute = Some((st.volume, st.net_volume_classified));
                }
            });
            minute
        };
        let continuous = run(0);
        assert_eq!(continuous.map(|m| m.0), Some(10), "control");
        assert_eq!(run(1), continuous, "after a restart");
        // Inside a replay the minute still settles (review round 14): it is
        // withheld, never written with a different buy/sell split.
        assert_eq!(run(2), None, "after a gap inside the replay");
    }

    /// Review round 14 (HIGH, reproduced by the reviewer): the re-send clears
    /// the gap, but the gap also reset the tick-rule direction. Without
    /// settling, the next trade at an UNCHANGED price had no sign, and the
    /// replay wrote its minute with an unclassified net over the stored,
    /// classified one. Whatever the replay emits must equal what live stored.
    #[test]
    fn test_regression_a_resent_trade_still_settles_an_unsigned_next_trade() {
        let at = |ts: u32, px: f32, cum: u32| tick(GAP_SID, SEG_EQ, ts, px, cum);
        let run = |replay: bool| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            if replay {
                agg.set_replay_mode(true);
            }
            let mut minute = None;
            let mut keep = |_: Feed, _: u64, _: u8, tf: TfIndex, st: LiveCandleState| {
                if tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 360 {
                    minute = Some((st.volume, st.net_volume_classified));
                }
            };
            agg.consume_tick(Feed::Dhan, &at(OPEN + 60, 100.0, 1_000), None, &mut keep);
            agg.consume_tick(Feed::Dhan, &at(OPEN + 61, 101.0, 1_050), None, &mut keep);
            if replay {
                agg.mark_replay_gap();
            }
            let mut resent = at(OPEN + 61, 101.0, 1_050);
            resent.open_interest = 7;
            agg.consume_tick(Feed::Dhan, &resent, None, &mut keep);
            // Same price: only the carried direction can sign it.
            agg.consume_tick(Feed::Dhan, &at(OPEN + 400, 101.0, 1_060), None, &mut keep);
            if replay {
                agg.finish_replay(false, &mut keep);
            }
            agg.catch_up_seal_all(OPEN + 100_000, &mut keep);
            minute
        };
        let stored = run(false);
        assert_eq!(
            stored,
            Some((10, true)),
            "control: live signs it from the carry"
        );
        let rebuilt = run(true);
        assert!(
            rebuilt.is_none() || rebuilt == stored,
            "the replay wrote {rebuilt:?} over the stored {stored:?}"
        );
    }

    /// Plan ITEM 47, review round 12 (MEDIUM, reproduced by the reviewer): a
    /// clean hand-over KEEPS the complete buckets still open (round 4: after
    /// a crash they exist nowhere else). The post-hand-over settling then
    /// marked every open bucket partial on the first live tick that added
    /// volume, and a kept bucket that ended before the capture start was
    /// suppressed when it rolled — so whether it survived depended on the
    /// delivery lag of the next two ticks. Kept buckets must come out exactly
    /// as with no capture-start rule at all.
    #[test]
    fn test_regression_live_settling_does_not_drop_a_bucket_the_hand_over_kept() {
        let run = |capture_start: u32| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            let mut cum = 1_000_u32;
            // Every 5 s from 09:15:05 to 09:29:40, nothing skipped: the M5
            // 09:25 and M10 09:20 buckets are complete and open at the end.
            let mut ts = OPEN + 5;
            while ts <= OPEN + 880 {
                cum += 100;
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
                ts += 5;
            }
            agg.set_live_capture_start(capture_start);
            agg.finish_replay(false, ignore_seal);
            // Live, with the usual delivery lag: trade times 09:29:52 and
            // 09:29:57 arrive after the 09:30:02 capture start.
            let mut out: std::collections::HashMap<(TfIndex, u32), i64> =
                std::collections::HashMap::new();
            for (t, add) in [(OPEN + 892, 100), (OPEN + 897, 100), (OPEN + 910, 100)] {
                cum += add;
                agg.consume_tick(Feed::Dhan, &gtick(t, cum), None, |_, _, _, tf, st| {
                    out.insert((tf, st.bucket_start_ist_secs), st.signed_volume());
                });
            }
            out
        };
        let control = run(0);
        let with_rule = run(OPEN + 902);
        for (tf, start) in [
            (TfIndex::M5, OPEN + 600),
            (TfIndex::M10, OPEN + 300),
            (TfIndex::M1, OPEN + 840),
        ] {
            assert!(
                control.contains_key(&(tf, start)),
                "{tf:?} {start}: the control run emits the kept bucket: {control:?}"
            );
            assert_eq!(
                with_rule.get(&(tf, start)),
                control.get(&(tf, start)),
                "{tf:?} {start}: a bucket the hand-over kept is dropped by live settling"
            );
        }
    }

    /// Plan ITEM 47, review round 11 (CRITICAL, reproduced by the reviewer):
    /// the hand-over taint belongs to the ONE bucket open at the hand-over.
    /// Round 8 removed the line in `roll_replay_bits` that cleared it on the
    /// roll, so a tainted timeframe suppressed every later live bar of that
    /// instrument for the rest of the process — M3 to M60 for most
    /// instruments after any restart, and every timeframe when the replay
    /// ended on a gap. Here three hours of live ticks follow a tainted
    /// hand-over: only the tainted bucket may be withheld.
    #[test]
    fn test_regression_hand_over_taint_suppresses_only_the_bucket_open_at_the_hand_over() {
        for ended_on_gap in [false, true] {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            agg.mark_replay_gap();
            for (ts, cum) in [(OPEN + 62, 1_000), (OPEN + 70, 1_050)] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
            }
            agg.set_live_capture_start(OPEN + 100);
            agg.finish_replay(ended_on_gap, ignore_seal);
            assert!(
                agg.slots[0].replay_taint_open != 0,
                "ended_on_gap={ended_on_gap}: the hand-over tainted something"
            );

            let mut emitted: std::collections::HashSet<(TfIndex, u32)> =
                std::collections::HashSet::new();
            let mut cum = 2_000_u32;
            let mut ts = OPEN + 120;
            while ts < OPEN + 120 + 3 * 3_600 {
                cum += 10;
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, |_, _, _, tf, st| {
                    emitted.insert((tf, st.bucket_start_ist_secs));
                });
                ts += 10;
            }
            agg.catch_up_seal_all(OPEN + 100_000, |_, _, _, tf, st| {
                emitted.insert((tf, st.bucket_start_ist_secs));
            });

            for tf in TfIndex::ALL {
                let tainted = tf.bucket_start(OPEN + 70);
                assert!(
                    !emitted.contains(&(tf, tainted)),
                    "ended_on_gap={ended_on_gap} {tf:?}: the bucket open at the hand-over \
                     is withheld"
                );
                let mut t = OPEN + 120;
                while t < OPEN + 120 + 3 * 3_600 {
                    let bucket = tf.bucket_start(t);
                    if bucket > tainted {
                        assert!(
                            emitted.contains(&(tf, bucket)),
                            "ended_on_gap={ended_on_gap} {tf:?}: live bucket {bucket} \
                             after the tainted one {tainted} must be emitted"
                        );
                    }
                    t += 10;
                }
            }
        }
    }

    /// Review 2026-09-29, finding 1: a partial bucket still OPEN when the
    /// replay hands over to the live feed must not seal later in live mode and
    /// overwrite the complete stored bar. A complete open bucket is unaffected.
    #[test]
    fn test_finish_replay_taints_open_partial_buckets_only() {
        // Every bucket the replay touched is partial (slot first seen during
        // the replay): after the hand-over, nothing it left open is emitted.
        let seal_after = |finish: bool| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            agg.mark_replay_gap();
            for (ts, cum) in [(OPEN + 62, 1_000), (OPEN + 70, 1_050)] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
            }
            if finish {
                agg.finish_replay(false, ignore_seal);
            } else {
                agg.set_replay_mode(false);
            }
            let mut sealed: Vec<(TfIndex, u32)> = Vec::new();
            agg.catch_up_seal_all(OPEN + 100_000, |_, _, _, tf, st| {
                sealed.push((tf, st.bucket_start_ist_secs));
            });
            sealed
        };
        assert!(
            seal_after(false).iter().any(|(tf, _)| *tf == TfIndex::M1),
            "without the hand-over the partial minute leaks out in live mode"
        );
        // Only buckets the second tick opened survive: each holds its whole
        // tick. Nothing that holds the seeding tick, and no minute, leaks.
        let finished = seal_after(true);
        assert!(
            finished
                .iter()
                .all(|(tf, start)| *tf != TfIndex::M1 && *start > OPEN + 62),
            "{finished:?}"
        );

        // A minute opened after the re-seed is complete: it still seals.
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        for (ts, cum) in [
            (OPEN + 20, 995),
            (OPEN + 30, 1_000),
            (OPEN + 60, 1_010),
            (OPEN + 70, 1_050),
        ] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
        }
        // As in production: the lane records when it began listening, here
        // after the complete minute ended (a crash-restart; review round 4).
        agg.set_live_capture_start(OPEN + 3_000);
        agg.finish_replay(false, ignore_seal);
        let mut sealed: Vec<(TfIndex, u32, i64)> = Vec::new();
        agg.catch_up_seal_all(OPEN + 100_000, |_, _, _, tf, st| {
            sealed.push((tf, st.bucket_start_ist_secs, st.signed_volume()));
        });
        assert!(
            sealed
                .iter()
                .any(|(tf, start, v)| *tf == TfIndex::M1 && *start == OPEN + 60 && v.abs() == 50),
            "the complete minute is emitted after the hand-over: {sealed:?}"
        );

        // After the hand-over, a live bucket opened after the capture start is
        // live data: emitted.
        let mut out = 0_usize;
        agg.consume_tick(Feed::Dhan, &gtick(OPEN + 3_600, 2_000), None, ignore_seal);
        agg.consume_tick(Feed::Dhan, &gtick(OPEN + 3_610, 2_010), None, ignore_seal);
        agg.consume_tick(
            Feed::Dhan,
            &gtick(OPEN + 3_671, 2_020),
            None,
            |_, _, _, tf, _| {
                if tf == TfIndex::M1 {
                    out += 1;
                }
            },
        );
        assert_eq!(out, 1, "the first live minute seals normally in live mode");
    }

    /// Review round 2 (2026-09-29): a gap may have skipped late amendments to
    /// the last SEALED bar, which the previous process stored. A replayed
    /// late tick after the gap must not re-emit that bar without them.
    #[test]
    fn test_mark_replay_gap_suppresses_a_late_amendment_of_a_bar_sealed_before_it() {
        let run = |gap: bool| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            let t0 = OPEN + 60;
            // Seed; the t0 + 60 minute is partial (the post-seed settling
            // marks it); the t0 + 120 minute is complete and sealed, so HELD,
            // by t0 + 180.
            for (ts, cum) in [
                (t0 + 5, 1_000),
                (t0 + 60, 1_010),
                (t0 + 70, 1_050),
                (t0 + 120, 1_100),
                (t0 + 130, 1_110),
                (t0 + 180, 1_150),
            ] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
            }
            if gap {
                agg.mark_replay_gap();
            }
            // A late tick for the sealed t0 + 120 minute. The held bar is
            // released at the hand-over, so that is where it is counted
            // (review round 7: counting only this tick's callback read 0 in
            // both arms once replay began holding sealed bars).
            let late = gtick(t0 + 170, 1_160);
            agg.consume_tick(Feed::Dhan, &late, None, ignore_seal);
            let mut emitted_m1 = 0_u32;
            agg.finish_replay(false, |_, _, _, tf, st| {
                if tf == TfIndex::M1 && st.bucket_start_ist_secs == t0 + 120 {
                    emitted_m1 += 1;
                }
            });
            (emitted_m1, ())
        };
        let (without_gap, ()) = run(false);
        assert_eq!(
            without_gap, 1,
            "control: with no gap the complete, amended bar is written once"
        );
        let (after_gap, ()) = run(true);
        assert_eq!(
            after_gap, 0,
            "after a gap the sealed bar is partial: not re-emitted"
        );
    }

    /// Review round 2 (2026-09-29), candle finding 1: across the hand-over, a
    /// live tick that lands in the SAME bucket the replay left open keeps the
    /// whole span's volume. The cumulative counter attributes it exactly;
    /// re-basing there dropped the downtime from the first live bar.
    #[test]
    fn test_finish_replay_same_bucket_live_tick_keeps_the_downtime_volume() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let t0 = OPEN + 600; // a minute boundary
        // Replay: a complete minute opened by a roll, then two ticks in it.
        for (ts, cum) in [
            (t0 - 40, 48_000),
            (t0 - 30, 49_000),
            (t0 + 5, 50_000),
            (t0 + 20, 50_300),
        ] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
        }
        agg.finish_replay(false, ignore_seal);
        let mut m1: Vec<(u32, i64)> = Vec::new();
        for (ts, cum) in [(t0 + 40, 51_000), (t0 + 50, 51_100), (t0 + 61, 51_200)] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, |_, _, _, tf, st| {
                if tf == TfIndex::M1 {
                    m1.push((st.bucket_start_ist_secs, st.signed_volume()));
                }
            });
        }
        assert!(
            m1.iter()
                .any(|(start, v)| *start == t0 && v.unsigned_abs() == 51_100 - 49_000),
            "the minute holds every share from its opening tick to its last: {m1:?}"
        );
    }

    /// Review round 2 (2026-09-29), data-loss finding 1: when the replay
    /// ENDED on a gap (the previous process lived on past its last frame and
    /// stored the complete bar), even a complete-looking open bucket is held
    /// back: its tail is missing.
    #[test]
    fn test_finish_replay_ended_on_gap_taints_every_open_bucket() {
        let sealed_after = |ended_on_gap: bool| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            for (ts, cum) in [
                (OPEN + 20, 995),
                (OPEN + 30, 1_000),
                (OPEN + 60, 1_010),
                (OPEN + 70, 1_050),
            ] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
            }
            let (tainted, _) = agg.finish_replay(ended_on_gap, ignore_seal);
            let mut m1 = 0_usize;
            agg.catch_up_seal_all(OPEN + 100_000, |_, _, _, tf, st| {
                if tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 60 {
                    m1 += 1;
                }
            });
            (tainted, m1)
        };
        assert_eq!(
            sealed_after(false).1,
            1,
            "reached the end of the WAL: the complete minute seals"
        );
        let (tainted, m1) = sealed_after(true);
        assert!(tainted > 0);
        assert_eq!(
            m1, 0,
            "ended on a gap: the minute is missing its tail and is held back"
        );
    }

    /// Review round 2 (2026-09-29), candle finding 2: with no bucket open,
    /// the close seal can return the LAST SEALED bar amended with a settled
    /// carry. It must be judged by the sealed bits: a partial replayed bar is
    /// not re-emitted through that path either.
    #[test]
    fn test_force_seal_all_judges_an_amended_last_sealed_bar_by_its_sealed_bits() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        // A slot first seen in the replay: its first minute is partial.
        for (ts, cum) in [(OPEN + 62, 1_000), (OPEN + 70, 1_050)] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
        }
        // The catch-up seal closes that minute (suppressed); nothing is open.
        agg.catch_up_seal_all(OPEN + 200, ignore_seal);
        // A late tick for that minute leaves a carry behind.
        agg.consume_tick(Feed::Dhan, &gtick(OPEN + 100, 1_080), None, ignore_seal);
        let mut reemitted = 0_usize;
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 60 {
                reemitted += 1;
            }
        });
        assert_eq!(
            reemitted, 0,
            "the partial minute is not re-emitted by the close seal"
        );
    }

    /// Review round 3 (2026-09-29): a STALE first packet after a gap must not
    /// re-seed the baseline low and count its span twice. The baseline stays
    /// seeded across a gap, so the stale gate still refuses it.
    #[test]
    fn test_mark_replay_gap_stale_first_packet_does_not_count_twice() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let t0 = OPEN + 600;
        let mut bars: Vec<(TfIndex, u32, u64, i64)> = Vec::new();
        let mut feed = |agg: &mut MultiTfAggregator, ts: u32, cum: u32| {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, |_, _, _, tf, st| {
                bars.push((
                    tf,
                    st.bucket_start_ist_secs,
                    st.volume,
                    st.net_volume_signed,
                ));
            });
        };
        for (ts, cum) in [(t0 + 5, 800), (t0 + 10, 900), (t0 + 15, 1_000)] {
            feed(&mut agg, ts, cum);
        }
        agg.mark_replay_gap();
        feed(&mut agg, t0 + 20, 950); // stale: below the last accepted 1,000
        for (ts, cum) in [
            (t0 + 25, 1_100),
            (t0 + 30, 1_150),
            (t0 + 65, 1_200),
            (t0 + 125, 1_300),
        ] {
            feed(&mut agg, ts, cum);
        }
        agg.finish_replay(false, |_, _, _, tf, st| {
            bars.push((
                tf,
                st.bucket_start_ist_secs,
                st.volume,
                st.net_volume_signed,
            ));
        });
        assert!(
            bars.iter().all(|(_, _, v, net)| net.unsigned_abs() <= *v),
            "no bar may carry more net than volume: {bars:?}"
        );
        assert!(
            bars.iter()
                .any(|(tf, start, v, _)| *tf == TfIndex::M1 && *start == t0 + 60 && *v == 50),
            "the minute after the gap holds exactly its 50 shares (1,150 to 1,200): {bars:?}"
        );
        // The second opened after the stale packet: 100 shares traded, and a
        // low re-seed at 950 would have published 150.
        assert!(
            !bars
                .iter()
                .any(|(tf, start, v, _)| *tf == TfIndex::S1 && *start == t0 + 25 && *v > 100),
            "no bar counts the stale packet's span twice: {bars:?}"
        );
    }

    /// Review round 3 (2026-09-29): a counter RESTART inside the skipped span
    /// must still be detected, or the frame freezes on the old axis and every
    /// later bar reads 0.
    #[test]
    fn test_mark_replay_gap_counter_restart_inside_the_gap_is_detected() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let t0 = OPEN + 600;
        let mut m1: Vec<(u32, u64)> = Vec::new();
        let mut feed = |agg: &mut MultiTfAggregator, ts: u32, cum: u32| {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, |_, _, _, tf, st| {
                if tf == TfIndex::M1 {
                    m1.push((st.bucket_start_ist_secs, st.volume));
                }
            });
        };
        for (ts, cum) in [(t0 + 5, 4_000_000_000), (t0 + 10, 4_000_000_500)] {
            feed(&mut agg, ts, cum);
        }
        agg.mark_replay_gap();
        for (ts, cum) in [
            (t0 + 20, 100_000), // the counter restarted inside the gap
            (t0 + 30, 100_050),
            (t0 + 65, 100_100),
            (t0 + 125, 100_200),
            (t0 + 185, 100_300),
        ] {
            feed(&mut agg, ts, cum);
        }
        // A replay holds each sealed bar until it can no longer be amended;
        // the hand-over releases the rest.
        agg.finish_replay(false, |_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                m1.push((st.bucket_start_ist_secs, st.volume));
            }
        });
        assert!(
            m1.iter().any(|(start, v)| *start == t0 + 120 && *v == 100),
            "a minute after the restart counts on the new axis: {m1:?}"
        );
    }

    /// Review round 3 (2026-09-29), finding HIGH-1: the first packet after a
    /// subscribe carries the instrument's LAST trade time, which for a quiet
    /// contract can be an hour old. It must not open a one-tick bar for that
    /// old bucket and overwrite the row the previous process stored.
    #[test]
    fn test_set_live_capture_start_suppresses_a_fragment_of_an_old_bucket() {
        let run = |capture_from: u32| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_live_capture_start(capture_from);
            let restart = OPEN + 3_000; // 10:05
            let mut m1: Vec<u32> = Vec::new();
            for (ts, cum) in [
                (OPEN + 600, 5_000), // snapshot: last traded at 09:25
                (restart + 5, 5_100),
                (restart + 70, 5_200),
                (restart + 130, 5_300),
            ] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, |_, _, _, tf, st| {
                    if tf == TfIndex::M1 {
                        m1.push(st.bucket_start_ist_secs);
                    }
                });
            }
            m1
        };
        let without = run(0);
        assert!(
            without.contains(&(OPEN + 600)),
            "control: the old fragment is emitted: {without:?}"
        );
        let with = run(OPEN + 3_000);
        assert!(
            !with.contains(&(OPEN + 600)),
            "the 09:25 fragment is held back: {with:?}"
        );
        assert!(
            with.contains(&(OPEN + 3_060)),
            "live minutes after the restart are emitted: {with:?}"
        );
    }

    /// Review round 3 (2026-09-29): a replay that ENDED on a gap may have
    /// skipped late amendments to every last-sealed bar; a live late tick must
    /// not re-emit one without them.
    #[test]
    fn test_finish_replay_ended_on_gap_blocks_amending_the_last_sealed_bar() {
        let reemitted = |ended_on_gap: bool| {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            let t0 = OPEN + 60;
            // The t0 + 120 minute is complete (the t0 + 60 one is partial:
            // the post-seed settling marks it) and the last sealed bar.
            for (ts, cum) in [
                (t0 + 5, 1_000),
                (t0 + 60, 1_010),
                (t0 + 70, 1_050),
                (t0 + 120, 1_100),
                (t0 + 130, 1_110),
                (t0 + 180, 1_150),
            ] {
                agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
            }
            agg.finish_replay(ended_on_gap, ignore_seal);
            let mut count = 0_u32;
            // A live late tick for the sealed t0 + 120 minute.
            agg.consume_tick(
                Feed::Dhan,
                &gtick(t0 + 170, 1_160),
                None,
                |_, _, _, tf, st| {
                    if tf == TfIndex::M1 && st.bucket_start_ist_secs == t0 + 120 {
                        count += 1;
                    }
                },
            );
            count
        };
        assert_eq!(
            reemitted(false),
            1,
            "control (review round 7): a clean replay leaves the bar amendable"
        );
        assert_eq!(
            reemitted(true),
            0,
            "ended on a gap: the last sealed bar is held"
        );
    }

    /// Review round 4 (2026-09-29): a STALE first packet after a gap that
    /// rolls a bucket opens it chained to the pre-gap end. The next tick lands
    /// in that bucket, but the skipped span began in the previous one, so the
    /// new bucket must not take it, and it is partial.
    #[test]
    fn test_mark_replay_gap_stale_packet_rolling_a_bucket_opens_it_partial() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let t0 = OPEN + 600;
        let mut m1: Vec<(u32, u64)> = Vec::new();
        let mut feed = |agg: &mut MultiTfAggregator, ts: u32, cum: u32| {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, |_, _, _, tf, st| {
                if tf == TfIndex::M1 {
                    m1.push((st.bucket_start_ist_secs, st.volume));
                }
            });
        };
        for (ts, cum) in [(t0 + 5, 990), (t0 + 10, 1_000)] {
            feed(&mut agg, ts, cum);
        }
        agg.mark_replay_gap(); // the skipped span traded up to 1,500
        feed(&mut agg, t0 + 62, 999); // stale, but it rolls the t0 minute
        feed(&mut agg, t0 + 65, 1_600); // the first counted tick after the gap
        feed(&mut agg, t0 + 125, 1_700); // rolls the t0 + 60 minute
        feed(&mut agg, t0 + 185, 1_750); // the t0 + 180 minute is complete
        feed(&mut agg, t0 + 245, 1_800);
        // Sealed bars are HELD during a replay; the hand-over releases them
        // (review round 7: without this the list stayed empty and both
        // assertions below passed vacuously).
        agg.finish_replay(false, |_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                m1.push((st.bucket_start_ist_secs, st.volume));
            }
        });
        assert!(
            m1.iter().any(|(start, _)| *start == t0 + 180),
            "control: the complete minute after settling is written: {m1:?}"
        );
        assert!(
            !m1.iter().any(|(start, _)| *start == t0 + 60),
            "the minute the stale packet opened is partial and not written: {m1:?}"
        );
        assert!(
            m1.iter().all(|(_, v)| *v < 500),
            "no minute takes the skipped span: {m1:?}"
        );
    }

    /// Plan ITEM 47, the core contract as a property (review round 5): fold a
    /// random trading stretch once in full (the live process) and once with
    /// random frames missing (a gapped WAL replay). Every bar the replay
    /// EMITS must equal the live bar for the same bucket in volume, open,
    /// high, low and close; anything it cannot see in full it must hold back.
    /// Stale packets and exchange session extremes are included, because both
    /// have their own paths across a gap.
    mod replay_contract_property {
        use super::*;
        use proptest::prelude::*;
        use std::collections::HashMap;

        type Bars = HashMap<(TfIndex, u32), LiveCandleState>;

        /// Receipt in UTC nanoseconds for an IST fold second.
        fn receipt_nanos(ist_secs: u32) -> i64 {
            (i64::from(ist_secs) - crate::candles::tf_index::IST_UTC_OFFSET_SECS) * 1_000_000_000
        }

        #[derive(Clone, Debug)]
        struct Step {
            dt: u32,
            dcum: i32,
            dpx: i8,
            dropped: bool,
        }

        fn fold(steps: &[Step], replay: bool) -> Bars {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            if replay {
                agg.set_replay_mode(true);
                agg.mark_replay_gap(); // a pass always starts after a gap
            }
            let mut bars: Bars = HashMap::new();
            let (mut ts, mut cum, mut px) = (OPEN + 60, 10_000_i64, 1_000.0_f32);
            let (mut hi, mut lo) = (px, px);
            let mut gap = false;
            for step in steps {
                ts += step.dt;
                cum = (cum + i64::from(step.dcum)).max(0);
                px = (px + f32::from(step.dpx) * 0.05).max(1.0);
                hi = hi.max(px);
                lo = lo.min(px);
                if replay && step.dropped {
                    gap = true;
                    continue;
                }
                if std::mem::replace(&mut gap, false) {
                    agg.mark_replay_gap();
                }
                let t = ParsedTick {
                    security_id: GAP_SID,
                    exchange_segment_code: SEG_EQ,
                    last_traded_price: px,
                    exchange_timestamp: ts,
                    volume: u32::try_from(cum).unwrap_or(u32::MAX),
                    day_high: hi,
                    day_low: lo,
                    received_at_nanos: receipt_nanos(ts),
                    ..ParsedTick::default()
                };
                agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st);
                });
            }
            if replay {
                // As in production: the replay hands over to the live feed,
                // and it ended on a gap if its last frames were skipped. Bars
                // still held are released here.
                let _ = agg.finish_replay(gap, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st);
                });
            }
            agg.catch_up_seal_all(ts + 100_000, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st);
            });
            bars
        }

        /// A restart (review round 11): `head` is replayed with its drops,
        /// the replay hands over to the live feed, and `tail` then arrives
        /// live. Returns the bars written and the fold second at which the
        /// hand-over's reach ends: the first live tick, which re-seeds the
        /// cumulative because a restart always has downtime.
        fn fold_restart(head: &[Step], tail: &[Step]) -> (Bars, u32) {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            agg.set_replay_mode(true);
            agg.mark_replay_gap();
            let mut bars: Bars = HashMap::new();
            let (mut ts, mut cum, mut px) = (OPEN + 60, 10_000_i64, 1_000.0_f32);
            let (mut hi, mut lo) = (px, px);
            let mut gap = false;
            let mut reach = ts;
            for (i, step) in head.iter().chain(tail).enumerate() {
                ts += step.dt;
                cum = (cum + i64::from(step.dcum)).max(0);
                px = (px + f32::from(step.dpx) * 0.05).max(1.0);
                hi = hi.max(px);
                lo = lo.min(px);
                let live = i >= head.len();
                if i == head.len() {
                    // The new process began listening at this tick.
                    agg.set_live_capture_start(ts);
                    let _ = agg.finish_replay(gap, |_, _, _, tf, st| {
                        bars.insert((tf, st.bucket_start_ist_secs), st);
                    });
                    // The hand-over is always a gap (a restart has downtime):
                    // the first live tick re-seeds, so its bucket is the
                    // last one the hand-over reaches.
                    reach = ts;
                    gap = false;
                }
                if !live && step.dropped {
                    gap = true;
                    continue;
                }
                if std::mem::replace(&mut gap, false) {
                    agg.mark_replay_gap();
                }
                let t = ParsedTick {
                    security_id: GAP_SID,
                    exchange_segment_code: SEG_EQ,
                    last_traded_price: px,
                    exchange_timestamp: ts,
                    volume: u32::try_from(cum).unwrap_or(u32::MAX),
                    day_high: hi,
                    day_low: lo,
                    received_at_nanos: receipt_nanos(ts),
                    ..ParsedTick::default()
                };
                agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, tf, st| {
                    bars.insert((tf, st.bucket_start_ist_secs), st);
                });
            }
            agg.catch_up_seal_all(ts + 100_000, |_, _, _, tf, st| {
                bars.insert((tf, st.bucket_start_ist_secs), st);
            });
            (bars, reach)
        }

        /// Steps whose cumulative may also go BACKWARDS (a stale re-send).
        fn stale_step() -> impl Strategy<Value = Step> {
            (
                1_u32..90,
                -60_i32..400,
                -4_i8..=4,
                prop::bool::weighted(0.25),
            )
                .prop_map(|(dt, dcum, dpx, dropped)| Step {
                    dt,
                    dcum,
                    dpx,
                    dropped,
                })
        }

        fn step() -> impl Strategy<Value = Step> {
            (1_u32..90, 0_i32..400, -4_i8..=4, prop::bool::weighted(0.25)).prop_map(
                |(dt, dcum, dpx, dropped)| Step {
                    dt,
                    dcum,
                    dpx,
                    dropped,
                },
            )
        }

        /// Two contracts interleaved, repeated quotes (same second, price and
        /// cumulative) and late trades (a trade time behind the stream).
        #[derive(Clone, Debug)]
        struct MixedStep {
            second: bool,
            dt: u32,
            late_by: u32,
            dcum: i32,
            dpx: i8,
            dropped: bool,
        }

        type MixedBars = HashMap<(u64, TfIndex, u32), LiveCandleState>;

        fn fold_mixed(steps: &[MixedStep], replay: bool) -> MixedBars {
            let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
            if replay {
                agg.set_replay_mode(true);
                agg.mark_replay_gap();
            }
            let mut bars: MixedBars = HashMap::new();
            let mut clock = OPEN + 60;
            // Per contract: cumulative, price, session high, session low.
            let mut state = [(10_000_i64, 1_000.0_f32, 1_000.0_f32, 1_000.0_f32); 2];
            let mut gap = false;
            for step in steps {
                clock += step.dt;
                let which = usize::from(step.second);
                let (cum, px, hi, lo) = &mut state[which];
                *cum = (*cum + i64::from(step.dcum)).max(0);
                *px = (*px + f32::from(step.dpx) * 0.05).max(1.0);
                *hi = hi.max(*px);
                *lo = lo.min(*px);
                if replay && step.dropped {
                    gap = true;
                    continue;
                }
                if std::mem::replace(&mut gap, false) {
                    agg.mark_replay_gap();
                }
                let sid = GAP_SID + u64::from(step.second);
                let t = ParsedTick {
                    security_id: sid,
                    exchange_segment_code: SEG_EQ,
                    last_traded_price: *px,
                    exchange_timestamp: clock.saturating_sub(step.late_by).max(OPEN),
                    volume: u32::try_from(*cum).unwrap_or(u32::MAX),
                    day_high: *hi,
                    day_low: *lo,
                    // Received now, whatever its trade time says.
                    received_at_nanos: receipt_nanos(clock),
                    ..ParsedTick::default()
                };
                agg.consume_tick(Feed::Dhan, &t, None, |_, id, _, tf, st| {
                    bars.insert((id, tf, st.bucket_start_ist_secs), st);
                });
            }
            if replay {
                let _ = agg.finish_replay(gap, |_, id, _, tf, st| {
                    bars.insert((id, tf, st.bucket_start_ist_secs), st);
                });
            }
            agg.catch_up_seal_all(clock + 100_000, |_, id, _, tf, st| {
                bars.insert((id, tf, st.bucket_start_ist_secs), st);
            });
            bars
        }

        fn mixed_step() -> impl Strategy<Value = MixedStep> {
            (
                any::<bool>(),
                0_u32..30,
                prop_oneof![8 => Just(0_u32), 1 => 1_u32..20, 1 => 250_u32..900],
                0_i32..300,
                -3_i8..=3,
                prop::bool::weighted(0.25),
            )
                .prop_map(|(second, dt, late_by, dcum, dpx, dropped)| MixedStep {
                    second,
                    dt,
                    late_by,
                    dcum,
                    dpx,
                    dropped,
                })
        }

        /// Regression pinned from the round-6 property run: a SKIPPED tick
        /// captured before a LATE tick with an older trade time. The replay
        /// then opened the skipped tick's minute from a later tick and took it
        /// for complete; the gap frontier marks it partial.
        #[test]
        fn test_regression_skipped_tick_before_a_late_tick_leaves_its_minute_partial() {
            let step = |dt, late_by, dpx, dropped| MixedStep {
                second: false,
                dt,
                late_by,
                dcum: 0,
                dpx,
                dropped,
            };
            let steps = vec![
                step(0, 0, 0, true),
                step(0, 13, 0, false),
                step(7, 3, 1, false),
                step(0, 0, 0, false),
                step(0, 0, 0, false),
            ];
            let live = fold_mixed(&steps, false);
            let replay = fold_mixed(&steps, true);
            for (key, bar) in &replay {
                let truth = live.get(key).expect("the live fold emitted this bar");
                assert_eq!(bar.open.to_bits(), truth.open.to_bits(), "{key:?} open");
            }
        }

        /// NON-VACUITY (review rounds 7 and 8): a replay that held back every
        /// bar would pass an equality check. For a stream with nothing
        /// dropped, every live bar starting after the pass-start settling
        /// (the first tick that adds volume with a known direction) and after
        /// the pass-start gap frontier (the first tick plus the clock-skew
        /// margin) must be rebuilt too.
        fn rebuilt_after_settling(
            steps: &[Step],
            live: &Bars,
            replay: &Bars,
        ) -> Result<(), TestCaseError> {
            prop_assert!(replay.len() <= live.len());
            let (mut ts, mut px) = (OPEN + 60, 1_000.0_f32);
            let (mut moved, mut settled_at) = (false, None);
            for (i, s) in steps.iter().enumerate() {
                ts += s.dt;
                let prev = px;
                px = (px + f32::from(s.dpx) * 0.05).max(1.0);
                if i == 0 {
                    continue; // the seeding tick
                }
                // The tick rule learns a direction only from a price change
                // on a tick that ADDS volume (`classify_tick_volume` returns
                // early on a zero delta without touching the carry).
                if s.dcum > 0 {
                    moved |= px.to_bits() != prev.to_bits();
                }
                if moved && s.dcum > 0 {
                    settled_at = Some(ts);
                    break;
                }
            }
            let first_ts = OPEN + 60 + steps.first().map_or(0, |s| s.dt);
            let frontier = first_ts + REPLAY_FRONTIER_SKEW_SECS;
            if let Some(end) = settled_at.map(|at: u32| at.max(frontier)) {
                for (tf, start) in live.keys() {
                    prop_assert!(
                        *start <= end || replay.contains_key(&(*tf, *start)),
                        "{:?} {}: complete after settling ({}), yet not rebuilt",
                        tf,
                        start,
                        end
                    );
                }
            }
            Ok(())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]
            #[test]
            fn test_replay_with_gaps_emits_only_bars_identical_to_the_live_fold(
                steps in prop::collection::vec(step(), 1..160)
            ) {
                let live = fold(&steps, false);
                let replay = fold(&steps, true);
                for ((tf, start), bar) in &replay {
                    let Some(truth) = live.get(&(*tf, *start)) else {
                        return Err(TestCaseError::fail(format!(
                            "{tf:?} {start}: the replay emitted a bar the live fold never did"
                        )));
                    };
                    prop_assert_eq!(bar.volume, truth.volume, "{:?} {} volume", tf, start);
                    prop_assert_eq!(bar.open.to_bits(), truth.open.to_bits(), "{:?} {} open", tf, start);
                    prop_assert_eq!(bar.high.to_bits(), truth.high.to_bits(), "{:?} {} high", tf, start);
                    prop_assert_eq!(bar.low.to_bits(), truth.low.to_bits(), "{:?} {} low", tf, start);
                    prop_assert_eq!(bar.close.to_bits(), truth.close.to_bits(), "{:?} {} close", tf, start);
                    prop_assert_eq!(bar.net_volume_signed, truth.net_volume_signed, "{:?} {} net", tf, start);
                    prop_assert_eq!(
                        bar.net_volume_classified,
                        truth.net_volume_classified,
                        "{:?} {} classified",
                        tf,
                        start
                    );
                }
                // With nothing dropped, the replay must reproduce the live fold
                // except the bars the pass-start gap makes partial.
                if steps.iter().all(|s| !s.dropped) {
                    rebuilt_after_settling(&steps, &live, &replay)?;
                }
            }

            /// Review round 8: the lower bound above runs only when no step
            /// was dropped, about 2% of that property's cases. Here nothing
            /// is ever dropped, so every case checks that a replay rebuilds
            /// every bar live wrote once the pass-start settling is over.
            #[test]
            fn test_replay_with_nothing_dropped_rebuilds_every_settled_bar(
                steps in prop::collection::vec(step(), 1..160)
            ) {
                let steps: Vec<Step> = steps
                    .into_iter()
                    .map(|s| Step { dropped: false, ..s })
                    .collect();
                let live = fold(&steps, false);
                let replay = fold(&steps, true);
                for (key, bar) in &replay {
                    let Some(truth) = live.get(key) else {
                        return Err(TestCaseError::fail(format!(
                            "{key:?}: the replay emitted a bar the live fold never did"
                        )));
                    };
                    prop_assert_eq!(bar.volume, truth.volume, "{:?} volume", key);
                    prop_assert_eq!(bar.close.to_bits(), truth.close.to_bits(), "{:?} close", key);
                }
                rebuilt_after_settling(&steps, &live, &replay)?;
            }

            /// Review round 11: every property above stops at the hand-over,
            /// which is how a taint that never cleared (and suppressed every
            /// later live bar) passed all of them. Here the replay hands over
            /// and live ticks keep arriving for up to a few hours: every
            /// bucket that opens after the hand-over's reach must be written,
            /// with the same volume and prices as a process that never
            /// restarted. (The buy/sell split is not compared: a restarted
            /// process may not know the tick-rule direction yet, and no stored
            /// bar exists for these buckets to disagree with.)
            #[test]
            fn test_live_bars_after_a_replay_hand_over_are_all_written(
                head in prop::collection::vec(step(), 1..120),
                tail in prop::collection::vec(step(), 1..160)
            ) {
                let all: Vec<Step> = head
                    .iter()
                    .chain(&tail)
                    .cloned()
                    .map(|s| Step { dropped: false, ..s })
                    .collect();
                let live = fold(&all, false);
                let (restarted, reach) = fold_restart(&head, &tail);
                for ((tf, start), truth) in &live {
                    if *start <= tf.bucket_start(reach) {
                        continue;
                    }
                    let Some(bar) = restarted.get(&(*tf, *start)) else {
                        return Err(TestCaseError::fail(format!(
                            "{tf:?} {start}: a live bucket after the hand-over (reach {reach}) \
                             was never written"
                        )));
                    };
                    prop_assert_eq!(bar.volume, truth.volume, "{:?} {} volume", tf, start);
                    prop_assert_eq!(bar.open.to_bits(), truth.open.to_bits(), "{:?} {} open", tf, start);
                    prop_assert_eq!(bar.high.to_bits(), truth.high.to_bits(), "{:?} {} high", tf, start);
                    prop_assert_eq!(bar.low.to_bits(), truth.low.to_bits(), "{:?} {} low", tf, start);
                    prop_assert_eq!(bar.close.to_bits(), truth.close.to_bits(), "{:?} {} close", tf, start);
                }
            }

            /// Round 6: the same contract with two contracts interleaved,
            /// repeated quotes and late trades.
            #[test]
            fn test_replay_with_gaps_mixed_stream_emits_only_live_identical_bars(
                steps in prop::collection::vec(mixed_step(), 1..200)
            ) {
                let live = fold_mixed(&steps, false);
                let replay = fold_mixed(&steps, true);
                for ((id, tf, start), bar) in &replay {
                    let Some(truth) = live.get(&(*id, *tf, *start)) else {
                        return Err(TestCaseError::fail(format!(
                            "{id} {tf:?} {start}: the replay emitted a bar the live fold never did"
                        )));
                    };
                    prop_assert_eq!(bar.volume, truth.volume, "{} {:?} {} volume", id, tf, start);
                    prop_assert_eq!(bar.open.to_bits(), truth.open.to_bits(), "{} {:?} {} open", id, tf, start);
                    prop_assert_eq!(bar.high.to_bits(), truth.high.to_bits(), "{} {:?} {} high", id, tf, start);
                    prop_assert_eq!(bar.low.to_bits(), truth.low.to_bits(), "{} {:?} {} low", id, tf, start);
                    prop_assert_eq!(bar.close.to_bits(), truth.close.to_bits(), "{} {:?} {} close", id, tf, start);
                    prop_assert_eq!(bar.net_volume_signed, truth.net_volume_signed, "{} {:?} {} net", id, tf, start);
                    prop_assert_eq!(
                        bar.net_volume_classified,
                        truth.net_volume_classified,
                        "{} {:?} {} classified",
                        id,
                        tf,
                        start
                    );
                }
            }

            /// The honest limit, bounded (review round 5): a packet stale
            /// against the SKIPPED span cannot be recognised by a replay that
            /// never saw that span. Its over-count on any emitted bar is at
            /// most the largest backward step of the cumulative in the
            /// stretch; it never grows beyond it.
            #[test]
            fn test_replay_with_stale_packets_overcounts_at_most_the_largest_regression(
                steps in prop::collection::vec(stale_step(), 1..160)
            ) {
                let live = fold(&steps, false);
                let replay = fold(&steps, true);
                let mut cum = 10_000_i64;
                let mut high_water = cum;
                let mut worst_regression = 0_i64;
                for step in &steps {
                    cum = (cum + i64::from(step.dcum)).max(0);
                    worst_regression = worst_regression.max(high_water - cum);
                    high_water = high_water.max(cum);
                }
                for ((tf, start), bar) in &replay {
                    let Some(truth) = live.get(&(*tf, *start)) else {
                        return Err(TestCaseError::fail(format!(
                            "{tf:?} {start}: the replay emitted a bar the live fold never did"
                        )));
                    };
                    let excess = i64::try_from(bar.volume).unwrap_or(i64::MAX)
                        - i64::try_from(truth.volume).unwrap_or(i64::MAX);
                    prop_assert!(
                        excess <= worst_regression,
                        "{:?} {}: over by {} with worst regression {}",
                        tf, start, excess, worst_regression
                    );
                }
            }
        }
    }

    /// A bucket opened after the re-seed saw every tick in it: still emitted.
    #[test]
    fn test_mark_replay_gap_complete_bucket_after_gap_is_still_emitted() {
        let (marked, _) = feed_gap_shape(true, true);
        assert!(
            marked
                .iter()
                .any(|(tf, start, v)| *tf == TfIndex::S1 && *start == OPEN + 307 && v.abs() == 10),
            "a 1-second bar past the first post-gap increase and the gap frontier's clock-skew margin is complete: {marked:?}"
        );
    }

    /// Outside a replay nothing is suppressed, and the re-seed still stops
    /// the skipped span's volume from landing in one bar.
    #[test]
    fn test_mark_replay_gap_live_mode_still_emits_partial_bars() {
        let (live, suppressed) = feed_gap_shape(false, true);
        assert_eq!(suppressed, 0, "live mode suppresses nothing");
        assert!(
            live.iter()
                .any(|(tf, start, _)| *tf == TfIndex::S1 && *start == OPEN + 300),
            "live mode still emits the bar the re-seeding tick opened"
        );
        assert!(
            live.iter().all(|(_, _, v)| v.abs() <= 110),
            "and it never carries the skipped span: {live:?}"
        );
    }

    /// Review round 7 (2026-09-29): with no replay, `finish_replay` touches
    /// nothing and releases nothing.
    #[test]
    fn test_finish_replay_without_a_replay_is_a_no_op() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        for (ts, cum) in [(OPEN + 5, 1_000), (OPEN + 65, 1_050)] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
        }
        let out = agg.finish_replay(true, |_, _, _, _, _| {
            panic!("no replay ran, so nothing is held and nothing is released")
        });
        assert_eq!(out, (0, 0));
    }

    /// Folds a complete minute at `OPEN + 180` in replay mode and seals it,
    /// so it is HELD (the minute before it is partial: the post-seed
    /// settling marks it). Returns the aggregator; `gap` marks a gap
    /// afterwards, which makes the held bar partial (skipped frames may have
    /// amended it).
    fn held_complete_minute(gap: bool) -> MultiTfAggregator {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        for (ts, cum) in [
            (OPEN + 65, 1_000),
            (OPEN + 120, 1_010),
            (OPEN + 130, 1_050),
            (OPEN + 180, 1_100),
            (OPEN + 190, 1_150),
            (OPEN + 240, 1_200),
        ] {
            // Shorter frames release their held bars as the next one seals;
            // the M1 bar at `OPEN + 180` stays held until a sweep.
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, |_, _, _, tf, st| {
                assert!(
                    !(tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 180),
                    "the M1 bar is held, not emitted, by the tick that seals it"
                );
            });
        }
        if gap {
            agg.mark_replay_gap();
        }
        agg
    }

    /// Counts the M1 bars for `OPEN + 180` a sweep hands to its callback.
    fn m1_at_180(count: &mut u32) -> impl FnMut(Feed, u64, u8, TfIndex, LiveCandleState) + '_ {
        move |_, _, _, tf, st| {
            if tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 180 {
                *count += 1;
            }
        }
    }

    /// Review round 7: the close seal releases a held complete bar (emitted
    /// once) and counts a held bar a gap made partial (never emitted).
    #[test]
    fn test_force_seal_all_releases_a_held_bar() {
        let mut complete = 0_u32;
        let mut agg = held_complete_minute(false);
        agg.force_seal_all(m1_at_180(&mut complete));
        assert_eq!(complete, 1, "the held complete minute is emitted once");
        let (_, suppressed_clean) = agg.finish_replay(false, ignore_seal);

        let mut partial = 0_u32;
        let mut agg = held_complete_minute(true);
        agg.force_seal_all(m1_at_180(&mut partial));
        assert_eq!(partial, 0, "after a gap the held minute is partial");
        let (_, suppressed_gap) = agg.finish_replay(false, ignore_seal);
        assert!(
            suppressed_gap > suppressed_clean,
            "and it is counted: {suppressed_gap} vs {suppressed_clean}"
        );
    }

    /// Review round 7: a catch-up sweep that would seal the bucket after a
    /// held bar releases the held bar first, emitted if complete and counted
    /// if a gap marked it; the bar it seals is then held in its turn.
    #[test]
    fn test_catch_up_seal_releases_a_held_bar_before_sealing_the_next() {
        let mut complete = 0_u32;
        let mut agg = held_complete_minute(false);
        agg.catch_up_seal_all(OPEN + 400, m1_at_180(&mut complete));
        assert_eq!(complete, 1, "the held complete minute is emitted once");
        let mut next = 0_u32;
        agg.finish_replay(false, |_, _, _, tf, st| {
            if tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 240 {
                next += 1;
            }
        });
        assert_eq!(
            next, 1,
            "the minute the sweep sealed was held until the hand-over"
        );

        let mut partial = 0_u32;
        let mut agg = held_complete_minute(true);
        agg.catch_up_seal_all(OPEN + 400, m1_at_180(&mut partial));
        assert_eq!(partial, 0, "after a gap the held minute is partial");
    }

    /// Review round 7 (hostile review, HIGH): live closes a quiet bucket by
    /// its periodic catch-up seal once the watermark is the catch-up margin
    /// past its end; a tick for that bucket arriving later amends the closed
    /// bar and carries its volume into the NEXT bucket. A replay runs no such
    /// sweep, so it folds the same tick into the still-open bucket. Both bars
    /// then differ from what live stored, and must not be written.
    /// `mode`: 0 = no late tick evidence (control), 1 = another instrument
    /// pushed the watermark, 2 = the late tick's own receipt shows it.
    fn late_tick_minutes(mode: u8) -> Vec<u32> {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let t0 = OPEN + 600;
        let mut m1: Vec<u32> = Vec::new();
        let feed = |agg: &mut MultiTfAggregator, t: ParsedTick, m1: &mut Vec<u32>| {
            agg.consume_tick(Feed::Dhan, &t, None, |_, sid, _, tf, st| {
                if sid == GAP_SID && tf == TfIndex::M1 {
                    m1.push(st.bucket_start_ist_secs);
                }
            });
        };
        for (ts, cum) in [
            (t0 + 5, 1_000),
            (t0 + 10, 1_010),
            (t0 + 65, 1_020),
            (t0 + 70, 1_030),
        ] {
            feed(&mut agg, gtick(ts, cum), &mut m1);
        }
        if mode == 1 {
            // t0 + 60 ends at t0 + 120; 240 s later live may have closed it.
            feed(
                &mut agg,
                tick(GAP_SID + 1, SEG_EQ, t0 + 420, 500.0, 5_000),
                &mut m1,
            );
        }
        let mut late = gtick(t0 + 80, 1_100);
        if mode == 2 {
            late.received_at_nanos = (i64::from(t0 + 420) - 19_800) * 1_000_000_000;
        }
        feed(&mut agg, late, &mut m1);
        for (ts, cum) in [(t0 + 130, 1_150), (t0 + 190, 1_200), (t0 + 250, 1_250)] {
            feed(&mut agg, gtick(ts, cum), &mut m1);
        }
        agg.finish_replay(false, |_, sid, _, tf, st| {
            if sid == GAP_SID && tf == TfIndex::M1 {
                m1.push(st.bucket_start_ist_secs);
            }
        });
        m1
    }

    #[test]
    fn test_regression_late_tick_after_a_live_catch_up_leaves_both_minutes_partial() {
        let t0 = OPEN + 600;
        let control = late_tick_minutes(0);
        assert!(
            control.contains(&(t0 + 60)) && control.contains(&(t0 + 120)),
            "control: with no sign that live closed the minute, both are written: {control:?}"
        );
        for mode in [1, 2] {
            let fixed = late_tick_minutes(mode);
            assert!(
                !fixed.contains(&(t0 + 60)) && !fixed.contains(&(t0 + 120)),
                "mode {mode}: the minute the late tick landed in and the next are \
                 partial: {fixed:?}"
            );
            assert!(
                fixed.contains(&(t0 + 180)),
                "mode {mode}: the minute after them is complete again: {fixed:?}"
            );
        }
    }

    /// Review round 8 (hostile review, HIGH): a REPEATED quote (same trade,
    /// new open interest) takes an early return before the fold. Arriving
    /// after live may have closed its bucket by catch-up, it refreshed the
    /// replay's still-open bucket with quote fields live never wrote there.
    /// `push` = another instrument moved the watermark far enough.
    fn repeat_after_catch_up_minutes(push: bool) -> Vec<u32> {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let t0 = OPEN + 600;
        let mut m1: Vec<u32> = Vec::new();
        let feed = |agg: &mut MultiTfAggregator, t: ParsedTick, m1: &mut Vec<u32>| {
            agg.consume_tick(Feed::Dhan, &t, None, |_, sid, _, tf, st| {
                if sid == GAP_SID && tf == TfIndex::M1 {
                    m1.push(st.bucket_start_ist_secs);
                }
            })
        };
        for (ts, cum) in [
            (t0 + 5, 1_000),
            (t0 + 10, 1_010),
            (t0 + 65, 1_020),
            (t0 + 70, 1_030),
        ] {
            feed(&mut agg, gtick(ts, cum), &mut m1);
        }
        if push {
            feed(
                &mut agg,
                tick(GAP_SID + 1, SEG_EQ, t0 + 420, 500.0, 5_000),
                &mut m1,
            );
        }
        let mut repeat = gtick(t0 + 70, 1_030);
        repeat.open_interest = 5_100;
        assert!(
            feed(&mut agg, repeat, &mut m1).repeat_quote,
            "the packet must take the repeat-quote path, or this tests nothing"
        );
        for (ts, cum) in [(t0 + 130, 1_100), (t0 + 190, 1_150)] {
            feed(&mut agg, gtick(ts, cum), &mut m1);
        }
        agg.finish_replay(false, |_, sid, _, tf, st| {
            if sid == GAP_SID && tf == TfIndex::M1 {
                m1.push(st.bucket_start_ist_secs);
            }
        });
        m1
    }

    #[test]
    fn test_regression_repeat_quote_after_a_live_catch_up_leaves_its_minute_partial() {
        let t0 = OPEN + 600;
        let control = repeat_after_catch_up_minutes(false);
        assert!(
            control.contains(&(t0 + 60)),
            "control: with no sign live closed it, the refreshed minute is written: {control:?}"
        );
        let fixed = repeat_after_catch_up_minutes(true);
        assert!(
            !fixed.contains(&(t0 + 60)),
            "the minute the repeat refreshed may differ from live's: {fixed:?}"
        );
        assert!(
            fixed.contains(&(t0 + 120)),
            "a repeat adds no volume, so the next minute stays complete: {fixed:?}"
        );
    }

    /// Review round 8: a late tick's mark on the NEXT bucket does not survive
    /// the hand-over, where it could only discard a live bar that no previous
    /// process ever stored.
    #[test]
    fn test_finish_replay_clears_the_next_bucket_mark() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let t0 = OPEN + 600;
        for (ts, cum) in [(t0 + 5, 1_000), (t0 + 10, 1_010), (t0 + 65, 1_020)] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
        }
        let other = tick(GAP_SID + 1, SEG_EQ, t0 + 420, 500.0, 5_000);
        agg.consume_tick(Feed::Dhan, &other, None, ignore_seal);
        agg.consume_tick(Feed::Dhan, &gtick(t0 + 80, 1_030), None, ignore_seal);
        assert!(agg.slots.iter().any(|s| s.replay_next_partial != 0));
        agg.finish_replay(false, ignore_seal);
        assert!(
            agg.slots.iter().all(|s| s.replay_next_partial == 0),
            "no next-bucket mark survives the hand-over"
        );
    }

    /// Review round 7: a tick OLDER than the open bucket, arriving once live
    /// may have closed that bucket by catch-up, amends the LAST SEALED bar
    /// in the replay, which live could not have done. The sealed bar, the
    /// open one and the one after it are all partial; a margin too large to
    /// reach (`set_catch_up_margin_secs`) turns the rule off.
    fn older_late_tick_minutes(push_watermark: bool, margin: Option<u32>) -> Vec<u32> {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        if let Some(secs) = margin {
            agg.set_catch_up_margin_secs(secs);
        }
        agg.set_replay_mode(true);
        let t0 = OPEN + 600;
        let mut m1 = Vec::new();
        // A held bar is released when the next bar of its timeframe seals,
        // so every callback is collected, not only the hand-over's.
        let feed = |agg: &mut MultiTfAggregator, t: ParsedTick, m1: &mut Vec<u32>| {
            agg.consume_tick(Feed::Dhan, &t, None, |_, sid, _, tf, st| {
                if sid == GAP_SID && tf == TfIndex::M1 {
                    m1.push(st.bucket_start_ist_secs);
                }
            });
        };
        for (ts, cum) in [
            (t0 + 5, 1_000),
            (t0 + 10, 1_010),
            (t0 + 65, 1_020),
            (t0 + 70, 1_030),
        ] {
            feed(&mut agg, gtick(ts, cum), &mut m1);
        }
        if push_watermark {
            feed(
                &mut agg,
                tick(GAP_SID + 1, SEG_EQ, t0 + 420, 500.0, 5_000),
                &mut m1,
            );
        }
        // Rolls the t0 + 60 minute and opens t0 + 120; then a trade for the
        // sealed t0 + 60 minute.
        for (ts, cum) in [
            (t0 + 130, 1_100),
            (t0 + 80, 1_110),
            (t0 + 190, 1_150),
            (t0 + 250, 1_200),
            (t0 + 310, 1_250),
        ] {
            feed(&mut agg, gtick(ts, cum), &mut m1);
        }
        agg.finish_replay(false, |_, sid, _, tf, st| {
            if sid == GAP_SID && tf == TfIndex::M1 {
                m1.push(st.bucket_start_ist_secs);
            }
        });
        m1
    }

    #[test]
    fn test_late_tick_older_than_the_open_bucket_marks_the_sealed_bar_too() {
        let t0 = OPEN + 600;
        for control in [
            older_late_tick_minutes(false, None),
            older_late_tick_minutes(true, Some(u32::MAX)),
        ] {
            assert!(
                [t0 + 60, t0 + 120, t0 + 180, t0 + 240]
                    .iter()
                    .all(|m| control.contains(m)),
                "control: nothing says live closed a bucket first: {control:?}"
            );
        }
        let fixed = older_late_tick_minutes(true, None);
        assert!(
            [t0 + 60, t0 + 120, t0 + 180]
                .iter()
                .all(|m| !fixed.contains(m)),
            "the amended sealed minute, the open one and the next are partial: {fixed:?}"
        );
        assert!(
            fixed.contains(&(t0 + 240)),
            "the minute after them is complete again: {fixed:?}"
        );
    }

    /// The default margin is the app's measured catch-up margin, and the
    /// setter replaces it.
    #[test]
    fn test_set_catch_up_margin_secs_replaces_the_default() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        assert_eq!(agg.catch_up_margin_secs, DEFAULT_CATCH_UP_MARGIN_SECS);
        agg.set_catch_up_margin_secs(600);
        assert_eq!(agg.catch_up_margin_secs, 600);
    }

    /// Review round 7 (hostile review, LOW): a counter restart that is the
    /// first tick after a gap resolves the gap without `gap_now` (it is also
    /// stale), and must still set the gap frontier.
    #[test]
    fn test_counter_restart_after_a_gap_sets_the_gap_frontier() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let high = u32::MAX - 10;
        // The seeding tick's own frontier expires an hour later.
        for (ts, cum) in [(OPEN + 5, high - 10), (OPEN + 3_700, high)] {
            agg.consume_tick(Feed::Dhan, &gtick(ts, cum), None, ignore_seal);
        }
        assert!(agg.slots.iter().all(|s| s.replay_gap_frontier == 0));
        agg.mark_replay_gap();
        agg.consume_tick(Feed::Dhan, &gtick(OPEN + 3_710, 5), None, ignore_seal);
        assert!(
            agg.slots
                .iter()
                .all(|s| s.replay_gap_frontier >= OPEN + 3_710),
            "the restart tick resolved the gap and set a frontier"
        );
    }

    /// Review round 7 (hot-path review): the gap frontier is replay-only. At
    /// the hand-over it is cleared, so live ticks never pay its pass and a
    /// live bucket is never marked partial by it.
    #[test]
    fn test_finish_replay_clears_the_gap_frontier() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        agg.consume_tick(Feed::Dhan, &gtick(OPEN + 5, 1_000), None, ignore_seal);
        agg.mark_replay_gap();
        let mut first = gtick(OPEN + 10, 1_100);
        // A receipt far in the future sets a frontier that would cover every
        // live bucket of the next hour if it survived the hand-over.
        first.received_at_nanos = (i64::from(OPEN + 3_000) - 19_800) * 1_000_000_000;
        agg.consume_tick(Feed::Dhan, &first, None, ignore_seal);
        assert!(agg.slots.iter().any(|s| s.replay_gap_frontier != 0));
        agg.finish_replay(false, ignore_seal);
        assert!(
            agg.slots.iter().all(|s| s.replay_gap_frontier == 0),
            "no frontier survives the hand-over"
        );
    }
    /// A late tick amends the last SEALED bar. If a replay could only partly
    /// see that bar, the amendment is suppressed too.
    #[test]
    fn test_set_replay_mode_suppresses_amendment_of_a_partial_bar() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        agg.set_replay_mode(true);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let _ = agg.consume_tick(Feed::Dhan, &gtick(OPEN, 1_000), None, sink);
        agg.mark_replay_gap();
        // Re-seeds and opens a partial minute; the next minute's tick seals it.
        let _ = agg.consume_tick(Feed::Dhan, &gtick(OPEN + 120, 5_000), None, sink);
        let _ = agg.consume_tick(Feed::Dhan, &gtick(OPEN + 185, 5_010), None, sink);
        // A late tick for the sealed partial minute.
        let mut emitted_m1_late = false;
        let late = agg.consume_tick(
            Feed::Dhan,
            &gtick(OPEN + 150, 5_010),
            None,
            |_, _, _, tf, st| {
                if tf == TfIndex::M1 && st.bucket_start_ist_secs == OPEN + 120 {
                    emitted_m1_late = true;
                }
            },
        );
        assert!(
            !emitted_m1_late,
            "the partial minute must not be re-emitted"
        );
        assert!(late.replay_partial_suppressed > 0, "and it is counted");
    }

    // -- bar_for_window (2026-09-18) ---------------------------------------
    //
    // These three own a guarantee that used to live one layer up, in
    // `top_volume_snapshot`: that a reading is for the row's OWN window or is
    // absent. It moved here because the decision moved here — the probe now
    // resolves the window by integer equality instead of handing back
    // whatever bucket happens to be open and letting a downstream column
    // report the discrepancy as a skew.

    /// A cell that has never seen a tick carries `bucket_start_ist_secs == 0`,
    /// the fold's documented never-opened sentinel. Asking it for a real
    /// window must answer `None` — storing that empty bar would claim a
    /// volume of 0 for a bar that does not exist.
    #[test]
    fn bar_for_window_refuses_a_never_opened_cell() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        // Tick instrument 77 so a slot EXISTS — the refusal under test is the
        // window mismatch, not a missing slot, and a test that could not tell
        // them apart would pass with the equality check deleted.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, OPEN, 100.0, 1_000),
            None,
            sink,
        );
        assert_eq!(
            agg.bar_for_window(Feed::Dhan, 77, SEG_IDX, TfIndex::S1, 0),
            None,
            "the never-opened sentinel is not a window any row can ask for"
        );
    }

    /// The whole point: a contract that has ROLLED — the ordinary state of a
    /// busy one — still answers for the window that just closed, out of the
    /// last-sealed bar. Before 2026-09-18 the probe returned the open bucket,
    /// so the busiest contracts were read one window late every time.
    #[test]
    fn bar_for_window_answers_from_the_last_sealed_bar_after_the_fold_rolls() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, OPEN, 100.0, 1_000),
            None,
            sink,
        );
        // A tick one second later rolls the 1-second bucket.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, OPEN + 1, 101.0, 1_100),
            None,
            sink,
        );

        let open = agg
            .snapshot(Feed::Dhan, 77, SEG_IDX, TfIndex::S1)
            .expect("slot exists");
        assert_eq!(
            open.bucket_start_ist_secs,
            OPEN + 1,
            "the fold has rolled — this is precisely the state that used to \
             hand a sweep the WRONG window"
        );

        let closed = agg
            .bar_for_window(Feed::Dhan, 77, SEG_IDX, TfIndex::S1, OPEN)
            .expect("the window that just closed is still reachable");
        assert_eq!(closed.bucket_start_ist_secs, OPEN);
    }

    /// A window the cell holds NEITHER open NOR last-sealed is refused, not
    /// approximated. This is the assertion that makes the other two mean
    /// something: without it a probe that returned any bar at all would pass
    /// them both.
    #[test]
    fn bar_for_window_refuses_a_window_the_cell_does_not_hold() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, OPEN, 100.0, 1_000),
            None,
            sink,
        );
        assert_eq!(
            agg.bar_for_window(Feed::Dhan, 77, SEG_IDX, TfIndex::S1, OPEN + 60),
            None,
            "a window the cell never held must be absent, never the nearest bar"
        );
    }

    /// `window_bar` hands back the same bar `bar_for_window` does, plus the
    /// two bucket starts a ranker needs to tell "did not trade in W" from
    /// "its W bar was already overwritten". Three states, one cell.
    #[test]
    fn test_window_bar_reports_the_bar_and_both_bucket_starts() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let sink = |_: Feed, _: u64, _: u8, _: TfIndex, _: LiveCandleState| {};
        assert_eq!(
            agg.window_bar(Feed::Dhan, 77, SEG_IDX, TfIndex::S1, OPEN),
            None,
            "no slot: the fold never saw this instrument"
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, OPEN, 100.0, 1_000),
            None,
            sink,
        );
        let only_open = agg
            .window_bar(Feed::Dhan, 77, SEG_IDX, TfIndex::S1, OPEN)
            .expect("slot exists");
        assert_eq!(only_open.open_bucket_ist_secs, OPEN);
        assert_eq!(
            only_open.last_sealed_bucket_ist_secs, 0,
            "nothing sealed yet"
        );
        assert_eq!(only_open.bar.map(|b| b.bucket_start_ist_secs), Some(OPEN));

        // Two more seconds: OPEN seals, OPEN+1 seals, OPEN+2 is open. The OPEN
        // bar has now been overwritten by the later seal.
        for (dt, cum) in [(1_u32, 1_100_u32), (2, 1_250)] {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, OPEN + dt, 101.0, cum),
                None,
                sink,
            );
        }
        let rolled = agg
            .window_bar(Feed::Dhan, 77, SEG_IDX, TfIndex::S1, OPEN)
            .expect("slot exists");
        assert_eq!(rolled.bar, None, "the OPEN bar left the fold");
        assert_eq!(rolled.open_bucket_ist_secs, OPEN + 2);
        assert_eq!(rolled.last_sealed_bucket_ist_secs, OPEN + 1);

        let sealed = agg
            .window_bar(Feed::Dhan, 77, SEG_IDX, TfIndex::S1, OPEN + 1)
            .expect("slot exists");
        assert_eq!(
            sealed.bar.map(|b| b.volume),
            Some(100),
            "the OPEN+1 bar is read from the last-sealed bucket, its own volume"
        );
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

        let mut frames_checked: Vec<TfIndex> = Vec::new();
        for tf in TfIndex::ALL {
            // Keep only frames whose buckets TILE this minute: the first starts
            // exactly on the minute and the last still starts inside it. That
            // admits every sub-minute frame and excludes D1, whose bucket began
            // at midnight and holds volume this minute never saw.
            if tf.bucket_start(base) != base || tf.bucket_start(base + 59) >= base + 60 {
                continue;
            }
            frames_checked.push(tf);
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

        // Pin the PROPERTY, not a count. A floor like `>= 10` was written when
        // the second-scale family alone was 19 frames; the 2026-09-19 collapse
        // took TF_COUNT to 9, so that floor became arithmetically impossible —
        // a guard that can only fail. Naming the frames the tiling filter MUST
        // admit is strictly stronger anyway, and it cannot go stale the next
        // time TF_COUNT moves: every sub-minute frame tiles a minute by
        // definition, and M1 is the minute itself.
        for tf in TfIndex::ALL {
            if !tf.is_second_scale() && tf != TfIndex::M1 {
                continue;
            }
            assert!(
                frames_checked.contains(&tf),
                "{tf:?} tiles this minute but the filter excluded it — the \
                 tiling filter is dropping frames it should admit, so this \
                 test is no longer checking what it claims (admitted: \
                 {frames_checked:?})"
            );
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
        let _ = agg.consume_tick(Feed::Dhan, &first, None, ignore_seal);
        let second = tick(13, SEG_IDX, OPEN + 3_610, 101.0, 1_000_500);
        let _ = agg.consume_tick(Feed::Dhan, &second, None, ignore_seal);
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
        let _ = agg.consume_tick(Feed::Dhan, &day1, None, ignore_seal);
        agg.force_seal_all(ignore_seal);

        // Next session: cumulative restarts small.
        let mut m1: Vec<u64> = Vec::new();
        for (off, cum) in [(0_u32, 100_u32), (10, 700)] {
            let t = tick(13, SEG_IDX, OPEN + 86_400 + off, 100.0, cum);
            let _ = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);
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
        assert_eq!(
            total(TfIndex::M60),
            expected,
            "the widest frame spans the same window"
        );
        assert_eq!(total(TfIndex::S1), total(TfIndex::M1));
        assert_eq!(total(TfIndex::M1), total(TfIndex::M60));
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
        assert_eq!(total(TfIndex::M60), expected);
        assert_eq!(total(TfIndex::S1), expected);
    }

    /// The carried SIGN travels with the carried units into the SAME bar, so
    /// the receiving bar counts one set of trades twice over — once gross,
    /// once net.
    ///
    /// This is the half a gross-only fix silently leaves broken. `volume` and
    /// `net_volume` are two readings of ONE set of trades, and
    /// `net_volume().abs() <= volume` is a structural fact only while both are
    /// fed from the same deltas. Settling the gross alone would hand the
    /// receiving bar units whose direction it never learned, while
    /// `net_volume_classified` still reported the bar fully classified — a
    /// confident answer over volume nobody signed.
    ///
    /// WHICH bar receives it is the part this test pins, and it changed on
    /// 2026-09-11. The carry is settled into the bar that is OPEN when the
    /// late tick arrives, at that bar's SEAL — not into the next bar to open.
    /// Right-endpoint chaining forces it: the following bucket must start
    /// exactly where this one ended, so units parked past that endpoint would
    /// fall in a gap. It is also the better answer on its own terms — the open
    /// bar is the one temporally nearer the refused tick.
    ///
    /// BITE PROOF: dropping `carry.net` from `UnattributedCarry::settle_into`
    /// leaves this bar at `Some(1000)` against a gross of 2,000 — half its
    /// flow missing, and nothing anywhere saying so.
    #[test]
    fn a_settled_carry_brings_its_sign_with_it_not_just_its_units() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut m1: std::collections::HashMap<u32, LiveCandleState> =
            std::collections::HashMap::new();

        // Every tick is an UPTICK, so every delta is buy-initiated and the
        // arithmetic stays readable: net must equal gross throughout.
        for (off, cum, px) in [
            (0_u32, 1_000_u32, 100.0_f32), // seeds the baseline (unclassified)
            (60, 2_000, 101.0),            // rolls 1m: opens bucket 60
            (10, 3_000, 102.0),            // LATE for 1m — carries +1,000
            (120, 4_000, 103.0),           // rolls 1m: SEALS bucket 60
        ] {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, OPEN + off, px, cum),
                None,
                |_, _, _, tf, st| {
                    if tf == TfIndex::M1 {
                        m1.insert(st.bucket_start_ist_secs, st);
                    }
                },
            );
        }

        let bar = m1
            .get(&(OPEN + 60))
            .copied()
            .expect("bucket 60 sealed when the 120 tick rolled it");
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
    /// ⚠ HONEST SCOPE, and it is narrower than it looks. On the live path the
    /// `unclassified` half of the carry is BELT-AND-BRACES rather than
    /// load-bearing, because of how `classify_tick_volume` reaches its `None`
    /// arm: real volume at an UNCHANGED price with no side ever revealed for
    /// this instrument. Once any side reveals itself the per-instrument sign
    /// carry latches, and every later flat tick classifies — so a carry can
    /// only be unclassified while EVERY bar so far is also unclassified,
    /// including the one that receives it. There is therefore no reachable
    /// sequence in which a fully-classified bar settles unsignable units.
    ///
    /// The flag is kept anyway, and this test with it, for two reasons: the
    /// `Ticker`-mode path (`consume_tick_with_prices`) passes `None` for every
    /// tick, and a future change to the classifier that widens its `None` arm
    /// would otherwise silently publish a net over volume it never signed.
    /// What this test pins UNCONDITIONALLY is the other half — that the units
    /// are counted.
    ///
    /// BITE PROOF: dropping the `owed > state.volume` widening from
    /// `UnattributedCarry::settle_into` leaves this bar at 1,000 of its 2,000
    /// units.
    #[test]
    fn a_bar_that_settles_unsignable_units_refuses_to_report_a_net() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut m1: std::collections::HashMap<u32, LiveCandleState> =
            std::collections::HashMap::new();

        for (off, cum, px) in [
            // Flat opening prints: no side ever reveals itself, so every
            // delta below is real volume with no readable direction.
            (0_u32, 1_000_u32, 100.0_f32), // first tick — unclassifiable
            (60, 2_000, 100.0),            // flat, carry still 0 — unclassifiable
            (10, 3_000, 100.0),            // LATE for 1m, and UNSIGNABLE
            (120, 4_000, 101.0),           // first real move: SEALS bucket 60
        ] {
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(77, SEG_IDX, OPEN + off, px, cum),
                None,
                |_, _, _, tf, st| {
                    if tf == TfIndex::M1 {
                        m1.insert(st.bucket_start_ist_secs, st);
                    }
                },
            );
        }

        let bar = m1
            .get(&(OPEN + 60))
            .copied()
            .expect("bucket 60 sealed when the 120 tick rolled it");
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
        let _ = agg.force_seal_all(ignore_seal);

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
            ignore_seal,
        );
        assert_eq!(agg.len(), 1);
        assert!(!agg.is_empty());
        // Same identity again — no new slot.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 1, 101.0, 2),
            None,
            ignore_seal,
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
            ignore_seal,
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
            ignore_seal,
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
            ignore_seal,
        );
        assert_eq!(agg.watermark_secs(), OPEN + 100);
        // An older (late) tick must not pull the watermark back.
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 10, 100.0, 2),
            None,
            ignore_seal,
        );
        assert_eq!(agg.watermark_secs(), OPEN + 100);
        // A post-close tick still advances it (so the last session bar can seal).
        let post_close = DAY + 56_400 + 5;
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, post_close, 100.0, 3),
            None,
            ignore_seal,
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
            ignore_seal,
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
        let stats = agg.consume_tick(Feed::Dhan, &honest, None, ignore_seal);
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
        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);
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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

        assert!(
            !stats.stale_trading_day,
            "a tick whose exchange day IS the receipt day must fold normally"
        );
        assert!(
            agg.lookup(Feed::Dhan, 66_422, SEG_IDX).is_some(),
            "and it must open its bucket"
        );
    }

    /// A tick stamped just before IST midnight and received just after it IS
    /// refused as stale — and that is the gate finally doing what its own
    /// comment has always claimed.
    ///
    /// ## ⚠ RE-BLESSED 2026-09-18, and this is a STRENGTHENING, not a loss
    ///
    /// This test used to assert the opposite, on the reasoning that the fold
    /// clock took the receipt inside the ±300 s trusted band so both sides of
    /// the comparison landed on the same day. That was an accurate
    /// description of the hybrid — and it meant the day gate was
    /// **structurally unable to fire** for any tick whose receipt sat within
    /// five minutes of its stamp, because it was comparing a receipt-derived
    /// day against the receipt day. A self-comparison.
    ///
    /// The gate's own comment states the rule it is meant to enforce, verbatim:
    /// *"Using it on both sides makes the rule symmetric and order-independent:
    /// the exchange day must BE the receipt day."* Under the 2026-09-18
    /// ts-bucketing directive `fold_secs` IS the exchange stamp, so that is now
    /// what the code compares. The rule and the implementation agree for the
    /// first time.
    ///
    /// ## The old premise was false, and it is worth naming
    ///
    /// The retired assertion justified itself with *"refusing it would drop
    /// real closing trades every session"*. It would not: NSE closes at 15:30
    /// IST, the candle window closes at 15:40, and the box is stopped by
    /// 17:30 — no session trade is anywhere near IST midnight, and any tick
    /// that were would already be refused `out_of_session` by the
    /// seconds-of-day gate below. The cost of this strengthening in production
    /// is therefore zero, and it is stated rather than assumed.
    ///
    /// ## ⚠ What it buys, corrected the same day — it is NARROWER than the
    /// ## first draft of this note claimed
    ///
    /// That draft said this strengthening buys the 2026-09-10 operator row —
    /// a connect-snapshot of a dormant contract carrying a LAST TRADE TIME
    /// from a previous session (measured mean 5 hours, max 34 days). **It
    /// does not, and the arithmetic says so plainly: a stamp 5 hours old sits
    /// 18,000 s from its receipt, far outside the retired ±300 s band, so the
    /// hybrid ALREADY fell back to the exchange stamp and this gate ALREADY
    /// fired on it.** That case was caught before this change and is caught
    /// after it; claiming it as a win would be crediting a fix for work the
    /// old code did.
    ///
    /// What it ACTUALLY buys is the one shape the band could hide: a stamp and
    /// a receipt WITHIN 300 s of each other that nonetheless STRADDLE IST
    /// midnight — precisely this test's fixture. Under the hybrid `fold_secs`
    /// was the receipt, so `fold_day == receipt_day` and the gate was
    /// arithmetically incapable of firing. Under the identity it fires.
    ///
    /// Narrow, and worth having anyway: a gate that cannot fire on its own
    /// fixture is the class this repository keeps having to correct, and the
    /// band is exactly where a stale stamp is hardest to tell from a fresh
    /// one by eye.
    #[test]
    fn a_tick_stamped_before_ist_midnight_and_received_after_it_is_stale() {
        let mut agg = MultiTfAggregator::default();

        let just_before_midnight = DAY - 1;
        let just_after_midnight_utc_secs =
            i64::from(DAY + 1) - crate::candles::tf_index::IST_UTC_OFFSET_SECS;

        let mut t = tick(66_422, SEG_IDX, just_before_midnight, 142.50, 12_000);
        t.received_at_nanos = just_after_midnight_utc_secs * 1_000_000_000;

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

        assert!(
            stats.stale_trading_day,
            "the exchange day must BE the receipt day — the gate's own stated \
             rule. Two seconds apart is irrelevant: they are different IST \
             DAYS, which is the only question this gate asks."
        );
        assert!(
            agg.lookup(Feed::Dhan, 66_422, SEG_IDX).is_none(),
            "and a refused tick must open no bucket"
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
            ignore_seal,
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
            ignore_seal,
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
        // And 60m never crossed a boundary at all.
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
            ignore_seal,
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
            ignore_seal,
        );
        let mut seals: Vec<SealRow> = Vec::new();
        let emitted = agg.force_seal_all(|f, s, g, tf, st| seals.push(row(f, s, g, tf, st)));
        assert_eq!(emitted, TF_COUNT, "one bar per opened timeframe");
        assert_eq!(seals.len(), TF_COUNT);
        // Idempotent: a second flush emits NOTHING (never a duplicate row).
        assert_eq!(agg.force_seal_all(ignore_seal), 0);
    }

    #[test]
    fn test_multi_tf_aggregator_catch_up_seal_all_closes_only_ended_buckets() {
        let mut agg = MultiTfAggregator::default();
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 5, 100.0, 1),
            None,
            ignore_seal,
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

    /// Builds an aggregator whose instruments tick at scattered times, so a
    /// catch-up cutoff seals a mix of frames per slot.
    fn scattered_book(instruments: u64, seed: u64) -> MultiTfAggregator {
        let mut agg = MultiTfAggregator::default();
        for sid in 1..=instruments {
            let mix = sid.wrapping_mul(2_654_435_761).wrapping_add(seed);
            let first = OPEN + (mix % 170) as u32;
            let _ = agg.consume_tick(
                Feed::Dhan,
                &tick(sid, SEG_EQ, first, 100.0 + (mix % 50) as f32, 10),
                None,
                ignore_seal,
            );
            if mix % 3 == 0 {
                let _ = agg.consume_tick(
                    Feed::Dhan,
                    &tick(sid, SEG_EQ, first + (mix % 7) as u32, 101.0, 25),
                    None,
                    ignore_seal,
                );
            }
        }
        agg
    }

    type Sealed = (Feed, u64, u8, TfIndex, LiveCandleState);

    /// Audit PR4b: the drain now runs the catch-up seal in slices. Whatever
    /// the slice size, the bars sealed must be exactly the bars one
    /// `catch_up_seal_all` call seals, in the same order, and the cursor
    /// must end at the slot count.
    #[test]
    fn catch_up_seal_in_slices_seals_the_same_bars() {
        let mut sealed_any = 0_usize;
        for (instruments, seed) in [(0_u64, 0_u64), (1, 7), (37, 3), (300, 11), (1_031, 5)] {
            for cutoff in [OPEN, OPEN + 30, OPEN + 61, OPEN + 200] {
                let mut whole = scattered_book(instruments, seed);
                let mut want: Vec<Sealed> = Vec::new();
                let n = whole.catch_up_seal_all(cutoff, |f, s, g, tf, st| {
                    want.push((f, s, g, tf, st));
                });
                assert_eq!(n, want.len());
                sealed_any += n;

                for step in [1_usize, 2, 7, 256, 5_000] {
                    let mut sliced = scattered_book(instruments, seed);
                    let mut got: Vec<Sealed> = Vec::new();
                    let mut cursor = 0_usize;
                    let mut total = 0_usize;
                    let mut calls = 0_usize;
                    loop {
                        let (emitted, next) =
                            sliced.catch_up_seal_slots(cutoff, cursor, step, |f, s, g, tf, st| {
                                got.push((f, s, g, tf, st));
                            });
                        total += emitted;
                        assert!(next >= cursor, "the cursor never moves backwards");
                        cursor = next;
                        calls += 1;
                        if cursor >= sliced.len() {
                            break;
                        }
                        assert!(calls <= instruments as usize + 1, "the slices must finish");
                    }
                    assert_eq!(
                        got, want,
                        "{instruments} slots, step {step}, cutoff {cutoff}: the sliced \
                         catch-up sealed different bars"
                    );
                    assert_eq!(total, n);
                    assert_eq!(cursor, sliced.len());
                }
            }
        }
        assert!(
            sealed_any > 1_000,
            "the fixture must seal real bars, sealed {sealed_any}"
        );
    }

    /// A cursor past the end (the book never shrinks, but a caller could hold
    /// a stale one) seals nothing and reports the slot count, never panics.
    #[test]
    fn catch_up_seal_slots_past_the_end_is_a_no_op() {
        let mut agg = scattered_book(5, 1);
        let (emitted, next) = agg.catch_up_seal_slots(OPEN + 200, 99, 10, ignore_seal);
        assert_eq!((emitted, next), (0, 5));
        let (emitted, next) = agg.catch_up_seal_slots(OPEN + 200, 0, 0, ignore_seal);
        assert_eq!((emitted, next), (0, 0), "a zero budget visits nothing");
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
            ignore_seal,
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(27, SEG_EQ, OPEN, 200.0, 20),
            None,
            ignore_seal,
        );
        let _ = agg.consume_tick(
            Feed::Truedata,
            &tick(27, SEG_IDX, OPEN, 300.0, 30),
            None,
            ignore_seal,
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
                ignore_seal,
            );
            assert!(stats.folded(), "tick {i} must fold");
            assert_eq!(stats.sealed_count, 0, "same second seals nothing");
            assert_eq!(stats.late_count, 0, "same second is never late");
        }
        // Even the finest frame (1s) keeps them all in ONE bucket.
        for tf in [TfIndex::S1, TfIndex::M1, TfIndex::M60] {
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
            ignore_seal,
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
                ignore_seal,
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
                ignore_seal,
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
            ignore_seal,
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
            ignore_seal,
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
            ignore_seal,
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
                ignore_seal,
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
            ignore_seal,
        );
        assert!(good.folded(), "a real price must still fold");
    }

    // -- repeated quote packets (2026-09-23) --------------------------------
    //
    // Dhan re-sends a Full/Quote packet whenever the BOOK or OI changes, with
    // the OLD last-trade time, price and cumulative volume. Before the filter
    // every copy counted as a trade. These pin both halves: a repeat changes
    // nothing but the quote fields, and a REAL trade is never mistaken for one.

    const REPEAT_SID: u64 = 4_242;
    const SEG_FNO: u8 = 2;

    fn quote(ts: u32, price: f32, cum: u32, oi: u32, received_at_nanos: i64) -> ParsedTick {
        let mut t = tick(REPEAT_SID, SEG_FNO, ts, price, cum);
        t.open_interest = oi;
        t.received_at_nanos = received_at_nanos;
        t
    }

    fn m1(agg: &MultiTfAggregator) -> LiveCandleState {
        agg.snapshot(Feed::Dhan, REPEAT_SID, SEG_FNO, TfIndex::M1)
            .expect("slot exists")
    }

    #[test]
    fn a_repeated_quote_is_not_counted_as_a_trade_but_refreshes_oi() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        // A UTC receipt one second after the trade: the day gate compares the
        // IST receipt day with the trade day, so the fixture must be same-day.
        let base_ns: i64 = i64::from(OPEN + 21 - 19_800) * 1_000_000_000;
        let first = agg.consume_tick(
            Feed::Dhan,
            &quote(OPEN + 20, 101.25, 500, 1_000, base_ns),
            None,
            ignore_seal,
        );
        assert!(first.folded() && !first.repeat_quote);
        let before = m1(&agg);

        let again = agg.consume_tick(
            Feed::Dhan,
            &quote(OPEN + 20, 101.25, 500, 1_200, base_ns + 30_000_000_000),
            None,
            ignore_seal,
        );
        assert!(
            again.repeat_quote,
            "same time, price and volume is a repeat"
        );
        assert_eq!(
            again.sealed_count + again.late_count,
            0,
            "a repeat seals and amends nothing"
        );

        let after = m1(&agg);
        assert_eq!(
            after.tick_count, 1,
            "tick_count must stay at the one real trade"
        );
        assert_eq!(after.oi, 1_200, "the quote field still updates");
        assert_eq!(
            after.last_receipt_ist_nanos, before.last_receipt_ist_nanos,
            "a repeat arriving 30 s later must not stretch the close latency"
        );
        assert_eq!(after.volume, before.volume);
    }

    #[test]
    fn a_hundred_repeats_after_the_seal_emit_no_amendment() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let t = quote(OPEN + 5, 250.5, 9_000, 10, 0);
        let _ = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);
        let sealed = agg.catch_up_seal_all(OPEN + 61, ignore_seal);
        assert!(sealed > 0, "the fixture must actually seal something");

        let mut emitted = 0_usize;
        for i in 0..100_u32 {
            let copy = quote(OPEN + 5, 250.5, 9_000, 10 + i, 0);
            let stats = agg.consume_tick(Feed::Dhan, &copy, None, |_, _, _, _, _| {
                emitted += 1;
            });
            assert!(stats.repeat_quote, "copy {i} must read as a repeat");
            assert_eq!(stats.late_count, 0, "copy {i} must not amend a sealed bar");
        }
        assert_eq!(emitted, 0, "no sealed bar may be re-emitted for a repeat");
    }

    #[test]
    fn a_real_trade_at_the_same_second_is_never_a_repeat() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let _ = agg.consume_tick(
            Feed::Dhan,
            &quote(OPEN + 30, 99.0, 700, 0, 0),
            None,
            ignore_seal,
        );
        // One more unit traded inside the same second.
        let more = agg.consume_tick(
            Feed::Dhan,
            &quote(OPEN + 30, 99.0, 701, 0, 0),
            None,
            ignore_seal,
        );
        assert!(!more.repeat_quote && more.folded());
        // Same volume, different price: an index trades with volume 0 always,
        // so its price is the only thing that tells two prints apart.
        let moved = agg.consume_tick(
            Feed::Dhan,
            &quote(OPEN + 30, 99.05, 701, 0, 0),
            None,
            ignore_seal,
        );
        assert!(!moved.repeat_quote && moved.folded());
        // Same price and volume, a later trade time: a new print.
        let later = agg.consume_tick(
            Feed::Dhan,
            &quote(OPEN + 31, 99.05, 701, 0, 0),
            None,
            ignore_seal,
        );
        assert!(!later.repeat_quote && later.folded());
        assert_eq!(m1(&agg).tick_count, 4);
    }

    #[test]
    fn an_index_print_with_zero_volume_and_a_new_price_still_counts() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        for price in [23_146.45_f32, 23_146.75, 23_146.45] {
            let s = agg.consume_tick(
                Feed::Dhan,
                &tick(13, SEG_IDX, OPEN + 40, price, 0),
                None,
                ignore_seal,
            );
            assert!(!s.repeat_quote, "price {price} moved, so it is a print");
        }
        let s = agg
            .snapshot(Feed::Dhan, 13, SEG_IDX, TfIndex::M1)
            .expect("slot exists");
        assert_eq!(s.tick_count, 3);
    }

    #[test]
    fn a_repeat_that_reports_a_new_session_high_is_still_folded() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        let mut first = quote(OPEN + 50, 100.0, 300, 0, 0);
        first.day_high = 101.0;
        first.day_low = 99.0;
        let _ = agg.consume_tick(Feed::Dhan, &first, None, ignore_seal);
        // Identical trade, but the exchange's running high moved: a print we
        // never received happened, and the fold is what attributes it.
        let mut copy = first;
        copy.day_high = 102.0;
        let s = agg.consume_tick(Feed::Dhan, &copy, None, ignore_seal);
        assert!(
            !s.repeat_quote,
            "a moved session extreme is evidence, not a repeat"
        );
        assert!(
            m1(&agg).high >= 102.0,
            "the unseen print must widen the bar"
        );
    }

    #[test]
    fn the_very_first_packet_is_never_a_repeat() {
        let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
        // Zero volume, zero price bits would collide with a zeroed baseline if
        // the seeded flag were ignored.
        let s = agg.consume_tick(
            Feed::Dhan,
            &quote(OPEN + 1, 50.0, 0, 0, 0),
            None,
            ignore_seal,
        );
        assert!(!s.repeat_quote && s.folded());
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
            receipt_day_mismatch: _,
            future_trading_day: _,
            untraded_timestamp: _,
            out_of_band_timestamp: _,
            // Informational, not a refusal — see the field doc.
            repeat_quote: _,
            // Not a refusal either: the tick WAS folded; only the output of a
            // bar a replay could partly see was held back (plan ITEM 47).
            replay_partial_suppressed: _,
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
            ignore_seal,
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
            ignore_seal,
        );
        let before = agg.watermark_secs();

        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, OPEN + 120, f32::NAN, 2),
            None,
            ignore_seal,
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
            ignore_seal,
        );
        let post_close = DAY + 56_500; // past the 15:40 session upper bound
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(13, SEG_IDX, post_close, 100.0, 2),
            None,
            ignore_seal,
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
            ignore_seal,
        );
        let _ = agg.consume_tick(
            Feed::Dhan,
            &tick(2, SEG_IDX, OPEN, 100.0, 1),
            None,
            ignore_seal,
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
            ignore_seal,
        );
        assert!(ok.folded());
        // A second refusal counts again but logs only once (latch).
        let stats2 = agg.consume_tick(
            Feed::Dhan,
            &tick(4, SEG_IDX, OPEN, 100.0, 1),
            None,
            ignore_seal,
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
            ignore_seal,
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
                ignore_seal,
            );
        }
        let slots = agg.len();
        // Cutoff far in the past: every cell is visited, none seals. This is
        // the pure traversal cost — the shape that runs on 99% of sweeps.
        let t0 = std::time::Instant::now();
        let emitted = agg.catch_up_seal_all(OPEN, ignore_seal);
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
                ignore_seal,
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
        assert_eq!(
            hostile_total(&bars, TfIndex::M60),
            expected,
            "sanity: the widest frame sweeps everything"
        );
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
        assert_eq!(
            hostile_total(&bars, TfIndex::M60),
            expected,
            "sanity: the widest frame swept the late tick in-bucket"
        );
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
        assert_eq!(
            hostile_total(&bars, TfIndex::M60),
            expected,
            "sanity: the widest frame's bucket never closed, so it swept the late \
             tick in-bucket and must show the full span"
        );
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

    /// FINDING C — the carried SIGN survives a counter restart even though
    /// `rebase_open_buckets` claims to drop the carry.
    ///
    /// The restart is detected AFTER the timeframe loop, so the wrapping tick
    /// has already rolled the bucket and `open_bucket` has already seeded the
    /// new bar's `net_volume_signed` with `carry.net`. `rebase_open_buckets`
    /// then clears `carried_*` and re-anchors `bucket_start_cumulative` — but
    /// it never touches `net_volume_signed`. The gross died with the counter;
    /// the sign did not.
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
            Some(200),
            "the bar's only classified flow was +200 (a buy); the published \
             net must not be negative"
        );
    }

    /// FINDING C3 — the OTHER `chain_broken` consumption site, and the one
    /// that fails SILENTLY for a whole bucket.
    ///
    /// The two restart tests above both restart on a tick that ROLLS, so the
    /// broken chain is consumed at the roll site. This one restarts while the
    /// frame has NO open bucket — the slot was drained by the watermark
    /// sealer — so the flag is consumed by
    /// `AggregatorCell::next_bucket_start_cumulative` instead.
    ///
    /// That path is the dangerous one. `rebase_open_buckets` re-anchors OPEN
    /// buckets onto the new axis, so a bar that was open across the restart
    /// chains correctly. `last_sealed` is NOT re-anchored — a bar sealed
    /// BEFORE the restart keeps a right endpoint on the erased axis. Chaining
    /// the next bucket to it computes `volume = cumulative − start` against a
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
            t5.volume, 700,
            "a bucket opened after a counter restart must anchor on the LIVE \
             cumulative, not on a right endpoint from the erased axis — \
             chaining across the restart reports 0 volume while {} ticks land \
             in the bar, with no error anywhere",
            t5.tick_count
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
            // Ground truth: D1's bucket spans the day, so it never refuses a
            // tick. Use the raw arithmetic so a D1 defect cannot hide one.
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
        let mut first: Option<(Vec<(u32, u32)>, u64, u64)> = None;
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
            let m60 = hostile_total(&bars, TfIndex::M60);
            let s1 = hostile_total(&bars, TfIndex::S1);
            if s1 != m60 {
                failures += 1;
                if first.is_none() {
                    first = Some((seq, s1, m60));
                }
            }
        }
        assert_eq!(failures, 0, "monotonic control failed: {first:?}");
    }

    /// FUZZ — every frame must tile the day to the SAME total, over many
    /// randomly reordered / occasionally-stale sequences. The oracle is D1,
    /// whose single bucket spans the day and therefore never refuses a tick.
    #[test]
    fn hostile_fuzz_every_frame_tiles_the_day_to_one_total() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut failures: Vec<(Vec<(u32, u32)>, u64, u64, u64)> = Vec::new();
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
            let m60 = hostile_total(&bars, TfIndex::M60);
            let s1 = hostile_total(&bars, TfIndex::S1);
            let m1 = hostile_total(&bars, TfIndex::M1);
            if s1 != m60 || m1 != m60 {
                failures.push((seq, s1, m1, m60));
            }
        }
        let over = failures.iter().filter(|(_, s1, _, m60)| s1 > m60).count();
        let under = failures.iter().filter(|(_, s1, _, m60)| s1 < m60).count();
        let m1_over = failures.iter().filter(|(_, _, m1, m60)| m1 > m60).count();
        let m1_under = failures.iter().filter(|(_, _, m1, m60)| m1 < m60).count();
        let stale = failures
            .iter()
            .filter(|(seq, _, _, _)| seq.windows(2).any(|w| w[1].1 < w[0].1))
            .count();
        println!(
            "HOSTILE FUZZ: {} failures / 4000. S1 over-reports (DOUBLE COUNT): {over}; \
             S1 under-reports (LOSS): {under}. M1 over: {m1_over}; M1 under: {m1_under}. \
             failures whose sequence contains a STALE (backwards) cumulative: {stale}",
            failures.len()
        );
        // Hoisted out of the `assert!` argument list. Inside it the chain is
        // evaluated only on failure, so it is dead text on every passing run —
        // and a six-line expression buried in a macro argument is the harder
        // thing to read either way. `take(3)` of an empty vec costs nothing.
        let first_three = failures
            .iter()
            .take(3)
            .map(|(seq, s1, m1, m60)| format!("  seq={seq:?}\n    S1={s1} M1={m1} M60={m60}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            failures.is_empty(),
            "{} of 4000 sequences failed conservation. First 3:\n{first_three}",
            failures.len()
        );
    }
}

// -- the EXCHANGE clock, driven END TO END through a real fold ---------------
//
// ADDED 2026-08-28 for the receipt clock; REPLACED 2026-09-18 for the exchange
// clock, per the operator's directive recorded in
// `websocket-connection-scope-lock.md`, "2026-09-18 (SECOND) — OHLCV AND
// VOLUME BUCKET ON THE EXCHANGE `ts`".
//
// The reason this module exists at all survives both directives intact, and it
// is the reason it was not simply deleted with the behaviour it pinned:
// `ParsedTick::default().received_at_nanos == 0`, and EVERY candle fixture in
// this workspace leaves it there. So a suite that never drives a NON-ZERO
// receipt through a real bucket proves nothing about which clock is in
// control — it passes "by construction, not by agreement". Every test below
// therefore still sets a real receipt, and now asserts that it changes
// NOTHING. That is a harder property to satisfy accidentally than the one it
// replaced.
//
// Three of the four tests below are UNCHANGED in expectation. That is not
// laziness — it is the measured shape of this change: the delta-bounded hybrid
// already fell back to the trade stamp for a stale snapshot, a replayed frame,
// a clock step and a UTC/IST mix-up. The ONLY population it moved was a live
// tick whose delivery lag crossed a bucket boundary, and that is the one test
// whose expectation flips.
#[cfg(test)]
mod exchange_clock_end_to_end_tests {
    use super::tests::{CANDLE_OPEN, SEG_IDX, tick};
    use super::*;

    /// UTC nanos for an IST second — the conversion the day gates invert.
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

    /// THE ONE BEHAVIOURAL CHANGE, pinned in the direction it actually moved.
    ///
    /// A trade printed at 09:29:59 and delivered at 09:30:01. Until
    /// 2026-09-18 the two seconds of delivery lag put this packet in the 09:30
    /// bar. It now files into 09:29 — the minute the exchange says the trade
    /// happened, and the minute the vendor's own 1-minute tape puts it in.
    ///
    /// Measured on production 2026-08-27 (NIFTY, full session): the exchange
    /// clock produced 385/385 session minutes and 382 bars matching the
    /// vendor's tape; the receipt clock produced 351/385 and 321. This test is
    /// the single packet where that difference is created.
    #[test]
    fn delivery_lag_across_a_minute_boundary_files_the_bar_by_the_trade_stamp() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        let traded = CANDLE_OPEN + 30 * 60 - 1; // 09:29:59
        let received = CANDLE_OPEN + 30 * 60 + 1; // 09:30:01
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(received);

        let _ = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

        assert_ne!(
            TfIndex::M1.bucket_start(received),
            TfIndex::M1.bucket_start(traded),
            "fixture must straddle a minute boundary or it proves nothing"
        );
        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(traded)),
            "a trade printed at 09:29:59 belongs to the 09:29 bar however late \
             it is delivered — operator directive 2026-09-18. A receipt-derived \
             bucket here is the change undone."
        );
    }

    /// A snapshot whose last trade was 100 minutes ago files into the bar its
    /// trade stamp names.
    ///
    /// UNCHANGED in expectation, and worth keeping for exactly that reason:
    /// the delta guard already produced this answer, so the test proves the
    /// simplification did not move the case that people most expect it to.
    /// (An earlier draft of this suite was written believing the receipt clock
    /// re-dated such a snapshot to now. It never did — the test written to
    /// demonstrate it failed instead, which is how the belief was caught.)
    #[test]
    fn a_stale_snapshot_still_buckets_on_its_trade_stamp() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        let traded = CANDLE_OPEN + 20 * 60;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(traded + 100 * 60);

        let _ = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(traded)),
            "a dormant contract's snapshot belongs to the bar of its last trade"
        );
    }

    /// A WAL frame re-stamped at replay: the receipt reads 9 hours after the
    /// trade. UNCHANGED in expectation, and now unconditional rather than
    /// rescued by a bound — which is the point. Measured on production
    /// 2026-08-27: 9.1% of a session's NIFTY ticks replayed 9–20 HOURS after
    /// their true arrival, and nine hours later is a perfectly SANE epoch.
    #[test]
    fn a_replayed_frame_buckets_on_the_trade_stamp() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        let traded = CANDLE_OPEN + 20 * 60;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(traded + 9 * 3_600);

        let _ = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(traded)),
            "replayed frames must land in the bars they originally belonged to"
        );
    }

    /// The close is owned by the LATEST-TRADED packet, not the last-received.
    ///
    /// Two packets inside one minute: the first traded at +20 s, the second
    /// traded at +5 s but arriving later. Under the receipt clock the late
    /// arrival owned the close and the bar closed at 107.0. Under the exchange
    /// clock the order guard refuses it as older and the bar closes at 100.0 —
    /// the price of the last TRADE in the minute, which is what a candle close
    /// means and what the vendor's own tape reports.
    ///
    /// High and low are unaffected: both packets still fold into the range.
    #[test]
    fn the_close_is_owned_by_the_latest_traded_packet() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);
        let minute = TfIndex::M1.bucket_start(CANDLE_OPEN + 30 * 60);

        let mut first = tick(13, SEG_IDX, minute + 20, 100.0, 10);
        first.received_at_nanos = receipt_nanos_for_ist(minute + 25);
        let mut second = tick(13, SEG_IDX, minute + 5, 107.0, 20);
        second.received_at_nanos = receipt_nanos_for_ist(minute + 45);

        let _ = agg.consume_tick(Feed::Dhan, &first, None, ignore_seal);
        let _ = agg.consume_tick(Feed::Dhan, &second, None, ignore_seal);

        let mut close = f64::NAN;
        let mut high = f64::NAN;
        agg.force_seal_all(|_, _, _, tf, st| {
            if tf == TfIndex::M1 {
                close = st.close;
                high = st.high;
            }
        });
        assert!(
            (close - 100.0).abs() < 1e-9,
            "the LATEST-TRADED packet owns the close (got {close}); the \
             out-of-order 107.0 traded EARLIER in the minute and must not \
             become its closing price"
        );
        assert!(
            (high - 107.0).abs() < 1e-9,
            "the out-of-order packet must still widen the RANGE (got {high}) — \
             only close OWNERSHIP is decided by ordering, never the extremes"
        );
    }

    /// THE REPAIR, and the largest thing this directive buys.
    ///
    /// The tick writer's session gate (`session_window::classify`) has always
    /// keyed on the exchange stamp alone. The fold keyed on the hybrid. So a
    /// print traded at 15:39:30 and delivered at 15:40:05 was WRITTEN to
    /// `ticks` and refused by every candle as `out_of_session` — one clock
    /// admitting what the other refused, recorded at length in
    /// `session_window.rs` as "deliberately NOT reconciled here".
    ///
    /// It is now structurally impossible: both paths read one clock. This test
    /// is that divergence, in the exact band the doc names.
    #[test]
    fn a_late_delivered_closing_print_is_now_folded_as_well_as_written() {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::REFOLD, 4);

        // 15:39:30 IST traded, 15:40:05 IST received — 35 s of delivery lag,
        // well inside this feed's measured p99 of 46 s.
        let day = (CANDLE_OPEN / 86_400) * 86_400;
        let traded = day + 15 * 3_600 + 39 * 60 + 30;
        let received = day + 15 * 3_600 + 40 * 60 + 5;
        let mut t = tick(13, SEG_IDX, traded, 100.0, 10);
        t.received_at_nanos = receipt_nanos_for_ist(received);

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

        assert!(
            received % 86_400 >= MARKET_CLOSE_SECS_OF_DAY_IST,
            "fixture must place the RECEIPT at or past the window close, or it \
             does not exercise the divergence"
        );
        assert!(
            stats.folded(),
            "an in-window trade must fold however late it is delivered; on the \
             receipt clock this same packet was refused `out_of_session` while \
             its row was written"
        );
        assert_eq!(
            seal_m1_bucket(&mut agg),
            Some(TfIndex::M1.bucket_start(traded)),
            "and it belongs to the 15:39 bar"
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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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
        let _ = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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
        let _ = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);
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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

    /// `receipt_day_mismatch` separates the two ways `stale_trading_day` is
    /// raised, and a WAL replay depends on the difference (2026-09-23).
    ///
    /// The morning replay on 2026-09-23 counted 4 ticks as LOST and paged
    /// `WS-SPILL-01`. All 4 were connect snapshots whose trade date was the
    /// previous session while the receipt was today — the receipt-clock arm,
    /// the operator's 2026-09-10 rule, the same verdict the live feed reaches.
    /// A tick received on the day it traded and replayed the NEXT morning is
    /// different: it is refused by the watermark, it is real captured data, and
    /// it must still read as loss. One flag, three arms, all pinned here.
    #[test]
    fn receipt_day_mismatch_is_set_by_the_receipt_arms_and_never_by_the_watermark() {
        // Receipt-clock STALE: traded yesterday, received today.
        let mut agg = MultiTfAggregator::default();
        let yesterday_1529 = DAY - 86_400 + 15 * 3_600 + 29 * 60;
        let mut stale = tick(66_422, SEG_IDX, yesterday_1529, 142.50, 12_000);
        stale.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));
        let stats = agg.consume_tick(Feed::Dhan, &stale, None, ignore_seal);
        assert!(stats.stale_trading_day && stats.receipt_day_mismatch);

        // Receipt-clock FUTURE: stamped tomorrow, received today.
        let mut agg = MultiTfAggregator::default();
        let mut future = tick(13, SEG_IDX, TODAY_0916 + 86_400, 100.0, 1);
        future.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));
        let stats = agg.consume_tick(Feed::Dhan, &future, None, ignore_seal);
        assert!(stats.future_trading_day && stats.receipt_day_mismatch);

        // WATERMARK stale: a fresh tick today moves the watermark; then a tick
        // that traded AND was received yesterday arrives (a replayed frame).
        // Its receipt matches its trade day, so the receipt arm passes it and
        // the watermark refuses it — and this one is real data.
        let mut agg = MultiTfAggregator::default();
        let mut today = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
        today.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));
        let first = agg.consume_tick(Feed::Dhan, &today, None, ignore_seal);
        assert!(
            first.folded(),
            "fixture: today's tick must fold and set the watermark"
        );
        let mut replayed = tick(13, SEG_IDX, yesterday_1529, 99.0, 1);
        replayed.received_at_nanos = receipt_at_ist(i64::from(yesterday_1529));
        let stats = agg.consume_tick(Feed::Dhan, &replayed, None, ignore_seal);
        assert!(
            stats.stale_trading_day,
            "fixture: the watermark arm must be what refuses this tick"
        );
        assert!(
            !stats.receipt_day_mismatch,
            "a tick received on the day it traded is NOT a receipt-day mismatch — \
             marking it would let a replay report real lost data as by-design"
        );
    }

    // -- day boundaries -----------------------------------------------------

    /// One SECOND apart across IST midnight is a genuine day mismatch and is
    /// refused.
    ///
    /// ## ⚠ Re-stated 2026-09-18 — the old safety argument cited a mechanism
    /// ## and a test that no longer exist
    ///
    /// This used to read: *"It is safe because the trusted band already
    /// collapses a real straddle onto one clock (pinned by
    /// `a_tick_straddling_ist_midnight_inside_the_trusted_band_is_not_stale`)."*
    /// Both halves are gone. The ts-bucketing directive deleted the trusted
    /// band, and the test it named now asserts the OPPOSITE under the name
    /// `a_tick_stamped_before_ist_midnight_and_received_after_it_is_stale` —
    /// because with one clock the gate finally compares the EXCHANGE day
    /// against the receipt day, which is the rule its own comment always
    /// claimed to enforce.
    ///
    /// The sharp edge is therefore real and unmitigated: the comparison is on
    /// DAYS, so a single second flips it, and nothing collapses a straddle any
    /// more. **What makes it safe is the session window, not the fold clock**:
    /// the candle session is `[09:00, 15:40)` IST and the box is stopped by
    /// 17:30, so nothing legitimate trades within a second of IST midnight,
    /// and anything that did would already be refused `out_of_session` by the
    /// seconds-of-day gate. That is a weaker guarantee than the one this
    /// comment used to claim, and it is stated plainly rather than implied.
    #[test]
    fn an_exchange_stamp_one_second_before_midnight_is_stale_against_the_next_day() {
        let mut agg = MultiTfAggregator::default();
        // Receipt a full working day later — the mismatch is unambiguous and
        // does not depend on any band, which no longer exists.
        let mut t = tick(13, SEG_IDX, DAY - 1, 100.0, 1);
        t.received_at_nanos = receipt_at_ist(i64::from(DAY + 33_400));

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

        let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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

                let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

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
