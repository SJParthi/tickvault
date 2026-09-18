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
pub const TF_COUNT: usize = 24;

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
    /// 1-day candles (86_400 s — UTC-aligned arithmetic; the
    /// IST-midnight rollover task force-seals open bars at IST 00:00
    /// every trading day so the UTC boundary does not produce stale
    /// candles in practice).
    D1 = 4,
    // -- Second-scale frames (C3, operator directive 2026-07-21) ------
    // APPENDED after D1 so every pre-existing seal-spill ordinal
    // (0..=4) stays byte-stable (SEAL_SPILL_FORMAT_VERSION stays 1).
    // WAS "STRUCTURAL ONLY: all 16 frames are GDF-feed-gated — ZERO rows
    // until the GDF 1s live feed lands (separate lane)". CORRECTED
    // 2026-08-19: the Dhan live lane was revived (2026-08-09) and switched
    // on (2026-08-11), and the fold has no feed gate, so these frames carry
    // real rows today. The REST 1m cadence fold half of that sentence still
    // holds — a 1-minute vendor bar cannot open a sub-minute bucket.
    /// 1-second candles (1 s). Live on the Dhan tick lane since 2026-08-11.
    S1 = 5,
    /// 2-second candles (2 s). Live on the Dhan tick lane since 2026-08-11.
    S2 = 6,
    /// 3-second candles (3 s). Live on the Dhan tick lane since 2026-08-11.
    S3 = 7,
    /// 4-second candles (4 s). Live on the Dhan tick lane since 2026-08-11.
    S4 = 8,
    /// 5-second candles (5 s). Live on the Dhan tick lane since 2026-08-11.
    S5 = 9,
    /// 6-second candles (6 s). Live on the Dhan tick lane since 2026-08-11.
    S6 = 10,
    /// 7-second candles (7 s). Live on the Dhan tick lane since 2026-08-11.
    S7 = 11,
    /// 8-second candles (8 s). Live on the Dhan tick lane since 2026-08-11.
    S8 = 12,
    /// 9-second candles (9 s). Live on the Dhan tick lane since 2026-08-11.
    S9 = 13,
    /// 10-second candles (10 s). Live on the Dhan tick lane since 2026-08-11.
    S10 = 14,
    /// 11-second candles (11 s). Live on the Dhan tick lane since 2026-08-11.
    S11 = 15,
    /// 12-second candles (12 s). Live on the Dhan tick lane since 2026-08-11.
    S12 = 16,
    /// 13-second candles (13 s). Live on the Dhan tick lane since 2026-08-11.
    S13 = 17,
    /// 14-second candles (14 s). Live on the Dhan tick lane since 2026-08-11.
    S14 = 18,
    /// 15-second candles (15 s). Live on the Dhan tick lane since 2026-08-11.
    S15 = 19,
    /// 30-second candles (30 s). Live on the Dhan tick lane since 2026-08-11.
    S30 = 20,
    // -- Minute frames completing the operator's 13-frame set ---------
    // APPENDED after S30 (2026-08-10) so every pre-existing ordinal
    // (0..=20) stays byte-stable and SEAL_SPILL_FORMAT_VERSION stays 1.
    //
    // WHY these three and not others: operator Quote 13 (2026-08-08,
    // `daily-universe-scope-expansion-2026-05-27.md` §0) specifies
    // thirteen current-day timeframes — 1s/5s/10s/15s/30s, then
    // 1m/2m/3m/5m/15m/30m/60m, then 1d. Ten of the thirteen already
    // existed; M2, M30 and M60 did NOT, so three of the frames the
    // r8g.xlarge upgrade was bought to serve were literally
    // unrepresentable. These are the missing three.
    //
    // Unlike the second-scale block above, these are NOT structural
    // placeholders: the minute-scale frames are derivable from the
    // existing tick and REST-fold paths the moment a producer exists.
    /// 2-minute candles (120 s).
    M2 = 21,
    /// 30-minute candles (1_800 s).
    M30 = 22,
    /// 60-minute candles (3_600 s). NOTE the 09:15 IST session anchor
    /// means the final 60m bucket of a regular session is PARTIAL —
    /// the grid runs 09:15/10:15/…/15:15, so the last bar covers
    /// 15:15–15:30 (15 minutes), not a full hour. Same for M30's
    /// 15:15–15:30 bucket. That is a property of anchoring to the open
    /// rather than to the hour, and it is deliberate: a bar that starts
    /// at the open is comparable across days, one that starts at 09:00
    /// is not.
    M60 = 23,
}

impl TfIndex {
    /// All 21 timeframes in ORDINAL (seal-spill append) order: the 5
    /// legacy frames (1m/3m/5m/15m/1d) first, then the 16 GDF-gated
    /// second-scale frames (1s..15s, 30s) appended by C3 — so the
    /// array is deliberately NOT globally seconds-ascending. The index
    /// of each entry equals its [`Self::as_ordinal`] value, which the
    /// hot-path `[Mutex<LiveCandleState>; TF_COUNT]` array indexing
    /// relies on.
    pub const ALL: [TfIndex; TF_COUNT] = [
        TfIndex::M1,
        TfIndex::M3,
        TfIndex::M5,
        TfIndex::M15,
        TfIndex::D1,
        TfIndex::S1,
        TfIndex::S2,
        TfIndex::S3,
        TfIndex::S4,
        TfIndex::S5,
        TfIndex::S6,
        TfIndex::S7,
        TfIndex::S8,
        TfIndex::S9,
        TfIndex::S10,
        TfIndex::S11,
        TfIndex::S12,
        TfIndex::S13,
        TfIndex::S14,
        TfIndex::S15,
        TfIndex::S30,
        TfIndex::M2,
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
            4 => Some(Self::D1),
            5 => Some(Self::S1),
            6 => Some(Self::S2),
            7 => Some(Self::S3),
            8 => Some(Self::S4),
            9 => Some(Self::S5),
            10 => Some(Self::S6),
            11 => Some(Self::S7),
            12 => Some(Self::S8),
            13 => Some(Self::S9),
            14 => Some(Self::S10),
            15 => Some(Self::S11),
            16 => Some(Self::S12),
            17 => Some(Self::S13),
            18 => Some(Self::S14),
            19 => Some(Self::S15),
            20 => Some(Self::S30),
            21 => Some(Self::M2),
            22 => Some(Self::M30),
            23 => Some(Self::M60),
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
            Self::D1 => "candles_1d",
            Self::S1 => "candles_1s",
            Self::S2 => "candles_2s",
            Self::S3 => "candles_3s",
            Self::S4 => "candles_4s",
            Self::S5 => "candles_5s",
            Self::S6 => "candles_6s",
            Self::S7 => "candles_7s",
            Self::S8 => "candles_8s",
            Self::S9 => "candles_9s",
            Self::S10 => "candles_10s",
            Self::S11 => "candles_11s",
            Self::S12 => "candles_12s",
            Self::S13 => "candles_13s",
            Self::S14 => "candles_14s",
            Self::S15 => "candles_15s",
            Self::S30 => "candles_30s",
            Self::M2 => "candles_2m",
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
            Self::D1 => 86_400,
            Self::S1 => 1,
            Self::S2 => 2,
            Self::S3 => 3,
            Self::S4 => 4,
            Self::S5 => 5,
            Self::S6 => 6,
            Self::S7 => 7,
            Self::S8 => 8,
            Self::S9 => 9,
            Self::S10 => 10,
            Self::S11 => 11,
            Self::S12 => 12,
            Self::S13 => 13,
            Self::S14 => 14,
            Self::S15 => 15,
            Self::S30 => 30,
            Self::M2 => 120,
            Self::M30 => 1_800,
            Self::M60 => 3_600,
        }
    }

    /// True for the 16 GDF-gated second-scale frames (bucket < 60 s:
    /// 1s..=15s + 30s). These frames are STRUCTURAL until the GDF 1s live
    /// feed lands (separate lane) — ZERO rows are written today, the REST
    /// 1m cadence folds only the 5-frame minute/day set, and the RAM store
    /// allocates them as capacity-1 placeholders (never full session rings).
    #[inline]
    #[must_use]
    pub const fn is_second_scale(self) -> bool {
        self.seconds_per_bucket() < 60
    }

    /// True for the NINE native timeframes the operator asks for today.
    ///
    /// Operator, 2026-08-08 (verbatim, typos preserved — the same quote the
    /// r8g.xlarge was sized against, `daily-universe-scope-expansion` Quote 13):
    ///
    /// > "current day ticks secodns multiple seocdns tiemframes liek 1 seocnd
    /// > 5 seconds 10 15 30 seocnds dude nad then even mintue level tiemframes
    /// > liek 1,2,3,5,15,30,60 and 1 dya also"
    ///
    /// That is `S1 S5 S10 S15 S30` + `M1 M2 M3 M5 M15 M30 M60` + `D1` = 13.
    ///
    /// # Why this exists
    ///
    /// ⚠ **The three paragraphs below describe the 2026-08-08 set and are
    /// SUPERSEDED by the 2026-09-18 section at the end of this doc.** They are
    /// kept because the REASONING — gate emission, never delete variants — is
    /// what this function still does; only the membership moved. Read the
    /// named frames in them as history, not as the current set.
    ///
    /// The enum carries **24** variants, so **eleven** second-scale frames —
    /// `S2 S3 S4 S6 S7 S8 S9 S11 S12 S13 S14` — are neither requested nor used
    /// by anything. Before this gate the live lane sealed all 24 on every fold,
    /// so those eleven wrote rows to disk every bucket, for nobody.
    ///
    /// The plan item that flagged this proposed gating **all sixteen**
    /// second-scale frames off, citing the disk cost. That would have been
    /// wrong in the opposite direction: it deletes `S1 S5 S10 S15 S30`, which
    /// are five of the frames the operator explicitly requested and the
    /// reason the 32 GiB instance was bought. Gating exactly the eleven
    /// unrequested frames removes most of the cost while removing none of the
    /// capability — the requirement and the disk concern were never actually
    /// in conflict, only the two framings of the fix were.
    ///
    /// # Scope
    ///
    /// This gates ROW EMISSION, not folding. The aggregator still keeps its
    /// `[_; TF_COUNT]` slots and ordinals, so nothing here changes the array
    /// layout, the audit-table `timeframe` symbols, or ordinal decoding —
    /// deleting variants would cascade through all of that for no benefit.
    ///
    /// Changing this set needs a fresh dated operator quote, exactly like the
    /// constants it derives from; `tf_index_operator_set_is_the_operators_nine`
    /// pins it.
    ///
    /// # 2026-08-25 — D1 removed (13 -> 12)
    ///
    /// Operator directive 2026-08-25: *"never evr do th edeirvation of 1day
    /// usign these intenrla tiemframes clauclation"*. `D1` therefore leaves
    /// this set, so the live lane stops emitting `candles_1d`. Recorded in
    /// `.claude/rules/project/live-feed-purity.md` rule 10 BEFORE this edit,
    /// per the rule-file-first law. The variant, its ordinal and its slot are
    /// untouched — only EMISSION is gated, exactly as the eleven unrequested
    /// second-scale frames already are.
    ///
    /// # 2026-09-18 — the set is REPLACED (12 -> 9 native), and 10m is DERIVED
    ///
    /// Operator directive 2026-09-18, recorded in
    /// `websocket-connection-scope-lock.md` section "2026-09-18 — ELEVEN
    /// TIMEFRAMES, ONE SIGNED VOLUME" BEFORE this edit, per the
    /// rule-file-first law. Verbatim: *"we will have one and only candles
    /// tables timeframe which is ticks, 1s, 3s, 5s, 1m, 3m, 5m, 10m, 15m,
    /// 30m, 60m right dude only these timeframes alone dude okay?"*
    ///
    /// Net against the 2026-08-08 set: **`S3` GAINS** emission, and
    /// **`S10` / `S15` / `S30` / `M2` LOSE** it. The DDL still creates all
    /// 24 names from `TfIndex::ALL`, so those four tables keep existing and
    /// keep the rows they already hold. Only new rows stop.
    ///
    /// ⚠ **CORRECTED 2026-09-18, hours after the line above was written.**
    /// It originally closed *"and no populated table is ever dropped (SEBI
    /// retention)"*. That is false twice over, and both halves are checkable
    /// in one grep:
    ///
    /// 1. **Candle tables are NOT a SEBI never-delete class.** They are
    ///    retention-swept like any other market-data table —
    ///    `partition_manager.rs` iterates `candle_table_names()` and detaches
    ///    DAY partitions past the cutoff. So "keeps every row it already
    ///    holds" is true only inside the retention window; older partitions
    ///    leave on schedule, exactly as they always have.
    /// 2. **One candle table IS dropped by name, at boot.**
    ///    `shadow_persistence::drop_legacy_candle_objects` carries a literal
    ///    `DROP TABLE IF EXISTS candles_1s;` — written when `S1` was an
    ///    Engine-A leftover. `S1` is now a frame the operator asked for, so
    ///    that drop's original rationale no longer holds. It is version-gated
    ///    behind a one-shot marker (`LEGACY_DROP_SWEEP_VERSION`) and the
    ///    `candle_ddl_boot` site documents the consequence, so it does not
    ///    fire on an ordinary boot — but the sentence claimed a guarantee
    ///    ("never") that the tree does not make.
    ///
    /// Recorded rather than quietly reworded because the shape is the point:
    /// "SEBI retention" is this repository's strongest never-delete claim, and
    /// borrowing it for a class it does not cover reads as a guarantee to
    /// anyone who does not re-check. The retirement of a frame's WRITER says
    /// nothing about the fate of its TABLE, and the two must be checked
    /// separately.
    ///
    /// ## Why this returns NINE for an eleven-item list
    ///
    /// The operator's list has eleven entries and two of them are not native
    /// fold frames:
    ///
    /// | Entry | Where it comes from |
    /// |---|---|
    /// | `ticks` | the separate `ticks` table, not a candle frame at all |
    /// | `10m` | **DERIVED** from `candles_1m`, never folded |
    /// | the other nine | this set |
    ///
    /// `10m` is derived on the operator's own instruction (*"no 10s derive
    /// the 10m dude okay"*) and there is no `M10` variant to add. That is
    /// deliberate and load-bearing: a new variant would take ordinal 24,
    /// move `TF_COUNT` 24 -> 25, resize every `[_; TF_COUNT]` array and the
    /// seal ring with it, and add a 25th scalar fold to the per-tick path —
    /// for a bar that is exactly `first(open) / max(high) / min(low) /
    /// last(close)` over ten 1-minute bars. Deriving costs nothing per tick.
    #[inline]
    #[must_use]
    pub const fn is_operator_requested(self) -> bool {
        matches!(
            self,
            Self::S1
                | Self::S3
                | Self::S5
                | Self::M1
                | Self::M3
                | Self::M5
                | Self::M15
                | Self::M30
                | Self::M60
        )
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
            Self::D1 => "1d",
            Self::S1 => "1s",
            Self::S2 => "2s",
            Self::S3 => "3s",
            Self::S4 => "4s",
            Self::S5 => "5s",
            Self::S6 => "6s",
            Self::S7 => "7s",
            Self::S8 => "8s",
            Self::S9 => "9s",
            Self::S10 => "10s",
            Self::S11 => "11s",
            Self::S12 => "12s",
            Self::S13 => "13s",
            Self::S14 => "14s",
            Self::S15 => "15s",
            Self::S30 => "30s",
            Self::M2 => "2m",
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
    fn test_tf_index_all_has_twenty_four_distinct_variants() {
        let mut seen = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            assert!(seen.insert(tf), "duplicate variant in TfIndex::ALL: {tf:?}");
        }
        assert_eq!(TfIndex::ALL.len(), TF_COUNT);
        assert_eq!(TF_COUNT, 24);
    }

    /// C3 (2026-07-21): the 16 second-scale frames are APPENDED after
    /// D1 so every pre-existing seal-spill ordinal (0..=4) stays
    /// stable — `ALL` is therefore ordinal-ordered, NOT globally
    /// seconds-ascending. This pin is strictly stronger than the old
    /// ascending check: it pins the EXACT seconds sequence, and each
    /// ordinal block stays strictly ascending within itself.
    #[test]
    fn test_tf_index_ordinal_order_pins_exact_seconds_sequence() {
        let secs: Vec<u32> = TfIndex::ALL
            .iter()
            .map(|tf| tf.seconds_per_bucket())
            .collect();
        // Appended 2026-08-10: M2/M30/M60 (120/1800/3600) complete the
        // operator's thirteen frames. They land at the END of the
        // second-scale block, which keeps that block strictly ascending —
        // the property the windows() check below relies on.
        let expected: Vec<u32> = [60_u32, 180, 300, 900, 86_400]
            .into_iter()
            .chain(1..=15)
            .chain([30, 120, 1_800, 3_600])
            .collect();
        assert_eq!(secs, expected, "ordinal seconds sequence drifted");
        for block in [&secs[..5], &secs[5..]] {
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
    fn test_tf_index_ordinals_are_append_only_literal_pins() {
        assert_eq!(TfIndex::M1 as u8, 0);
        assert_eq!(TfIndex::M3 as u8, 1);
        assert_eq!(TfIndex::M5 as u8, 2);
        assert_eq!(TfIndex::M15 as u8, 3);
        assert_eq!(TfIndex::D1 as u8, 4);
        assert_eq!(TfIndex::S1 as u8, 5);
        assert_eq!(TfIndex::S2 as u8, 6);
        assert_eq!(TfIndex::S3 as u8, 7);
        assert_eq!(TfIndex::S4 as u8, 8);
        assert_eq!(TfIndex::S5 as u8, 9);
        assert_eq!(TfIndex::S6 as u8, 10);
        assert_eq!(TfIndex::S7 as u8, 11);
        assert_eq!(TfIndex::S8 as u8, 12);
        assert_eq!(TfIndex::S9 as u8, 13);
        assert_eq!(TfIndex::S10 as u8, 14);
        assert_eq!(TfIndex::S11 as u8, 15);
        assert_eq!(TfIndex::S12 as u8, 16);
        assert_eq!(TfIndex::S13 as u8, 17);
        assert_eq!(TfIndex::S14 as u8, 18);
        assert_eq!(TfIndex::S15 as u8, 19);
        assert_eq!(TfIndex::S30 as u8, 20);
        // Appended 2026-08-10 to complete the operator's thirteen frames
        // (Quote 13). ADD NEW FRAMES BELOW THIS LINE ONLY — inserting one
        // above silently re-maps every already-spilled ordinal.
        assert_eq!(TfIndex::M2 as u8, 21);
        assert_eq!(TfIndex::M30 as u8, 22);
        assert_eq!(TfIndex::M60 as u8, 23);
        // The pinned block above must cover EVERY variant: a new appended
        // frame that nobody pinned would slip through otherwise.
        assert_eq!(
            TF_COUNT, 24,
            "a frame was added — pin its literal ordinal above"
        );
        // …and the seconds are pinned per-ordinal too, so a variant cannot be
        // re-pointed at a different bucket size while keeping its ordinal.
        assert_eq!(TfIndex::M1.seconds_per_bucket(), 60);
        assert_eq!(TfIndex::D1.seconds_per_bucket(), 86_400);
        assert_eq!(TfIndex::S1.seconds_per_bucket(), 1);
        assert_eq!(TfIndex::S30.seconds_per_bucket(), 30);
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
            "candles_1d",
            "candles_1s",
            "candles_2s",
            "candles_3s",
            "candles_4s",
            "candles_5s",
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
            // Appended 2026-08-10 with M2/M30/M60 — the three frames of the
            // operator's thirteen that previously had no enum variant.
            "candles_2m",
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
        let mut seen = std::collections::HashSet::new();
        let expected = [
            "1m", "3m", "5m", "15m", "1d", "1s", "2s", "3s", "4s", "5s", "6s", "7s", "8s", "9s",
            "10s", "11s", "12s", "13s", "14s", "15s",
            "30s", // Appended 2026-08-10 (operator Quote 13's thirteen frames).
            "2m", "30m", "60m",
        ];
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
        assert_eq!(TfIndex::D1.seconds_per_bucket(), 86_400);
        // Second-scale frames (C3): S1..S15 are 1..=15 s, S30 is 30 s.
        for ord in 5..=19_usize {
            let tf = TfIndex::from_ordinal(ord).expect("second-scale ordinal");
            assert_eq!(
                tf.seconds_per_bucket(),
                u32::try_from(ord - 4).expect("fits"),
                "S-frame seconds drifted at ordinal {ord}"
            );
        }
        assert_eq!(TfIndex::S30.seconds_per_bucket(), 30);
        // Every minute-class TF is a whole number of minutes.
        for tf in [TfIndex::M1, TfIndex::M3, TfIndex::M5, TfIndex::M15] {
            assert_eq!(tf.seconds_per_bucket() % 60, 0);
        }
    }

    #[test]
    fn test_tf_index_second_scale_gate_is_exactly_the_16_gdf_frames() {
        // GDF-gate predicate: exactly the 16 sub-minute frames (S1..S15 +
        // S30) are second-scale; the legacy 5-frame live set is NOT.
        let second_scale: Vec<TfIndex> = TfIndex::ALL
            .into_iter()
            .filter(|tf| tf.is_second_scale())
            .collect();
        assert_eq!(second_scale.len(), 16, "second-scale frame count drifted");
        for tf in [
            TfIndex::M1,
            TfIndex::M3,
            TfIndex::M5,
            TfIndex::M15,
            TfIndex::D1,
        ] {
            assert!(!tf.is_second_scale(), "{tf:?} wrongly GDF-gated");
        }
        for tf in second_scale {
            assert!(
                tf.seconds_per_bucket() < 60,
                "{tf:?} gated but not sub-minute"
            );
            assert!(
                tf.as_ordinal() >= 5,
                "{tf:?} gated frame in legacy ordinals"
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
    /// NOT seconds order: D1 (ordinal 4) precedes S1 (ordinal 5).
    #[test]
    fn test_tf_index_total_ordering_matches_ordinal_append_order() {
        let mut sorted = TfIndex::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, TfIndex::ALL);
    }

    /// The operator-requested set is EXACTLY the nine native frames of the
    /// 2026-09-18 directive.
    ///
    /// Both halves are named individually rather than counted. A count alone
    /// would pass if a requested frame were swapped for an unrequested one,
    /// and that is precisely the mistake available here: the enum's
    /// second-scale variants sit immediately adjacent to one another
    /// (`S2`/`S3`/`S4`, `S4`/`S5`/`S6`), so an off-by-one in either direction
    /// is a plausible edit that a length check would wave through — and the
    /// 2026-09-18 change is itself exactly such an edit, moving `S3` in and
    /// `S10`/`S15`/`S30`/`M2` out.
    ///
    /// The four that LEFT get their own assertion block rather than being
    /// folded in with the never-requested eleven, for the same reason `D1`
    /// has always had its own: "asked for, then withdrawn" and "never asked
    /// for" are different facts, and a reader restoring one of them needs to
    /// see which it is.
    #[test]
    fn tf_index_operator_set_is_the_operators_nine() {
        let requested: Vec<TfIndex> = TfIndex::ALL
            .iter()
            .copied()
            .filter(|tf| tf.is_operator_requested())
            .collect();

        assert_eq!(
            requested.len(),
            9,
            "operator directive 2026-09-18 named eleven entries: `ticks` (not a \
             candle frame), `10m` (DERIVED from candles_1m, no TfIndex variant) \
             and these NINE native frames. Found {}. Changing this set needs a \
             fresh dated quote.",
            requested.len()
        );

        // The nine that must emit rows.
        for tf in [
            TfIndex::S1,
            TfIndex::S3,
            TfIndex::S5,
            TfIndex::M1,
            TfIndex::M3,
            TfIndex::M5,
            TfIndex::M15,
            TfIndex::M30,
            TfIndex::M60,
        ] {
            assert!(
                tf.is_operator_requested(),
                "{tf:?} is one of the operator's nine and must emit rows"
            );
        }

        // Requested on 2026-08-08, WITHDRAWN on 2026-09-18. Their tables keep
        // every row already written — the DDL still creates all 24 names and
        // no populated table is ever dropped (SEBI retention). Only new rows
        // stop.
        for tf in [TfIndex::S10, TfIndex::S15, TfIndex::S30, TfIndex::M2] {
            assert!(
                !tf.is_operator_requested(),
                "{tf:?} was dropped from the emission set by the 2026-09-18 \
                 directive — restoring it needs a fresh dated quote"
            );
        }

        // D1 — removed 2026-08-25, for a DIFFERENT reason than the four
        // above: the operator ruled that a day bar must never be derived from
        // the internal fold at all, not merely that he no longer wants the
        // table.
        assert!(
            !TfIndex::D1.is_operator_requested(),
            "operator 2026-08-25: 1d must NEVER be derived from the internal \
             timeframe fold — the live lane must not emit candles_1d"
        );

        // The ten that exist but were NEVER asked for. `S3` left this list on
        // 2026-09-18; it is now in the requested nine above.
        for tf in [
            TfIndex::S2,
            TfIndex::S4,
            TfIndex::S6,
            TfIndex::S7,
            TfIndex::S8,
            TfIndex::S9,
            TfIndex::S11,
            TfIndex::S12,
            TfIndex::S13,
            TfIndex::S14,
        ] {
            assert!(
                !tf.is_operator_requested(),
                "{tf:?} was never requested — emitting it writes rows to disk \
                 every bucket for nobody"
            );
        }
    }
}
