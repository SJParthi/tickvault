//! Runtime-indexable timeframe handle for the live candle engine.
//!
//! The hot-path multi-TF aggregator needs an O(1) ordinal-indexable
//! mapping from `(timeframe → QuestDB table name + DEDUP key +
//! bucket-seconds + display name)`. [`TfIndex`] is that handle: a
//! `#[repr(u8)]` enum whose ordinal indexes the per-instrument
//! `[Mutex<LiveCandleState>; TF_COUNT]` slot array AND the
//! storage-side `[Sender; TF_COUNT]` ILP writer array.
//!
//! ## The 21 timeframes (1s–15s, 30s, 1m, 3m, 5m, 15m, 1d)
//!
//! The candle re-architecture (#T1) ships ONE aggregator that derives
//! all 21 timeframes directly from the live tick stream and flushes
//! each sealed bar straight to its own plain QuestDB table
//! (`candles_1s` … `candles_1d`; legacy frames write-stopped). There
//! are no `_shadow` tables and no materialized-view cascade — every
//! timeframe is a first-class table written at seal time.
//!
//! Variant ordinals are FROZEN (1m…1d at 0..=4; 1s–15s + 30s
//! appended at 5..=20) so the ordinal
//! returned by [`Self::as_ordinal`] is stable. Reordering variants is
//! a SEMVER break — every consumer indexing by ordinal (the
//! per-instrument `[Mutex<LiveCandleState>; TF_COUNT]`, the ILP
//! `[Sender; TF_COUNT]` writer, the audit-table `timeframe` SYMBOL column)
//! breaks silently.

/// Number of timeframes the live candle engine derives. Pinned here so
/// the per-instrument slot array and the storage-side sender array
/// share one source of truth. 21 since C3 (operator frame directive
/// 2026-07-21): 1s..15s + 30s + 1m/3m/5m/15m + broker 1d — the 16
/// second-scale frames are STRUCTURAL ONLY (GDF-feed-gated, zero rows
/// until the GDF 1s live feed lands in its own lane).
///
/// 24 since 2026-08-10: M2/M30/M60 appended (ordinals 21/22/23) to
/// complete the thirteen current-day frames of operator Quote 13
/// (2026-08-08). Three of those thirteen previously had no enum variant
/// at all, so they could not be derived, stored, or queried. The append
/// is ordinal-stable — every pre-existing ordinal 0..=20 is unchanged,
/// so `SEAL_SPILL_FORMAT_VERSION` stays 1 and previously-spilled
/// segments still replay.
///
/// **CORRECTED 2026-08-19 — the "STRUCTURAL ONLY (GDF-feed-gated, zero
/// rows)" sentence above is STALE and has been since 2026-08-11.** It was
/// true when written: the second-scale frames were added while no live
/// tick feed existed, and GDF was the expected 1-second producer. Then the
/// Dhan live main-feed WS was revived (scope-lock 2026-08-09) and its
/// default flipped ON (2026-08-11), and `MultiTfAggregator::consume_tick`
/// folds every tick into `TfIndex::ALL` with NO feed gate anywhere — not in
/// the fold, not in the seal sink, not in the ILP writer. So all 16
/// second-scale frames produce real rows on the live Dhan lane today.
///
/// That matters beyond bookkeeping: the sentence understates the fold's
/// real cost by 16 of 24 frames, and this repository has twice recorded a
/// stale doc manufacturing a false finding (see the O(1) table in
/// CLAUDE.md). The claim is retained above rather than deleted, per house
/// convention, so the correction is auditable.
///
/// ⚠ **BOTH blocks above are HISTORY: the set is NINE, not 24, since
/// 2026-09-19.** Every second-scale frame they argue about except `S1`,
/// `S3` and `S5` is gone, along with `D1` and `M2`, on the operator's
/// directive (`websocket-connection-scope-lock.md`, "THE FOLD COLLAPSES TO
/// NINE FRAMES"): *"dude these shodu lnot even be considered or derived or
/// calcualted anywhere dude okay?Not written anywhere 2s 4s 6s 10s 15s 30s
/// 2m 1d"*. The surviving nine are `M1 M3 M5 M15 S1 S3 S5 M30 M60`; `10m`
/// stays a VIEW over `candles_1m` rather than a tenth fold frame.
///
/// **Ordinals RENUMBERED with that change**, which is why
/// `SEAL_SPILL_FORMAT_VERSION` moved 3 → 4 in the same commit: the spill
/// record stores the frame as a raw ordinal byte, `S1` was 5 and is now 4,
/// and a byte in range decodes silently into the wrong frame rather than
/// failing. A record older than 4 is REFUSED rather than reinterpreted.
pub const TF_COUNT: usize = 9;

/// 09:15:00 IST expressed as seconds-of-day (`9*3600 + 15*60`).
/// The NSE regular trading session opens at 09:15:00 — every candle
/// bucket grid is anchored here so the first candle of every timeframe
/// starts exactly at the market open.
///
/// **STILL LIVE after the 2026-08-28 grid move**, and the reason matters:
/// the candle GRID now anchors at 09:00, but the question "which bar owns the
/// exchange's official day open/high/low?" is still answered by the MARKET
/// open. See `is_days_first_session_bucket` in `aggregator_cell.rs`.
pub(crate) const MARKET_OPEN_SECS_OF_DAY_IST: u32 = 33_300;

/// 09:00:00 IST expressed as seconds-of-day (`9*3600`).
///
/// **THE CANDLE GRID ANCHOR — deliberately NOT the market open.**
///
/// Added 2026-08-28 under the dated operator authorization in
/// `websocket-connection-scope-lock.md` ("CANDLES FROM 09:00"). The NSE
/// pre-open call auction runs 09:00-09:12 (order collection, then matching),
/// and its equilibrium price IS the 09:15 open — the exchange does not
/// discover a second one. Anchoring the candle grid at 09:15 therefore threw
/// away the twelve minutes in which that price is actually formed, which is
/// the one window where knowing the open EARLY has value: it is what makes an
/// ATM +/-25 option window computable at 09:13 instead of guessed from
/// yesterday's close.
///
/// **This is NOT `MARKET_OPEN_SECS_OF_DAY_IST` and must never be conflated
/// with it.** That constant means "the exchange is open for trading" and
/// still gates orders, risk, the day-OHLC tracker and every market-hours
/// alarm window at 09:15. This one means "we are recording candles". The two
/// answer different questions and the pin test below asserts BOTH values
/// independently, plus the ordering between them, so neither can silently
/// absorb the other.
///
/// Grid consequence, stated because it is a real behavioural change and not
/// a no-op. The offset between the two anchors is 900 s, so a frame's grid
/// is UNCHANGED exactly when `900 % seconds_per_bucket == 0`. **NINE of the
/// 24 frames move, not three** — an earlier draft of this comment said
/// "2m/30m/60m" and was wrong by omission, which is worth recording because
/// the omitted one is the loudest:
///
/// | moves | why |
/// |---|---|
/// | **D1 (86_400 s)** | `bucket_start` returns `session_open` for the daily bar, so **every daily candle's `ts` moves 09:15 -> 09:00** |
/// | 2m (120), 30m (1800), 60m (3600) | 900 is not a multiple of any of them |
/// | 7s, 8s, 11s, 13s, 14s | live — see the correction below |
///
/// **CORRECTED before this shipped:** the row above first read "GDF-gated,
/// zero rows today — latent, not live". That is true of the `spot_bar_store`
/// RAM rings, which allocate capacity-1 placeholders for second-scale frames,
/// and it is FALSE of the candle tables. `AggregatorCell` folds `TfIndex::ALL`
/// with no `is_second_scale` filter, so all 24 frames write real
/// `candles_<tf>` rows on the live Dhan lane. Those five grids therefore move
/// for real, not latently. The distinction matters because "latent" is how a
/// reader decides not to check something.
///
/// Unchanged: 1s, 2s, 3s, 5s, 6s, 9s, 10s, 12s, 15s, 20s, 30s, 1m, 3m, 5m,
/// 15m (900 divides all of them).
///
/// Clock-aligned is the more conventional answer and is what every external
/// chart shows, but it IS a change, and it is why this must land BEFORE a
/// session opens rather than during one.
///
/// **THREE CONSEQUENCES BEYOND THE GRID, found by an adversarial boundary
/// sweep and disclosed rather than discovered later.**
///
/// 1. **D1/M30/M60 high and low now include pre-open auction prints.** Those
///    buckets SPAN 09:00-09:15, so the fold widens their range with auction
///    LTPs, which can sit outside the exchange's regular-session day high/low.
///    The `day_ohlc_tracker` stays 09:15-gated (2026-08-25 scope lock), so the
///    D1 candle and the day-OHLC tracker CAN now legitimately disagree on high
///    and low. Both are correct for their own question; anything comparing
///    them must not treat the difference as an error.
/// 2. **Pre-open bars persist `open_pct = 0.0`.** `session_open` is the
///    exchange's `day_open`, which is 0 during the auction because no trade
///    has printed, and `pct_change` correctly refuses a non-positive baseline.
///    So ~15 bars per instrument per day carry a zero that means "no session
///    open exists yet" but is indistinguishable from "flat". Semantically
///    right, and worth knowing before reading a pre-open chart.
/// 3. **A counter changes shape.** Roughly 400,000 ticks per session move
///    from `refused(out_of_session)` to `folded`. Any baseline or alarm
///    threshold derived from that counter's historical value is now measuring
///    a different population.
///
/// A fourth, and it is a PRE-EXISTING hole this change WIDENS rather than
/// creates. `WsFrameSpill::replay_all` is not day-scoped, and at replay time
/// the aggregator's watermark is 0 — so the stale-trading-day gate
/// (`exchange_timestamp / 86_400 < watermark / 86_400`) always passes on the
/// first replayed frame. A stale frame therefore reaches the SESSION gate as
/// its only remaining defence. Previously a stale frame stamped in
/// [09:00, 09:15) was refused there; now it folds and opens a bucket dated on
/// a closed day. The exposure grows from 22,500 s of the day to 23,400 s —
/// about 4% wider, on a hole that already existed for the other 22,500. Not
/// introduced here, not fixed here, and recorded so it is not rediscovered as
/// new: the real fix is day-scoping the replay or seeding the watermark from
/// the staged frames, both outside this change.
///
/// One more, recorded because it is correct only by arithmetic coincidence:
/// `tf_consistency_boot::is_on_grid` re-anchors to 09:00 and therefore
/// re-classifies rows written on the OLD grid. It survives because that
/// verifier covers M3/M5/M15 only, and 900 divides all three. Adding any
/// frame where `900 % S != 0` to it — M2, M30, M60 — would make every
/// historical row report `OffGridTs`.
///
/// **The mid-session hazard, precisely.** The `candles_<tf>` DEDUP key is
/// `(ts, security_id, segment, feed)` — it carries no grid identity. So a
/// restart onto the new grid mid-session does NOT collide and does NOT gap:
/// it leaves BOTH rows. A 10:20 restart on 30m keeps the old complete
/// `ts=09:45` bar and adds a new `ts=10:00` bar holding only 10:20-10:30.
/// Nothing detects this — `tf_consistency` recomputes M3/M5/M15, which are
/// exactly the frames whose grids did NOT move. That is a process control,
/// not a mechanism, and it is stated plainly rather than implied: deploy this
/// between sessions.
pub(crate) const CANDLE_SESSION_OPEN_SECS_OF_DAY_IST: u32 = 32_400;

/// IST is UTC+05:30. A receipt instant is UTC; `exchange_timestamp` is
/// already IST (never add the offset to it — see `data-integrity.md`).
///
/// Retained after the 2026-09-18 ts-bucketing directive because the two
/// clock-INDEPENDENT day gates in `multi_tf_aggregator` still compare the
/// fold day against the RECEIPT day, and that comparison needs the offset.
pub(crate) const IST_UTC_OFFSET_SECS: i64 = 19_800;

/// The clock the candle grid buckets on: **the exchange stamp, and nothing
/// else**.
///
/// # The directive
///
/// Operator, 2026-09-18: *"as of now to set the rpecise ohlcv we used the
/// recived at right dude but now we have a catch bro which is see we need to
/// use this ts dude nowhere hereafetr we hsodu luse received at to define our
/// ohlcv dude okay? our only apporach si to use this ts to set our ohlcv
/// everyhwere dude okay even volume also ddue okay?"* — recorded with its
/// full contract in `websocket-connection-scope-lock.md`, section
/// "2026-09-18 (SECOND) — OHLCV AND VOLUME BUCKET ON THE EXCHANGE `ts`".
///
/// This REVERSES the 2026-08-28 receipt-clock directive, which the same rule
/// file records as itself a reversal of a measurement. The measurement is
/// what decided it, and it is worth restating because it argues FOR this
/// change rather than merely permitting it — production, 2026-08-27, NIFTY:
///
/// | | exchange clock (`ts`) | receipt clock |
/// |---|---|---|
/// | session minutes present | **385 / 385** | 351 / 385 |
/// | bars matching the vendor's own tape | **382 (99.2%)** | 321 (83.4%) |
/// | phantom bars outside market hours | **0** | 4 |
/// | ticks filed on the WRONG DAY | **0** | 4,319 |
/// | ticks that would change MINUTE on the LIVE path | — | **0 of 83,871** |
///
/// # What this function no longer does, stated exactly
///
/// Until 2026-09-18 this was a DELTA-BOUNDED HYBRID: it preferred the receipt
/// when the receipt sat within `[-10 s, +300 s]` of the trade stamp, and fell
/// back to the trade stamp outside that band. So a dormant snapshot, a
/// replayed WAL frame, a clock step and a UTC/IST mix-up ALREADY bucketed on
/// `ts`; the only population the hybrid moved was a live tick whose delivery
/// lag crossed a bucket boundary.
///
/// **That correction is what this directive deletes, and nothing else.** The
/// last row of the table above is the measured size of it on the live path:
/// zero of 83,871 ticks changed minute, because Dhan stamps whole seconds and
/// we receive inside the same second. The population it genuinely moved is a
/// trade printed at 09:29:59 and delivered at 09:30:01, which the hybrid filed
/// into 09:30 and this files into 09:29 — the bar the exchange says it belongs
/// to, and the bar the vendor's own tape puts it in.
///
/// # What this REPAIRS, which is larger than what it costs
///
/// The tick writer's session gate (`session_window::classify`) has always
/// keyed on the exchange stamp alone. The fold keyed on the hybrid. So for a
/// band of prints — an exchange stamp inside the window with a receipt past
/// it, and its converse — a tick was written to `ticks` and absent from every
/// candle, or folded into a candle while the row was refused. That divergence
/// is recorded at length in `session_window.rs` and is now **structurally
/// impossible**: both paths read one clock.
///
/// # What still reads the RECEIPT, deliberately
///
/// - `ws_lag_ms` measures exchange-versus-receipt. Folding one into the other
///   makes it identically zero, which is why
///   `crates/app/tests/ws_lag_clock_guard.rs` exists and why this function
///   cannot be handed a receipt at all any more — the signature is the guard.
/// - The two day gates in `MultiTfAggregator::consume_tick`
///   (`stale_trading_day` / `future_trading_day`) compare the fold day against
///   the RECEIPT day. They are UNCHANGED: their question is "is the vendor's
///   stamp from a different trading day than the one we are living in", which
///   needs both clocks by construction.
/// - `received_at` remains a stored column on every tick row.
///
/// # Why this is still a function and not an inlined field read
///
/// It is the ONE named site that answers "which clock buckets a candle". A
/// bare `tick.exchange_timestamp` at fourteen call sites answers it fourteen
/// times, and the next directive would have to find all fourteen. The
/// single-argument signature is itself the mechanical guarantee: a receipt
/// cannot reach the fold clock, because there is nowhere to put it.
///
/// # Complexity
///
/// O(1) time, O(1) space, zero allocation, zero branches — the identity, which
/// the optimiser removes entirely. Safe to call per tick and per timeframe.
#[inline]
#[must_use]
pub const fn fold_clock_ist_secs(exchange_timestamp: u32) -> u32 {
    exchange_timestamp
}

/// 15:30:00 IST expressed as seconds-of-day (`15*3600 + 30*60`).
/// The NSE regular session closes at 15:30:00 — the candle window is
/// the half-open interval `[09:15:00, 15:30:00)`, so the last 1-minute
/// candle is `[15:29:00, 15:30:00)` (stamped 15:29). 375 1m candles/day.
/// Production consumer restored 2026-08-09 with the tick aggregator rebuild:
/// `MultiTfAggregator::consume_tick` gates the candle window on
/// `[MARKET_OPEN_SECS_OF_DAY_IST, MARKET_CLOSE_SECS_OF_DAY_IST)`. (It was
/// `#[cfg(test)]` between the 2026-07-17 stage-3 sweep, which deleted its
/// only caller, and that rebuild.) NOTE the value is 56_400 = 15:40:00 IST,
/// not 15:30 — the NSE CAS change of 2026-08-03; the pin test below asserts
/// it against the common-crate G1 gate so the two can never drift.
pub(crate) const MARKET_CLOSE_SECS_OF_DAY_IST: u32 = 56_400;

/// Runtime-indexable handle for the 21 candle timeframes.
///
/// Use [`Self::ALL`] to iterate. Use [`Self::from_ordinal`] for
/// runtime decoding (e.g. parsing audit-table rows). Use
/// [`Self::table_name`] / [`Self::dedup_key`] /
/// [`Self::seconds_per_bucket`] / [`Self::display_name`] for direct
/// look-up without recomputing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TfIndex {
    /// 1-minute candles (60 s).
    M1 = 0,
    /// 3-minute candles (180 s).
    M3 = 1,
    /// 5-minute candles (300 s).
    M5 = 2,
    /// 15-minute candles (900 s).
    M15 = 3,
    // -- The three surviving second-scale frames ----------------------
    // Sixteen second-scale frames were appended here on 2026-07-21 and
    // fifteen of them — plus `D1` at the old ordinal 4 and `M2` at 21 —
    // were REMOVED on 2026-09-19 by the operator's nine-frame directive
    // (`websocket-connection-scope-lock.md`, "THE FOLD COLLAPSES TO NINE
    // FRAMES"). The removal is what forced the ordinal renumbering below
    // and the SEAL_SPILL_FORMAT_VERSION 3 -> 4 bump that goes with it:
    // `S1` was ordinal 5 and is now 4, so an old spill record replayed
    // against this table would decode into the WRONG frame silently.
    /// 1-second candles (1 s). Live on the Dhan tick lane since 2026-08-11.
    S1 = 4,
    /// 3-second candles (3 s). Live on the Dhan tick lane since 2026-08-11.
    S3 = 5,
    /// 5-second candles (5 s). Live on the Dhan tick lane since 2026-08-11.
    S5 = 6,
    // -- The two surviving appended minute frames ---------------------
    /// 30-minute candles (1_800 s).
    M30 = 7,
    /// 60-minute candles (3_600 s). NOTE the 09:15 IST session anchor
    /// means the final 60m bucket of a regular session is PARTIAL —
    /// the grid runs 09:15/10:15/…/15:15, so the last bar covers
    /// 15:15–15:30 (15 minutes), not a full hour. Same for M30's
    /// 15:15–15:30 bucket. That is a property of anchoring to the open
    /// rather than to the hour, and it is deliberate: a bar that starts
    /// at the open is comparable across days, one that starts at 09:00
    /// is not.
    M60 = 8,
}

impl TfIndex {
    /// All NINE timeframes in ORDINAL order: the 4 minute frames that
    /// have held ordinals 0..=3 since the beginning, then the 3 surviving
    /// second-scale frames, then M30 and M60 — so the array is
    /// deliberately NOT globally seconds-ascending. The index of each
    /// entry equals its [`Self::as_ordinal`] value, which the hot-path
    /// `[Mutex<LiveCandleState>; TF_COUNT]` array indexing relies on.
    ///
    /// 2026-09-19: was 24 entries. `M1`..`M15` kept their ordinals; every
    /// other survivor renumbered, because `ALL` is indexed BY ordinal and
    /// [`Self::from_ordinal`] is its inverse, so the ordinals must stay
    /// contiguous `0..TF_COUNT`. There is no arrangement that removes the
    /// old ordinal 4 and leaves the rest where they were — which is
    /// exactly why the spill format version moved with it.
    pub const ALL: [TfIndex; TF_COUNT] = [
        TfIndex::M1,
        TfIndex::M3,
        TfIndex::M5,
        TfIndex::M15,
        TfIndex::S1,
        TfIndex::S3,
        TfIndex::S5,
        TfIndex::M30,
        TfIndex::M60,
    ];

    /// Returns the ordinal (`0..TF_COUNT`) used to index the
    /// per-instrument `[Mutex<LiveCandleState>; TF_COUNT]` array AND the
    /// storage-side `[Sender; TF_COUNT]` ILP writer array.
    #[inline]
    #[must_use]
    pub const fn as_ordinal(self) -> usize {
        self as usize
    }

    /// Decodes an ordinal back to a [`TfIndex`]. Returns `None` for
    /// out-of-range input (`>= TF_COUNT`). Used by the audit-table
    /// reader and any MCP `questdb_sql` consumer that surfaces
    /// `timeframe` SYMBOL rows.
    #[inline]
    #[must_use]
    pub const fn from_ordinal(ord: usize) -> Option<Self> {
        match ord {
            0 => Some(Self::M1),
            1 => Some(Self::M3),
            2 => Some(Self::M5),
            3 => Some(Self::M15),
            4 => Some(Self::S1),
            5 => Some(Self::S3),
            6 => Some(Self::S5),
            7 => Some(Self::M30),
            8 => Some(Self::M60),
            _ => None,
        }
    }

    /// Returns the plain QuestDB table name for this timeframe
    /// (`candles_1m` … `candles_1d`). The seal-time ILP writer uses
    /// this for `Buffer::table(...)`.
    #[inline]
    #[must_use]
    pub const fn table_name(self) -> &'static str {
        match self {
            Self::M1 => "candles_1m",
            Self::M3 => "candles_3m",
            Self::M5 => "candles_5m",
            Self::M15 => "candles_15m",
            Self::S1 => "candles_1s",
            Self::S3 => "candles_3s",
            Self::S5 => "candles_5s",
            Self::M30 => "candles_30m",
            Self::M60 => "candles_60m",
        }
    }

    /// Returns the `DEDUP UPSERT KEYS(...)` column list for this
    /// timeframe's table. Includes the designated timestamp `ts`
    /// explicitly — QuestDB rejects a DEDUP key that omits the
    /// designated timestamp column. The composite `(security_id,
    /// segment)` satisfies the I-P1-11 segment-aware uniqueness rule.
    ///
    /// `feed` (operator lock 2026-06-19, "same tables + feed column") is
    /// part of the key so a Dhan candle and a Groww candle for the SAME
    /// `(ts, security_id, segment)` minute are BOTH kept — distinct broker
    /// feeds are distinct observations, never a duplicate. The Dhan candle
    /// writer stamps a constant `feed='dhan'` and the Groww writer a
    /// constant `feed='groww'`, so the label is replay-stable and does NOT
    /// break the minute-bucket idempotency guarantee. Must equal
    /// `DEDUP_KEY_CANDLES` in `shadow_persistence.rs` (pinned by
    /// `test_dedup_key_candles_matches_tf_index_dedup_key`).
    #[inline]
    #[must_use]
    pub const fn dedup_key(self) -> &'static str {
        "ts, security_id, segment, feed"
    }

    /// Bucket size in seconds.
    #[inline]
    #[must_use]
    pub const fn seconds_per_bucket(self) -> u32 {
        match self {
            Self::M1 => 60,
            Self::M3 => 180,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::S1 => 1,
            Self::S3 => 3,
            Self::S5 => 5,
            Self::M30 => 1_800,
            Self::M60 => 3_600,
        }
    }

    /// True for the second-scale frames (bucket < 60 s). Since 2026-09-19 that
    /// is THREE — `S1`, `S3`, `S5` — and all three EMIT rows from the live
    /// Dhan fold. (Until 2026-09-22 this doc said "the 16 GDF-gated frames …
    /// ZERO rows are written today", which described the 24-frame era.)
    #[inline]
    #[must_use]
    pub const fn is_second_scale(self) -> bool {
        self.seconds_per_bucket() < 60
    }

    /// Short display name (`"1m"`, `"3m"`, ..., `"1d"`). Stable across
    /// the codebase and the audit-table `timeframe` SYMBOL column.
    #[inline]
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M3 => "3m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::S1 => "1s",
            Self::S3 => "3s",
            Self::S5 => "5s",
            Self::M30 => "30m",
            Self::M60 => "60m",
        }
    }

    /// Aligns a tick's IST-second timestamp to the start of its
    /// containing bucket for this timeframe.
    ///
    /// Buckets are anchored to the **CANDLE SESSION open**,
    /// `CANDLE_SESSION_OPEN_SECS_OF_DAY_IST` = **09:00:00 IST**, NOT to the
    /// epoch — so every timeframe's first candle of the day starts exactly at
    /// 09:00 (a 15m bucket is `[09:00,09:15)`). A tick at or before that
    /// anchor falls in the first bucket.
    ///
    /// ⚠ This paragraph read "the **09:15:00 IST market open** … a 15m bucket
    /// is `[09:15,09:30)`" until 2026-09-18, and the two lines of code
    /// immediately below it have anchored on 09:00 since the 2026-08-28
    /// pre-open directive. The inline comment there recorded the move; this
    /// doc block, which is what a reader and `cargo doc` actually see, did
    /// not — so the function's own documentation contradicted its body for
    /// three weeks, and it contradicted it about the exact quantity
    /// (`ts` bucket boundaries) that decides which candle a tick lands in.
    ///
    /// The last sentence of the old text is also gone rather than reworded:
    /// it said the market-hours gate "keeps genuine pre-open ticks out
    /// anyway", which the same 2026-08-28 directive reversed — pre-open ticks
    /// from 09:00 are now deliberately folded.
    ///
    /// `tick_ist_secs` MUST be the IST epoch second derived from the
    /// WS LTT field (NEVER `Utc::now()` per `data-integrity.md`).
    ///
    /// # Saturating by design
    ///
    /// Every add here is `saturating_add`, NOT a bare `+`. The release profile
    /// sets `overflow-checks = true` with `panic = "abort"`, so an arithmetic
    /// overflow does not return a wrong number — it kills the process, and in
    /// the 16-connection feed that means all sixteen sockets, not one tick.
    ///
    /// Today's only production caller (`MultiTfAggregator::consume_tick`)
    /// gates on the session window first, and for any `ts` passing that gate
    /// `ts % 86_400 >= MARKET_OPEN_SECS_OF_DAY_IST`, which algebraically rules
    /// the overflow out. But that is an UNDOCUMENTED PRECONDITION on a `pub
    /// fn`, enforced by a caller rather than by the type system — a second
    /// call site would silently reintroduce a remote abort, since
    /// `tick_ist_secs` is a raw `u32` off the wire that no parser
    /// range-validates. Flagged by two independent adversarial reviews
    /// (2026-08-09). Saturating makes the function safe for ANY input, so the
    /// precondition stops being load-bearing.
    #[inline]
    #[must_use]
    pub const fn bucket_start(self, tick_ist_secs: u32) -> u32 {
        let secs = self.seconds_per_bucket();
        let day_start = (tick_ist_secs / 86_400) * 86_400;
        // 2026-08-28: anchored on the CANDLE session open (09:00), not the
        // market open (09:15) — see CANDLE_SESSION_OPEN_SECS_OF_DAY_IST.
        let session_open = day_start.saturating_add(CANDLE_SESSION_OPEN_SECS_OF_DAY_IST);
        if tick_ist_secs <= session_open {
            return session_open;
        }
        session_open.saturating_add(((tick_ist_secs - session_open) / secs) * secs)
    }

    /// Returns the (exclusive) bucket-end for a given bucket-start.
    /// Equivalent to `bucket_start + seconds_per_bucket()`, saturating.
    ///
    /// Saturating for the same reason as [`Self::bucket_start`]: this one has
    /// NO production caller today, so it has no gate protecting it at all —
    /// it is one direct call away from an abort on a wire-derived value.
    #[inline]
    #[must_use]
    pub const fn bucket_end(self, bucket_start: u32) -> u32 {
        bucket_start.saturating_add(self.seconds_per_bucket())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The directive, stated as an equality: the fold clock IS the exchange
    /// stamp, and the receipt has no way to influence it.
    ///
    /// ## What this replaced (2026-09-18)
    ///
    /// Five tests pinned the delta-bounded hybrid: a live receipt one second
    /// late winning the bucket, a nine-hour replay stamp falling back, the
    /// `[-10 s, +300 s]` boundary on both sides, the absent-receipt sentinel,
    /// and the UTC/IST mix-up failing closed. Every one of them pinned a
    /// behaviour that no longer exists, and four of the five pinned a FALLBACK
    /// that is now the only path — so they would have passed unchanged while
    /// proving nothing. They are replaced rather than deleted, because a value
    /// nothing pins is a value a refactor can change in silence.
    #[test]
    fn the_fold_clock_is_the_exchange_stamp_and_nothing_else() {
        // The shapes the old hybrid treated differently, now all identical:
        // an ordinary mid-session second, the second a live receipt would have
        // moved (delivery lag across a minute boundary), a replayed frame's
        // stamp, and the arithmetic extremes.
        for exch in [
            0_u32,
            1,
            1_779_362_677,       // ~11:24 IST, the old fixture
            1_779_362_677 + 300, // the old lag bound
            1_779_362_677 - 10,  // the old lead bound
            u32::MAX,
        ] {
            assert_eq!(
                fold_clock_ist_secs(exch),
                exch,
                "operator directive 2026-09-18: OHLCV and volume bucket on the \
                 exchange `ts`. Any transformation here — a receipt, a clamp, \
                 a blend — re-opens the `ticks`-versus-candle divergence that \
                 `session_window.rs` records, because the tick writer's gate \
                 reads the exchange stamp alone."
            );
        }
    }

    /// It is `const`, and that is load-bearing rather than decoration.
    ///
    /// A `const fn` cannot read a clock, cannot allocate, and cannot call a
    /// non-const helper — so the compiler itself now refuses the entire class
    /// of change this directive forbids. The identity is evaluated at compile
    /// time wherever the stamp is known, and inlined to nothing where it is
    /// not: O(1) is not a claim here, it is the absence of code.
    #[test]
    fn the_fold_clock_is_evaluated_at_compile_time() {
        const FOLDED: u32 = fold_clock_ist_secs(1_779_362_677);
        assert_eq!(FOLDED, 1_779_362_677);
    }

    /// Session-constant drift pin (operator directive 2026-07-03): the
    /// trading-crate seconds-of-day session constants that gate the candle
    /// grid MUST stay 09:15:00 / 15:30:00 IST AND agree exactly with the
    /// canonical common-crate G1 gate constants (`MARKET_OPEN_IST_NANOS` /
    /// `MARKET_CLOSE_IST_NANOS`, nanos-of-day). If either representation is
    /// edited alone, this test fails the build — the day-OHLC gate
    /// (`day_ohlc_session_accepts` in the app crate) delegates to the
    /// common-crate gate, so this pin keeps ALL session windows identical.
    #[test]
    fn test_session_constants_pinned_and_agree_with_common_crate() {
        use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};

        assert_eq!(MARKET_OPEN_SECS_OF_DAY_IST, 33_300, "09:15:00 IST");
        // 2026-08-28: the CANDLE grid anchor is deliberately EARLIER than the
        // market open. Both values are pinned independently AND their ordering
        // is pinned, so a future edit cannot quietly collapse one into the
        // other in either direction — which is the whole risk of having two
        // constants that both look like "when does the day start".
        assert_eq!(
            CANDLE_SESSION_OPEN_SECS_OF_DAY_IST, 32_400,
            "09:00:00 IST - the NSE pre-open call auction starts here"
        );
        assert!(
            CANDLE_SESSION_OPEN_SECS_OF_DAY_IST < MARKET_OPEN_SECS_OF_DAY_IST,
            "the candle grid must open BEFORE the market, never at or after it"
        );
        assert_eq!(
            MARKET_OPEN_SECS_OF_DAY_IST - CANDLE_SESSION_OPEN_SECS_OF_DAY_IST,
            900,
            "the pre-open capture window is exactly 15 minutes (09:00-09:15)"
        );
        assert_eq!(
            MARKET_CLOSE_SECS_OF_DAY_IST, 56_400,
            "15:40:00 IST (exclusive) — NSE CAS change 2026-08-03"
        );
        assert_eq!(
            i64::from(MARKET_OPEN_SECS_OF_DAY_IST) * 1_000_000_000,
            MARKET_OPEN_IST_NANOS,
            "trading-crate open constant drifted from common-crate G1 gate open"
        );
        assert_eq!(
            i64::from(MARKET_CLOSE_SECS_OF_DAY_IST) * 1_000_000_000,
            MARKET_CLOSE_IST_NANOS,
            "trading-crate close constant drifted from common-crate G1 gate close"
        );
    }

    #[test]
    fn test_tf_index_all_has_nine_distinct_variants() {
        let mut seen = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            assert!(seen.insert(tf), "duplicate variant in TfIndex::ALL: {tf:?}");
        }
        assert_eq!(TfIndex::ALL.len(), TF_COUNT);
        assert_eq!(TF_COUNT, 9);
    }

    /// The ordinal sequence is NOT globally seconds-ascending, and since the
    /// 2026-09-19 nine-frame collapse it is not two blocks either — it is
    /// three: the original minute block `M1 M3 M5 M15`, the second-scale
    /// survivors `S1 S3 S5` that C3 appended after it on 2026-07-21, and
    /// `M30 M60` appended on 2026-08-10. The collapse removed fifteen frames
    /// from the MIDDLE of that sequence and closed the gaps, so every
    /// surviving ordinal above `M15` MOVED.
    ///
    /// This pin is the EXACT seconds sequence, and that is what makes such a
    /// move fail the build: a round-trip test is structurally blind to a
    /// renumbering, because `from_ordinal(n).as_ordinal() == n` holds under
    /// ANY consistent table.
    #[test]
    fn test_tf_index_ordinal_order_pins_exact_seconds_sequence() {
        let secs: Vec<u32> = TfIndex::ALL
            .iter()
            .map(|tf| tf.seconds_per_bucket())
            .collect();
        let expected: Vec<u32> = vec![60, 180, 300, 900, 1, 3, 5, 1_800, 3_600];
        assert_eq!(secs, expected, "ordinal seconds sequence drifted");
        for block in [&secs[..4], &secs[4..7], &secs[7..]] {
            for window in block.windows(2) {
                assert!(
                    window[0] < window[1],
                    "each ordinal block must be strictly ascending by \
                     seconds_per_bucket; got {} >= {}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// ADVERSARIAL REGRESSION (2026-08-09, flagged by two independent
    /// reviews). `tick_ist_secs` is a raw `u32` off the wire that no parser
    /// range-validates. The release profile is `overflow-checks = true` with
    /// `panic = "abort"`, so an overflow here does not produce a wrong candle
    /// — it aborts the process, taking all sixteen sockets down. These must
    /// return a saturated value for EVERY input, with no caller-side gate.
    #[test]
    fn test_bucket_math_saturates_instead_of_aborting_on_extreme_inputs() {
        for tf in TfIndex::ALL {
            for ts in [u32::MAX, u32::MAX - 1, u32::MAX - 86_399, 0, 1] {
                let start = tf.bucket_start(ts);
                // Reaching here at all is the assertion: a bare `+` would have
                // aborted the test process under overflow-checks.
                let end = tf.bucket_end(start);
                assert!(
                    end >= start,
                    "{tf:?}: bucket_end({start}) = {end} must never wrap below \
                     its start for ts {ts}"
                );
            }
            // The saturating ceiling is reachable and stable.
            assert_eq!(
                tf.bucket_end(u32::MAX),
                u32::MAX,
                "{tf:?}: bucket_end must clamp at u32::MAX, not wrap to 0"
            );
        }
    }

    #[test]
    fn test_tf_index_ordinals_are_literal_pins() {
        // RENAMED from `..._append_only_literal_pins` on 2026-09-19, because
        // the append-only property it asserted is exactly what the nine-frame
        // collapse broke. Fifteen frames were REMOVED from the middle of the
        // table and the gaps closed, so `S1` moved 5 → 4, `S5` 9 → 6, `M30`
        // 22 → 7 and `M60` 23 → 8. A byte already on disk therefore decodes
        // into a DIFFERENT frame, silently — every retired ordinal is still IN
        // RANGE for the nine-entry table, so refusal-past-the-end cannot catch
        // it. That is why `SEAL_SPILL_FORMAT_VERSION` went 3 → 4 in the same
        // change: the version refusal is the guarantee, and these literals are
        // what stop the table moving again without one.
        assert_eq!(TfIndex::M1 as u8, 0);
        assert_eq!(TfIndex::M3 as u8, 1);
        assert_eq!(TfIndex::M5 as u8, 2);
        assert_eq!(TfIndex::M15 as u8, 3);
        assert_eq!(TfIndex::S1 as u8, 4);
        assert_eq!(TfIndex::S3 as u8, 5);
        assert_eq!(TfIndex::S5 as u8, 6);
        assert_eq!(TfIndex::M30 as u8, 7);
        assert_eq!(TfIndex::M60 as u8, 8);
        // The pinned block above must cover EVERY variant: a frame added or
        // removed without a pin here would slip through otherwise.
        assert_eq!(
            TF_COUNT, 9,
            "a frame was added or removed — pin its literal ordinal above, \
             and bump SEAL_SPILL_FORMAT_VERSION if any existing one moved"
        );
        // …and the seconds are pinned per-ordinal too, so a variant cannot be
        // re-pointed at a different bucket size while keeping its ordinal.
        assert_eq!(TfIndex::M1.seconds_per_bucket(), 60);
        assert_eq!(TfIndex::S1.seconds_per_bucket(), 1);
        assert_eq!(TfIndex::S5.seconds_per_bucket(), 5);
        assert_eq!(TfIndex::M60.seconds_per_bucket(), 3_600);
    }
    #[test]
    fn test_tf_index_ordinal_round_trip() {
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            assert_eq!(tf.as_ordinal(), idx, "ordinal mismatch for {tf:?}");
            assert_eq!(
                TfIndex::from_ordinal(idx),
                Some(*tf),
                "from_ordinal({idx}) failed to roundtrip"
            );
        }
    }

    #[test]
    fn test_tf_index_from_ordinal_rejects_out_of_range() {
        assert_eq!(TfIndex::from_ordinal(TF_COUNT), None);
        assert_eq!(TfIndex::from_ordinal(usize::MAX), None);
    }

    #[test]
    fn test_tf_index_table_names_are_plain_and_canonical() {
        let names: [&str; TF_COUNT] = std::array::from_fn(|i| {
            TfIndex::from_ordinal(i)
                .expect("ordinal in range")
                .table_name()
        });
        let expected = [
            "candles_1m",
            "candles_3m",
            "candles_5m",
            "candles_15m",
            // The second-scale block (2026-09-19 collapse): 1s/3s/5s only.
            "candles_1s",
            "candles_3s",
            "candles_5s",
            "candles_30m",
            "candles_60m",
        ];
        assert_eq!(names, expected);
        // No `_shadow` suffix anywhere — these are first-class tables.
        for name in names {
            assert!(!name.contains("shadow"), "{name} must be a plain table");
        }
    }

    #[test]
    fn test_tf_index_table_names_unique() {
        let mut seen = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            assert!(
                seen.insert(tf.table_name()),
                "duplicate table_name {}",
                tf.table_name()
            );
        }
    }

    #[test]
    fn test_tf_index_dedup_key_includes_ts_and_segment_for_i_p1_11() {
        // QuestDB rejects a DEDUP key that omits the designated
        // timestamp; I-P1-11 requires the segment alongside security_id.
        for tf in TfIndex::ALL {
            let key = tf.dedup_key();
            assert!(
                key.contains("ts"),
                "{} dedup key missing ts",
                tf.display_name()
            );
            assert!(
                key.contains("security_id"),
                "{} dedup key missing security_id",
                tf.display_name()
            );
            assert!(
                key.contains("segment"),
                "I-P1-11 violation: {} dedup key {:?} missing segment",
                tf.display_name(),
                key
            );
            assert!(
                key.contains("feed"),
                "feed-in-key (operator 2026-06-19): {} dedup key {:?} missing feed",
                tf.display_name(),
                key
            );
        }
    }

    #[test]
    fn test_tf_index_display_names_unique_and_stable() {
        // The nine names in ORDINAL order (2026-09-19 collapse). The three
        // blocks are the enum's own layout: the four legacy minute frames,
        // then the second-scale frames, then the two long minute frames.
        let mut seen = std::collections::HashSet::new();
        let expected = ["1m", "3m", "5m", "15m", "1s", "3s", "5s", "30m", "60m"];
        assert_eq!(
            expected.len(),
            TF_COUNT,
            "the expected-name list must name every frame"
        );
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            let name = tf.display_name();
            assert_eq!(
                name, expected[idx],
                "display_name diverged at ordinal {idx}"
            );
            assert!(seen.insert(name), "duplicate display_name {name}");
        }
    }

    #[test]
    fn test_tf_index_seconds_per_bucket_values() {
        assert_eq!(TfIndex::M1.seconds_per_bucket(), 60);
        assert_eq!(TfIndex::M3.seconds_per_bucket(), 180);
        assert_eq!(TfIndex::M5.seconds_per_bucket(), 300);
        assert_eq!(TfIndex::M15.seconds_per_bucket(), 900);
        assert_eq!(TfIndex::M30.seconds_per_bucket(), 1_800);
        assert_eq!(TfIndex::M60.seconds_per_bucket(), 3_600);
        // The three surviving second-scale frames.
        assert_eq!(TfIndex::S1.seconds_per_bucket(), 1);
        assert_eq!(TfIndex::S3.seconds_per_bucket(), 3);
        assert_eq!(TfIndex::S5.seconds_per_bucket(), 5);
        // Every minute-class TF is a whole number of minutes.
        for tf in [
            TfIndex::M1,
            TfIndex::M3,
            TfIndex::M5,
            TfIndex::M15,
            TfIndex::M30,
            TfIndex::M60,
        ] {
            assert_eq!(tf.seconds_per_bucket() % 60, 0);
        }
    }

    #[test]
    fn test_tf_index_second_scale_gate_is_exactly_the_three_second_frames() {
        // `is_second_scale` is a SECONDS predicate (`< 60`), never an
        // ordinal range — which is why it survived the 2026-09-19 collapse
        // untouched while the ordinals moved under it. Before the collapse
        // this test named sixteen frames and asserted `as_ordinal() >= 5`;
        // both were true of the 24-frame table and neither is true now
        // (S1 is ordinal 4), so the assertion is stated as the SET instead.
        let second_scale: Vec<TfIndex> = TfIndex::ALL
            .into_iter()
            .filter(|tf| tf.is_second_scale())
            .collect();
        assert_eq!(
            second_scale,
            vec![TfIndex::S1, TfIndex::S3, TfIndex::S5],
            "the second-scale set is exactly the operator's three sub-minute frames"
        );
        for tf in [
            TfIndex::M1,
            TfIndex::M3,
            TfIndex::M5,
            TfIndex::M15,
            TfIndex::M30,
            TfIndex::M60,
        ] {
            assert!(!tf.is_second_scale(), "{tf:?} wrongly reads as sub-minute");
        }
        for tf in second_scale {
            assert!(
                tf.seconds_per_bucket() < 60,
                "{tf:?} gated but not sub-minute"
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_start_aligns_to_seconds_per_bucket() {
        // An in-window IST tick (~11:24 IST). Buckets anchor to the
        // 09:00:00 CANDLE session open (2026-08-28: was the 09:15 market
        // open), NOT the epoch. The distinction is visible in this test for
        // 2m/30m/60m, whose grids moved: the 900 s between the two anchors
        // divides evenly into 60/180/300/900 but not into 120/1800/3600.
        let tick = 1_779_362_677_u32;
        let session_open = (tick / 86_400) * 86_400 + 32_400;
        for tf in TfIndex::ALL {
            let bucket = tf.bucket_start(tick);
            let secs = tf.seconds_per_bucket();
            assert!(
                bucket <= tick,
                "bucket_start past input for {}",
                tf.display_name()
            );
            assert_eq!(
                (bucket - session_open) % secs,
                0,
                "bucket_start not anchored to 09:00 for {}",
                tf.display_name()
            );
            assert!(
                tick - bucket < secs,
                "bucket_start too far below input for {}",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_start_idempotent_on_aligned_input() {
        let tick = 1_779_362_677_u32;
        for tf in TfIndex::ALL {
            let bucket = tf.bucket_start(tick);
            assert_eq!(
                tf.bucket_start(bucket),
                bucket,
                "bucket_start should be idempotent on a bucket boundary for {}",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_tf_index_bucket_end_equals_start_plus_secs() {
        for tf in TfIndex::ALL {
            let start = tf.bucket_start(1_716_192_000_u32);
            let end = tf.bucket_end(start);
            assert_eq!(end - start, tf.seconds_per_bucket());
        }
    }

    #[test]
    fn test_tf_index_repr_u8_matches_ordinal() {
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            assert_eq!(*tf as u8, u8::try_from(idx).expect("ordinal fits u8"));
        }
    }

    /// `Ord` sorts by the `repr(u8)` discriminant = the seal-spill
    /// APPEND order (C3) — which is exactly `ALL`'s order. Deliberately
    /// NOT seconds order: M15 (ordinal 3, 900 s) precedes S1 (ordinal 4, 1 s)
    /// (2026-09-22: this read "D1 (ordinal 4) precedes S1 (ordinal 5)", the
    /// pre-2026-09-19 numbering; D1 no longer exists).
    #[test]
    fn test_tf_index_total_ordering_matches_ordinal_append_order() {
        let mut sorted = TfIndex::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, TfIndex::ALL);
    }

    /// `TfIndex::ALL` IS exactly the nine native frames the operator asked
    /// for on 2026-09-18 — no more, no fewer, and each named individually.
    ///
    /// ## Why every frame is named rather than counted
    ///
    /// A count alone would pass if one frame were swapped for another, and
    /// that is precisely the mistake available here: the second-scale
    /// variants sit adjacent to one another (`S1`/`S3`/`S5`), so an
    /// off-by-one in either direction is a plausible edit a length check
    /// would wave through.
    ///
    /// ## Why this pins the ENUM and no longer a predicate
    ///
    /// Until 2026-09-19 the enum carried 24 variants and a predicate
    /// (`is_operator_requested`) decided which of them emitted rows; this
    /// test pinned that predicate's answer across four blocks — the nine
    /// requested, the four withdrawn on 2026-09-18, `D1` (withdrawn
    /// 2026-08-25 for a DIFFERENT reason — the operator ruled a day bar must
    /// never be derived from the internal fold at all), and ten that were
    /// never asked for.
    ///
    /// The 2026-09-19 directive DELETED those fifteen variants outright
    /// (`TF_COUNT` 24 -> 9), so the predicate became tautologically true and
    /// went with them. The guarantee it carried now belongs to the enum: a
    /// frame cannot be emitted-but-unwanted, because an unwanted frame has
    /// no variant to be. Restoring any of the fifteen means re-adding a
    /// variant, which fails this test by name.
    ///
    /// The four-way split is NOT reproduced here, deliberately: "asked for,
    /// then withdrawn" and "never asked for" are different facts about
    /// variants that no longer exist, and the historical record of which was
    /// which lives in the dated rule-file sections, not in a test over an
    /// enum that cannot express them. What a reader needs from THIS test is
    /// the live set.
    #[test]
    fn tf_index_all_is_the_operators_nine() {
        assert_eq!(
            TfIndex::ALL.len(),
            9,
            "operator directive 2026-09-18 named eleven entries: `ticks` (not \
             a candle frame), `10m` (DERIVED from candles_1m as a view, so no \
             TfIndex variant) and these NINE native frames. Found {}. \
             Changing this set needs a fresh dated quote.",
            TfIndex::ALL.len()
        );

        assert_eq!(
            TfIndex::ALL,
            [
                TfIndex::M1,
                TfIndex::M3,
                TfIndex::M5,
                TfIndex::M15,
                TfIndex::S1,
                TfIndex::S3,
                TfIndex::S5,
                TfIndex::M30,
                TfIndex::M60,
            ],
            "TfIndex::ALL must be exactly the operator's nine, in ordinal \
             order. `ALL` is indexed BY ordinal and `from_ordinal` is its \
             inverse, so the order is load-bearing, not cosmetic."
        );

        // The retired names, asserted as ABSENT by table name rather than by
        // variant — a deleted variant cannot be named in Rust, so this is the
        // only form the guarantee can take here.
        let live: Vec<&str> = TfIndex::ALL.iter().map(|tf| tf.table_name()).collect();
        for retired in [
            "candles_1d",
            "candles_2s",
            "candles_4s",
            "candles_6s",
            "candles_7s",
            "candles_8s",
            "candles_9s",
            "candles_10s",
            "candles_11s",
            "candles_12s",
            "candles_13s",
            "candles_14s",
            "candles_15s",
            "candles_30s",
            "candles_2m",
        ] {
            assert!(
                !live.contains(&retired),
                "{retired} was retired by the 2026-09-19 directive — \
                 restoring it needs a fresh dated quote, and its DROP entry \
                 in shadow_persistence::RETIRED_CANDLE_TABLES would have to \
                 move with it"
            );
        }
    }
}
