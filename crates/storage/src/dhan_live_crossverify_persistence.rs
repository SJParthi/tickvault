//! Daily 15:31 IST Dhan LIVE-vs-REST cross-verification — audit persistence
//! (the revived Dhan live feed's ONLY ground-truth check).
//!
//! # Why this table is the most load-bearing audit surface in the feed
//!
//! The Dhan India live feed carries **no sequence number** and offers **no
//! snapshot-on-subscribe**, so packet loss is *undetectable at the protocol
//! level*. Comparing our aggregated `candles_1m` (`feed='dhan'`) against
//! Dhan's OWN official 1-minute tape (`/v2/charts/intraday`, the
//! `no-rest-except-live-feed-2026-06-27.md` §8 KEEP class) is therefore the
//! only ground truth available — the design (PR #1731) requires it live from
//! day one, not as a supplementary check.
//!
//! # The predecessor was BLIND SINCE BIRTH — and this schema is the answer
//!
//! The retired 15:31 cross-verify compared NANOSECOND literals against
//! QuestDB's MICROSECOND `TIMESTAMP` column, so its `WHERE` window sat around
//! **year 58502** and matched ZERO rows on every run it ever made. It
//! honestly reported `compared=0` — and *therefore never fired a mismatch
//! page*, for the entire life of the feature (fixed by PR #1474, commit
//! `f84b4398`; see `websocket-connection-scope-lock.md` §E row 4).
//!
//! Two structural defenses live here so that class cannot recur:
//!
//! 1. [`DhanLiveXverifyOutcome::Blind`] is a FIRST-CLASS, DISTINCT outcome.
//!    `compared == 0` with rows on either side can never render as
//!    [`DhanLiveXverifyOutcome::Clean`] — the `Clean` variant is only
//!    constructible from a `minutes_compared > 0` branch, and
//!    [`DhanLiveXverifyOutcome::is_pass`] returns `false` for every
//!    zero-comparison verdict (audit Rule 11: no false-OK).
//! 2. `outcome` is IN the daily DEDUP key + [`DhanLiveXverifyOutcome::is_measured`]
//!    powers a keep-better rerun guard, so a measured verdict is never erased
//!    by a later blind rerun.
//!
//! # Doctrine — NEITHER side is ground truth
//!
//! Our live capture is a conflated ~1/sec SAMPLED stream; Dhan's candle API is
//! their FULL tape. Divergence is EXPECTED and partly STRUCTURAL, not a bug
//! (`groww-second-feed-scope-2026-06-19.md` §37.5). Both sides' cells are
//! stored verbatim and the run row carries divergence-magnitude percentiles so
//! the operator can SEE what normal looks like before judging anything.
//!
//! Cold path, best-effort, once a day. Never on the tick hot path. Writes
//! ONLY the three tables below — NEVER `ticks`, `candles_*` or
//! `historical_candles` (the live-feed purity rule).
//!
//! # Schema
//!
//! ```text
//! CREATE TABLE IF NOT EXISTS dhan_live_crossverify_cell_audit (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, feed SYMBOL,
//!     security_id LONG, segment SYMBOL, minute_ts_ist TIMESTAMP,
//!     kind SYMBOL, field SYMBOL,
//!     live_value DOUBLE, rest_value DOUBLE,
//!     live_volume LONG, rest_volume LONG, diff_paise LONG,
//!     attempt_at TIMESTAMP
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(ts, trading_date_ist, feed, security_id, segment,
//!                   minute_ts_ist, kind, field)
//!
//! CREATE TABLE IF NOT EXISTS dhan_live_crossverify_daily (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, feed SYMBOL,
//!     instruments LONG, minutes_compared LONG, cells_diverged LONG,
//!     missing_live LONG, missing_live_traded LONG,
//!     missing_live_zero_volume LONG, missing_rest LONG,
//!     tail_unsealed LONG, out_of_session LONG,
//!     noise_p50_paise LONG, noise_p95_paise LONG, noise_max_paise LONG,
//!     tolerance_paise LONG, outcome SYMBOL,
//!     late_excused LONG, missing_live_unjudged LONG, missing_judgeable SYMBOL,
//!     attempt_at TIMESTAMP, run_complete BOOLEAN
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(ts, trading_date_ist, feed, outcome)
//!
//! CREATE TABLE IF NOT EXISTS dhan_rest_1m_tape (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, feed SYMBOL, source SYMBOL,
//!     security_id LONG, segment SYMBOL, instrument SYMBOL,
//!     open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE,
//!     volume LONG, fetched_at TIMESTAMP
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(ts, security_id, segment, feed, source)
//! ```
//!
//! ## 2026-10-06 (plan ITEM 51c, `no-rest-except-live-feed-2026-06-27.md` §12.15.9)
//!
//! The daily table gains `late_excused`, `missing_live_unjudged` and
//! `missing_judgeable`, and the cell audit gains the kinds `late_excused` and
//! `missing_live_unjudged`, all through the existing `ALTER ADD COLUMN IF NOT
//! EXISTS` self-heal; DEDUP keys are unchanged. The `tail_unsealed` column and
//! cell kind are KEPT for the rows written before this change and are written
//! 0 from it: the literal two-minute tail they recorded is replaced by the
//! derived late window, whose excused minutes are `late_excused`. A review fix
//! the same day adds `attempt_at` (daily and cell audit) and `run_complete`
//! (daily), same self-heal, in no DEDUP key.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table — one row per divergent / missing / unsealed / out-of-session cell.
pub const DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE: &str = "dhan_live_crossverify_cell_audit";
/// QuestDB table — one row per run (the divergence TREND + the keep-better target).
pub const DHAN_LIVE_XVERIFY_DAILY_TABLE: &str = "dhan_live_crossverify_daily";

/// QuestDB table — **the vendor's own minute candles, exactly as fetched.**
///
/// Added 2026-08-26 on operator instruction. Until today this comparison
/// fetched Dhan's tape, compared it in memory, and **threw it away**: only the
/// cells that DISAGREED survived, on the audit table above. Three consequences,
/// all of which the operator named:
///
/// 1. **No manual verification.** "What did Dhan say for this instrument at
///    09:16?" was unanswerable unless that minute happened to diverge.
/// 2. **No re-verification without re-fetching.** Re-running the comparison
///    meant ~868 more requests against a rate-limited vendor budget — so in
///    practice it was never re-run.
/// 3. **No other analysis.** The vendor's own record simply did not exist on
///    disk in any form.
///
/// Storing the pull turns the comparison into a pure function over two stored
/// tables: fetch once, compare as often as you like, offline.
pub const DHAN_REST_1M_TAPE_TABLE: &str = "dhan_rest_1m_tape";

/// The `source` in-key wire value for rows written by the daily sweep.
///
/// **This column exists because of a defect found the same day it was added.**
/// The sibling table `rest_spot_1m` carries a `source` column that is NOT in
/// its DEDUP key — so two legs writing the same instrument-minute from
/// different sources silently overwrite each other. Writing this sweep into
/// that table would have destroyed the per-minute leg's index rows with no
/// error anywhere. Here `source` is in the key from the first line, so the
/// same collision cannot be introduced later by a second writer.
pub const DHAN_REST_1M_TAPE_SOURCE_SWEEP: &str = "xverify_daily_sweep";

/// The `feed` in-key wire value (operator override 2026-06-28 — `feed` in the
/// DEDUP key of EVERY persisted table). Both sides of this comparison are
/// Dhan, so unlike the cross-BROKER comparators the honest label is the plain
/// feed id; the comparison scope (live WS vs official REST tape) is named by
/// the `side` semantics of each column, not by the feed tag.
pub const DHAN_LIVE_XVERIFY_FEED: &str = "dhan";

/// Cell-audit DEDUP key.
///
/// * Designated `ts` FIRST (the 2026-04-28 regression rule).
/// * `security_id` PAIRED with `segment` (I-P1-11 + `dedup_segment_meta_guard`)
///   — Dhan reuses numeric ids across segments, so an unpaired id would
///   silently collapse two instruments into one row.
/// * `feed` in-key (the 2026-06-28 feed-in-key lock).
/// * `field` + `kind` in-key so a diverged AND a missing finding on the same
///   minute both survive.
///
/// Declared as a real `const` deliberately: an inline literal evades the
/// `dedup_segment_meta_guard` scanner, which only reads `DEDUP_KEY_*`
/// declarations.
pub const DEDUP_KEY_DHAN_LIVE_XVERIFY_CELL_AUDIT: &str =
    "ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, kind, field";

/// Daily DEDUP key — `outcome` in-key so a MEASURED verdict is never erased in
/// place by a later blind/degraded rerun (the brutex/spot keep-better
/// precedent; the caller's keep-better guard additionally refuses to write the
/// downgrade at all).
pub const DEDUP_KEY_DHAN_LIVE_XVERIFY_DAILY: &str = "ts, trading_date_ist, feed, outcome";

/// Vendor-tape DEDUP key.
///
/// * Designated `ts` FIRST, and here `ts` IS the minute the candle belongs to
///   — so the table partitions and queries by market time, which is what a
///   human asking "what was the 09:16 price" actually wants.
/// * `security_id` PAIRED with `segment` (I-P1-11): Dhan reuses numeric ids
///   across segments, and an unpaired id would collapse two instruments.
/// * `feed` in-key (the 2026-06-28 feed-in-key lock).
/// * `source` in-key — see [`DHAN_REST_1M_TAPE_SOURCE_SWEEP`] for the defect
///   in the sibling table that this prevents.
///
/// Re-fetching the same minute overwrites it with the same values, so a rerun
/// is idempotent rather than duplicating the tape.
pub const DEDUP_KEY_DHAN_REST_1M_TAPE: &str = "ts, security_id, segment, feed, source";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format cell kind (the `kind` SYMBOL column + the
/// `tv_dhan_live_xverify_findings_total{kind}` counter label). Never reworded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DhanLiveXverifyCellKind {
    /// Both sides have the minute; an OHLC field differs beyond tolerance.
    Diverged,
    /// The minute is in Dhan's official REST tape but NOT in our live capture.
    ///
    /// This is the finding the whole subsystem exists for: with no sequence
    /// number on the wire, an absent live minute is the closest observable
    /// proxy for packet loss.
    MissingLive,
    /// The minute is in our live capture but not in Dhan's REST tape.
    MissingRest,
    /// A live-side gap in the LAST TWO session minutes at run time — the seal
    /// may legitimately not have landed by 15:31. Recorded, NEVER counted as
    /// `missing_live` and never a divergence.
    ///
    /// **2026-10-06 (§12.15.9): no longer written.** Kept so the rows written
    /// before that day still read; the derived late window writes
    /// [`Self::LateExcused`] instead.
    TailUnsealed,
    /// A traded or index minute missing from our side inside the derived
    /// end-of-session window (`LATE_SEAL_WINDOW_MINUTES`, or every bucket
    /// ending after the published seal progress), judged under the `Excuse`
    /// policy: it may simply not have sealed by the read. Recorded, never
    /// real, and it holds the day at `partial` at best (§12.15.9).
    LateExcused,
    /// A traded or index minute missing from our side on a run whose live
    /// read was cut short, so a missing minute cannot be told from an unread
    /// one. Recorded, never real, and it holds the day at `partial` at best
    /// (§12.15.9).
    MissingLiveUnjudged,
    /// A row outside `[09:15, 15:40)` IST — recorded, not classified.
    OutOfSession,
}

impl DhanLiveXverifyCellKind {
    /// Stable wire label (audit SYMBOL + counter label). Never reworded.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diverged => "diverged",
            Self::MissingLive => "missing_live",
            Self::MissingRest => "missing_rest",
            Self::TailUnsealed => "tail_unsealed",
            Self::LateExcused => "late_excused",
            Self::MissingLiveUnjudged => "missing_live_unjudged",
            Self::OutOfSession => "out_of_session",
        }
    }

    /// `true` for the kinds that constitute REAL divergence.
    ///
    /// `TailUnsealed`, `LateExcused`, `MissingLiveUnjudged` and `OutOfSession`
    /// are explicitly EXCLUDED — they are
    /// reported categories, never evidence of a feed problem (the doctrine:
    /// separate expected fluctuation from real divergence).
    #[must_use]
    pub const fn is_real_divergence(self) -> bool {
        matches!(self, Self::Diverged | Self::MissingLive | Self::MissingRest)
    }
}

/// Stable wire-format daily outcome (the `outcome` SYMBOL column + the
/// `tv_dhan_live_xverify_runs_total{status}` counter label). Never reworded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DhanLiveXverifyOutcome {
    /// `compared > 0`, zero real divergence, and nothing incomplete, excused
    /// or unjudged. The ONLY passing verdict.
    Clean,
    /// `compared > 0` and ≥1 real divergence: a price difference, or a judged,
    /// not-excused traded or index minute missing from our side. Nothing
    /// downgrades it (§12.15.9).
    Diverged,
    /// `compared > 0`, no real divergence, and something kept the day from
    /// `clean`: a failed fetch or spent budget, a truncated live read, a minute
    /// missing from the vendor (`missing_rest`), an excused late minute
    /// (`late_excused`) or an unjudged one (`missing_live_unjudged`).
    Partial,
    /// BOTH sides empty — a non-trading day or the feed was off. Info.
    NoData,
    /// Rows existed on at least one side, yet ZERO minutes overlapped.
    ///
    /// **This is the anti-#1474 outcome.** The retired cross-verify's
    /// nanosecond-vs-microsecond window produced exactly this state on every
    /// run and reported it as an unremarkable `compared=0`. Here it is a
    /// distinct, loud verdict that can never read as a pass.
    Blind,
    /// A run leg failed hard (read error, fetch failure, budget elapsed).
    Degraded,
}

impl DhanLiveXverifyOutcome {
    /// Stable wire label. Never reworded.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Diverged => "diverged",
            Self::Partial => "partial",
            Self::NoData => "no_data",
            Self::Blind => "blind",
            Self::Degraded => "degraded",
        }
    }

    /// `true` only for a verdict that actually COMPARED something and found
    /// nothing wrong.
    ///
    /// The single most important predicate in this module: `Blind`, `NoData`
    /// and `Degraded` all return `false`, so a zero-comparison run can never
    /// be rendered, counted or alerted as a pass (audit Rule 11 — no
    /// false-OK). Pinned by `blind_and_no_data_are_never_a_pass`.
    #[must_use]
    pub const fn is_pass(self) -> bool {
        matches!(self, Self::Clean)
    }

    /// `true` for a MEASURED verdict — one backed by ≥1 compared minute.
    /// The keep-better rerun guard refuses to overwrite one of these with a
    /// later `no_data` / `blind` / `degraded`.
    #[must_use]
    pub const fn is_measured(self) -> bool {
        matches!(self, Self::Clean | Self::Diverged | Self::Partial)
    }

    /// `true` when the run produced NO comparison at all and therefore proves
    /// nothing about feed health. Drives the loud non-vacuity log line.
    #[must_use]
    pub const fn is_vacuous(self) -> bool {
        matches!(self, Self::Blind | Self::NoData | Self::Degraded)
    }
}

/// Whether this run could judge minutes missing from our side (the
/// `missing_judgeable` SYMBOL column). Stable wire labels, never reworded.
///
/// Added 2026-10-06 (plan ITEM 51c, §12.15.9). A failed vendor fetch cannot
/// fake a missing LIVE minute (a target with no vendor bars adds only
/// `missing_rest`), so it never turns judging off. A live read cut at its row
/// cap can: it reads `ORDER BY ts ASC LIMIT`, so the cap drops the END of the
/// day and every minute after the cut would read as lost. That is the one
/// input that turns judging off today; plan item 51d adds the readiness
/// reasons. Until then a `judged` day can still count a minute as missing
/// whose sealed bar was not yet readable at the read (queued, spilled, or not
/// yet applied by QuestDB's WAL) or whose live row was skipped as malformed;
/// §12.15.9 records this under its Honest limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DhanLiveXverifyMissingJudgeable {
    /// Every missing traded or index minute outside the excused window is
    /// judged as real loss.
    Judged,
    /// The live read hit its row cap; no missing minute is judged.
    LiveTruncated,
}

impl DhanLiveXverifyMissingJudgeable {
    /// Stable wire label. Never reworded.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Judged => "judged",
            Self::LiveTruncated => "live_truncated",
        }
    }

    /// `true` only for [`Self::Judged`].
    #[must_use]
    pub const fn is_judged(self) -> bool {
        matches!(self, Self::Judged)
    }
}

/// One live-vs-REST cell finding, ready for ILP write.
///
/// Both sides' raw rupee values are retained verbatim (never rounded on the
/// way in) so the operator can inspect the actual numbers; `diff_paise` is the
/// integer comparison magnitude.
#[derive(Clone, Debug, PartialEq)]
pub struct DhanLiveXverifyCellFinding {
    /// Designated timestamp — the DETERMINISTIC run stamp (target trading day
    /// 15:31:00 IST, nanoseconds), regardless of when the run actually fired.
    /// Reruns UPSERT in place.
    pub run_ts_ist_nanos: i64,
    /// The verified trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Dhan `security_id` (paired with `segment` in the DEDUP key per I-P1-11).
    pub security_id: i64,
    /// Segment SYMBOL string (`IDX_I` / `NSE_FNO` / ...).
    pub segment: String,
    /// The compared minute — bucket OPEN time (IST nanoseconds).
    pub minute_ts_ist_nanos: i64,
    pub kind: DhanLiveXverifyCellKind,
    /// `open` / `high` / `low` / `close` / `none` (presence rows).
    pub field: &'static str,
    /// Our live-capture cell (rupees; `0.0` on presence rows).
    pub live_value: f64,
    /// Dhan's official REST cell (rupees; `0.0` on presence rows).
    pub rest_value: f64,
    /// Live volume, stored EXACTLY and never classified.
    pub live_volume: i64,
    /// REST volume, stored EXACTLY and never classified.
    pub rest_volume: i64,
    /// `|live - rest|` in paise (`0` on presence rows).
    pub diff_paise: i64,
}

/// One daily summary row, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct DhanLiveXverifyDailyRow {
    pub run_ts_ist_nanos: i64,
    pub trading_date_ist_nanos: i64,
    /// Distinct instruments seen on either side.
    pub instruments: i64,
    /// Minutes present on BOTH sides — the non-vacuity denominator.
    pub minutes_compared: i64,
    /// OHLC cells that differed beyond tolerance.
    pub cells_diverged: i64,
    pub missing_live: i64,
    /// Of `missing_live`, the minutes whose Dhan bar carried NON-ZERO volume
    /// — the exchange recorded trades and we hold no candle. A genuine gap.
    ///
    /// Split out 2026-08-26 because `missing_live` alone was unanswerable: at
    /// 31.2% of fetched minutes it could mean a catastrophe or a non-event,
    /// and one number could not tell them apart. **Uninformative for `IDX_I`**
    /// — an index has no volume, so every index bar lands in the zero bucket
    /// regardless.
    pub missing_live_traded: i64,
    /// Of `missing_live`, the minutes whose Dhan bar carried ZERO volume —
    /// nothing traded, and our fold correctly emits nothing for a bucket no
    /// tick opened. See [`Self::missing_live_traded`] for the IDX_I caveat.
    pub missing_live_zero_volume: i64,
    pub missing_rest: i64,
    pub tail_unsealed: i64,
    pub out_of_session: i64,
    /// Median divergence magnitude in paise — QUANTIFIES normal noise.
    pub noise_p50_paise: i64,
    /// 95th-percentile divergence magnitude in paise.
    pub noise_p95_paise: i64,
    /// Largest single divergence magnitude in paise.
    pub noise_max_paise: i64,
    /// The comparison tolerance actually applied (paise) — recorded so a
    /// later knob change is visible in the trend rather than silently
    /// re-baselining history.
    pub tolerance_paise: i64,
    pub outcome: DhanLiveXverifyOutcome,
    /// Traded or index minutes missing from our side inside the excused
    /// end-of-session window (cell kind `late_excused`, §12.15.9). Never real;
    /// any one holds the day at `partial` at best.
    pub late_excused: i64,
    /// Traded or index minutes missing from our side on a run that could not
    /// judge them (cell kind `missing_live_unjudged`, §12.15.9). Never real;
    /// any one holds the day at `partial` at best.
    pub missing_live_unjudged: i64,
    /// Whether missing minutes were judged on this run, and if not, why.
    pub missing_judgeable: DhanLiveXverifyMissingJudgeable,
    /// When the attempt that wrote this row persisted it (IST nanoseconds,
    /// one reading per attempt). Every attempt of a day writes its daily row at
    /// the same deterministic `ts`, and `outcome` is in the DEDUP key, so two
    /// attempts that read differently leave two rows: the row with the newest
    /// `attempt_at` is the day's latest verdict (§12.15.9 review). Not in the
    /// DEDUP key.
    pub attempt_at_ist_nanos: i64,
    /// `true` when neither the run budget nor the live read cap cut that
    /// attempt short and at most `MAX_MARKER_REST_FAILURE_PERCENT` (5%) of its
    /// vendor fetches failed (`run_is_complete`). An attempt that is not
    /// complete is retried, so its row is never the last word unless no later
    /// attempt ran.
    pub run_complete: bool,
}

/// One minute of the vendor's own tape, exactly as fetched.
///
/// Deliberately NOT a comparison result — this is the raw pull, stored before
/// any judgement is applied to it. That separation is the whole point: the
/// comparison becomes a pure function over stored data instead of a
/// side-effect of a network call that cannot be repeated.
#[derive(Debug, Clone, PartialEq)]
pub struct DhanRestTapeRow {
    /// The minute this candle belongs to — the DESIGNATED timestamp, so the
    /// table partitions by market time rather than by when we happened to ask.
    pub minute_ts_ist_nanos: i64,
    pub trading_date_ist_nanos: i64,
    pub security_id: i64,
    pub segment: String,
    /// The label we asked the vendor with, verbatim. See
    /// [`REST_1M_TAPE_COLUMNS`] for why this is worth a column.
    pub instrument: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    /// When we pulled it. Distinct from `minute_ts_ist_nanos`: the gap between
    /// them is how stale the vendor's own record was when we read it, and it
    /// is not derivable from anything else on the row.
    pub fetched_at_nanos: i64,
}

/// Idempotent `CREATE TABLE` DDL for the cell-audit table. Pure.
#[must_use]
pub fn dhan_live_xverify_cell_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            feed             SYMBOL, \
            security_id      LONG, \
            segment          SYMBOL, \
            minute_ts_ist    TIMESTAMP, \
            kind             SYMBOL, \
            field            SYMBOL, \
            live_value       DOUBLE, \
            rest_value       DOUBLE, \
            live_volume      LONG, \
            rest_volume      LONG, \
            diff_paise       LONG, \
            attempt_at       TIMESTAMP\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_DHAN_LIVE_XVERIFY_CELL_AUDIT});"
    )
}

/// Idempotent `CREATE TABLE` DDL for the daily run table. Pure.
#[must_use]
pub fn dhan_live_xverify_daily_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {DHAN_LIVE_XVERIFY_DAILY_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            feed             SYMBOL, \
            instruments      LONG, \
            minutes_compared LONG, \
            cells_diverged   LONG, \
            missing_live     LONG, \
            missing_live_traded LONG, \
            missing_live_zero_volume LONG, \
            missing_rest     LONG, \
            tail_unsealed    LONG, \
            out_of_session   LONG, \
            noise_p50_paise  LONG, \
            noise_p95_paise  LONG, \
            noise_max_paise  LONG, \
            tolerance_paise  LONG, \
            outcome          SYMBOL, \
            late_excused     LONG, \
            missing_live_unjudged LONG, \
            missing_judgeable SYMBOL, \
            attempt_at       TIMESTAMP, \
            run_complete     BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_DHAN_LIVE_XVERIFY_DAILY});"
    )
}

/// Every cell-audit column, for the idempotent `ALTER ADD COLUMN` self-heal.
/// Idempotent `CREATE TABLE` DDL for the vendor tape. Pure.
#[must_use]
pub fn dhan_rest_1m_tape_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {DHAN_REST_1M_TAPE_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            feed             SYMBOL, \
            source           SYMBOL, \
            security_id      LONG, \
            segment          SYMBOL, \
            instrument       SYMBOL, \
            open             DOUBLE, \
            high             DOUBLE, \
            low              DOUBLE, \
            close            DOUBLE, \
            volume           LONG, \
            fetched_at       TIMESTAMP\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_DHAN_REST_1M_TAPE});"
    )
}

/// Columns added by `ALTER ... ADD COLUMN IF NOT EXISTS` for the vendor tape.
///
/// `instrument` is the label we ASKED with, kept verbatim. It is the one
/// column that makes a wrong-label failure diagnosable after the fact: a
/// vendor "no candles" answer is identical for a mislabelled instrument and a
/// genuinely untraded one, and without the label there is nothing to compare
/// against the master afterwards.
const REST_1M_TAPE_COLUMNS: &[(&str, &str)] = &[
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("source", "SYMBOL"),
    ("security_id", "LONG"),
    ("segment", "SYMBOL"),
    ("instrument", "SYMBOL"),
    ("open", "DOUBLE"),
    ("high", "DOUBLE"),
    ("low", "DOUBLE"),
    ("close", "DOUBLE"),
    ("volume", "LONG"),
    ("fetched_at", "TIMESTAMP"),
];

const CELL_AUDIT_COLUMNS: &[(&str, &str)] = &[
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("security_id", "LONG"),
    ("segment", "SYMBOL"),
    ("minute_ts_ist", "TIMESTAMP"),
    ("kind", "SYMBOL"),
    ("field", "SYMBOL"),
    ("live_value", "DOUBLE"),
    ("rest_value", "DOUBLE"),
    ("live_volume", "LONG"),
    ("rest_volume", "LONG"),
    ("diff_paise", "LONG"),
    ("attempt_at", "TIMESTAMP"),
];

/// Every daily-row column, for the idempotent `ALTER ADD COLUMN` self-heal.
const DAILY_COLUMNS: &[(&str, &str)] = &[
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("instruments", "LONG"),
    ("minutes_compared", "LONG"),
    ("cells_diverged", "LONG"),
    ("missing_live", "LONG"),
    ("missing_live_traded", "LONG"),
    ("missing_live_zero_volume", "LONG"),
    ("missing_rest", "LONG"),
    ("tail_unsealed", "LONG"),
    ("out_of_session", "LONG"),
    ("noise_p50_paise", "LONG"),
    ("noise_p95_paise", "LONG"),
    ("noise_max_paise", "LONG"),
    ("tolerance_paise", "LONG"),
    ("outcome", "SYMBOL"),
    ("late_excused", "LONG"),
    ("missing_live_unjudged", "LONG"),
    ("missing_judgeable", "SYMBOL"),
    ("attempt_at", "TIMESTAMP"),
    ("run_complete", "BOOLEAN"),
];

/// The full ordered DDL statement list (CREATE → per-column `ALTER ADD COLUMN
/// IF NOT EXISTS` → `DEDUP ENABLE`). Never a drop. Pure, so the shape is unit
/// testable without a live QuestDB.
#[must_use]
pub fn dhan_live_xverify_ddl_statements() -> Vec<String> {
    let mut statements = vec![
        dhan_live_xverify_cell_audit_create_ddl(),
        dhan_live_xverify_daily_create_ddl(),
        dhan_rest_1m_tape_create_ddl(),
    ];
    for (col, ty) in CELL_AUDIT_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    for (col, ty) in DAILY_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {DHAN_LIVE_XVERIFY_DAILY_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    for (col, ty) in REST_1M_TAPE_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {DHAN_REST_1M_TAPE_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_DHAN_LIVE_XVERIFY_CELL_AUDIT});"
    ));
    statements.push(format!(
        "ALTER TABLE {DHAN_REST_1M_TAPE_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_DHAN_REST_1M_TAPE});"
    ));
    statements.push(format!(
        "ALTER TABLE {DHAN_LIVE_XVERIFY_DAILY_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_DHAN_LIVE_XVERIFY_DAILY});"
    ));
    statements
}

/// Create both audit tables if absent. Failures log at `error!` but NEVER
/// block: a failed ensure leaves the first ILP write to auto-create the table
/// WITHOUT dedup — a duplicate-row window until a later ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (the statement list is unit-tested via dhan_live_xverify_ddl_statements).
pub async fn ensure_dhan_live_crossverify_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            metrics::counter!(
                "tv_dhan_live_xverify_query_failures_total",
                "stage" => "ensure_client_build"
            )
            .increment(1);
            error!(
                code = ErrorCode::DhanLiveXverify01RunDegraded.code_str(),
                comparator = "dhan_live_vs_dhan_rest",
                stage = "ensure_client_build",
                ?err,
                "dhan_live_crossverify: HTTP client build failed — audit tables \
                 not ensured (first ILP write may auto-create them WITHOUT dedup)"
            );
            return;
        }
    };
    for ddl in &dhan_live_xverify_ddl_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                metrics::counter!(
                    "tv_dhan_live_xverify_query_failures_total",
                    "stage" => "ensure_ddl"
                )
                .increment(1);
                error!(
                    code = ErrorCode::DhanLiveXverify01RunDegraded.code_str(),
                    comparator = "dhan_live_vs_dhan_rest",
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "dhan_live_crossverify: DDL returned non-2xx (dedup may be \
                     missing — duplicate-row window until a later ensure)"
                );
            }
            Err(err) => {
                metrics::counter!(
                    "tv_dhan_live_xverify_query_failures_total",
                    "stage" => "ensure_ddl"
                )
                .increment(1);
                error!(
                    code = ErrorCode::DhanLiveXverify01RunDegraded.code_str(),
                    comparator = "dhan_live_vs_dhan_rest",
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "dhan_live_crossverify: DDL request failed"
                );
            }
        }
    }
}

/// The writer's ILP-over-HTTP request timeout, in milliseconds.
pub const DHAN_LIVE_XVERIFY_ILP_REQUEST_TIMEOUT_MS: u64 = 5_000;

/// The writer's ILP-over-HTTP minimum throughput, bytes per second. The
/// client lets one flush run `request_timeout + bytes / this` before it gives
/// up (questdb-rs 6.1.0, `sender/mod.rs`). 102,400 is the library default,
/// written out so [`DhanLiveXverifyAuditWriter::flush_worst_case`] cannot
/// drift from what the client does (2026-10-06, §12.15.8).
pub const DHAN_LIVE_XVERIFY_ILP_MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 102_400;

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson) + the shadow-writer knobs.
fn dhan_live_xverify_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout={};\
         request_min_throughput={};",
        config.host,
        config.http_port,
        DHAN_LIVE_XVERIFY_ILP_REQUEST_TIMEOUT_MS,
        DHAN_LIVE_XVERIFY_ILP_MIN_THROUGHPUT_BYTES_PER_SEC
    )
}

/// Lazy ILP-over-HTTP writer for the two cross-verify tables.
///
/// An unreachable QuestDB at construction still builds (rows buffer locally);
/// `flush` returns `Err` — including server-side rejects via the HTTP ACK —
/// and the pending buffer is DISCARDED on failure (poisoned-buffer defense:
/// findings are recomputable and reruns are DEDUP-idempotent).
pub struct DhanLiveXverifyAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl DhanLiveXverifyAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via for_test()).
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seed the discard series at zero BEFORE the first row can be dropped.
        //
        // A counter never incremented is ABSENT from the exporter, not zero,
        // and an absent series reads as health -- when it may mean the single
        // discard episode is the only sample there will ever be. If this name
        // is ever EMF-selected the CloudWatch agent's dropped-first-sample rule
        // turns that into total silence on the one episode that matters; that
        // rule cost 104,540 depth rows their classification on 2026-08-28.
        //
        // Only the DHAN name is seeded. The Groww sibling sits on a branch with
        // no production caller since the 2026-08-21 feed removal, and a
        // permanently-zero series for a path that cannot write implies a live
        // path that was deleted -- the false-OK this seeding exists to prevent.
        metrics::counter!("tv_dhan_live_xverify_audit_rows_discarded_total").increment(0);
        let conf = dhan_live_xverify_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "dhan_live_crossverify writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Test-only byte length of the ILP buffer — the quantity the 2026-08-26
    /// loss was measured in (207,965,278 against a 104,857,600 cap).
    #[cfg(test)]
    fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Appends one cell finding row. Symbols BEFORE columns (the ILP
    /// tags-before-fields rule).
    ///
    /// `attempt_at_ist_nanos` is one reading per attempt (§12.15.9 review): a
    /// finding an earlier attempt wrote and a later one did not keeps the
    /// earlier stamp, so the day's latest spot findings are the rows whose
    /// `attempt_at` equals the latest daily row's. The §12.15.6 option pass
    /// takes its own reading and writes no daily row, so its cells carry that
    /// pass's stamp; and a later attempt that stops after flushing some cells
    /// but before its daily row restamps those findings with no daily row to
    /// match; an attempt that loses a cell batch in a failed flush and still
    /// writes its daily row comes back short too, and `run_complete` does not
    /// reflect the persist (all recorded in §12.15.9's Honest limits).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_cell(
        &mut self,
        f: &DhanLiveXverifyCellFinding,
        attempt_at_ist_nanos: i64,
    ) -> Result<()> {
        self.buffer
            .table(DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE)
            .context("table")?
            .symbol("feed", DHAN_LIVE_XVERIFY_FEED)
            .context("feed")?
            .symbol("segment", f.segment.as_str())
            .context("segment")?
            .symbol("kind", f.kind.as_str())
            .context("kind")?
            .symbol("field", f.field)
            .context("field")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(f.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", f.security_id)
            .context("security_id")?
            .column_ts("minute_ts_ist", TimestampNanos::new(f.minute_ts_ist_nanos))
            .context("minute_ts_ist")?
            .column_f64("live_value", f.live_value)
            .context("live_value")?
            .column_f64("rest_value", f.rest_value)
            .context("rest_value")?
            .column_i64("live_volume", f.live_volume)
            .context("live_volume")?
            .column_i64("rest_volume", f.rest_volume)
            .context("rest_volume")?
            .column_i64("diff_paise", f.diff_paise)
            .context("diff_paise")?
            .column_ts("attempt_at", TimestampNanos::new(attempt_at_ist_nanos))
            .context("attempt_at")?
            .at(TimestampNanos::new(f.run_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Appends one row of the vendor's own tape.
    ///
    /// Symbols BEFORE columns (the ILP tags-before-fields rule).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_rest_tape(&mut self, r: &DhanRestTapeRow) -> Result<()> {
        self.buffer
            .table(DHAN_REST_1M_TAPE_TABLE)
            .context("table")?
            .symbol("feed", DHAN_LIVE_XVERIFY_FEED)
            .context("feed")?
            .symbol("source", DHAN_REST_1M_TAPE_SOURCE_SWEEP)
            .context("source")?
            .symbol("segment", r.segment.as_str())
            .context("segment")?
            .symbol("instrument", r.instrument.as_str())
            .context("instrument")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_f64("open", r.open)
            .context("open")?
            .column_f64("high", r.high)
            .context("high")?
            .column_f64("low", r.low)
            .context("low")?
            .column_f64("close", r.close)
            .context("close")?
            .column_i64("volume", r.volume)
            .context("volume")?
            .column_ts("fetched_at", TimestampNanos::new(r.fetched_at_nanos))
            .context("fetched_at")?
            // The MINUTE is the designated timestamp, not the run time. A
            // human asking "what was the 09:16 price" is asking about market
            // time; stamping the run time would file every candle of the day
            // under one instant and make that question unanswerable.
            .at(TimestampNanos::new(r.minute_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Appends the daily summary row.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_daily(&mut self, r: &DhanLiveXverifyDailyRow) -> Result<()> {
        self.buffer
            .table(DHAN_LIVE_XVERIFY_DAILY_TABLE)
            .context("table")?
            .symbol("feed", DHAN_LIVE_XVERIFY_FEED)
            .context("feed")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            .symbol("missing_judgeable", r.missing_judgeable.as_str())
            .context("missing_judgeable")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("instruments", r.instruments)
            .context("instruments")?
            .column_i64("minutes_compared", r.minutes_compared)
            .context("minutes_compared")?
            .column_i64("cells_diverged", r.cells_diverged)
            .context("cells_diverged")?
            .column_i64("missing_live", r.missing_live)
            .context("missing_live")?
            .column_i64("missing_live_traded", r.missing_live_traded)
            .context("missing_live_traded")?
            .column_i64("missing_live_zero_volume", r.missing_live_zero_volume)
            .context("missing_live_zero_volume")?
            .column_i64("missing_rest", r.missing_rest)
            .context("missing_rest")?
            .column_i64("tail_unsealed", r.tail_unsealed)
            .context("tail_unsealed")?
            .column_i64("out_of_session", r.out_of_session)
            .context("out_of_session")?
            .column_i64("noise_p50_paise", r.noise_p50_paise)
            .context("noise_p50_paise")?
            .column_i64("noise_p95_paise", r.noise_p95_paise)
            .context("noise_p95_paise")?
            .column_i64("noise_max_paise", r.noise_max_paise)
            .context("noise_max_paise")?
            .column_i64("tolerance_paise", r.tolerance_paise)
            .context("tolerance_paise")?
            .column_i64("late_excused", r.late_excused)
            .context("late_excused")?
            .column_i64("missing_live_unjudged", r.missing_live_unjudged)
            .context("missing_live_unjudged")?
            .column_ts("attempt_at", TimestampNanos::new(r.attempt_at_ist_nanos))
            .context("attempt_at")?
            .column_bool("run_complete", r.run_complete)
            .context("run_complete")?
            .at(TimestampNanos::new(r.run_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flush threshold for the streaming append path.
    ///
    /// # The live loss this exists to stop (MEASURED 2026-08-26, not modelled)
    ///
    /// `persist_xverify_report` appended EVERY finding and then flushed once.
    /// On 2026-08-26 that was 764,003 rows, and the single flush was refused:
    ///
    /// ```text
    /// Could not flush buffer: Buffer size of 207965278 exceeds maximum
    /// configured allowed size of 104857600 bytes.
    /// ```
    ///
    /// The poisoned-buffer defence then discarded all 764,003 — so the revived
    /// feed's ONE ground-truth record has no row for that day, and
    /// `WS-GAP-03 source=xverify_failed` fired saying exactly that.
    ///
    /// It is not a one-off. The comparison ran fine on 2026-08-25 at 201,353
    /// findings; the universe grew and the count went 3.8x in a single day, so
    /// every session from here produces a buffer over the cap.
    ///
    /// **The cruellest part was the daily SUMMARY row.** It is appended after
    /// the cells and flushed with them, so an oversized cell buffer destroys
    /// the one row that records a comparison happened at all — losing the
    /// detail AND the evidence of the loss in the same refusal. Chunking fixes
    /// that as a side effect: the cells drain in pieces, and the summary meets
    /// a nearly-empty buffer.
    ///
    /// 32 MiB against the 100 MiB server cap is ~3x headroom, so a row growing
    /// wider does not silently re-approach the limit; at the measured ~272
    /// bytes/row that is ~123,000 rows per flush, about 6 flushes for a
    /// 2026-08-26-sized session.
    pub const FLUSH_THRESHOLD_BYTES: usize = 32 * 1024 * 1024;

    /// Flush ONLY if the buffer has grown past [`Self::FLUSH_THRESHOLD_BYTES`].
    ///
    /// Called per-append by streaming producers so the buffer is bounded by
    /// the threshold rather than by the total row count. Cheap when it does
    /// nothing: one `len()` compare.
    ///
    /// On failure the poisoned-buffer defence still applies — that chunk is
    /// discarded and the error returned — but the buffer is then EMPTY, so a
    /// caller that keeps going loses one chunk instead of the whole run. That
    /// is a deliberate change in failure shape: all-or-nothing gave "no rows
    /// today", which is at least honest; chunked gives "most rows today",
    /// which is better ONLY IF the shortfall is reported. The caller must
    /// count these and mark the run degraded — never round a partial write up
    /// to success.
    ///
    /// # Errors
    /// Propagates [`Self::flush`], including its discard.
    pub fn flush_if_large(&mut self) -> Result<()> {
        if self.buffer.len() < Self::FLUSH_THRESHOLD_BYTES {
            return Ok(());
        }
        self.flush()
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending buffer is DISCARDED (poisoned-buffer defense);
    /// the caller stamps the run degraded — never a fabricated clean verdict.
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (pending discarded).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "dhan_live_crossverify: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending row(s) discarded (recomputable, DEDUP-idempotent rerun)"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "dhan_live_crossverify ILP flush failed — {dropped} pending row(s) \
                     discarded (poisoned-buffer defense; rerun is DEDUP-idempotent)"
                )))
            }
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!(
                    "dhan_live_crossverify: ILP sender vanished — {dropped} row(s) discarded"
                );
            }
        }
    }

    /// The longest one [`Self::flush`] of the current buffer can block: the
    /// request timeout plus the buffer's bytes at the minimum throughput, as
    /// the ILP client computes it, rounded up to a whole millisecond. With
    /// `retry_timeout=0` there is no retry on top. A caller with a deadline
    /// starts a flush only when `now + this` is still before it (2026-10-06,
    /// §12.15.8). O(1): one `len()` and integer arithmetic.
    #[must_use]
    pub fn flush_worst_case(&self) -> std::time::Duration {
        let bytes = u64::try_from(self.buffer.len()).unwrap_or(u64::MAX);
        let extra_ms = bytes
            .saturating_mul(1_000)
            .div_ceil(DHAN_LIVE_XVERIFY_ILP_MIN_THROUGHPUT_BYTES_PER_SEC);
        std::time::Duration::from_millis(
            DHAN_LIVE_XVERIFY_ILP_REQUEST_TIMEOUT_MS.saturating_add(extra_ms),
        )
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded count; counted by
    /// `tv_dhan_live_xverify_audit_rows_discarded_total` so a discard is
    /// never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_dhan_live_xverify_audit_rows_discarded_total")
                .increment(dropped as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }

    /// Drop every buffered-but-unflushed row because the caller chose to stop
    /// writing, not because a write failed (2026-10-06, §12.15.8: the
    /// cross-verification persist stops itself at its attempt's deadline).
    /// Returns the dropped count; counted on the local-only
    /// [`DHAN_LIVE_XVERIFY_AUDIT_ROWS_ABANDONED_COUNTER`], never on
    /// `tv_dhan_live_xverify_audit_rows_discarded_total`, which is a §2.10
    /// `audit_rows` member and pages: a deliberate stop of a recomputable
    /// write is not a lost audit row. The caller must log the stop. O(1).
    pub fn abandon_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!(DHAN_LIVE_XVERIFY_AUDIT_ROWS_ABANDONED_COUNTER)
                .increment(dropped as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

/// Rows [`DhanLiveXverifyAuditWriter::abandon_pending`] dropped on purpose.
/// Local `/metrics` only: no EMF name, filter or alarm reads it (§12.15.8).
pub const DHAN_LIVE_XVERIFY_AUDIT_ROWS_ABANDONED_COUNTER: &str =
    "tv_dhan_live_xverify_audit_rows_abandoned_total";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cell() -> DhanLiveXverifyCellFinding {
        DhanLiveXverifyCellFinding {
            run_ts_ist_nanos: 1,
            trading_date_ist_nanos: 2,
            security_id: 13,
            segment: "IDX_I".to_string(),
            minute_ts_ist_nanos: 3,
            kind: DhanLiveXverifyCellKind::Diverged,
            field: "high",
            live_value: 100.5,
            rest_value: 100.55,
            live_volume: 0,
            rest_volume: 0,
            diff_paise: 5,
        }
    }

    fn sample_daily() -> DhanLiveXverifyDailyRow {
        DhanLiveXverifyDailyRow {
            run_ts_ist_nanos: 1,
            trading_date_ist_nanos: 2,
            instruments: 4,
            minutes_compared: 375,
            cells_diverged: 2,
            missing_live: 1,
            missing_live_traded: 1,
            missing_live_zero_volume: 0,
            missing_rest: 0,
            tail_unsealed: 2,
            out_of_session: 0,
            noise_p50_paise: 3,
            noise_p95_paise: 25,
            noise_max_paise: 60,
            tolerance_paise: 0,
            outcome: DhanLiveXverifyOutcome::Diverged,
            late_excused: 4,
            missing_live_unjudged: 0,
            missing_judgeable: DhanLiveXverifyMissingJudgeable::Judged,
            attempt_at_ist_nanos: 3,
            run_complete: false,
        }
    }

    // -- The non-vacuity contract (the #1474 lesson) -------------------------

    /// THE anti-#1474 test. The retired cross-verify compared zero rows on
    /// every run and that state was indistinguishable from success. Here,
    /// every zero-comparison verdict must be a NON-pass — exhaustively.
    #[test]
    fn is_pass_is_measured_and_is_vacuous_reject_every_zero_comparison_verdict() {
        for outcome in [
            DhanLiveXverifyOutcome::Blind,
            DhanLiveXverifyOutcome::NoData,
            DhanLiveXverifyOutcome::Degraded,
        ] {
            assert!(
                !outcome.is_pass(),
                "{} must NEVER render as a pass — this is exactly how the \
                 retired 15:31 cross-verify stayed blind since birth",
                outcome.as_str()
            );
            assert!(
                !outcome.is_measured(),
                "{} compared nothing, so it cannot be a measured verdict",
                outcome.as_str()
            );
            assert!(
                outcome.is_vacuous(),
                "{} proves nothing about feed health and must be flagged vacuous",
                outcome.as_str()
            );
        }
        // Clean is the ONLY pass.
        assert!(DhanLiveXverifyOutcome::Clean.is_pass());
        assert!(!DhanLiveXverifyOutcome::Diverged.is_pass());
        assert!(!DhanLiveXverifyOutcome::Partial.is_pass());
        // ...and the measured set is exactly the compared>0 set.
        for outcome in [
            DhanLiveXverifyOutcome::Clean,
            DhanLiveXverifyOutcome::Diverged,
            DhanLiveXverifyOutcome::Partial,
        ] {
            assert!(outcome.is_measured());
            assert!(!outcome.is_vacuous());
        }
    }

    #[test]
    fn outcome_as_str_wire_labels_are_stable() {
        assert_eq!(DhanLiveXverifyOutcome::Clean.as_str(), "clean");
        assert_eq!(DhanLiveXverifyOutcome::Diverged.as_str(), "diverged");
        assert_eq!(DhanLiveXverifyOutcome::Partial.as_str(), "partial");
        assert_eq!(DhanLiveXverifyOutcome::NoData.as_str(), "no_data");
        assert_eq!(DhanLiveXverifyOutcome::Blind.as_str(), "blind");
        assert_eq!(DhanLiveXverifyOutcome::Degraded.as_str(), "degraded");
    }

    #[test]
    fn cell_kind_as_str_stable_and_is_real_divergence_excludes_expected_kinds() {
        assert_eq!(DhanLiveXverifyCellKind::Diverged.as_str(), "diverged");
        assert_eq!(
            DhanLiveXverifyCellKind::MissingLive.as_str(),
            "missing_live"
        );
        assert_eq!(
            DhanLiveXverifyCellKind::MissingRest.as_str(),
            "missing_rest"
        );
        assert_eq!(
            DhanLiveXverifyCellKind::TailUnsealed.as_str(),
            "tail_unsealed"
        );
        assert_eq!(
            DhanLiveXverifyCellKind::OutOfSession.as_str(),
            "out_of_session"
        );

        assert!(DhanLiveXverifyCellKind::Diverged.is_real_divergence());
        assert!(DhanLiveXverifyCellKind::MissingLive.is_real_divergence());
        assert!(DhanLiveXverifyCellKind::MissingRest.is_real_divergence());
        // The doctrine: expected fluctuation is NOT divergence.
        assert!(
            !DhanLiveXverifyCellKind::TailUnsealed.is_real_divergence(),
            "an unsealed 15:28/15:29 minute at 15:31 is expected, not a fault"
        );
        assert!(!DhanLiveXverifyCellKind::OutOfSession.is_real_divergence());
    }

    // -- DEDUP key contract (I-P1-11 + feed-in-key) --------------------------

    #[test]
    fn cell_dedup_key_pairs_security_id_with_segment_and_carries_feed_and_ts() {
        let key = DEDUP_KEY_DHAN_LIVE_XVERIFY_CELL_AUDIT;
        assert!(key.contains("security_id"), "identity must be in the key");
        assert!(
            key.contains("segment"),
            "I-P1-11: Dhan reuses security_id across segments — an unpaired \
             id silently collapses two instruments into one row"
        );
        assert!(key.contains("feed"), "feed-in-key lock 2026-06-28");
        assert!(
            key.starts_with("ts,"),
            "designated timestamp must lead the DEDUP key"
        );
        // kind + field so a diverged and a missing finding on the same minute
        // both survive.
        assert!(key.contains("kind"));
        assert!(key.contains("field"));
        assert!(key.contains("minute_ts_ist"));
    }

    #[test]
    fn daily_dedup_key_carries_ts_feed_and_outcome() {
        let key = DEDUP_KEY_DHAN_LIVE_XVERIFY_DAILY;
        assert!(key.starts_with("ts,"));
        assert!(key.contains("feed"));
        assert!(
            key.contains("outcome"),
            "outcome in-key so a measured verdict is not erased by a blind rerun"
        );
        assert!(
            !key.contains("security_id"),
            "the daily row is per-run, not per-instrument"
        );
    }

    // -- DDL ------------------------------------------------------------------

    #[test]
    fn dhan_live_xverify_cell_audit_create_ddl_has_every_column_and_key() {
        let ddl = dhan_live_xverify_cell_audit_create_ddl();
        for tok in [
            "ts",
            "trading_date_ist",
            "feed",
            "security_id",
            "segment",
            "minute_ts_ist",
            "kind",
            "field",
            "live_value",
            "rest_value",
            "live_volume",
            "rest_volume",
            "diff_paise",
        ] {
            assert!(ddl.contains(tok), "cell DDL missing `{tok}`: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        assert!(ddl.contains(DEDUP_KEY_DHAN_LIVE_XVERIFY_CELL_AUDIT));
    }

    #[test]
    fn dhan_live_xverify_daily_create_ddl_has_every_column_and_key() {
        let ddl = dhan_live_xverify_daily_create_ddl();
        for tok in [
            "instruments",
            "minutes_compared",
            "cells_diverged",
            "missing_live",
            "missing_rest",
            "tail_unsealed",
            "out_of_session",
            "noise_p50_paise",
            "noise_p95_paise",
            "noise_max_paise",
            "tolerance_paise",
            "outcome",
            "late_excused",
            "missing_live_unjudged",
            "missing_judgeable",
        ] {
            assert!(ddl.contains(tok), "daily DDL missing `{tok}`: {ddl}");
        }
        assert!(ddl.contains(DEDUP_KEY_DHAN_LIVE_XVERIFY_DAILY));
    }

    /// The DDL list must be self-healing (CREATE, then per-column ALTER ADD
    /// COLUMN IF NOT EXISTS, then DEDUP ENABLE) and must NEVER drop.
    #[test]
    fn dhan_live_xverify_ddl_statements_are_idempotent_and_never_drop() {
        let statements = dhan_live_xverify_ddl_statements();

        // Expressed PER TABLE rather than as one total, so adding a table
        // needs no literal bumped here — and, more usefully, a table that
        // arrives with a CREATE but no DEDUP ENABLE fails loudly instead of
        // sliding under a total that still adds up. An auto-created table has
        // no dedup at all, which is a silent duplicate-row window.
        let tables: &[(&str, usize)] = &[
            (DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE, CELL_AUDIT_COLUMNS.len()),
            (DHAN_LIVE_XVERIFY_DAILY_TABLE, DAILY_COLUMNS.len()),
            (DHAN_REST_1M_TAPE_TABLE, REST_1M_TAPE_COLUMNS.len()),
        ];
        let mut expected = 0usize;
        for (table, columns) in tables {
            let creates = statements
                .iter()
                .filter(|s| s.contains("CREATE TABLE IF NOT EXISTS") && s.contains(table))
                .count();
            assert_eq!(creates, 1, "{table}: exactly one CREATE");
            let dedups = statements
                .iter()
                .filter(|s| s.contains("DEDUP ENABLE") && s.contains(table))
                .count();
            assert_eq!(
                dedups, 1,
                "{table}: exactly one DEDUP ENABLE — without it the table \
                 auto-creates on first write with no dedup at all"
            );
            let alters = statements
                .iter()
                .filter(|s| s.contains("ADD COLUMN IF NOT EXISTS") && s.contains(table))
                .count();
            assert_eq!(alters, *columns, "{table}: one ALTER per declared column");
            expected += 2 + columns;
        }
        assert_eq!(
            statements.len(),
            expected,
            "every statement must belong to a declared table — a stray one \
             means a table was half-registered"
        );
        for s in &statements {
            assert!(
                !s.to_ascii_uppercase().contains("DROP"),
                "self-heal DDL must never drop: {s}"
            );
            assert!(
                s.contains("IF NOT EXISTS") || s.contains("DEDUP ENABLE"),
                "every statement must be idempotent: {s}"
            );
        }
        assert!(statements[0].contains(DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE));
        assert!(statements[1].contains(DHAN_LIVE_XVERIFY_DAILY_TABLE));
        // DEDUP ENABLE lands last so the columns exist first.
        assert!(statements[statements.len() - 2].contains("DEDUP ENABLE"));
        assert!(statements[statements.len() - 1].contains("DEDUP ENABLE"));
    }

    /// Every column named in the CREATE DDL must also have an ALTER
    /// self-heal entry, or an old table would never gain it.
    #[test]
    fn every_created_column_has_an_alter_selfheal_entry() {
        let cell_ddl = dhan_live_xverify_cell_audit_create_ddl();
        for (col, _) in CELL_AUDIT_COLUMNS {
            assert!(
                cell_ddl.contains(col),
                "ALTER column `{col}` absent from the cell CREATE DDL"
            );
        }
        let daily_ddl = dhan_live_xverify_daily_create_ddl();
        for (col, _) in DAILY_COLUMNS {
            assert!(
                daily_ddl.contains(col),
                "ALTER column `{col}` absent from the daily CREATE DDL"
            );
        }
    }

    // -- Live-feed purity ----------------------------------------------------

    /// This module must never write a market-data table. The tables it owns
    /// are audit-only.
    #[test]
    fn tables_are_audit_only_never_market_data() {
        for table in [
            DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE,
            DHAN_LIVE_XVERIFY_DAILY_TABLE,
        ] {
            assert!(table.starts_with("dhan_live_crossverify_"));
            for forbidden in ["ticks", "candles_1m", "historical_candles"] {
                assert_ne!(
                    table, forbidden,
                    "live-feed purity: the comparator may only write its own audit tables"
                );
            }
        }
    }

    // -- ILP writer ----------------------------------------------------------

    #[test]
    fn new_writer_is_lazy_and_empty() {
        let w = DhanLiveXverifyAuditWriter::for_test();
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
    }

    #[test]
    fn append_cell_emits_symbols_before_columns_and_counts_pending() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell(), 7).expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.contains(DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE));
        assert!(line.contains("feed=dhan"));
        assert!(line.contains("segment=IDX_I"));
        assert!(line.contains("kind=diverged"));
        assert!(line.contains("field=high"));
        assert!(line.contains("security_id=13"));
        assert!(line.contains("diff_paise=5"));
        // ILP requires every tag (symbol) before the first field.
        let first_field = line.find(' ').expect("space between tags and fields");
        let tags = &line[..first_field];
        assert!(tags.contains("feed=dhan"), "symbols must precede columns");
        assert!(!tags.contains("live_value"), "columns must follow symbols");
    }

    /// 2026-10-06 (plan ITEM 51c, §12.15.9). The three new daily columns
    /// reach an already-created table through the ALTER self-heal with the
    /// type the CREATE declares, the ILP line carries them with the matching
    /// wire type (a symbol for `missing_judgeable`, integers for the counts),
    /// the two new cell kinds have stable labels, and no DEDUP key moved.
    #[test]
    fn late_excused_and_unjudged_columns_self_heal_and_dedup_keys_unchanged() {
        let statements = dhan_live_xverify_ddl_statements();
        let create = dhan_live_xverify_daily_create_ddl();
        for (col, ty) in [
            ("late_excused", "LONG"),
            ("missing_live_unjudged", "LONG"),
            ("missing_judgeable", "SYMBOL"),
        ] {
            let alter = format!(
                "ALTER TABLE {DHAN_LIVE_XVERIFY_DAILY_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
            );
            assert!(statements.contains(&alter), "missing self-heal: {alter}");
            assert!(
                DAILY_COLUMNS.contains(&(col, ty)),
                "{col} must be in the daily manifest as {ty}"
            );
            assert!(create.contains(col), "CREATE must declare {col}");
        }
        // DEDUP keys are unchanged by this change (and by any new column).
        assert_eq!(
            DEDUP_KEY_DHAN_LIVE_XVERIFY_DAILY,
            "ts, trading_date_ist, feed, outcome"
        );
        assert_eq!(
            DEDUP_KEY_DHAN_LIVE_XVERIFY_CELL_AUDIT,
            "ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, kind, field"
        );
        // The wire.
        let mut row = sample_daily();
        row.late_excused = 4;
        row.missing_live_unjudged = 7;
        row.missing_judgeable = DhanLiveXverifyMissingJudgeable::LiveTruncated;
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_daily(&row).expect("append");
        let line = w.buffer_utf8();
        let first_field = line.find(' ').expect("space between tags and fields");
        let (tags, fields) = line.split_at(first_field);
        assert!(
            tags.contains("missing_judgeable=live_truncated"),
            "missing_judgeable is a SYMBOL, so it must ride with the tags: {line}"
        );
        assert!(fields.contains("late_excused=4i"), "{line}");
        assert!(fields.contains("missing_live_unjudged=7i"), "{line}");
        // tail_unsealed is still written (0 from this change on).
        row.tail_unsealed = 0;
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_daily(&row).expect("append");
        assert!(w.buffer_utf8().contains("tail_unsealed=0i"));
        // Stable labels.
        assert_eq!(
            DhanLiveXverifyCellKind::LateExcused.as_str(),
            "late_excused"
        );
        assert_eq!(
            DhanLiveXverifyCellKind::MissingLiveUnjudged.as_str(),
            "missing_live_unjudged"
        );
        assert_eq!(DhanLiveXverifyMissingJudgeable::Judged.as_str(), "judged");
        assert_eq!(
            DhanLiveXverifyMissingJudgeable::LiveTruncated.as_str(),
            "live_truncated"
        );
        assert!(DhanLiveXverifyMissingJudgeable::Judged.is_judged());
        assert!(!DhanLiveXverifyMissingJudgeable::LiveTruncated.is_judged());
    }

    /// 2026-10-06 (§12.15.9 review). Every attempt of a day writes its daily
    /// row at the same deterministic `ts` and `outcome` is in the DEDUP key,
    /// so a retry that reads differently leaves a second row. `attempt_at`
    /// (one reading per attempt, on the daily row AND every cell) and
    /// `run_complete` (daily) let a reader pick the latest attempt and its
    /// own findings. Both reach an old table through the ALTER self-heal with
    /// the CREATE's type, ride the ILP line as fields with the matching wire
    /// type, and stay OUT of every DEDUP key: in the key they would turn each
    /// retry's rows into new rows instead of upserting the same finding.
    #[test]
    fn attempt_at_and_run_complete_self_heal_reach_the_wire_and_stay_out_of_dedup() {
        let statements = dhan_live_xverify_ddl_statements();
        let daily = dhan_live_xverify_daily_create_ddl();
        let cell = dhan_live_xverify_cell_audit_create_ddl();
        for (table, ddl, columns, col, ty) in [
            (
                DHAN_LIVE_XVERIFY_DAILY_TABLE,
                &daily,
                DAILY_COLUMNS,
                "attempt_at",
                "TIMESTAMP",
            ),
            (
                DHAN_LIVE_XVERIFY_DAILY_TABLE,
                &daily,
                DAILY_COLUMNS,
                "run_complete",
                "BOOLEAN",
            ),
            (
                DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE,
                &cell,
                CELL_AUDIT_COLUMNS,
                "attempt_at",
                "TIMESTAMP",
            ),
        ] {
            let alter = format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {col} {ty};");
            assert!(statements.contains(&alter), "missing self-heal: {alter}");
            assert!(columns.contains(&(col, ty)), "{table}.{col} must be {ty}");
            assert!(
                ddl.split_whitespace()
                    .collect::<Vec<_>>()
                    .windows(2)
                    .any(|w| w[0] == col && w[1].trim_end_matches([',', ')']) == ty),
                "{table} CREATE must declare {col} {ty}: {ddl}"
            );
        }
        for key in [
            DEDUP_KEY_DHAN_LIVE_XVERIFY_DAILY,
            DEDUP_KEY_DHAN_LIVE_XVERIFY_CELL_AUDIT,
        ] {
            assert!(!key.contains("attempt_at"), "{key}");
            assert!(!key.contains("run_complete"), "{key}");
        }

        // The wire: a TIMESTAMP field is `<micros>t`, a BOOLEAN is `t`/`f`,
        // and both are fields, never tags.
        let mut row = sample_daily();
        row.attempt_at_ist_nanos = 1_759_751_460_123_456_000;
        row.run_complete = true;
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_daily(&row).expect("append");
        let line = w.buffer_utf8();
        let (tags, fields) = line.split_at(line.find(' ').expect("tags then fields"));
        assert!(!tags.contains("attempt_at") && !tags.contains("run_complete"));
        assert!(fields.contains("attempt_at=1759751460123456t"), "{line}");
        assert!(fields.contains("run_complete=t"), "{line}");
        row.run_complete = false;
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_daily(&row).expect("append");
        assert!(w.buffer_utf8().contains("run_complete=f"));

        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell(), 1_759_751_460_123_456_000)
            .expect("append");
        let line = w.buffer_utf8();
        let (tags, fields) = line.split_at(line.find(' ').expect("tags then fields"));
        assert!(!tags.contains("attempt_at"));
        assert!(fields.contains("attempt_at=1759751460123456t"), "{line}");
    }

    #[test]
    fn append_daily_writes_the_outcome_symbol_and_noise_stats() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_daily(&sample_daily()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.contains(DHAN_LIVE_XVERIFY_DAILY_TABLE));
        assert!(line.contains("outcome=diverged"));
        assert!(line.contains("minutes_compared=375"));
        assert!(line.contains("noise_p50_paise=3"));
        assert!(line.contains("noise_p95_paise=25"));
        assert!(line.contains("noise_max_paise=60"));
        assert!(line.contains("tolerance_paise=0"));
        assert!(line.contains("tail_unsealed=2"));
    }

    /// The 2026-08-26 loss, as a test.
    ///
    /// That session produced 764,003 findings, the single flush built a
    /// 207,965,278-byte buffer against the server's 104,857,600 cap, and the
    /// poisoned-buffer defence discarded every row — the revived feed's only
    /// ground truth had no record for the day.
    ///
    /// `for_test()` has no sender, so a flush that is genuinely REACHED must
    /// fail — and that failure is what makes this test meaningful rather than
    /// decorative. It appends until the buffer is over the threshold, then
    /// asserts `flush_if_large` actually TRIED to send (Err, buffer emptied by
    /// the poisoned-buffer defence). Under the old shape the buffer just kept
    /// growing, so no call at any size could have produced that.
    ///
    /// The small-run half matters too: below the threshold the call must be a
    /// no-op, because on the live path reaching a flush per row would turn one
    /// round trip into hundreds of thousands.
    #[test]
    fn flush_if_large_bounds_the_buffer_by_bytes_not_row_count() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        // Well under the threshold: nothing should be offered for flush.
        for _ in 0..50 {
            w.append_cell(&sample_cell(), 7).expect("append");
        }
        assert!(
            w.buffer_len() < DhanLiveXverifyAuditWriter::FLUSH_THRESHOLD_BYTES,
            "a small run must not trigger a flush"
        );
        assert!(
            w.flush_if_large().is_ok(),
            "below the threshold this is a no-op even with no sender — it must \
             not be reached, because reaching it would try to send"
        );
        assert_eq!(w.pending(), 50, "a no-op flush must not clear pending");

        // Now cross it for real. The append loop is bounded by BYTES, exactly
        // as the production caller is, so this cannot silently spin if the row
        // width changes.
        while w.buffer_len() < DhanLiveXverifyAuditWriter::FLUSH_THRESHOLD_BYTES {
            w.append_cell(&sample_cell(), 7).expect("append");
        }
        let rows_at_threshold = w.pending();
        assert!(
            w.flush_if_large().is_err(),
            "past the threshold the flush must be REACHED — with no sender that \
             surfaces as Err. An Ok here would mean the threshold is never \
             acted on and the buffer grows without bound, which is the \
             2026-08-26 defect"
        );
        assert_eq!(
            w.pending(),
            0,
            "a reached flush that fails must discard its chunk (poisoned-buffer \
             defence), leaving the buffer empty for the next chunk — not \
             carrying {rows_at_threshold} poisoned rows forward into it"
        );
    }

    /// The threshold has to leave real headroom under the server cap, or the
    /// fix re-creates the bug the first time a row gets wider.
    #[test]
    fn the_flush_threshold_leaves_headroom_under_the_server_cap() {
        // The cap named verbatim in the 2026-08-26 failure.
        const OBSERVED_SERVER_CAP_BYTES: usize = 104_857_600;
        assert!(
            DhanLiveXverifyAuditWriter::FLUSH_THRESHOLD_BYTES * 3 <= OBSERVED_SERVER_CAP_BYTES,
            "the flush threshold must sit at least 3x under the server's \
             configured max buffer, so a wider row cannot silently re-approach \
             the limit that discarded 764,003 rows on 2026-08-26"
        );
        assert!(
            DhanLiveXverifyAuditWriter::FLUSH_THRESHOLD_BYTES > 1024 * 1024,
            "too small a threshold turns one flush into thousands of round \
             trips on the daily comparator"
        );
    }

    #[test]
    fn flush_with_no_pending_rows_is_ok() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        assert!(w.flush().is_ok(), "an empty flush is a no-op, not an error");
    }

    #[test]
    fn flush_without_sender_errors_and_discards_the_poisoned_buffer() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell(), 7).expect("append");
        w.append_daily(&sample_daily()).expect("append");
        assert_eq!(w.pending(), 2);
        let err = w.flush().expect_err("disconnected writer must fail loudly");
        assert!(
            err.to_string().contains("discarded"),
            "the failure must name the discard: {err}"
        );
        assert_eq!(w.pending(), 0, "poisoned buffer must be dropped");
        assert!(w.buffer_utf8().is_empty());
    }

    #[test]
    fn discard_pending_returns_the_dropped_count_and_clears() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell(), 7).expect("append");
        w.append_cell(&sample_cell(), 7).expect("append");
        assert_eq!(w.discard_pending(), 2);
        assert_eq!(w.pending(), 0);
        assert_eq!(w.discard_pending(), 0, "discarding twice is a no-op");
    }

    /// A deliberate stop drops the buffer like a discard, but counts on its
    /// own local counter, never on the paging discard counter (§12.15.8).
    #[test]
    fn abandon_pending_returns_the_dropped_count_clears_and_never_counts_as_discarded() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell(), 7).expect("append");
        w.append_daily(&sample_daily()).expect("append");
        assert_eq!(w.abandon_pending(), 2);
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
        assert_eq!(w.abandon_pending(), 0, "abandoning twice is a no-op");
        let src = include_str!("dhan_live_crossverify_persistence.rs");
        let body = src
            .split("pub fn abandon_pending(")
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .expect("abandon_pending exists");
        assert!(body.contains("DHAN_LIVE_XVERIFY_AUDIT_ROWS_ABANDONED_COUNTER"));
        assert!(!body.contains("rows_discarded_total"));
        assert_eq!(
            DHAN_LIVE_XVERIFY_AUDIT_ROWS_ABANDONED_COUNTER,
            "tv_dhan_live_xverify_audit_rows_abandoned_total"
        );
    }

    #[test]
    fn ilp_conf_targets_the_configured_questdb_http_port() {
        let cfg = QuestDbConfig {
            host: "questdb.internal".to_string(),
            http_port: 9009,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = dhan_live_xverify_ilp_http_conf(&cfg);
        assert!(conf.contains("http::addr=questdb.internal:9009"));
        assert!(conf.contains("retry_timeout=0"));
        assert!(conf.contains("request_timeout=5000;"));
        assert!(conf.contains("request_min_throughput=102400;"));
        // The conf must parse: a typo here is a writer with no sender, which
        // discards every row.
        assert!(Sender::from_conf(&conf).is_ok());
    }

    /// The worst case follows the ILP client's own formula: 5 s, plus the
    /// buffer's bytes at 102,400 B/s, rounded up to a millisecond.
    #[test]
    fn test_flush_worst_case_is_timeout_plus_bytes_at_min_throughput() {
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        assert_eq!(w.flush_worst_case(), std::time::Duration::from_secs(5));
        w.append_daily(&sample_daily()).expect("append");
        let bytes = u64::try_from(w.buffer_len()).expect("len");
        assert!(bytes > 0);
        let want_ms = 5_000 + (bytes * 1_000).div_ceil(102_400);
        assert_eq!(
            w.flush_worst_case(),
            std::time::Duration::from_millis(want_ms)
        );
        // 20,000 rows at the measured ~272 B/row is ~53 s on top of the 5 s.
        assert!(w.flush_worst_case() > std::time::Duration::from_secs(5));
    }

    /// `is_measured` gates the keep-better rerun guard: get it wrong and a
    /// blind rerun silently overwrites a day's real evidence.
    #[test]
    fn test_is_measured_is_exactly_the_compared_greater_than_zero_set() {
        // Measured == a real comparison happened.
        assert!(DhanLiveXverifyOutcome::Clean.is_measured());
        assert!(DhanLiveXverifyOutcome::Diverged.is_measured());
        assert!(DhanLiveXverifyOutcome::Partial.is_measured());
        // Not measured == nothing was compared.
        assert!(!DhanLiveXverifyOutcome::Blind.is_measured());
        assert!(!DhanLiveXverifyOutcome::NoData.is_measured());
        assert!(!DhanLiveXverifyOutcome::Degraded.is_measured());
        // is_measured and is_vacuous must partition the space exactly.
        for o in [
            DhanLiveXverifyOutcome::Clean,
            DhanLiveXverifyOutcome::Diverged,
            DhanLiveXverifyOutcome::Partial,
            DhanLiveXverifyOutcome::Blind,
            DhanLiveXverifyOutcome::NoData,
            DhanLiveXverifyOutcome::Degraded,
        ] {
            assert_ne!(
                o.is_measured(),
                o.is_vacuous(),
                "{} must be either measured or vacuous, never both or neither",
                o.as_str()
            );
            // And a pass is always a subset of measured.
            if o.is_pass() {
                assert!(o.is_measured(), "a pass must be a measured verdict");
            }
        }
    }

    /// `is_vacuous` is what the caller reads to log the loud
    /// "this run proved nothing" line. Every zero-comparison verdict must be
    /// vacuous — that is the whole anti-blind-since-birth contract.
    #[test]
    fn test_is_vacuous_covers_every_zero_comparison_verdict() {
        for o in [
            DhanLiveXverifyOutcome::Blind,
            DhanLiveXverifyOutcome::NoData,
            DhanLiveXverifyOutcome::Degraded,
        ] {
            assert!(o.is_vacuous(), "{} compared nothing", o.as_str());
            assert!(!o.is_pass());
        }
        for o in [
            DhanLiveXverifyOutcome::Clean,
            DhanLiveXverifyOutcome::Diverged,
            DhanLiveXverifyOutcome::Partial,
        ] {
            assert!(!o.is_vacuous(), "{} compared something", o.as_str());
        }
    }

    /// `is_real_divergence` is the doctrine boundary: it separates EXPECTED
    /// fluctuation (an unsealed tail minute, an out-of-session row) from an
    /// actual feed problem. Widening it would manufacture false alarms;
    /// narrowing it would hide real loss.
    #[test]
    fn test_is_real_divergence_separates_expected_fluctuation_from_real_faults() {
        // Real: a price disagreement, or a minute one side never had.
        assert!(DhanLiveXverifyCellKind::Diverged.is_real_divergence());
        assert!(
            DhanLiveXverifyCellKind::MissingLive.is_real_divergence(),
            "a minute Dhan's own tape has but we never captured is the closest \
             proxy we have for packet loss — it MUST count"
        );
        assert!(DhanLiveXverifyCellKind::MissingRest.is_real_divergence());
        // Expected: recorded for the operator, never counted as a fault.
        assert!(
            !DhanLiveXverifyCellKind::TailUnsealed.is_real_divergence(),
            "15:28/15:29 may legitimately be unsealed at 15:31"
        );
        assert!(
            !DhanLiveXverifyCellKind::OutOfSession.is_real_divergence(),
            "a row outside [09:15, 15:40) is recorded, never classified"
        );
        // Exactly three of the seven kinds are real.
        // 2026-10-06 (§12.15.9): an excused late minute and an unjudged
        // minute are recorded, never real.
        assert!(!DhanLiveXverifyCellKind::LateExcused.is_real_divergence());
        assert!(!DhanLiveXverifyCellKind::MissingLiveUnjudged.is_real_divergence());
        let kinds = [
            DhanLiveXverifyCellKind::Diverged,
            DhanLiveXverifyCellKind::MissingLive,
            DhanLiveXverifyCellKind::MissingRest,
            DhanLiveXverifyCellKind::TailUnsealed,
            DhanLiveXverifyCellKind::LateExcused,
            DhanLiveXverifyCellKind::MissingLiveUnjudged,
            DhanLiveXverifyCellKind::OutOfSession,
        ];
        assert_eq!(kinds.iter().filter(|k| k.is_real_divergence()).count(), 3);
    }

    // -----------------------------------------------------------------
    // THE VENDOR TAPE — added 2026-08-26 on operator instruction
    //
    // Until today the fetched tape was compared in memory and thrown away:
    // only DISAGREEMENTS survived. So "what did Dhan say at 09:16?" had no
    // answer, and re-verifying meant ~868 more rate-limited requests.
    // -----------------------------------------------------------------

    fn tape_row() -> DhanRestTapeRow {
        DhanRestTapeRow {
            minute_ts_ist_nanos: 1_786_233_600 * 1_000_000_000 + 33_360 * 1_000_000_000,
            trading_date_ist_nanos: 1_786_233_600 * 1_000_000_000,
            security_id: 25,
            segment: "NSE_EQ".to_string(),
            instrument: "RELIANCE".to_string(),
            open: 1414.2,
            high: 1420.0,
            low: 1410.5,
            close: 1418.75,
            volume: 91_240,
            fetched_at_nanos: 1_786_233_600 * 1_000_000_000 + 56_460 * 1_000_000_000,
        }
    }

    #[test]
    fn the_tape_dedup_key_carries_source() {
        // THE DEFECT THIS TABLE EXISTS TO AVOID.
        //
        // The sibling `rest_spot_1m` has a `source` column that is NOT in its
        // dedup key. Two legs writing the same instrument-minute from
        // different sources therefore overwrite each other with no error
        // anywhere — which is why this sweep could not simply be written
        // there. Dropping `source` from this key re-creates that trap.
        assert!(
            DEDUP_KEY_DHAN_REST_1M_TAPE.contains("source"),
            "source must be in the key, or a second writer silently overwrites \
             this one — exactly the collision that forced a new table"
        );
    }

    #[test]
    fn the_tape_dedup_key_pairs_security_id_with_segment() {
        // I-P1-11: Dhan reuses numeric ids across segments. An unpaired id
        // collapses two different instruments into one row.
        let k = DEDUP_KEY_DHAN_REST_1M_TAPE;
        assert!(k.contains("security_id") && k.contains("segment"));
        assert!(k.contains("feed"), "the 2026-06-28 feed-in-key lock");
    }

    #[test]
    fn dhan_rest_1m_tape_create_ddl_is_idempotent_day_partitioned_and_deduped() {
        let ddl = dhan_rest_1m_tape_create_ddl();
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS "));
        assert!(ddl.contains(DHAN_REST_1M_TAPE_TABLE));
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_DHAN_REST_1M_TAPE})")));
        assert!(!ddl.to_ascii_uppercase().contains("DROP"));
    }

    #[test]
    fn append_rest_tape_stamps_the_minute_not_the_run() {
        // If `ts` were the run time, every candle of the day would file under
        // one instant and "what was the 09:16 price" becomes unanswerable —
        // which is the question this table was added to answer.
        assert!(
            DEDUP_KEY_DHAN_REST_1M_TAPE.starts_with("ts,"),
            "designated ts first (the 2026-04-28 regression rule)"
        );
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        let r = tape_row();
        w.append_rest_tape(&r).expect("append");
        let wire = w.buffer_utf8();
        assert!(
            wire.contains(&r.minute_ts_ist_nanos.to_string()),
            "the minute must be the row's designated timestamp"
        );
    }

    #[test]
    fn the_tape_row_carries_the_label_we_asked_with() {
        // A vendor "no candles" answer looks identical for a MISLABELLED
        // instrument and a genuinely untraded one. Without the label stored,
        // that ambiguity can never be resolved after the fact.
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        w.append_rest_tape(&tape_row()).expect("append");
        assert!(w.buffer_utf8().contains("RELIANCE"));
    }

    #[test]
    fn the_tape_ddl_is_registered_so_the_table_actually_gets_created() {
        // A writer whose table is never created auto-creates it on first
        // write WITHOUT dedup — a duplicate-row window the whole key design
        // above would silently miss.
        let ddl = dhan_live_xverify_ddl_statements();
        let joined = ddl.join("\n");
        assert!(
            joined.contains(DHAN_REST_1M_TAPE_TABLE),
            "the tape table must be in the DDL list, or nothing creates it"
        );
        assert!(
            joined.contains(&format!(
                "ALTER TABLE {DHAN_REST_1M_TAPE_TABLE} DEDUP ENABLE"
            )),
            "dedup must be enabled explicitly — an auto-created table has none"
        );
    }

    #[test]
    fn the_tape_write_is_idempotent_by_construction() {
        // A rerun re-fetches the same minutes. With the key above those
        // overwrite in place rather than duplicating the tape, so re-running
        // the comparison is free of side effects.
        let mut a = DhanLiveXverifyAuditWriter::for_test();
        let mut b = DhanLiveXverifyAuditWriter::for_test();
        a.append_rest_tape(&tape_row()).expect("a");
        b.append_rest_tape(&tape_row()).expect("b");
        assert_eq!(
            a.buffer_utf8(),
            b.buffer_utf8(),
            "the same input must produce byte-identical rows, or the dedup key \
             cannot collapse them"
        );
    }

    #[test]
    fn the_tape_table_is_not_the_sibling_table() {
        // Guard against a future "simplification" that points this writer at
        // rest_spot_1m. That table's key omits `source`, so the sweep would
        // silently destroy the per-minute leg's index rows.
        assert_ne!(DHAN_REST_1M_TAPE_TABLE, "rest_spot_1m");
        assert_ne!(DHAN_REST_1M_TAPE_TABLE, DHAN_LIVE_XVERIFY_CELL_AUDIT_TABLE);
    }
}
