//! BruteX↔TickVault daily 1-minute cross-verify — the 15:50 IST runner
//! (Unit 7, the I/O shell over the pure `brutex_crossverify_compare` core).
//!
//! Flow (once per trading day at `[brutex_crossverify]
//! trigger_secs_of_day_ist`, default 15:50 IST):
//!
//! 1. QuestDB reads (`/exec`, micros-literal WHERE bounds — the
//!    cross_verify_1m regression lock): the `feed='groww'`
//!    `instrument_lifecycle` mapping rows + the day's `candles_1m`
//!    (`feed='groww'`) minute bars.
//! 2. S3 (`aws-sdk-s3`, ap-south-1): paginated ListObjectsV2 under
//!    `<prefix>/<YYYY-MM-DD>/`, bounded GetObject per `.csv` key (size cap,
//!    bounded attempts + backoff), optional best-effort `_MANIFEST.json`
//!    count check. An empty listing re-polls every `repoll_interval_secs`
//!    until `deadline_secs_of_day_ist`, then records NO_DATA honestly.
//! 3. `compare_day` (pure) → cell findings + daily outcome; ONE
//!    `error!(code = BRUTEX-XVERIFY-01)` when any beyond-tolerance OHLC cell
//!    exists; every degraded leg is a stage-tagged BRUTEX-XVERIFY-02.
//! 4. Keep-better daily guard: a MEASURED existing run row
//!    (`clean|diverged|partial`) is never overwritten by an evidence-less
//!    one (`no_data|blind|degraded`) — the write is suppressed with
//!    `stage="outcome_regression"` (the scoreboard precedent).
//! 5. Persist cells + per-symbol + `__RUN__` daily rows (deterministic run
//!    `ts` = 15:50:00 IST of the trading day so re-runs UPSERT in place;
//!    `observed_at` = the actual wall clock, one value per run) and send the
//!    Telegram `BrutexCrossverifySummary` (gated on `telegram_enabled`).
//!
//! The whole subsystem is COLD-PATH forensic aggregation — never on the
//! tick hot path, the order path, or any feed's recovery path. Honest
//! envelope: presence in `candles_1m` is what WE captured; segment labels
//! must agree between `instrument_lifecycle` and the Groww candle rows for
//! a symbol to pair (a mismatch surfaces as `blind`, never a false clean).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{FixedOffset, NaiveDate, TimeZone, Timelike, Utc};
use tracing::{error, info, warn};

use tickvault_common::config::{ApplicationConfig, BrutexCrossverifyConfig, QuestDbConfig};
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::brutex_crossverify_persistence::{
    BRUTEX_CROSSVERIFY_DAILY_TABLE, BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
    BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL, BrutexCrossverifyCellKind, BrutexCrossverifyCellRow,
    BrutexCrossverifyDailyOutcome, BrutexCrossverifyDailyRow, BrutexCrossverifyField,
    BrutexCrossverifyWriter, ensure_brutex_crossverify_tables, keep_better_blocks_downgrade,
};

use crate::brutex_crossverify_compare::{
    BarKey, CellFinding, CompareCfg, DayComparison, GrowwSymbolMap, LifecycleRow, MinuteBar,
    Resolution, SymbolTally, compare_day, parse_brutex_csv,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// HTTP timeout for the QuestDB `/exec` reads (cold path).
const HTTP_TIMEOUT_SECS: u64 = 30;

/// Deterministic run-row seconds-of-day (15:50:00 IST) — the daily rows'
/// designated `ts` uses THIS constant (never the configured trigger) so a
/// re-run / backfill always UPSERTs the same row.
pub const BRUTEX_XVERIFY_RUN_ROW_SECS_OF_DAY_IST: i64 = 57_000;

/// Fallback trigger when `[brutex_crossverify] trigger_secs_of_day_ist` is
/// out of range (15:50:00 IST).
const DEFAULT_TRIGGER_SECS_OF_DAY_IST: u32 = 57_000;

/// Accepted-row cap per CSV object (a 5 MiB object cannot legitimately
/// carry more; bounds memory on a hostile body).
const MAX_ROWS_PER_CSV: usize = 200_000;

/// Cap on persisted CELL rows per run (a fully-misaligned ~770-symbol day
/// would otherwise emit ~500K+ ILP rows; the truncation is counted in the
/// run-row note — the daily aggregates stay exact).
const MAX_CELL_ROWS_PERSISTED: usize = 50_000;

/// Aggregate accepted-row ceiling across ALL folded CSVs in one run —
/// defense-in-depth on top of the per-object [`MAX_ROWS_PER_CSV`] cap.
/// A hostile/runaway producer publishing MANY near-cap objects (the
/// 2,000-key list cap × 200K rows/object = 400M rows) could otherwise
/// fold unbounded rows into RAM; a legitimate ~770-symbol day is ~290K
/// rows, so 1M gives >3x headroom while bounding worst-case memory.
const MAX_TOTAL_FOLDED_ROWS: usize = 1_000_000;

/// Pure decision: has the aggregate folded-row ceiling been reached?
/// Inclusive at the cap so the fold stops BEFORE the next object.
fn total_row_cap_reached(total_rows_folded: usize) -> bool {
    total_rows_folded >= MAX_TOTAL_FOLDED_ROWS
}

/// Base backoff between S3 GetObject attempts (doubles per attempt).
const S3_RETRY_BASE_MS: u64 = 500;

/// The best-effort manifest object's filename.
const MANIFEST_FILENAME: &str = "_MANIFEST.json";

/// Force-run env toggle (`=1` / `=true`): run the cross-verify immediately.
pub const BRUTEX_XVERIFY_FORCE_ENV: &str = "TICKVAULT_BRUTEX_XVERIFY_NOW";

/// Optional backfill date env (`YYYY-MM-DD`), honored ONLY with the force
/// toggle; strict fail-closed validation.
pub const BRUTEX_XVERIFY_DATE_ENV: &str = "TICKVAULT_BRUTEX_XVERIFY_DATE";

const NANOS_PER_SEC: i64 = 1_000_000_000;
const MICROS_PER_DAY: i64 = 86_400_000_000;

// ---------------------------------------------------------------------------
// Pure — scheduling decisions
// ---------------------------------------------------------------------------

/// Decision for WHEN the daily cross-verify should fire (the
/// `feed_scoreboard_boot::ScoreboardStart` idiom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XverifyStart {
    /// Not a trading day and not forced → do not run.
    SkipNonTradingDay,
    /// Past the trigger on a trading-day boot → run once, immediately.
    RunCatchUp,
    /// Operator forced an on-demand run.
    RunNow,
    /// Sleep this many seconds, then run at the trigger.
    SleepThenRun(u64),
}

/// Pure decision: when should the daily cross-verify fire.
#[must_use]
pub fn decide_brutex_xverify_start(
    now_secs_of_day_ist: u32,
    is_trading_day: bool,
    force_now: bool,
    trigger_secs_of_day_ist: u32,
) -> XverifyStart {
    if force_now {
        return XverifyStart::RunNow;
    }
    if !is_trading_day {
        return XverifyStart::SkipNonTradingDay;
    }
    if now_secs_of_day_ist >= trigger_secs_of_day_ist {
        return XverifyStart::RunCatchUp;
    }
    XverifyStart::SleepThenRun(u64::from(trigger_secs_of_day_ist - now_secs_of_day_ist))
}

/// Validate the configured trigger at spawn (the scoreboard
/// `sanitize_scoreboard_trigger` lesson: a typo'd value ≥ 86400 sleeps past
/// the 16:30 auto-stop forever — silent teardown). Out-of-range falls back
/// to 15:50 IST; returns `(effective, was_invalid)`.
#[must_use]
pub fn sanitize_xverify_trigger(configured_secs_of_day_ist: u32) -> (u32, bool) {
    if configured_secs_of_day_ist < 86_400 {
        (configured_secs_of_day_ist, false)
    } else {
        (DEFAULT_TRIGGER_SECS_OF_DAY_IST, true)
    }
}

/// Strict `YYYY-MM-DD` parse for the backfill env override (fail-closed —
/// anything else yields `None` and the run is REFUSED loudly).
#[must_use]
pub fn parse_xverify_date_override(raw: &str) -> Option<(NaiveDate, String)> {
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
    let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    Some((date, raw.to_owned()))
}

/// Re-poll decision while the day's S3 listing is empty: `Some(wait_secs)`
/// to sleep-and-retry, `None` when the wall-clock deadline has passed.
#[must_use]
pub fn next_poll_wait(
    now_secs_of_day_ist: u32,
    deadline_secs_of_day_ist: u32,
    repoll_interval_secs: u64,
) -> Option<u64> {
    if now_secs_of_day_ist >= deadline_secs_of_day_ist {
        return None;
    }
    let remaining = u64::from(deadline_secs_of_day_ist - now_secs_of_day_ist);
    Some(repoll_interval_secs.min(remaining).max(1))
}

// ---------------------------------------------------------------------------
// Pure — time helpers
// ---------------------------------------------------------------------------

/// IST-midnight nanos for a trading date (the repo's IST-wall-clock-epoch
/// `ts` representation).
#[must_use]
pub fn ist_midnight_nanos(date: NaiveDate) -> i64 {
    let Some(epoch) = NaiveDate::from_ymd_opt(1970, 1, 1) else {
        return 0;
    };
    date.signed_duration_since(epoch)
        .num_days()
        .saturating_mul(86_400)
        .saturating_mul(NANOS_PER_SEC)
}

/// A TIMESTAMP column's micros value → `YYYY-MM-DD` (IST-wall-clock epoch
/// convention — no offset applied). `None` for pre-epoch values.
#[must_use]
pub fn micros_to_ymd(micros: i64) -> Option<String> {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let days_u64 = u64::try_from(days).ok()?;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)?;
    let date = epoch.checked_add_days(chrono::Days::new(days_u64))?;
    Some(date.format("%Y-%m-%d").to_string())
}

/// Rupees (f64) → integer paise (`round(v * 100)`); `None` for non-finite
/// or absurd (>1e7 rupees) values — the parse-side rejection mirrored on
/// the live side so junk can never become a paise cell.
#[must_use]
pub fn rupees_to_paise(rupees: f64) -> Option<i64> {
    if !rupees.is_finite() || rupees.abs() > 10_000_000.0 {
        return None;
    }
    // APPROVED: |rupees| ≤ 1e7 so paise ≤ 1e9 — exact in f64, fits i64.
    #[allow(clippy::cast_possible_truncation)]
    Some((rupees * 100.0).round() as i64)
}

// ---------------------------------------------------------------------------
// Pure — SQL builders (micros-literal lock)
// ---------------------------------------------------------------------------

/// The `instrument_lifecycle` mapping SELECT (`feed='groww'`, live rows
/// only). `expiry_date` is projected as an INTEGER micros value
/// (`(expiry_date / 1)`) so the parser never touches ISO strings.
#[must_use]
pub fn lifecycle_select_sql() -> String {
    let feed = Feed::Groww.as_str();
    format!(
        "SELECT security_id, exchange_segment, instrument_type, symbol_name, \
         underlying_symbol, (expiry_date / 1) AS expiry_micros \
         FROM instrument_lifecycle \
         WHERE feed = '{feed}' AND dry_run = false"
    )
}

/// The day's live `candles_1m` SELECT (`feed='groww'`).
///
/// REGRESSION LOCK (the `cross_verify_1m_boot::our_candles_select_sql`
/// lesson, empirically confirmed on QuestDB 9.3.5): a bare integer literal
/// compared against a TIMESTAMP column is epoch MICROSECONDS — the WHERE
/// bounds are `nanos / 1000`; and `(ts / 1)` yields micros, so the
/// projected key is re-scaled `* 1000` back to nanos. Pinned by the
/// digit-magnitude test below.
#[must_use]
pub fn live_candles_select_sql(day_start_ist_nanos: i64) -> String {
    let a = day_start_ist_nanos / 1_000;
    let b = a.saturating_add(MICROS_PER_DAY);
    let feed = Feed::Groww.as_str();
    format!(
        "SELECT (ts / 1) * 1000 AS ts_nanos, security_id, segment, open, high, \
         low, close, volume, tick_count \
         FROM candles_1m \
         WHERE feed = '{feed}' AND ts >= {a} AND ts < {b} ORDER BY ts ASC"
    )
}

/// The keep-better read: today's existing `__RUN__` daily-row outcome
/// (micros literal on the `trading_date_ist` TIMESTAMP column).
#[must_use]
pub fn existing_run_outcome_sql(day_start_ist_nanos: i64) -> String {
    let d = day_start_ist_nanos / 1_000;
    let feed = Feed::Groww.as_str();
    format!(
        "SELECT outcome FROM {BRUTEX_CROSSVERIFY_DAILY_TABLE} \
         WHERE feed = '{feed}' AND symbol = '{BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL}' \
         AND trading_date_ist = {d} LIMIT 1"
    )
}

// ---------------------------------------------------------------------------
// Pure — /exec dataset parsers
// ---------------------------------------------------------------------------

/// One live `candles_1m` row projection.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveCandleRow {
    /// IST minute-bucket nanos (already re-scaled from the micros key).
    pub minute_nanos: i64,
    /// Instrument security id.
    pub security_id: i64,
    /// Exchange segment wire label.
    pub segment: String,
    /// OHLC in rupees (converted to paise at fold time).
    pub open: f64,
    /// High, rupees.
    pub high: f64,
    /// Low, rupees.
    pub low: f64,
    /// Close, rupees.
    pub close: f64,
    /// Minute volume (0 on the Groww LTP-only feed).
    pub volume: i64,
    /// Live tick count for the minute.
    pub tick_count: i64,
}

/// Parse the lifecycle `/exec` dataset into [`LifecycleRow`]s. Row order:
/// `[security_id, exchange_segment, instrument_type, symbol_name,
/// underlying_symbol, expiry_micros]`. Fail-soft per row.
#[must_use]
pub fn parse_lifecycle_dataset(body: &str) -> Vec<LifecycleRow> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 6 {
            continue;
        }
        let Some(security_id) = cols[0].as_i64() else {
            continue;
        };
        let seg = cols[1].as_str().unwrap_or_default();
        let itype = cols[2].as_str().unwrap_or_default();
        let symbol = cols[3].as_str().unwrap_or_default();
        if seg.is_empty() || itype.is_empty() || symbol.is_empty() {
            continue;
        }
        let underlying = cols[4].as_str().unwrap_or_default();
        let expiry_ymd = cols[5].as_i64().and_then(micros_to_ymd);
        out.push(LifecycleRow {
            security_id,
            exchange_segment: seg.to_owned(),
            instrument_type: itype.to_owned(),
            symbol_name: symbol.to_owned(),
            underlying_symbol: underlying.to_owned(),
            expiry_ymd,
        });
    }
    out
}

/// Parse the live `candles_1m` `/exec` dataset. Row order:
/// `[ts_nanos, security_id, segment, open, high, low, close, volume,
/// tick_count]`. Fail-soft per row.
#[must_use]
pub fn parse_live_candles_dataset(body: &str) -> Vec<LiveCandleRow> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 9 {
            continue;
        }
        let (Some(ts), Some(sid), Some(seg), Some(o), Some(h), Some(l), Some(c)) = (
            cols[0].as_i64(),
            cols[1].as_i64(),
            cols[2].as_str(),
            cols[3].as_f64(),
            cols[4].as_f64(),
            cols[5].as_f64(),
            cols[6].as_f64(),
        ) else {
            continue;
        };
        let volume = cols[7]
            .as_i64()
            .or_else(|| cols[7].as_f64().map(|f| f as i64))
            .unwrap_or(0);
        let tick_count = cols[8]
            .as_i64()
            .or_else(|| cols[8].as_f64().map(|f| f as i64))
            .unwrap_or(0);
        out.push(LiveCandleRow {
            minute_nanos: ts,
            security_id: sid,
            segment: seg.to_owned(),
            open: o,
            high: h,
            low: l,
            close: c,
            volume,
            tick_count,
        });
    }
    out
}

/// Parse the keep-better read body: the first row's first column (the
/// existing run-row outcome label), if any.
#[must_use]
pub fn parse_existing_outcome(body: &str) -> Option<String> {
    let v = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let first = v.get("dataset")?.as_array()?.first()?;
    let label = first.as_array()?.first()?.as_str()?;
    Some(label.to_owned())
}

// ---------------------------------------------------------------------------
// Pure — S3 key helpers
// ---------------------------------------------------------------------------

/// The day's S3 listing prefix: `<prefix>/<YYYY-MM-DD>/` (a trailing `/`
/// on the configured prefix is tolerated).
#[must_use]
pub fn day_prefix(prefix: &str, date_label: &str) -> String {
    let trimmed = prefix.trim_end_matches('/');
    format!("{trimmed}/{date_label}/")
}

/// `true` for keys the runner fetches as BruteX CSVs (case-insensitive
/// `.csv` suffix). The segment path component in the key is ADVISORY only —
/// symbols come from the CSV rows and the mapping decides identity.
#[must_use]
pub fn key_is_csv(key: &str) -> bool {
    key.len() >= 4 && key[key.len() - 4..].eq_ignore_ascii_case(".csv")
}

/// `true` for the day's best-effort manifest object.
#[must_use]
// TEST-EXEMPT: covered by test_key_is_csv_and_manifest (same-module tests).
pub fn key_is_manifest(key: &str) -> bool {
    key.rsplit('/')
        .next()
        .is_some_and(|f| f == MANIFEST_FILENAME)
}

/// Parse the best-effort manifest body: `{"files": N}`.
#[must_use]
pub fn parse_manifest_files(body: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("files")?
        .as_u64()
}

// ---------------------------------------------------------------------------
// Pure — outcome folding + summary shaping
// ---------------------------------------------------------------------------

/// Fold the measured comparison outcome with the run's partial-evidence
/// flag: a CLEAN verdict over partial coverage is honestly `partial`
/// (audit Rule 11); every other measured/unmeasured outcome stands.
#[must_use]
pub fn fold_outcome(
    measured: BrutexCrossverifyDailyOutcome,
    run_partial: bool,
) -> BrutexCrossverifyDailyOutcome {
    if run_partial && measured == BrutexCrossverifyDailyOutcome::Clean {
        BrutexCrossverifyDailyOutcome::Partial
    } else {
        measured
    }
}

/// Per-symbol daily-row outcome from its tally.
#[must_use]
pub fn symbol_outcome(t: &SymbolTally) -> BrutexCrossverifyDailyOutcome {
    if t.cells_diverged > 0 {
        BrutexCrossverifyDailyOutcome::Diverged
    } else if t.minutes_compared == 0 {
        BrutexCrossverifyDailyOutcome::Blind
    } else if t.missing_live > 0 || t.missing_brutex > 0 {
        BrutexCrossverifyDailyOutcome::Partial
    } else {
        BrutexCrossverifyDailyOutcome::Clean
    }
}

/// Distinct `(symbol, minute)` pairs with ≥1 divergent cell — the
/// minute-level divergence count the Telegram digest reports.
#[must_use]
pub fn diverged_minutes(findings: &[CellFinding]) -> u64 {
    let mut seen: BTreeSet<(&str, i64)> = BTreeSet::new();
    for f in findings {
        if f.kind == BrutexCrossverifyCellKind::Diverged {
            seen.insert((f.symbol.as_str(), f.minute_nanos));
        }
    }
    seen.len() as u64
}

/// Pre-formatted top-offender lines (≤ `max` symbols with the most
/// divergent cells; empty when nothing diverged).
#[must_use]
pub fn top_offenders(
    per_symbol: &std::collections::BTreeMap<String, SymbolTally>,
    max: usize,
) -> String {
    let mut worst: Vec<(&String, &SymbolTally)> = per_symbol
        .iter()
        .filter(|(_, t)| t.cells_diverged > 0)
        .collect();
    worst.sort_by(|a, b| {
        b.1.cells_diverged
            .cmp(&a.1.cells_diverged)
            .then(a.0.cmp(b.0))
    });
    worst
        .into_iter()
        .take(max)
        .map(|(sym, t)| {
            format!(
                "{sym}: {} divergent cells across {} compared minutes",
                t.cells_diverged, t.minutes_compared
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convert one pure comparison finding into a persistence cell row.
#[must_use]
pub fn cell_row_from_finding(
    f: &CellFinding,
    ids: &HashMap<String, (i64, String)>,
    trading_date_ist_nanos: i64,
    observed_at_ist_nanos: i64,
) -> BrutexCrossverifyCellRow {
    let (security_id, segment) = ids
        .get(&f.symbol)
        .cloned()
        .unwrap_or((BRUTEX_CROSSVERIFY_MISSING_SENTINEL, "none".to_owned()));
    BrutexCrossverifyCellRow {
        ts_ist_nanos: f.minute_nanos,
        trading_date_ist_nanos,
        feed: Feed::Groww.as_str(),
        symbol: f.symbol.clone(),
        security_id,
        exchange_segment: segment,
        minute_ts_ist_nanos: f.minute_nanos,
        kind: f.kind,
        field: f.field,
        brutex_paise: f.brutex_value,
        live_paise: f.live_value,
        brutex_volume: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
        live_volume: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
        observed_at_ist_nanos,
        note: String::new(),
    }
}

/// The Telegram-facing run summary returned by the runner (the supervisor
/// task builds the `BrutexCrossverifySummary` event from it).
#[derive(Clone, Debug)]
pub struct RunSummary {
    /// `YYYY-MM-DD` trading date.
    pub trading_date_ist: String,
    /// The (folded) daily outcome.
    pub outcome: BrutexCrossverifyDailyOutcome,
    /// CSV objects parsed OK (`-1` = unknown).
    pub files_read: i64,
    /// Symbols with ≥1 compared minute (`-1` = unknown).
    pub symbols_compared: i64,
    /// Compared minutes with every field within tolerance.
    pub matched: i64,
    /// Compared minutes with a beyond-tolerance difference.
    pub diverged: i64,
    /// Live minutes BruteX lacks.
    pub missing_brutex: i64,
    /// BruteX minutes live lacks.
    pub missing_live: i64,
    /// 15:28/15:29 close-seal-timing minutes (informational).
    pub tail_unsealed: i64,
    /// BruteX symbols that resolved to no live instrument.
    pub unmapped: i64,
    /// p95 absolute price diff, paise (`-1` = not measured).
    pub noise_p95_paise: i64,
    /// Max absolute price diff, paise (`-1` = not measured).
    pub noise_max_paise: i64,
    /// Pre-formatted worst-offender lines.
    pub top_offenders: String,
    /// Plain-English context line (alignment hint / suppression note).
    pub hint: String,
}

impl RunSummary {
    /// An unmeasured-day summary (all counts `-1` sentinels except zeros
    /// that are genuinely measured-zero).
    #[must_use]
    // TEST-EXEMPT: trivial sentinel constructor; folded values covered by the pure-fn tests + used in the unmeasured-day paths.
    pub fn unmeasured(
        label: &str,
        outcome: BrutexCrossverifyDailyOutcome,
        files_read: i64,
        hint: String,
    ) -> Self {
        Self {
            trading_date_ist: label.to_owned(),
            outcome,
            files_read,
            symbols_compared: -1,
            matched: -1,
            diverged: -1,
            missing_brutex: -1,
            missing_live: -1,
            tail_unsealed: -1,
            unmapped: -1,
            noise_p95_paise: -1,
            noise_max_paise: -1,
            top_offenders: String::new(),
            hint,
        }
    }
}

// ---------------------------------------------------------------------------
// I/O shell — counters + wall clock
// ---------------------------------------------------------------------------

/// One increment of the stage-labelled degraded counter (static labels
/// only — the label set is the fixed stage vocabulary).
fn degraded_counter(stage: &'static str) {
    metrics::counter!("tv_brutex_crossverify_degraded_total", "stage" => stage).increment(1);
}

/// IST wall-clock nanos (second granularity — `observed_at` resolution).
fn now_ist_nanos() -> i64 {
    Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .saturating_mul(NANOS_PER_SEC)
}

/// IST seconds-of-day right now.
fn now_ist_secs_of_day() -> u32 {
    let secs = Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .rem_euclid(86_400);
    u32::try_from(secs).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// I/O shell — QuestDB + S3 legs (thin; the pure parts above are the tests)
// ---------------------------------------------------------------------------

// TEST-EXEMPT: thin reqwest GET against a live QuestDB /exec endpoint; the SQL builders + dataset parsers around it are unit-tested.
async fn exec_query(
    client: &reqwest::Client,
    questdb: &QuestDbConfig,
    sql: &str,
) -> Result<String, String> {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let resp = client
        .get(&url)
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    resp.text().await.map_err(|e| format!("read: {e}"))
}

// TEST-EXEMPT: AWS SDK client construction needs live credential/region resolution (the secret_manager create_ssm_client idiom).
async fn build_s3_client() -> aws_sdk_s3::Client {
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("ap-south-1"))
        .load()
        .await;
    aws_sdk_s3::Client::new(&cfg)
}

// TEST-EXEMPT: thin paginated ListObjectsV2 wrapper (needs live S3); the key filters + poll decision are unit-tested.
async fn list_day_objects(
    s3: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    max_keys: u32,
) -> Result<(Vec<(String, i64)>, bool), String> {
    let mut out: Vec<(String, i64)> = Vec::new();
    let mut token: Option<String> = None;
    let mut capped = false;
    loop {
        let mut req = s3.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(t) = token.take() {
            req = req.continuation_token(t);
        }
        let resp = req.send().await.map_err(|e| format!("list: {e}"))?;
        for obj in resp.contents() {
            if out.len() >= max_keys as usize {
                capped = true;
                break;
            }
            if let Some(key) = obj.key() {
                out.push((key.to_owned(), obj.size().unwrap_or(0)));
            }
        }
        if capped || resp.is_truncated() != Some(true) {
            break;
        }
        token = resp.next_continuation_token().map(ToOwned::to_owned);
        if token.is_none() {
            break;
        }
    }
    Ok((out, capped))
}

// TEST-EXEMPT: thin bounded-attempt GetObject wrapper (needs live S3); size guard + attempt bounds are constants, callers unit-test the folds.
async fn get_object_bytes(
    s3: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    max_bytes: u64,
    attempts: u32,
) -> Result<Vec<u8>, String> {
    let attempts = attempts.max(1);
    let mut last_err = String::new();
    for attempt in 1..=attempts {
        match s3.get_object().bucket(bucket).key(key).send().await {
            Ok(resp) => {
                if let Some(len) = resp.content_length()
                    && len > 0
                    && u64::try_from(len).unwrap_or(u64::MAX) > max_bytes
                {
                    return Err(format!("object too large: {len} bytes > cap {max_bytes}"));
                }
                match resp.body.collect().await {
                    Ok(data) => {
                        let bytes = data.into_bytes();
                        if bytes.len() as u64 > max_bytes {
                            return Err(format!(
                                "object too large after read: {} bytes > cap {max_bytes}",
                                bytes.len()
                            ));
                        }
                        return Ok(bytes.to_vec());
                    }
                    Err(e) => last_err = format!("body: {e}"),
                }
            }
            Err(e) => last_err = format!("get: {e}"),
        }
        if attempt < attempts {
            tokio::time::sleep(Duration::from_millis(
                S3_RETRY_BASE_MS.saturating_mul(u64::from(attempt)),
            ))
            .await;
        }
    }
    Err(last_err)
}

// ---------------------------------------------------------------------------
// I/O shell — daily-row persistence helpers
// ---------------------------------------------------------------------------

/// Build the `__RUN__` aggregate daily row.
#[allow(clippy::too_many_arguments)] // APPROVED: single-caller row assembly; a params struct would just relocate the arity.
#[must_use]
// TEST-EXEMPT: field-for-field struct assembly with no logic; the outcome/counts feeding it are unit-tested pure fns.
pub fn run_daily_row(
    run_ts_nanos: i64,
    trading_date_ist_nanos: i64,
    outcome: BrutexCrossverifyDailyOutcome,
    cmp: Option<&DayComparison>,
    unmapped_symbols: i64,
    objects_fetched: i64,
    run_partial: bool,
    observed_at_ist_nanos: i64,
    note: String,
) -> BrutexCrossverifyDailyRow {
    let s = BRUTEX_CROSSVERIFY_MISSING_SENTINEL;
    let (minutes, cells, diverged, miss_live, miss_bx, tail, oos) =
        cmp.map_or((s, s, s, s, s, s, s), |c| {
            (
                i64::try_from(c.minutes_compared).unwrap_or(i64::MAX),
                i64::try_from(c.cells_compared).unwrap_or(i64::MAX),
                i64::try_from(c.cells_diverged).unwrap_or(i64::MAX),
                i64::try_from(c.missing_live).unwrap_or(i64::MAX),
                i64::try_from(c.missing_brutex).unwrap_or(i64::MAX),
                i64::try_from(c.tail_unsealed).unwrap_or(i64::MAX),
                i64::try_from(c.out_of_session).unwrap_or(i64::MAX),
            )
        });
    BrutexCrossverifyDailyRow {
        ts_ist_nanos: run_ts_nanos,
        trading_date_ist_nanos,
        feed: Feed::Groww.as_str(),
        symbol: BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL.to_owned(),
        security_id: s,
        exchange_segment: "none".to_owned(),
        outcome,
        minutes_compared: minutes,
        cells_compared: cells,
        diverged_cells: diverged,
        missing_live: miss_live,
        missing_brutex: miss_bx,
        tail_unsealed: tail,
        out_of_session: oos,
        unmapped_symbols,
        objects_fetched,
        run_partial,
        observed_at_ist_nanos,
        note,
    }
}

/// Read the existing run-row outcome (keep-better guard input). A read
/// failure returns `None` with the guard OFF for the run (warned — the
/// scoreboard precedent: an unreadable guard never blocks a fresh write).
// TEST-EXEMPT: thin exec_query + unit-tested parse_existing_outcome composition.
async fn read_existing_outcome(
    client: &reqwest::Client,
    questdb: &QuestDbConfig,
    day_start_ist_nanos: i64,
) -> Option<String> {
    match exec_query(
        client,
        questdb,
        &existing_run_outcome_sql(day_start_ist_nanos),
    )
    .await
    {
        Ok(body) => parse_existing_outcome(&body),
        Err(err) => {
            warn!(
                %err,
                "brutex_crossverify: keep-better read failed — guard OFF for this run"
            );
            None
        }
    }
}

/// Write ONLY the `__RUN__` daily row (the unmeasured-day paths), honoring
/// the keep-better guard. Returns the summary unchanged.
#[allow(clippy::too_many_arguments)] // APPROVED: single-purpose unmeasured-day finalizer with two call sites.
// TEST-EXEMPT: I/O composition over unit-tested pure parts (keep_better_blocks_downgrade / run_daily_row); needs live QuestDB.
async fn record_unmeasured_day(
    client: &reqwest::Client,
    questdb: &QuestDbConfig,
    run_ts_nanos: i64,
    trading_date_ist_nanos: i64,
    outcome: BrutexCrossverifyDailyOutcome,
    objects_fetched: i64,
    note: String,
    summary: RunSummary,
) -> RunSummary {
    let existing = read_existing_outcome(client, questdb, trading_date_ist_nanos).await;
    if keep_better_blocks_downgrade(existing.as_deref(), outcome.as_str()) {
        error!(
            code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
            stage = "outcome_regression",
            existing = existing.as_deref().unwrap_or("?"),
            new = outcome.as_str(),
            "BRUTEX-XVERIFY-02: refusing to overwrite a MEASURED daily row \
             with an evidence-less one — write suppressed (keep-better guard)"
        );
        degraded_counter("outcome_regression");
        metrics::counter!("tv_brutex_crossverify_runs_total", "outcome" => outcome.as_str())
            .increment(1);
        return summary;
    }
    let mut writer = BrutexCrossverifyWriter::new(questdb);
    let row = run_daily_row(
        run_ts_nanos,
        trading_date_ist_nanos,
        outcome,
        None,
        BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
        objects_fetched,
        false,
        now_ist_nanos(),
        note,
    );
    if let Err(err) = writer.append_daily_row(&row) {
        error!(
            code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
            stage = "persist_append",
            ?err,
            "BRUTEX-XVERIFY-02: daily-row append failed"
        );
        degraded_counter("persist_append");
    } else if let Err(err) = writer.flush() {
        error!(
            code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
            stage = "persist_flush",
            ?err,
            "BRUTEX-XVERIFY-02: daily-row flush failed — the forensic row for \
             this day is missing until a re-run"
        );
        degraded_counter("persist_flush");
    }
    metrics::counter!("tv_brutex_crossverify_runs_total", "outcome" => outcome.as_str())
        .increment(1);
    summary
}

// ---------------------------------------------------------------------------
// I/O shell — the daily run
// ---------------------------------------------------------------------------

/// Run one trading day's BruteX cross-verify end-to-end. Fail-soft: every
/// degraded leg logs a stage-tagged BRUTEX-XVERIFY-02 and the day records
/// an honest outcome — this function returns `Err` only for I/O-shell
/// preconditions that make ANY record impossible (the caller pages
/// `BrutexCrossverifyAborted`).
// TEST-EXEMPT: once-per-day I/O orchestration over the unit-tested pure core (parse/map/compare/fold/SQL/keys); a direct test needs live S3 + QuestDB — covered operationally by TICKVAULT_BRUTEX_XVERIFY_NOW.
pub async fn run_brutex_crossverify(
    cfg: &BrutexCrossverifyConfig,
    questdb: &QuestDbConfig,
    trading_date: NaiveDate,
    date_label: &str,
) -> Result<RunSummary, String> {
    let day_start_nanos = ist_midnight_nanos(trading_date);
    let run_ts_nanos = day_start_nanos
        .saturating_add(BRUTEX_XVERIFY_RUN_ROW_SECS_OF_DAY_IST.saturating_mul(NANOS_PER_SEC));

    ensure_brutex_crossverify_tables(questdb).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http_client_build: {e}"))?;

    // ── Leg 1: the symbol mapping (instrument_lifecycle, feed='groww') ──
    let mapping_rows = match exec_query(&client, questdb, &lifecycle_select_sql()).await {
        Ok(body) => parse_lifecycle_dataset(&body),
        Err(err) => {
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "questdb_read",
                leg = "instrument_lifecycle",
                %err,
                "BRUTEX-XVERIFY-02: mapping read failed — the day degrades \
                 (no false-OK)"
            );
            degraded_counter("questdb_read");
            return Ok(record_unmeasured_day(
                &client,
                questdb,
                run_ts_nanos,
                day_start_nanos,
                BrutexCrossverifyDailyOutcome::Degraded,
                BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                "degraded: instrument_lifecycle read failed".to_owned(),
                RunSummary::unmeasured(
                    date_label,
                    BrutexCrossverifyDailyOutcome::Degraded,
                    -1,
                    "the live symbol mapping could not be read".to_owned(),
                ),
            )
            .await);
        }
    };
    let map = GrowwSymbolMap::from_lifecycle_rows(mapping_rows);

    // ── Leg 2: S3 listing (re-poll while empty, until the deadline) ──
    let s3 = build_s3_client().await;
    let prefix = day_prefix(&cfg.prefix, date_label);
    let mut run_partial = false;
    let mut degraded_notes: Vec<String> = Vec::new();
    let (csv_keys, manifest_key) = loop {
        match list_day_objects(&s3, &cfg.bucket, &prefix, cfg.max_keys).await {
            Ok((keys, capped)) => {
                if capped {
                    error!(
                        code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                        stage = "s3_list_capped",
                        cap = cfg.max_keys,
                        "BRUTEX-XVERIFY-02: listing exceeded the key cap — \
                         only the first {} keys are read (partial coverage)",
                        cfg.max_keys
                    );
                    degraded_counter("s3_list_capped");
                    run_partial = true;
                    degraded_notes.push(format!("listing capped at {} keys", cfg.max_keys));
                }
                let csv: Vec<(String, i64)> = keys
                    .iter()
                    .filter(|(k, _)| key_is_csv(k))
                    .cloned()
                    .collect();
                let manifest = keys
                    .iter()
                    .find(|(k, _)| key_is_manifest(k))
                    .map(|(k, _)| k.clone());
                if !csv.is_empty() {
                    break (csv, manifest);
                }
            }
            Err(err) => {
                error!(
                    code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                    stage = "s3_list",
                    %err,
                    "BRUTEX-XVERIFY-02: S3 listing failed — re-polling until \
                     the deadline"
                );
                degraded_counter("s3_list");
            }
        }
        match next_poll_wait(
            now_ist_secs_of_day(),
            cfg.deadline_secs_of_day_ist,
            cfg.repoll_interval_secs,
        ) {
            Some(wait) => tokio::time::sleep(Duration::from_secs(wait)).await,
            None => {
                info!(
                    prefix = %prefix,
                    "brutex_crossverify: no CSV objects by the deadline — \
                     recording NO_DATA for the day"
                );
                return Ok(record_unmeasured_day(
                    &client,
                    questdb,
                    run_ts_nanos,
                    day_start_nanos,
                    BrutexCrossverifyDailyOutcome::NoData,
                    0,
                    "no_data: the day's S3 prefix stayed empty through the deadline".to_owned(),
                    RunSummary::unmeasured(
                        date_label,
                        BrutexCrossverifyDailyOutcome::NoData,
                        0,
                        "no BruteX files appeared by the 16:05 IST deadline".to_owned(),
                    ),
                )
                .await);
            }
        }
    };

    // ── Leg 2b: best-effort manifest count check ──
    if let Some(mkey) = manifest_key {
        match get_object_bytes(&s3, &cfg.bucket, &mkey, cfg.max_object_bytes, 1).await {
            Ok(bytes) => {
                let body = String::from_utf8_lossy(&bytes);
                if let Some(expected) = parse_manifest_files(&body)
                    && expected != csv_keys.len() as u64
                {
                    warn!(
                        expected,
                        listed = csv_keys.len(),
                        "brutex_crossverify: manifest file-count mismatch — \
                         the day is marked partial"
                    );
                    run_partial = true;
                    degraded_notes.push(format!(
                        "manifest expected {expected} files, listed {}",
                        csv_keys.len()
                    ));
                }
            }
            Err(err) => {
                warn!(%err, "brutex_crossverify: manifest fetch failed (best-effort)");
            }
        }
    }

    // ── Leg 3: fetch + parse each CSV; resolve symbols; fold bars ──
    let mut brutex_bars: HashMap<BarKey, MinuteBar> = HashMap::new();
    let mut symbol_ids: HashMap<String, (i64, String)> = HashMap::new();
    let mut unmapped_syms: BTreeSet<String> = BTreeSet::new();
    let mut files_read: i64 = 0;
    let mut bad_rows_total: usize = 0;
    let mut total_rows_folded: usize = 0;
    for (idx, (key, size)) in csv_keys.iter().enumerate() {
        if total_row_cap_reached(total_rows_folded) {
            let skipped_objects = csv_keys.len() - idx;
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "total_row_cap",
                total_rows = total_rows_folded,
                skipped_objects,
                "BRUTEX-XVERIFY-02: aggregate folded-row ceiling reached — \
                 stopping the fold (partial coverage)"
            );
            degraded_counter("total_row_cap");
            run_partial = true;
            degraded_notes.push(format!(
                "total_row_cap: {total_rows_folded} rows folded, \
                 {skipped_objects} objects skipped"
            ));
            break;
        }
        if now_ist_secs_of_day() >= cfg.deadline_secs_of_day_ist {
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "wall_clock_cap",
                fetched = files_read,
                total = csv_keys.len(),
                "BRUTEX-XVERIFY-02: wall-clock deadline hit mid-run — \
                 classifying with what was fetched"
            );
            degraded_counter("wall_clock_cap");
            run_partial = true;
            degraded_notes.push("wall_clock_cap: deadline hit mid-fetch".to_owned());
            break;
        }
        if *size > 0 && u64::try_from(*size).unwrap_or(u64::MAX) > cfg.max_object_bytes {
            warn!(key = %key, size, "brutex_crossverify: object over size cap — skipped");
            degraded_counter("s3_object_oversize");
            run_partial = true;
            continue;
        }
        let bytes = match get_object_bytes(
            &s3,
            &cfg.bucket,
            key,
            cfg.max_object_bytes,
            cfg.fetch_attempts_per_object,
        )
        .await
        {
            Ok(b) => b,
            Err(err) => {
                error!(
                    code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                    stage = "s3_get",
                    key = %key,
                    %err,
                    "BRUTEX-XVERIFY-02: object fetch failed after bounded \
                     attempts — skipped (partial coverage)"
                );
                degraded_counter("s3_get");
                run_partial = true;
                continue;
            }
        };
        match parse_brutex_csv(&bytes, MAX_ROWS_PER_CSV) {
            Ok(parsed) => {
                files_read += 1;
                bad_rows_total += parsed.bad_rows;
                total_rows_folded = total_rows_folded.saturating_add(parsed.rows.len());
                if parsed.truncated {
                    run_partial = true;
                    degraded_notes.push(format!("{key}: row cap hit (truncated)"));
                }
                for row in parsed.rows {
                    match map.resolve(&row.symbol) {
                        Resolution::Resolved {
                            security_id,
                            segment,
                        } => {
                            symbol_ids
                                .entry(row.symbol.clone())
                                .or_insert((security_id, segment));
                            brutex_bars.insert(
                                (row.symbol, row.minute_nanos),
                                MinuteBar {
                                    open_paise: row.open_paise,
                                    high_paise: row.high_paise,
                                    low_paise: row.low_paise,
                                    close_paise: row.close_paise,
                                    volume: row.volume,
                                    tick_count: 0,
                                },
                            );
                        }
                        Resolution::Unmapped | Resolution::Ambiguous => {
                            unmapped_syms.insert(row.symbol);
                        }
                    }
                }
            }
            Err(err) => {
                error!(
                    code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                    stage = "csv_parse",
                    key = %key,
                    %err,
                    "BRUTEX-XVERIFY-02: CSV parse failed — object skipped"
                );
                degraded_counter("csv_parse");
                run_partial = true;
            }
        }
    }
    if files_read == 0 {
        return Ok(record_unmeasured_day(
            &client,
            questdb,
            run_ts_nanos,
            day_start_nanos,
            BrutexCrossverifyDailyOutcome::Degraded,
            0,
            "degraded: every listed object failed to fetch/parse".to_owned(),
            RunSummary::unmeasured(
                date_label,
                BrutexCrossverifyDailyOutcome::Degraded,
                0,
                "BruteX files were listed but none could be read".to_owned(),
            ),
        )
        .await);
    }

    // ── Leg 4: the day's live candles (feed='groww') ──
    let live_body =
        match exec_query(&client, questdb, &live_candles_select_sql(day_start_nanos)).await {
            Ok(body) => body,
            Err(err) => {
                error!(
                    code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                    stage = "questdb_read",
                    leg = "candles_1m",
                    %err,
                    "BRUTEX-XVERIFY-02: live candles read failed — the day degrades"
                );
                degraded_counter("questdb_read");
                return Ok(record_unmeasured_day(
                    &client,
                    questdb,
                    run_ts_nanos,
                    day_start_nanos,
                    BrutexCrossverifyDailyOutcome::Degraded,
                    files_read,
                    "degraded: candles_1m read failed".to_owned(),
                    RunSummary::unmeasured(
                        date_label,
                        BrutexCrossverifyDailyOutcome::Degraded,
                        files_read,
                        "the live minute candles could not be read".to_owned(),
                    ),
                )
                .await);
            }
        };
    let reverse: HashMap<(i64, &str), &String> = symbol_ids
        .iter()
        .map(|(sym, (sid, seg))| ((*sid, seg.as_str()), sym))
        .collect();
    let mut live_bars: HashMap<BarKey, MinuteBar> = HashMap::new();
    let mut live_paise_rejects: u64 = 0;
    for r in parse_live_candles_dataset(&live_body) {
        let Some(sym) = reverse.get(&(r.security_id, r.segment.as_str())) else {
            // Live instruments outside the BruteX symbol set are not part
            // of the comparison universe (BruteX decides its own coverage).
            continue;
        };
        let (Some(o), Some(h), Some(l), Some(c)) = (
            rupees_to_paise(r.open),
            rupees_to_paise(r.high),
            rupees_to_paise(r.low),
            rupees_to_paise(r.close),
        ) else {
            live_paise_rejects += 1;
            continue;
        };
        live_bars.insert(
            ((*sym).clone(), r.minute_nanos),
            MinuteBar {
                open_paise: o,
                high_paise: h,
                low_paise: l,
                close_paise: c,
                volume: r.volume,
                tick_count: r.tick_count,
            },
        );
    }
    if live_paise_rejects > 0 {
        warn!(
            live_paise_rejects,
            "brutex_crossverify: live rows with non-finite/absurd prices were skipped"
        );
    }

    // ── Leg 5: the pure comparison ──
    // Synchronous CPU work over up to ~1M folded bars — run it under
    // `block_in_place` so a worst-case compare cannot stall this tokio
    // worker's peers. `block_in_place` panics on a current_thread
    // runtime, so the flavor guard falls back to an inline call there
    // (tests / degenerate runtimes; the prod runtime is multi-thread).
    let cmp_cfg = CompareCfg {
        tolerance_paise: cfg.price_tolerance_paise,
        compare_volume: cfg.compare_volume,
    };
    let cmp = if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(|| compare_day(&brutex_bars, &live_bars, &cmp_cfg))
    } else {
        compare_day(&brutex_bars, &live_bars, &cmp_cfg)
    };
    let measured = cmp
        .outcome
        .unwrap_or(BrutexCrossverifyDailyOutcome::Degraded);
    // Files parsed but NO symbol resolved: the comparer sees two empty maps
    // (NoData) — the honest label is BLIND (we had BruteX data, nothing
    // could be paired).
    let measured = if measured == BrutexCrossverifyDailyOutcome::NoData && !unmapped_syms.is_empty()
    {
        BrutexCrossverifyDailyOutcome::Blind
    } else {
        measured
    };
    let outcome = fold_outcome(measured, run_partial);

    // ONE divergence page-feed line per run (counts in the payload).
    if cmp.cells_diverged > 0 {
        error!(
            code = ErrorCode::BrutexXverify01DivergenceFound.code_str(),
            trading_date = date_label,
            diverged_cells = cmp.cells_diverged,
            diverged_minutes = diverged_minutes(&cmp.findings),
            minutes_compared = cmp.minutes_compared,
            noise_p95_paise = cmp.noise.p95_paise,
            noise_max_paise = cmp.noise.max_paise,
            "BRUTEX-XVERIFY-01: beyond-tolerance OHLC divergence between the \
             BruteX 1-minute candles and our live capture"
        );
    }

    // ── Leg 6: keep-better guard, then persist ──
    let existing = read_existing_outcome(&client, questdb, day_start_nanos).await;
    let mut hint = cmp.alignment_hint.clone().unwrap_or_default();
    if keep_better_blocks_downgrade(existing.as_deref(), outcome.as_str()) {
        error!(
            code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
            stage = "outcome_regression",
            existing = existing.as_deref().unwrap_or("?"),
            new = outcome.as_str(),
            "BRUTEX-XVERIFY-02: refusing to overwrite a MEASURED daily row \
             with an evidence-less one — write suppressed (keep-better guard)"
        );
        degraded_counter("outcome_regression");
        if hint.is_empty() {
            hint = "write suppressed: the day already carries a measured row".to_owned();
        }
    } else {
        let observed_at = now_ist_nanos();
        let mut writer = BrutexCrossverifyWriter::new(questdb);
        let mut append_errors: u64 = 0;
        // Cells: findings (capped) + one row per unmapped symbol.
        let persisted = cmp.findings.len().min(MAX_CELL_ROWS_PERSISTED);
        if cmp.findings.len() > MAX_CELL_ROWS_PERSISTED {
            degraded_notes.push(format!(
                "cell rows capped: {} of {} persisted",
                MAX_CELL_ROWS_PERSISTED,
                cmp.findings.len()
            ));
        }
        for f in &cmp.findings[..persisted] {
            let row = cell_row_from_finding(f, &symbol_ids, day_start_nanos, observed_at);
            if writer.append_cell_row(&row).is_err() {
                append_errors += 1;
            }
        }
        for sym in &unmapped_syms {
            let row = BrutexCrossverifyCellRow {
                ts_ist_nanos: run_ts_nanos,
                trading_date_ist_nanos: day_start_nanos,
                feed: Feed::Groww.as_str(),
                symbol: sym.clone(),
                security_id: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                exchange_segment: "none".to_owned(),
                minute_ts_ist_nanos: run_ts_nanos,
                kind: BrutexCrossverifyCellKind::UnmappedSymbol,
                field: BrutexCrossverifyField::None,
                brutex_paise: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                live_paise: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                brutex_volume: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                live_volume: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                observed_at_ist_nanos: observed_at,
                note: String::new(),
            };
            if writer.append_cell_row(&row).is_err() {
                append_errors += 1;
            }
        }
        // Per-symbol daily rows.
        for (sym, tally) in &cmp.per_symbol {
            let (sid, seg) = symbol_ids
                .get(sym)
                .cloned()
                .unwrap_or((BRUTEX_CROSSVERIFY_MISSING_SENTINEL, "none".to_owned()));
            let row = BrutexCrossverifyDailyRow {
                ts_ist_nanos: run_ts_nanos,
                trading_date_ist_nanos: day_start_nanos,
                feed: Feed::Groww.as_str(),
                symbol: sym.clone(),
                security_id: sid,
                exchange_segment: seg,
                outcome: symbol_outcome(tally),
                minutes_compared: i64::try_from(tally.minutes_compared).unwrap_or(i64::MAX),
                cells_compared: i64::try_from(
                    tally.cells_matched.saturating_add(tally.cells_diverged),
                )
                .unwrap_or(i64::MAX),
                diverged_cells: i64::try_from(tally.cells_diverged).unwrap_or(i64::MAX),
                missing_live: i64::try_from(tally.missing_live).unwrap_or(i64::MAX),
                missing_brutex: i64::try_from(tally.missing_brutex).unwrap_or(i64::MAX),
                tail_unsealed: i64::try_from(tally.tail_unsealed).unwrap_or(i64::MAX),
                out_of_session: i64::try_from(tally.out_of_session).unwrap_or(i64::MAX),
                unmapped_symbols: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                objects_fetched: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                run_partial,
                observed_at_ist_nanos: observed_at,
                note: String::new(),
            };
            if writer.append_daily_row(&row).is_err() {
                append_errors += 1;
            }
        }
        // The __RUN__ aggregate row.
        let mut note_parts = degraded_notes.clone();
        if bad_rows_total > 0 {
            note_parts.push(format!("{bad_rows_total} bad csv rows skipped"));
        }
        if cmp.volume_check_noop {
            note_parts.push("volume check no-op (live volume all zero)".to_owned());
        }
        if let Some(h) = &cmp.alignment_hint {
            note_parts.push(h.clone());
        }
        let run_row = run_daily_row(
            run_ts_nanos,
            day_start_nanos,
            outcome,
            Some(&cmp),
            i64::try_from(unmapped_syms.len()).unwrap_or(i64::MAX),
            files_read,
            run_partial,
            observed_at,
            note_parts.join("; "),
        );
        if writer.append_daily_row(&run_row).is_err() {
            append_errors += 1;
        }
        if append_errors > 0 {
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "persist_append",
                append_errors,
                "BRUTEX-XVERIFY-02: {append_errors} row appends failed"
            );
            degraded_counter("persist_append");
        }
        if let Err(err) = writer.flush() {
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "persist_flush",
                ?err,
                "BRUTEX-XVERIFY-02: cross-verify row flush failed — the \
                 forensic rows for this day are missing until a re-run"
            );
            degraded_counter("persist_flush");
        }
    }

    // ── Leg 7: counters + summary ──
    metrics::counter!("tv_brutex_crossverify_runs_total", "outcome" => outcome.as_str())
        .increment(1);
    let cell_counts: [(&'static str, u64); 7] = [
        ("matched", cmp.cells_matched),
        ("diverged", cmp.cells_diverged),
        ("missing_live", cmp.missing_live),
        ("missing_brutex", cmp.missing_brutex),
        ("tail_unsealed", cmp.tail_unsealed),
        ("out_of_session", cmp.out_of_session),
        ("unmapped_symbol", unmapped_syms.len() as u64),
    ];
    for (kind, n) in cell_counts {
        if n > 0 {
            metrics::counter!("tv_brutex_crossverify_cells_total", "kind" => kind).increment(n);
        }
    }

    let div_minutes = diverged_minutes(&cmp.findings);
    let symbols_compared = cmp
        .per_symbol
        .values()
        .filter(|t| t.minutes_compared > 0)
        .count();
    let measured_any = cmp.minutes_compared > 0;
    Ok(RunSummary {
        trading_date_ist: date_label.to_owned(),
        outcome,
        files_read,
        symbols_compared: i64::try_from(symbols_compared).unwrap_or(i64::MAX),
        matched: i64::try_from(cmp.minutes_compared.saturating_sub(div_minutes))
            .unwrap_or(i64::MAX),
        diverged: i64::try_from(div_minutes).unwrap_or(i64::MAX),
        missing_brutex: i64::try_from(cmp.missing_brutex).unwrap_or(i64::MAX),
        missing_live: i64::try_from(cmp.missing_live).unwrap_or(i64::MAX),
        tail_unsealed: i64::try_from(cmp.tail_unsealed).unwrap_or(i64::MAX),
        unmapped: i64::try_from(unmapped_syms.len()).unwrap_or(i64::MAX),
        noise_p95_paise: if measured_any {
            cmp.noise.p95_paise
        } else {
            -1
        },
        noise_max_paise: if measured_any {
            cmp.noise.max_paise
        } else {
            -1
        },
        top_offenders: top_offenders(&cmp.per_symbol, 5),
        hint,
    })
}

// ---------------------------------------------------------------------------
// Spawn — scheduler + supervised wrapper (both boot paths call this)
// ---------------------------------------------------------------------------

/// Spawn the daily BruteX cross-verify scheduler (the
/// `spawn_feed_scoreboard_tasks` shape): disabled config → one `info!` and
/// nothing spawns; trading-day gate; sleep-to-trigger / late-boot catch-up;
/// `TICKVAULT_BRUTEX_XVERIFY_NOW=1` force-run (+ optional strict
/// `TICKVAULT_BRUTEX_XVERIFY_DATE=YYYY-MM-DD` backfill, refused fail-closed
/// on a malformed date or a non-trading target). The inner run's
/// `Err`/panic pages `BrutexCrossverifyAborted`; cancellation is silent.
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested pure decision fns (decide_brutex_xverify_start / parse_xverify_date_override / sanitize_xverify_trigger); spawn sites live in main.rs next to the scoreboard's.
pub fn spawn_brutex_crossverify_task(
    config: &ApplicationConfig,
    trading_calendar: &Arc<TradingCalendar>,
    notifier: &Arc<NotificationService>,
) {
    if !config.brutex_crossverify.enabled {
        info!("brutex_crossverify: disabled by [brutex_crossverify] config — nothing spawned");
        return;
    }
    let bx_cfg = config.brutex_crossverify.clone();
    let qcfg = config.questdb.clone();
    let cal = Arc::clone(trading_calendar);
    let telegram_enabled = bx_cfg.telegram_enabled;
    let bx_notifier = Arc::clone(notifier);

    let inner = tokio::spawn(async move {
        let Some(ist) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
            return Err("ist_offset_construction_failed".to_owned());
        };
        let boot_ist = ist.from_utc_datetime(&Utc::now().naive_utc());
        let today_ist = boot_ist.date_naive();
        let boot_secs = boot_ist.time().num_seconds_from_midnight();
        let force_now = std::env::var(BRUTEX_XVERIFY_FORCE_ENV)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        // Backfill date override — honored ONLY with the force toggle;
        // strict fail-closed validation (a malformed / non-trading target
        // REFUSES the run loudly, never aggregates the wrong day).
        let date_override: Option<(NaiveDate, String)> = if force_now {
            match std::env::var(BRUTEX_XVERIFY_DATE_ENV) {
                Ok(raw) => match parse_xverify_date_override(&raw) {
                    Some(v) => Some(v),
                    None => {
                        error!(
                            code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                            stage = "date_override_parse",
                            raw = %raw,
                            "BRUTEX-XVERIFY-02: invalid {BRUTEX_XVERIFY_DATE_ENV} \
                             — expected strict YYYY-MM-DD; refusing the forced run"
                        );
                        return Err("invalid_date_override".to_owned());
                    }
                },
                Err(_) => None,
            }
        } else {
            None
        };
        if force_now && date_override.is_none() && !cal.is_trading_day(today_ist) {
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "forced_now_non_trading",
                date = %today_ist.format("%Y-%m-%d"),
                "BRUTEX-XVERIFY-02: {BRUTEX_XVERIFY_FORCE_ENV} without a date \
                 on a non-trading day — refusing (pass \
                 {BRUTEX_XVERIFY_DATE_ENV}=YYYY-MM-DD for the trading day you \
                 meant)"
            );
            return Err("forced_now_non_trading".to_owned());
        }
        if let Some((d, label)) = &date_override
            && !cal.is_trading_day(*d)
        {
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "date_override_reject",
                date = %label,
                "BRUTEX-XVERIFY-02: {BRUTEX_XVERIFY_DATE_ENV} is not a \
                 trading day — refusing the forced backfill"
            );
            return Err("date_override_non_trading".to_owned());
        }
        let (trigger, trig_invalid) = sanitize_xverify_trigger(bx_cfg.trigger_secs_of_day_ist);
        if trig_invalid {
            error!(
                code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                stage = "trigger_config",
                configured = bx_cfg.trigger_secs_of_day_ist,
                effective = trigger,
                "BRUTEX-XVERIFY-02: trigger_secs_of_day_ist out of range — \
                 falling back to 15:50 IST"
            );
        }
        let decision = decide_brutex_xverify_start(
            boot_secs,
            cal.is_trading_day(today_ist),
            force_now,
            trigger,
        );
        match decision {
            XverifyStart::SkipNonTradingDay => {
                info!("brutex_crossverify: skipping (non-trading day)");
                return Ok(None);
            }
            XverifyStart::RunCatchUp => {
                info!(
                    now = %boot_ist.time(),
                    "brutex_crossverify: late boot (past the trigger) — \
                     running the day's cross-verify now as a catch-up"
                );
            }
            XverifyStart::RunNow => {
                info!(
                    backfill_date = date_override.as_ref().map_or("today", |(_, l)| l.as_str()),
                    "brutex_crossverify: force env set — running on-demand NOW"
                );
            }
            XverifyStart::SleepThenRun(secs) => {
                info!(secs, "brutex_crossverify: sleeping until the daily trigger");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
        // Target date: recompute after any sleep (SleepThenRun is same-day
        // by construction; the immediate paths recompute to the same day).
        let (date, label) = match date_override {
            Some((d, l)) => (d, l),
            None => {
                let run_ist = ist.from_utc_datetime(&Utc::now().naive_utc());
                let d = run_ist.date_naive();
                (d, d.format("%Y-%m-%d").to_string())
            }
        };
        run_brutex_crossverify(&bx_cfg, &qcfg, date, &label)
            .await
            .map(Some)
    });

    tokio::spawn(async move {
        let today_label = || {
            let secs = Utc::now()
                .timestamp()
                .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
            let days = secs.div_euclid(86_400);
            u64::try_from(days)
                .ok()
                .and_then(|d| {
                    NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_days(chrono::Days::new(d))
                })
                .map_or_else(
                    || "unknown".to_owned(),
                    |d| d.format("%Y-%m-%d").to_string(),
                )
        };
        match inner.await {
            Ok(Ok(Some(s))) => {
                if telegram_enabled {
                    bx_notifier.notify(NotificationEvent::BrutexCrossverifySummary {
                        trading_date_ist: s.trading_date_ist,
                        outcome: s.outcome.as_str().to_owned(),
                        files_read: s.files_read,
                        symbols_compared: s.symbols_compared,
                        matched: s.matched,
                        diverged: s.diverged,
                        missing_brutex: s.missing_brutex,
                        missing_live: s.missing_live,
                        tail_unsealed: s.tail_unsealed,
                        unmapped: s.unmapped,
                        noise_p95_paise: s.noise_p95_paise,
                        noise_max_paise: s.noise_max_paise,
                        top_offenders: s.top_offenders,
                        hint: s.hint,
                    });
                } else {
                    info!("brutex_crossverify: Telegram disabled — forensic rows written only");
                }
            }
            Ok(Ok(None)) => {} // non-trading day skip — nothing to send.
            Ok(Err(reason)) => {
                error!(
                    code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                    stage = "daily_run",
                    %reason,
                    "BRUTEX-XVERIFY-02: the daily BruteX cross-verify run failed"
                );
                bx_notifier.notify(NotificationEvent::BrutexCrossverifyAborted {
                    trading_date_ist: today_label(),
                    reason,
                });
            }
            Err(join_err) if join_err.is_panic() => {
                error!(
                    code = ErrorCode::BrutexXverify02RunDegraded.code_str(),
                    stage = "daily_panic",
                    %join_err,
                    "BRUTEX-XVERIFY-02: the daily BruteX cross-verify task crashed"
                );
                bx_notifier.notify(NotificationEvent::BrutexCrossverifyAborted {
                    trading_date_ist: today_label(),
                    reason: "task_panicked".to_owned(),
                });
            }
            Err(_) => {} // cancellation (shutdown teardown) — silent by design.
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── aggregate row ceiling (FIX 2, review round 1) ──

    #[test]
    fn test_total_row_cap_reached_boundaries() {
        assert!(!total_row_cap_reached(0), "empty fold never capped");
        assert!(
            !total_row_cap_reached(MAX_TOTAL_FOLDED_ROWS - 1),
            "one below the ceiling keeps folding"
        );
        assert!(
            total_row_cap_reached(MAX_TOTAL_FOLDED_ROWS),
            "inclusive at the ceiling — the NEXT object is skipped"
        );
        assert!(
            total_row_cap_reached(usize::MAX),
            "saturated accumulator stays capped"
        );
    }

    #[test]
    fn test_total_row_cap_leaves_headroom_over_a_legit_day() {
        // A legitimate ~770-symbol day is ~290K rows (770 × 375 minutes);
        // the ceiling must not trip on real traffic.
        assert!(MAX_TOTAL_FOLDED_ROWS >= 3 * 770 * 375);
        // ...but must be far below the hostile worst case
        // (2,000 keys × MAX_ROWS_PER_CSV).
        assert!(MAX_TOTAL_FOLDED_ROWS < 2_000 * MAX_ROWS_PER_CSV);
    }

    // ── scheduling ──

    #[test]
    fn test_decide_brutex_xverify_start_all_arms() {
        assert_eq!(
            decide_brutex_xverify_start(1_000, false, true, 57_000),
            XverifyStart::RunNow,
            "force wins over non-trading"
        );
        assert_eq!(
            decide_brutex_xverify_start(1_000, false, false, 57_000),
            XverifyStart::SkipNonTradingDay
        );
        assert_eq!(
            decide_brutex_xverify_start(57_001, true, false, 57_000),
            XverifyStart::RunCatchUp
        );
        assert_eq!(
            decide_brutex_xverify_start(56_900, true, false, 57_000),
            XverifyStart::SleepThenRun(100)
        );
    }

    #[test]
    fn test_sanitize_xverify_trigger_bounds() {
        assert_eq!(sanitize_xverify_trigger(57_000), (57_000, false));
        assert_eq!(sanitize_xverify_trigger(0), (0, false));
        assert_eq!(sanitize_xverify_trigger(86_400), (57_000, true));
        assert_eq!(sanitize_xverify_trigger(u32::MAX), (57_000, true));
    }

    #[test]
    fn test_parse_xverify_date_override_strict_fail_closed() {
        let (d, label) = parse_xverify_date_override("2026-07-10").expect("valid");
        assert_eq!(label, "2026-07-10");
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 7, 10).expect("date"));
        for bad in [
            "2026-7-10",
            "2026/07/10",
            "2026-13-01",
            "20260710",
            "../etc",
            "2026-07-10x",
            "",
        ] {
            assert!(
                parse_xverify_date_override(bad).is_none(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn test_next_poll_wait_deadline_semantics() {
        assert_eq!(next_poll_wait(57_000, 57_900, 120), Some(120));
        assert_eq!(
            next_poll_wait(57_850, 57_900, 120),
            Some(50),
            "clamped to remaining"
        );
        assert_eq!(next_poll_wait(57_900, 57_900, 120), None, "at deadline");
        assert_eq!(next_poll_wait(58_000, 57_900, 120), None, "past deadline");
        assert_eq!(
            next_poll_wait(57_899, 57_900, 0),
            Some(1),
            "never a busy loop"
        );
    }

    // ── time helpers ──

    #[test]
    fn test_ist_midnight_nanos_known_date() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 10).expect("date");
        // 2026-07-10 = 20_644 days since epoch.
        assert_eq!(ist_midnight_nanos(d), 20_644 * 86_400 * 1_000_000_000);
    }

    #[test]
    fn test_micros_to_ymd_roundtrip() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 10).expect("date");
        let micros = ist_midnight_nanos(d) / 1_000;
        assert_eq!(micros_to_ymd(micros).as_deref(), Some("2026-07-10"));
        // Mid-day micros floor to the same date.
        assert_eq!(
            micros_to_ymd(micros + 12 * 3_600 * 1_000_000).as_deref(),
            Some("2026-07-10")
        );
        assert!(micros_to_ymd(-1).is_none(), "pre-epoch refused");
    }

    #[test]
    fn test_rupees_to_paise_guards() {
        assert_eq!(rupees_to_paise(100.005), Some(10_001), "round half up");
        assert_eq!(rupees_to_paise(0.0), Some(0));
        assert_eq!(rupees_to_paise(f64::NAN), None);
        assert_eq!(rupees_to_paise(f64::INFINITY), None);
        assert_eq!(
            rupees_to_paise(20_000_000.0),
            None,
            "absurd magnitude refused"
        );
    }

    // ── SQL builders (micros-literal lock, digit-magnitude style) ──

    #[test]
    fn test_live_candles_select_sql_micros_window_and_nanos_key() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 10).expect("date");
        let nanos = ist_midnight_nanos(d);
        let sql = live_candles_select_sql(nanos);
        // The projected key is re-scaled back to nanos.
        assert!(
            sql.contains("(ts / 1) * 1000 AS ts_nanos"),
            "nanos key rescale: {sql}"
        );
        assert!(sql.contains("feed = 'groww'"), "feed scope: {sql}");
        assert!(sql.contains("tick_count"), "tick_count projected: {sql}");
        // Digit-magnitude: the WHERE literal is MICROS (16 digits for a
        // 2026 date), never the 19-digit nanos value.
        let lower = format!("{}", nanos / 1_000);
        assert_eq!(lower.len(), 16, "2026 micros literal is 16 digits");
        assert!(
            sql.contains(&format!("ts >= {lower}")),
            "micros lower bound: {sql}"
        );
        assert!(
            !sql.contains(&nanos.to_string()),
            "the 19-digit NANOS literal must never appear: {sql}"
        );
        let upper = format!("{}", nanos / 1_000 + 86_400_000_000);
        assert!(
            sql.contains(&format!("ts < {upper}")),
            "micros upper bound: {sql}"
        );
    }

    #[test]
    fn test_lifecycle_select_sql_shape() {
        let sql = lifecycle_select_sql();
        assert!(sql.contains("feed = 'groww'"));
        assert!(sql.contains("dry_run = false"), "§27 dry-run isolation");
        assert!(
            sql.contains("(expiry_date / 1) AS expiry_micros"),
            "expiry projected as integer micros: {sql}"
        );
    }

    #[test]
    fn test_existing_run_outcome_sql_micros_literal() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 10).expect("date");
        let nanos = ist_midnight_nanos(d);
        let sql = existing_run_outcome_sql(nanos);
        assert!(sql.contains("symbol = '__RUN__'"));
        assert!(sql.contains(&format!("trading_date_ist = {}", nanos / 1_000)));
        assert!(
            !sql.contains(&nanos.to_string()),
            "nanos literal banned: {sql}"
        );
    }

    // ── dataset parsers ──

    #[test]
    fn test_parse_lifecycle_dataset_rows_and_expiry() {
        let body = r#"{"dataset":[
            [11536,"NSE_EQ","EQUITY","TCS","",null],
            [35001,"NSE_FNO","FUTIDX","NIFTY-Jul2026-FUT","NIFTY",1785283200000000],
            [null,"NSE_EQ","EQUITY","BAD","",null],
            [1,"","EQUITY","X","",null]
        ]}"#;
        let rows = parse_lifecycle_dataset(body);
        assert_eq!(rows.len(), 2, "junk rows skipped fail-soft");
        assert_eq!(rows[0].symbol_name, "TCS");
        assert_eq!(rows[0].expiry_ymd, None);
        assert_eq!(rows[1].underlying_symbol, "NIFTY");
        // 1_785_283_200_000_000 micros = epoch day 20_663 = 2026-07-29
        // (anchor: day 20_644 = 2026-07-10, pinned in test_ist_midnight_nanos).
        assert_eq!(rows[1].expiry_ymd.as_deref(), Some("2026-07-29"));
        assert!(parse_lifecycle_dataset("not json").is_empty());
        assert!(parse_lifecycle_dataset("{}").is_empty());
    }

    #[test]
    fn test_parse_live_candles_dataset_rows() {
        let body = r#"{"dataset":[
            [1783701900000000000,11536,"NSE_EQ",100.5,101.0,100.0,100.75,0,42],
            [1783701900000000000,null,"NSE_EQ",1,2,3,4,0,0]
        ]}"#;
        let rows = parse_live_candles_dataset(body);
        assert_eq!(rows.len(), 1, "null security_id skipped");
        assert_eq!(rows[0].minute_nanos, 1_783_701_900_000_000_000);
        assert_eq!(rows[0].security_id, 11_536);
        assert_eq!(rows[0].segment, "NSE_EQ");
        assert_eq!(rows[0].tick_count, 42);
    }

    #[test]
    fn test_parse_existing_outcome_first_cell() {
        assert_eq!(
            parse_existing_outcome(r#"{"dataset":[["clean"]]}"#).as_deref(),
            Some("clean")
        );
        assert_eq!(parse_existing_outcome(r#"{"dataset":[]}"#), None);
        assert_eq!(parse_existing_outcome("junk"), None);
    }

    // ── S3 key helpers ──

    #[test]
    fn test_day_prefix_trailing_slash_tolerated() {
        assert_eq!(day_prefix("brutex", "2026-07-10"), "brutex/2026-07-10/");
        assert_eq!(day_prefix("brutex/", "2026-07-10"), "brutex/2026-07-10/");
    }

    #[test]
    fn test_key_is_csv_and_manifest() {
        assert!(key_is_csv("brutex/2026-07-10/NSE_EQ/TCS.csv"));
        assert!(key_is_csv("brutex/2026-07-10/TCS.CSV"));
        assert!(!key_is_csv("brutex/2026-07-10/_MANIFEST.json"));
        assert!(!key_is_csv("brutex/2026-07-10/"));
        assert!(!key_is_csv("csv"));
        assert!(key_is_manifest("brutex/2026-07-10/_MANIFEST.json"));
        assert!(!key_is_manifest("brutex/2026-07-10/MANIFEST.json"));
    }

    #[test]
    fn test_parse_manifest_files_shape() {
        assert_eq!(parse_manifest_files(r#"{"files": 7}"#), Some(7));
        assert_eq!(parse_manifest_files(r#"{"files": -1}"#), None);
        assert_eq!(parse_manifest_files(r#"{"count": 7}"#), None);
        assert_eq!(parse_manifest_files("junk"), None);
    }

    // ── outcome folding + summary shaping ──

    #[test]
    fn test_fold_outcome_partial_only_downgrades_clean() {
        use BrutexCrossverifyDailyOutcome as O;
        assert_eq!(fold_outcome(O::Clean, true), O::Partial);
        assert_eq!(fold_outcome(O::Clean, false), O::Clean);
        assert_eq!(fold_outcome(O::Diverged, true), O::Diverged);
        assert_eq!(fold_outcome(O::NoData, true), O::NoData);
        assert_eq!(fold_outcome(O::Blind, true), O::Blind);
    }

    #[test]
    fn test_symbol_outcome_classification() {
        use BrutexCrossverifyDailyOutcome as O;
        let mut t = SymbolTally::default();
        assert_eq!(symbol_outcome(&t), O::Blind, "nothing compared");
        t.minutes_compared = 10;
        assert_eq!(symbol_outcome(&t), O::Clean);
        t.missing_live = 1;
        assert_eq!(symbol_outcome(&t), O::Partial);
        t.cells_diverged = 1;
        assert_eq!(symbol_outcome(&t), O::Diverged, "diverged wins");
    }

    #[test]
    fn test_diverged_minutes_distinct_pairs() {
        let f = |sym: &str, minute: i64, kind| CellFinding {
            symbol: sym.to_owned(),
            minute_nanos: minute,
            kind,
            field: BrutexCrossverifyField::Open,
            brutex_value: 1,
            live_value: 2,
        };
        use BrutexCrossverifyCellKind as K;
        let findings = vec![
            f("A", 1, K::Diverged),
            f("A", 1, K::Diverged), // same minute, second field
            f("A", 2, K::Diverged),
            f("B", 1, K::MissingLive), // not a divergence
        ];
        assert_eq!(diverged_minutes(&findings), 2);
    }

    #[test]
    fn test_top_offenders_sorted_and_bounded() {
        let mut m = std::collections::BTreeMap::new();
        let t = |div: u64, cmp_min: u64| SymbolTally {
            minutes_compared: cmp_min,
            cells_diverged: div,
            ..Default::default()
        };
        m.insert("AAA".to_owned(), t(2, 10));
        m.insert("BBB".to_owned(), t(9, 10));
        m.insert("CCC".to_owned(), t(0, 10));
        let s = top_offenders(&m, 5);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2, "zero-divergence symbols excluded");
        assert!(lines[0].starts_with("BBB:"), "worst first: {s}");
        assert_eq!(top_offenders(&m, 1).lines().count(), 1, "bounded");
    }

    #[test]
    fn test_cell_row_from_finding_identity_and_sentinels() {
        let mut ids = HashMap::new();
        ids.insert("TCS".to_owned(), (11_536_i64, "NSE_EQ".to_owned()));
        let f = CellFinding {
            symbol: "TCS".to_owned(),
            minute_nanos: 123,
            kind: BrutexCrossverifyCellKind::Diverged,
            field: BrutexCrossverifyField::Close,
            brutex_value: 10_050,
            live_value: 10_060,
        };
        let row = cell_row_from_finding(&f, &ids, 100, 200);
        assert_eq!(row.security_id, 11_536);
        assert_eq!(row.exchange_segment, "NSE_EQ");
        assert_eq!(row.minute_ts_ist_nanos, 123);
        assert_eq!(row.brutex_volume, BRUTEX_CROSSVERIFY_MISSING_SENTINEL);
        // Unknown symbol → sentinels, never a guess.
        let g = CellFinding {
            symbol: "UNKNOWN".to_owned(),
            ..f
        };
        let row = cell_row_from_finding(&g, &ids, 100, 200);
        assert_eq!(row.security_id, BRUTEX_CROSSVERIFY_MISSING_SENTINEL);
        assert_eq!(row.exchange_segment, "none");
    }

    #[test]
    fn test_run_daily_row_sentinels_without_comparison() {
        let row = run_daily_row(
            1,
            2,
            BrutexCrossverifyDailyOutcome::NoData,
            None,
            -1,
            0,
            false,
            3,
            "note".to_owned(),
        );
        assert_eq!(row.symbol, BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL);
        assert_eq!(row.minutes_compared, BRUTEX_CROSSVERIFY_MISSING_SENTINEL);
        assert_eq!(row.objects_fetched, 0, "measured zero stays zero");
    }

    #[test]
    fn test_run_daily_row_carries_comparison_counts() {
        let cmp = DayComparison {
            minutes_compared: 5,
            cells_compared: 20,
            cells_diverged: 2,
            missing_live: 1,
            ..Default::default()
        };
        let row = run_daily_row(
            1,
            2,
            BrutexCrossverifyDailyOutcome::Diverged,
            Some(&cmp),
            0,
            3,
            true,
            4,
            String::new(),
        );
        assert_eq!(row.minutes_compared, 5);
        assert_eq!(row.cells_compared, 20);
        assert_eq!(row.diverged_cells, 2);
        assert_eq!(row.missing_live, 1);
        assert!(row.run_partial);
    }
}
