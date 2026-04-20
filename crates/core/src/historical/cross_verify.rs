//! Cross-verification of historical candle data integrity.
//!
//! Queries QuestDB to verify that the unified `historical_candles` table
//! has adequate coverage across all 5 timeframes (1m, 5m, 15m, 60m, 1d)
//! for every fetched instrument.
//!
//! # Verification Checks
//! 1. **Per-timeframe candle count**: each timeframe has expected minimum candles
//! 2. **OHLC consistency**: high >= low for all stored candles
//! 3. **Data integrity**: no non-positive prices (open/high/low/close <= 0)
//! 4. **Timestamp bounds**: intraday candles within 09:15–15:29 IST market hours
//! 5. **Multi-instrument coverage**: all instruments have data in all timeframes
//!
//! # Historical vs Live Cross-Match
//! After post-market re-fetch, compares `historical_candles` (Dhan REST API)
//! against materialized views (`candles_1m`, `candles_5m`, etc.) from live
//! WebSocket ticks. Detects missed ticks, price drift, and data gaps.
//!
//! # Automation
//! Runs after historical candle fetch — no human intervention required.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Utc};
use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    CANDLES_PER_TRADING_DAY, IST_UTC_OFFSET_SECONDS, QUESTDB_TABLE_HISTORICAL_CANDLES, TIMEFRAME_1M,
};
use tickvault_common::instrument_registry::InstrumentRegistry;

// ---------------------------------------------------------------------------
// Violation Detail — per-candle diagnostic record
// ---------------------------------------------------------------------------

/// A single candle that failed cross-verification, with full diagnostic context.
///
/// Used for OHLC violations, data violations, and timestamp violations.
/// Contains enough information for Telegram messages to be self-diagnostic.
#[derive(Debug, Clone)]
pub struct ViolationDetail {
    /// Human-readable symbol (e.g., "RELIANCE", "NIFTY50"). From registry lookup.
    pub symbol: String,
    /// Exchange segment string (e.g., "NSE_EQ", "IDX_I").
    pub segment: String,
    /// Timeframe (e.g., "1m", "5m", "1d").
    pub timeframe: String,
    /// Timestamp formatted as IST string (e.g., "2026-03-18 10:15").
    pub timestamp_ist: String,
    /// What's wrong (e.g., "high < low", "open <= 0", "outside market hours").
    pub violation: String,
    /// Actual OHLCV values for context (e.g., "O=0.0 H=2450.5 L=2440.0 C=2445.0").
    pub values: String,
}

// ---------------------------------------------------------------------------
// Cross-Match Types — Historical vs Live comparison
// ---------------------------------------------------------------------------

/// A single candle mismatch between historical API data and live materialized view.
#[derive(Debug, Clone)]
pub struct CrossMatchMismatch {
    /// Human-readable symbol (e.g., "RELIANCE").
    pub symbol: String,
    /// Exchange segment string (e.g., "NSE_EQ").
    pub segment: String,
    /// Timeframe (e.g., "1m", "5m").
    pub timeframe: String,
    /// Timestamp formatted as IST string.
    pub timestamp_ist: String,
    /// Type of mismatch: "price_diff", "missing_live", "missing_historical".
    pub mismatch_type: String,
    /// Historical OHLCV values (e.g., "O=2450.0 H=2465.0 L=2448.0 C=2460.0 V=125000").
    pub hist_values: String,
    /// Live OHLCV values (e.g., "O=2450.0 H=2463.5 L=2448.0 C=2460.0 V=118500"),
    /// or "\[MISSING\]" if no live data.
    pub live_values: String,
    /// Field-level diff summary (e.g., "H(-1.5) V(-6500)").
    pub diff_summary: String,
}

/// Summary of historical vs live candle cross-match.
#[derive(Debug)]
pub struct CrossMatchReport {
    /// Number of timeframes compared (up to 5).
    pub timeframes_checked: usize,
    /// Total candles compared across all timeframes.
    pub candles_compared: usize,
    /// Total mismatched candles.
    pub mismatches: usize,
    /// Historical candle exists but live doesn't (WebSocket missed ticks).
    pub missing_live: usize,
    /// Live candle exists but historical doesn't (should not happen).
    pub missing_historical: usize,
    /// OI mismatches for derivatives (both OI > 0, differ by > tolerance).
    pub oi_mismatches: usize,
    /// Detailed mismatch records (all of them — no arbitrary cap).
    pub mismatch_details: Vec<CrossMatchMismatch>,
    /// Materialized view tables that don't exist yet (skipped during cross-match).
    pub missing_views: Vec<String>,
    /// Per-timeframe mismatch counts for Telegram display (e.g., "1m: 3, 5m: 0").
    pub per_timeframe_mismatches: Vec<(String, usize)>,
    /// Whether the cross-match passed (mismatches within tolerance).
    pub passed: bool,
    /// True if at least one live materialized view contains rows in the
    /// last 3 days. When `false`, the cross-match is meaningless —
    /// the `candles_compared` counter comes from a `LEFT JOIN` that
    /// preserves historical rows even when the live side is empty, so
    /// the caller MUST treat "candles_compared > 0 but
    /// live_candles_present = false" as SKIP, not as a pass.
    pub live_candles_present: bool,
}

// ---------------------------------------------------------------------------
// Verification Result
// ---------------------------------------------------------------------------

/// Summary of cross-verification across all timeframes.
#[derive(Debug)]
pub struct CrossVerificationReport {
    /// Number of unique instruments verified.
    pub instruments_checked: usize,
    /// Number of instruments with complete 1m candle coverage.
    pub instruments_complete: usize,
    /// Number of instruments with missing candles in any timeframe.
    pub instruments_with_gaps: usize,
    /// Total candle count across all timeframes.
    pub total_candles_in_db: usize,
    /// Per-timeframe candle counts.
    pub timeframe_counts: Vec<TimeframeCoverage>,
    /// Number of candles with OHLC inconsistency (high < low).
    pub ohlc_violations: usize,
    /// Detailed OHLC violation records (capped at 20).
    pub ohlc_details: Vec<ViolationDetail>,
    /// Number of candles with non-positive prices.
    pub data_violations: usize,
    /// Detailed data violation records (capped at 20).
    pub data_details: Vec<ViolationDetail>,
    /// Number of intraday candles outside market hours.
    pub timestamp_violations: usize,
    /// Detailed timestamp violation records (capped at 20).
    pub timestamp_details: Vec<ViolationDetail>,
    /// Number of candles on weekends (Saturday/Sunday — NSE closed).
    pub weekend_violations: usize,
    /// Detailed weekend violation records (capped at 20).
    pub weekend_details: Vec<ViolationDetail>,
    /// Whether the verification passed overall.
    pub passed: bool,
}

/// Coverage summary for a single timeframe.
#[derive(Debug, Clone)]
pub struct TimeframeCoverage {
    /// Timeframe label (e.g., "1m", "5m", "15m", "60m", "1d").
    pub timeframe: String,
    /// Number of instruments with data in this timeframe.
    pub instrument_count: usize,
    /// Total candles in this timeframe.
    pub candle_count: usize,
}

// ---------------------------------------------------------------------------
// QuestDB Query Response Types
// ---------------------------------------------------------------------------

/// QuestDB HTTP API response wrapper.
#[derive(Debug, serde::Deserialize)]
struct QuestDbQueryResponse {
    dataset: Vec<Vec<serde_json::Value>>,
    #[allow(dead_code)] // APPROVED: field required for deserialization, used for debug logging
    count: usize,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB verification queries.
const VERIFY_QUERY_TIMEOUT_SECS: u64 = 30;

/// Minimum candle coverage ratio to consider an instrument "complete".
/// 95% of 375 = 356 candles minimum for the cross-verification window
/// (09:15–15:29 continuous trading session; pre-market 09:00–09:14 excluded
/// because Dhan historical API has no data for that window).
const MIN_CANDLE_COVERAGE_RATIO: f64 = 0.95;

/// All timeframes we verify.
const VERIFIED_TIMEFRAMES: &[&str] = &["1m", "5m", "15m", "60m", "1d"];

/// Maximum violation detail rows to fetch per category.
/// High cap for memory safety only — we show ALL violations, not hiding anything.
/// If there are more than 500, the count query tells you the real total.
const MAX_VIOLATION_DETAILS: usize = 500;

/// Price tolerance for cross-match comparison (absolute).
/// EXACT MATCHING: zero tolerance. Any price difference flags as mismatch.
/// f32→f64 already fixed via `f32_to_f64_clean()` — no IEEE754 noise.
const CROSS_MATCH_PRICE_EPSILON: f64 = 0.0;

/// Volume tolerance for cross-match (relative).
/// EXACT MATCHING: zero tolerance. Any volume difference flags as mismatch.
const CROSS_MATCH_VOLUME_TOLERANCE_PCT: f64 = 0.0;

/// OI (Open Interest) tolerance for cross-match (relative).
/// EXACT MATCHING: zero tolerance. Any OI difference flags as mismatch.
const CROSS_MATCH_OI_TOLERANCE_PCT: f64 = 0.0;

/// Mapping from historical timeframe labels to materialized view table names.
const CROSS_MATCH_TIMEFRAMES: &[(&str, &str)] = &[
    ("1m", "candles_1m"),
    ("5m", "candles_5m"),
    ("15m", "candles_15m"),
    ("60m", "candles_1h"),
    ("1d", "candles_1d"),
];

// ---------------------------------------------------------------------------
// Today-only SQL window (fixes "12 days accumulated data falsely passing" bug)
// ---------------------------------------------------------------------------

/// Today's IST trading-session SQL window, ready to drop into WHERE clauses.
///
/// **Why this exists:** before 2026-04-20 the cross-verify queries used a
/// rolling `ts > dateadd('d', -3, now())` 3-day window. Because QuestDB data
/// is persistent across app restarts (Docker volume), that window captured
/// ticks from MANY prior trading sessions. Cross-match compared 12+ days of
/// accumulated live ticks against the REST historical fetch — which trivially
/// "passed" because both data sources come from Dhan. The OK was legitimate
/// but meaningless for verifying TODAY's session.
///
/// `ts` in QuestDB was stored as IST-epoch (WebSocket LTT is IST epoch seconds
/// per `data-integrity.md`). A literal like `'2026-04-20T09:15:00.000000Z'`
/// is parsed by QuestDB as UTC-epoch for 09:15 on that date — which exactly
/// matches the IST-epoch representation we stored for the IST 09:15 tick,
/// because both share the same numeric value (IST wall clock treated as UTC).
///
/// The window is `[today_0915_ist, min(now_ist, today_1530_ist)]` so that:
/// - During market hours → compare only what live has produced so far
/// - Post-market → compare the full 09:15-15:30 session
/// - Pre-market (before 09:15) → `from_utc` returns `None`
///
/// Production caller builds this via [`TodayIstWindow::from_now`]. Tests
/// drive determinism via [`TodayIstWindow::from_utc`].
#[derive(Debug, Clone)]
pub struct TodayIstWindow {
    /// IST date of "today" (used for Telegram labels like "TODAY ONLY: 2026-04-20").
    pub today_ist: NaiveDate,
    /// SQL-ready timestamp literal for window start (including quotes).
    /// Example: `'2026-04-20T09:15:00.000000Z'`.
    pub start_sql: String,
    /// SQL-ready timestamp literal for window end (including quotes).
    /// Example: `'2026-04-20T15:30:00.000000Z'` or earlier during the session.
    pub end_sql: String,
}

impl TodayIstWindow {
    /// Computes today's IST trading-session window from the current UTC time.
    /// Returns `None` before 09:15 IST (no window exists yet).
    pub fn from_now() -> Option<Self> {
        Self::from_utc(Utc::now())
    }

    /// Computes today's IST trading-session window from a given UTC instant.
    /// Pure function — suitable for deterministic tests.
    pub fn from_utc(now_utc: DateTime<Utc>) -> Option<Self> {
        // APPROVED: IST_UTC_OFFSET_SECONDS (19800) is a compile-time provable
        // valid FixedOffset — same pattern as trading_calendar::ist_fixed_offset.
        let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS)?;
        let now_ist = now_utc.with_timezone(&ist);
        let today_ist = now_ist.date_naive();
        let start_naive: NaiveDateTime = today_ist.and_hms_opt(9, 15, 0)?;
        let market_end_naive: NaiveDateTime = today_ist.and_hms_opt(15, 30, 0)?;
        let now_naive = now_ist.naive_local();
        // Pre-market: nothing to compare yet.
        if now_naive < start_naive {
            return None;
        }
        let effective_end = if now_naive < market_end_naive {
            now_naive
        } else {
            market_end_naive
        };
        Some(Self {
            today_ist,
            start_sql: format!("'{}'", start_naive.format("%Y-%m-%dT%H:%M:%S.000000Z")),
            end_sql: format!("'{}'", effective_end.format("%Y-%m-%dT%H:%M:%S.000000Z")),
        })
    }

    /// Returns the SQL fragment `ts >= '...' AND ts <= '...'` suitable for
    /// dropping into any WHERE clause. Replaces the old
    /// `ts > dateadd('d', -3, now())` 3-day window.
    pub fn where_clause(&self) -> String {
        format!(
            "ts >= {start} AND ts <= {end}",
            start = self.start_sql,
            end = self.end_sql,
        )
    }

    /// Same as [`where_clause`] but prefixed with a table alias
    /// (e.g. `"h"` → `h.ts >= '...' AND h.ts <= '...'`).
    pub fn where_clause_aliased(&self, alias: &str) -> String {
        format!(
            "{alias}.ts >= {start} AND {alias}.ts <= {end}",
            start = self.start_sql,
            end = self.end_sql,
        )
    }
}

// ---------------------------------------------------------------------------
// Once-per-day success cache (Parthiban directive, 2026-04-20):
// "post verification only once per day until it achieves success"
// ---------------------------------------------------------------------------

/// Returns the on-disk marker path used to record that today's cross-
/// verification + cross-match BOTH succeeded. Filename embeds the IST
/// trading date so a single file is naturally per-day. Lives under
/// `data/cache/` alongside other one-shot daily markers.
pub fn cross_verify_success_marker_path(today_ist: NaiveDate) -> std::path::PathBuf {
    std::path::PathBuf::from("data/cache").join(format!(
        "cross-verify-success-{}.marker",
        today_ist.format("%Y-%m-%d")
    ))
}

/// `true` when today's cross-verification has already succeeded earlier
/// in the same trading day (marker file present). The caller skips the
/// whole post-market verification block when this returns true — fixes
/// the "every restart re-runs the verification even though it already
/// passed" failure mode that blew up the operator's Telegram on
/// crash-recovery boots.
pub fn cross_verify_already_succeeded_today(today_ist: NaiveDate) -> bool {
    cross_verify_success_marker_path(today_ist).exists()
}

/// Records that today's cross-verification + cross-match both passed.
/// Best-effort: returns the std::io error so the caller can decide
/// whether to surface it; not panicking lets the verification still
/// "succeed" in the operator's eyes even if disk write fails.
pub fn mark_cross_verify_success(today_ist: NaiveDate) -> std::io::Result<()> {
    let path = cross_verify_success_marker_path(today_ist);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, today_ist.to_string())
}

// ---------------------------------------------------------------------------
// Pure Helper Functions (extracted for testability)
// ---------------------------------------------------------------------------

/// Parses coverage rows into a per-timeframe map of (security_id, candle_count).
///
/// Each row is expected to have [timeframe, security_id, count].
fn parse_coverage_rows(
    dataset: &[Vec<serde_json::Value>],
) -> (HashMap<String, Vec<(i64, usize)>>, usize) {
    let mut timeframe_instruments: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
    let mut total_candles = 0_usize;

    for row in dataset {
        if row.len() < 3 {
            continue;
        }
        let tf = row[0].as_str().unwrap_or("").to_string();
        let sid = row[1].as_i64().unwrap_or(0);
        let count = row[2].as_i64().unwrap_or(0).max(0) as usize;
        total_candles = total_candles.saturating_add(count);

        timeframe_instruments
            .entry(tf)
            .or_default()
            .push((sid, count));
    }

    (timeframe_instruments, total_candles)
}

/// Builds per-timeframe coverage summaries from the parsed timeframe map.
fn build_timeframe_coverage(
    timeframe_instruments: &HashMap<String, Vec<(i64, usize)>>,
) -> Vec<TimeframeCoverage> {
    let mut timeframe_counts: Vec<TimeframeCoverage> =
        Vec::with_capacity(VERIFIED_TIMEFRAMES.len());
    for &tf in VERIFIED_TIMEFRAMES {
        let instruments = timeframe_instruments.get(tf);
        let instrument_count = instruments.map_or(0, Vec::len);
        let candle_count: usize = instruments.map_or(0, |v| v.iter().map(|(_, c)| *c).sum());
        timeframe_counts.push(TimeframeCoverage {
            timeframe: tf.to_string(),
            instrument_count,
            candle_count,
        });
    }
    timeframe_counts
}

/// Classifies instruments by 1m candle coverage completeness.
///
/// Returns `(instruments_complete, instruments_with_gaps)`.
fn classify_1m_coverage(instruments: &[(i64, usize)], min_candles: usize) -> (usize, usize) {
    let mut complete = 0_usize;
    let mut gaps = 0_usize;

    for &(_sid, count) in instruments {
        if count >= min_candles {
            complete = complete.saturating_add(1);
        } else {
            gaps = gaps.saturating_add(1);
        }
    }

    (complete, gaps)
}

/// Collects unique instrument IDs across all timeframes.
fn count_unique_instruments(timeframe_instruments: &HashMap<String, Vec<(i64, usize)>>) -> usize {
    let mut all_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for instruments in timeframe_instruments.values() {
        for &(sid, _) in instruments {
            all_ids.insert(sid);
        }
    }
    all_ids.len()
}

/// Determines if the cross-verification report should pass.
///
/// FAIL if: zero instruments OR any gaps OR any violations of any type.
fn determine_verification_passed(
    instruments_checked: usize,
    instruments_with_gaps: usize,
    ohlc_violations: usize,
    data_violations: usize,
    timestamp_violations: usize,
    weekend_violations: usize,
) -> bool {
    instruments_checked > 0
        && instruments_with_gaps == 0
        && ohlc_violations == 0
        && data_violations == 0
        && timestamp_violations == 0
        && weekend_violations == 0
}

/// Detects whether a price mismatch exists between historical and live OHLC values.
///
/// Returns `true` if any of the four price fields differ by more than epsilon.
fn has_price_mismatch(d_open: f64, d_high: f64, d_low: f64, d_close: f64, epsilon: f64) -> bool {
    d_open.abs() > epsilon
        || d_high.abs() > epsilon
        || d_low.abs() > epsilon
        || d_close.abs() > epsilon
}

/// Detects whether a volume mismatch exists.
///
/// Exact matching: any difference in volume values flags as mismatch.
fn has_volume_mismatch(h_volume: i64, m_volume: i64, _tolerance_pct: f64) -> bool {
    h_volume != m_volume
}

/// Detects whether an OI mismatch exists.
///
/// Exact matching: any difference in OI values flags as mismatch.
/// Both zero = match (equity instruments). One zero, one non-zero = mismatch.
fn has_oi_mismatch(h_oi: i64, m_oi: i64, _tolerance_pct: f64) -> bool {
    h_oi != m_oi
}

/// Classifies the type of mismatch based on which fields differ.
///
/// Returns one of: `"oi_diff"`, `"volume_diff"`, or `"price_diff"`.
fn classify_mismatch_type(
    price_mismatch: bool,
    volume_mismatch: bool,
    oi_mismatch: bool,
) -> &'static str {
    if oi_mismatch && !price_mismatch && !volume_mismatch {
        "oi_diff"
    } else if volume_mismatch && !price_mismatch {
        "volume_diff"
    } else {
        "price_diff"
    }
}

/// OHLCV + OI values for a single candle (used in cross-match comparisons).
#[derive(Debug, Clone, Copy)]
struct CandleValues {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: i64,
    oi: i64,
}

impl CandleValues {
    /// Formats as display string with OI.
    fn format_with_oi(self) -> String {
        format!(
            "O={} H={} L={} C={} V={} OI={}",
            self.open, self.high, self.low, self.close, self.volume, self.oi
        )
    }

    /// Formats as display string without OI.
    fn format_ohlcv(self) -> String {
        format!(
            "O={} H={} L={} C={} V={}",
            self.open, self.high, self.low, self.close, self.volume
        )
    }
}

/// Context identifying a candle row (symbol, segment, timeframe, timestamp).
struct CandleContext {
    symbol: String,
    segment: String,
    timeframe: String,
    timestamp_ist: String,
}

/// Parameters for building a diff summary.
struct DiffSummaryParams {
    d_open: f64,
    d_high: f64,
    d_low: f64,
    d_close: f64,
    h_volume: i64,
    m_volume: i64,
    h_oi: i64,
    m_oi: i64,
    epsilon: f64,
    volume_mismatch: bool,
    oi_mismatch: bool,
}

/// Builds a diff summary string for fields that exceed tolerance.
///
/// Example output: `"O(+1.50) H(-0.75) V(+5000 15%)"`.
fn build_diff_summary(params: &DiffSummaryParams) -> String {
    let mut diffs = Vec::new();
    if params.d_open.abs() > params.epsilon {
        diffs.push(format!("O({:+.2})", params.d_open));
    }
    if params.d_high.abs() > params.epsilon {
        diffs.push(format!("H({:+.2})", params.d_high));
    }
    if params.d_low.abs() > params.epsilon {
        diffs.push(format!("L({:+.2})", params.d_low));
    }
    if params.d_close.abs() > params.epsilon {
        diffs.push(format!("C({:+.2})", params.d_close));
    }
    let d_vol = params.m_volume.saturating_sub(params.h_volume);
    if params.volume_mismatch {
        let h_vol_max = (params.h_volume.max(1)) as f64;
        let vol_diff_pct =
            (params.m_volume.saturating_sub(params.h_volume) as f64).abs() / h_vol_max;
        diffs.push(format!("V({d_vol:+} {vol_diff_pct:.0}%)"));
    }
    if params.oi_mismatch {
        let d_oi = params.m_oi.saturating_sub(params.h_oi);
        diffs.push(format!("OI({d_oi:+})"));
    }
    diffs.join(" ")
}

/// Builds a `ViolationDetail` from parsed candle row values.
fn build_violation_detail(
    ctx: CandleContext,
    violation_type: &str,
    candle: CandleValues,
) -> ViolationDetail {
    ViolationDetail {
        symbol: ctx.symbol,
        segment: ctx.segment,
        timeframe: ctx.timeframe,
        timestamp_ist: ctx.timestamp_ist,
        violation: violation_type.to_string(),
        values: candle.format_ohlcv(),
    }
}

/// Classifies a single cross-match row and returns a mismatch if any tolerance exceeded.
///
/// Returns `None` if the row passes all tolerance checks (no mismatch).
fn classify_cross_match_row(
    ctx: CandleContext,
    hist: CandleValues,
    live: CandleValues,
) -> Option<CrossMatchMismatch> {
    let d_open = live.open - hist.open;
    let d_high = live.high - hist.high;
    let d_low = live.low - hist.low;
    let d_close = live.close - hist.close;

    let price_mismatch =
        has_price_mismatch(d_open, d_high, d_low, d_close, CROSS_MATCH_PRICE_EPSILON);
    let volume_mismatch =
        has_volume_mismatch(hist.volume, live.volume, CROSS_MATCH_VOLUME_TOLERANCE_PCT);
    let oi_mismatch = has_oi_mismatch(hist.oi, live.oi, CROSS_MATCH_OI_TOLERANCE_PCT);

    if !price_mismatch && !volume_mismatch && !oi_mismatch {
        return None;
    }

    let mismatch_type = classify_mismatch_type(price_mismatch, volume_mismatch, oi_mismatch);

    let diff_summary = build_diff_summary(&DiffSummaryParams {
        d_open,
        d_high,
        d_low,
        d_close,
        h_volume: hist.volume,
        m_volume: live.volume,
        h_oi: hist.oi,
        m_oi: live.oi,
        epsilon: CROSS_MATCH_PRICE_EPSILON,
        volume_mismatch,
        oi_mismatch,
    });

    Some(CrossMatchMismatch {
        symbol: ctx.symbol,
        segment: ctx.segment,
        timeframe: ctx.timeframe,
        timestamp_ist: ctx.timestamp_ist,
        mismatch_type: mismatch_type.to_string(),
        hist_values: hist.format_with_oi(),
        live_values: live.format_with_oi(),
        diff_summary,
    })
}

/// Builds a `CrossMatchMismatch` for when live data is missing.
fn build_missing_live_mismatch(ctx: CandleContext, hist: CandleValues) -> CrossMatchMismatch {
    CrossMatchMismatch {
        symbol: ctx.symbol,
        segment: ctx.segment,
        timeframe: ctx.timeframe,
        timestamp_ist: ctx.timestamp_ist,
        mismatch_type: "missing_live".to_string(),
        hist_values: hist.format_with_oi(),
        live_values: "[MISSING — no live data for this candle]".to_string(),
        diff_summary: String::new(),
    }
}

/// Determines if the cross-match report should pass.
fn determine_cross_match_passed(
    total_mismatches: usize,
    total_compared: usize,
    missing_views_count: usize,
) -> bool {
    total_mismatches == 0 && total_compared > 0 && missing_views_count == 0
}

/// Parsed fields from a single QuestDB violation row.
/// Expected row columns: security_id, segment, timeframe, ts, open, high, low, close, volume.
#[derive(Debug, Clone)]
struct ParsedViolationRow {
    security_id: i64,
    segment: String,
    timeframe: String,
    timestamp_value: serde_json::Value,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: i64,
}

/// Parses a single QuestDB violation row from JSON values.
///
/// Returns `None` if the row has fewer than 9 columns.
fn parse_single_violation_row(row: &[serde_json::Value]) -> Option<ParsedViolationRow> {
    if row.len() < 9 {
        return None;
    }
    Some(ParsedViolationRow {
        security_id: row[0].as_i64().unwrap_or(0),
        segment: row[1].as_str().unwrap_or("").to_string(),
        timeframe: row[2].as_str().unwrap_or("").to_string(),
        timestamp_value: row[3].clone(),
        open: row[4].as_f64().unwrap_or(0.0),
        high: row[5].as_f64().unwrap_or(0.0),
        low: row[6].as_f64().unwrap_or(0.0),
        close: row[7].as_f64().unwrap_or(0.0),
        volume: row[8].as_i64().unwrap_or(0),
    })
}

/// Converts a `ParsedViolationRow` into a `ViolationDetail` using the registry for
/// symbol lookup and `format_timestamp_ist` for timestamp formatting.
fn violation_row_to_detail(
    parsed: &ParsedViolationRow,
    registry: &InstrumentRegistry,
    violation_type: &str,
) -> ViolationDetail {
    build_violation_detail(
        CandleContext {
            symbol: lookup_symbol(registry, parsed.security_id),
            segment: parsed.segment.clone(),
            timeframe: parsed.timeframe.clone(),
            timestamp_ist: format_timestamp_ist(&parsed.timestamp_value),
        },
        violation_type,
        CandleValues {
            open: parsed.open,
            high: parsed.high,
            low: parsed.low,
            close: parsed.close,
            volume: parsed.volume,
            oi: 0,
        },
    )
}

/// Parsed fields from a single QuestDB cross-match row.
/// Expected columns: security_id, segment, ts,
///   h_open, h_high, h_low, h_close, h_volume,
///   m_open, m_high, m_low, m_close, m_volume,
///   h_open_interest, m_open_interest
#[derive(Debug, Clone)]
struct ParsedCrossMatchRow {
    security_id: i64,
    segment: String,
    timestamp_value: serde_json::Value,
    hist: CandleValues,
    live_is_null: bool,
    live: CandleValues,
}

/// Parses a single QuestDB cross-match row from JSON values.
///
/// Returns `None` if the row has fewer than 15 columns.
fn parse_single_cross_match_row(row: &[serde_json::Value]) -> Option<ParsedCrossMatchRow> {
    if row.len() < 15 {
        return None;
    }

    let live_is_null = row[8].is_null();

    let live = if live_is_null {
        CandleValues {
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0,
            oi: 0,
        }
    } else {
        CandleValues {
            open: row[8].as_f64().unwrap_or(0.0),
            high: row[9].as_f64().unwrap_or(0.0),
            low: row[10].as_f64().unwrap_or(0.0),
            close: row[11].as_f64().unwrap_or(0.0),
            volume: row[12].as_i64().unwrap_or(0),
            oi: row[14].as_i64().unwrap_or(0),
        }
    };

    Some(ParsedCrossMatchRow {
        security_id: row[0].as_i64().unwrap_or(0),
        segment: row[1].as_str().unwrap_or("").to_string(),
        timestamp_value: row[2].clone(),
        hist: CandleValues {
            open: row[3].as_f64().unwrap_or(0.0),
            high: row[4].as_f64().unwrap_or(0.0),
            low: row[5].as_f64().unwrap_or(0.0),
            close: row[6].as_f64().unwrap_or(0.0),
            volume: row[7].as_i64().unwrap_or(0),
            oi: row[13].as_i64().unwrap_or(0),
        },
        live_is_null,
        live,
    })
}

/// Classifies a parsed cross-match row into a mismatch detail, if any.
///
/// Returns `Some(CrossMatchMismatch)` if there is a mismatch (price, volume, OI, or missing live),
/// or `None` if the row passes all tolerance checks.
fn classify_parsed_cross_match_row(
    parsed: &ParsedCrossMatchRow,
    registry: &InstrumentRegistry,
    timeframe: &str,
) -> Option<CrossMatchMismatch> {
    let ctx = CandleContext {
        symbol: lookup_symbol(registry, parsed.security_id),
        segment: parsed.segment.clone(),
        timeframe: timeframe.to_string(),
        timestamp_ist: format_timestamp_ist(&parsed.timestamp_value),
    };

    if parsed.live_is_null {
        return Some(build_missing_live_mismatch(ctx, parsed.hist));
    }

    classify_cross_match_row(ctx, parsed.hist, parsed.live)
}

// ---------------------------------------------------------------------------
// Cross-Verification Logic
// ---------------------------------------------------------------------------

/// Creates a failed report with all zeros.
fn failed_report() -> CrossVerificationReport {
    CrossVerificationReport {
        instruments_checked: 0,
        instruments_complete: 0,
        instruments_with_gaps: 0,
        total_candles_in_db: 0,
        timeframe_counts: Vec::new(),
        ohlc_violations: 0,
        ohlc_details: Vec::new(),
        data_violations: 0,
        data_details: Vec::new(),
        timestamp_violations: 0,
        timestamp_details: Vec::new(),
        weekend_violations: 0,
        weekend_details: Vec::new(),
        passed: false,
    }
}

/// Runs cross-verification of candle data integrity in QuestDB.
///
/// Checks all 5 timeframes for coverage and validates OHLC consistency,
/// data integrity (non-positive prices), and timestamp bounds (market hours).
///
/// # Arguments
/// * `questdb_config` — QuestDB connection config
/// * `registry` — Instrument registry for security_id → symbol lookup
///
/// # Returns
/// A `CrossVerificationReport` with pass/fail status, per-timeframe breakdown,
/// violation counts, and detailed violation records.
pub async fn verify_candle_integrity(
    questdb_config: &QuestDbConfig,
    registry: &InstrumentRegistry,
    today_window: &TodayIstWindow,
) -> CrossVerificationReport {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(VERIFY_QUERY_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for cross-verification");
            return failed_report();
        }
    };

    // Today's IST trading-session window — replaces the old rolling 3-day
    // `dateadd('d', -3, now())` bound, which (because QuestDB persists across
    // app restarts via Docker volume) captured up to 12 prior sessions'
    // accumulated ticks. See `TodayIstWindow` docstring for the full rationale.
    let ts_filter = today_window.where_clause();

    // --- Step 1: Per-timeframe coverage query ---
    // Groups by (timeframe, security_id) to get counts per instrument per timeframe.
    // Narrowed to TODAY ONLY (09:15-15:30 IST). Daily candles (stamped at IST
    // midnight = hour 0 of the NEXT calendar day in the old semantics) are
    // intentionally NOT captured by this intraday window — the daily cross-check
    // happens on a separate path post-close.
    let coverage_query = format!(
        "SELECT timeframe, security_id, count() as candle_count \
         FROM {} \
         WHERE {ts_filter} \
         GROUP BY timeframe, security_id \
         ORDER BY timeframe, candle_count DESC",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let coverage_data = match execute_query(&client, &base_url, &coverage_query).await {
        Some(data) => data,
        None => return failed_report(),
    };

    // Parse coverage results into per-timeframe maps
    let (timeframe_instruments, total_candles) = parse_coverage_rows(&coverage_data.dataset);

    // Build per-timeframe coverage summary
    let timeframe_counts = build_timeframe_coverage(&timeframe_instruments);

    // --- Step 2: 1m coverage check (primary completeness metric) ---
    let one_m_instruments = timeframe_instruments.get(TIMEFRAME_1M);

    #[allow(clippy::arithmetic_side_effects)]
    // APPROVED: f64 multiplication for coverage ratio check — no overflow risk
    let min_candles = (CANDLES_PER_TRADING_DAY as f64 * MIN_CANDLE_COVERAGE_RATIO) as usize;

    let (instruments_complete, instruments_with_gaps) = if let Some(instruments) = one_m_instruments
    {
        let (complete, gaps) = classify_1m_coverage(instruments, min_candles);
        // Log first 10 gaps for debugging
        let mut gap_log_count = 0_usize;
        for &(sid, count) in instruments {
            if count < min_candles && gap_log_count < 10 {
                debug!(
                    security_id = sid,
                    candle_count = count,
                    expected = CANDLES_PER_TRADING_DAY,
                    timeframe = TIMEFRAME_1M,
                    "instrument has incomplete 1m candle coverage"
                );
                gap_log_count = gap_log_count.saturating_add(1);
            }
        }
        (complete, gaps)
    } else {
        (0_usize, 0_usize)
    };

    // Count unique instruments across all timeframes
    let instruments_checked = count_unique_instruments(&timeframe_instruments);

    // --- Step 3: OHLC consistency check (high < low) with details ---
    let ohlc_count_query = format!(
        "SELECT count() FROM {} \
         WHERE {ts_filter} AND high < low",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let ohlc_violations = extract_count(&client, &base_url, &ohlc_count_query).await;

    let ohlc_details = if ohlc_violations > 0 {
        let ohlc_detail_query = format!(
            "SELECT security_id, segment, timeframe, ts, open, high, low, close, volume \
             FROM {} \
             WHERE {ts_filter} AND high < low \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, MAX_VIOLATION_DETAILS
        );
        parse_violation_rows(
            &client,
            &base_url,
            &ohlc_detail_query,
            registry,
            "high < low",
        )
        .await
    } else {
        Vec::new()
    };

    if ohlc_violations > 0 {
        warn!(
            ohlc_violations,
            "OHLC consistency check found candles with high < low"
        );
    }

    // --- Step 4: Data integrity check (non-positive prices) with details ---
    let data_count_query = format!(
        "SELECT count() FROM {} \
         WHERE {ts_filter} \
         AND (open <= 0 OR high <= 0 OR low <= 0 OR close <= 0)",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let data_violations = extract_count(&client, &base_url, &data_count_query).await;

    let data_details = if data_violations > 0 {
        let data_detail_query = format!(
            "SELECT security_id, segment, timeframe, ts, open, high, low, close, volume \
             FROM {} \
             WHERE {ts_filter} \
             AND (open <= 0 OR high <= 0 OR low <= 0 OR close <= 0) \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, MAX_VIOLATION_DETAILS
        );
        parse_violation_rows(
            &client,
            &base_url,
            &data_detail_query,
            registry,
            "price <= 0",
        )
        .await
    } else {
        Vec::new()
    };

    if data_violations > 0 {
        warn!(
            data_violations,
            "data integrity check found candles with non-positive prices"
        );
    }

    // --- Step 5: Timestamp bounds check (intraday outside market hours) ---
    // QuestDB stores IST-as-UTC: hours in DB ARE IST hours directly.
    // Valid range: [09:15, 15:30) IST — 15:30 is ALWAYS exclusive for ALL timeframes.
    // Last valid candle: 15:29 for 1m, 15:25 for 5m, 15:15 for 15m, 15:00 for 60m.
    // Daily candles: no time check (stamped at IST midnight).
    // Any candle at 15:30+ is a violation regardless of timeframe.
    let ts_count_query = format!(
        "SELECT count() FROM {} \
         WHERE {ts_filter} \
         AND timeframe != '1d' \
         AND (hour(ts) < 9 OR hour(ts) > 15 \
              OR (hour(ts) = 9 AND minute(ts) < 15) \
              OR (hour(ts) = 15 AND minute(ts) > 29))",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let timestamp_violations = extract_count(&client, &base_url, &ts_count_query).await;

    let timestamp_details = if timestamp_violations > 0 {
        let ts_detail_query = format!(
            "SELECT security_id, segment, timeframe, ts, open, high, low, close, volume \
             FROM {} \
             WHERE {ts_filter} \
             AND timeframe != '1d' \
             AND (hour(ts) < 9 OR hour(ts) > 15 \
                  OR (hour(ts) = 9 AND minute(ts) < 15) \
                  OR (hour(ts) = 15 AND minute(ts) > 29)) \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, MAX_VIOLATION_DETAILS
        );
        parse_violation_rows(
            &client,
            &base_url,
            &ts_detail_query,
            registry,
            "outside 09:15-15:29 market hours",
        )
        .await
    } else {
        Vec::new()
    };

    if timestamp_violations > 0 {
        warn!(
            timestamp_violations,
            "timestamp bounds check found intraday candles outside market hours"
        );
    }

    // --- Step 5b: Weekend candle check ---
    // Candles on Saturday (day_of_week=6) or Sunday (day_of_week=7) should NEVER exist.
    // NSE is closed on weekends — no trading, no settlement, no data.
    let weekend_count_query = format!(
        "SELECT count() FROM {} \
         WHERE {ts_filter} \
         AND (day_of_week(ts) = 6 OR day_of_week(ts) = 7)",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let weekend_violations = extract_count(&client, &base_url, &weekend_count_query).await;

    let weekend_details = if weekend_violations > 0 {
        let weekend_detail_query = format!(
            "SELECT security_id, segment, timeframe, ts, open, high, low, close, volume \
             FROM {} \
             WHERE {ts_filter} \
             AND (day_of_week(ts) = 6 OR day_of_week(ts) = 7) \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, MAX_VIOLATION_DETAILS
        );
        parse_violation_rows(
            &client,
            &base_url,
            &weekend_detail_query,
            registry,
            "weekend candle (Saturday/Sunday — NSE closed)",
        )
        .await
    } else {
        Vec::new()
    };

    if weekend_violations > 0 {
        error!(
            weekend_violations,
            "CRITICAL: candles found on weekends — NSE is closed on Sat/Sun"
        );
    }

    // --- Step 6: Determine pass/fail ---
    // FAIL if: zero instruments OR any gaps OR any violations of any type
    let passed = determine_verification_passed(
        instruments_checked,
        instruments_with_gaps,
        ohlc_violations,
        data_violations,
        timestamp_violations,
        weekend_violations,
    );

    // Log per-timeframe summary
    for tc in &timeframe_counts {
        info!(
            timeframe = %tc.timeframe,
            instruments = tc.instrument_count,
            candles = tc.candle_count,
            "timeframe coverage"
        );
    }

    info!(
        instruments_checked,
        instruments_complete,
        instruments_with_gaps,
        total_candles,
        ohlc_violations,
        data_violations,
        timestamp_violations,
        weekend_violations,
        passed,
        "multi-timeframe cross-verification complete"
    );

    CrossVerificationReport {
        instruments_checked,
        instruments_complete,
        instruments_with_gaps,
        total_candles_in_db: total_candles,
        timeframe_counts,
        ohlc_violations,
        ohlc_details,
        data_violations,
        data_details,
        timestamp_violations,
        timestamp_details,
        weekend_violations,
        weekend_details,
        passed,
    }
}

// ---------------------------------------------------------------------------
// Historical vs Live Cross-Match
// ---------------------------------------------------------------------------

/// Compares historical candle data (Dhan REST API) against live materialized
/// views (WebSocket ticks) for the same instruments and timestamps.
///
/// Detects:
/// - **Missing live candles**: historical exists but live doesn't (WebSocket dropped ticks)
/// - **Price mismatches**: OHLCV values differ beyond tolerance
/// - **Missing historical**: live exists but historical doesn't (rare)
///
/// Runs after post-market re-fetch when both datasets are complete for the day.
///
/// # Arguments
/// * `questdb_config` — QuestDB connection config
/// * `registry` — Instrument registry for security_id → symbol lookup
pub async fn cross_match_historical_vs_live(
    questdb_config: &QuestDbConfig,
    registry: &InstrumentRegistry,
    today_window: &TodayIstWindow,
) -> CrossMatchReport {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(VERIFY_QUERY_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for cross-match");
            return failed_cross_match_report();
        }
    };

    // Today's IST trading-session window (see `TodayIstWindow`). `ts_filter`
    // for unaliased queries (bare `candles_*` live tables); `ts_filter_h` for
    // LEFT-JOIN queries that alias `historical_candles` as `h`.
    let ts_filter = today_window.where_clause();
    let ts_filter_h = today_window.where_clause_aliased("h");

    // First-fetch gate (Parthiban directive, 2026-04-20): if today's live
    // `candles_1m` view has ZERO rows, there is nothing meaningful to
    // compare against — it's a first fetch, a fresh DB, or a post-market
    // boot before the next session. Skip the entire cross-match with a
    // clear `live_candles_present = false` signal so the caller emits the
    // typed SKIPPED Telegram notification instead of running the full
    // LEFT-JOIN pipeline against an empty live side (which would "succeed"
    // vacuously with zero rows compared).
    //
    // The cheaper `candles_1m` probe is authoritative — if 1m is empty, no
    // higher timeframe can possibly have data.
    let live_probe_query = format!("SELECT count() FROM candles_1m WHERE {ts_filter}");
    let live_probe_rows = extract_count(&client, &base_url, &live_probe_query).await;
    if live_probe_rows == 0 {
        info!(
            today_ist = %today_window.today_ist,
            window_start = %today_window.start_sql,
            window_end = %today_window.end_sql,
            "cross-match SKIPPED — zero live 1m candles for today's window \
             (first fetch / fresh DB / pre-live-data boot)"
        );
        return CrossMatchReport {
            timeframes_checked: 0,
            candles_compared: 0,
            mismatches: 0,
            missing_live: 0,
            missing_historical: 0,
            oi_mismatches: 0,
            mismatch_details: Vec::new(),
            missing_views: Vec::new(),
            per_timeframe_mismatches: Vec::new(),
            passed: false,
            // `false` → caller's main.rs:4285 branch fires the typed
            // `CandleCrossMatchSkipped` Telegram notification.
            live_candles_present: false,
        };
    }

    let mut total_compared = 0_usize;
    let mut total_mismatches = 0_usize;
    let mut total_missing_live = 0_usize;
    let total_missing_historical = 0_usize;
    let mut total_oi_mismatches = 0_usize;
    let mut all_details: Vec<CrossMatchMismatch> = Vec::new();
    let mut timeframes_checked = 0_usize;
    let mut missing_views: Vec<String> = Vec::new();
    let mut per_timeframe_mismatches: Vec<(String, usize)> = Vec::new();
    // Explicit live-data presence flag. The per-timeframe count queries
    // use LEFT JOIN which preserves historical rows even when the live
    // MV is empty, so `candles_compared` alone cannot distinguish
    // "N candles joined against live data" from "N historical rows and
    // zero matching live rows". Track whether any live MV has rows in
    // the last 3 days separately.
    let mut live_candles_present = false;

    for &(hist_tf, live_table) in CROSS_MATCH_TIMEFRAMES {
        // M2: Check if the materialized view exists before JOINing.
        // SAFETY: live_table is from CROSS_MATCH_TIMEFRAMES constants, not user input.
        // Use SHOW COLUMNS instead of tables() because QuestDB's tables() does NOT
        // include materialized views — only base tables. SHOW COLUMNS works for both.
        let view_probe_query = format!("SHOW COLUMNS FROM {}", live_table);
        let view_exists = match client
            .get(&base_url)
            .query(&[("query", &view_probe_query)])
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        };
        if !view_exists {
            warn!(
                live_table,
                timeframe = hist_tf,
                "materialized view table does not exist — skipping cross-match for this timeframe"
            );
            missing_views.push(live_table.to_string());
            continue;
        }

        // Direct live-MV row count (NOT a LEFT JOIN). Determines whether
        // there is any live data at all to compare against. If zero,
        // we still run the per-timeframe LEFT JOIN below for completeness
        // but the caller will refuse to emit "OK" without at least one
        // timeframe reporting live rows.
        let live_count_query = format!("SELECT count() FROM {} WHERE {ts_filter}", live_table);
        let live_tf_rows = extract_count(&client, &base_url, &live_count_query).await;
        if live_tf_rows > 0 {
            live_candles_present = true;
        }

        // Count total comparable candles for this timeframe
        let count_query = format!(
            "SELECT count() FROM {} h \
             LEFT JOIN {} m ON h.security_id = m.security_id AND h.ts = m.ts AND h.segment = m.segment \
             WHERE h.timeframe = '{}' AND {ts_filter_h}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, live_table, hist_tf
        );

        let tf_total = extract_count(&client, &base_url, &count_query).await;
        total_compared = total_compared.saturating_add(tf_total);

        if tf_total == 0 {
            continue;
        }
        timeframes_checked = timeframes_checked.saturating_add(1);

        // Fetch ALL joined rows for this timeframe — Rust applies epsilon + volume + OI checks.
        // SQL pre-filters with generous tolerance to avoid fetching perfectly matching rows.
        let detail_query = format!(
            "SELECT h.security_id, h.segment, h.ts, \
                    h.open, h.high, h.low, h.close, h.volume, \
                    m.open, m.high, m.low, m.close, m.volume, \
                    h.open_interest, m.open_interest \
             FROM {} h \
             LEFT JOIN {} m ON h.security_id = m.security_id AND h.ts = m.ts AND h.segment = m.segment \
             WHERE h.timeframe = '{}' AND {ts_filter_h} \
             AND (m.open IS NULL \
                  OR abs(h.open - m.open) > {eps} \
                  OR abs(h.high - m.high) > {eps} \
                  OR abs(h.low - m.low) > {eps} \
                  OR abs(h.close - m.close) > {eps} \
                  OR abs(h.volume - m.volume) > 0 \
                  OR abs(h.open_interest - m.open_interest) > 0) \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES,
            live_table,
            hist_tf,
            MAX_VIOLATION_DETAILS,
            eps = CROSS_MATCH_PRICE_EPSILON,
        );

        let details =
            parse_cross_match_rows_with_oi(&client, &base_url, &detail_query, registry, hist_tf)
                .await;

        for detail in &details {
            total_mismatches = total_mismatches.saturating_add(1);
            if detail.mismatch_type == "missing_live" {
                total_missing_live = total_missing_live.saturating_add(1);
            }
            if detail.mismatch_type == "oi_diff" {
                total_oi_mismatches = total_oi_mismatches.saturating_add(1);
            }
        }

        // Track per-timeframe mismatch count for Telegram display.
        let tf_mismatch_count = details.len();
        per_timeframe_mismatches.push((hist_tf.to_string(), tf_mismatch_count));

        all_details.extend(details);
    }

    let passed =
        determine_cross_match_passed(total_mismatches, total_compared, missing_views.len());

    info!(
        timeframes_checked,
        candles_compared = total_compared,
        mismatches = total_mismatches,
        missing_live = total_missing_live,
        oi_mismatches = total_oi_mismatches,
        missing_views_count = missing_views.len(),
        passed,
        "historical vs live cross-match complete"
    );

    CrossMatchReport {
        timeframes_checked,
        candles_compared: total_compared,
        mismatches: total_mismatches,
        missing_live: total_missing_live,
        missing_historical: total_missing_historical,
        oi_mismatches: total_oi_mismatches,
        mismatch_details: all_details,
        missing_views,
        per_timeframe_mismatches,
        passed,
        live_candles_present,
    }
}

/// Creates a failed cross-match report.
fn failed_cross_match_report() -> CrossMatchReport {
    CrossMatchReport {
        timeframes_checked: 0,
        candles_compared: 0,
        mismatches: 0,
        missing_live: 0,
        missing_historical: 0,
        oi_mismatches: 0,
        mismatch_details: Vec::new(),
        missing_views: Vec::new(),
        per_timeframe_mismatches: Vec::new(),
        passed: false,
        live_candles_present: false,
    }
}

// ---------------------------------------------------------------------------
// Query Helpers
// ---------------------------------------------------------------------------

/// Executes a QuestDB HTTP query and returns the parsed response.
async fn execute_query(
    client: &Client,
    base_url: &str,
    query: &str,
) -> Option<QuestDbQueryResponse> {
    let result = client.get(base_url).query(&[("query", query)]).send().await;

    match result {
        Ok(response) => {
            if !response.status().is_success() {
                let status = response.status();
                warn!(%status, "QuestDB query returned non-success");
                return None;
            }
            match response.json::<QuestDbQueryResponse>().await {
                Ok(data) => Some(data),
                Err(err) => {
                    warn!(?err, "failed to parse QuestDB query response");
                    None
                }
            }
        }
        Err(err) => {
            tracing::error!(
                ?err,
                "QuestDB query request failed — verification cannot proceed"
            );
            None
        }
    }
}

/// Executes a `SELECT count()` query and returns the count as usize.
async fn extract_count(client: &Client, base_url: &str, query: &str) -> usize {
    match execute_query(client, base_url, query).await {
        Some(data) => {
            if let Some(row) = data.dataset.first() {
                row.first().and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize
            } else {
                0
            }
        }
        None => 0,
    }
}

/// Looks up the display label for a security_id from the instrument registry.
/// Falls back to the raw security_id string if not found.
fn lookup_symbol(registry: &InstrumentRegistry, security_id: i64) -> String {
    #[allow(clippy::cast_sign_loss)]
    // APPROVED: security_id from QuestDB is always non-negative
    let sid_u32 = security_id as u32;
    registry
        .get(sid_u32)
        .map(|inst| inst.display_label.clone())
        .unwrap_or_else(|| security_id.to_string())
}

/// Formats a QuestDB timestamp value as an IST time string.
/// QuestDB returns timestamps as ISO strings like "2026-03-18T03:45:00.000000Z"
/// (which is already IST-as-UTC in our storage convention).
fn format_timestamp_ist(ts_value: &serde_json::Value) -> String {
    // QuestDB HTTP API returns timestamps as ISO strings
    if let Some(ts_str) = ts_value.as_str() {
        // Extract date and time from ISO format: "2026-03-18T03:45:00.000000Z"
        // Since our storage is IST-as-UTC, display as-is but reformat
        if let Some((date_part, time_rest)) = ts_str.split_once('T') {
            let time_part = time_rest
                .split_once('.')
                .map_or(time_rest, |(t, _)| t)
                .trim_end_matches('Z');
            // Show HH:MM only (skip seconds for readability)
            let time_hm = if time_part.len() >= 5 {
                &time_part[..5]
            } else {
                time_part
            };
            return format!("{} {} IST", date_part, time_hm);
        }
        return ts_str.to_string();
    }
    // Fallback for numeric epoch
    ts_value.to_string()
}

/// Parses violation detail rows from a QuestDB query response.
/// Expected columns: security_id, segment, timeframe, ts, open, high, low, close, volume
async fn parse_violation_rows(
    client: &Client,
    base_url: &str,
    query: &str,
    registry: &InstrumentRegistry,
    violation_type: &str,
) -> Vec<ViolationDetail> {
    let data = match execute_query(client, base_url, query).await {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut details = Vec::with_capacity(data.dataset.len().min(MAX_VIOLATION_DETAILS));

    for row in &data.dataset {
        if let Some(parsed) = parse_single_violation_row(row) {
            details.push(violation_row_to_detail(&parsed, registry, violation_type));
        }

        if details.len() >= MAX_VIOLATION_DETAILS {
            break;
        }
    }

    details
}

/// Parses cross-match rows with OI, applying Rust-side epsilon + volume + OI checks.
/// Expected columns: security_id, segment, ts,
///   h_open, h_high, h_low, h_close, h_volume,
///   m_open, m_high, m_low, m_close, m_volume,
///   h_open_interest, m_open_interest
async fn parse_cross_match_rows_with_oi(
    client: &Client,
    base_url: &str,
    query: &str,
    registry: &InstrumentRegistry,
    timeframe: &str,
) -> Vec<CrossMatchMismatch> {
    let data = match execute_query(client, base_url, query).await {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut details = Vec::with_capacity(data.dataset.len().min(MAX_VIOLATION_DETAILS));

    for row in &data.dataset {
        let Some(parsed) = parse_single_cross_match_row(row) else {
            continue;
        };

        if let Some(mismatch) = classify_parsed_cross_match_row(&parsed, registry, timeframe) {
            details.push(mismatch);
        } else {
            // SQL pre-filter caught it but Rust-side says it's fine (e.g., volume diff < 10%)
            continue;
        }

        if details.len() >= MAX_VIOLATION_DETAILS {
            break;
        }
    }

    details
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // TodayIstWindow — today-only SQL window (fixes "12 days accumulated" bug)
    // -----------------------------------------------------------------------

    /// Build a UTC instant equivalent to an IST wall-clock time on the given date.
    fn utc_at_ist(year: i32, month: u32, day: u32, ist_h: u32, ist_m: u32) -> DateTime<Utc> {
        // IST is UTC+5:30 → subtract 5:30 from IST wall-clock to get UTC.
        let naive_ist = NaiveDate::from_ymd_opt(year, month, day)
            .expect("valid date")
            .and_hms_opt(ist_h, ist_m, 0)
            .expect("valid time");
        let naive_utc = naive_ist - chrono::Duration::seconds(i64::from(IST_UTC_OFFSET_SECONDS));
        DateTime::<Utc>::from_naive_utc_and_offset(naive_utc, Utc)
    }

    #[test]
    fn test_from_utc_pre_market_returns_none() {
        // 09:14 IST — 1 minute before market open — should return None.
        let now = utc_at_ist(2026, 4, 20, 9, 14);
        assert!(TodayIstWindow::from_utc(now).is_none());
    }

    #[test]
    fn test_from_utc_at_market_open_returns_some_at_boundary() {
        // Exactly 09:15 IST — boundary, should be Some with start == end.
        let now = utc_at_ist(2026, 4, 20, 9, 15);
        let w = TodayIstWindow::from_utc(now).expect("at boundary is inclusive");
        assert_eq!(w.today_ist, NaiveDate::from_ymd_opt(2026, 4, 20).unwrap());
        assert!(w.start_sql.contains("T09:15:00"));
        // now == start → effective_end == start
        assert_eq!(w.start_sql, w.end_sql);
    }

    #[test]
    fn test_today_ist_window_mid_session_clamps_end_to_now() {
        // 12:00 IST — mid-session — window end should be 12:00, not 15:30.
        let now = utc_at_ist(2026, 4, 20, 12, 0);
        let w = TodayIstWindow::from_utc(now).expect("mid-session window exists");
        assert!(w.start_sql.contains("T09:15:00"));
        assert!(w.end_sql.contains("T12:00:00"));
    }

    #[test]
    fn test_today_ist_window_post_market_clamps_end_to_1530() {
        // 18:00 IST — post-market — end should be 15:30, not 18:00.
        let now = utc_at_ist(2026, 4, 20, 18, 0);
        let w = TodayIstWindow::from_utc(now).expect("post-market window exists");
        assert!(w.start_sql.contains("T09:15:00"));
        assert!(
            w.end_sql.contains("T15:30:00"),
            "post-market end must clamp to 15:30: {}",
            w.end_sql
        );
    }

    #[test]
    fn test_today_ist_window_at_market_close_returns_full_session() {
        // Exactly 15:30 IST — close boundary — should clamp to 15:30.
        let now = utc_at_ist(2026, 4, 20, 15, 30);
        let w = TodayIstWindow::from_utc(now).expect("close boundary window exists");
        assert!(w.end_sql.contains("T15:30:00"));
    }

    #[test]
    fn test_today_ist_window_where_clause_format() {
        let now = utc_at_ist(2026, 4, 20, 12, 0);
        let w = TodayIstWindow::from_utc(now).unwrap();
        let clause = w.where_clause();
        assert!(clause.starts_with("ts >= '"), "got: {clause}");
        assert!(
            clause.contains(" AND ts <= '"),
            "clause must be bounded both sides: {clause}"
        );
        assert!(clause.contains("2026-04-20T09:15:00"));
        assert!(clause.contains("2026-04-20T12:00:00"));
    }

    #[test]
    fn test_today_ist_window_where_clause_aliased_format() {
        let now = utc_at_ist(2026, 4, 20, 12, 0);
        let w = TodayIstWindow::from_utc(now).unwrap();
        let clause = w.where_clause_aliased("h");
        assert!(clause.starts_with("h.ts >= '"), "got: {clause}");
        assert!(clause.contains(" AND h.ts <= '"));
        // Every `ts` reference must carry the `h.` alias — there should be
        // exactly two (one for `>=`, one for `<=`) and no bare ones.
        assert_eq!(
            clause.matches("h.ts").count(),
            2,
            "expected exactly 2 h.ts references: {clause}"
        );
    }

    #[test]
    fn test_from_now_returns_some_during_market_or_none_otherwise() {
        // Smoke-covers `from_now`: the wrapper over `from_utc(Utc::now())`.
        // We can't assert a specific value since "now" depends on CI clock,
        // but we can assert it doesn't panic and returns the Option shape.
        let _ = TodayIstWindow::from_now();
    }

    #[test]
    fn test_from_utc_is_stable_across_day_boundary() {
        // 2026-04-20 23:59 UTC == 2026-04-21 05:29 IST — still today-ist = 21,
        // but 05:29 is pre-market → None.
        let now = DateTime::<Utc>::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(2026, 4, 20)
                .unwrap()
                .and_hms_opt(23, 59, 0)
                .unwrap(),
            Utc,
        );
        assert!(
            TodayIstWindow::from_utc(now).is_none(),
            "pre-market on the NEXT IST day must return None"
        );
    }

    #[test]
    fn test_today_ist_window_rolls_over_at_ist_midnight() {
        // 18:45 UTC on 2026-04-20 == 00:15 IST on 2026-04-21.
        // That's pre-market on 2026-04-21 IST → None, and `today_ist` would
        // be 21 if a window existed.
        let now = DateTime::<Utc>::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(2026, 4, 20)
                .unwrap()
                .and_hms_opt(18, 45, 0)
                .unwrap(),
            Utc,
        );
        assert!(TodayIstWindow::from_utc(now).is_none());
    }

    #[test]
    fn test_today_ist_window_sql_literal_microsecond_padding() {
        let now = utc_at_ist(2026, 4, 20, 12, 0);
        let w = TodayIstWindow::from_utc(now).unwrap();
        // QuestDB timestamp literal requires the `.000000Z` suffix
        // (microsecond precision) — don't drop it, QuestDB is strict.
        assert!(
            w.start_sql.contains(".000000Z"),
            "start must carry microsecond suffix: {}",
            w.start_sql
        );
        assert!(
            w.end_sql.contains(".000000Z"),
            "end must carry microsecond suffix: {}",
            w.end_sql
        );
    }

    // -----------------------------------------------------------------------
    // Once-per-day success marker (Parthiban directive 2026-04-20:
    //   "only once in a day until it achieves success")
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_verify_success_marker_path_includes_date() {
        let date = NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        let path = cross_verify_success_marker_path(date);
        let s = path.to_string_lossy();
        assert!(
            s.contains("cross-verify-success-2026-04-20"),
            "marker filename must embed the IST date so it's per-day; got: {s}"
        );
        assert!(
            s.ends_with(".marker"),
            "marker file extension expected; got: {s}"
        );
    }

    #[test]
    fn test_cross_verify_success_marker_path_changes_per_day() {
        let d1 = NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 4, 21).unwrap();
        assert_ne!(
            cross_verify_success_marker_path(d1),
            cross_verify_success_marker_path(d2),
            "marker path must be per-day so yesterday's success doesn't shadow today"
        );
    }

    #[test]
    fn test_cross_verify_already_succeeded_today_false_when_marker_absent() {
        // A date so far in the future no marker can ever exist on-disk.
        let date = NaiveDate::from_ymd_opt(2099, 12, 31).unwrap();
        assert!(
            !cross_verify_already_succeeded_today(date),
            "no marker file → should return false"
        );
    }

    #[test]
    fn test_mark_cross_verify_success_then_already_succeeded_returns_true() {
        // Use today's date for a real round-trip — write then check then
        // delete to leave the working tree clean for subsequent runs.
        // Best-effort cleanup; if the test crashes mid-way the marker is
        // for "today" anyway and is harmless next time.
        let date = NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        let marker = cross_verify_success_marker_path(date);
        // Pre-clean in case a stale marker from a previous test exists.
        let _ = std::fs::remove_file(&marker);
        assert!(!cross_verify_already_succeeded_today(date));

        mark_cross_verify_success(date).expect("write marker");
        assert!(
            cross_verify_already_succeeded_today(date),
            "after writing marker, idempotency check must return true"
        );

        // Cleanup — leave repo clean for re-runs.
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn test_cross_verification_report_default_values() {
        let report = CrossVerificationReport {
            instruments_checked: 100,
            instruments_complete: 95,
            instruments_with_gaps: 5,
            total_candles_in_db: 37500,
            timeframe_counts: vec![],
            ohlc_violations: 0,
            ohlc_details: vec![],
            data_violations: 0,
            data_details: vec![],
            timestamp_violations: 0,
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
            passed: false,
        };
        assert!(!report.passed);
        assert_eq!(report.instruments_checked, 100);
    }

    #[test]
    fn test_min_candle_coverage_calculation() {
        #[allow(clippy::arithmetic_side_effects)]
        let min_candles = (CANDLES_PER_TRADING_DAY as f64 * MIN_CANDLE_COVERAGE_RATIO) as usize;
        // 375 * 0.95 = 356.25 → 356
        assert_eq!(min_candles, 356);
    }

    #[test]
    fn test_verified_timeframes_has_all_five() {
        assert_eq!(VERIFIED_TIMEFRAMES.len(), 5);
        assert!(VERIFIED_TIMEFRAMES.contains(&"1m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"5m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"15m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"60m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"1d"));
    }

    #[test]
    fn test_timeframe_coverage_structure() {
        let tc = TimeframeCoverage {
            timeframe: "5m".to_string(),
            instrument_count: 50,
            candle_count: 3750,
        };
        assert_eq!(tc.timeframe, "5m");
        assert_eq!(tc.instrument_count, 50);
        assert_eq!(tc.candle_count, 3750);
    }

    #[test]
    fn test_multi_timeframe_report_fields() {
        let report = CrossVerificationReport {
            instruments_checked: 50,
            instruments_complete: 48,
            instruments_with_gaps: 2,
            total_candles_in_db: 187500,
            timeframe_counts: vec![
                TimeframeCoverage {
                    timeframe: "1m".to_string(),
                    instrument_count: 50,
                    candle_count: 18750,
                },
                TimeframeCoverage {
                    timeframe: "5m".to_string(),
                    instrument_count: 50,
                    candle_count: 3750,
                },
            ],
            ohlc_violations: 0,
            ohlc_details: vec![],
            data_violations: 0,
            data_details: vec![],
            timestamp_violations: 0,
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
            passed: false,
        };

        assert_eq!(report.timeframe_counts.len(), 2);
        assert_eq!(report.timeframe_counts[0].timeframe, "1m");
        assert_eq!(report.timeframe_counts[1].timeframe, "5m");
        assert_eq!(report.ohlc_violations, 0);
        assert!(!report.passed);
    }

    #[test]
    fn test_failed_report_all_zeros() {
        let report = failed_report();
        assert_eq!(report.instruments_checked, 0);
        assert_eq!(report.instruments_complete, 0);
        assert_eq!(report.instruments_with_gaps, 0);
        assert_eq!(report.total_candles_in_db, 0);
        assert!(report.timeframe_counts.is_empty());
        assert_eq!(report.ohlc_violations, 0);
        assert!(report.ohlc_details.is_empty());
        assert_eq!(report.data_violations, 0);
        assert!(report.data_details.is_empty());
        assert_eq!(report.timestamp_violations, 0);
        assert!(report.timestamp_details.is_empty());
        assert!(!report.passed);
    }

    #[test]
    fn test_zero_instruments_fails_verification() {
        // When instruments_checked == 0, passed must be false.
        // This prevents empty DB silently passing verification.
        let instruments_checked = 0;
        let instruments_with_gaps = 0;
        let ohlc_violations = 0;
        let data_violations = 0;
        let timestamp_violations = 0;
        let weekend_violations = 0;

        let passed = instruments_checked > 0
            && instruments_with_gaps == 0
            && ohlc_violations == 0
            && data_violations == 0
            && timestamp_violations == 0
            && weekend_violations == 0;

        assert!(!passed, "zero instruments must FAIL verification");
    }

    #[test]
    fn test_passed_requires_zero_all_violations() {
        // passed = true only when ALL conditions met
        let cases = [
            // (checked, gaps, ohlc, data, ts, expected_pass)
            (100, 0, 0, 0, 0, true),  // all good
            (100, 1, 0, 0, 0, false), // gaps
            (100, 0, 1, 0, 0, false), // ohlc violation
            (100, 0, 0, 1, 0, false), // data violation
            (100, 0, 0, 0, 1, false), // timestamp violation
            (0, 0, 0, 0, 0, false),   // zero instruments
        ];

        for (checked, gaps, ohlc, data, ts, expected) in cases {
            let passed = checked > 0 && gaps == 0 && ohlc == 0 && data == 0 && ts == 0;
            assert_eq!(
                passed, expected,
                "checked={checked} gaps={gaps} ohlc={ohlc} data={data} ts={ts}"
            );
        }
    }

    #[test]
    fn test_violation_detail_struct() {
        let detail = ViolationDetail {
            symbol: "RELIANCE".to_string(),
            segment: "NSE_EQ".to_string(),
            timeframe: "1m".to_string(),
            timestamp_ist: "2026-03-18 10:15 IST".to_string(),
            violation: "high < low".to_string(),
            values: "O=2450.0 H=2440.0 L=2450.0 C=2445.0".to_string(),
        };
        assert_eq!(detail.symbol, "RELIANCE");
        assert_eq!(detail.violation, "high < low");
        assert!(detail.values.contains("H=2440.0"));
    }

    #[test]
    fn test_cross_match_mismatch_struct() {
        let mismatch = CrossMatchMismatch {
            symbol: "TCS".to_string(),
            segment: "NSE_EQ".to_string(),
            timeframe: "1m".to_string(),
            timestamp_ist: "2026-03-18 11:30 IST".to_string(),
            mismatch_type: "price_diff".to_string(),
            hist_values: "O=3520.0 H=3535.0 L=3518.0 C=3530.0 V=45000".to_string(),
            live_values: "O=3520.0 H=3535.0 L=3518.0 C=3528.5 V=42100".to_string(),
            diff_summary: "C(-1.5) V(-2900)".to_string(),
        };
        assert_eq!(mismatch.mismatch_type, "price_diff");
        assert!(mismatch.diff_summary.contains("C(-1.5)"));
    }

    #[test]
    fn test_cross_match_missing_live_struct() {
        let mismatch = CrossMatchMismatch {
            symbol: "NIFTY50".to_string(),
            segment: "IDX_I".to_string(),
            timeframe: "5m".to_string(),
            timestamp_ist: "2026-03-18 14:00 IST".to_string(),
            mismatch_type: "missing_live".to_string(),
            hist_values: "O=23480.0 H=23510.0 L=23475.0 C=23505.0 V=0".to_string(),
            live_values: "[MISSING — no live data for this candle]".to_string(),
            diff_summary: String::new(),
        };
        assert_eq!(mismatch.mismatch_type, "missing_live");
        assert!(mismatch.live_values.contains("MISSING"));
    }

    #[test]
    fn test_cross_match_report_struct() {
        let report = CrossMatchReport {
            timeframes_checked: 5,
            candles_compared: 187500,
            mismatches: 12,
            missing_live: 8,
            missing_historical: 0,
            oi_mismatches: 2,
            mismatch_details: vec![],
            missing_views: vec![],
            per_timeframe_mismatches: vec![],
            passed: false,
            live_candles_present: true,
        };
        assert_eq!(report.timeframes_checked, 5);
        assert_eq!(report.mismatches, 12);
        assert_eq!(report.missing_live, 8);
        assert_eq!(report.oi_mismatches, 2);
        assert!(report.missing_views.is_empty());
        assert!(!report.passed);
    }

    #[test]
    fn test_cross_match_timeframe_mapping() {
        assert_eq!(CROSS_MATCH_TIMEFRAMES.len(), 5);
        // Verify the critical 60m → candles_1h mapping
        let mapping_60m = CROSS_MATCH_TIMEFRAMES.iter().find(|(tf, _)| *tf == "60m");
        assert_eq!(
            mapping_60m,
            Some(&("60m", "candles_1h")),
            "60m must map to candles_1h (not candles_60m)"
        );
    }

    #[test]
    fn test_failed_cross_match_report() {
        let report = failed_cross_match_report();
        assert_eq!(report.timeframes_checked, 0);
        assert_eq!(report.candles_compared, 0);
        assert_eq!(report.mismatches, 0);
        assert_eq!(report.oi_mismatches, 0);
        assert!(report.mismatch_details.is_empty());
        assert!(report.missing_views.is_empty());
        assert!(!report.passed);
    }

    #[test]
    fn test_format_timestamp_ist_iso_format() {
        let ts = serde_json::Value::String("2026-03-18T03:45:00.000000Z".to_string());
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "2026-03-18 03:45 IST");
    }

    #[test]
    fn test_format_timestamp_ist_no_fractional() {
        let ts = serde_json::Value::String("2026-03-18T10:15:00Z".to_string());
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "2026-03-18 10:15 IST");
    }

    #[test]
    fn test_format_timestamp_ist_fallback() {
        let ts = serde_json::json!(1773050340);
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "1773050340");
    }

    #[test]
    fn test_cross_match_exact_price_matching() {
        // EXACT matching: zero tolerance for price comparisons.
        const {
            assert!(
                CROSS_MATCH_PRICE_EPSILON == 0.0,
                "price epsilon must be 0.0 for exact matching"
            );
        }
    }

    #[test]
    fn test_epsilon_catches_real_tick_diff() {
        // Minimum tick in Indian markets = 0.05
        // Epsilon (1e-6) must be WAY below that
        let min_tick = 0.05_f64;
        assert!(
            min_tick > CROSS_MATCH_PRICE_EPSILON * 1000.0,
            "epsilon must be at least 1000x smaller than min tick"
        );
    }

    #[test]
    fn test_cross_match_exact_volume_matching() {
        // EXACT matching: zero tolerance for volume comparisons.
        const {
            assert!(
                CROSS_MATCH_VOLUME_TOLERANCE_PCT == 0.0,
                "volume tolerance must be 0.0 for exact matching"
            );
        }
    }

    #[test]
    fn test_cross_match_exact_oi_matching() {
        // EXACT matching: zero tolerance for OI comparisons.
        const {
            assert!(
                CROSS_MATCH_OI_TOLERANCE_PCT == 0.0,
                "OI tolerance must be 0.0 for exact matching"
            );
        }
    }

    #[test]
    fn test_cross_match_report_includes_oi_mismatches_field() {
        let report = CrossMatchReport {
            timeframes_checked: 3,
            candles_compared: 50000,
            mismatches: 5,
            missing_live: 2,
            missing_historical: 0,
            oi_mismatches: 3,
            mismatch_details: vec![],
            missing_views: vec!["candles_1d".to_string()],
            per_timeframe_mismatches: vec![],
            passed: false,
            live_candles_present: true,
        };
        assert_eq!(report.oi_mismatches, 3);
        assert_eq!(report.missing_views.len(), 1);
        assert_eq!(report.missing_views[0], "candles_1d");
    }

    #[test]
    fn test_cross_match_report_includes_missing_views_field() {
        let report = CrossMatchReport {
            timeframes_checked: 3,
            candles_compared: 50000,
            mismatches: 0,
            missing_live: 0,
            missing_historical: 0,
            oi_mismatches: 0,
            mismatch_details: vec![],
            missing_views: vec!["candles_5m".to_string(), "candles_1d".to_string()],
            per_timeframe_mismatches: vec![],
            passed: false,
            live_candles_present: false,
        };
        assert!(!report.passed, "missing views must fail cross-match");
        assert_eq!(report.missing_views.len(), 2);
    }

    /// Regression: fresh Friday-evening boot (no live ticks) must NOT be
    /// reported as a pass.
    ///
    /// Before the fix at cross_verify.rs + main.rs:3907, the per-timeframe
    /// LEFT JOIN count would return all historical rows even with an
    /// empty live MV, so `candles_compared > 0` while no actual
    /// comparison occurred. `live_candles_present` is the explicit
    /// signal the caller must use to skip the pass/fail decision.
    #[test]
    fn test_cross_match_report_live_candles_present_flag_is_required() {
        // When live MV is empty, live_candles_present = false. The caller
        // at main.rs MUST treat this as SKIP regardless of the
        // `candles_compared` number, which is a meaningless LEFT JOIN
        // count when there is no live data.
        let report = CrossMatchReport {
            timeframes_checked: 0,
            candles_compared: 348_968, // LEFT JOIN count of historical rows
            mismatches: 0,
            missing_live: 0,
            missing_historical: 0,
            oi_mismatches: 0,
            mismatch_details: vec![],
            missing_views: vec![],
            per_timeframe_mismatches: vec![],
            passed: false, // determine_cross_match_passed sees 0 compared+mismatches correctly
            live_candles_present: false, // <-- the new explicit signal
        };
        assert!(
            !report.live_candles_present,
            "fresh boot with empty MV must not claim live candles are present"
        );
    }

    #[test]
    fn test_violation_detail_no_arbitrary_cap() {
        // MAX_VIOLATION_DETAILS is a memory safety cap only — high enough to show everything
        const {
            assert!(
                MAX_VIOLATION_DETAILS >= 100,
                "violation detail cap must be >= 100 to show all violations"
            );
        }
    }

    #[tokio::test]
    async fn test_verify_candle_integrity_unreachable_host() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let registry = InstrumentRegistry::empty();
        let window = TodayIstWindow::from_utc(utc_at_ist(2026, 4, 20, 12, 0))
            .expect("mid-session test window");
        let report = verify_candle_integrity(&config, &registry, &window).await;
        assert!(!report.passed);
        assert_eq!(report.instruments_checked, 0);
    }

    #[tokio::test]
    async fn test_cross_match_unreachable_host() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let registry = InstrumentRegistry::empty();
        let window = TodayIstWindow::from_utc(utc_at_ist(2026, 4, 20, 12, 0))
            .expect("mid-session test window");
        let report = cross_match_historical_vs_live(&config, &registry, &window).await;
        assert!(!report.passed);
        assert_eq!(report.candles_compared, 0);
    }

    // -----------------------------------------------------------------------
    // parse_coverage_rows tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_coverage_rows_empty() {
        let (map, total) = parse_coverage_rows(&[]);
        assert!(map.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn test_parse_coverage_rows_single_row() {
        let dataset = vec![vec![
            serde_json::json!("1m"),
            serde_json::json!(1333),
            serde_json::json!(375),
        ]];
        let (map, total) = parse_coverage_rows(&dataset);
        assert_eq!(total, 375);
        assert_eq!(map.len(), 1);
        assert_eq!(map["1m"], vec![(1333, 375)]);
    }

    #[test]
    fn test_parse_coverage_rows_multiple_timeframes() {
        let dataset = vec![
            vec![
                serde_json::json!("1m"),
                serde_json::json!(100),
                serde_json::json!(375),
            ],
            vec![
                serde_json::json!("5m"),
                serde_json::json!(100),
                serde_json::json!(75),
            ],
            vec![
                serde_json::json!("1m"),
                serde_json::json!(200),
                serde_json::json!(350),
            ],
        ];
        let (map, total) = parse_coverage_rows(&dataset);
        assert_eq!(total, 800);
        assert_eq!(map["1m"].len(), 2);
        assert_eq!(map["5m"].len(), 1);
    }

    #[test]
    fn test_parse_coverage_rows_short_row_skipped() {
        let dataset = vec![
            vec![serde_json::json!("1m"), serde_json::json!(100)], // too short
            vec![
                serde_json::json!("1m"),
                serde_json::json!(200),
                serde_json::json!(100),
            ],
        ];
        let (map, total) = parse_coverage_rows(&dataset);
        assert_eq!(total, 100);
        assert_eq!(map["1m"].len(), 1);
    }

    #[test]
    fn test_parse_coverage_rows_negative_count_clamped() {
        let dataset = vec![vec![
            serde_json::json!("1m"),
            serde_json::json!(100),
            serde_json::json!(-5),
        ]];
        let (map, total) = parse_coverage_rows(&dataset);
        assert_eq!(total, 0);
        assert_eq!(map["1m"], vec![(100, 0)]);
    }

    #[test]
    fn test_parse_coverage_rows_null_values() {
        let dataset = vec![vec![
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ]];
        let (map, total) = parse_coverage_rows(&dataset);
        assert_eq!(total, 0);
        assert_eq!(map[""], vec![(0, 0)]);
    }

    // -----------------------------------------------------------------------
    // build_timeframe_coverage tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_timeframe_coverage_empty() {
        let map = HashMap::new();
        let result = build_timeframe_coverage(&map);
        assert_eq!(result.len(), 5);
        for tc in &result {
            assert_eq!(tc.instrument_count, 0);
            assert_eq!(tc.candle_count, 0);
        }
    }

    #[test]
    fn test_build_timeframe_coverage_partial() {
        let mut map: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
        map.insert("1m".to_string(), vec![(100, 375), (200, 300)]);
        map.insert("1d".to_string(), vec![(100, 1)]);

        let result = build_timeframe_coverage(&map);
        let tf_1m = result.iter().find(|tc| tc.timeframe == "1m").unwrap();
        assert_eq!(tf_1m.instrument_count, 2);
        assert_eq!(tf_1m.candle_count, 675);

        let tf_1d = result.iter().find(|tc| tc.timeframe == "1d").unwrap();
        assert_eq!(tf_1d.instrument_count, 1);
        assert_eq!(tf_1d.candle_count, 1);

        let tf_5m = result.iter().find(|tc| tc.timeframe == "5m").unwrap();
        assert_eq!(tf_5m.instrument_count, 0);
        assert_eq!(tf_5m.candle_count, 0);
    }

    #[test]
    fn test_build_timeframe_coverage_all_five_timeframes() {
        let result = build_timeframe_coverage(&HashMap::new());
        let timeframes: Vec<&str> = result.iter().map(|tc| tc.timeframe.as_str()).collect();
        assert_eq!(timeframes, vec!["1m", "5m", "15m", "60m", "1d"]);
    }

    // -----------------------------------------------------------------------
    // classify_1m_coverage tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_1m_coverage_empty() {
        let (complete, gaps) = classify_1m_coverage(&[], 356);
        assert_eq!(complete, 0);
        assert_eq!(gaps, 0);
    }

    #[test]
    fn test_classify_1m_coverage_all_complete() {
        let instruments = vec![(100, 375), (200, 356), (300, 400)];
        let (complete, gaps) = classify_1m_coverage(&instruments, 356);
        assert_eq!(complete, 3);
        assert_eq!(gaps, 0);
    }

    #[test]
    fn test_classify_1m_coverage_all_gaps() {
        let instruments = vec![(100, 100), (200, 50), (300, 0)];
        let (complete, gaps) = classify_1m_coverage(&instruments, 356);
        assert_eq!(complete, 0);
        assert_eq!(gaps, 3);
    }

    #[test]
    fn test_classify_1m_coverage_mixed() {
        let instruments = vec![(100, 375), (200, 100), (300, 356)];
        let (complete, gaps) = classify_1m_coverage(&instruments, 356);
        assert_eq!(complete, 2);
        assert_eq!(gaps, 1);
    }

    #[test]
    fn test_classify_1m_coverage_exact_threshold() {
        let instruments = vec![(100, 356)];
        let (complete, gaps) = classify_1m_coverage(&instruments, 356);
        assert_eq!(complete, 1);
        assert_eq!(gaps, 0);
    }

    #[test]
    fn test_classify_1m_coverage_one_below_threshold() {
        let instruments = vec![(100, 355)];
        let (complete, gaps) = classify_1m_coverage(&instruments, 356);
        assert_eq!(complete, 0);
        assert_eq!(gaps, 1);
    }

    // -----------------------------------------------------------------------
    // count_unique_instruments tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_count_unique_instruments_empty() {
        assert_eq!(count_unique_instruments(&HashMap::new()), 0);
    }

    #[test]
    fn test_count_unique_instruments_no_overlap() {
        let mut map: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
        map.insert("1m".to_string(), vec![(100, 375)]);
        map.insert("5m".to_string(), vec![(200, 75)]);
        assert_eq!(count_unique_instruments(&map), 2);
    }

    #[test]
    fn test_count_unique_instruments_with_overlap() {
        let mut map: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
        map.insert("1m".to_string(), vec![(100, 375), (200, 300)]);
        map.insert("5m".to_string(), vec![(100, 75), (300, 60)]);
        // Unique: 100, 200, 300
        assert_eq!(count_unique_instruments(&map), 3);
    }

    // -----------------------------------------------------------------------
    // determine_verification_passed tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_verification_passed_all_good() {
        assert!(determine_verification_passed(100, 0, 0, 0, 0, 0));
    }

    #[test]
    fn test_determine_verification_passed_zero_instruments() {
        assert!(!determine_verification_passed(0, 0, 0, 0, 0, 0));
    }

    #[test]
    fn test_determine_verification_passed_with_gaps() {
        assert!(!determine_verification_passed(100, 5, 0, 0, 0, 0));
    }

    #[test]
    fn test_determine_verification_passed_with_ohlc() {
        assert!(!determine_verification_passed(100, 0, 3, 0, 0, 0));
    }

    #[test]
    fn test_determine_verification_passed_with_data_violations() {
        assert!(!determine_verification_passed(100, 0, 0, 2, 0, 0));
    }

    #[test]
    fn test_determine_verification_passed_with_timestamp_violations() {
        assert!(!determine_verification_passed(100, 0, 0, 0, 1, 0));
    }

    #[test]
    fn test_determine_verification_passed_with_weekend_violations() {
        assert!(!determine_verification_passed(100, 0, 0, 0, 0, 1));
    }

    #[test]
    fn test_determine_verification_passed_all_violations() {
        assert!(!determine_verification_passed(100, 1, 2, 3, 4, 5));
    }

    // -----------------------------------------------------------------------
    // has_price_mismatch tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_price_mismatch_no_mismatch() {
        assert!(!has_price_mismatch(0.0, 0.0, 0.0, 0.0, 1e-6));
    }

    #[test]
    fn test_has_price_mismatch_open_exceeds() {
        assert!(has_price_mismatch(0.01, 0.0, 0.0, 0.0, 1e-6));
    }

    #[test]
    fn test_has_price_mismatch_high_exceeds() {
        assert!(has_price_mismatch(0.0, -0.05, 0.0, 0.0, 1e-6));
    }

    #[test]
    fn test_has_price_mismatch_low_exceeds() {
        assert!(has_price_mismatch(0.0, 0.0, 0.001, 0.0, 1e-6));
    }

    #[test]
    fn test_has_price_mismatch_close_exceeds() {
        assert!(has_price_mismatch(0.0, 0.0, 0.0, -1.5, 1e-6));
    }

    #[test]
    fn test_has_price_mismatch_within_epsilon() {
        let eps = 1e-6;
        assert!(!has_price_mismatch(1e-7, -1e-7, 5e-8, -3e-8, eps));
    }

    // -----------------------------------------------------------------------
    // has_volume_mismatch tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_volume_mismatch_exact_matching() {
        // EXACT matching: any difference flags as mismatch.
        assert!(has_volume_mismatch(100000, 105000, 0.0));
        assert!(has_volume_mismatch(100000, 100001, 0.0));
        assert!(has_volume_mismatch(100, 110, 0.0));
    }

    #[test]
    fn test_has_volume_mismatch_zero_hist() {
        // h_volume = 0, max(0,1) = 1, diff = |50-0|/1 = 50.0 > 0.10
        assert!(has_volume_mismatch(0, 50, 0.10));
    }

    #[test]
    fn test_has_volume_mismatch_both_zero() {
        assert!(!has_volume_mismatch(0, 0, 0.10));
    }

    // -----------------------------------------------------------------------
    // has_oi_mismatch tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_oi_mismatch_both_zero() {
        assert!(!has_oi_mismatch(0, 0, 0.10));
    }

    #[test]
    fn test_has_oi_mismatch_exact_matching() {
        // EXACT matching: any difference flags as mismatch, even if one is zero.
        assert!(has_oi_mismatch(0, 1000, 0.0));
        assert!(has_oi_mismatch(1000, 0, 0.0));
        assert!(has_oi_mismatch(10000, 10500, 0.0));
        assert!(has_oi_mismatch(10000, 10001, 0.0));
    }

    #[test]
    fn test_has_oi_mismatch_exceeds_tolerance() {
        assert!(has_oi_mismatch(10000, 12000, 0.10));
    }

    // -----------------------------------------------------------------------
    // classify_mismatch_type tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_mismatch_type_oi_only() {
        assert_eq!(classify_mismatch_type(false, false, true), "oi_diff");
    }

    #[test]
    fn test_classify_mismatch_type_volume_only() {
        assert_eq!(classify_mismatch_type(false, true, false), "volume_diff");
    }

    #[test]
    fn test_classify_mismatch_type_price_only() {
        assert_eq!(classify_mismatch_type(true, false, false), "price_diff");
    }

    #[test]
    fn test_classify_mismatch_type_price_and_volume() {
        assert_eq!(classify_mismatch_type(true, true, false), "price_diff");
    }

    #[test]
    fn test_classify_mismatch_type_all_mismatches() {
        assert_eq!(classify_mismatch_type(true, true, true), "price_diff");
    }

    #[test]
    fn test_classify_mismatch_type_volume_and_oi() {
        // When both volume and OI mismatch but no price → falls to volume_diff
        assert_eq!(classify_mismatch_type(false, true, true), "volume_diff");
    }

    #[test]
    fn test_classify_mismatch_type_none() {
        // Edge case: no mismatches at all → defaults to price_diff
        assert_eq!(classify_mismatch_type(false, false, false), "price_diff");
    }

    // -----------------------------------------------------------------------
    // build_diff_summary tests
    // -----------------------------------------------------------------------

    // Helper to build DiffSummaryParams concisely in tests
    fn diff_params(
        d_open: f64,
        d_high: f64,
        d_low: f64,
        d_close: f64,
        h_volume: i64,
        m_volume: i64,
        h_oi: i64,
        m_oi: i64,
        volume_mismatch: bool,
        oi_mismatch: bool,
    ) -> DiffSummaryParams {
        DiffSummaryParams {
            d_open,
            d_high,
            d_low,
            d_close,
            h_volume,
            m_volume,
            h_oi,
            m_oi,
            epsilon: 1e-6,
            volume_mismatch,
            oi_mismatch,
        }
    }

    fn test_ctx(symbol: &str, segment: &str, tf: &str, ts: &str) -> CandleContext {
        CandleContext {
            symbol: symbol.to_string(),
            segment: segment.to_string(),
            timeframe: tf.to_string(),
            timestamp_ist: ts.to_string(),
        }
    }

    fn cv(open: f64, high: f64, low: f64, close: f64, volume: i64, oi: i64) -> CandleValues {
        CandleValues {
            open,
            high,
            low,
            close,
            volume,
            oi,
        }
    }

    #[test]
    fn test_build_diff_summary_price_only() {
        let result = build_diff_summary(&diff_params(
            1.5, 0.0, 0.0, -0.5, 1000, 1000, 0, 0, false, false,
        ));
        assert!(result.contains("O(+1.50)"));
        assert!(result.contains("C(-0.50)"));
        assert!(!result.contains('H'));
        assert!(!result.contains('L'));
        assert!(!result.contains('V'));
    }

    #[test]
    fn test_build_diff_summary_volume_mismatch() {
        let result = build_diff_summary(&diff_params(
            0.0, 0.0, 0.0, 0.0, 1000, 1200, 0, 0, true, false,
        ));
        assert!(result.contains('V'));
        assert!(!result.contains('O'));
    }

    #[test]
    fn test_build_diff_summary_oi_mismatch() {
        let result = build_diff_summary(&diff_params(
            0.0, 0.0, 0.0, 0.0, 1000, 1000, 5000, 6000, false, true,
        ));
        assert!(result.contains("OI("));
    }

    #[test]
    fn test_build_diff_summary_all_fields() {
        let result = build_diff_summary(&diff_params(
            1.0, -2.0, 0.5, -0.3, 1000, 1500, 5000, 6000, true, true,
        ));
        assert!(result.contains("O(+1.00)"));
        assert!(result.contains("H(-2.00)"));
        assert!(result.contains("L(+0.50)"));
        assert!(result.contains("C(-0.30)"));
        assert!(result.contains('V'));
        assert!(result.contains("OI("));
    }

    #[test]
    fn test_build_diff_summary_empty_when_no_diffs() {
        let result = build_diff_summary(&diff_params(
            0.0, 0.0, 0.0, 0.0, 1000, 1000, 0, 0, false, false,
        ));
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // CandleValues format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_candle_values_format_ohlcv() {
        let c = cv(100.5, 110.0, 95.0, 105.0, 50000, 0);
        assert_eq!(c.format_ohlcv(), "O=100.5 H=110 L=95 C=105 V=50000");
    }

    #[test]
    fn test_candle_values_format_with_oi() {
        let c = cv(100.5, 110.0, 95.0, 105.0, 50000, 12000);
        assert_eq!(
            c.format_with_oi(),
            "O=100.5 H=110 L=95 C=105 V=50000 OI=12000"
        );
    }

    // -----------------------------------------------------------------------
    // build_violation_detail tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_violation_detail() {
        let detail = build_violation_detail(
            test_ctx("RELIANCE", "NSE_EQ", "1m", "2026-03-18 10:15 IST"),
            "high < low",
            cv(2450.0, 2440.0, 2450.0, 2445.0, 100, 0),
        );
        assert_eq!(detail.symbol, "RELIANCE");
        assert_eq!(detail.violation, "high < low");
        assert!(detail.values.contains("O=2450"));
        assert!(detail.values.contains("H=2440"));
    }

    #[test]
    fn test_build_violation_detail_price_violation() {
        let detail = build_violation_detail(
            test_ctx("TCS", "NSE_EQ", "5m", "2026-03-18 11:00 IST"),
            "price <= 0",
            cv(0.0, 100.0, 95.0, 98.0, 500, 0),
        );
        assert_eq!(detail.violation, "price <= 0");
        assert!(detail.values.contains("O=0"));
    }

    // -----------------------------------------------------------------------
    // classify_cross_match_row tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_cross_match_row_no_mismatch() {
        let result = classify_cross_match_row(
            test_ctx("RELIANCE", "NSE_EQ", "1m", "2026-03-18 10:15 IST"),
            cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
            cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
        );
        assert!(result.is_none(), "identical values should return None");
    }

    #[test]
    fn test_classify_cross_match_row_price_diff() {
        let result = classify_cross_match_row(
            test_ctx("RELIANCE", "NSE_EQ", "1m", "2026-03-18 10:15 IST"),
            cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
            cv(2450.0, 2465.0, 2448.0, 2455.0, 125000, 0),
        );
        let mismatch = result.unwrap();
        assert_eq!(mismatch.mismatch_type, "price_diff");
        assert!(mismatch.diff_summary.contains('C'));
    }

    #[test]
    fn test_classify_cross_match_row_volume_diff() {
        let result = classify_cross_match_row(
            test_ctx("TCS", "NSE_EQ", "5m", "2026-03-18 11:00 IST"),
            cv(3500.0, 3510.0, 3495.0, 3505.0, 100000, 0),
            cv(3500.0, 3510.0, 3495.0, 3505.0, 120000, 0),
        );
        let mismatch = result.unwrap();
        assert_eq!(mismatch.mismatch_type, "volume_diff");
        assert!(mismatch.diff_summary.contains('V'));
    }

    #[test]
    fn test_classify_cross_match_row_oi_diff() {
        let result = classify_cross_match_row(
            test_ctx("NIFTY", "NSE_FNO", "1m", "2026-03-18 09:30 IST"),
            cv(23000.0, 23050.0, 22990.0, 23040.0, 500000, 10000),
            cv(23000.0, 23050.0, 22990.0, 23040.0, 500000, 12000),
        );
        let mismatch = result.unwrap();
        assert_eq!(mismatch.mismatch_type, "oi_diff");
        assert!(mismatch.diff_summary.contains("OI("));
    }

    #[test]
    fn test_classify_cross_match_row_exact_volume_mismatch() {
        // EXACT matching: any volume diff = mismatch.
        let result = classify_cross_match_row(
            test_ctx("TCS", "NSE_EQ", "1m", "2026-03-18 10:00 IST"),
            cv(3500.0, 3510.0, 3495.0, 3505.0, 100000, 0),
            cv(3500.0, 3510.0, 3495.0, 3505.0, 105000, 0),
        );
        assert!(
            result.is_some(),
            "exact matching: 5% volume diff IS a mismatch"
        );
    }

    #[test]
    fn test_classify_cross_match_row_all_mismatches() {
        let result = classify_cross_match_row(
            test_ctx("RELIANCE", "NSE_FNO", "15m", "2026-03-18 12:00 IST"),
            cv(2450.0, 2465.0, 2448.0, 2460.0, 100000, 10000),
            cv(2455.0, 2470.0, 2445.0, 2450.0, 120000, 12000),
        );
        let mismatch = result.unwrap();
        assert_eq!(mismatch.mismatch_type, "price_diff");
        assert!(mismatch.diff_summary.contains('O'));
        assert!(mismatch.diff_summary.contains('H'));
        assert!(mismatch.diff_summary.contains('V'));
        assert!(mismatch.diff_summary.contains("OI("));
    }

    // -----------------------------------------------------------------------
    // build_missing_live_mismatch tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_missing_live_mismatch() {
        let mismatch = build_missing_live_mismatch(
            test_ctx("RELIANCE", "NSE_EQ", "1m", "2026-03-18 10:15 IST"),
            cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
        );
        assert_eq!(mismatch.mismatch_type, "missing_live");
        assert!(mismatch.live_values.contains("MISSING"));
        assert!(mismatch.diff_summary.is_empty());
        assert!(mismatch.hist_values.contains("O=2450"));
    }

    #[test]
    fn test_build_missing_live_mismatch_with_oi() {
        let mismatch = build_missing_live_mismatch(
            test_ctx("NIFTY", "NSE_FNO", "5m", "2026-03-18 14:00 IST"),
            cv(23000.0, 23050.0, 22990.0, 23040.0, 500000, 15000),
        );
        assert!(mismatch.hist_values.contains("OI=15000"));
        assert_eq!(mismatch.timeframe, "5m");
    }

    // -----------------------------------------------------------------------
    // determine_cross_match_passed tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_cross_match_passed_all_good() {
        assert!(determine_cross_match_passed(0, 100, 0));
    }

    #[test]
    fn test_determine_cross_match_passed_with_mismatches() {
        assert!(!determine_cross_match_passed(5, 100, 0));
    }

    #[test]
    fn test_determine_cross_match_passed_zero_compared() {
        assert!(!determine_cross_match_passed(0, 0, 0));
    }

    #[test]
    fn test_determine_cross_match_passed_with_missing_views() {
        assert!(!determine_cross_match_passed(0, 100, 2));
    }

    #[test]
    fn test_determine_cross_match_passed_mismatches_and_missing_views() {
        assert!(!determine_cross_match_passed(3, 100, 1));
    }

    // -----------------------------------------------------------------------
    // format_timestamp_ist additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_timestamp_ist_short_time() {
        let ts = serde_json::Value::String("2026-03-18T09:15Z".to_string());
        let result = format_timestamp_ist(&ts);
        // Time part "09:15" with Z trimmed, len >= 5
        assert_eq!(result, "2026-03-18 09:15 IST");
    }

    #[test]
    fn test_format_timestamp_ist_no_t_separator() {
        let ts = serde_json::Value::String("2026-03-18 10:30:00".to_string());
        let result = format_timestamp_ist(&ts);
        // No 'T' separator — returns as-is
        assert_eq!(result, "2026-03-18 10:30:00");
    }

    #[test]
    fn test_format_timestamp_ist_null_value() {
        let ts = serde_json::Value::Null;
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "null");
    }

    #[test]
    fn test_format_timestamp_ist_boolean_value() {
        let ts = serde_json::json!(true);
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "true");
    }

    // -----------------------------------------------------------------------
    // lookup_symbol tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_lookup_symbol_empty_registry() {
        let registry = InstrumentRegistry::empty();
        let result = lookup_symbol(&registry, 1333);
        assert_eq!(result, "1333");
    }

    #[test]
    fn test_lookup_symbol_zero_id() {
        let registry = InstrumentRegistry::empty();
        let result = lookup_symbol(&registry, 0);
        assert_eq!(result, "0");
    }

    // -----------------------------------------------------------------------
    // Additional coverage tests — format_timestamp_ist edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_timestamp_ist_only_date_part() {
        let ts = serde_json::Value::String("2026-03-18T".to_string());
        let result = format_timestamp_ist(&ts);
        // Has 'T' but time_rest is empty — will produce date + " " + something short
        assert!(result.contains("2026-03-18"));
    }

    #[test]
    fn test_format_timestamp_ist_very_short_time_part() {
        let ts = serde_json::Value::String("2026-03-18T1Z".to_string());
        let result = format_timestamp_ist(&ts);
        // time_part after trimming Z is "1", len < 5 so returns as-is
        assert_eq!(result, "2026-03-18 1 IST");
    }

    #[test]
    fn test_format_timestamp_ist_with_fractional_and_z() {
        let ts = serde_json::Value::String("2026-03-18T14:30:00.123456Z".to_string());
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "2026-03-18 14:30 IST");
    }

    #[test]
    fn test_format_timestamp_ist_float_value() {
        let ts = serde_json::json!(1773050340.5);
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "1773050340.5");
    }

    #[test]
    fn test_format_timestamp_ist_array_value() {
        let ts = serde_json::json!([1, 2, 3]);
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "[1,2,3]");
    }

    // -----------------------------------------------------------------------
    // lookup_symbol with negative id
    // -----------------------------------------------------------------------

    #[test]
    fn test_lookup_symbol_negative_id() {
        let registry = InstrumentRegistry::empty();
        // Negative i64 cast to u32 wraps — should still return the i64 as string
        let result = lookup_symbol(&registry, -1);
        assert_eq!(result, "-1");
    }

    #[test]
    fn test_lookup_symbol_large_id() {
        let registry = InstrumentRegistry::empty();
        let result = lookup_symbol(&registry, 999999);
        assert_eq!(result, "999999");
    }

    // -----------------------------------------------------------------------
    // CandleValues formatting edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_candle_values_format_ohlcv_zero_values() {
        let c = cv(0.0, 0.0, 0.0, 0.0, 0, 0);
        let result = c.format_ohlcv();
        assert_eq!(result, "O=0 H=0 L=0 C=0 V=0");
    }

    #[test]
    fn test_candle_values_format_with_oi_zero_oi() {
        let c = cv(100.0, 110.0, 95.0, 105.0, 50000, 0);
        let result = c.format_with_oi();
        assert!(result.contains("OI=0"));
    }

    #[test]
    fn test_candle_values_format_ohlcv_large_values() {
        let c = cv(99999.99, 100000.0, 99999.0, 99999.5, i64::MAX, 0);
        let result = c.format_ohlcv();
        assert!(result.contains("O=99999.99"));
        assert!(result.contains(&format!("V={}", i64::MAX)));
    }

    #[test]
    fn test_candle_values_format_with_oi_large_oi() {
        let c = cv(100.0, 110.0, 95.0, 105.0, 50000, 9999999);
        let result = c.format_with_oi();
        assert!(result.contains("OI=9999999"));
    }

    // -----------------------------------------------------------------------
    // build_violation_detail additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_violation_detail_timestamp_violation() {
        let detail = build_violation_detail(
            test_ctx("INFY", "NSE_EQ", "1m", "2026-03-18 08:00 IST"),
            "outside 09:15-15:29 market hours",
            cv(1500.0, 1510.0, 1495.0, 1505.0, 1000, 0),
        );
        assert_eq!(detail.violation, "outside 09:15-15:29 market hours");
        assert_eq!(detail.symbol, "INFY");
        assert_eq!(detail.segment, "NSE_EQ");
        assert_eq!(detail.timeframe, "1m");
        assert_eq!(detail.timestamp_ist, "2026-03-18 08:00 IST");
    }

    // -----------------------------------------------------------------------
    // classify_cross_match_row additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_cross_match_row_epsilon_boundary() {
        // Differences exactly at epsilon — should NOT trigger mismatch
        let eps = CROSS_MATCH_PRICE_EPSILON;
        let result = classify_cross_match_row(
            test_ctx("TCS", "NSE_EQ", "1m", "2026-03-18 10:00 IST"),
            cv(100.0, 200.0, 50.0, 150.0, 100000, 0),
            cv(100.0 + eps, 200.0 - eps, 50.0 + eps, 150.0 - eps, 100000, 0),
        );
        assert!(result.is_none(), "diffs at epsilon boundary should pass");
    }

    #[test]
    fn test_classify_cross_match_row_any_price_diff_is_mismatch() {
        // EXACT matching: any price difference = mismatch.
        let result = classify_cross_match_row(
            test_ctx("TCS", "NSE_EQ", "1m", "2026-03-18 10:00 IST"),
            cv(100.0, 200.0, 50.0, 150.0, 100000, 0),
            cv(100.05, 200.0, 50.0, 150.0, 100000, 0),
        );
        assert!(result.is_some(), "any price diff should trigger mismatch");
        let m = result.unwrap();
        assert_eq!(m.mismatch_type, "price_diff");
    }

    // -----------------------------------------------------------------------
    // build_missing_live_mismatch additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_missing_live_mismatch_preserves_context() {
        let mismatch = build_missing_live_mismatch(
            test_ctx("INFY", "NSE_EQ", "15m", "2026-03-18 12:30 IST"),
            cv(1500.0, 1510.0, 1495.0, 1505.0, 75000, 0),
        );
        assert_eq!(mismatch.symbol, "INFY");
        assert_eq!(mismatch.segment, "NSE_EQ");
        assert_eq!(mismatch.timeframe, "15m");
        assert_eq!(mismatch.timestamp_ist, "2026-03-18 12:30 IST");
        assert_eq!(mismatch.mismatch_type, "missing_live");
    }

    // -----------------------------------------------------------------------
    // has_volume_mismatch edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_volume_mismatch_negative_volumes() {
        // Negative volumes should still work with saturating_sub
        assert!(has_volume_mismatch(-100, 100, 0.10));
    }

    #[test]
    fn test_has_volume_mismatch_large_volumes_exact() {
        // EXACT matching: even large proportionally close volumes flag as mismatch.
        assert!(has_volume_mismatch(1_000_000, 1_050_000, 0.0));
        // Equal volumes = no mismatch.
        assert!(!has_volume_mismatch(1_000_000, 1_000_000, 0.0));
    }

    // -----------------------------------------------------------------------
    // has_oi_mismatch edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_oi_mismatch_equal_values() {
        // Identical OI values — no mismatch
        assert!(!has_oi_mismatch(5000, 5000, 0.10));
    }

    #[test]
    fn test_has_oi_mismatch_negative_oi() {
        // EXACT matching: different values = mismatch, even if negative.
        assert!(has_oi_mismatch(-100, 100, 0.0));
        assert!(has_oi_mismatch(100, -100, 0.0));
        // Equal negative values = no mismatch.
        assert!(!has_oi_mismatch(-100, -100, 0.0));
    }

    // -----------------------------------------------------------------------
    // build_diff_summary edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_diff_summary_negative_volume_diff() {
        let result = build_diff_summary(&diff_params(
            0.0, 0.0, 0.0, 0.0, 2000, 1000, 0, 0, true, false,
        ));
        assert!(result.contains('V'));
        assert!(result.contains('-'));
    }

    #[test]
    fn test_build_diff_summary_oi_only() {
        let result = build_diff_summary(&diff_params(
            0.0, 0.0, 0.0, 0.0, 1000, 1000, 10000, 15000, false, true,
        ));
        assert!(result.contains("OI("));
        // Should not contain "O(" price diff — only "OI("
        assert!(!result.contains("O("));
        assert!(!result.contains('V'));
    }

    // -----------------------------------------------------------------------
    // determine_cross_match_passed edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_cross_match_passed_zero_mismatches_zero_compared() {
        // Zero compared = fails even with zero mismatches
        assert!(!determine_cross_match_passed(0, 0, 0));
    }

    #[test]
    fn test_determine_cross_match_passed_large_values() {
        assert!(determine_cross_match_passed(0, 1_000_000, 0));
    }

    // -----------------------------------------------------------------------
    // failed_cross_match_report tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_failed_cross_match_report_not_passed() {
        let report = failed_cross_match_report();
        assert!(!report.passed);
        assert_eq!(report.missing_historical, 0);
    }

    // -----------------------------------------------------------------------
    // parse_coverage_rows additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_coverage_rows_extra_columns_ignored() {
        let dataset = vec![vec![
            serde_json::json!("1m"),
            serde_json::json!(1333),
            serde_json::json!(375),
            serde_json::json!("extra_column"), // extra columns are fine
        ]];
        let (map, total) = parse_coverage_rows(&dataset);
        assert_eq!(total, 375);
        assert_eq!(map["1m"], vec![(1333, 375)]);
    }

    #[test]
    fn test_parse_coverage_rows_float_count_truncated() {
        let dataset = vec![vec![
            serde_json::json!("5m"),
            serde_json::json!(100),
            serde_json::json!(99.9), // float — as_i64 returns None → 0
        ]];
        let (map, total) = parse_coverage_rows(&dataset);
        // serde_json::Value::Number(99.9).as_i64() is None → 0
        assert_eq!(total, 0);
        assert_eq!(map["5m"], vec![(100, 0)]);
    }

    // -----------------------------------------------------------------------
    // classify_1m_coverage edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_1m_coverage_zero_threshold() {
        // All instruments are "complete" when threshold is 0
        let instruments = vec![(100, 0), (200, 1)];
        let (complete, gaps) = classify_1m_coverage(&instruments, 0);
        assert_eq!(complete, 2);
        assert_eq!(gaps, 0);
    }

    #[test]
    fn test_classify_1m_coverage_large_threshold() {
        let instruments = vec![(100, 375), (200, 500)];
        let (complete, gaps) = classify_1m_coverage(&instruments, 1000);
        assert_eq!(complete, 0);
        assert_eq!(gaps, 2);
    }

    // -----------------------------------------------------------------------
    // count_unique_instruments edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_count_unique_instruments_duplicate_ids() {
        let mut map: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
        map.insert("1m".to_string(), vec![(100, 375), (100, 300)]); // same id
        assert_eq!(count_unique_instruments(&map), 1);
    }

    #[test]
    fn test_count_unique_instruments_all_same_id() {
        let mut map: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
        map.insert("1m".to_string(), vec![(42, 375)]);
        map.insert("5m".to_string(), vec![(42, 75)]);
        map.insert("15m".to_string(), vec![(42, 25)]);
        map.insert("60m".to_string(), vec![(42, 7)]);
        map.insert("1d".to_string(), vec![(42, 1)]);
        assert_eq!(count_unique_instruments(&map), 1);
    }

    // -----------------------------------------------------------------------
    // parse_single_violation_row tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_single_violation_row_valid() {
        let row = vec![
            serde_json::json!(1333),                          // security_id
            serde_json::json!("NSE_EQ"),                      // segment
            serde_json::json!("1m"),                          // timeframe
            serde_json::json!("2026-03-18T10:15:00.000000Z"), // ts
            serde_json::json!(2450.0),                        // open
            serde_json::json!(2465.0),                        // high
            serde_json::json!(2448.0),                        // low
            serde_json::json!(2460.0),                        // close
            serde_json::json!(125000),                        // volume
        ];
        let parsed = parse_single_violation_row(&row).unwrap();
        assert_eq!(parsed.security_id, 1333);
        assert_eq!(parsed.segment, "NSE_EQ");
        assert_eq!(parsed.timeframe, "1m");
        assert_eq!(parsed.open, 2450.0);
        assert_eq!(parsed.high, 2465.0);
        assert_eq!(parsed.low, 2448.0);
        assert_eq!(parsed.close, 2460.0);
        assert_eq!(parsed.volume, 125000);
    }

    #[test]
    fn test_parse_single_violation_row_too_short() {
        let row = vec![
            serde_json::json!(1333),
            serde_json::json!("NSE_EQ"),
            serde_json::json!("1m"),
        ];
        assert!(parse_single_violation_row(&row).is_none());
    }

    #[test]
    fn test_parse_single_violation_row_empty() {
        assert!(parse_single_violation_row(&[]).is_none());
    }

    #[test]
    fn test_parse_single_violation_row_exactly_8_columns() {
        // 8 columns is still too short (need 9)
        let row: Vec<serde_json::Value> = (0..8).map(|i| serde_json::json!(i)).collect();
        assert!(parse_single_violation_row(&row).is_none());
    }

    #[test]
    fn test_parse_single_violation_row_null_values_default() {
        let row = vec![
            serde_json::Value::Null, // security_id -> 0
            serde_json::Value::Null, // segment -> ""
            serde_json::Value::Null, // timeframe -> ""
            serde_json::Value::Null, // ts
            serde_json::Value::Null, // open -> 0.0
            serde_json::Value::Null, // high -> 0.0
            serde_json::Value::Null, // low -> 0.0
            serde_json::Value::Null, // close -> 0.0
            serde_json::Value::Null, // volume -> 0
        ];
        let parsed = parse_single_violation_row(&row).unwrap();
        assert_eq!(parsed.security_id, 0);
        assert_eq!(parsed.segment, "");
        assert_eq!(parsed.timeframe, "");
        assert_eq!(parsed.open, 0.0);
        assert_eq!(parsed.high, 0.0);
        assert_eq!(parsed.low, 0.0);
        assert_eq!(parsed.close, 0.0);
        assert_eq!(parsed.volume, 0);
    }

    #[test]
    fn test_parse_single_violation_row_extra_columns() {
        let row = vec![
            serde_json::json!(42),
            serde_json::json!("NSE_FNO"),
            serde_json::json!("5m"),
            serde_json::json!("2026-03-18T12:00:00Z"),
            serde_json::json!(100.0),
            serde_json::json!(110.0),
            serde_json::json!(95.0),
            serde_json::json!(105.0),
            serde_json::json!(50000),
            serde_json::json!("extra"), // extra column — ignored
        ];
        let parsed = parse_single_violation_row(&row).unwrap();
        assert_eq!(parsed.security_id, 42);
        assert_eq!(parsed.segment, "NSE_FNO");
    }

    #[test]
    fn test_parse_single_violation_row_string_number_defaults() {
        // String in numeric field -> as_i64() returns None -> defaults to 0
        let row = vec![
            serde_json::json!("not_a_number"),
            serde_json::json!("NSE_EQ"),
            serde_json::json!("1m"),
            serde_json::json!("2026-03-18T10:15:00Z"),
            serde_json::json!("not_float"),
            serde_json::json!(100.0),
            serde_json::json!(95.0),
            serde_json::json!(98.0),
            serde_json::json!("not_int"),
        ];
        let parsed = parse_single_violation_row(&row).unwrap();
        assert_eq!(parsed.security_id, 0);
        assert_eq!(parsed.open, 0.0);
        assert_eq!(parsed.volume, 0);
    }

    // -----------------------------------------------------------------------
    // violation_row_to_detail tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_violation_row_to_detail_basic() {
        let parsed = ParsedViolationRow {
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            timeframe: "1m".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:15:00.000000Z"),
            open: 2450.0,
            high: 2440.0,
            low: 2460.0,
            close: 2445.0,
            volume: 100,
        };
        let registry = InstrumentRegistry::empty();
        let detail = violation_row_to_detail(&parsed, &registry, "high < low");
        assert_eq!(detail.symbol, "1333"); // empty registry falls back to id
        assert_eq!(detail.segment, "NSE_EQ");
        assert_eq!(detail.timeframe, "1m");
        assert_eq!(detail.violation, "high < low");
        assert!(detail.values.contains("O=2450"));
        assert!(detail.values.contains("H=2440"));
        assert_eq!(detail.timestamp_ist, "2026-03-18 10:15 IST");
    }

    #[test]
    fn test_violation_row_to_detail_price_violation() {
        let parsed = ParsedViolationRow {
            security_id: 42,
            segment: "NSE_FNO".to_string(),
            timeframe: "5m".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T11:00:00Z"),
            open: 0.0,
            high: 100.0,
            low: 95.0,
            close: 98.0,
            volume: 500,
        };
        let registry = InstrumentRegistry::empty();
        let detail = violation_row_to_detail(&parsed, &registry, "price <= 0");
        assert_eq!(detail.violation, "price <= 0");
        assert!(detail.values.contains("O=0"));
    }

    // -----------------------------------------------------------------------
    // parse_single_cross_match_row tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_single_cross_match_row_valid_with_live() {
        let row = vec![
            serde_json::json!(1333),                   // security_id
            serde_json::json!("NSE_EQ"),               // segment
            serde_json::json!("2026-03-18T10:15:00Z"), // ts
            serde_json::json!(2450.0),                 // h_open
            serde_json::json!(2465.0),                 // h_high
            serde_json::json!(2448.0),                 // h_low
            serde_json::json!(2460.0),                 // h_close
            serde_json::json!(125000),                 // h_volume
            serde_json::json!(2450.0),                 // m_open
            serde_json::json!(2465.0),                 // m_high
            serde_json::json!(2448.0),                 // m_low
            serde_json::json!(2460.0),                 // m_close
            serde_json::json!(125000),                 // m_volume
            serde_json::json!(0),                      // h_oi
            serde_json::json!(0),                      // m_oi
        ];
        let parsed = parse_single_cross_match_row(&row).unwrap();
        assert_eq!(parsed.security_id, 1333);
        assert_eq!(parsed.segment, "NSE_EQ");
        assert!(!parsed.live_is_null);
        assert_eq!(parsed.hist.open, 2450.0);
        assert_eq!(parsed.hist.high, 2465.0);
        assert_eq!(parsed.live.open, 2450.0);
        assert_eq!(parsed.live.volume, 125000);
    }

    #[test]
    fn test_parse_single_cross_match_row_live_null() {
        let row = vec![
            serde_json::json!(1333),
            serde_json::json!("NSE_EQ"),
            serde_json::json!("2026-03-18T10:15:00Z"),
            serde_json::json!(2450.0),
            serde_json::json!(2465.0),
            serde_json::json!(2448.0),
            serde_json::json!(2460.0),
            serde_json::json!(125000),
            serde_json::Value::Null, // live open is NULL (missing live)
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!(5000), // h_oi
            serde_json::Value::Null, // m_oi
        ];
        let parsed = parse_single_cross_match_row(&row).unwrap();
        assert!(parsed.live_is_null);
        assert_eq!(parsed.hist.oi, 5000);
        // Live values should be zeroed when null
        assert_eq!(parsed.live.open, 0.0);
        assert_eq!(parsed.live.volume, 0);
    }

    #[test]
    fn test_parse_single_cross_match_row_too_short() {
        let row: Vec<serde_json::Value> = (0..14).map(|i| serde_json::json!(i)).collect();
        assert!(parse_single_cross_match_row(&row).is_none());
    }

    #[test]
    fn test_parse_single_cross_match_row_empty() {
        assert!(parse_single_cross_match_row(&[]).is_none());
    }

    #[test]
    fn test_parse_single_cross_match_row_with_oi() {
        let row = vec![
            serde_json::json!(49081),
            serde_json::json!("NSE_FNO"),
            serde_json::json!("2026-03-18T09:30:00Z"),
            serde_json::json!(23000.0),
            serde_json::json!(23050.0),
            serde_json::json!(22990.0),
            serde_json::json!(23040.0),
            serde_json::json!(500000),
            serde_json::json!(23000.0),
            serde_json::json!(23050.0),
            serde_json::json!(22990.0),
            serde_json::json!(23040.0),
            serde_json::json!(500000),
            serde_json::json!(10000), // h_oi
            serde_json::json!(12000), // m_oi
        ];
        let parsed = parse_single_cross_match_row(&row).unwrap();
        assert_eq!(parsed.hist.oi, 10000);
        assert_eq!(parsed.live.oi, 12000);
    }

    #[test]
    fn test_parse_single_cross_match_row_null_defaults() {
        let row: Vec<serde_json::Value> = vec![serde_json::Value::Null; 15];
        let parsed = parse_single_cross_match_row(&row).unwrap();
        assert_eq!(parsed.security_id, 0);
        assert_eq!(parsed.segment, "");
        assert!(parsed.live_is_null);
        assert_eq!(parsed.hist.open, 0.0);
        assert_eq!(parsed.hist.volume, 0);
    }

    // -----------------------------------------------------------------------
    // classify_parsed_cross_match_row tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_parsed_cross_match_row_no_mismatch() {
        let parsed = ParsedCrossMatchRow {
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:15:00Z"),
            hist: cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
            live_is_null: false,
            live: cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "1m");
        assert!(result.is_none());
    }

    #[test]
    fn test_classify_parsed_cross_match_row_missing_live() {
        let parsed = ParsedCrossMatchRow {
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:15:00Z"),
            hist: cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
            live_is_null: true,
            live: cv(0.0, 0.0, 0.0, 0.0, 0, 0),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "1m").unwrap();
        assert_eq!(result.mismatch_type, "missing_live");
        assert!(result.live_values.contains("MISSING"));
    }

    #[test]
    fn test_classify_parsed_cross_match_row_price_diff() {
        let parsed = ParsedCrossMatchRow {
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:15:00Z"),
            hist: cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
            live_is_null: false,
            live: cv(2450.0, 2465.0, 2448.0, 2455.0, 125000, 0),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "1m").unwrap();
        assert_eq!(result.mismatch_type, "price_diff");
        assert!(result.diff_summary.contains('C'));
    }

    #[test]
    fn test_classify_parsed_cross_match_row_volume_diff() {
        let parsed = ParsedCrossMatchRow {
            security_id: 42,
            segment: "NSE_EQ".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T11:00:00Z"),
            hist: cv(3500.0, 3510.0, 3495.0, 3505.0, 100000, 0),
            live_is_null: false,
            live: cv(3500.0, 3510.0, 3495.0, 3505.0, 120000, 0),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "5m").unwrap();
        assert_eq!(result.mismatch_type, "volume_diff");
    }

    #[test]
    fn test_classify_parsed_cross_match_row_oi_diff() {
        let parsed = ParsedCrossMatchRow {
            security_id: 49081,
            segment: "NSE_FNO".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T09:30:00Z"),
            hist: cv(23000.0, 23050.0, 22990.0, 23040.0, 500000, 10000),
            live_is_null: false,
            live: cv(23000.0, 23050.0, 22990.0, 23040.0, 500000, 12000),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "1m").unwrap();
        assert_eq!(result.mismatch_type, "oi_diff");
    }

    #[test]
    fn test_classify_parsed_cross_match_row_exact_volume_diff() {
        // EXACT matching: any volume difference = mismatch.
        let parsed = ParsedCrossMatchRow {
            security_id: 42,
            segment: "NSE_EQ".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:00:00Z"),
            hist: cv(3500.0, 3510.0, 3495.0, 3505.0, 100000, 0),
            live_is_null: false,
            live: cv(3500.0, 3510.0, 3495.0, 3505.0, 105000, 0),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "1m");
        assert!(
            result.is_some(),
            "exact matching: volume diff IS a mismatch"
        );
    }

    #[test]
    fn test_classify_parsed_cross_match_row_preserves_timeframe() {
        let parsed = ParsedCrossMatchRow {
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:15:00Z"),
            hist: cv(2450.0, 2465.0, 2448.0, 2460.0, 125000, 0),
            live_is_null: true,
            live: cv(0.0, 0.0, 0.0, 0.0, 0, 0),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "15m").unwrap();
        assert_eq!(result.timeframe, "15m");
    }

    #[test]
    fn test_classify_parsed_cross_match_row_symbol_lookup() {
        let parsed = ParsedCrossMatchRow {
            security_id: 99999,
            segment: "NSE_EQ".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:15:00Z"),
            hist: cv(100.0, 110.0, 95.0, 105.0, 50000, 0),
            live_is_null: true,
            live: cv(0.0, 0.0, 0.0, 0.0, 0, 0),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "1m").unwrap();
        // Empty registry falls back to security_id string
        assert_eq!(result.symbol, "99999");
    }

    #[test]
    fn test_classify_parsed_cross_match_row_all_mismatches() {
        let parsed = ParsedCrossMatchRow {
            security_id: 1333,
            segment: "NSE_FNO".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T12:00:00Z"),
            hist: cv(2450.0, 2465.0, 2448.0, 2460.0, 100000, 10000),
            live_is_null: false,
            live: cv(2455.0, 2470.0, 2445.0, 2450.0, 120000, 12000),
        };
        let registry = InstrumentRegistry::empty();
        let result = classify_parsed_cross_match_row(&parsed, &registry, "15m").unwrap();
        assert_eq!(result.mismatch_type, "price_diff"); // price takes priority
        assert!(result.diff_summary.contains('O'));
        assert!(result.diff_summary.contains('V'));
        assert!(result.diff_summary.contains("OI("));
    }

    // -----------------------------------------------------------------------
    // parse_single_violation_row — additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_single_violation_row_all_null_values() {
        let row = vec![
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ];
        let parsed = parse_single_violation_row(&row).unwrap();
        assert_eq!(parsed.security_id, 0);
        assert_eq!(parsed.segment, "");
        assert_eq!(parsed.open, 0.0);
    }

    #[test]
    fn test_parse_single_violation_row_exactly_9_elements() {
        let row: Vec<serde_json::Value> = (0..9).map(|i| serde_json::json!(i)).collect();
        assert!(parse_single_violation_row(&row).is_some());
    }

    #[test]
    fn test_parse_single_violation_row_8_elements() {
        let row: Vec<serde_json::Value> = (0..8).map(|i| serde_json::json!(i)).collect();
        assert!(parse_single_violation_row(&row).is_none());
    }

    // -----------------------------------------------------------------------
    // parse_single_cross_match_row — additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_single_cross_match_row_null_live_data() {
        let row: Vec<serde_json::Value> = vec![
            serde_json::json!(1333),
            serde_json::json!("NSE_EQ"),
            serde_json::json!("2026-03-18T10:15:00Z"),
            serde_json::json!(100.0),
            serde_json::json!(110.0),
            serde_json::json!(95.0),
            serde_json::json!(105.0),
            serde_json::json!(50000),
            serde_json::Value::Null, // m_open is null
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(0),
        ];
        let parsed = parse_single_cross_match_row(&row).unwrap();
        assert!(parsed.live_is_null);
        assert_eq!(parsed.live.open, 0.0);
        assert_eq!(parsed.live.volume, 0);
    }

    #[test]
    fn test_parse_single_cross_match_row_14_elements_rejected() {
        let row: Vec<serde_json::Value> = (0..14).map(|i| serde_json::json!(i)).collect();
        assert!(parse_single_cross_match_row(&row).is_none());
    }

    #[test]
    fn test_parse_single_cross_match_row_15_elements_accepted() {
        let row: Vec<serde_json::Value> = (0..15).map(|i| serde_json::json!(i as f64)).collect();
        assert!(parse_single_cross_match_row(&row).is_some());
    }

    // -----------------------------------------------------------------------
    // violation_row_to_detail tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_violation_row_to_detail_uses_registry_fallback() {
        let parsed = ParsedViolationRow {
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            timeframe: "1m".to_string(),
            timestamp_value: serde_json::json!("2026-03-18T10:15:00Z"),
            open: 100.0,
            high: 90.0,
            low: 95.0,
            close: 92.0,
            volume: 1000,
        };
        let registry = InstrumentRegistry::empty();
        let detail = violation_row_to_detail(&parsed, &registry, "high < low");
        assert_eq!(detail.symbol, "1333"); // fallback to security_id string
        assert_eq!(detail.violation, "high < low");
        assert_eq!(detail.segment, "NSE_EQ");
        assert_eq!(detail.timeframe, "1m");
        assert!(detail.values.contains("H=90"));
    }

    // -----------------------------------------------------------------------
    // failed_report and failed_cross_match_report consistency
    // -----------------------------------------------------------------------

    #[test]
    fn test_failed_report_always_not_passed() {
        assert!(!failed_report().passed);
    }

    #[test]
    fn test_failed_cross_match_report_always_not_passed() {
        assert!(!failed_cross_match_report().passed);
    }
}
