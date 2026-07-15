//! Daily timeframe-consistency verifier (operator directive 2026-07-13:
//! *"how will you guarantee that all our defined timeframes internally are
//! correct — how do you identify whether any miscalculation or data
//! issues"*).
//!
//! At **15:40 IST** every trading day (after the Dhan 15:30:05 close-time
//! force-seal + writer drain and the 15:31 cross-verify burst, before the
//! 15:45 scoreboard), recompute every stored higher-timeframe candle — the
//! 19 TFs `2m..4h` (`TfIndex::ALL` minus `M1` the baseline minus `D1`,
//! which is excluded by design: Dhan drops D1 at the write boundary per
//! `live-feed-purity.md` rule 10, and Groww D1 is a partial-day
//! midnight-sealed bucket) — from its constituent `candles_1m` rows and
//! compare EXACTLY (integer-paise OHLC, exact i64 volume).
//!
//! Two passes per run:
//! - `feed='dhan'` verifies **TODAY** (amend-frozen after the close seal);
//! - `feed='groww'` verifies the **PREVIOUS trading day**. Groww has NO
//!   close-time force-seal — its session-tail buckets would only seal at
//!   the IST-midnight force-seal, which NEVER RUNS on the prod schedule
//!   (the box auto-stops at 16:30 IST and the bridge resumes from a
//!   persisted NDJSON offset, so the tails are never rebuilt). The Groww
//!   catch-up seal additionally requires a window end E ≤ watermark −
//!   60s with a max pre-close watermark of 15:29:59, so EVERY window
//!   whose effective end lands within [`GROWW_CATCHUP_MARGIN_SECS`] of
//!   the 15:30:00 close is STRUCTURALLY absent for Groww — every final
//!   window PLUS e.g. the 2m penultimate `[15:27, 15:29)` window
//!   (refuter round 2, 2026-07-13). A missing un-catch-up-able tail row
//!   takes the NON-PAGING `tail_unsealed` carve-out (counter only — no
//!   audit row, no page); a PRESENT tail row is sealed data and is
//!   compared normally; earlier Groww windows keep full paging
//!   classification. Dhan finals are covered by the 15:30:05 close-time
//!   force-seal and are never carved out.
//!
//! The bucket grid is REIMPLEMENTED here independently (windows
//! `[33_300 + k*S, min(+S, 55_800))` per trading day) and cross-pinned
//! against `TfIndex::bucket_start` by a hand-literal TRIPWIRE test below —
//! the parts of the aggregator that COULD be miscalculated (anchoring,
//! floor arithmetic) are exactly what this verifier must check, so it never
//! calls `bucket_start` in production code.
//!
//! Findings land in the `tf_consistency_audit` table (feed-keyed DEDUP,
//! deterministic run ts = target day 15:40:00 IST ⇒ reruns UPSERT in
//! place), counters `tv_tf_verify_*`, ONE coalesced TF-VERIFY-01 `error!`
//! per (feed, date) pass with ≥1 paging finding, and ONE
//! `TfConsistencySummary` Telegram per run. Cold path only — reads
//! `candles_*`, writes ONLY its own audit table (live-feed purity).
//!
//! Runbook: `.claude/rules/project/tf-consistency-error-codes.md`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::NaiveDate;
use tracing::{error, info, warn};

use tickvault_common::config::{ApplicationConfig, QuestDbConfig};
use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::segment::{segment_code_to_str, segment_str_to_code};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::tf_consistency_audit_persistence::{
    FindingCategory, TfConsistencyAuditWriter, TfConsistencyFinding,
    ensure_tf_consistency_audit_table,
};
use tickvault_trading::candles::TfIndex;

/// IST seconds-of-day of the daily trigger (15:40:00) — after the Dhan
/// 15:30:05 close-time force-seal (+ ~100ms drain cadence) and the 15:31
/// cross-verify burst, before the 15:45 scoreboard and the 16:30 auto-stop.
pub const TF_VERIFY_TRIGGER_SECS_OF_DAY_IST: u32 = 15 * 3600 + 40 * 60; // 56_400

/// Hard wall-clock budget for the whole run (both passes). Checked between
/// SIDs; on exceed the remaining SIDs count as read-degraded and the run
/// classifies Degraded — worst case ends well before the 16:30 auto-stop.
pub const TF_VERIFY_RUN_BUDGET_SECS: u64 = 900;

/// Per-request HTTP timeout against the local QuestDB `/exec` endpoint.
const TF_VERIFY_HTTP_TIMEOUT_SECS: u64 = 15;

/// Response-body byte cap on every `/exec` read — checked BEFORE the
/// serde_json parse (stage `oversize`), so a hostile / runaway response
/// can never be fed to the JSON parser. Sized well above any legitimate
/// bounded query result (≤3,000 rows).
pub const TF_VERIFY_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Explicit SQL `LIMIT` on the per-SID `candles_1m` day query. Sized above
/// the theoretical max (375 rows/day) so a healthy day can never touch it;
/// `returned == limit` is the truncation TRIPWIRE (read-degraded, never a
/// silent partial compare — QuestDB's `/exec` default row cap is
/// unverified-live, so this design refuses to depend on it either way).
pub const TF_VERIFY_1M_ROW_LIMIT: usize = 500;

/// Explicit SQL `LIMIT` on the per-SID 19-way higher-TF UNION query. Sized
/// above the arithmetic worst case (Σ per-TF daily bucket counts = 903).
pub const TF_VERIFY_TF_UNION_ROW_LIMIT: usize = 2_000;

/// Explicit SQL `LIMIT` on the per-feed instrument-discovery query. Sized
/// above the `MAX_DAILY_UNIVERSE_SIZE = 1200` envelope.
pub const TF_VERIFY_DISCOVERY_ROW_LIMIT: usize = 3_000;

/// Audit-row blast-radius cap per run: a systemic bug (e.g. an anchoring
/// change) would otherwise produce ~1M rows in one run. Beyond the cap the
/// counts stay exact but only counters/summary carry them (`truncated`).
pub const TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN: usize = 10_000;

/// Politeness: yield to the runtime + sleep briefly every N SIDs so the
/// once-a-day sweep never monopolizes the box next to the 15:45 scoreboard.
const TF_VERIFY_POLITENESS_EVERY_SIDS: usize = 25;
const TF_VERIFY_POLITENESS_SLEEP_MS: u64 = 5;

/// Max plain-English offender lines carried into the Telegram summary.
const TF_VERIFY_TOP_DETAIL_MAX: usize = 10;

/// Max sample `security_id segment` pairs named by the coalesced
/// data-derived out-of-session exclusion warn (refuter round 2 —
/// self-disarm visibility; one warn per pass, never per instrument).
const TF_VERIFY_OUT_OF_SESSION_SAMPLE_MAX: usize = 5;

/// How far back the Groww previous-trading-day walk searches before the
/// pass degrades loudly (a >7-day gap has never occurred on NSE).
pub const TF_VERIFY_PREV_DAY_LOOKBACK_DAYS: u32 = 7;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// 09:15:00 IST as seconds-of-day. Compile-time drift-pinned against the
/// canonical common-crate G1 gate constants — editing either representation
/// alone fails the build (the `tf_index.rs` session-constant discipline).
const SESSION_OPEN_SECS_OF_DAY_IST: u32 = 33_300;
/// 15:30:00 IST as seconds-of-day (exclusive session close).
const SESSION_CLOSE_SECS_OF_DAY_IST: u32 = 55_800;
const _: () = assert!(
    SESSION_OPEN_SECS_OF_DAY_IST as i64 * NANOS_PER_SEC == MARKET_OPEN_IST_NANOS,
    "session open drifted from the canonical common-crate constant"
);
const _: () = assert!(
    SESSION_CLOSE_SECS_OF_DAY_IST as i64 * NANOS_PER_SEC == MARKET_CLOSE_IST_NANOS,
    "session close drifted from the canonical common-crate constant"
);

/// The Groww aggregator's catch-up-seal lateness margin (refuter round 2,
/// 2026-07-13). The Groww catch-up seal only seals a window whose end E
/// satisfies E ≤ watermark − margin, and the max pre-close watermark is
/// 15:29:59 — so any window whose effective end lands within this margin
/// of the 15:30:00 close can NEVER seal intraday on the prod schedule
/// (e.g. the 2m penultimate `[15:27, 15:29)` window, E = 55_740, plus
/// every final window). Mirrors
/// `tickvault_trading::candles::CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW`
/// (= 60) — equality tripwire-pinned in the tests below.
pub const GROWW_CATCHUP_MARGIN_SECS: u32 = 60;

/// Task slug for the once-per-trading-day delivery marker
/// (`data/state/daily/tf-consistency-YYYY-MM-DD.marker` — Telegram
/// cleanliness overhaul, coordinator-relayed directive 2026-07-15). ONLY the
/// `RunCatchUp` arm consults it; the real 15:40 fire (`SleepThenRun`) and
/// forced runs (`RunNow`) never do, and it is written ONLY after a `pass`
/// summary was dispatched — a FAILURE-class day re-runs AND re-pages on
/// restart (Rule 11), and a `no_data` day no longer seals it either (G4a,
/// fix round 2: a trading-day no_data can be a silently-empty discovery
/// dataset — the PR #1474 blind-since-birth class — so a same-day restart
/// must re-run, not read "nothing to check" forever).
const TF_CONSISTENCY_MARKER_TASK: &str = "tf-consistency";

/// Pure notify predicate for the daily summary: a `no_data` day ("nothing to
/// check" — feeds were off) is LOG-ONLY, never a Telegram card; every other
/// verdict (pass / mismatch / degraded / blind) notifies.
///
/// F4 (2026-07-15 fix round): a FORCED run (`TICKVAULT_TF_VERIFY_NOW=1`)
/// ALWAYS notifies, `no_data` included — the catch-up skip message itself
/// tells the operator to force a run, and a forced run answering with
/// silence would be a Rule-11 hole. Scheduled/catch-up runs keep `no_data`
/// log-only.
///
/// MERGE-NOTE (G9a, fix round 2 — collision site 1 of 2 with the in-flight
/// `claude/groww-live-off-rest-only` branch): that branch adds its OWN
/// `pub(crate) fn should_notify_summary(status_label: &str) -> bool` —
/// DIFFERENT arity, and its notify arm keeps the original un-tupled
/// JoinHandle shape. This is NOT a trivial union: git conflicts on both
/// sites, and resolving toward the 1-arg version silently LOSES the F4
/// forced-run Rule-11 fix with no test failing on the surviving side.
/// RESOLUTION RULE for whichever branch lands second: keep THIS 2-arg
/// forced-aware version — forced runs always notify. (Site 2 is the
/// `Ok(Ok(Some((s, forced_run))))` notify arm below; see also the plan
/// file's "Merge coordination" section.)
#[must_use]
pub fn should_notify_summary(status_label: &str, forced_run: bool) -> bool {
    forced_run || status_label != "no_data"
}

// ---------------------------------------------------------------------------
// Scheduling (pure)
// ---------------------------------------------------------------------------

/// Decision for WHEN the daily timeframe-consistency run should fire.
/// Late-boot = `RunCatchUp` (the scoreboard idiom — the data only gets MORE
/// stable after 15:40, and the deterministic run ts makes reruns
/// DEDUP-idempotent), NOT the cross-verify `SkipPastTrigger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TfVerifyStart {
    /// Not a trading day and not forced → do not run.
    SkipNonTradingDay,
    /// Operator forced an on-demand run (`TICKVAULT_TF_VERIFY_NOW`).
    RunNow,
    /// Past the trigger on a trading-day boot → run once, immediately.
    RunCatchUp,
    /// Sleep this many seconds, then run at 15:40:00 IST.
    SleepThenRun(u64),
}

/// Pure decision: when should the daily verifier fire.
#[must_use]
pub fn decide_tf_verify_start(
    now_secs_of_day_ist: u32,
    is_trading_day: bool,
    force_now: bool,
) -> TfVerifyStart {
    if force_now {
        return TfVerifyStart::RunNow;
    }
    if !is_trading_day {
        return TfVerifyStart::SkipNonTradingDay;
    }
    if now_secs_of_day_ist >= TF_VERIFY_TRIGGER_SECS_OF_DAY_IST {
        return TfVerifyStart::RunCatchUp;
    }
    TfVerifyStart::SleepThenRun(u64::from(
        TF_VERIFY_TRIGGER_SECS_OF_DAY_IST - now_secs_of_day_ist,
    ))
}

/// Parse + validate the `TICKVAULT_TF_VERIFY_DATE=YYYY-MM-DD` backfill
/// override. STRICT fail-closed shape validation (exactly `YYYY-MM-DD`,
/// digits only, a real calendar date) — anything else yields `None` and the
/// caller refuses the run loudly rather than verifying the wrong day.
#[must_use]
pub fn parse_tf_verify_date(raw: &str) -> Option<NaiveDate> {
    let raw = raw.trim();
    if raw.len() != 10 {
        return None;
    }
    let ok_shape = raw.chars().enumerate().all(|(i, c)| {
        if i == 4 || i == 7 {
            c == '-'
        } else {
            c.is_ascii_digit()
        }
    });
    if !ok_shape {
        return None;
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()
}

/// The previous trading day strictly before `from`, walking back at most
/// [`TF_VERIFY_PREV_DAY_LOOKBACK_DAYS`] calendar days. `None` when the
/// calendar has no trading day inside the window (the caller degrades the
/// Groww pass loudly — never guesses a date).
#[must_use]
pub fn previous_trading_day(calendar: &TradingCalendar, from: NaiveDate) -> Option<NaiveDate> {
    let mut d = from;
    for _ in 0..TF_VERIFY_PREV_DAY_LOOKBACK_DAYS {
        d = d.pred_opt()?;
        if calendar.is_trading_day(d) {
            return Some(d);
        }
    }
    None
}

/// IST-midnight-as-epoch nanoseconds for a trading date (our `candles_*`
/// `ts` stores IST wall-clock as an epoch — `data-integrity.md`). Pure.
#[must_use]
pub fn ist_day_start_nanos(date: NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0)
        .saturating_mul(NANOS_PER_SEC)
}

/// The DETERMINISTIC audit run timestamp for a target day — the day's
/// 15:40:00 IST, regardless of when the run actually fired (catch-up /
/// forced backfill), so reruns UPSERT the same rows. Pure.
#[must_use]
pub fn deterministic_run_ts_nanos(day_start_ist_nanos: i64) -> i64 {
    day_start_ist_nanos
        .saturating_add(i64::from(TF_VERIFY_TRIGGER_SECS_OF_DAY_IST).saturating_mul(NANOS_PER_SEC))
}

// ---------------------------------------------------------------------------
// The verified timeframe set + the independent bucket grid (pure)
// ---------------------------------------------------------------------------

/// The 19 comparison targets: `TfIndex::ALL` minus `M1` (the recompute
/// baseline) minus `D1` (excluded by design — Dhan drops D1 at the write
/// boundary; Groww D1 is a partial-day midnight bucket).
#[must_use]
pub fn tf_verify_targets() -> Vec<TfIndex> {
    TfIndex::ALL
        .into_iter()
        .filter(|tf| !matches!(tf, TfIndex::M1 | TfIndex::D1))
        .collect()
}

/// One session-truncated bucket window in IST seconds-of-day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketWindow {
    /// The bucket OPEN (the stored row's `ts` label).
    pub start_secs_of_day: u32,
    /// `min(start + S, 15:30:00)` — 1m membership naturally truncates here
    /// (the aggregator's session gate admits no ≥15:30 tick).
    pub end_effective_secs_of_day: u32,
    /// The last window of the session (possibly partial).
    pub is_final: bool,
}

/// The full trading-day bucket grid for a bucket size of `tf_secs` seconds:
/// windows `[33_300 + k*S, min(33_300 + (k+1)*S, 55_800))` while the start
/// is inside the session. INDEPENDENT of `TfIndex::bucket_start` on purpose
/// (see the module doc + the tripwire test). Pure.
#[must_use]
pub fn bucket_grid(tf_secs: u32) -> Vec<BucketWindow> {
    let mut out = Vec::new();
    let mut start = SESSION_OPEN_SECS_OF_DAY_IST;
    while start < SESSION_CLOSE_SECS_OF_DAY_IST {
        let natural_end = start.saturating_add(tf_secs);
        out.push(BucketWindow {
            start_secs_of_day: start,
            end_effective_secs_of_day: natural_end.min(SESSION_CLOSE_SECS_OF_DAY_IST),
            is_final: false,
        });
        start = natural_end;
    }
    if let Some(last) = out.last_mut() {
        last.is_final = true;
    }
    out
}

/// Seconds-of-day of an IST-epoch-nanosecond row timestamp relative to its
/// day start. Pure; may be negative / ≥86400 for a row outside the day
/// window (the caller classifies those off-grid).
#[must_use]
pub fn secs_of_day_from_nanos(ts_nanos: i64, day_start_ist_nanos: i64) -> i64 {
    ts_nanos.saturating_sub(day_start_ist_nanos) / NANOS_PER_SEC
}

/// `true` when a stored row's seconds-of-day sits exactly on the
/// 09:15-anchored grid for a bucket size of `tf_secs`. An off-grid stored
/// timestamp is the purest anchoring-bug signal. Pure.
#[must_use]
pub fn is_on_grid(secs_of_day: i64, tf_secs: u32) -> bool {
    let open = i64::from(SESSION_OPEN_SECS_OF_DAY_IST);
    let close = i64::from(SESSION_CLOSE_SECS_OF_DAY_IST);
    secs_of_day >= open && secs_of_day < close && (secs_of_day - open) % i64::from(tf_secs) == 0
}

/// NEW-MEDIUM (refuter round 2): `true` when a Groww window can NEVER be
/// catch-up-sealed intraday on the prod schedule — its effective end lands
/// within [`GROWW_CATCHUP_MARGIN_SECS`] of the 15:30:00 close, so the
/// `end ≤ watermark − margin` seal condition is unsatisfiable before the
/// midnight force-seal (which never runs on the prod schedule). Catches
/// every final window AND e.g. the 2m penultimate `[15:27, 15:29)` window
/// (E = 55_740). Pure.
#[must_use]
pub fn groww_uncatchupable_tail(end_effective_secs_of_day: u32) -> bool {
    end_effective_secs_of_day.saturating_add(GROWW_CATCHUP_MARGIN_SECS)
        >= SESSION_CLOSE_SECS_OF_DAY_IST
}

// ---------------------------------------------------------------------------
// Rows, recompute + compare (pure)
// ---------------------------------------------------------------------------

/// One candle row read back from a `candles_*` table (the projected
/// columns: `ts_nanos, open, high, low, close, volume, tick_count`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CandleRow {
    pub ts_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub tick_count: i64,
}

/// The recomputed higher-TF values over a window's 1m members.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Recomputed {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub tick_count: i64,
}

/// Σ(volume) overflowed i64 — corrupt input data; the window is skipped and
/// the pass degrades (never a silent wrap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeOverflow;

/// Recompute a higher-TF candle from its window's 1m members (MUST be
/// sorted ascending by ts): O = first open, H = max, L = min, C = last
/// close, V = Σ volume (checked), tick_count = Σ (soft signal). `None` on
/// an empty window (zero-tick buckets are never emitted by the aggregator,
/// so absence on both sides is legitimate silence). Pure.
///
/// # Errors
/// [`VolumeOverflow`] when Σ(volume) overflows i64 (corrupt data).
pub fn recompute_window(members: &[CandleRow]) -> Result<Option<Recomputed>, VolumeOverflow> {
    let (Some(first), Some(last)) = (members.first(), members.last()) else {
        return Ok(None);
    };
    let mut high = first.high;
    let mut low = first.low;
    let mut volume: i64 = 0;
    let mut tick_count: i64 = 0;
    for m in members {
        if m.high > high {
            high = m.high;
        }
        if m.low < low {
            low = m.low;
        }
        volume = volume.checked_add(m.volume).ok_or(VolumeOverflow)?;
        tick_count = tick_count.saturating_add(m.tick_count);
    }
    Ok(Some(Recomputed {
        open: first.open,
        high,
        low,
        close: last.close,
        volume,
        tick_count,
    }))
}

/// Integer-paise comparison key for a 2dp-rounded price. Both sides are
/// `round_to_2dp` outputs of identical f64s (the recompute is pure
/// selection — max/min/first/last commute with monotone rounding), so this
/// survives any JSON float re-serialization wobble while staying EXACT —
/// never a float `|diff| <= epsilon` compare (the §37 house precedent).
#[must_use]
pub fn to_paise(v: f64) -> i64 {
    // Comparison key only (nothing is written back rounded); 2dp prices
    // ×100 fit i64 for every real NSE price.
    // APPROVED: cold-path paise comparison key — truncation impossible in range
    #[allow(clippy::cast_possible_truncation)]
    {
        (v * 100.0).round() as i64
    }
}

/// A strict field of the stored-vs-recomputed comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffField {
    Open,
    High,
    Low,
    Close,
    Volume,
}

impl DiffField {
    /// Stable wire label (audit `field` SYMBOL).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::High => "high",
            Self::Low => "low",
            Self::Close => "close",
            Self::Volume => "volume",
        }
    }
}

/// Strict-field diff: OHLC via integer-paise equality, volume via exact
/// i64 equality. `tick_count` is deliberately NOT here (soft — legitimate
/// divergence classes exist, e.g. a Dhan tick DiscardLate for 1m but
/// in-bucket for a longer TF). Pure.
#[must_use]
pub fn diff_strict_fields(stored: &CandleRow, rec: &Recomputed) -> Vec<DiffField> {
    let mut out = Vec::new();
    if to_paise(stored.open) != to_paise(rec.open) {
        out.push(DiffField::Open);
    }
    if to_paise(stored.high) != to_paise(rec.high) {
        out.push(DiffField::High);
    }
    if to_paise(stored.low) != to_paise(rec.low) {
        out.push(DiffField::Low);
    }
    if to_paise(stored.close) != to_paise(rec.close) {
        out.push(DiffField::Close);
    }
    if stored.volume != rec.volume {
        out.push(DiffField::Volume);
    }
    out
}

/// Sorts + de-duplicates rows by `ts_nanos`, keeping the FIRST row per key
/// as returned (stable within one response — the stable sort preserves the
/// server's arrival order among equal keys, so WHICH duplicate is "first"
/// may differ across runs) and returning the duplicated keys — DEDUP
/// should make duplicates impossible, so their presence is a paging
/// finding. Pure.
#[must_use]
pub fn dedup_sorted_rows(mut rows: Vec<CandleRow>) -> (Vec<CandleRow>, Vec<i64>) {
    rows.sort_by_key(|r| r.ts_nanos);
    let mut dup_ts = Vec::new();
    let mut out: Vec<CandleRow> = Vec::with_capacity(rows.len());
    for r in rows {
        if out.last().is_some_and(|prev| prev.ts_nanos == r.ts_nanos) {
            if dup_ts.last() != Some(&r.ts_nanos) {
                dup_ts.push(r.ts_nanos);
            }
            continue;
        }
        out.push(r);
    }
    (out, dup_ts)
}

/// One finding cell drafted by the pure compare (identity added by the
/// pass when it builds the audit row).
#[derive(Clone, Debug, PartialEq)]
pub struct FindingDraft {
    pub tf: &'static str,
    pub bucket_secs_of_day: i64,
    pub category: FindingCategory,
    pub field: &'static str,
    pub stored_value: f64,
    pub recomputed_value: f64,
    pub stored_volume: i64,
    pub recomputed_volume: i64,
    pub one_m_rows: i64,
}

/// Pure per-(instrument, TF) compare counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TfCompareCounts {
    pub buckets_compared: u64,
    pub bucket_gaps: u64,
    pub soft_tick_count: u64,
    pub volume_overflows: u64,
    /// Un-catch-up-able tail-window stored rows ABSENT under the Groww
    /// carve-out (window end within [`GROWW_CATCHUP_MARGIN_SECS`] of the
    /// close — structurally unsealed on the prod schedule) — counted,
    /// never a finding, never a page.
    pub tail_unsealed: u64,
}

/// Compare ONE instrument's stored rows of ONE higher TF against the
/// recompute over its (sorted, de-duplicated) 1m rows. Pure — O(rows) with
/// a single sweep over the grid.
///
/// `exempt_unsealable_tail` is the Groww tail carve-out: when `true`, an
/// ABSENT stored row on any [`groww_uncatchupable_tail`] window (its
/// effective end within [`GROWW_CATCHUP_MARGIN_SECS`] of the 15:30 close
/// — every final window plus e.g. the 2m penultimate `[15:27, 15:29)`
/// window) counts `tail_unsealed` instead of a paging `missing_tf_row`
/// (those windows never seal on the prod schedule — the 16:30 IST
/// auto-stop precedes the midnight force-seal, and the catch-up seal's
/// 60s lateness margin blocks them intraday). A PRESENT tail row is
/// still compared normally, and earlier windows keep full
/// classification. Dhan passes `false`.
#[must_use]
pub fn compare_tf(
    tf: TfIndex,
    rows_1m_sorted: &[CandleRow],
    stored_rows: &[CandleRow],
    day_start_ist_nanos: i64,
    exempt_unsealable_tail: bool,
) -> (Vec<FindingDraft>, TfCompareCounts) {
    let tf_secs = tf.seconds_per_bucket();
    let tf_label = tf.display_name();
    let windows = bucket_grid(tf_secs);
    let mut findings = Vec::new();
    let mut counts = TfCompareCounts::default();

    // Index stored rows by seconds-of-day; classify off-grid + duplicates.
    let mut stored_by_sod: HashMap<i64, CandleRow> = HashMap::with_capacity(stored_rows.len());
    for r in stored_rows {
        let sod = secs_of_day_from_nanos(r.ts_nanos, day_start_ist_nanos);
        if !is_on_grid(sod, tf_secs) {
            findings.push(FindingDraft {
                tf: tf_label,
                bucket_secs_of_day: sod,
                category: FindingCategory::OffGridTs,
                field: "n/a",
                stored_value: r.close,
                recomputed_value: 0.0,
                stored_volume: r.volume,
                recomputed_volume: 0,
                one_m_rows: 0,
            });
            continue;
        }
        if stored_by_sod.contains_key(&sod) {
            findings.push(FindingDraft {
                tf: tf_label,
                bucket_secs_of_day: sod,
                category: FindingCategory::DuplicateKey,
                field: "n/a",
                stored_value: r.close,
                recomputed_value: 0.0,
                stored_volume: r.volume,
                recomputed_volume: 0,
                one_m_rows: 0,
            });
            // Compare on the FIRST row seen (server-order-dependent
            // across runs; stable within this one response).
            continue;
        }
        stored_by_sod.insert(sod, *r);
    }

    // Single sweep: assign sorted 1m rows to windows via a moving cursor.
    let mut cursor = 0usize;
    for w in &windows {
        let start = i64::from(w.start_secs_of_day);
        let end = i64::from(w.end_effective_secs_of_day);
        // Advance past rows before the window (defensive — rows outside
        // the session cannot exist for gated instruments).
        while cursor < rows_1m_sorted.len()
            && secs_of_day_from_nanos(rows_1m_sorted[cursor].ts_nanos, day_start_ist_nanos) < start
        {
            cursor += 1;
        }
        let member_start = cursor;
        while cursor < rows_1m_sorted.len()
            && secs_of_day_from_nanos(rows_1m_sorted[cursor].ts_nanos, day_start_ist_nanos) < end
        {
            cursor += 1;
        }
        let members = &rows_1m_sorted[member_start..cursor];
        let one_m_rows = members.len() as i64;
        let stored = stored_by_sod.remove(&start);

        let recomputed = match recompute_window(members) {
            Ok(r) => r,
            Err(VolumeOverflow) => {
                counts.volume_overflows = counts.volume_overflows.saturating_add(1);
                continue; // corrupt input — the pass degrades; never a wrap.
            }
        };

        match (stored, recomputed) {
            (Some(s), Some(rec)) => {
                counts.buckets_compared = counts.buckets_compared.saturating_add(1);
                for field in diff_strict_fields(&s, &rec) {
                    let (sv, rv) = match field {
                        DiffField::Open => (s.open, rec.open),
                        DiffField::High => (s.high, rec.high),
                        DiffField::Low => (s.low, rec.low),
                        DiffField::Close => (s.close, rec.close),
                        // Exact i64 volumes ride the LONG columns — never a
                        // lossy i64→f64 widening in the DOUBLE cells.
                        DiffField::Volume => (0.0, 0.0),
                    };
                    findings.push(FindingDraft {
                        tf: tf_label,
                        bucket_secs_of_day: start,
                        category: FindingCategory::Mismatch,
                        field: field.as_str(),
                        stored_value: sv,
                        recomputed_value: rv,
                        stored_volume: s.volume,
                        recomputed_volume: rec.volume,
                        one_m_rows,
                    });
                }
                if s.tick_count != rec.tick_count {
                    counts.soft_tick_count = counts.soft_tick_count.saturating_add(1);
                }
                let window_minutes =
                    i64::from(w.end_effective_secs_of_day - w.start_secs_of_day) / 60;
                if one_m_rows < window_minutes {
                    counts.bucket_gaps = counts.bucket_gaps.saturating_add(1);
                }
            }
            (None, Some(rec)) => {
                if exempt_unsealable_tail && groww_uncatchupable_tail(w.end_effective_secs_of_day) {
                    // H1 (widened, refuter round 2) Groww tail carve-out:
                    // an un-catch-up-able tail bucket is STRUCTURALLY
                    // absent (no close-time force-seal; the catch-up
                    // seal's 60s margin blocks any window ending within
                    // it of the close; the 16:30 IST auto-stop precedes
                    // the midnight seal) — counted, no audit row, no page.
                    counts.tail_unsealed = counts.tail_unsealed.saturating_add(1);
                } else {
                    findings.push(FindingDraft {
                        tf: tf_label,
                        bucket_secs_of_day: start,
                        category: FindingCategory::MissingTfRow,
                        field: "n/a",
                        stored_value: 0.0,
                        recomputed_value: rec.close,
                        stored_volume: 0,
                        recomputed_volume: rec.volume,
                        one_m_rows,
                    });
                }
            }
            (Some(s), None) => {
                findings.push(FindingDraft {
                    tf: tf_label,
                    bucket_secs_of_day: start,
                    category: FindingCategory::No1mCoverage,
                    field: "n/a",
                    stored_value: s.close,
                    recomputed_value: 0.0,
                    stored_volume: s.volume,
                    recomputed_volume: 0,
                    one_m_rows: 0,
                });
            }
            (None, None) => {}
        }
    }
    (findings, counts)
}

// ---------------------------------------------------------------------------
// SQL builders (pure — every literal is an i64/const; no user input)
//
// REGRESSION LOCK (the cross_verify_1m 2026-07-10 lesson, empirically
// confirmed on the pinned QuestDB 9.3.5): a bare integer literal compared
// against a TIMESTAMP column is interpreted as epoch MICROSECONDS; `(ts/1)`
// projects MICROS, so the key is re-scaled `* 1000` back to nanoseconds.
// ---------------------------------------------------------------------------

fn day_bounds_micros(day_start_ist_nanos: i64) -> (i64, i64) {
    let start = day_start_ist_nanos / 1_000;
    (start, start.saturating_add(SECS_PER_DAY * MICROS_PER_SEC))
}

/// Per-SID `candles_1m` day query — micros WHERE window, nanos-projected
/// key, feed-scoped, ordered, explicitly LIMIT-bounded. Pure.
#[must_use]
pub fn select_1m_sql(
    feed: &str,
    security_id: i64,
    segment: &str,
    day_start_ist_nanos: i64,
) -> String {
    let (start, end) = day_bounds_micros(day_start_ist_nanos);
    format!(
        "SELECT (ts / 1) * 1000 AS ts_nanos, open, high, low, close, volume, tick_count \
         FROM candles_1m \
         WHERE security_id = {security_id} AND segment = '{segment}' AND feed = '{feed}' \
         AND ts >= {start} AND ts < {end} ORDER BY ts ASC LIMIT {TF_VERIFY_1M_ROW_LIMIT}"
    )
}

/// Per-SID 19-way UNION ALL across `candles_2m..candles_4h`, each arm
/// tagged with its display label. Pure; excludes `candles_1m` (the
/// baseline) and `candles_1d` (excluded by design).
#[must_use]
pub fn select_tf_union_sql(
    feed: &str,
    security_id: i64,
    segment: &str,
    day_start_ist_nanos: i64,
) -> String {
    let (start, end) = day_bounds_micros(day_start_ist_nanos);
    let arms: Vec<String> = tf_verify_targets()
        .into_iter()
        .map(|tf| {
            format!(
                "SELECT '{label}' AS tf, (ts / 1) * 1000 AS ts_nanos, \
                 open, high, low, close, volume, tick_count \
                 FROM {table} \
                 WHERE security_id = {security_id} AND segment = '{segment}' \
                 AND feed = '{feed}' AND ts >= {start} AND ts < {end}",
                label = tf.display_name(),
                table = tf.table_name(),
            )
        })
        .collect();
    // M4: wrap the UNION in a subquery so the LIMIT binds to the WHOLE
    // union (the discovery-builder shape) — a bare LIMIT after the last
    // UNION ALL arm binds unreliably on QuestDB.
    format!(
        "SELECT * FROM ({}) LIMIT {TF_VERIFY_TF_UNION_ROW_LIMIT}",
        arms.join(" UNION ALL ")
    )
}

/// Per-feed instrument discovery: DISTINCT (security_id, segment) over a
/// UNION of `candles_1m` + the 19 higher-TF tables for the day window —
/// higher-TF tables INCLUDED so a phantom TF row with no 1m data is still
/// discovered. Pure.
#[must_use]
pub fn select_instruments_sql(feed: &str, day_start_ist_nanos: i64) -> String {
    let (start, end) = day_bounds_micros(day_start_ist_nanos);
    let mut tables: Vec<&'static str> = vec![TfIndex::M1.table_name()];
    tables.extend(tf_verify_targets().into_iter().map(TfIndex::table_name));
    let arms: Vec<String> = tables
        .into_iter()
        .map(|table| {
            format!(
                "SELECT security_id, segment FROM {table} \
                 WHERE feed = '{feed}' AND ts >= {start} AND ts < {end}"
            )
        })
        .collect();
    format!(
        "SELECT DISTINCT security_id, segment FROM ({}) LIMIT {TF_VERIFY_DISCOVERY_ROW_LIMIT}",
        arms.join(" UNION ALL ")
    )
}

// ---------------------------------------------------------------------------
// /exec dataset parsing (pure — skip-malformed, never panics)
// ---------------------------------------------------------------------------

fn json_dataset(body: &str) -> Result<Vec<serde_json::Value>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("malformed JSON: {e}"))?;
    let rows = v
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "missing dataset".to_string())?;
    Ok(rows.clone())
}

fn row_i64(cols: &[serde_json::Value], idx: usize) -> Option<i64> {
    cols.get(idx).and_then(|c| {
        c.as_i64().or_else(|| {
            // Cold-path JSON fallback for numeric columns QuestDB may
            // serialize as floats; matches the cross_verify volume parse.
            // APPROVED: cold-path float-to-i64 column fallback precedent
            #[allow(clippy::cast_possible_truncation)]
            c.as_f64().map(|f| f as i64)
        })
    })
}

fn candle_row_from_cols(cols: &[serde_json::Value], offset: usize) -> Option<CandleRow> {
    Some(CandleRow {
        ts_nanos: row_i64(cols, offset)?,
        open: cols.get(offset + 1)?.as_f64()?,
        high: cols.get(offset + 2)?.as_f64()?,
        low: cols.get(offset + 3)?.as_f64()?,
        close: cols.get(offset + 4)?.as_f64()?,
        volume: row_i64(cols, offset + 5).unwrap_or(0),
        tick_count: row_i64(cols, offset + 6).unwrap_or(0),
    })
}

/// Parse the per-SID `candles_1m` dataset. `truncated` = the dataset hit
/// the query's LIMIT (the tripwire — never a silent partial compare).
///
/// # Errors
/// Malformed body (not JSON / no dataset). Individual bad rows are skipped.
pub fn parse_1m_dataset(body: &str, limit: usize) -> Result<(Vec<CandleRow>, bool), String> {
    let rows = json_dataset(body)?;
    let truncated = rows.len() >= limit;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(cols) = row.as_array() else { continue };
        if let Some(r) = candle_row_from_cols(cols, 0) {
            out.push(r);
        }
    }
    Ok((out, truncated))
}

/// Parse the per-SID higher-TF UNION dataset (`[tf, ts_nanos, o, h, l, c,
/// volume, tick_count]`). Unknown tf labels are skipped (cannot happen for
/// SQL we built; defensive). `truncated` as above.
///
/// # Errors
/// Malformed body (not JSON / no dataset).
pub fn parse_tf_union_dataset(
    body: &str,
    limit: usize,
) -> Result<(Vec<(TfIndex, CandleRow)>, bool), String> {
    let rows = json_dataset(body)?;
    let truncated = rows.len() >= limit;
    // PERF: the label→TfIndex lookup table is hoisted ABOVE the row loop —
    // a fresh tf_verify_targets() Vec per ROW allocated quadratically.
    let targets = tf_verify_targets();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(cols) = row.as_array() else { continue };
        let Some(label) = cols.first().and_then(|c| c.as_str()) else {
            continue;
        };
        let Some(tf) = targets
            .iter()
            .copied()
            .find(|tf| tf.display_name() == label)
        else {
            continue;
        };
        if let Some(r) = candle_row_from_cols(cols, 1) {
            out.push((tf, r));
        }
    }
    Ok((out, truncated))
}

/// Parse the discovery dataset (`[security_id, segment]`). `truncated` as
/// above.
///
/// # Errors
/// Malformed body (not JSON / no dataset).
pub fn parse_instruments_dataset(
    body: &str,
    limit: usize,
) -> Result<(Vec<(i64, String)>, bool), String> {
    let rows = json_dataset(body)?;
    let truncated = rows.len() >= limit;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(cols) = row.as_array() else { continue };
        let (Some(sid), Some(segment)) = (row_i64(cols, 0), cols.get(1).and_then(|c| c.as_str()))
        else {
            continue;
        };
        out.push((sid, segment.to_string()));
    }
    Ok((out, truncated))
}

// ---------------------------------------------------------------------------
// Run status + summary (pure)
// ---------------------------------------------------------------------------

/// Honest per-pass / per-run verdict (audit Rule 11 — `compared == 0`
/// never reads as a pass).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    /// Coverage + zero paging findings — the verdict is trustworthy.
    Pass,
    /// ≥1 paging finding (mismatch / missing / no-coverage / off-grid /
    /// duplicate) — the operator must judge the audit rows.
    MismatchFound,
    /// Some leg degraded (query/truncation/flush/budget) — the run cannot
    /// vouch for the full universe.
    Degraded,
    /// Zero candles compared while rows existed (or reads failed) — the
    /// run vouches for NOTHING.
    Blind,
    /// Zero rows on BOTH sides and zero failures — feed off; genuinely
    /// nothing to check (never a daily High page).
    NoData,
}

impl RunStatus {
    /// Stable wire label (`tv_tf_verify_runs_total{status}` + Telegram).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::MismatchFound => "mismatch",
            Self::Degraded => "degraded",
            Self::Blind => "blind",
            Self::NoData => "no_data",
        }
    }
}

/// Classify one pass. Precedence (design contract): `compared == 0` with
/// rows seen or any degrade → Blind; `compared == 0` clean-and-empty →
/// NoData; paging findings → MismatchFound; degraded → Degraded; else
/// Pass. Pure.
#[must_use]
pub fn classify_run_status(
    buckets_compared: u64,
    paging_findings: u64,
    degraded: bool,
    rows_seen: bool,
) -> RunStatus {
    if buckets_compared == 0 {
        if rows_seen || degraded {
            return RunStatus::Blind;
        }
        return RunStatus::NoData;
    }
    if paging_findings > 0 {
        return RunStatus::MismatchFound;
    }
    if degraded {
        return RunStatus::Degraded;
    }
    RunStatus::Pass
}

/// Combine the two per-pass statuses into the run verdict: both feeds off
/// → NoData; nothing comparable anywhere → Blind; any paging finding →
/// MismatchFound; any degraded/blind leg → Degraded; else Pass (a
/// feed-off NoData leg never demotes the other leg's Pass). Pure.
#[must_use]
pub fn combine_statuses(a: RunStatus, b: RunStatus) -> RunStatus {
    use RunStatus::{Blind, Degraded, MismatchFound, NoData, Pass};
    match (a, b) {
        (NoData, NoData) => NoData,
        (Blind | NoData, Blind | NoData) => Blind,
        (MismatchFound, _) | (_, MismatchFound) => MismatchFound,
        (Blind | Degraded, _) | (_, Blind | Degraded) => Degraded,
        (Pass, Pass) | (Pass, NoData) | (NoData, Pass) => Pass,
    }
}

/// Per-(feed, date) pass counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassStats {
    pub date_label: String,
    pub instruments: u64,
    pub buckets_compared: u64,
    pub mismatch_cells: u64,
    pub missing_tf_rows: u64,
    pub no_coverage: u64,
    pub off_grid: u64,
    pub duplicates: u64,
    pub bucket_gaps: u64,
    pub soft_tick_count: u64,
    /// Groww un-catch-up-able tail-window carve-out count (structurally
    /// unsealed on the prod schedule) — a counter, never a finding,
    /// never a page.
    pub tail_unsealed: u64,
    pub query_failures: u64,
    pub rows_seen: bool,
    pub degraded: bool,
}

impl PassStats {
    /// Total paging-class findings (the TF-VERIFY-01 trigger). Pure.
    #[must_use]
    pub fn paging_findings(&self) -> u64 {
        self.mismatch_cells
            .saturating_add(self.missing_tf_rows)
            .saturating_add(self.no_coverage)
            .saturating_add(self.off_grid)
            .saturating_add(self.duplicates)
    }
}

/// The run-level summary the boot wiring converts into the typed
/// `TfConsistencySummary` Telegram event.
#[derive(Clone, Debug, PartialEq)]
pub struct TfConsistencySummaryData {
    pub dhan_date_ist: String,
    pub groww_date_ist: String,
    pub instruments: u64,
    pub buckets_compared: u64,
    pub mismatches: u64,
    pub missing_tf_rows: u64,
    pub no_coverage: u64,
    pub off_grid: u64,
    pub duplicates: u64,
    /// Groww end-of-day buckets absent BY DESIGN (never sealed on the prod
    /// schedule) — not verified, not paged; named in the Telegram summary.
    pub tail_unsealed: u64,
    pub degraded: bool,
    pub truncated: bool,
    pub status_label: String,
    pub top_detail: Vec<String>,
}

/// IST 12-hour rendering of a seconds-of-day instant ("9:15 AM",
/// "3:29 PM") — the Telegram commandment 9 form. Pure.
#[must_use]
pub fn format_ist_12h(secs_of_day: i64) -> String {
    let sod = secs_of_day.rem_euclid(SECS_PER_DAY);
    let hour24 = sod / 3600;
    let minute = (sod % 3600) / 60;
    let (hour12, suffix) = match hour24 {
        0 => (12, "AM"),
        1..=11 => (hour24, "AM"),
        12 => (12, "PM"),
        _ => (hour24 - 12, "PM"),
    };
    format!("{hour12}:{minute:02} {suffix}")
}

/// One plain-English offender line for the Telegram top-detail block
/// (no library names, no file paths — the 10 commandments). Pure.
#[must_use]
pub fn detail_line(feed: &str, segment: &str, security_id: i64, d: &FindingDraft) -> String {
    let when = format_ist_12h(d.bucket_secs_of_day);
    match d.category {
        FindingCategory::Mismatch => {
            if d.field == "volume" {
                format!(
                    "{feed} {segment} id {security_id} {tf} {when}: volume stored \
                     {sv} recomputed {rv}",
                    tf = d.tf,
                    sv = d.stored_volume,
                    rv = d.recomputed_volume,
                )
            } else {
                format!(
                    "{feed} {segment} id {security_id} {tf} {when}: {field} stored \
                     {sv:.2} recomputed {rv:.2}",
                    tf = d.tf,
                    field = d.field,
                    sv = d.stored_value,
                    rv = d.recomputed_value,
                )
            }
        }
        FindingCategory::MissingTfRow => format!(
            "{feed} {segment} id {security_id} {tf} {when}: candle missing \
             (1-minute data present)",
            tf = d.tf,
        ),
        FindingCategory::No1mCoverage => format!(
            "{feed} {segment} id {security_id} {tf} {when}: candle has no \
             1-minute data behind it",
            tf = d.tf,
        ),
        FindingCategory::OffGridTs => format!(
            "{feed} {segment} id {security_id} {tf} {when}: timestamp is off \
             the 9:15 grid",
            tf = d.tf,
        ),
        FindingCategory::DuplicateKey => format!(
            "{feed} {segment} id {security_id} {tf} {when}: duplicate rows \
             for one key",
            tf = d.tf,
        ),
    }
}

/// Convert the process-global always-on set (`(security_id, segment_code)`
/// — GIFT Nifty today) into the `(security_id, segment_str)` form the
/// discovery rows use. Always-on instruments are EXCLUDED from the
/// compare (midnight-only seals + pre-open clamp semantics). Pure.
#[must_use]
pub fn always_on_string_set(set: &HashSet<(u64, u8)>) -> HashSet<(i64, String)> {
    set.iter()
        .filter_map(|(sid, code)| {
            i64::try_from(*sid)
                .ok()
                .map(|sid| (sid, segment_code_to_str(*code).to_string()))
        })
        .collect()
}

/// H2 — state-independent always-on detection. `true` when ANY parsed 1m
/// baseline row for the target day sits outside the `[09:15:00, 15:30:00)`
/// IST session window. Only always-on instruments (GIFT Nifty) can carry
/// out-of-session 1m rows — the aggregator's session gate blocks everyone
/// else — so this identifies always-on membership from the DATA itself,
/// even when the process-global `always_on::current()` set is empty (the
/// FAST crash-recovery boot arm never initializes it). Applied BEFORE any
/// classification (including `off_grid_ts`). Pure.
#[must_use]
pub fn has_out_of_session_1m_row(rows_1m: &[CandleRow], day_start_ist_nanos: i64) -> bool {
    rows_1m.iter().any(|r| {
        let sod = secs_of_day_from_nanos(r.ts_nanos, day_start_ist_nanos);
        sod < i64::from(SESSION_OPEN_SECS_OF_DAY_IST)
            || sod >= i64::from(SESSION_CLOSE_SECS_OF_DAY_IST)
    })
}

/// L8 — second-order SQL-injection defense. A discovered `segment` string
/// comes back from the database and is re-interpolated into the follow-up
/// per-SID queries, so it MUST be one of the exact literals
/// `segment_code_to_str` can produce (the 8 known segment strings; the
/// `"UNKNOWN"` fallback arm is deliberately NOT allowlisted).
/// `segment_str_to_code` IS that exact allowlist — a poisoned segment
/// (e.g. `x' OR '1'='1`) is refused and never reaches a query. Pure.
#[must_use]
pub fn is_allowlisted_segment(segment: &str) -> bool {
    segment_str_to_code(segment).is_some()
}

/// SEC — the `/exec` response-body cap classify arm: `true` when the body
/// exceeds [`TF_VERIFY_MAX_RESPONSE_BYTES`], in which case the caller
/// refuses it (stage `oversize`) BEFORE any JSON parse. Pure.
#[must_use]
pub fn response_exceeds_cap(body_len: usize) -> bool {
    body_len > TF_VERIFY_MAX_RESPONSE_BYTES
}

/// L7(a) + L7-completion (refuter round 2) — pure refusal decision for a
/// FORCED run (`TICKVAULT_TF_VERIFY_NOW`).
/// Refused on a non-trading day without a DATE (nothing to verify — the
/// scoreboard round-5 mechanic) AND — whenever the run targets TODAY,
/// i.e. bare `NOW` OR `NOW` + `DATE=today` (the round-1 bypass) — on a
/// trading day BEFORE the 15:40 IST trigger (today's candles are still
/// being sealed — verifying them would page false findings). `NOW` +
/// `DATE` for a PAST trading day is validated separately and is never
/// refused here; `DATE=today` AT/AFTER the trigger is allowed.
/// `None` = run proceeds. Pure.
#[must_use]
pub fn forced_run_refusal(
    has_date_override: bool,
    date_override_is_today: bool,
    is_trading_day: bool,
    now_secs_of_day_ist: u32,
) -> Option<&'static str> {
    if has_date_override && !date_override_is_today {
        // A PAST trading day (already validated) — sealed data; proceed.
        return None;
    }
    if !has_date_override && !is_trading_day {
        return Some("non-trading day — nothing sealed to verify");
    }
    // Targets TODAY (bare NOW, or NOW + DATE=today — the L7-completion
    // bypass closure): refuse before the trigger on a trading day.
    if is_trading_day && now_secs_of_day_ist < TF_VERIFY_TRIGGER_SECS_OF_DAY_IST {
        return Some(
            "before the 3:40 PM IST trigger on a trading day — today's \
             candles are still unsealed",
        );
    }
    None
}

// ---------------------------------------------------------------------------
// The orchestrator (async — live QuestDB deps; pure helpers tested above)
// ---------------------------------------------------------------------------

struct PassParams<'a> {
    client: &'a reqwest::Client,
    exec_url: &'a str,
    feed: &'static str,
    date: NaiveDate,
    always_on: &'a HashSet<(i64, String)>,
    run_started: std::time::Instant,
}

/// Shared mutable run state across the two passes.
struct RunState {
    writer: TfConsistencyAuditWriter,
    audit_rows_written: usize,
    truncated: bool,
    top_detail: Vec<String>,
}

async fn http_get_text(
    client: &reqwest::Client,
    url: &str,
    query: &str,
) -> Result<String, (String, &'static str)> {
    let resp = client
        .get(url)
        .query(&[("query", query)])
        .send()
        .await
        .map_err(|e| (format!("send: {e}"), "questdb_unreachable"))?;
    if !resp.status().is_success() {
        return Err((format!("http {}", resp.status()), "query_failed"));
    }
    // SEC (refuter round 2): refuse a DECLARED oversize body BEFORE
    // reading it at all. Honest residual: a chunked-transfer response
    // (no Content-Length header) is still fully buffered by `.text()`
    // before the post-read cap below can fire — bounded in practice by
    // the per-request timeout; documented in the runbook honest
    // envelope.
    if let Some(declared) = resp.content_length()
        && declared > TF_VERIFY_MAX_RESPONSE_BYTES as u64
    {
        return Err((
            format!(
                "declared content-length {declared} bytes exceeds the \
                 {TF_VERIFY_MAX_RESPONSE_BYTES}-byte cap"
            ),
            "oversize",
        ));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| (format!("read: {e}"), "query_failed"))?;
    // SEC: refuse an oversize body BEFORE the serde_json parse.
    if response_exceeds_cap(body.len()) {
        return Err((
            format!(
                "response body {} bytes exceeds the {TF_VERIFY_MAX_RESPONSE_BYTES}-byte cap",
                body.len()
            ),
            "oversize",
        ));
    }
    Ok(body)
}

fn count_query_failure(stats: &mut PassStats, stage: &'static str) {
    stats.query_failures = stats.query_failures.saturating_add(1);
    stats.degraded = true;
    metrics::counter!("tv_tf_verify_query_failures_total", "stage" => stage).increment(1);
}

/// One (feed, date) verification pass. Private live-deps helper — the
/// classification/recompute/SQL/parse decisions it drives are the pure
/// fns unit-tested below.
async fn run_tf_pass(p: PassParams<'_>, state: &mut RunState) -> PassStats {
    let mut stats = PassStats {
        date_label: p.date.format("%Y-%m-%d").to_string(),
        ..PassStats::default()
    };
    let day_start_nanos = ist_day_start_nanos(p.date);
    let run_ts_nanos = deterministic_run_ts_nanos(day_start_nanos);

    // D1 is excluded by design for BOTH feeds — counted once per pass so
    // the exclusion is visible, never silent.
    metrics::counter!("tv_tf_verify_excluded_total", "reason" => "d1").increment(1);

    // ── Discovery ──
    let discovery_sql = select_instruments_sql(p.feed, day_start_nanos);
    let instruments = match http_get_text(p.client, p.exec_url, &discovery_sql).await {
        Ok(body) => match parse_instruments_dataset(&body, TF_VERIFY_DISCOVERY_ROW_LIMIT) {
            Ok((rows, truncated)) => {
                if truncated {
                    count_query_failure(&mut stats, "truncated");
                    warn!(
                        feed = p.feed,
                        "tf_consistency: instrument discovery hit its LIMIT — \
                         coverage partial (truncation tripwire)"
                    );
                }
                rows
            }
            Err(reason) => {
                count_query_failure(&mut stats, "discovery");
                warn!(feed = p.feed, %reason, "tf_consistency: discovery parse failed");
                Vec::new()
            }
        },
        Err((reason, stage)) => {
            // Transport-class stages (questdb_unreachable / oversize) keep
            // their identity; generic HTTP/read failures fold into the
            // discovery stage.
            let stage = if stage == "query_failed" {
                "discovery"
            } else {
                stage
            };
            count_query_failure(&mut stats, stage);
            warn!(feed = p.feed, %reason, "tf_consistency: discovery query failed");
            Vec::new()
        }
    };
    if instruments.is_empty() {
        info!(
            feed = p.feed,
            date = %stats.date_label,
            degraded = stats.degraded,
            "tf_consistency: no instruments discovered for this feed/day"
        );
        emit_pass_degrade_signal(p.feed, &stats);
        return stats;
    }
    stats.rows_seen = true;

    // H1 (widened, refuter round 2): the Groww pass carves out ABSENT
    // rows on un-catch-up-able tail windows (effective end within the
    // 60s catch-up margin of the close — structurally unsealed on the
    // prod schedule); Dhan finals are close-time-sealed.
    let exempt_unsealable_tail = p.feed == "groww";

    // ── Per-instrument sweep ──
    let mut pass_samples: Vec<String> = Vec::new();
    let mut bad_segments: u64 = 0;
    // Refuter round 2 (self-disarm visibility): data-derived
    // out-of-session exclusions — reported via ONE coalesced warn after
    // the sweep, never per instrument.
    let mut out_of_session_excluded: u64 = 0;
    let mut out_of_session_samples: Vec<String> = Vec::new();
    for (idx, (sid, segment)) in instruments.iter().enumerate() {
        // Wall-clock budget between SIDs: remaining SIDs read-degraded.
        // The `break` right after guarantees exactly ONE coalesced emission.
        if p.run_started.elapsed().as_secs() >= TF_VERIFY_RUN_BUDGET_SECS {
            let remaining = (instruments.len() - idx) as u64;
            stats.query_failures = stats.query_failures.saturating_add(remaining);
            stats.degraded = true;
            metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "budget_exceeded")
                .increment(remaining);
            error!(
                code = ErrorCode::TfVerify02RunDegraded.code_str(),
                stage = "budget_exceeded",
                feed = p.feed,
                remaining,
                "TF-VERIFY-02: run budget exceeded — remaining instruments \
                 skipped (run classifies Degraded)"
            );
            break;
        }
        if idx > 0 && idx % TF_VERIFY_POLITENESS_EVERY_SIDS == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(
                TF_VERIFY_POLITENESS_SLEEP_MS,
            ))
            .await;
        }
        // L8: the discovered segment string is re-interpolated into the
        // follow-up per-SID SQL — refuse anything outside the exact
        // known-segment allowlist BEFORE building any query (second-order
        // injection defense). Counted per instrument; ONE coalesced
        // TF-VERIFY-02 error! per pass after the sweep.
        if !is_allowlisted_segment(segment) {
            bad_segments = bad_segments.saturating_add(1);
            count_query_failure(&mut stats, "bad_segment");
            continue;
        }
        if p.always_on.contains(&(*sid, segment.clone())) {
            metrics::counter!("tv_tf_verify_excluded_total", "reason" => "always_on").increment(1);
            continue;
        }
        stats.instruments = stats.instruments.saturating_add(1);

        // Query A — the 1m baseline. The parsed rows are kept even when
        // the LIMIT tripped — the always-on exclusion below must see them
        // FIRST (refuter round 3).
        let sql_1m = select_1m_sql(p.feed, *sid, segment, day_start_nanos);
        let (rows_1m, truncated_1m) = match http_get_text(p.client, p.exec_url, &sql_1m).await {
            Ok(body) => match parse_1m_dataset(&body, TF_VERIFY_1M_ROW_LIMIT) {
                Ok(pair) => pair,
                Err(reason) => {
                    count_query_failure(&mut stats, "query_failed");
                    warn!(feed = p.feed, security_id = sid, %reason, "tf_consistency: 1m parse failed (skip)");
                    continue;
                }
            },
            Err((reason, stage)) => {
                count_query_failure(&mut stats, stage);
                warn!(feed = p.feed, security_id = sid, %reason, "tf_consistency: 1m query failed (skip)");
                continue;
            }
        };

        // H2: state-independent always-on exclusion. Only always-on
        // instruments (GIFT Nifty) can carry out-of-session 1m rows (the
        // aggregator session gate blocks everyone else), so a single 1m
        // row outside [09:15, 15:30) IST marks the instrument always-on
        // even when the process-global set is empty (the FAST
        // crash-recovery boot arm never initializes it). Applied BEFORE
        // any classification, including off_grid — AND BEFORE the Query-A
        // truncation tripwire (refuter round 3): GIFT Nifty's ~21h day
        // (~1,260 1m rows) exceeds TF_VERIFY_1M_ROW_LIMIT, so classifying
        // truncation first made every FAST-arm boot (empty always_on set)
        // a guaranteed daily Degraded page. The exclusion is still sound
        // on a TRUNCATED row set: with 1m DEDUP engaged the session
        // window holds at most 375 distinct 1m rows (< the 500 LIMIT —
        // pinned by the `TF_VERIFY_1M_ROW_LIMIT > 375` assertion below),
        // so a truncated (>= LIMIT-row) response carries out-of-session
        // rows, and under the query's ORDER BY ts ASC the pre-open rows
        // sort into the returned prefix (even a purely post-close
        // overflow lands inside the LIMIT because in-session rows fill
        // at most 375 of its slots). If 1m DEDUP were broken (duplicate
        // keys inflating the in-session count past the LIMIT with zero
        // out-of-session rows), this exclusion simply does not fire and
        // the instrument falls through to the LOUD Query-A truncation
        // degrade below — never a silent partial compare.
        if has_out_of_session_1m_row(&rows_1m, day_start_nanos) {
            // Refuter round 2: the DATA-derived exclusion gets its own
            // reason label — an instrument reaching this arm is by
            // construction NOT in the always-on registry set (the
            // set-based arm above already `continue`d), so folding it
            // into reason="always_on" misattributed it.
            metrics::counter!("tv_tf_verify_excluded_total", "reason" => "out_of_session")
                .increment(1);
            out_of_session_excluded = out_of_session_excluded.saturating_add(1);
            if out_of_session_samples.len() < TF_VERIFY_OUT_OF_SESSION_SAMPLE_MAX {
                out_of_session_samples.push(format!("{sid} {segment}"));
            }
            // Mirror the set-based exclusion above: an excluded instrument
            // is not counted as examined.
            stats.instruments = stats.instruments.saturating_sub(1);
            continue;
        }

        // Truncation tripwire — ONLY for a NON-excluded instrument
        // (refuter round 3): a truncated compare set is never trusted
        // (no silent partial compare), but an EXCLUDED always-on
        // instrument was never going to be compared at all, so its
        // truncation must not degrade the run.
        if truncated_1m {
            count_query_failure(&mut stats, "truncated");
            continue;
        }

        // Query B — the 19-way higher-TF union.
        let sql_tf = select_tf_union_sql(p.feed, *sid, segment, day_start_nanos);
        let tf_rows = match http_get_text(p.client, p.exec_url, &sql_tf).await {
            Ok(body) => match parse_tf_union_dataset(&body, TF_VERIFY_TF_UNION_ROW_LIMIT) {
                Ok((rows, false)) => rows,
                Ok((_, true)) => {
                    count_query_failure(&mut stats, "truncated");
                    continue;
                }
                Err(reason) => {
                    count_query_failure(&mut stats, "query_failed");
                    warn!(feed = p.feed, security_id = sid, %reason, "tf_consistency: TF-union parse failed (skip)");
                    continue;
                }
            },
            Err((reason, stage)) => {
                count_query_failure(&mut stats, stage);
                warn!(feed = p.feed, security_id = sid, %reason, "tf_consistency: TF-union query failed (skip)");
                continue;
            }
        };

        // 1m duplicates are their own paging finding (DEDUP not engaged).
        let (rows_1m_sorted, dup_1m_ts) = dedup_sorted_rows(rows_1m);
        let mut drafts: Vec<FindingDraft> = dup_1m_ts
            .into_iter()
            .map(|ts| FindingDraft {
                tf: TfIndex::M1.display_name(),
                bucket_secs_of_day: secs_of_day_from_nanos(ts, day_start_nanos),
                category: FindingCategory::DuplicateKey,
                field: "n/a",
                stored_value: 0.0,
                recomputed_value: 0.0,
                stored_volume: 0,
                recomputed_volume: 0,
                one_m_rows: 0,
            })
            .collect();

        // Group the union rows per TF and compare each target TF.
        let mut by_tf: HashMap<u8, Vec<CandleRow>> = HashMap::new();
        for (tf, row) in tf_rows {
            by_tf.entry(tf as u8).or_default().push(row);
        }
        for tf in tf_verify_targets() {
            let stored = by_tf.remove(&(tf as u8)).unwrap_or_default();
            if stored.is_empty() && rows_1m_sorted.is_empty() {
                continue;
            }
            let (tf_drafts, counts) = compare_tf(
                tf,
                &rows_1m_sorted,
                &stored,
                day_start_nanos,
                exempt_unsealable_tail,
            );
            drafts.extend(tf_drafts);
            stats.buckets_compared = stats
                .buckets_compared
                .saturating_add(counts.buckets_compared);
            stats.bucket_gaps = stats.bucket_gaps.saturating_add(counts.bucket_gaps);
            stats.soft_tick_count = stats.soft_tick_count.saturating_add(counts.soft_tick_count);
            stats.tail_unsealed = stats.tail_unsealed.saturating_add(counts.tail_unsealed);
            if counts.volume_overflows > 0 {
                // Corrupt data — Σ(volume) overflowed i64. Degrades the run
                // under the query_failed stage (the data was unusable).
                stats.query_failures = stats.query_failures.saturating_add(counts.volume_overflows);
                stats.degraded = true;
                metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "query_failed")
                    .increment(counts.volume_overflows);
            }
        }

        // Fold the drafts into audit rows + counters + samples.
        for d in &drafts {
            match d.category {
                FindingCategory::Mismatch => {
                    stats.mismatch_cells = stats.mismatch_cells.saturating_add(1);
                }
                FindingCategory::MissingTfRow => {
                    stats.missing_tf_rows = stats.missing_tf_rows.saturating_add(1);
                }
                FindingCategory::No1mCoverage => {
                    stats.no_coverage = stats.no_coverage.saturating_add(1);
                }
                FindingCategory::OffGridTs => {
                    stats.off_grid = stats.off_grid.saturating_add(1);
                }
                FindingCategory::DuplicateKey => {
                    stats.duplicates = stats.duplicates.saturating_add(1);
                }
            }
            metrics::counter!("tv_tf_verify_findings_total", "category" => d.category.as_str())
                .increment(1);
            if pass_samples.len() < TF_VERIFY_TOP_DETAIL_MAX {
                pass_samples.push(detail_line(p.feed, segment, *sid, d));
            }
            if state.top_detail.len() < TF_VERIFY_TOP_DETAIL_MAX {
                state.top_detail.push(detail_line(p.feed, segment, *sid, d));
            }
            if state.audit_rows_written >= TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN {
                state.truncated = true;
                continue; // count-only past the cap (blast-radius bound).
            }
            let finding = TfConsistencyFinding {
                run_ts_ist_nanos: run_ts_nanos,
                trading_date_ist_nanos: day_start_nanos,
                feed: p.feed,
                security_id: *sid,
                segment: segment.clone(),
                tf: d.tf,
                bucket_ts_ist_nanos: day_start_nanos
                    .saturating_add(d.bucket_secs_of_day.saturating_mul(NANOS_PER_SEC)),
                category: d.category,
                field: d.field,
                stored_value: d.stored_value,
                recomputed_value: d.recomputed_value,
                stored_volume: d.stored_volume,
                recomputed_volume: d.recomputed_volume,
                one_m_rows: d.one_m_rows,
            };
            if state.writer.append_finding(&finding).is_err() {
                count_query_failure(&mut stats, "flush_failed");
            } else {
                state.audit_rows_written = state.audit_rows_written.saturating_add(1);
            }
        }
    }

    // M3: the compared/gap/tail counters are emitted ONCE per pass — the
    // stats fields are CUMULATIVE across instruments, so an in-loop
    // emission would overcount quadratically.
    metrics::counter!("tv_tf_verify_buckets_compared_total").increment(stats.buckets_compared);
    metrics::counter!("tv_tf_verify_bucket_gap_total").increment(stats.bucket_gaps);
    // H1 (widened): Groww un-catch-up-able tail windows absent by design
    // (never sealed on the prod schedule) — visible, never silent, never
    // a page.
    metrics::counter!("tv_tf_verify_tail_unsealed_total").increment(stats.tail_unsealed);

    // Soft tick_count divergence is a counter, never a page.
    metrics::counter!("tv_tf_verify_soft_divergence_total", "field" => "tick_count")
        .increment(stats.soft_tick_count);

    // Refuter round 2 (H2 self-disarm visibility): ONE coalesced warn per
    // pass naming instruments excluded by the DATA-derived out-of-session
    // rule while absent from the always-on registry — expected only on
    // FAST-arm boots (the registry set is never initialized there);
    // anywhere else it flags a possible exclusion misattribution.
    if out_of_session_excluded > 0 {
        warn!(
            feed = p.feed,
            date = %stats.date_label,
            count = out_of_session_excluded,
            samples = %out_of_session_samples.join(", "),
            "tf_consistency: instrument(s) excluded by the data-derived \
             out-of-session rule without an always-on registration"
        );
    }

    // L8: ONE coalesced bad-segment error per pass (never per instrument).
    // The poisoned value itself is deliberately NOT logged — it is
    // attacker-controlled text; the audit trail is the count + the stage
    // counter.
    if bad_segments > 0 {
        error!(
            code = ErrorCode::TfVerify02RunDegraded.code_str(),
            stage = "bad_segment",
            feed = p.feed,
            date = %stats.date_label,
            count = bad_segments,
            "TF-VERIFY-02: discovered segment value(s) outside the \
             known-segment allowlist — those instruments were skipped and \
             their segment strings never reached a follow-up query"
        );
    }

    // Coalesced TF-VERIFY-01 — ONE per (feed, date) pass with ≥1 paging
    // finding; the samples name up to 10 offenders (never per-row spam).
    if stats.paging_findings() > 0 {
        error!(
            code = ErrorCode::TfVerify01MismatchFound.code_str(),
            feed = p.feed,
            date = %stats.date_label,
            mismatches = stats.mismatch_cells,
            missing_tf_rows = stats.missing_tf_rows,
            no_coverage = stats.no_coverage,
            off_grid = stats.off_grid,
            duplicates = stats.duplicates,
            buckets_compared = stats.buckets_compared,
            samples = %pass_samples.join(" | "),
            "TF-VERIFY-01: higher-timeframe candles diverge from their \
             1-minute recompute — operator judgment required over the \
             tf_consistency_audit rows"
        );
    }
    emit_pass_degrade_signal(p.feed, &stats);
    stats
}

/// Coalesced per-pass TF-VERIFY-02 degrade signal (edge — one line per
/// pass, not per SID). L5: the runs-verdict counters are deliberately NOT
/// emitted here — they are emitted once in `run_tf_consistency` AFTER the
/// final audit flush, so a flush failure can never record a pass verdict
/// for a run that reports degraded.
fn emit_pass_degrade_signal(feed: &'static str, stats: &PassStats) {
    if stats.degraded {
        error!(
            code = ErrorCode::TfVerify02RunDegraded.code_str(),
            stage = "query_failed",
            feed,
            date = %stats.date_label,
            query_failures = stats.query_failures,
            "TF-VERIFY-02: the timeframe-consistency pass degraded — the run \
             cannot vouch for the full universe (per-stage counts in \
             tv_tf_verify_query_failures_total)"
        );
    }
}

/// Run the full daily verification: the Dhan pass for `dhan_date` (today)
/// and the Groww pass for `groww_date` (the previous trading day; `None`
/// degrades that leg loudly). Cold path, fail-soft; never blocks. Returns
/// the summary for the caller to emit the typed Telegram event.
// TEST-EXEMPT: live-deps async orchestrator (grid/recompute/compare/classify/SQL/parse decisions are pure fns unit-tested below); wiring pinned by crates/app/tests/tf_consistency_wiring_guard.rs.
pub async fn run_tf_consistency(
    questdb_config: &QuestDbConfig,
    dhan_date: NaiveDate,
    groww_date: Option<NaiveDate>,
) -> TfConsistencySummaryData {
    ensure_tf_consistency_audit_table(questdb_config).await;

    let exec_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let dhan_label = dhan_date.format("%Y-%m-%d").to_string();
    let groww_label = groww_date
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(TF_VERIFY_HTTP_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "client_build")
                .increment(1);
            error!(
                code = ErrorCode::TfVerify02RunDegraded.code_str(),
                stage = "client_build",
                ?err,
                "TF-VERIFY-02: HTTP client build failed — verification skipped (blind)"
            );
            metrics::counter!("tv_tf_verify_runs_total", "status" => RunStatus::Blind.as_str())
                .increment(2);
            return TfConsistencySummaryData {
                dhan_date_ist: dhan_label,
                groww_date_ist: groww_label,
                instruments: 0,
                buckets_compared: 0,
                mismatches: 0,
                missing_tf_rows: 0,
                no_coverage: 0,
                off_grid: 0,
                duplicates: 0,
                tail_unsealed: 0,
                degraded: true,
                truncated: false,
                status_label: RunStatus::Blind.as_str().to_string(),
                top_detail: Vec::new(),
            };
        }
    };

    let always_on = always_on_string_set(&tickvault_common::always_on::current());
    let run_started = std::time::Instant::now();
    let mut state = RunState {
        writer: TfConsistencyAuditWriter::new(questdb_config),
        audit_rows_written: 0,
        truncated: false,
        top_detail: Vec::new(),
    };

    // ── Pass 1: Dhan verifies TODAY (amend-frozen after the close seal).
    let dhan_stats = run_tf_pass(
        PassParams {
            client: &client,
            exec_url: &exec_url,
            feed: "dhan",
            date: dhan_date,
            always_on: &always_on,
            run_started,
        },
        &mut state,
    )
    .await;

    // ── Pass 2: Groww verifies the PREVIOUS trading day (its tails seal
    // at IST midnight — D-1 is fully stable, final buckets included).
    let groww_stats = if let Some(date) = groww_date {
        run_tf_pass(
            PassParams {
                client: &client,
                exec_url: &exec_url,
                feed: "groww",
                date,
                always_on: &always_on,
                run_started,
            },
            &mut state,
        )
        .await
    } else {
        error!(
            code = ErrorCode::TfVerify02RunDegraded.code_str(),
            stage = "discovery",
            feed = "groww",
            "TF-VERIFY-02: no previous trading day found within the lookback \
             window — the Groww pass is skipped (degraded, never a guessed date)"
        );
        metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "discovery").increment(1);
        // L5: no runs-verdict counter here — the flush-adjusted final
        // classification below emits it (this synthetic degraded stats
        // block classifies Blind there).
        PassStats {
            date_label: groww_label.clone(),
            degraded: true,
            ..PassStats::default()
        }
    };

    // ── Final audit flush (skip-when-empty is the writer's contract). A
    // failed flush discards pending rows (poisoned-buffer defense) and the
    // run must not read Pass over unpersisted findings.
    let mut flush_degraded = false;
    if let Err(err) = state.writer.flush() {
        flush_degraded = true;
        metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "flush_failed")
            .increment(1);
        error!(
            code = ErrorCode::TfVerify02RunDegraded.code_str(),
            stage = "flush_failed",
            error_chain = %format!("{err:#}"),
            "TF-VERIFY-02: final audit flush failed — findings unpersisted \
             (discarded; rerun is DEDUP-idempotent)"
        );
    }

    let dhan_status = classify_run_status(
        dhan_stats.buckets_compared,
        dhan_stats.paging_findings(),
        dhan_stats.degraded || flush_degraded,
        dhan_stats.rows_seen,
    );
    let groww_status = classify_run_status(
        groww_stats.buckets_compared,
        groww_stats.paging_findings(),
        groww_stats.degraded || flush_degraded,
        groww_stats.rows_seen,
    );
    let combined = combine_statuses(dhan_status, groww_status);

    // L5: the runs-verdict counters are emitted HERE — after the final
    // flush fed into the per-pass classification above — so a failed
    // flush can never record status="pass" while the run reads degraded.
    metrics::counter!("tv_tf_verify_runs_total", "status" => dhan_status.as_str()).increment(1);
    metrics::counter!("tv_tf_verify_runs_total", "status" => groww_status.as_str()).increment(1);

    let summary = TfConsistencySummaryData {
        dhan_date_ist: dhan_stats.date_label.clone(),
        groww_date_ist: groww_stats.date_label.clone(),
        instruments: dhan_stats
            .instruments
            .saturating_add(groww_stats.instruments),
        buckets_compared: dhan_stats
            .buckets_compared
            .saturating_add(groww_stats.buckets_compared),
        mismatches: dhan_stats
            .mismatch_cells
            .saturating_add(groww_stats.mismatch_cells),
        missing_tf_rows: dhan_stats
            .missing_tf_rows
            .saturating_add(groww_stats.missing_tf_rows),
        no_coverage: dhan_stats
            .no_coverage
            .saturating_add(groww_stats.no_coverage),
        off_grid: dhan_stats.off_grid.saturating_add(groww_stats.off_grid),
        duplicates: dhan_stats.duplicates.saturating_add(groww_stats.duplicates),
        tail_unsealed: dhan_stats
            .tail_unsealed
            .saturating_add(groww_stats.tail_unsealed),
        degraded: dhan_stats.degraded || groww_stats.degraded || flush_degraded,
        truncated: state.truncated,
        status_label: combined.as_str().to_string(),
        top_detail: state.top_detail,
    };
    info!(
        status = %summary.status_label,
        dhan_date = %summary.dhan_date_ist,
        groww_date = %summary.groww_date_ist,
        instruments = summary.instruments,
        buckets_compared = summary.buckets_compared,
        mismatches = summary.mismatches,
        missing_tf_rows = summary.missing_tf_rows,
        no_coverage = summary.no_coverage,
        off_grid = summary.off_grid,
        duplicates = summary.duplicates,
        tail_unsealed = summary.tail_unsealed,
        "tf_consistency: daily timeframe-consistency verification complete"
    );
    summary
}

// ---------------------------------------------------------------------------
// Spawn wiring (dual-spawn from main.rs — process-global prefix + FAST arm)
// ---------------------------------------------------------------------------

/// Once-per-process guard: main.rs calls this from BOTH boot paths (the
/// FAST crash-recovery arm returns before the process-global prefix), and
/// a runtime lane restart must never double-spawn the daily task.
static TF_CONSISTENCY_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Spawn the daily timeframe-consistency verifier: the inner task decides
/// (skip / sleep-to-15:40 / catch-up / forced run), runs the two passes,
/// and returns the summary; the outer supervisor converts the outcome into
/// the typed Telegram (`TfConsistencySummary` on success,
/// `TfConsistencyAborted` on Err/panic; graceful-shutdown cancellation
/// stays silent). Self-gates on `[tf_consistency] enabled` + the trading
/// day; `TICKVAULT_TF_VERIFY_NOW` forces a run and
/// `TICKVAULT_TF_VERIFY_DATE=YYYY-MM-DD` backfills a past trading day.
// TEST-EXEMPT: tokio wiring over the unit-tested pure decisions (decide_tf_verify_start / previous_trading_day / parse_tf_verify_date); the dual spawn sites are pinned by crates/app/tests/tf_consistency_wiring_guard.rs.
pub fn spawn_tf_consistency_tasks(
    config: &ApplicationConfig,
    trading_calendar: &std::sync::Arc<TradingCalendar>,
    notifier: &std::sync::Arc<NotificationService>,
) {
    if !config.tf_consistency.enabled {
        info!("tf_consistency: disabled by [tf_consistency] config — nothing spawned");
        return;
    }
    if TF_CONSISTENCY_SPAWNED.swap(true, Ordering::SeqCst) {
        // The other boot path already spawned it this process.
        return;
    }
    let qcfg = config.questdb.clone();
    let calendar = std::sync::Arc::clone(trading_calendar);
    let inner: tokio::task::JoinHandle<Result<Option<(TfConsistencySummaryData, bool)>, String>> =
        tokio::spawn(async move {
            use chrono::{FixedOffset, TimeZone, Timelike, Utc};
            use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
            let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                return Err("IST offset construction failed".to_string());
            };
            let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
            let today_ist = now_ist.date_naive();
            let now_secs_of_day = now_ist.time().num_seconds_from_midnight();
            let force_now = std::env::var("TICKVAULT_TF_VERIFY_NOW")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            // Past-day backfill: TICKVAULT_TF_VERIFY_DATE=YYYY-MM-DD,
            // honored ONLY alongside TICKVAULT_TF_VERIFY_NOW. Fail-closed:
            // a malformed / future / non-trading target refuses the run
            // loudly (the outer supervisor pages Aborted).
            let date_override: Option<NaiveDate> = if force_now {
                match std::env::var("TICKVAULT_TF_VERIFY_DATE") {
                    Ok(raw) => match parse_tf_verify_date(&raw) {
                        Some(d) if d <= today_ist && calendar.is_trading_day(d) => Some(d),
                        Some(d) => {
                            return Err(format!(
                                "TICKVAULT_TF_VERIFY_DATE {d} must be a past-or-today \
                                 trading day — refusing the forced backfill"
                            ));
                        }
                        None => {
                            return Err(format!(
                                "invalid TICKVAULT_TF_VERIFY_DATE {raw:?} — expected YYYY-MM-DD"
                            ));
                        }
                    },
                    Err(_) => None,
                }
            } else {
                // L7(b): DATE without NOW does nothing — say so ONCE
                // instead of silently ignoring the operator's override.
                if std::env::var("TICKVAULT_TF_VERIFY_DATE").is_ok() {
                    warn!(
                        "tf_consistency: TICKVAULT_TF_VERIFY_DATE is ignored \
                         without TICKVAULT_TF_VERIFY_NOW=1 — set both to \
                         backfill a past trading day"
                    );
                }
                None
            };
            let is_trading_day = calendar.is_trading_day(today_ist);
            match decide_tf_verify_start(now_secs_of_day, is_trading_day, force_now) {
                TfVerifyStart::SkipNonTradingDay => {
                    info!("tf_consistency: skipping (non-trading day)");
                    return Ok(None);
                }
                TfVerifyStart::RunNow => {
                    // L7(a) + the scoreboard round-5 mechanic: NOW without a
                    // DATE is refused on a non-trading day (an empty weekend
                    // "today") AND — for ANY run targeting today, i.e. bare
                    // NOW or NOW + DATE=today (L7-completion, refuter round
                    // 2) — on a trading day BEFORE the 15:40 trigger
                    // (today's candles are still unsealed — verifying them
                    // would page false findings). Info log, never a page;
                    // NOW + DATE for a past trading day stays allowed, and
                    // DATE=today AFTER the trigger stays allowed.
                    if let Some(reason) = forced_run_refusal(
                        date_override.is_some(),
                        date_override == Some(today_ist),
                        is_trading_day,
                        now_secs_of_day,
                    ) {
                        info!(
                            %reason,
                            "tf_consistency: forced run refused (pass \
                             TICKVAULT_TF_VERIFY_DATE=YYYY-MM-DD for a PAST \
                             trading day, or re-run after 3:40 PM IST)"
                        );
                        return Ok(None);
                    }
                    info!(
                        backfill_date = date_override
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "today".to_string()),
                        "tf_consistency: forced run (operator dry-run / backfill)"
                    );
                }
                TfVerifyStart::RunCatchUp => {
                    // Once-per-trading-day gate (2026-07-15): a post-15:40
                    // restart must not re-fire an already-delivered daily
                    // card. ONLY this catch-up arm consults the marker —
                    // the scheduled 15:40 fire and forced runs never do,
                    // and a FAILURE-class day never writes one (Rule 11:
                    // suppression never hides a failure).
                    if crate::daily_task_marker::daily_marker_exists(
                        TF_CONSISTENCY_MARKER_TASK,
                        today_ist,
                    ) {
                        // G11 (fix round 2): name the EXACT honored marker
                        // path so the operator hint is copy-pasteable.
                        let marker_path = crate::daily_task_marker::daily_marker_path(
                            TF_CONSISTENCY_MARKER_TASK,
                            today_ist,
                        );
                        info!(
                            marker_date = %today_ist,
                            marker_path = %marker_path.display(),
                            "tf_consistency: today's summary was already delivered — \
                             honoring the {today_ist} delivery marker and skipping \
                             the catch-up re-run (remove the marker file above or \
                             set TICKVAULT_TF_VERIFY_NOW=1 to force)"
                        );
                        return Ok(None);
                    }
                    info!(
                        now = %now_ist.time(),
                        "tf_consistency: late boot (past 15:40 IST) — running the day's \
                         verification now as a catch-up (DEDUP-idempotent)"
                    );
                }
                TfVerifyStart::SleepThenRun(secs_until) => {
                    info!(secs_until, "tf_consistency: sleeping until 15:40:00 IST");
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;
                }
            }
            let target = date_override.unwrap_or(today_ist);
            let groww_date = previous_trading_day(&calendar, target);
            let summary = run_tf_consistency(&qcfg, target, groww_date).await;
            info!("PROOF: tf_consistency verifier fired @ 15:40:00 IST");
            // F4: the forced flag rides along so the notify predicate can
            // keep a forced run's answer LOUD (no_data included).
            Ok(Some((summary, force_now)))
        });
    let tf_notifier = std::sync::Arc::clone(notifier);
    tokio::spawn(async move {
        match inner.await {
            Ok(Ok(Some((s, forced_run)))) => {
                // MERGE-NOTE (G9a, fix round 2 — collision site 2 of 2 with
                // `claude/groww-live-off-rest-only`): that branch
                // restructures this SAME notify arm around a 1-arg
                // should_notify_summary and the un-tupled JoinHandle.
                // RESOLUTION RULE: keep the tupled (summary, forced_run)
                // shape + the 2-arg predicate — forced runs always notify
                // (the F4 Rule-11 fix). See the fn doc + the plan file's
                // "Merge coordination" section.
                let status_label = s.status_label.clone();
                // Marker keyed on the VERIFIED (Dhan) date, never blindly
                // "today" — a forced past-date backfill must not suppress
                // today's catch-up (2026-07-15 once-per-day gate).
                let marker_date =
                    chrono::NaiveDate::parse_from_str(&s.dhan_date_ist, "%Y-%m-%d").ok();
                if should_notify_summary(&status_label, forced_run) {
                    tf_notifier.notify(NotificationEvent::TfConsistencySummary {
                        dhan_date_ist: s.dhan_date_ist,
                        groww_date_ist: s.groww_date_ist,
                        instruments: s.instruments,
                        buckets_compared: s.buckets_compared,
                        mismatches: s.mismatches,
                        missing_tf_rows: s.missing_tf_rows,
                        no_coverage: s.no_coverage,
                        off_grid: s.off_grid,
                        duplicates: s.duplicates,
                        tail_unsealed: s.tail_unsealed,
                        degraded: s.degraded,
                        truncated: s.truncated,
                        status_label: s.status_label,
                        top_detail: s.top_detail,
                    });
                } else {
                    // no_data is log-only (2026-07-15): "nothing to check"
                    // must never page — the suppressed send is replaced by
                    // this one visible line (never a silent skip).
                    //
                    // G4b (fix round 2): warn!, not info! — this arm is
                    // structurally TRADING-DAY-only (scheduled/catch-up
                    // runs exist only on trading days per
                    // decide_tf_verify_start; forced runs always notify),
                    // and a trading-day "nothing to check" while Groww ran
                    // (every prod day) can also be a silently-empty
                    // discovery query (the PR #1474 blind-since-birth
                    // class) — warn! keeps it above the info noise floor.
                    //
                    // G4c — RULE-CONTRACT TENSION, recorded deliberately
                    // (see the plan file's Observability section):
                    // `tf-consistency-error-codes.md` §3's delivery
                    // contract reads "the typed TfConsistencySummary
                    // Telegram (ONE per run — Info only when clean WITH
                    // coverage or pure no-data)". This suppression
                    // intentionally deviates for the scheduled/catch-up
                    // no_data arm per the operator's direct 2026-07-15
                    // cleanliness escalation — PENDING the rule-file
                    // supersession the operator must land with a dated
                    // quote. Rule files are not editable in this PR.
                    warn!(
                        status = %status_label,
                        dhan_date = %s.dhan_date_ist,
                        groww_date = %s.groww_date_ist,
                        "tf_consistency: nothing to check today (trading day) — \
                         Telegram summary suppressed (log-only; pass, failure \
                         and FORCED days still notify). If the feeds DID run \
                         today, suspect an empty discovery query."
                    );
                }
                // G4a (fix round 2): ONLY a "pass" marks the day delivered.
                // no_data no longer seals the marker — a trading-day
                // no_data can be a silently-empty discovery dataset (the
                // PR #1474 class), and marker-sealing it made every
                // same-day restart's catch-up skip too ("nothing to check"
                // forever, zero operator signal). mismatch/degraded/blind
                // ALSO leave the marker unwritten so a restart re-runs +
                // re-pages (Rule 11).
                //
                // F5 HONEST RESIDUAL (fix round 1, 2026-07-15 — timeboxed
                // decision): `notify()` is spawn-and-return with NO public
                // completion signal, so this marker records DISPATCH, not
                // DELIVERY. A Telegram-transport failure on a PASS day
                // still writes the marker, and every same-day restart then
                // skips — the operator gets no card until the next trading
                // day (self-heals). Bounded by: the TELEGRAM-01 drop
                // counter + the tv-telegram-drops CloudWatch alarm cover
                // transport-failure visibility, and the catch-up skip line
                // above names the honored marker date so a suppressed day
                // is diagnosable from logs. Gating the marker on delivery
                // needs a service.rs completion signal — deliberately not
                // smuggled into this fix round (see the plan file's
                // Failure Modes entry).
                if status_label == "pass"
                    && let Some(d) = marker_date
                {
                    crate::daily_task_marker::write_daily_marker(TF_CONSISTENCY_MARKER_TASK, d);
                }
            }
            Ok(Ok(None)) => {} // non-trading-day skip / refused force — no page.
            Ok(Err(reason)) => {
                error!(
                    code = ErrorCode::TfVerify02RunDegraded.code_str(),
                    stage = "daily_run",
                    %reason,
                    "TF-VERIFY-02: the daily timeframe-consistency run failed"
                );
                tf_notifier.notify(NotificationEvent::TfConsistencyAborted { detail: reason });
            }
            Err(join_err) if join_err.is_panic() => {
                error!(
                    code = ErrorCode::TfVerify02RunDegraded.code_str(),
                    stage = "daily_panic",
                    %join_err,
                    "TF-VERIFY-02: the daily timeframe-consistency task crashed"
                );
                tf_notifier.notify(NotificationEvent::TfConsistencyAborted {
                    detail: format!("the timeframe check task crashed: {join_err}"),
                });
            }
            Err(_) => {
                // Cancellation during graceful shutdown (16:30 IST auto-stop,
                // `make stop`) — normal teardown, NOT an abort. No page.
                info!("tf_consistency: task cancelled during shutdown");
            }
        }
    });
    info!("tf_consistency: daily timeframe-consistency verifier spawned (process-global)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sod: i64, o: f64, h: f64, l: f64, c: f64, v: i64, tc: i64) -> CandleRow {
        CandleRow {
            ts_nanos: DAY_START + sod * NANOS_PER_SEC,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
            tick_count: tc,
        }
    }

    /// A whole IST day number (multiple of 86_400) so `bucket_start`'s
    /// day-floor arithmetic aligns with the verifier's seconds-of-day.
    const DAY_START: i64 = 20_000 * 86_400 * NANOS_PER_SEC;

    // -------------------------------------------------------------------
    // Scheduling
    // -------------------------------------------------------------------

    #[test]
    fn test_decide_tf_verify_start_trigger_constant_is_1540_ist() {
        assert_eq!(TF_VERIFY_TRIGGER_SECS_OF_DAY_IST, 56_400);
    }

    // -------------------------------------------------------------------
    // should_notify_summary (2026-07-15 no_data log-only gate)
    // -------------------------------------------------------------------

    #[test]
    fn test_should_notify_summary_suppresses_only_no_data() {
        assert!(
            !should_notify_summary("no_data", false),
            "a nothing-to-check day is log-only on scheduled/catch-up runs"
        );
    }

    #[test]
    fn test_should_notify_summary_forced_run_always_notifies() {
        // F4 (2026-07-15 fix round): a FORCED run must never answer with
        // silence — the catch-up skip message itself tells the operator to
        // force one; no_data included.
        assert!(
            should_notify_summary("no_data", true),
            "a forced run must notify even on no_data"
        );
        for label in ["pass", "mismatch", "degraded", "blind", ""] {
            assert!(should_notify_summary(label, true), "{label} forced");
        }
    }

    #[test]
    fn test_should_notify_summary_pass_and_failure_classes_notify() {
        // Rule 11: every verdict with information content still notifies —
        // pass (positive daily signal) AND every failure class.
        for label in ["pass", "mismatch", "degraded", "blind"] {
            assert!(
                should_notify_summary(label, false),
                "{label} must keep notifying"
            );
        }
        // Fail-open on an unknown / drifted label: notify, never silence.
        assert!(should_notify_summary("some_future_label", false));
        assert!(should_notify_summary("", false));
    }

    #[test]
    fn test_tf_consistency_marker_task_slug_is_pinned() {
        // The marker filename is derived from this slug — a drift would
        // orphan existing markers and re-fire the day's card once.
        assert_eq!(TF_CONSISTENCY_MARKER_TASK, "tf-consistency");
    }

    #[test]
    fn test_decide_tf_verify_start_sleeps_before_trigger() {
        // 15:39:00 IST → sleep 60s to 15:40:00.
        let now = 15 * 3600 + 39 * 60;
        assert_eq!(
            decide_tf_verify_start(now, true, false),
            TfVerifyStart::SleepThenRun(60)
        );
    }

    #[test]
    fn test_decide_tf_verify_start_catch_up_at_and_after_trigger() {
        assert_eq!(
            decide_tf_verify_start(TF_VERIFY_TRIGGER_SECS_OF_DAY_IST, true, false),
            TfVerifyStart::RunCatchUp
        );
        assert_eq!(
            decide_tf_verify_start(17 * 3600, true, false),
            TfVerifyStart::RunCatchUp
        );
    }

    #[test]
    fn test_decide_tf_verify_start_skips_non_trading_day() {
        assert_eq!(
            decide_tf_verify_start(10 * 3600, false, false),
            TfVerifyStart::SkipNonTradingDay
        );
    }

    #[test]
    fn test_decide_tf_verify_start_force_overrides_everything() {
        assert_eq!(
            decide_tf_verify_start(10 * 3600, false, true),
            TfVerifyStart::RunNow
        );
        assert_eq!(
            decide_tf_verify_start(20 * 3600, true, true),
            TfVerifyStart::RunNow
        );
    }

    #[test]
    fn test_parse_tf_verify_date_strict_shape() {
        assert_eq!(
            parse_tf_verify_date("2026-07-10"),
            NaiveDate::from_ymd_opt(2026, 7, 10)
        );
        assert_eq!(
            parse_tf_verify_date(" 2026-07-10 "),
            parse_tf_verify_date("2026-07-10")
        );
        assert!(parse_tf_verify_date("2026-7-10").is_none());
        assert!(parse_tf_verify_date("2026/07/10").is_none());
        assert!(parse_tf_verify_date("2026-13-01").is_none());
        assert!(parse_tf_verify_date("garbage").is_none());
        assert!(parse_tf_verify_date("").is_none());
    }

    fn test_calendar() -> TradingCalendar {
        use tickvault_common::config::{NseHolidayEntry, TradingConfig};
        TradingCalendar::from_config(&TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-07-10".to_string(),
                name: "Synthetic Holiday".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        })
        .expect("test calendar")
    }

    #[test]
    fn test_previous_trading_day_walks_back_over_weekend_and_holiday() {
        let cal = test_calendar();
        // Monday 2026-07-13 walks back over Sun 12 / Sat 11 / holiday Fri 10
        // to Thursday 2026-07-09.
        let monday = NaiveDate::from_ymd_opt(2026, 7, 13).expect("date");
        assert_eq!(
            previous_trading_day(&cal, monday),
            NaiveDate::from_ymd_opt(2026, 7, 9)
        );
        // A plain mid-week day walks back exactly one day.
        let thursday = NaiveDate::from_ymd_opt(2026, 7, 9).expect("date");
        assert_eq!(
            previous_trading_day(&cal, thursday),
            NaiveDate::from_ymd_opt(2026, 7, 8)
        );
    }

    #[test]
    fn test_ist_day_start_nanos_is_midnight_epoch() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 13).expect("date");
        let nanos = ist_day_start_nanos(d);
        assert_eq!(nanos % (SECS_PER_DAY * NANOS_PER_SEC), 0, "whole IST day");
        // Round-trips through chrono.
        let secs = nanos / NANOS_PER_SEC;
        assert_eq!(
            chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.date_naive()),
            Some(d)
        );
    }

    #[test]
    fn test_deterministic_run_ts_nanos_is_1540_of_target_day() {
        let day = 20_000_i64 * 86_400 * NANOS_PER_SEC;
        let run_ts = deterministic_run_ts_nanos(day);
        assert_eq!((run_ts - day) / NANOS_PER_SEC, 56_400, "15:40:00 IST");
    }

    // -------------------------------------------------------------------
    // Targets + grid
    // -------------------------------------------------------------------

    #[test]
    fn test_tf_verify_targets_exclude_m1_and_d1() {
        let targets = tf_verify_targets();
        assert_eq!(targets.len(), 19, "21 TFs minus M1 minus D1");
        assert!(!targets.contains(&TfIndex::M1));
        assert!(!targets.contains(&TfIndex::D1));
        assert_eq!(targets.first(), Some(&TfIndex::M2));
        assert_eq!(targets.last(), Some(&TfIndex::H4));
    }

    /// Per-TF daily bucket counts pinned as literals — each equals
    /// ceil(375 / minutes-per-bucket) for the 375-minute session.
    #[test]
    fn test_bucket_grid_daily_counts_all_19_tfs() {
        let expected: [(TfIndex, usize); 19] = [
            (TfIndex::M2, 188),
            (TfIndex::M3, 125),
            (TfIndex::M4, 94),
            (TfIndex::M5, 75),
            (TfIndex::M6, 63),
            (TfIndex::M7, 54),
            (TfIndex::M8, 47),
            (TfIndex::M9, 42),
            (TfIndex::M10, 38),
            (TfIndex::M11, 35),
            (TfIndex::M12, 32),
            (TfIndex::M13, 29),
            (TfIndex::M14, 27),
            (TfIndex::M15, 25),
            (TfIndex::M30, 13),
            (TfIndex::H1, 7),
            (TfIndex::H2, 4),
            (TfIndex::H3, 3),
            (TfIndex::H4, 2),
        ];
        for (tf, count) in expected {
            let grid = bucket_grid(tf.seconds_per_bucket());
            assert_eq!(grid.len(), count, "window count for {}", tf.display_name());
            // ceil(375 / S_minutes) cross-check computed independently.
            let s_min = (tf.seconds_per_bucket() / 60) as usize;
            assert_eq!(count, 375usize.div_ceil(s_min), "{}", tf.display_name());
            // Exactly the last window is final; every end ≤ 15:30.
            assert!(grid.last().is_some_and(|w| w.is_final));
            assert_eq!(grid.iter().filter(|w| w.is_final).count(), 1);
            for w in &grid {
                assert!(w.end_effective_secs_of_day <= SESSION_CLOSE_SECS_OF_DAY_IST);
                assert!(w.start_secs_of_day < w.end_effective_secs_of_day);
            }
        }
    }

    #[test]
    fn test_bucket_grid_partial_final_windows_truncate_at_close() {
        // M2's last window is one minute: [15:29, 15:30).
        let m2 = bucket_grid(120);
        let last = m2.last().expect("windows");
        assert_eq!(last.start_secs_of_day, 15 * 3600 + 29 * 60);
        assert_eq!(
            last.end_effective_secs_of_day,
            SESSION_CLOSE_SECS_OF_DAY_IST
        );
        // H1's last window is [15:15, 16:15) effective 15:30.
        let h1 = bucket_grid(3_600);
        let last = h1.last().expect("windows");
        assert_eq!(last.start_secs_of_day, 15 * 3600 + 15 * 60);
        assert_eq!(
            last.end_effective_secs_of_day,
            SESSION_CLOSE_SECS_OF_DAY_IST
        );
        // H4's last window is [13:15, 17:15) effective 15:30.
        let h4 = bucket_grid(14_400);
        let last = h4.last().expect("windows");
        assert_eq!(last.start_secs_of_day, 13 * 3600 + 15 * 60);
        assert_eq!(
            last.end_effective_secs_of_day,
            SESSION_CLOSE_SECS_OF_DAY_IST
        );
    }

    /// TRIPWIRE (the mask-risk answer): the verifier's INDEPENDENT grid
    /// must agree with `TfIndex::bucket_start` for probe instants across
    /// every target TF, AND with hand-typed literals computed by a human
    /// (neither implementation). If the aggregator's anchoring ever
    /// changes, this fails the build and forces a conscious dual update.
    #[test]
    fn test_tripwire_grid_agrees_with_tf_index_bucket_start() {
        let day_start_secs = 20_000_u32 * 86_400;
        let probes_sod: [u32; 9] = [
            33_300, // 09:15:00
            33_301, // 09:15:01
            33_450, // 09:17:30
            35_099, // 09:44:59
            35_100, // 09:45:00
            43_200, // 12:00:00
            54_899, // 15:14:59
            54_900, // 15:15:00
            55_799, // 15:29:59
        ];
        for tf in tf_verify_targets() {
            let grid = bucket_grid(tf.seconds_per_bucket());
            for sod in probes_sod {
                let tick = day_start_secs + sod;
                let agg_bucket = tf.bucket_start(tick) - day_start_secs;
                let ours = grid
                    .iter()
                    .find(|w| (w.start_secs_of_day..w.end_effective_secs_of_day).contains(&sod))
                    .unwrap_or_else(|| {
                        panic!("no window contains {sod} for {}", tf.display_name())
                    });
                assert_eq!(
                    ours.start_secs_of_day,
                    agg_bucket,
                    "grid drift vs TfIndex::bucket_start for {} at sod {sod}",
                    tf.display_name()
                );
            }
        }
        // Hand-typed literals (typed by a human, computed by neither impl):
        // 5m @ 09:17:30 → 09:15:00.
        assert_eq!(
            TfIndex::M5.bucket_start(day_start_secs + 33_450) - day_start_secs,
            33_300
        );
        // H1 @ 15:20:00 → 15:15:00.
        assert_eq!(
            TfIndex::H1.bucket_start(day_start_secs + 55_200) - day_start_secs,
            54_900
        );
        // 30m @ 09:45:00 → 09:45:00 (exact boundary opens the next bucket).
        assert_eq!(
            TfIndex::M30.bucket_start(day_start_secs + 35_100) - day_start_secs,
            35_100
        );
        // And the verifier's own grid agrees with those literals.
        assert!(
            bucket_grid(300)
                .iter()
                .any(|w| w.start_secs_of_day == 33_300)
        );
        assert!(
            bucket_grid(3_600)
                .iter()
                .any(|w| w.start_secs_of_day == 54_900)
        );
        assert!(
            bucket_grid(1_800)
                .iter()
                .any(|w| w.start_secs_of_day == 35_100)
        );
    }

    #[test]
    fn test_is_on_grid_boundaries() {
        // 5m grid: 09:15 on, 09:16 off, 09:20 on; outside the session off.
        assert!(is_on_grid(33_300, 300));
        assert!(!is_on_grid(33_360, 300));
        assert!(is_on_grid(33_600, 300));
        assert!(!is_on_grid(i64::from(SESSION_CLOSE_SECS_OF_DAY_IST), 300));
        assert!(!is_on_grid(0, 300));
        assert!(!is_on_grid(-60, 300));
        // H1 grid: 15:15 on (the partial final bucket's label).
        assert!(is_on_grid(54_900, 3_600));
        assert!(!is_on_grid(54_960, 3_600));
    }

    #[test]
    fn test_secs_of_day_from_nanos_round_trip() {
        assert_eq!(
            secs_of_day_from_nanos(DAY_START + 33_300 * NANOS_PER_SEC, DAY_START),
            33_300
        );
        assert_eq!(secs_of_day_from_nanos(DAY_START, DAY_START), 0);
    }

    // -------------------------------------------------------------------
    // Recompute + compare
    // -------------------------------------------------------------------

    #[test]
    fn test_recompute_window_first_max_min_last_sum() {
        let members = [
            row(33_300, 100.0, 101.0, 99.5, 100.5, 10, 3),
            row(33_360, 100.5, 102.0, 100.0, 101.5, 20, 4),
            row(33_420, 101.5, 101.75, 98.0, 99.0, 30, 5),
        ];
        let rec = recompute_window(&members)
            .expect("no overflow")
            .expect("non-empty");
        assert_eq!(rec.open, 100.0, "first open");
        assert_eq!(rec.high, 102.0, "max high");
        assert_eq!(rec.low, 98.0, "min low");
        assert_eq!(rec.close, 99.0, "last close");
        assert_eq!(rec.volume, 60, "sum volume");
        assert_eq!(rec.tick_count, 12, "sum tick_count");
    }

    #[test]
    fn test_recompute_window_empty_and_single_member() {
        assert_eq!(recompute_window(&[]), Ok(None));
        let single = [row(33_300, 5.0, 6.0, 4.0, 5.5, 7, 2)];
        let rec = recompute_window(&single).expect("ok").expect("some");
        assert_eq!(
            rec,
            Recomputed {
                open: 5.0,
                high: 6.0,
                low: 4.0,
                close: 5.5,
                volume: 7,
                tick_count: 2
            }
        );
    }

    #[test]
    fn test_recompute_window_volume_checked_add_flags_overflow() {
        let members = [
            row(33_300, 1.0, 1.0, 1.0, 1.0, i64::MAX, 1),
            row(33_360, 1.0, 1.0, 1.0, 1.0, 1, 1),
        ];
        assert_eq!(recompute_window(&members), Err(VolumeOverflow));
        // i64::MAX alone is fine (boundary, no wrap).
        let max_only = [row(33_300, 1.0, 1.0, 1.0, 1.0, i64::MAX, 1)];
        assert_eq!(
            recompute_window(&max_only).expect("ok").map(|r| r.volume),
            Some(i64::MAX)
        );
    }

    #[test]
    fn test_to_paise_boundaries() {
        assert_eq!(to_paise(0.0), 0);
        assert_eq!(to_paise(25_647.50), 2_564_750);
        assert_eq!(to_paise(25_647.49), 2_564_749);
        // 0.005 neighborhood: ties round away from zero.
        assert_eq!(to_paise(0.005), 1);
        assert_eq!(to_paise(0.004), 0);
        assert_eq!(to_paise(-1.25), -125);
        // Large price stays exact at 2dp.
        assert_eq!(to_paise(99_999.99), 9_999_999);
    }

    #[test]
    fn test_diff_strict_fields_per_field_and_volume_exact() {
        let stored = row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3);
        let rec = Recomputed {
            open: 100.25,
            high: 101.0,
            low: 99.0,
            close: 100.75,
            volume: 12,
            tick_count: 3,
        };
        let diffs = diff_strict_fields(&stored, &rec);
        assert_eq!(
            diffs,
            vec![DiffField::Open, DiffField::Close, DiffField::Volume]
        );
        // Identical values (incl. a sub-paise wobble) do not diff.
        let rec_eq = Recomputed {
            open: 100.000_000_1,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 10,
            tick_count: 99,
        };
        assert!(diff_strict_fields(&stored, &rec_eq).is_empty());
    }

    #[test]
    fn test_diff_field_as_str_labels_stable() {
        assert_eq!(DiffField::Open.as_str(), "open");
        assert_eq!(DiffField::High.as_str(), "high");
        assert_eq!(DiffField::Low.as_str(), "low");
        assert_eq!(DiffField::Close.as_str(), "close");
        assert_eq!(DiffField::Volume.as_str(), "volume");
    }

    #[test]
    fn test_dedup_sorted_rows_keeps_first_and_reports_dups() {
        let a = row(33_300, 1.0, 1.0, 1.0, 1.0, 1, 1);
        let mut b = a;
        b.close = 2.0;
        let c = row(33_360, 2.0, 2.0, 2.0, 2.0, 2, 2);
        let (rows, dups) = dedup_sorted_rows(vec![c, a, b]);
        assert_eq!(rows.len(), 2);
        // Which duplicate is "first" is server-order-dependent across
        // runs; within THIS response the first-seen row wins (stable sort).
        assert_eq!(rows[0].close, 1.0, "first-seen row wins in this response");
        assert_eq!(dups, vec![a.ts_nanos]);
        // No duplicates → empty dup list.
        let (rows, dups) = dedup_sorted_rows(vec![c]);
        assert_eq!(rows.len(), 1);
        assert!(dups.is_empty());
    }

    // -------------------------------------------------------------------
    // compare_tf classification
    // -------------------------------------------------------------------

    /// 5m window [09:15, 09:20): three 1m members recompute to the stored
    /// row exactly → one compared bucket, zero findings; the two missing
    /// interior minutes count a bucket gap (never a finding).
    #[test]
    fn test_compare_tf_matching_window_counts_gap_not_finding() {
        let ones = vec![
            row(33_300, 100.0, 101.0, 99.5, 100.5, 10, 3),
            row(33_360, 100.5, 102.0, 100.0, 101.5, 20, 4),
            row(33_420, 101.5, 101.75, 98.0, 99.0, 30, 5),
        ];
        let stored = vec![row(33_300, 100.0, 102.0, 98.0, 99.0, 60, 12)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, false);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(counts.buckets_compared, 1);
        assert_eq!(counts.bucket_gaps, 1, "3 of 5 minutes present");
        assert_eq!(counts.soft_tick_count, 0);
    }

    #[test]
    fn test_compare_tf_reports_mismatch_per_field() {
        let ones = vec![row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        // Stored high + volume wrong.
        let stored = vec![row(33_300, 100.0, 105.0, 99.0, 100.5, 11, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, false);
        assert_eq!(counts.buckets_compared, 1);
        let cats: Vec<_> = findings.iter().map(|f| (f.category, f.field)).collect();
        assert!(cats.contains(&(FindingCategory::Mismatch, "high")));
        assert!(cats.contains(&(FindingCategory::Mismatch, "volume")));
        assert_eq!(findings.len(), 2);
        // Volume rows carry the exact i64s in the LONG cells.
        let vol = findings
            .iter()
            .find(|f| f.field == "volume")
            .expect("volume");
        assert_eq!((vol.stored_volume, vol.recomputed_volume), (11, 10));
        assert_eq!((vol.stored_value, vol.recomputed_value), (0.0, 0.0));
    }

    #[test]
    fn test_compare_tf_missing_tf_row_pages_when_1m_present() {
        let ones = vec![row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &[], DAY_START, false);
        assert_eq!(counts.buckets_compared, 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::MissingTfRow);
        assert_eq!(findings[0].bucket_secs_of_day, 33_300);
        assert_eq!(findings[0].one_m_rows, 1);
    }

    #[test]
    fn test_compare_tf_no_1m_coverage_when_stored_over_nothing() {
        let stored = vec![row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &[], &stored, DAY_START, false);
        assert_eq!(counts.buckets_compared, 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::No1mCoverage);
    }

    #[test]
    fn test_compare_tf_off_grid_stored_ts_detected() {
        // A 09:16:00 "5m" row is off the 09:15-anchored grid.
        let stored = vec![row(33_360, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, _) = compare_tf(TfIndex::M5, &[], &stored, DAY_START, false);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::OffGridTs);
        assert_eq!(findings[0].bucket_secs_of_day, 33_360);
    }

    #[test]
    fn test_compare_tf_duplicate_stored_rows_flagged_first_wins() {
        let ones = vec![row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let first = row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3);
        let mut second = first;
        second.close = 999.0;
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &[first, second], DAY_START, false);
        // The duplicate is flagged; the compare ran on the FIRST row and
        // matched, so no mismatch rows.
        assert_eq!(counts.buckets_compared, 1);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::DuplicateKey);
    }

    #[test]
    fn test_compare_tf_window_boundary_membership() {
        // ts == window start is IN; ts == window end is OUT (next window).
        let ones = vec![
            row(33_300, 1.0, 1.0, 1.0, 1.0, 1, 1), // [09:15, 09:20)
            row(33_600, 2.0, 2.0, 2.0, 2.0, 2, 1), // exactly 09:20 → next
        ];
        let stored = vec![
            row(33_300, 1.0, 1.0, 1.0, 1.0, 1, 1),
            row(33_600, 2.0, 2.0, 2.0, 2.0, 2, 1),
        ];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, false);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(counts.buckets_compared, 2);
    }

    #[test]
    fn test_compare_tf_partial_final_window_h1_members_stop_at_1529() {
        // H1 final window [15:15, 16:15) effective [15:15, 15:30): the
        // 15:29 minute is a member; nothing ≥ 15:30 can exist.
        let ones = vec![
            row(54_900, 10.0, 11.0, 9.0, 10.5, 5, 2),  // 15:15
            row(55_740, 10.5, 12.0, 10.0, 11.0, 5, 2), // 15:29
        ];
        let stored = vec![row(54_900, 10.0, 12.0, 9.0, 11.0, 10, 4)];
        let (findings, counts) = compare_tf(TfIndex::H1, &ones, &stored, DAY_START, false);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(counts.buckets_compared, 1);
    }

    #[test]
    fn test_compare_tf_tick_count_divergence_is_soft() {
        let ones = vec![row(33_300, 1.0, 1.0, 1.0, 1.0, 1, 5)];
        let stored = vec![row(33_300, 1.0, 1.0, 1.0, 1.0, 1, 6)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, false);
        assert!(findings.is_empty(), "soft signal must not page");
        assert_eq!(counts.soft_tick_count, 1);
    }

    #[test]
    fn test_compare_tf_volume_overflow_degrades_window() {
        let ones = vec![
            row(33_300, 1.0, 1.0, 1.0, 1.0, i64::MAX, 1),
            row(33_360, 1.0, 1.0, 1.0, 1.0, 1, 1),
        ];
        let stored = vec![row(33_300, 1.0, 1.0, 1.0, 1.0, 1, 1)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, false);
        assert_eq!(counts.volume_overflows, 1);
        assert_eq!(counts.buckets_compared, 0, "corrupt window not compared");
        assert!(findings.is_empty());
    }

    // -------------------------------------------------------------------
    // SQL builders
    // -------------------------------------------------------------------

    /// REGRESSION LOCK (the cross_verify 2026-07-10 lesson): micros WHERE
    /// window (16 digits for a 2026 day), nanos-rescaled key, feed filter,
    /// explicit LIMIT. Digit-magnitude semantics, not substring presence.
    #[test]
    fn test_select_1m_sql_micros_window_nanos_key_feed_filter_and_limit() {
        let day_start_ist_nanos = 1_784_005_200_000_000_000_i64;
        let sql = select_1m_sql("dhan", 13, "IDX_I", day_start_ist_nanos);
        let start_micros = day_start_ist_nanos / 1_000;
        let end_micros = start_micros + 86_400 * MICROS_PER_SEC;
        assert_eq!(start_micros.to_string().len(), 16, "16-digit micros bound");
        assert!(sql.contains(&format!("ts >= {start_micros}")), "{sql}");
        assert!(sql.contains(&format!("ts < {end_micros}")), "{sql}");
        assert!(
            !sql.contains(&day_start_ist_nanos.to_string()),
            "19-digit nanos literal banned: {sql}"
        );
        assert!(sql.contains("(ts / 1) * 1000 AS ts_nanos"), "{sql}");
        assert!(sql.contains("FROM candles_1m"), "{sql}");
        assert!(sql.contains("security_id = 13"), "{sql}");
        assert!(sql.contains("segment = 'IDX_I'"), "{sql}");
        assert!(sql.contains("feed = 'dhan'"), "{sql}");
        assert!(sql.contains("ORDER BY ts ASC"), "{sql}");
        assert!(sql.contains("LIMIT 500"), "explicit LIMIT tripwire: {sql}");
        assert!(sql.contains("tick_count"), "{sql}");
    }

    #[test]
    fn test_select_tf_union_sql_has_19_arms_excludes_1m_and_1d() {
        let sql = select_tf_union_sql("groww", 1333, "NSE_EQ", 1_784_005_200_000_000_000);
        assert_eq!(sql.matches(" UNION ALL ").count(), 18, "19 arms");
        for tf in tf_verify_targets() {
            assert!(
                sql.contains(&format!("FROM {}", tf.table_name())),
                "missing arm for {}: {sql}",
                tf.display_name()
            );
            assert!(
                sql.contains(&format!("'{}' AS tf", tf.display_name())),
                "{sql}"
            );
        }
        assert!(
            !sql.contains("FROM candles_1m "),
            "1m is the baseline: {sql}"
        );
        assert!(!sql.contains("candles_1d"), "D1 excluded by design: {sql}");
        assert!(sql.contains("feed = 'groww'"), "{sql}");
        // M4: the LIMIT must bind to the WHOLE union via the wrapped
        // subquery shape (the discovery-builder precedent) — a bare LIMIT
        // after the last UNION ALL arm binds unreliably on QuestDB.
        assert!(
            sql.starts_with("SELECT * FROM ("),
            "union must be wrapped so LIMIT binds to the whole union: {sql}"
        );
        assert!(
            sql.ends_with(") LIMIT 2000"),
            "explicit whole-union LIMIT tripwire: {sql}"
        );
    }

    #[test]
    fn test_select_instruments_sql_unions_all_20_tables_with_limit() {
        let sql = select_instruments_sql("dhan", 1_784_005_200_000_000_000);
        assert!(sql.starts_with("SELECT DISTINCT security_id, segment FROM ("));
        assert_eq!(sql.matches(" UNION ALL ").count(), 19, "1m + 19 targets");
        assert!(sql.contains("FROM candles_1m"), "{sql}");
        assert!(sql.contains("FROM candles_4h"), "{sql}");
        assert!(!sql.contains("candles_1d"), "{sql}");
        assert!(sql.contains("feed = 'dhan'"), "{sql}");
        assert!(sql.ends_with("LIMIT 3000"), "{sql}");
    }

    // -------------------------------------------------------------------
    // Parsers
    // -------------------------------------------------------------------

    #[test]
    fn test_parse_1m_dataset_happy_path_skips_malformed_flags_cap() {
        let body = r#"{"dataset":[
            [1780380120000000000,100.0,101.0,99.0,100.5,10,3],
            ["bad row"],
            [1780380180000000000,101.0,102.0,100.0,101.5,20,4]
        ]}"#;
        let (rows, truncated) = parse_1m_dataset(body, 500).expect("parse");
        assert_eq!(rows.len(), 2);
        assert!(!truncated);
        assert_eq!(rows[0].tick_count, 3);
        // Malformed body errors (never a silent empty pass).
        assert!(parse_1m_dataset("not json", 500).is_err());
        assert!(parse_1m_dataset("{}", 500).is_err());
        // At-cap dataset flags the truncation tripwire.
        let two_rows = r#"{"dataset":[
            [1,1.0,1.0,1.0,1.0,1,1],
            [2,1.0,1.0,1.0,1.0,1,1]
        ]}"#;
        let (_, truncated) = parse_1m_dataset(two_rows, 2).expect("parse");
        assert!(truncated, "returned == LIMIT must trip");
    }

    #[test]
    fn test_parse_tf_union_dataset_groups_by_label_and_flags_cap() {
        let body = r#"{"dataset":[
            ["5m",1780380120000000000,100.0,101.0,99.0,100.5,10,3],
            ["30m",1780380120000000000,100.0,102.0,98.0,101.0,60,12],
            ["bogus",1,1.0,1.0,1.0,1.0,1,1]
        ]}"#;
        let (rows, truncated) = parse_tf_union_dataset(body, 2_000).expect("parse");
        assert!(!truncated);
        assert_eq!(rows.len(), 2, "unknown tf label skipped");
        assert_eq!(rows[0].0, TfIndex::M5);
        assert_eq!(rows[1].0, TfIndex::M30);
        assert!(parse_tf_union_dataset("nope", 10).is_err());
        let (_, truncated) = parse_tf_union_dataset(body, 3).expect("parse");
        assert!(truncated);
    }

    #[test]
    fn test_parse_instruments_dataset_rows_and_cap() {
        let body = r#"{"dataset":[[13,"IDX_I"],[1333,"NSE_EQ"],["bad"]]}"#;
        let (rows, truncated) = parse_instruments_dataset(body, 3_000).expect("parse");
        assert_eq!(
            rows,
            vec![(13, "IDX_I".to_string()), (1333, "NSE_EQ".to_string())]
        );
        assert!(!truncated);
        let (_, truncated) = parse_instruments_dataset(body, 3).expect("parse");
        assert!(truncated);
        assert!(parse_instruments_dataset("x", 10).is_err());
    }

    // -------------------------------------------------------------------
    // Status classification
    // -------------------------------------------------------------------

    #[test]
    fn test_classify_run_status_precedence() {
        use RunStatus::{Blind, Degraded, MismatchFound, NoData, Pass};
        // compared == 0: rows seen (or any degrade) → Blind; clean-empty →
        // NoData (Rule 11: never a pass on an empty compare set).
        assert_eq!(classify_run_status(0, 0, false, true), Blind);
        assert_eq!(classify_run_status(0, 0, true, false), Blind);
        assert_eq!(
            classify_run_status(0, 5, false, true),
            Blind,
            "compared==0 wins"
        );
        assert_eq!(classify_run_status(0, 0, false, false), NoData);
        // Coverage: findings beat degraded; degraded beats pass.
        assert_eq!(classify_run_status(10, 1, true, true), MismatchFound);
        assert_eq!(classify_run_status(10, 0, true, true), Degraded);
        assert_eq!(classify_run_status(10, 0, false, true), Pass);
    }

    #[test]
    fn test_run_status_as_str_labels_stable() {
        assert_eq!(RunStatus::Pass.as_str(), "pass");
        assert_eq!(RunStatus::MismatchFound.as_str(), "mismatch");
        assert_eq!(RunStatus::Degraded.as_str(), "degraded");
        assert_eq!(RunStatus::Blind.as_str(), "blind");
        assert_eq!(RunStatus::NoData.as_str(), "no_data");
    }

    #[test]
    fn test_combine_statuses_matrix() {
        use RunStatus::{Blind, Degraded, MismatchFound, NoData, Pass};
        assert_eq!(combine_statuses(NoData, NoData), NoData);
        assert_eq!(combine_statuses(Blind, NoData), Blind);
        assert_eq!(combine_statuses(Blind, Blind), Blind);
        assert_eq!(combine_statuses(MismatchFound, Blind), MismatchFound);
        assert_eq!(combine_statuses(Pass, MismatchFound), MismatchFound);
        assert_eq!(
            combine_statuses(Pass, Blind),
            Degraded,
            "one leg unreadable"
        );
        assert_eq!(combine_statuses(Degraded, Pass), Degraded);
        assert_eq!(combine_statuses(Pass, Pass), Pass);
        assert_eq!(
            combine_statuses(Pass, NoData),
            Pass,
            "feed-off leg never demotes"
        );
        assert_eq!(combine_statuses(NoData, Pass), Pass);
    }

    #[test]
    fn test_pass_stats_paging_findings_sums_paging_classes_only() {
        let stats = PassStats {
            mismatch_cells: 1,
            missing_tf_rows: 2,
            no_coverage: 3,
            off_grid: 4,
            duplicates: 5,
            bucket_gaps: 100,
            soft_tick_count: 100,
            ..PassStats::default()
        };
        assert_eq!(stats.paging_findings(), 15, "gaps + soft never page");
    }

    // -------------------------------------------------------------------
    // Rendering helpers
    // -------------------------------------------------------------------

    #[test]
    fn test_format_ist_12h_bands() {
        assert_eq!(format_ist_12h(33_300), "9:15 AM");
        assert_eq!(format_ist_12h(35_100), "9:45 AM");
        assert_eq!(format_ist_12h(43_200), "12:00 PM");
        assert_eq!(format_ist_12h(54_900), "3:15 PM");
        assert_eq!(format_ist_12h(55_740), "3:29 PM");
        assert_eq!(format_ist_12h(0), "12:00 AM");
    }

    #[test]
    fn test_detail_line_wordings_per_category() {
        let base = FindingDraft {
            tf: "5m",
            bucket_secs_of_day: 36_900, // 10:15 AM
            category: FindingCategory::Mismatch,
            field: "high",
            stored_value: 25_647.5,
            recomputed_value: 25_647.0,
            stored_volume: 11,
            recomputed_volume: 10,
            one_m_rows: 5,
        };
        let line = detail_line("dhan", "IDX_I", 13, &base);
        assert_eq!(
            line,
            "dhan IDX_I id 13 5m 10:15 AM: high stored 25647.50 recomputed 25647.00"
        );
        let vol = FindingDraft {
            field: "volume",
            ..base.clone()
        };
        assert!(detail_line("dhan", "IDX_I", 13, &vol).contains("volume stored 11 recomputed 10"));
        let missing = FindingDraft {
            category: FindingCategory::MissingTfRow,
            field: "n/a",
            ..base.clone()
        };
        assert!(detail_line("groww", "NSE_EQ", 1333, &missing).contains("candle missing"));
        let no_cov = FindingDraft {
            category: FindingCategory::No1mCoverage,
            field: "n/a",
            ..base.clone()
        };
        assert!(detail_line("dhan", "IDX_I", 13, &no_cov).contains("no 1-minute data"));
        let off = FindingDraft {
            category: FindingCategory::OffGridTs,
            field: "n/a",
            ..base.clone()
        };
        assert!(detail_line("dhan", "IDX_I", 13, &off).contains("off the 9:15 grid"));
        let dup = FindingDraft {
            category: FindingCategory::DuplicateKey,
            field: "n/a",
            ..base
        };
        assert!(detail_line("dhan", "IDX_I", 13, &dup).contains("duplicate rows"));
    }

    #[test]
    fn test_always_on_string_set_maps_codes_to_segment_strings() {
        let mut raw: HashSet<(u64, u8)> = HashSet::new();
        raw.insert((5024, 0)); // GIFT Nifty, IDX_I
        raw.insert((42, 2)); // NSE_FNO
        let mapped = always_on_string_set(&raw);
        assert!(mapped.contains(&(5024, "IDX_I".to_string())));
        assert!(mapped.contains(&(42, "NSE_FNO".to_string())));
        assert_eq!(mapped.len(), 2);
        assert!(always_on_string_set(&HashSet::new()).is_empty());
    }

    // -------------------------------------------------------------------
    // Session-constant drift pin (runtime twin of the const asserts)
    // -------------------------------------------------------------------

    #[test]
    fn test_session_bounds_agree_with_common_constants() {
        assert_eq!(SESSION_OPEN_SECS_OF_DAY_IST, 33_300);
        assert_eq!(SESSION_CLOSE_SECS_OF_DAY_IST, 55_800);
        assert_eq!(
            i64::from(SESSION_OPEN_SECS_OF_DAY_IST) * NANOS_PER_SEC,
            MARKET_OPEN_IST_NANOS
        );
        assert_eq!(
            i64::from(SESSION_CLOSE_SECS_OF_DAY_IST) * NANOS_PER_SEC,
            MARKET_CLOSE_IST_NANOS
        );
    }

    #[test]
    fn test_run_budget_and_caps_are_sane() {
        // The budget ends the worst case at ~15:55 IST, before the 16:30
        // auto-stop; the caps sit strictly above the arithmetic maxima.
        assert_eq!(TF_VERIFY_RUN_BUDGET_SECS, 900);
        assert!(TF_VERIFY_1M_ROW_LIMIT > 375);
        assert!(TF_VERIFY_TF_UNION_ROW_LIMIT > 903);
        assert!(TF_VERIFY_DISCOVERY_ROW_LIMIT > 1_200);
        assert_eq!(TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN, 10_000);
        assert_eq!(TF_VERIFY_PREV_DAY_LOOKBACK_DAYS, 7);
        assert_eq!(TF_VERIFY_MAX_RESPONSE_BYTES, 8 * 1024 * 1024);
    }

    // -------------------------------------------------------------------
    // H1 — Groww final-window tail carve-out
    // -------------------------------------------------------------------

    /// H1: with the Groww carve-out ON, an ABSENT stored row on the FINAL
    /// window counts `tail_unsealed` (no finding, no page) — the final
    /// buckets never seal on the prod schedule.
    #[test]
    fn test_compare_tf_groww_final_missing_is_tail_unsealed_not_paging() {
        // M5 final window is [15:25, 15:30) — start 55_500.
        let ones = vec![row(55_500, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &[], DAY_START, true);
        assert!(
            findings.is_empty(),
            "groww final-window absence must not page: {findings:?}"
        );
        assert_eq!(counts.tail_unsealed, 1, "counted, never silent");
        assert_eq!(counts.buckets_compared, 0);
    }

    /// H1: the carve-out is FINAL-window-only — a missing NON-final Groww
    /// row is still the genuine dead-seal-leg `missing_tf_row` page.
    #[test]
    fn test_compare_tf_groww_nonfinal_missing_still_pages() {
        let ones = vec![row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &[], DAY_START, true);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::MissingTfRow);
        assert_eq!(counts.tail_unsealed, 0);
    }

    /// H1: Dhan (carve-out OFF) keeps paging a missing FINAL row — the
    /// 15:30:05 close-time force-seal covers Dhan finals, so absence is a
    /// real lost seal.
    #[test]
    fn test_compare_tf_dhan_final_missing_still_pages() {
        let ones = vec![row(55_500, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &[], DAY_START, false);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::MissingTfRow);
        assert_eq!(findings[0].bucket_secs_of_day, 55_500);
        assert_eq!(counts.tail_unsealed, 0);
    }

    /// H1 widened (refuter round 2): the 2m PENULTIMATE window
    /// `[15:27, 15:29)` has E = 55_740 and 55_740 + 60 ≥ 55_800, so it can
    /// never catch-up-seal intraday — an ABSENT Groww stored row there is
    /// `tail_unsealed`, NOT a paging `missing_tf_row`.
    #[test]
    fn test_compare_tf_groww_2m_penultimate_missing_is_tail_unsealed() {
        // 1m member inside the M2 penultimate window [55_620, 55_740).
        let ones = vec![row(55_620, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M2, &ones, &[], DAY_START, true);
        assert!(
            findings.is_empty(),
            "groww 2m penultimate absence must not page: {findings:?}"
        );
        assert_eq!(counts.tail_unsealed, 1, "counted, never silent");
        assert_eq!(counts.buckets_compared, 0);
    }

    /// H1 widened: an EARLIER 2m window (`[15:25, 15:27)`, E = 55_620;
    /// 55_620 + 60 < 55_800) is catch-up-able — an absent Groww row there
    /// still pages `missing_tf_row`.
    #[test]
    fn test_compare_tf_groww_2m_earlier_window_missing_still_pages() {
        let ones = vec![row(55_500, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M2, &ones, &[], DAY_START, true);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::MissingTfRow);
        assert_eq!(findings[0].bucket_secs_of_day, 55_500);
        assert_eq!(counts.tail_unsealed, 0);
    }

    /// H1 widened: the 5m PENULTIMATE window `[15:20, 15:25)` has
    /// E = 55_500; 55_500 + 60 < 55_800 → NOT exempt — an absent Groww row
    /// there still pages (the widening is margin-exact, not
    /// last-two-windows).
    #[test]
    fn test_compare_tf_groww_5m_penultimate_missing_still_pages() {
        let ones = vec![row(55_200, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &[], DAY_START, true);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::MissingTfRow);
        assert_eq!(findings[0].bucket_secs_of_day, 55_200);
        assert_eq!(counts.tail_unsealed, 0);
    }

    /// Refuter round 2: the pure tail predicate — boundary-exact against
    /// the 60s catch-up margin.
    #[test]
    fn test_groww_uncatchupable_tail_boundaries() {
        // Final windows always end at 15:30:00 → exempt.
        assert!(groww_uncatchupable_tail(SESSION_CLOSE_SECS_OF_DAY_IST));
        // Exactly margin before the close (E = 55_740) → exempt.
        assert!(groww_uncatchupable_tail(
            SESSION_CLOSE_SECS_OF_DAY_IST - GROWW_CATCHUP_MARGIN_SECS
        ));
        // One second earlier (E = 55_739) → catch-up-able, NOT exempt.
        assert!(!groww_uncatchupable_tail(
            SESSION_CLOSE_SECS_OF_DAY_IST - GROWW_CATCHUP_MARGIN_SECS - 1
        ));
        // Mid-session windows are never exempt.
        assert!(!groww_uncatchupable_tail(
            SESSION_OPEN_SECS_OF_DAY_IST + 300
        ));
    }

    /// TRIPWIRE (refuter round 2): the verifier's margin mirror must equal
    /// the aggregator's own Groww catch-up lateness margin — editing either
    /// constant alone fails the build.
    #[test]
    fn test_groww_catchup_margin_matches_aggregator_constant() {
        assert_eq!(
            GROWW_CATCHUP_MARGIN_SECS,
            tickvault_trading::candles::CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW,
            "GROWW_CATCHUP_MARGIN_SECS drifted from the aggregator's \
             CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW"
        );
    }

    /// H1: a PRESENT Groww final row is sealed data — it is compared
    /// normally and a mismatch still pages even with the carve-out ON.
    #[test]
    fn test_compare_tf_groww_final_present_mismatch_still_pages() {
        let ones = vec![row(55_500, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        // Stored final row exists but its high disagrees.
        let stored = vec![row(55_500, 100.0, 105.0, 99.0, 100.5, 10, 3)];
        let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, true);
        assert_eq!(counts.buckets_compared, 1);
        assert_eq!(counts.tail_unsealed, 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::Mismatch);
        assert_eq!(findings[0].field, "high");
    }

    // -------------------------------------------------------------------
    // H2 — state-independent always-on exclusion
    // -------------------------------------------------------------------

    /// H2: an out-of-session 1m row marks the instrument always-on from
    /// the DATA alone — no registry set involved, so the exclusion holds
    /// even on FAST-arm boots where `always_on::current()` is empty.
    #[test]
    fn test_has_out_of_session_1m_row_detects_always_on_without_registry() {
        // 03:00 IST — the GIFT-Nifty overnight signature.
        let overnight = vec![row(3 * 3600, 1.0, 1.0, 1.0, 1.0, 0, 1)];
        assert!(has_out_of_session_1m_row(&overnight, DAY_START));
        // Boundary: exactly 15:30:00 is OUT of session.
        let at_close = vec![row(55_800, 1.0, 1.0, 1.0, 1.0, 0, 1)];
        assert!(has_out_of_session_1m_row(&at_close, DAY_START));
        // Boundary: 09:14:59 is OUT; 09:15:00 and 15:29:00 are IN.
        let pre_open = vec![row(33_299, 1.0, 1.0, 1.0, 1.0, 0, 1)];
        assert!(has_out_of_session_1m_row(&pre_open, DAY_START));
        let in_session = vec![
            row(33_300, 1.0, 1.0, 1.0, 1.0, 0, 1),
            row(55_740, 1.0, 1.0, 1.0, 1.0, 0, 1),
        ];
        assert!(!has_out_of_session_1m_row(&in_session, DAY_START));
        assert!(!has_out_of_session_1m_row(&[], DAY_START));
    }

    /// H2: a NORMAL instrument (all 1m rows in-session) is NOT excluded —
    /// an out-of-grid STORED TF row still pages off_grid.
    #[test]
    fn test_in_session_1m_with_off_grid_stored_still_pages_off_grid() {
        let ones = vec![row(33_300, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        assert!(
            !has_out_of_session_1m_row(&ones, DAY_START),
            "in-session 1m rows must not trip the always-on exclusion"
        );
        // A 09:16:00 "5m" stored row is off the 09:15-anchored grid —
        // still a paging finding (the exclusion never swallows off_grid
        // for normal instruments), carve-out flag irrelevant.
        let stored = vec![row(33_360, 100.0, 101.0, 99.0, 100.5, 10, 3)];
        let (findings, _) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, true);
        assert!(
            findings
                .iter()
                .any(|f| f.category == FindingCategory::OffGridTs),
            "{findings:?}"
        );
    }

    // -------------------------------------------------------------------
    // L8 — segment allowlist (second-order injection defense)
    // -------------------------------------------------------------------

    #[test]
    fn test_is_allowlisted_segment_exact_allowlist_and_poison() {
        // Exactly the 8 strings segment_code_to_str can produce.
        for seg in [
            "IDX_I",
            "NSE_EQ",
            "NSE_FNO",
            "NSE_CURRENCY",
            "BSE_EQ",
            "MCX_COMM",
            "BSE_CURRENCY",
            "BSE_FNO",
        ] {
            assert!(is_allowlisted_segment(seg), "{seg} must be allowlisted");
        }
        // The unknown-arm fallback string is deliberately NOT allowlisted.
        assert!(!is_allowlisted_segment("UNKNOWN"));
        // A poisoned DB value must be refused, never interpolated.
        assert!(!is_allowlisted_segment("x' OR '1'='1"));
        assert!(!is_allowlisted_segment("IDX_I'; DROP TABLE ticks; --"));
        assert!(!is_allowlisted_segment(""));
        assert!(!is_allowlisted_segment("nse_eq"), "case-sensitive");
    }

    // -------------------------------------------------------------------
    // SEC — response body cap classify arm
    // -------------------------------------------------------------------

    #[test]
    fn test_response_exceeds_cap_boundary() {
        assert!(!response_exceeds_cap(0));
        assert!(!response_exceeds_cap(TF_VERIFY_MAX_RESPONSE_BYTES));
        assert!(response_exceeds_cap(TF_VERIFY_MAX_RESPONSE_BYTES + 1));
        assert!(response_exceeds_cap(usize::MAX));
    }

    // -------------------------------------------------------------------
    // L7(a) — forced-run refusal decision
    // -------------------------------------------------------------------

    #[test]
    fn test_forced_run_refusal_variants() {
        // NOW + DATE for a PAST trading day (validated separately) →
        // never refused here, at ANY time of day.
        assert_eq!(forced_run_refusal(true, false, true, 10 * 3600), None);
        assert_eq!(forced_run_refusal(true, false, false, 10 * 3600), None);
        // NOW without DATE on a non-trading day → refused.
        assert!(forced_run_refusal(false, false, false, 10 * 3600).is_some());
        // NOW without DATE on a trading day BEFORE 15:40 → refused (the
        // day's candles are still unsealed).
        let before = forced_run_refusal(false, false, true, TF_VERIFY_TRIGGER_SECS_OF_DAY_IST - 1);
        assert!(before.is_some_and(|r| r.contains("unsealed")), "{before:?}");
        // NOW without DATE AT/AFTER the 15:40 trigger on a trading day →
        // allowed (today is sealed).
        assert_eq!(
            forced_run_refusal(false, false, true, TF_VERIFY_TRIGGER_SECS_OF_DAY_IST),
            None
        );
        assert_eq!(forced_run_refusal(false, false, true, 17 * 3600), None);
    }

    /// L7-completion (refuter round 2): `NOW` + `DATE=today` on a trading
    /// day BEFORE the 15:40 trigger was a documented-refusal BYPASS (the
    /// date validator accepts `d <= today`; the old refusal short-circuited
    /// on any date override). It must now refuse exactly like bare `NOW`.
    #[test]
    fn test_forced_run_refusal_date_today_before_trigger_bypass_closed() {
        let refused = forced_run_refusal(
            true, // has_date_override
            true, // date_override_is_today
            true, // trading day
            TF_VERIFY_TRIGGER_SECS_OF_DAY_IST - 1,
        );
        assert!(
            refused.is_some_and(|r| r.contains("unsealed")),
            "DATE=today before the trigger must be refused: {refused:?}"
        );
    }

    /// L7-completion: `NOW` + `DATE=today` AT/AFTER the 15:40 trigger on a
    /// trading day stays ALLOWED (today is sealed — a same-day backfill).
    #[test]
    fn test_forced_run_refusal_date_today_after_trigger_allowed() {
        assert_eq!(
            forced_run_refusal(true, true, true, TF_VERIFY_TRIGGER_SECS_OF_DAY_IST),
            None
        );
        assert_eq!(forced_run_refusal(true, true, true, 17 * 3600), None);
        // A PAST date before the trigger remains unchanged (allowed).
        assert_eq!(
            forced_run_refusal(true, false, true, TF_VERIFY_TRIGGER_SECS_OF_DAY_IST - 1),
            None
        );
    }

    // -------------------------------------------------------------------
    // Structural ratchets (M3 / L5 / H2 / L8 wiring — source-order scans
    // over the PRODUCTION regions of this file; the extraction markers are
    // first-occurrence anchors, which always land in production code
    // because the tests sit at the end of the file)
    // -------------------------------------------------------------------

    const OWN_SRC: &str = include_str!("tf_consistency_boot.rs");

    fn run_tf_pass_body() -> &'static str {
        let start = OWN_SRC
            .find("async fn run_tf_pass")
            .expect("run_tf_pass must exist");
        let end = OWN_SRC
            .find("fn emit_pass_degrade_signal")
            .expect("emit_pass_degrade_signal must exist");
        assert!(start < end, "run_tf_pass must precede its degrade helper");
        &OWN_SRC[start..end]
    }

    /// M3: the cumulative-stats counters are emitted ONCE per pass — never
    /// inside the per-instrument loop (which double-counts quadratically).
    #[test]
    fn test_ratchet_pass_counters_emitted_once_outside_instrument_loop() {
        let body = run_tf_pass_body();
        let loop_start = body
            .find("for (idx, (sid, segment)) in instruments.iter()")
            .expect("per-instrument loop must exist");
        let post_loop = body
            .find("// M3:")
            .expect("the M3 once-per-pass emission block must exist");
        assert!(loop_start < post_loop);
        let loop_region = &body[loop_start..post_loop];
        for counter in [
            "tv_tf_verify_buckets_compared_total",
            "tv_tf_verify_bucket_gap_total",
            "tv_tf_verify_tail_unsealed_total",
        ] {
            assert!(
                !loop_region.contains(counter),
                "{counter} must not be emitted inside the per-instrument loop"
            );
            assert_eq!(
                body.matches(counter).count(),
                1,
                "{counter} must be emitted exactly once per pass"
            );
        }
    }

    /// L5: the runs-verdict counter is never emitted from the per-pass
    /// path — only after the final flush feeds the classification.
    #[test]
    fn test_ratchet_runs_counter_emitted_after_final_flush() {
        assert!(
            !run_tf_pass_body().contains("tv_tf_verify_runs_total"),
            "runs verdict must not be emitted per pass (pre-flush)"
        );
        let flush_at = OWN_SRC
            .find("state.writer.flush()")
            .expect("final flush must exist");
        let runs_at = OWN_SRC
            .find(r#""tv_tf_verify_runs_total", "status" => dhan_status"#)
            .expect("post-flush runs emission must exist");
        assert!(
            flush_at < runs_at,
            "the runs verdict counters must be emitted AFTER the final flush"
        );
    }

    /// H2 + L8 wiring order inside the per-instrument sweep: the segment
    /// allowlist gate precedes ANY query build, the data-derived
    /// always-on exclusion precedes the Query-A truncation tripwire
    /// (refuter round 3 — a truncated always-on instrument must be
    /// EXCLUDED, never degraded), Query B, and all classification.
    #[test]
    fn test_ratchet_exclusion_gates_precede_queries_and_classification() {
        let body = run_tf_pass_body();
        let allowlist = body
            .find("is_allowlisted_segment(segment)")
            .expect("L8 allowlist gate must exist");
        let q_a = body.find("select_1m_sql(").expect("Query A must exist");
        let out_of_session = body
            .find("has_out_of_session_1m_row(&rows_1m")
            .expect("H2 data-derived exclusion must exist");
        let q_b = body
            .find("select_tf_union_sql(")
            .expect("Query B must exist");
        let classify = body
            .find("dedup_sorted_rows(")
            .expect("classification start must exist");
        assert!(allowlist < q_a, "L8: allowlist before any interpolation");
        assert!(out_of_session < q_b, "H2: exclusion before Query B");
        assert!(
            out_of_session < classify,
            "H2: exclusion before ANY classification (incl. off_grid)"
        );
        // Refuter round 3: within the per-instrument loop, the FIRST
        // "truncated" classification is the Query-A tripwire — it must sit
        // AFTER the data-derived exclusion, otherwise GIFT Nifty's ~1,260
        // 1m rows (> the 500 LIMIT) turn every FAST-arm boot into a
        // guaranteed daily Degraded page.
        let loop_start = body
            .find("for (idx, (sid, segment)) in instruments.iter()")
            .expect("per-instrument loop must exist");
        let loop_body = &body[loop_start..];
        let out_of_session_in_loop = loop_body
            .find("has_out_of_session_1m_row(&rows_1m")
            .expect("H2 exclusion must be inside the loop");
        // Final-refuter hardening: anchor the Query-A tripwire on its
        // literal `if truncated_1m` guard, NOT on the first generic
        // "truncated" classification — otherwise deleting the Query-A
        // tripwire would let this ratchet match the Query-B occurrence
        // and pass vacuously.
        let query_a_tripwire = loop_body
            .find("if truncated_1m")
            .expect("the Query-A truncation tripwire (`if truncated_1m`) must exist");
        assert!(
            out_of_session_in_loop < query_a_tripwire,
            "refuter round 3: the always-on exclusion must precede the \
             Query-A truncation tripwire (a truncated always-on \
             instrument is excluded, never degraded)"
        );
        // Belt-and-braces: BOTH in-loop truncation classifications
        // (Query A + Query B) must exist — a refactor deleting either
        // tripwire fails here instead of passing vacuously.
        let in_loop_truncated_count = loop_body
            .matches(r#"count_query_failure(&mut stats, "truncated")"#)
            .count();
        assert_eq!(
            in_loop_truncated_count, 2,
            "exactly two in-loop truncation tripwires expected (Query A \
             + Query B); found {in_loop_truncated_count}"
        );
    }

    /// Refuter round 3: a >LIMIT always-on instrument (GIFT Nifty's ~21h
    /// day) parses truncated=true AND its returned prefix still carries
    /// out-of-session rows (ORDER BY ts ASC sorts pre-open rows first;
    /// the session window holds at most 375 rows < the LIMIT) — so the
    /// exclusion decision fires and the instrument is skipped WITHOUT a
    /// truncation degrade or page.
    #[test]
    fn test_truncated_always_on_prefix_still_excluded_not_degraded() {
        // Simulate the truncated response of a >LIMIT always-on day: the
        // first returned rows are pre-open (00:00 IST onward), exactly as
        // ORDER BY ts ASC delivers them. LIMIT 4 stands in for the 500.
        let limit = 4;
        let body = format!(
            r#"{{"dataset":[
                [{a},1.0,1.0,1.0,1.0,0,1],
                [{b},1.0,1.0,1.0,1.0,0,1],
                [{c},1.0,1.0,1.0,1.0,0,1],
                [{d},1.0,1.0,1.0,1.0,0,1]
            ]}}"#,
            a = DAY_START,
            b = DAY_START + 60 * NANOS_PER_SEC,
            c = DAY_START + 33_300 * NANOS_PER_SEC,
            d = DAY_START + 33_360 * NANOS_PER_SEC,
        );
        let (rows, truncated) = parse_1m_dataset(&body, limit).expect("parse");
        assert!(truncated, "at-LIMIT response must flag the tripwire");
        assert!(
            has_out_of_session_1m_row(&rows, DAY_START),
            "the truncated prefix must still expose the out-of-session \
             rows, so the exclusion (NOT a degrade) decides the instrument"
        );
        // The real-limit arithmetic backing the reasoning: a truncated
        // (>= LIMIT-row) response cannot be all in-session (375 max).
        assert!(TF_VERIFY_1M_ROW_LIMIT > 375);
    }
}
