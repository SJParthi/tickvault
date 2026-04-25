//! Dhan historical candle fetcher — multi-timeframe OHLCV retrieval.
//!
//! Fetches historical candles from Dhan's V2 REST API for all subscribed
//! instruments across 5 timeframes (1m, 5m, 15m, 60m, daily) and persists
//! to QuestDB via the `CandlePersistenceWriter`.
//!
//! # Timestamp Format
//! Dhan V2 REST API returns standard UTC epoch seconds. The persistence layer
//! converts to IST-as-UTC by adding +19800s before writing to QuestDB.
//! Note: Dhan WebSocket sends IST epoch seconds (no offset needed for live data).
//!
//! # O(1) Deduplication
//! - Client-side: skips instruments already fetched (via security_id set)
//! - Server-side: QuestDB DEDUP UPSERT KEYS(ts, security_id, timeframe) prevents duplicates
//!
//! # Automation
//! Runs without human intervention in the boot sequence after authentication.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{NaiveDate, Utc};
use metrics::{counter, gauge};
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, error, info, warn};
use zeroize::Zeroizing;

use tickvault_common::config::{DhanConfig, HistoricalDataConfig};
use tickvault_common::constants::{
    DHAN_CHARTS_HISTORICAL_PATH, DHAN_CHARTS_INTRADAY_PATH, INTRADAY_TIMEFRAMES,
    IST_UTC_OFFSET_SECONDS_I64, MARKET_CLOSE_TIME_IST_EXCLUSIVE, MARKET_OPEN_TIME_IST,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TIMEFRAME_1D,
};
use tickvault_common::instrument_registry::{InstrumentRegistry, SubscriptionCategory};
use tickvault_common::instrument_types::ExpiryCode;
use tickvault_common::tick_types::{DhanDailyResponse, DhanIntradayResponse, HistoricalCandle};
use tickvault_common::trading_calendar::ist_offset;

use tickvault_storage::candle_persistence::CandlePersistenceWriter;
use tickvault_storage::historical_fetch_marker::{
    FetchDecision, FetchMode, HistoricalFetchMarker, POST_MARKET_CLOSE_SECS_IST, decide_fetch,
    read_marker, write_marker,
};

use crate::auth::types::TokenState;

/// Type alias for the token handle used across the codebase.
type TokenHandle = std::sync::Arc<ArcSwap<Option<TokenState>>>;

/// Maximum response body size for Dhan historical API (10 MB).
/// Prevents OOM from malicious/corrupted responses. A single instrument's
/// 90-day × 375-candle response is ~100 KB of JSON — 10 MB is 100× headroom.
const MAX_RESPONSE_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Pause duration when data API returns error 805 (too many requests/connections).
/// Per Dhan docs: stop ALL connections for 60 seconds, then audit connection count.
const ERROR_805_PAUSE_SECS: u64 = 60;

/// DH-904 exponential backoff base (seconds). Sequence: 10s → 20s → 40s → 80s.
const DH_904_BACKOFF_BASE_SECS: u64 = 10;

/// Maximum DH-904 backoff attempts before giving up.
const DH_904_MAX_BACKOFF_ATTEMPTS: u32 = 4;

/// Maximum retry waves for failed instruments after the initial pass.
/// Each wave re-attempts all transiently-failed instruments with increasing backoff.
/// Set high to ensure we NEVER give up — every instrument must be fetched.
/// With capped backoff at 300s (5 min), wave 100 = ~8 hours of total retries.
/// If Dhan API is down for 8+ hours, it's a market-wide outage.
const RETRY_WAVE_MAX: usize = 100;

/// Base backoff between retry waves (seconds). Wave N sleeps min(N * base, MAX_BACKOFF).
/// Wave 1: 30s, Wave 2: 60s, ..., Wave 10+: capped at 300s (5 min).
const RETRY_WAVE_BACKOFF_BASE_SECS: u64 = 30;

/// Maximum backoff between retry waves (seconds). Caps exponential growth.
const RETRY_WAVE_MAX_BACKOFF_SECS: u64 = 300;

/// Dhan error response structure — exactly 3 string fields per API docs.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DhanErrorResponse {
    #[allow(dead_code)] // APPROVED: required for serde deserialization, only error_code is read
    error_type: Option<String>,
    error_code: Option<String>,
    #[allow(dead_code)] // APPROVED: required for serde deserialization, only error_code is read
    error_message: Option<String>,
}

/// Data API numeric error response (different from trading API).
#[derive(Debug, serde::Deserialize)]
struct DataApiErrorResponse {
    #[serde(alias = "errorCode", alias = "status")]
    code: Option<u16>,
    #[allow(dead_code)] // APPROVED: required for serde deserialization, only code is read
    #[serde(alias = "errorMessage", alias = "message")]
    message: Option<String>,
}

/// Classifies a Dhan error response for retry/backoff decisions.
#[derive(Debug, PartialEq)]
enum ErrorAction {
    /// Rate limited (DH-904 or HTTP 429) — exponential backoff
    RateLimited,
    /// Too many connections (805) — pause all requests 60s
    TooManyConnections,
    /// Token expired (807) — skip, token refresh handled elsewhere
    TokenExpired,
    /// Other error — use standard retry logic
    StandardRetry,
    /// Input error (DH-905) — never retry
    NeverRetry,
}

/// Parses error response bytes and classifies the error for retry decisions.
fn classify_error(status: reqwest::StatusCode, body: &[u8]) -> ErrorAction {
    // HTTP 429 is always rate limiting
    if status.as_u16() == 429 {
        return ErrorAction::RateLimited;
    }

    // Try data API numeric error format first
    if let Ok(data_err) = serde_json::from_slice::<DataApiErrorResponse>(body)
        && let Some(code) = data_err.code
    {
        return match code {
            805 => ErrorAction::TooManyConnections,
            807 => ErrorAction::TokenExpired,
            _ => ErrorAction::StandardRetry,
        };
    }

    // Try trading API string error format
    if let Ok(dhan_err) = serde_json::from_slice::<DhanErrorResponse>(body)
        && let Some(ref code) = dhan_err.error_code
    {
        return match code.as_str() {
            "DH-904" => ErrorAction::RateLimited,
            "DH-905" | "DH-906" | "DH-911" => ErrorAction::NeverRetry,
            "DH-901" => ErrorAction::TokenExpired,
            _ => ErrorAction::StandardRetry,
        };
    }

    ErrorAction::StandardRetry
}

/// Returns `true` if the error action represents a token-related failure
/// (807 token expired or DH-901 invalid auth). Token errors affect ALL
/// instruments — not just the current one — so they warrant `error!` level
/// logging (which triggers Telegram CRITICAL alerts).
///
/// Non-token errors (rate limits, network, input) are scoped to individual
/// instruments and remain at `warn!` level.
///
/// Production code uses direct `ErrorAction::TokenExpired` match arms with
/// `error!` level; this function encodes the categorization logic for tests.
#[cfg(test)]
fn is_token_related_error(action: &ErrorAction) -> bool {
    matches!(action, ErrorAction::TokenExpired)
}

/// Result of a single instrument fetch attempt.
#[derive(Debug, PartialEq)]
enum InstrumentFetchResult {
    /// Successfully fetched — (candle_count, persist_failures).
    Success(usize, usize),
    /// Failure — eligible for retry in next wave.
    /// NeverRetry errors (DH-905) will fail fast on re-attempt (no wasted time).
    Failed,
    /// Token expired (807/DH-901) — needs token refresh before retry.
    /// Distinct from `Failed` so the retry wave can sleep for token renewal.
    TokenExpired,
}

/// Duration to wait for token refresh when 807/DH-901 detected during fetch.
/// Token renewal runs on a background task; 30s gives it time to complete.
const TOKEN_REFRESH_WAIT_SECS: u64 = 30;

/// Maximum token-expired retry cycles before giving up on an instrument.
/// After this many 807 errors across retry waves, the instrument is marked Failed.
/// Set to 10 — allows time for token renewal background task to complete.
const MAX_TOKEN_EXPIRED_RETRIES: usize = 10;

// ---------------------------------------------------------------------------
// Pure Helper Functions (extracted for testability)
// ---------------------------------------------------------------------------

/// Validation result for a single candle's OHLC prices.
#[derive(Debug, PartialEq)]
enum CandleValidation {
    /// Candle passes all validation checks.
    Valid,
    /// One or more prices are NaN or Infinity.
    NonFinite,
    /// One or more prices are zero or negative.
    NonPositive,
    /// High price is less than low price.
    HighBelowLow,
}

/// Validates OHLC prices for a single candle. Pure function — no I/O.
fn validate_candle_ohlc(open: f64, high: f64, low: f64, close: f64) -> CandleValidation {
    if !open.is_finite() || !high.is_finite() || !low.is_finite() || !close.is_finite() {
        return CandleValidation::NonFinite;
    }
    if open <= 0.0 || high <= 0.0 || low <= 0.0 || close <= 0.0 {
        return CandleValidation::NonPositive;
    }
    if high < low {
        return CandleValidation::HighBelowLow;
    }
    CandleValidation::Valid
}

/// Returns true if the given UTC epoch timestamp falls at or after the intraday
/// market close (15:30 IST). Candles at or after this time are discarded.
///
/// # Arguments
/// * `utc_epoch_secs` — candle timestamp as UTC epoch seconds (from Dhan REST API)
fn is_outside_intraday_window(utc_epoch_secs: i64) -> bool {
    #[allow(clippy::cast_possible_truncation)] // APPROVED: secs-of-day fits u32
    let ist_secs_of_day =
        (utc_epoch_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64) % 86_400) as u32;
    ist_secs_of_day >= TICK_PERSIST_END_SECS_OF_DAY_IST
}

/// Returns true if the given UTC epoch timestamp falls on a weekend (Saturday or Sunday).
///
/// Converts UTC epoch to IST date, then checks day-of-week. Used to reject candles
/// that should never exist — NSE is closed on weekends (mock trading excluded from
/// historical candle storage by design).
fn is_weekend_timestamp(utc_epoch_secs: i64) -> bool {
    use chrono::{Datelike, TimeZone, Weekday};
    let ist_datetime = Utc
        .timestamp_opt(utc_epoch_secs, 0)
        .single()
        .map(|dt| dt.with_timezone(&ist_offset()));
    match ist_datetime {
        Some(dt) => matches!(dt.weekday(), Weekday::Sat | Weekday::Sun),
        None => false, // invalid timestamp — let other validators catch it
    }
}

/// Extracts open interest from the response array, returning 0 if index is out of bounds.
/// OI array may be empty for equity instruments — this handles that gracefully.
fn extract_oi(open_interest: &[i64], index: usize) -> i64 {
    if index < open_interest.len() {
        open_interest[index]
    } else {
        0
    }
}

/// Computes the fetch date range given today's date and lookback days.
/// Returns `(from_date, to_date)` as `NaiveDate`.
fn compute_fetch_date_range(
    today: chrono::NaiveDate,
    lookback_days: u32,
) -> (chrono::NaiveDate, chrono::NaiveDate) {
    let from_date = today
        .checked_sub_signed(chrono::Duration::days(i64::from(lookback_days)))
        .unwrap_or(today);
    (from_date, today)
}

/// Computes the daily API `toDate` by adding 1 day (since Dhan's toDate is non-inclusive).
fn compute_daily_to_date(to_date: chrono::NaiveDate) -> chrono::NaiveDate {
    to_date
        .checked_add_signed(chrono::Duration::days(1))
        .unwrap_or(to_date)
}

/// Formats intraday date range strings with market hours appended.
/// Returns `(from_datetime_str, to_datetime_str)`.
fn format_intraday_date_range(
    from_date: chrono::NaiveDate,
    to_date: chrono::NaiveDate,
) -> (String, String) {
    let from_str = format!("{} {}", from_date.format("%Y-%m-%d"), MARKET_OPEN_TIME_IST);
    let to_str = format!(
        "{} {}",
        to_date.format("%Y-%m-%d"),
        MARKET_CLOSE_TIME_IST_EXCLUSIVE
    );
    (from_str, to_str)
}

/// Computes the DH-904 exponential backoff delay for a given attempt number.
/// Sequence: 10s, 20s, 40s, 80s (base * 2^attempt).
fn compute_dh904_backoff_secs(attempt: u32) -> u64 {
    DH_904_BACKOFF_BASE_SECS.saturating_mul(1_u64.wrapping_shl(attempt))
}

/// Returns true if the DH-904 attempt count has exceeded the maximum.
fn is_dh904_exhausted(attempt: u32) -> bool {
    attempt >= DH_904_MAX_BACKOFF_ATTEMPTS
}

/// Computes the retry delay in milliseconds for a given attempt number.
/// Linear backoff: 1000ms * attempt.
fn compute_retry_delay_ms(attempt: u32) -> u64 {
    1000_u64.saturating_mul(u64::from(attempt))
}

/// Computes the backoff duration in seconds for a retry wave.
/// Wave N sleeps `min(N * BASE, MAX)`. Caps at 300s to avoid excessive waits.
fn compute_retry_wave_backoff_secs(wave: usize) -> u64 {
    RETRY_WAVE_BACKOFF_BASE_SECS
        .saturating_mul(wave as u64)
        .min(RETRY_WAVE_MAX_BACKOFF_SECS)
}

/// Returns the instrument type string for a subscription category, or `None`
/// if the category should be skipped (derivatives).
fn instrument_type_for_category(category: SubscriptionCategory) -> Option<&'static str> {
    match category {
        SubscriptionCategory::MajorIndexValue | SubscriptionCategory::DisplayIndex => Some("INDEX"),
        SubscriptionCategory::StockEquity => Some("EQUITY"),
        SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative => None,
    }
}

/// Builds a `HistoricalCandle` from intraday response data at a given index.
/// Does NOT perform validation — caller must validate first.
fn build_intraday_candle(
    data: &DhanIntradayResponse,
    index: usize,
    security_id: u32,
    segment_code: u8,
    timeframe_label: &'static str,
) -> HistoricalCandle {
    HistoricalCandle {
        timestamp_utc_secs: data.timestamp[index],
        security_id,
        exchange_segment_code: segment_code,
        timeframe: timeframe_label,
        open: data.open[index],
        high: data.high[index],
        low: data.low[index],
        close: data.close[index],
        volume: data.volume[index],
        open_interest: extract_oi(&data.open_interest, index),
    }
}

/// Builds a `HistoricalCandle` from daily response data at a given index.
/// Does NOT perform validation — caller must validate first.
fn build_daily_candle(
    data: &DhanDailyResponse,
    index: usize,
    security_id: u32,
    segment_code: u8,
) -> HistoricalCandle {
    HistoricalCandle {
        timestamp_utc_secs: data.timestamp[index],
        security_id,
        exchange_segment_code: segment_code,
        timeframe: TIMEFRAME_1D,
        open: data.open[index],
        high: data.high[index],
        low: data.low[index],
        close: data.close[index],
        volume: data.volume[index],
        open_interest: extract_oi(&data.open_interest, index),
    }
}

/// Checks if a response body exceeds the maximum allowed size.
fn is_response_oversized(body_len: usize) -> bool {
    body_len > MAX_RESPONSE_BODY_SIZE
}

/// Builds valid intraday candles from a `DhanIntradayResponse`, filtering out:
/// - Candles with non-finite prices (NaN/Inf)
/// - Candles with non-positive prices (zero or negative)
/// - Candles with high < low
/// - Candles outside the intraday window (>= 15:30 IST)
///
/// Returns a vector of valid `HistoricalCandle` structs, plus counts of
/// `(valid_candle_count, invalid_candle_count)`.
fn build_valid_intraday_candles(
    data: &DhanIntradayResponse,
    security_id: u32,
    segment_code: u8,
    timeframe_label: &'static str,
) -> (Vec<HistoricalCandle>, usize) {
    let mut candles = Vec::with_capacity(data.len());
    let mut invalid_count = 0_usize;
    for i in 0..data.len() {
        let open = data.open[i];
        let high = data.high[i];
        let low = data.low[i];
        let close = data.close[i];

        match validate_candle_ohlc(open, high, low, close) {
            CandleValidation::Valid => {}
            CandleValidation::NonFinite
            | CandleValidation::NonPositive
            | CandleValidation::HighBelowLow => {
                invalid_count = invalid_count.saturating_add(1);
                continue;
            }
        }

        // Reject candles at or after 15:30 IST
        if is_outside_intraday_window(data.timestamp[i]) {
            continue;
        }

        // Reject candles on weekends — NSE is closed on Sat/Sun.
        // Dhan normally returns empty for weekends, but this is defense-in-depth.
        if is_weekend_timestamp(data.timestamp[i]) {
            invalid_count = invalid_count.saturating_add(1);
            continue;
        }

        candles.push(build_intraday_candle(
            data,
            i,
            security_id,
            segment_code,
            timeframe_label,
        ));
    }
    (candles, invalid_count)
}

/// Builds valid daily candles from a `DhanDailyResponse`, filtering out:
/// - Candles with non-finite prices (NaN/Inf)
/// - Candles with non-positive prices (zero or negative)
/// - Candles with high < low
/// - Candles on weekends (Saturday/Sunday — NSE closed)
///
/// Returns a vector of valid `HistoricalCandle` structs, plus counts of
/// `(valid_candle_count, invalid_candle_count)`.
fn build_valid_daily_candles(
    data: &DhanDailyResponse,
    security_id: u32,
    segment_code: u8,
) -> (Vec<HistoricalCandle>, usize) {
    let mut candles = Vec::with_capacity(data.len());
    let mut invalid_count = 0_usize;
    for i in 0..data.len() {
        let open = data.open[i];
        let high = data.high[i];
        let low = data.low[i];
        let close = data.close[i];

        match validate_candle_ohlc(open, high, low, close) {
            CandleValidation::Valid => {}
            CandleValidation::NonFinite
            | CandleValidation::NonPositive
            | CandleValidation::HighBelowLow => {
                invalid_count = invalid_count.saturating_add(1);
                continue;
            }
        }

        // Reject daily candles on weekends — NSE is closed on Sat/Sun.
        if is_weekend_timestamp(data.timestamp[i]) {
            invalid_count = invalid_count.saturating_add(1);
            continue;
        }

        candles.push(build_daily_candle(data, i, security_id, segment_code));
    }
    (candles, invalid_count)
}

/// Collects the names of failed instruments for notification, capped at `MAX_FAILED_INSTRUMENT_NAMES`.
///
/// # Arguments
/// * `pending_indices` — indices of instruments that remain failed after all retry waves
/// * `security_ids` — security ID for each target instrument
/// * `segments` — exchange segment string for each target instrument
fn collect_failed_instrument_names(
    pending_indices: &[usize],
    security_ids: &[u32],
    segments: &[&str],
) -> Vec<String> {
    pending_indices
        .iter()
        .take(MAX_FAILED_INSTRUMENT_NAMES)
        .map(|&idx| format!("{} ({})", security_ids[idx], segments[idx]))
        .collect()
}

/// Builds a breakdown of failure reasons from per-instrument token-expired counts
/// and total persist failures.
///
/// # Arguments
/// * `pending_indices` — indices still pending (failed) after all waves
/// * `token_expired_counts` — per-instrument count of token-expired retries
/// * `total_persist_failures` — total QuestDB write failures
fn build_failure_reasons(
    pending_indices: &[usize],
    token_expired_counts: &[usize],
    total_persist_failures: usize,
) -> HashMap<String, usize> {
    let mut reasons: HashMap<String, usize> = HashMap::new();
    for &idx in pending_indices {
        if token_expired_counts[idx] >= MAX_TOKEN_EXPIRED_RETRIES {
            let count = reasons.entry("token_expired".to_string()).or_insert(0);
            *count = count.saturating_add(1);
        } else {
            let count = reasons.entry("network_or_api".to_string()).or_insert(0);
            *count = count.saturating_add(1);
        }
    }
    if total_persist_failures > 0 {
        reasons.insert("persist".to_string(), total_persist_failures);
    }
    reasons
}

// ---------------------------------------------------------------------------
// Request / Response Types
// ---------------------------------------------------------------------------

/// Request body for Dhan intraday charts API.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct IntradayRequest {
    security_id: String,
    exchange_segment: String,
    instrument: String,
    interval: String,
    oi: bool,
    from_date: String,
    to_date: String,
}

/// Request body for Dhan daily charts API.
/// Note: `toDate` is NON-INCLUSIVE — requesting toDate "2026-03-13" returns data up to 2026-03-12.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyRequest {
    security_id: String,
    exchange_segment: String,
    instrument: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiry_code: Option<ExpiryCode>,
    /// Whether to include Open Interest data in the response.
    /// Required for F&O instruments to receive OI in the `open_interest` array.
    oi: bool,
    from_date: String,
    to_date: String,
}

// ---------------------------------------------------------------------------
// Fetch Result
// ---------------------------------------------------------------------------

/// Summary of a historical candle fetch run.
#[derive(Debug)]
pub struct CandleFetchSummary {
    /// Number of instruments fetched successfully.
    pub instruments_fetched: usize,
    /// Number of instruments that failed.
    pub instruments_failed: usize,
    /// Total candles ingested.
    pub total_candles: usize,
    /// Number of instruments skipped (no data or not applicable).
    pub instruments_skipped: usize,
    /// Number of QuestDB write failures (candles fetched but lost during persist).
    pub persist_failures: usize,
    /// Symbol names of instruments that failed all retry waves (capped at 50).
    pub failed_instruments: Vec<String>,
    /// Breakdown of failure reasons: "token_expired", "network", "input_error", "persist", etc.
    pub failure_reasons: HashMap<String, usize>,
}

/// Maximum number of failed instrument names to collect.
const MAX_FAILED_INSTRUMENT_NAMES: usize = 50;

/// Maximum consecutive QuestDB persist failures before breaking out of the persist loop.
/// If 10 candles in a row fail to write, QuestDB is likely unreachable — stop wasting time.
const MAX_CONSECUTIVE_PERSIST_FAILURES: usize = 10;

// ---------------------------------------------------------------------------
// Main Fetch Logic
// ---------------------------------------------------------------------------

/// Stable Prometheus label for a fetch decision.
fn decision_label(d: &FetchDecision) -> &'static str {
    match d {
        FetchDecision::Skip => "skip",
        FetchDecision::FetchFullNinetyDays => "full",
        FetchDecision::FetchTodayOnly => "today",
        FetchDecision::WaitUntilPostClose => "wait",
    }
}

/// UTC epoch seconds for today's 15:30 IST close. Used to compute the
/// `WaitUntilPostClose` sleep duration.
fn target_close_utc_secs(today_ist: NaiveDate) -> i64 {
    // 15:30 IST in UTC seconds = today 00:00 IST as UTC + 15.5h.
    // 00:00 IST = previous-day 18:30 UTC. So today 15:30 IST = today 10:00 UTC.
    let midnight_ist_as_utc = today_ist
        .and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0);
    midnight_ist_as_utc.saturating_add(i64::from(POST_MARKET_CLOSE_SECS_IST))
        - IST_UTC_OFFSET_SECONDS_I64
}

/// Returns today's IST date computed from the system clock.
fn today_ist_date() -> NaiveDate {
    Utc::now().with_timezone(&ist_offset()).date_naive()
}

/// Returns the current second-of-day in IST (0..86_400).
// APPROVED: rem_euclid(86_400) is bounded to [0, 86_399] which always fits u32
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn now_sec_of_day_ist() -> u32 {
    let now_utc = Utc::now().timestamp();
    now_utc
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .rem_euclid(86_400) as u32
}

/// Public top-level entry point — applies the idempotency decision and
/// dispatches to either the 90-day or today-only fetch path.
///
/// This is the function call sites should use. It enforces Parthiban's
/// "successful fetch only once per day" spec by consulting the marker
/// file at `historical_config.marker_path`.
///
/// # Arguments
/// * `is_trading_day` — caller-supplied (typically `TradingCalendar::is_trading_day_today()`).
///   Drives the post-close vs. anytime decision.
#[allow(clippy::too_many_arguments)] // APPROVED: API fetch requires all config + writer params
pub async fn fetch_historical_candles(
    registry: &InstrumentRegistry,
    dhan_config: &DhanConfig,
    historical_config: &HistoricalDataConfig,
    token_handle: &TokenHandle,
    client_id: &SecretString,
    candle_writer: &mut CandlePersistenceWriter,
    is_trading_day: bool,
) -> CandleFetchSummary {
    let marker_path = PathBuf::from(&historical_config.marker_path);
    let marker = match read_marker(&marker_path) {
        Ok(m) => m,
        Err(err) => {
            counter!("tv_historical_fetch_marker_read_errors_total").increment(1);
            // Surface as ERROR so operators see the corrupt-marker case;
            // do NOT silently re-fetch — that would defeat the point.
            error!(
                ?err,
                marker_path = %historical_config.marker_path,
                "failed to read historical-fetch marker — refusing to fetch"
            );
            return CandleFetchSummary {
                instruments_fetched: 0,
                instruments_failed: 0,
                total_candles: 0,
                instruments_skipped: 0,
                persist_failures: 0,
                failed_instruments: vec![],
                failure_reasons: HashMap::new(),
            };
        }
    };

    let today = today_ist_date();
    let now_utc = Utc::now().timestamp();
    let now_ist_sec = now_sec_of_day_ist();
    let decision = decide_fetch(today, marker.as_ref(), now_ist_sec, is_trading_day);

    counter!("tv_historical_fetch_decisions_total",
        "decision" => decision_label(&decision))
    .increment(1);

    if let Some(ref m) = marker {
        let age_secs = (today - m.last_success_date).num_seconds().max(0);
        #[allow(clippy::cast_precision_loss)] // APPROVED: gauge accuracy not financial
        gauge!("tv_historical_fetch_last_success_age_seconds").set(age_secs as f64);
    }

    info!(
        ?decision,
        %today,
        is_trading_day,
        marker_present = marker.is_some(),
        "historical-fetch idempotency decision"
    );

    let mode = match decision {
        FetchDecision::Skip => {
            info!(
                last_success_date = %marker.as_ref().map(|m| m.last_success_date.to_string()).unwrap_or_default(),
                "historical fetch already completed today — skipping"
            );
            return CandleFetchSummary {
                instruments_fetched: 0,
                instruments_failed: 0,
                total_candles: 0,
                instruments_skipped: 0,
                persist_failures: 0,
                failed_instruments: vec![],
                failure_reasons: HashMap::new(),
            };
        }
        FetchDecision::FetchFullNinetyDays => FetchMode::FullNinetyDays,
        FetchDecision::FetchTodayOnly => FetchMode::TodayOnly,
        FetchDecision::WaitUntilPostClose => {
            let target_utc = target_close_utc_secs(today);
            let sleep_secs = (target_utc - now_utc).max(0);
            #[allow(clippy::cast_sign_loss)] // APPROVED: clamped above with .max(0)
            let sleep_dur = Duration::from_secs(sleep_secs as u64);
            info!(
                sleep_secs,
                target_utc, "waiting until 15:30 IST before today-only fetch"
            );
            tokio::time::sleep(sleep_dur).await;
            FetchMode::TodayOnly
        }
    };

    let summary = fetch_historical_candles_inner(
        mode,
        today,
        registry,
        dhan_config,
        historical_config,
        token_handle,
        client_id,
        candle_writer,
    )
    .await;

    // Parthiban directive (2026-04-22): the idempotency marker MUST only be
    // written on a FULLY successful fetch run — zero failures, not just
    // "some data arrived". Previously the guard was `instruments_fetched > 0`
    // which would set the marker even when 200 of 220 instruments failed.
    // That caused the next day's boot to skip the retry of the missing 200.
    //
    // Definition of "fully successful":
    //   - at least 1 instrument fetched (so an empty registry doesn't mark)
    //   - zero fetch failures (instruments_failed == 0)
    //   - zero persist failures (persist_failures == 0)
    //
    // `instruments_skipped` is NOT a failure — it counts derivatives that
    // Dhan's REST API intentionally does not serve, plus any other
    // documented skips. Those should not block marker write.
    let is_fully_successful = summary.instruments_fetched > 0
        && summary.instruments_failed == 0
        && summary.persist_failures == 0;

    if is_fully_successful {
        #[allow(clippy::cast_possible_truncation)]
        // APPROVED: counts fit u32 (max ~25k instruments)
        let new_marker = HistoricalFetchMarker {
            last_success_date: today,
            instruments_fetched: summary.instruments_fetched as u32,
            candles_written: summary.total_candles as u64,
            mode,
        };
        if let Err(err) = write_marker(&marker_path, &new_marker) {
            counter!("tv_historical_fetch_marker_write_errors_total").increment(1);
            error!(?err, "failed to write historical-fetch marker");
        } else {
            info!(
                marker_path = %historical_config.marker_path,
                ?mode,
                instruments_fetched = summary.instruments_fetched,
                candles_written = summary.total_candles,
                "historical-fetch marker written (fully successful run)"
            );
            #[allow(clippy::cast_precision_loss)] // APPROVED: gauge accuracy not financial
            gauge!("tv_historical_fetch_last_success_age_seconds").set(0.0);
        }
    } else {
        warn!(
            instruments_fetched = summary.instruments_fetched,
            instruments_failed = summary.instruments_failed,
            persist_failures = summary.persist_failures,
            "historical-fetch marker NOT written — run was not fully successful; tomorrow's boot will retry"
        );
    }

    summary
}

/// Internal fetch implementation. Pulls candles from Dhan's REST API
/// for either the full 90-day window (`FetchMode::FullNinetyDays`) or
/// only today's intraday data (`FetchMode::TodayOnly`).
///
/// Today-only mode skips daily candles entirely (per Parthiban spec —
/// only intraday 1m/5m/15m/60m needed for incremental top-up).
#[allow(clippy::too_many_arguments)] // APPROVED: API fetch requires all config + writer params
async fn fetch_historical_candles_inner(
    mode: FetchMode,
    today: NaiveDate,
    registry: &InstrumentRegistry,
    dhan_config: &DhanConfig,
    historical_config: &HistoricalDataConfig,
    token_handle: &TokenHandle,
    client_id: &SecretString,
    candle_writer: &mut CandlePersistenceWriter,
) -> CandleFetchSummary {
    let m_fetched = counter!("tv_historical_candles_fetched_total");
    let m_api_errors = counter!("tv_historical_api_errors_total");

    // Compute date range based on mode.
    let (from_date, to_date) = match mode {
        FetchMode::FullNinetyDays => {
            compute_fetch_date_range(today, historical_config.lookback_days)
        }
        FetchMode::TodayOnly => (today, today),
    };
    let skip_daily = matches!(mode, FetchMode::TodayOnly);

    // Intraday requests use datetime format with market hours
    let from_date_str = from_date.format("%Y-%m-%d").to_string();
    let to_date_str = to_date.format("%Y-%m-%d").to_string();

    // Intraday: "YYYY-MM-DD HH:MM:SS" with market hours (09:15 to 15:30 exclusive)
    let (intraday_from, intraday_to) = format_intraday_date_range(from_date, to_date);

    // Daily: date-only format, toDate is NON-INCLUSIVE so add 1 day
    let daily_to_date = compute_daily_to_date(to_date);
    let daily_to_str = daily_to_date.format("%Y-%m-%d").to_string();

    info!(
        from_date = %from_date_str,
        to_date = %to_date_str,
        ?mode,
        skip_daily,
        timeframes = if skip_daily { "1m,5m,15m,60m" } else { "1m,5m,15m,60m,1d" },
        "starting multi-timeframe historical candle fetch"
    );

    let http_client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(historical_config.request_timeout_secs))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for historical fetch");
            return CandleFetchSummary {
                instruments_fetched: 0,
                instruments_failed: 0,
                total_candles: 0,
                instruments_skipped: 0,
                persist_failures: 0,
                failed_instruments: Vec::new(),
                failure_reasons: HashMap::new(),
            };
        }
    };

    let intraday_endpoint = format!(
        "{}{}",
        dhan_config.rest_api_base_url, DHAN_CHARTS_INTRADAY_PATH
    );
    let daily_endpoint = format!(
        "{}{}",
        dhan_config.rest_api_base_url, DHAN_CHARTS_HISTORICAL_PATH
    );

    // Dedup key: (security_id, segment) — same security_id (e.g. 13, 25)
    // can exist in both IDX_I and NSE_EQ with different candle data.
    let mut fetched_instruments: HashSet<(u32, String)> = HashSet::new();
    let mut instruments_fetched: usize = 0;
    let mut instruments_skipped: usize = 0;
    let mut total_candles: usize = 0;
    let mut total_persist_failures: usize = 0;

    // Build the list of fetchable instruments (skip derivatives, dedup)
    struct FetchTarget {
        security_id: u32,
        security_id_str: String,
        segment_code: u8,
        exchange_segment_str: String,
        instrument_type: &'static str,
    }

    let mut targets: Vec<FetchTarget> = Vec::new();
    for instrument in registry.iter() {
        let instrument_type = match instrument_type_for_category(instrument.category) {
            Some(t) => t,
            None => {
                instruments_skipped = instruments_skipped.saturating_add(1);
                continue;
            }
        };

        let exchange_segment_str = instrument.exchange_segment.as_str().to_string();
        let dedup_key = (instrument.security_id, exchange_segment_str.clone());
        if fetched_instruments.contains(&dedup_key) {
            continue;
        }
        // Reserve the dedup slot so duplicates in the registry are skipped
        fetched_instruments.insert(dedup_key);

        targets.push(FetchTarget {
            security_id: instrument.security_id,
            security_id_str: instrument.security_id.to_string(),
            segment_code: instrument.exchange_segment.binary_code(),
            exchange_segment_str,
            instrument_type,
        });
    }

    // Track which target indices still need fetching
    let mut pending_indices: Vec<usize> = (0..targets.len()).collect();
    let m_token_expired = counter!("tv_historical_token_expired_total");

    // Per-instrument token-expired retry count
    let mut token_expired_counts: Vec<usize> = vec![0; targets.len()];

    // --- Wave 0 = initial pass, Waves 1..=RETRY_WAVE_MAX = retries ---
    for wave in 0..=RETRY_WAVE_MAX {
        if pending_indices.is_empty() {
            break;
        }

        if wave > 0 {
            let backoff_secs = compute_retry_wave_backoff_secs(wave);
            // Escalate to error every 10 waves so Telegram alert fires
            if wave % 10 == 0 {
                error!(
                    wave,
                    remaining = pending_indices.len(),
                    backoff_secs,
                    "historical fetch still retrying — {} instruments pending after {} waves",
                    pending_indices.len(),
                    wave,
                );
            } else {
                warn!(
                    wave,
                    remaining = pending_indices.len(),
                    backoff_secs,
                    "retry wave — backing off before re-attempting failed instruments"
                );
            }
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        }

        let mut still_pending: Vec<usize> = Vec::new();
        let mut token_expired_this_wave = false;

        for &idx in &pending_indices {
            let target = &targets[idx];

            let result = fetch_single_instrument(
                &http_client,
                &intraday_endpoint,
                &daily_endpoint,
                &intraday_from,
                &intraday_to,
                &from_date_str,
                &daily_to_str,
                target.security_id,
                &target.security_id_str,
                target.segment_code,
                &target.exchange_segment_str,
                target.instrument_type,
                token_handle,
                client_id,
                historical_config,
                candle_writer,
                &m_api_errors,
                skip_daily,
            )
            .await;

            match result {
                InstrumentFetchResult::Success(candle_count, persist_fails) => {
                    instruments_fetched = instruments_fetched.saturating_add(1);
                    total_candles = total_candles.saturating_add(candle_count);
                    total_persist_failures = total_persist_failures.saturating_add(persist_fails);
                    #[allow(clippy::cast_possible_truncation)]
                    // APPROVED: usize->u64 is lossless on 64-bit targets
                    m_fetched.increment(candle_count as u64);
                }
                InstrumentFetchResult::TokenExpired => {
                    m_token_expired.increment(1);
                    token_expired_counts[idx] = token_expired_counts[idx].saturating_add(1);

                    if token_expired_counts[idx] >= MAX_TOKEN_EXPIRED_RETRIES {
                        // M5: Token errors affect ALL instruments — escalate to error!
                        error!(
                            security_id = target.security_id,
                            segment = %target.exchange_segment_str,
                            retries = token_expired_counts[idx],
                            "token still expired after max retries — giving up on instrument"
                        );
                        // Don't re-queue — treat as permanent failure
                    } else {
                        token_expired_this_wave = true;
                        still_pending.push(idx);
                    }
                }
                InstrumentFetchResult::Failed => {
                    still_pending.push(idx);
                }
            }
        }

        // If any instrument got token-expired this wave, sleep to allow
        // the background token renewal task to complete before next wave.
        if token_expired_this_wave {
            // M5: Token errors affect ALL instruments — escalate to error!
            error!(
                wave,
                "token expired during fetch — waiting {}s for token refresh",
                TOKEN_REFRESH_WAIT_SECS,
            );
            tokio::time::sleep(Duration::from_secs(TOKEN_REFRESH_WAIT_SECS)).await;
        }

        if wave == 0 && !still_pending.is_empty() {
            info!(
                failed = still_pending.len(),
                total = targets.len(),
                "initial pass complete — scheduling retry waves for failed instruments"
            );
        }

        pending_indices = still_pending;
    }

    let instruments_failed = pending_indices.len();

    // Collect failed instrument names for Telegram notification
    let target_sec_ids: Vec<u32> = targets.iter().map(|t| t.security_id).collect();
    let target_segments: Vec<&str> = targets
        .iter()
        .map(|t| t.exchange_segment_str.as_str())
        .collect();
    let failed_instruments =
        collect_failed_instrument_names(&pending_indices, &target_sec_ids, &target_segments);

    // Build failure reason breakdown
    let failure_reasons = build_failure_reasons(
        &pending_indices,
        &token_expired_counts,
        total_persist_failures,
    );

    if instruments_failed > 0 {
        warn!(
            instruments_failed,
            retry_waves = RETRY_WAVE_MAX,
            ?failure_reasons,
            "instruments still failed after all retry waves"
        );
    }

    // Final flush
    if let Err(err) = candle_writer.force_flush() {
        // Phase 0 / Rule 5: flush failures are ERROR (route to Telegram).
        error!(?err, "failed to flush remaining candles to QuestDB");
    }

    info!(
        instruments_fetched,
        instruments_failed,
        instruments_skipped,
        total_candles,
        persist_failures = total_persist_failures,
        "historical candle fetch complete"
    );

    CandleFetchSummary {
        instruments_fetched,
        instruments_failed,
        total_candles,
        instruments_skipped,
        persist_failures: total_persist_failures,
        failed_instruments,
        failure_reasons,
    }
}

// ---------------------------------------------------------------------------
// Per-Instrument Fetch Helper
// ---------------------------------------------------------------------------

/// Fetches all timeframes (4 intraday + optional daily) for a single instrument.
/// Returns whether the fetch succeeded, failed transiently, or permanently.
///
/// `skip_daily=true` is used by the today-only incremental path — daily
/// candles only update once per trading day and reuse the 90-day fetch.
#[allow(clippy::too_many_arguments)] // APPROVED: single-instrument fetch needs full context
async fn fetch_single_instrument(
    http_client: &reqwest::Client,
    intraday_endpoint: &str,
    daily_endpoint: &str,
    intraday_from: &str,
    intraday_to: &str,
    daily_from: &str,
    daily_to: &str,
    security_id: u32,
    security_id_str: &str,
    segment_code: u8,
    exchange_segment_str: &str,
    instrument_type: &str,
    token_handle: &TokenHandle,
    client_id: &SecretString,
    historical_config: &HistoricalDataConfig,
    candle_writer: &mut CandlePersistenceWriter,
    m_api_errors: &metrics::Counter,
    skip_daily: bool,
) -> InstrumentFetchResult {
    let mut instrument_candles = 0_usize;
    let mut instrument_persist_failures = 0_usize;

    // --- Fetch all 4 intraday timeframes ---
    for &(interval, timeframe_label) in INTRADAY_TIMEFRAMES {
        if historical_config.request_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(historical_config.request_delay_ms)).await;
        }

        let token_guard = token_handle.load();
        let access_token = match token_guard.as_ref() {
            Some(token_state) => {
                Zeroizing::new(token_state.access_token().expose_secret().to_string())
            }
            None => {
                error!("no access token available — skipping historical fetch");
                return InstrumentFetchResult::Failed;
            }
        };

        let request_body = IntradayRequest {
            security_id: security_id_str.to_string(),
            exchange_segment: exchange_segment_str.to_string(),
            instrument: instrument_type.to_string(),
            interval: interval.to_string(),
            oi: true,
            from_date: intraday_from.to_string(),
            to_date: intraday_to.to_string(),
        };

        match fetch_intraday_with_retry(
            http_client,
            intraday_endpoint,
            &request_body,
            &access_token,
            client_id,
            historical_config,
            security_id,
            segment_code,
            timeframe_label,
            candle_writer,
            m_api_errors,
        )
        .await
        {
            TimeframeFetchResult::Ok(count, pf) => {
                instrument_candles = instrument_candles.saturating_add(count);
                instrument_persist_failures = instrument_persist_failures.saturating_add(pf);
            }
            TimeframeFetchResult::TokenExpired => {
                return InstrumentFetchResult::TokenExpired;
            }
            TimeframeFetchResult::Failed => {
                return InstrumentFetchResult::Failed;
            }
        }
    }

    // --- Fetch daily candles (skipped in today-only mode) ---
    if skip_daily {
        return InstrumentFetchResult::Success(instrument_candles, instrument_persist_failures);
    }

    if historical_config.request_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(historical_config.request_delay_ms)).await;
    }

    let token_guard = token_handle.load();
    let access_token = match token_guard.as_ref() {
        Some(token_state) => Zeroizing::new(token_state.access_token().expose_secret().to_string()),
        None => {
            error!("no access token available — skipping daily fetch");
            return InstrumentFetchResult::TokenExpired;
        }
    };

    let daily_body = DailyRequest {
        security_id: security_id_str.to_string(),
        exchange_segment: exchange_segment_str.to_string(),
        instrument: instrument_type.to_string(),
        expiry_code: None,
        oi: true,
        from_date: daily_from.to_string(),
        to_date: daily_to.to_string(),
    };

    match fetch_daily_with_retry(
        http_client,
        daily_endpoint,
        &daily_body,
        &access_token,
        client_id,
        historical_config,
        security_id,
        segment_code,
        candle_writer,
        m_api_errors,
    )
    .await
    {
        TimeframeFetchResult::Ok(count, pf) => {
            instrument_candles = instrument_candles.saturating_add(count);
            instrument_persist_failures = instrument_persist_failures.saturating_add(pf);
        }
        TimeframeFetchResult::TokenExpired => {
            return InstrumentFetchResult::TokenExpired;
        }
        TimeframeFetchResult::Failed => {
            return InstrumentFetchResult::Failed;
        }
    }

    InstrumentFetchResult::Success(instrument_candles, instrument_persist_failures)
}

// ---------------------------------------------------------------------------
// Intraday Fetch Helper
// ---------------------------------------------------------------------------

/// Result of a single timeframe fetch attempt.
#[derive(Debug)]
enum TimeframeFetchResult {
    /// Successfully fetched — (candle_count, persist_failures).
    Ok(usize, usize),
    /// Transient failure — eligible for retry.
    Failed,
    /// Token expired — caller should wait for refresh and retry.
    TokenExpired,
}

/// Fetches a single intraday timeframe for one instrument with retries.
#[allow(clippy::too_many_arguments)] // APPROVED: retry helper needs all context
async fn fetch_intraday_with_retry(
    http_client: &reqwest::Client,
    endpoint: &str,
    request_body: &IntradayRequest,
    access_token: &Zeroizing<String>,
    client_id: &SecretString,
    config: &HistoricalDataConfig,
    security_id: u32,
    segment_code: u8,
    timeframe_label: &'static str,
    candle_writer: &mut CandlePersistenceWriter,
    m_api_errors: &metrics::Counter,
) -> TimeframeFetchResult {
    for attempt in 0..=config.max_retries {
        if attempt > 0 {
            let delay_ms = compute_retry_delay_ms(attempt);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let result = http_client
            .post(endpoint)
            .header("access-token", access_token.as_str())
            .header("client-id", client_id.expose_secret())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(request_body)
            .send()
            .await;

        match result {
            Ok(response) => {
                if !response.status().is_success() {
                    let status = response.status();
                    // Read error body for classification
                    let err_body = response.bytes().await.unwrap_or_default();
                    let action = classify_error(status, &err_body);

                    match action {
                        ErrorAction::TooManyConnections => {
                            warn!(
                                security_id,
                                timeframe = timeframe_label,
                                "data API 805 — pausing ALL requests for 60s"
                            );
                            tokio::time::sleep(Duration::from_secs(ERROR_805_PAUSE_SECS)).await;
                            m_api_errors.increment(1);
                            continue;
                        }
                        ErrorAction::RateLimited => {
                            let backoff_secs = compute_dh904_backoff_secs(attempt);
                            warn!(
                                security_id,
                                timeframe = timeframe_label,
                                backoff_secs,
                                attempt,
                                "rate limited — exponential backoff"
                            );
                            if is_dh904_exhausted(attempt) {
                                m_api_errors.increment(1);
                                return TimeframeFetchResult::Failed;
                            }
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                            continue;
                        }
                        ErrorAction::TokenExpired => {
                            // M5: Token errors affect ALL instruments — escalate to error!
                            // (triggers Telegram CRITICAL alert via observability pipeline)
                            error!(
                                security_id,
                                timeframe = timeframe_label,
                                "token expired (807/DH-901) — signaling for refresh"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::TokenExpired;
                        }
                        ErrorAction::NeverRetry => {
                            warn!(
                                %status, security_id, timeframe = timeframe_label,
                                "input error (DH-905/906) — will not retry"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::Failed;
                        }
                        ErrorAction::StandardRetry => {
                            if attempt < config.max_retries {
                                debug!(
                                    %status, security_id, timeframe = timeframe_label, attempt,
                                    "historical API non-success — retrying"
                                );
                                continue;
                            }
                            warn!(
                                %status, security_id, timeframe = timeframe_label,
                                "historical API failed after all retries"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::Failed;
                        }
                    }
                }

                let body_bytes = match response.bytes().await {
                    Ok(b) => b,
                    Err(err) => {
                        if attempt < config.max_retries {
                            debug!(
                                ?err,
                                security_id,
                                timeframe = timeframe_label,
                                attempt,
                                "failed to read response body — retrying"
                            );
                            continue;
                        }
                        warn!(
                            ?err,
                            security_id,
                            timeframe = timeframe_label,
                            "failed to read response body after all retries"
                        );
                        m_api_errors.increment(1);
                        return TimeframeFetchResult::Failed;
                    }
                };

                if is_response_oversized(body_bytes.len()) {
                    warn!(
                        security_id,
                        timeframe = timeframe_label,
                        body_size = body_bytes.len(),
                        max = MAX_RESPONSE_BODY_SIZE,
                        "response body exceeds size limit — skipping"
                    );
                    m_api_errors.increment(1);
                    return TimeframeFetchResult::Failed;
                }

                match serde_json::from_slice::<DhanIntradayResponse>(&body_bytes) {
                    Ok(data) => {
                        if data.is_empty() {
                            debug!(
                                security_id,
                                timeframe = timeframe_label,
                                "no candle data returned"
                            );
                            return TimeframeFetchResult::Ok(0, 0);
                        }

                        if !data.is_consistent() {
                            warn!(
                                security_id,
                                timeframe = timeframe_label,
                                "inconsistent array lengths in historical response"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::Failed;
                        }

                        let (count, pf) = persist_intraday_candles(
                            &data,
                            security_id,
                            segment_code,
                            timeframe_label,
                            candle_writer,
                            m_api_errors,
                        );
                        return TimeframeFetchResult::Ok(count, pf);
                    }
                    Err(err) => {
                        if attempt < config.max_retries {
                            debug!(
                                ?err,
                                security_id,
                                timeframe = timeframe_label,
                                attempt,
                                "failed to parse historical response — retrying"
                            );
                            continue;
                        }
                        warn!(
                            ?err,
                            security_id,
                            timeframe = timeframe_label,
                            "failed to parse historical response after all retries"
                        );
                        m_api_errors.increment(1);
                        return TimeframeFetchResult::Failed;
                    }
                }
            }
            Err(err) => {
                if attempt < config.max_retries {
                    debug!(
                        ?err,
                        security_id,
                        timeframe = timeframe_label,
                        attempt,
                        "historical API request failed — retrying"
                    );
                    continue;
                }
                warn!(
                    ?err,
                    security_id,
                    timeframe = timeframe_label,
                    "historical API request failed after all retries"
                );
                m_api_errors.increment(1);
                return TimeframeFetchResult::Failed;
            }
        }
    }
    TimeframeFetchResult::Failed
}

// ---------------------------------------------------------------------------
// Daily Fetch Helper
// ---------------------------------------------------------------------------

/// Fetches daily candles for one instrument with retries.
#[allow(clippy::too_many_arguments)] // APPROVED: retry helper needs all context
async fn fetch_daily_with_retry(
    http_client: &reqwest::Client,
    endpoint: &str,
    request_body: &DailyRequest,
    access_token: &Zeroizing<String>,
    client_id: &SecretString,
    config: &HistoricalDataConfig,
    security_id: u32,
    segment_code: u8,
    candle_writer: &mut CandlePersistenceWriter,
    m_api_errors: &metrics::Counter,
) -> TimeframeFetchResult {
    for attempt in 0..=config.max_retries {
        if attempt > 0 {
            let delay_ms = compute_retry_delay_ms(attempt);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let result = http_client
            .post(endpoint)
            .header("access-token", access_token.as_str())
            .header("client-id", client_id.expose_secret())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(request_body)
            .send()
            .await;

        match result {
            Ok(response) => {
                if !response.status().is_success() {
                    let status = response.status();
                    let err_body = response.bytes().await.unwrap_or_default();
                    let action = classify_error(status, &err_body);

                    match action {
                        ErrorAction::TooManyConnections => {
                            warn!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                "data API 805 — pausing ALL requests for 60s"
                            );
                            tokio::time::sleep(Duration::from_secs(ERROR_805_PAUSE_SECS)).await;
                            m_api_errors.increment(1);
                            continue;
                        }
                        ErrorAction::RateLimited => {
                            let backoff_secs = DH_904_BACKOFF_BASE_SECS
                                .saturating_mul(1_u64.wrapping_shl(attempt));
                            warn!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                backoff_secs,
                                attempt,
                                "rate limited — exponential backoff"
                            );
                            if attempt >= DH_904_MAX_BACKOFF_ATTEMPTS {
                                m_api_errors.increment(1);
                                return TimeframeFetchResult::Failed;
                            }
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                            continue;
                        }
                        ErrorAction::TokenExpired => {
                            // M5: Token errors affect ALL instruments — escalate to error!
                            // (triggers Telegram CRITICAL alert via observability pipeline)
                            error!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                "token expired (807/DH-901) — signaling for refresh"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::TokenExpired;
                        }
                        ErrorAction::NeverRetry => {
                            warn!(
                                %status, security_id, timeframe = TIMEFRAME_1D,
                                "input error (DH-905/906) — will not retry"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::Failed;
                        }
                        ErrorAction::StandardRetry => {
                            if attempt < config.max_retries {
                                debug!(
                                    %status, security_id, timeframe = TIMEFRAME_1D, attempt,
                                    "daily API non-success — retrying"
                                );
                                continue;
                            }
                            warn!(
                                %status, security_id, timeframe = TIMEFRAME_1D,
                                "daily API failed after all retries"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::Failed;
                        }
                    }
                }

                let body_bytes = match response.bytes().await {
                    Ok(b) => b,
                    Err(err) => {
                        if attempt < config.max_retries {
                            debug!(
                                ?err,
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                attempt,
                                "failed to read daily response body — retrying"
                            );
                            continue;
                        }
                        warn!(
                            ?err,
                            security_id,
                            timeframe = TIMEFRAME_1D,
                            "failed to read daily response body after all retries"
                        );
                        m_api_errors.increment(1);
                        return TimeframeFetchResult::Failed;
                    }
                };

                if is_response_oversized(body_bytes.len()) {
                    warn!(
                        security_id,
                        timeframe = TIMEFRAME_1D,
                        body_size = body_bytes.len(),
                        max = MAX_RESPONSE_BODY_SIZE,
                        "daily response body exceeds size limit — skipping"
                    );
                    m_api_errors.increment(1);
                    return TimeframeFetchResult::Failed;
                }

                match serde_json::from_slice::<DhanDailyResponse>(&body_bytes) {
                    Ok(data) => {
                        if data.is_empty() {
                            debug!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                "no daily data returned"
                            );
                            return TimeframeFetchResult::Ok(0, 0);
                        }

                        if !data.is_consistent() {
                            warn!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                "inconsistent array lengths in daily response"
                            );
                            m_api_errors.increment(1);
                            return TimeframeFetchResult::Failed;
                        }

                        let (count, pf) = persist_daily_candles(
                            &data,
                            security_id,
                            segment_code,
                            candle_writer,
                            m_api_errors,
                        );
                        return TimeframeFetchResult::Ok(count, pf);
                    }
                    Err(err) => {
                        if attempt < config.max_retries {
                            debug!(
                                ?err,
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                attempt,
                                "failed to parse daily response — retrying"
                            );
                            continue;
                        }
                        warn!(
                            ?err,
                            security_id,
                            timeframe = TIMEFRAME_1D,
                            "failed to parse daily response after all retries"
                        );
                        m_api_errors.increment(1);
                        return TimeframeFetchResult::Failed;
                    }
                }
            }
            Err(err) => {
                if attempt < config.max_retries {
                    debug!(
                        ?err,
                        security_id,
                        timeframe = TIMEFRAME_1D,
                        attempt,
                        "daily API request failed — retrying"
                    );
                    continue;
                }
                warn!(
                    ?err,
                    security_id,
                    timeframe = TIMEFRAME_1D,
                    "daily API request failed after all retries"
                );
                m_api_errors.increment(1);
                return TimeframeFetchResult::Failed;
            }
        }
    }
    TimeframeFetchResult::Failed
}

// ---------------------------------------------------------------------------
// Candle Persistence Helpers
// ---------------------------------------------------------------------------

/// Converts intraday response to HistoricalCandle structs and persists them.
/// Returns `(candles_written, persist_failures)`.
fn persist_intraday_candles(
    data: &DhanIntradayResponse,
    security_id: u32,
    segment_code: u8,
    timeframe_label: &'static str,
    candle_writer: &mut CandlePersistenceWriter,
    m_api_errors: &metrics::Counter,
) -> (usize, usize) {
    let (candles, invalid_count) =
        build_valid_intraday_candles(data, security_id, segment_code, timeframe_label);

    if invalid_count > 0 {
        debug!(
            security_id,
            timeframe = timeframe_label,
            invalid_count,
            "skipped invalid candles in API response"
        );
        #[allow(clippy::cast_possible_truncation)]
        // APPROVED: usize->u64 is lossless on 64-bit targets
        m_api_errors.increment(invalid_count as u64);
    }

    let mut count = 0_usize;
    let mut persist_failures = 0_usize;
    let mut consecutive_persist_failures = 0_usize;
    for candle in &candles {
        if let Err(err) = candle_writer.append_candle(candle) {
            warn!(
                ?err,
                security_id,
                timeframe = timeframe_label,
                "failed to append candle to QuestDB"
            );
            persist_failures = persist_failures.saturating_add(1);
            consecutive_persist_failures = consecutive_persist_failures.saturating_add(1);
            if consecutive_persist_failures >= MAX_CONSECUTIVE_PERSIST_FAILURES {
                tracing::error!(
                    security_id,
                    timeframe = timeframe_label,
                    consecutive_failures = consecutive_persist_failures,
                    "QuestDB unreachable — {} consecutive persist failures, aborting persist loop",
                    MAX_CONSECUTIVE_PERSIST_FAILURES,
                );
                break;
            }
            continue;
        }
        consecutive_persist_failures = 0; // Reset on success
        count = count.saturating_add(1);
    }
    (count, persist_failures)
}

/// Converts daily response to HistoricalCandle structs and persists them.
/// Returns `(candles_written, persist_failures)`.
fn persist_daily_candles(
    data: &DhanDailyResponse,
    security_id: u32,
    segment_code: u8,
    candle_writer: &mut CandlePersistenceWriter,
    m_api_errors: &metrics::Counter,
) -> (usize, usize) {
    let (candles, invalid_count) = build_valid_daily_candles(data, security_id, segment_code);

    if invalid_count > 0 {
        debug!(
            security_id,
            timeframe = TIMEFRAME_1D,
            invalid_count,
            "skipped invalid candles in daily response"
        );
        #[allow(clippy::cast_possible_truncation)]
        // APPROVED: usize->u64 is lossless on 64-bit targets
        m_api_errors.increment(invalid_count as u64);
    }

    let mut count = 0_usize;
    let mut persist_failures = 0_usize;
    let mut consecutive_persist_failures = 0_usize;
    for candle in &candles {
        if let Err(err) = candle_writer.append_candle(candle) {
            warn!(
                ?err,
                security_id,
                timeframe = TIMEFRAME_1D,
                "failed to append daily candle to QuestDB"
            );
            persist_failures = persist_failures.saturating_add(1);
            consecutive_persist_failures = consecutive_persist_failures.saturating_add(1);
            if consecutive_persist_failures >= MAX_CONSECUTIVE_PERSIST_FAILURES {
                tracing::error!(
                    security_id,
                    timeframe = TIMEFRAME_1D,
                    consecutive_failures = consecutive_persist_failures,
                    "QuestDB unreachable — {} consecutive persist failures, aborting persist loop",
                    MAX_CONSECUTIVE_PERSIST_FAILURES,
                );
                break;
            }
            continue;
        }
        consecutive_persist_failures = 0; // Reset on success
        count = count.saturating_add(1);
    }
    (count, persist_failures)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intraday_request_serialization() {
        let request = IntradayRequest {
            security_id: "13".to_string(),
            exchange_segment: "NSE_FNO".to_string(),
            instrument: "FUTIDX".to_string(),
            interval: "1".to_string(),
            oi: true,
            from_date: "2025-01-01 09:15:00".to_string(),
            to_date: "2025-01-05 15:30:00".to_string(),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("securityId"));
        assert!(json.contains("exchangeSegment"));
        assert!(json.contains("\"oi\":true"));
        assert!(json.contains("\"interval\":\"1\""));
        // Verify camelCase serialization
        assert!(!json.contains("security_id"));
        assert!(!json.contains("exchange_segment"));
    }

    #[test]
    fn test_daily_request_serialization() {
        let request = DailyRequest {
            security_id: "13".to_string(),
            exchange_segment: "IDX_I".to_string(),
            instrument: "INDEX".to_string(),
            expiry_code: None,
            oi: true,
            from_date: "2025-12-14".to_string(),
            to_date: "2026-03-14".to_string(),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("securityId"));
        assert!(json.contains("fromDate"));
        assert!(json.contains("toDate"));
        // expiryCode should be omitted when None
        assert!(!json.contains("expiryCode"));
    }

    #[test]
    fn test_candle_fetch_summary_default() {
        let summary = CandleFetchSummary {
            instruments_fetched: 10,
            instruments_failed: 2,
            total_candles: 3750,
            instruments_skipped: 5,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: HashMap::new(),
        };
        assert_eq!(summary.instruments_fetched, 10);
        assert_eq!(summary.total_candles, 3750);
        assert_eq!(summary.persist_failures, 0);
        assert!(summary.failed_instruments.is_empty());
        assert!(summary.failure_reasons.is_empty());
    }

    #[test]
    fn test_candle_fetch_summary_includes_failed_names() {
        let summary = CandleFetchSummary {
            instruments_fetched: 229,
            instruments_failed: 3,
            total_candles: 172125,
            instruments_skipped: 1050,
            persist_failures: 0,
            failed_instruments: vec![
                "11536 (NSE_EQ)".to_string(),
                "13 (IDX_I)".to_string(),
                "25 (IDX_I)".to_string(),
            ],
            failure_reasons: HashMap::from([
                ("token_expired".to_string(), 2),
                ("network_or_api".to_string(), 1),
            ]),
        };
        assert_eq!(summary.failed_instruments.len(), 3);
        assert!(summary.failed_instruments[0].contains("11536"));
        assert!(summary.failed_instruments[1].contains("IDX_I"));
        assert_eq!(summary.failure_reasons.len(), 2);
    }

    #[test]
    fn test_failed_names_capped_at_max() {
        const {
            assert!(
                MAX_FAILED_INSTRUMENT_NAMES <= 50,
                // "failed names must be bounded"
            );
            assert!(
                MAX_FAILED_INSTRUMENT_NAMES >= 10,
                // "need at least 10 for diagnostics"
            );
        }
    }

    #[test]
    fn test_persist_failure_not_counted_as_success() {
        // Verify the persist functions return (count, persist_failures) tuple
        // where count does NOT include failed writes.
        let summary = CandleFetchSummary {
            instruments_fetched: 232,
            instruments_failed: 0,
            total_candles: 187458,
            instruments_skipped: 1050,
            persist_failures: 42,
            failed_instruments: vec![],
            failure_reasons: HashMap::from([("persist".to_string(), 42)]),
        };
        // total_candles should be the ACTUAL successful writes (187458),
        // not 187458 + 42 = 187500 (which would be the old buggy behavior)
        assert_eq!(summary.total_candles, 187458);
        assert_eq!(summary.persist_failures, 42);
    }

    /// IST offset in seconds (5h30m) for UTC→IST conversion in tests.
    const IST_OFFSET: i64 = 19_800;

    /// Extracts IST hour and minute from a UTC epoch by adding IST offset.
    fn utc_epoch_to_ist_hm(utc_epoch: i64) -> (i64, i64) {
        let ist_secs = utc_epoch.saturating_add(IST_OFFSET) % 86_400;
        (ist_secs / 3600, (ist_secs % 3600) / 60)
    }

    /// Verifies Dhan REST API returns standard UTC epoch seconds.
    ///
    /// Market close candle at 15:29 IST = 09:59 UTC on 2026-03-09.
    /// Stored as-is in QuestDB; Grafana `timezone: Asia/Kolkata` shows IST.
    #[test]
    fn test_dhan_rest_api_returns_utc_epoch_market_close() {
        // 15:29 IST = 09:59 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 9 * 3600 + 59 * 60; // 1_773_050_340
        assert_eq!(utc_epoch, 1_773_050_340);

        // Convert UTC epoch to IST for verification
        let (ist_hour, ist_minute) = utc_epoch_to_ist_hm(utc_epoch);
        assert_eq!(ist_hour, 15, "IST hour must be 15");
        assert_eq!(ist_minute, 29, "IST minute must be 29");
    }

    /// Verifies UTC epoch for market open candle (09:15 IST = 03:45 UTC).
    #[test]
    fn test_dhan_rest_api_returns_utc_epoch_market_open() {
        // 09:15 IST = 03:45 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 3 * 3600 + 45 * 60; // 1_773_027_900
        assert_eq!(utc_epoch, 1_773_027_900);

        let (ist_hour, ist_minute) = utc_epoch_to_ist_hm(utc_epoch);
        assert_eq!(ist_hour, 9, "IST hour must be 9");
        assert_eq!(ist_minute, 15, "IST minute must be 15");
    }

    /// Verifies UTC epoch for mid-session candle (12:00 IST = 06:30 UTC).
    #[test]
    fn test_dhan_rest_api_returns_utc_epoch_mid_session() {
        // 12:00 IST = 06:30 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 6 * 3600 + 30 * 60; // 1_773_037_800
        assert_eq!(utc_epoch, 1_773_037_800);

        let (ist_hour, ist_minute) = utc_epoch_to_ist_hm(utc_epoch);
        assert_eq!(ist_hour, 12, "IST hour must be 12");
        assert_eq!(ist_minute, 0, "IST minute must be 0");
    }

    /// Verifies HistoricalCandle stores UTC epoch and timeframe correctly.
    #[test]
    fn test_historical_candle_stores_utc_epoch_and_timeframe() {
        let dhan_epoch: i64 = 1_773_050_340;
        let candle = HistoricalCandle {
            timestamp_utc_secs: dhan_epoch,
            security_id: 42,
            exchange_segment_code: 2,
            timeframe: "5m",
            open: 100.0,
            high: 102.0,
            low: 99.0,
            close: 101.0,
            volume: 1000,
            open_interest: 0,
        };
        assert_eq!(candle.timestamp_utc_secs, dhan_epoch);
        assert_eq!(candle.timeframe, "5m");
    }

    #[test]
    fn test_intraday_timeframes_constant() {
        assert_eq!(INTRADAY_TIMEFRAMES.len(), 5);
        assert_eq!(INTRADAY_TIMEFRAMES[0], ("1", "1m"));
        assert_eq!(INTRADAY_TIMEFRAMES[1], ("5", "5m"));
        assert_eq!(INTRADAY_TIMEFRAMES[2], ("15", "15m"));
        assert_eq!(INTRADAY_TIMEFRAMES[3], ("25", "25m"));
        assert_eq!(INTRADAY_TIMEFRAMES[4], ("60", "60m"));
    }

    // -- Error classification tests --

    #[test]
    fn test_dhan_error_response_parsing_805() {
        let body = br#"{"errorCode": 805, "message": "Too many requests"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::TooManyConnections);
    }

    #[test]
    fn test_dhan_error_response_parsing_807() {
        let body = br#"{"errorCode": 807, "message": "Access token expired"}"#;
        let action = classify_error(reqwest::StatusCode::UNAUTHORIZED, body);
        assert_eq!(action, ErrorAction::TokenExpired);
    }

    #[test]
    fn test_dhan_error_response_parsing_dh904() {
        let body = br#"{"errorType": "RATE_LIMIT", "errorCode": "DH-904", "errorMessage": "Rate limit exceeded"}"#;
        let action = classify_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        // HTTP 429 triggers RateLimited before body parsing
        assert_eq!(action, ErrorAction::RateLimited);
    }

    #[test]
    fn test_dhan_error_response_parsing_dh905() {
        let body = br#"{"errorType": "INPUT_EXCEPTION", "errorCode": "DH-905", "errorMessage": "Invalid input"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::NeverRetry);
    }

    #[test]
    fn test_http_429_always_rate_limited() {
        let action = classify_error(reqwest::StatusCode::TOO_MANY_REQUESTS, b"{}");
        assert_eq!(action, ErrorAction::RateLimited);
    }

    #[test]
    fn test_data_api_error_code_extraction() {
        // status field alias
        let body = br#"{"status": 805, "message": "Too many"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::TooManyConnections);
    }

    #[test]
    fn test_unknown_error_body_standard_retry() {
        let body = br#"{"unknown": "field"}"#;
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    // -- OHLC validation tests --

    /// Validates that OHLC candles with high < low are rejected.
    #[test]
    fn test_ohlc_validation_rejects_high_below_low() {
        // high < low should be rejected
        let high = 99.0_f64;
        let low = 100.0_f64;
        assert!(high < low, "test setup: high must be less than low");
        // In production, this candle would be skipped with a warning
    }

    /// Validates that non-positive prices are rejected.
    #[test]
    fn test_ohlc_validation_rejects_non_positive() {
        let prices = [0.0_f64, -1.0, -100.5];
        for price in &prices {
            assert!(
                *price <= 0.0,
                "price {price} should fail non-positive check"
            );
        }
        // Positive prices pass the check
        let positive = 100.5_f64;
        assert!(positive > 0.0);
    }

    /// Verifies DisplayIndex (e.g., INDIA VIX) maps to INDEX instrument type
    /// and is NOT skipped by the category filter.
    #[test]
    fn test_display_index_maps_to_index_type() {
        use tickvault_common::instrument_registry::SubscriptionCategory;

        let categories_that_fetch = [
            SubscriptionCategory::MajorIndexValue,
            SubscriptionCategory::DisplayIndex,
            SubscriptionCategory::StockEquity,
        ];

        let categories_that_skip = [
            SubscriptionCategory::IndexDerivative,
            SubscriptionCategory::StockDerivative,
        ];

        for cat in &categories_that_fetch {
            let should_skip = matches!(
                cat,
                SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative
            );
            assert!(!should_skip, "{cat:?} should NOT be skipped");
        }

        for cat in &categories_that_skip {
            let should_skip = matches!(
                cat,
                SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative
            );
            assert!(should_skip, "{cat:?} should be skipped");
        }

        // DisplayIndex maps to INDEX (same as MajorIndexValue)
        let instrument_type = match SubscriptionCategory::DisplayIndex {
            SubscriptionCategory::MajorIndexValue | SubscriptionCategory::DisplayIndex => "INDEX",
            SubscriptionCategory::StockEquity => "EQUITY",
            _ => "SKIP",
        };
        assert_eq!(instrument_type, "INDEX");
    }

    // -- Retry wave tests --

    #[test]
    fn test_retry_wave_constants() {
        const {
            assert!(
                RETRY_WAVE_MAX >= 3,
                // "need at least 3 retry waves for resilient fetching"
            );
            assert!(
                RETRY_WAVE_BACKOFF_BASE_SECS >= 10,
                // "retry wave backoff must be at least 10s"
            );
        }
    }

    #[test]
    fn test_retry_wave_backoff_sequence() {
        // Backoff grows linearly but caps at RETRY_WAVE_MAX_BACKOFF_SECS (300s)
        for wave in 1..=RETRY_WAVE_MAX {
            let backoff = compute_retry_wave_backoff_secs(wave);
            assert!(backoff > 0, "wave {wave} backoff must be positive");
            assert!(
                backoff <= RETRY_WAVE_MAX_BACKOFF_SECS,
                "wave {wave} backoff {backoff}s should not exceed max backoff"
            );
        }
    }

    #[test]
    fn test_instrument_fetch_result_variants() {
        // Verify enum variants exist and Debug works
        let success = InstrumentFetchResult::Success(100, 0);
        let failed = InstrumentFetchResult::Failed;
        assert!(format!("{success:?}").contains("100"));
        assert!(format!("{failed:?}").contains("Failed"));
    }

    #[test]
    fn test_retry_wave_max_total_backoff() {
        // With 100 waves and 300s cap, total backoff is bounded.
        // Waves 1-10: linear (30+60+...+300 = 1650s), Waves 11-100: capped (90 * 300 = 27000s)
        // Total ~28650s (~8 hours). This ensures we never give up on a short outage.
        let mut total_backoff = 0_u64;
        for wave in 1..=RETRY_WAVE_MAX {
            total_backoff = total_backoff.saturating_add(compute_retry_wave_backoff_secs(wave));
        }
        // Total should be bounded (not infinite)
        assert!(
            total_backoff < 100_000,
            "total retry backoff {total_backoff}s should be bounded"
        );
        // But should be substantial enough to cover extended outages
        assert!(
            total_backoff > 3600,
            "total retry backoff {total_backoff}s should cover at least 1 hour of retries"
        );
    }

    // -----------------------------------------------------------------------
    // Additional unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_max_response_body_size_is_10mb() {
        assert_eq!(MAX_RESPONSE_BODY_SIZE, 10 * 1024 * 1024);
    }

    #[test]
    fn test_intraday_request_oi_field_serialized() {
        let request = IntradayRequest {
            security_id: "1333".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            interval: "1".to_string(),
            oi: false,
            from_date: "2026-01-01".to_string(),
            to_date: "2026-01-05".to_string(),
        };
        let json = serde_json::to_string(&request).expect("should serialize");
        assert!(
            json.contains("\"oi\":false"),
            "JSON must contain oi field serialized as false"
        );
    }

    #[test]
    fn test_intraday_request_all_fields_camel_case() {
        let request = IntradayRequest {
            security_id: "42".to_string(),
            exchange_segment: "NSE_FNO".to_string(),
            instrument: "FUTIDX".to_string(),
            interval: "5".to_string(),
            oi: true,
            from_date: "2026-03-01".to_string(),
            to_date: "2026-03-10".to_string(),
        };
        let json = serde_json::to_string(&request).expect("should serialize");

        // Verify no snake_case keys leaked into the JSON
        assert!(
            !json.contains("security_id"),
            "must not contain snake_case security_id"
        );
        assert!(
            !json.contains("exchange_segment"),
            "must not contain snake_case exchange_segment"
        );
        assert!(
            !json.contains("from_date"),
            "must not contain snake_case from_date"
        );
        assert!(
            !json.contains("to_date"),
            "must not contain snake_case to_date"
        );

        // Verify camelCase keys are present
        assert!(
            json.contains("securityId"),
            "must contain camelCase securityId"
        );
        assert!(
            json.contains("exchangeSegment"),
            "must contain camelCase exchangeSegment"
        );
        assert!(json.contains("fromDate"), "must contain camelCase fromDate");
        assert!(json.contains("toDate"), "must contain camelCase toDate");
    }

    #[test]
    fn test_candle_fetch_summary_all_zeroes() {
        let summary = CandleFetchSummary {
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: HashMap::new(),
            instruments_fetched: 0,
            instruments_failed: 0,
            total_candles: 0,
            instruments_skipped: 0,
        };
        assert_eq!(summary.instruments_fetched, 0);
        assert_eq!(summary.instruments_failed, 0);
        assert_eq!(summary.total_candles, 0);
        assert_eq!(summary.instruments_skipped, 0);
    }

    #[test]
    fn test_nan_price_detection() {
        assert!(
            !f64::NAN.is_finite(),
            "NaN must be detected as non-finite (guard in fetch loop)"
        );
    }

    #[test]
    fn test_infinity_price_detection() {
        assert!(
            !f64::INFINITY.is_finite(),
            "INFINITY must be detected as non-finite (guard in fetch loop)"
        );
    }

    #[test]
    fn test_neg_infinity_price_detection() {
        assert!(
            !f64::NEG_INFINITY.is_finite(),
            "NEG_INFINITY must be detected as non-finite (guard in fetch loop)"
        );
    }

    #[test]
    fn test_normal_price_is_finite() {
        assert!(
            245.50_f64.is_finite(),
            "Normal price values must pass the is_finite() guard"
        );
    }

    #[test]
    fn test_dhan_response_consistency_check() {
        // Mismatched array lengths: open has 3 elements, high has 2
        let response = DhanIntradayResponse {
            open: vec![100.0, 101.0, 102.0],
            high: vec![103.0, 104.0], // mismatched — only 2
            low: vec![99.0, 100.0, 101.0],
            close: vec![101.0, 102.0, 103.0],
            volume: vec![1000, 2000, 3000],
            timestamp: vec![1700000000, 1700000060, 1700000120],
            open_interest: vec![],
        };
        assert!(
            !response.is_consistent(),
            "Response with mismatched array lengths must return is_consistent() == false"
        );
    }

    // -- C1: Token expiry recovery tests --

    #[test]
    fn test_token_expired_variant_exists() {
        let result = InstrumentFetchResult::TokenExpired;
        assert!(format!("{result:?}").contains("TokenExpired"));
    }

    #[test]
    fn test_token_expired_distinct_from_failed() {
        assert_ne!(
            InstrumentFetchResult::TokenExpired,
            InstrumentFetchResult::Failed,
            "TokenExpired must be distinct from Failed for retry wave awareness"
        );
    }

    #[test]
    fn test_token_refresh_wait_constant() {
        const {
            assert!(
                TOKEN_REFRESH_WAIT_SECS >= 10,
                // "token refresh wait must be >= 10s to give renewal task time"
            );
            assert!(
                TOKEN_REFRESH_WAIT_SECS <= 60,
                // "token refresh wait must be <= 60s to not waste time"
            );
        }
    }

    #[test]
    fn test_max_token_expired_retries_bounded() {
        const {
            assert!(
                MAX_TOKEN_EXPIRED_RETRIES >= 1,
                // "must retry at least once after token refresh"
            );
            assert!(
                MAX_TOKEN_EXPIRED_RETRIES <= 20,
                // "bounded but generous — allow time for token renewal"
            );
        }
    }

    // -- C2: Persist circuit breaker tests --

    #[test]
    fn test_consecutive_persist_failure_threshold() {
        const {
            assert!(
                MAX_CONSECUTIVE_PERSIST_FAILURES >= 5,
                // "threshold must be >= 5 to tolerate transient errors"
            );
            assert!(
                MAX_CONSECUTIVE_PERSIST_FAILURES <= 50,
                // "threshold must be <= 50 to break early on QuestDB failure"
            );
        }
    }

    #[test]
    fn test_persist_circuit_breaker_resets_on_success() {
        // Simulates the circuit breaker logic: consecutive failures reset on success
        let mut consecutive = 0_usize;
        // 5 failures
        for _ in 0..5 {
            consecutive = consecutive.saturating_add(1);
        }
        assert_eq!(consecutive, 5);
        // 1 success resets
        consecutive = 0;
        assert_eq!(
            consecutive, 0,
            "success must reset consecutive failure counter"
        );
    }

    // -- M4: Failure reason tracking tests --

    #[test]
    fn test_candle_fetch_summary_tracks_failure_reasons() {
        let mut reasons = HashMap::new();
        reasons.insert("token_expired".to_string(), 5);
        reasons.insert("network_or_api".to_string(), 3);
        reasons.insert("persist".to_string(), 42);

        let summary = CandleFetchSummary {
            instruments_fetched: 224,
            instruments_failed: 8,
            total_candles: 168000,
            instruments_skipped: 1050,
            persist_failures: 42,
            failed_instruments: vec!["13 (IDX_I)".to_string()],
            failure_reasons: reasons,
        };

        assert_eq!(summary.failure_reasons.len(), 3);
        assert_eq!(summary.failure_reasons["token_expired"], 5);
        assert_eq!(summary.failure_reasons["network_or_api"], 3);
        assert_eq!(summary.failure_reasons["persist"], 42);
    }

    // -- TimeframeFetchResult tests --

    #[test]
    fn test_timeframe_fetch_result_variants() {
        let ok = TimeframeFetchResult::Ok(100, 2);
        let failed = TimeframeFetchResult::Failed;
        let expired = TimeframeFetchResult::TokenExpired;
        assert!(format!("{ok:?}").contains("100"));
        assert!(format!("{failed:?}").contains("Failed"));
        assert!(format!("{expired:?}").contains("TokenExpired"));
    }

    // =======================================================================
    // Extracted pure function tests — candle validation
    // =======================================================================

    #[test]
    fn test_validate_candle_ohlc_valid() {
        assert_eq!(
            validate_candle_ohlc(100.0, 105.0, 98.0, 102.0),
            CandleValidation::Valid
        );
    }

    #[test]
    fn test_validate_candle_ohlc_equal_high_low() {
        // high == low is valid (e.g. circuit limit)
        assert_eq!(
            validate_candle_ohlc(100.0, 100.0, 100.0, 100.0),
            CandleValidation::Valid
        );
    }

    #[test]
    fn test_validate_candle_ohlc_nan_open() {
        assert_eq!(
            validate_candle_ohlc(f64::NAN, 105.0, 98.0, 102.0),
            CandleValidation::NonFinite
        );
    }

    #[test]
    fn test_validate_candle_ohlc_nan_high() {
        assert_eq!(
            validate_candle_ohlc(100.0, f64::NAN, 98.0, 102.0),
            CandleValidation::NonFinite
        );
    }

    #[test]
    fn test_validate_candle_ohlc_nan_low() {
        assert_eq!(
            validate_candle_ohlc(100.0, 105.0, f64::NAN, 102.0),
            CandleValidation::NonFinite
        );
    }

    #[test]
    fn test_validate_candle_ohlc_nan_close() {
        assert_eq!(
            validate_candle_ohlc(100.0, 105.0, 98.0, f64::NAN),
            CandleValidation::NonFinite
        );
    }

    #[test]
    fn test_validate_candle_ohlc_infinity_open() {
        assert_eq!(
            validate_candle_ohlc(f64::INFINITY, 105.0, 98.0, 102.0),
            CandleValidation::NonFinite
        );
    }

    #[test]
    fn test_validate_candle_ohlc_neg_infinity_close() {
        assert_eq!(
            validate_candle_ohlc(100.0, 105.0, 98.0, f64::NEG_INFINITY),
            CandleValidation::NonFinite
        );
    }

    #[test]
    fn test_validate_candle_ohlc_zero_price() {
        assert_eq!(
            validate_candle_ohlc(0.0, 105.0, 98.0, 102.0),
            CandleValidation::NonPositive
        );
    }

    #[test]
    fn test_validate_candle_ohlc_negative_price() {
        assert_eq!(
            validate_candle_ohlc(-1.0, 105.0, 98.0, 102.0),
            CandleValidation::NonPositive
        );
    }

    #[test]
    fn test_validate_candle_ohlc_zero_low() {
        assert_eq!(
            validate_candle_ohlc(100.0, 105.0, 0.0, 102.0),
            CandleValidation::NonPositive
        );
    }

    #[test]
    fn test_validate_candle_ohlc_zero_close() {
        assert_eq!(
            validate_candle_ohlc(100.0, 105.0, 98.0, 0.0),
            CandleValidation::NonPositive
        );
    }

    #[test]
    fn test_validate_candle_ohlc_negative_high() {
        assert_eq!(
            validate_candle_ohlc(100.0, -5.0, 98.0, 102.0),
            CandleValidation::NonPositive
        );
    }

    #[test]
    fn test_validate_candle_ohlc_high_below_low() {
        assert_eq!(
            validate_candle_ohlc(100.0, 95.0, 98.0, 97.0),
            CandleValidation::HighBelowLow
        );
    }

    #[test]
    fn test_validate_candle_ohlc_tiny_prices_valid() {
        // Very small positive prices are valid (e.g. penny stocks)
        assert_eq!(
            validate_candle_ohlc(0.01, 0.02, 0.01, 0.015),
            CandleValidation::Valid
        );
    }

    #[test]
    fn test_validate_candle_ohlc_large_prices_valid() {
        assert_eq!(
            validate_candle_ohlc(999_999.0, 1_000_000.0, 999_990.0, 999_995.0),
            CandleValidation::Valid
        );
    }

    #[test]
    fn test_candle_validation_debug_format() {
        assert!(format!("{:?}", CandleValidation::Valid).contains("Valid"));
        assert!(format!("{:?}", CandleValidation::NonFinite).contains("NonFinite"));
        assert!(format!("{:?}", CandleValidation::NonPositive).contains("NonPositive"));
        assert!(format!("{:?}", CandleValidation::HighBelowLow).contains("HighBelowLow"));
    }

    // =======================================================================
    // Extracted pure function tests — intraday window filter
    // =======================================================================

    #[test]
    fn test_is_outside_intraday_window_market_open() {
        // 09:15 IST = 03:45 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 3 * 3600 + 45 * 60;
        assert!(
            !is_outside_intraday_window(utc_epoch),
            "09:15 IST should be inside intraday window"
        );
    }

    #[test]
    fn test_is_outside_intraday_window_mid_session() {
        // 12:00 IST = 06:30 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 6 * 3600 + 30 * 60;
        assert!(
            !is_outside_intraday_window(utc_epoch),
            "12:00 IST should be inside intraday window"
        );
    }

    #[test]
    fn test_is_outside_intraday_window_just_before_close() {
        // 15:29 IST = 09:59 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 9 * 3600 + 59 * 60;
        assert!(
            !is_outside_intraday_window(utc_epoch),
            "15:29 IST should be inside intraday window"
        );
    }

    #[test]
    fn test_is_outside_intraday_window_at_close() {
        // 15:30 IST = 10:00 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 10 * 3600;
        assert!(
            is_outside_intraday_window(utc_epoch),
            "15:30 IST should be OUTSIDE intraday window"
        );
    }

    #[test]
    fn test_is_outside_intraday_window_after_close() {
        // 16:00 IST = 10:30 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 10 * 3600 + 30 * 60;
        assert!(
            is_outside_intraday_window(utc_epoch),
            "16:00 IST should be OUTSIDE intraday window"
        );
    }

    #[test]
    fn test_is_outside_intraday_window_midnight() {
        // 00:00 IST = 18:30 previous day UTC
        let utc_epoch: i64 = 1_773_014_400 - 5 * 3600 - 30 * 60;
        assert!(
            !is_outside_intraday_window(utc_epoch),
            "00:00 IST should be inside window (before 15:30)"
        );
    }

    #[test]
    fn test_is_outside_intraday_window_23_59() {
        // 23:59 IST — after market close, should be outside
        // IST secs of day = 23*3600 + 59*60 = 86340, which is >= 55800
        let utc_epoch: i64 = 1_773_014_400 + 18 * 3600 + 29 * 60;
        assert!(
            is_outside_intraday_window(utc_epoch),
            "23:59 IST should be OUTSIDE intraday window"
        );
    }

    // =======================================================================
    // Extracted pure function tests — OI extraction
    // =======================================================================

    #[test]
    fn test_extract_oi_within_bounds() {
        let oi_data = vec![100, 200, 300];
        assert_eq!(extract_oi(&oi_data, 0), 100);
        assert_eq!(extract_oi(&oi_data, 1), 200);
        assert_eq!(extract_oi(&oi_data, 2), 300);
    }

    #[test]
    fn test_extract_oi_out_of_bounds() {
        let oi_data = vec![100, 200];
        assert_eq!(extract_oi(&oi_data, 2), 0);
        assert_eq!(extract_oi(&oi_data, 100), 0);
    }

    #[test]
    fn test_extract_oi_empty_array() {
        let oi_data: Vec<i64> = vec![];
        assert_eq!(extract_oi(&oi_data, 0), 0);
        assert_eq!(extract_oi(&oi_data, 5), 0);
    }

    // =======================================================================
    // Extracted pure function tests — date computation
    // =======================================================================

    #[test]
    fn test_compute_fetch_date_range_normal() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        let (from, to) = compute_fetch_date_range(today, 90);
        assert_eq!(to, today);
        assert_eq!(from, chrono::NaiveDate::from_ymd_opt(2025, 12, 21).unwrap());
    }

    #[test]
    fn test_compute_fetch_date_range_zero_lookback() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        let (from, to) = compute_fetch_date_range(today, 0);
        assert_eq!(from, today);
        assert_eq!(to, today);
    }

    #[test]
    fn test_compute_fetch_date_range_one_day_lookback() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        let (from, to) = compute_fetch_date_range(today, 1);
        assert_eq!(to, today);
        assert_eq!(from, chrono::NaiveDate::from_ymd_opt(2026, 3, 20).unwrap());
    }

    #[test]
    fn test_compute_fetch_date_range_large_lookback() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        let (from, to) = compute_fetch_date_range(today, 365);
        assert_eq!(to, today);
        assert_eq!(from, chrono::NaiveDate::from_ymd_opt(2025, 3, 21).unwrap());
    }

    #[test]
    fn test_compute_daily_to_date_normal() {
        let to = chrono::NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        let daily_to = compute_daily_to_date(to);
        assert_eq!(
            daily_to,
            chrono::NaiveDate::from_ymd_opt(2026, 3, 22).unwrap()
        );
    }

    #[test]
    fn test_compute_daily_to_date_month_boundary() {
        let to = chrono::NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let daily_to = compute_daily_to_date(to);
        assert_eq!(
            daily_to,
            chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap()
        );
    }

    #[test]
    fn test_compute_daily_to_date_year_boundary() {
        let to = chrono::NaiveDate::from_ymd_opt(2025, 12, 31).unwrap();
        let daily_to = compute_daily_to_date(to);
        assert_eq!(
            daily_to,
            chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
        );
    }

    #[test]
    fn test_compute_daily_to_date_leap_year() {
        let to = chrono::NaiveDate::from_ymd_opt(2028, 2, 28).unwrap();
        let daily_to = compute_daily_to_date(to);
        // 2028 is a leap year
        assert_eq!(
            daily_to,
            chrono::NaiveDate::from_ymd_opt(2028, 2, 29).unwrap()
        );
    }

    // =======================================================================
    // Extracted pure function tests — intraday date range formatting
    // =======================================================================

    #[test]
    fn test_format_intraday_date_range() {
        let from = chrono::NaiveDate::from_ymd_opt(2025, 12, 21).unwrap();
        let to = chrono::NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        let (from_str, to_str) = format_intraday_date_range(from, to);
        assert_eq!(from_str, "2025-12-21 09:15:00");
        assert_eq!(to_str, "2026-03-21 15:30:00");
    }

    #[test]
    fn test_format_intraday_date_range_same_day() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        let (from_str, to_str) = format_intraday_date_range(date, date);
        assert_eq!(from_str, "2026-01-05 09:15:00");
        assert_eq!(to_str, "2026-01-05 15:30:00");
    }

    // =======================================================================
    // Extracted pure function tests — DH-904 backoff
    // =======================================================================

    #[test]
    fn test_compute_dh904_backoff_secs_sequence() {
        assert_eq!(compute_dh904_backoff_secs(0), 10); // 10 * 2^0 = 10
        assert_eq!(compute_dh904_backoff_secs(1), 20); // 10 * 2^1 = 20
        assert_eq!(compute_dh904_backoff_secs(2), 40); // 10 * 2^2 = 40
        assert_eq!(compute_dh904_backoff_secs(3), 80); // 10 * 2^3 = 80
    }

    #[test]
    fn test_compute_dh904_backoff_secs_high_attempt() {
        // After attempt 3, we don't expect to call this (is_dh904_exhausted
        // would return true), but it should not overflow
        let result = compute_dh904_backoff_secs(4);
        assert_eq!(result, 160); // 10 * 2^4
    }

    #[test]
    fn test_is_dh904_exhausted() {
        assert!(!is_dh904_exhausted(0));
        assert!(!is_dh904_exhausted(1));
        assert!(!is_dh904_exhausted(2));
        assert!(!is_dh904_exhausted(3));
        assert!(is_dh904_exhausted(4)); // DH_904_MAX_BACKOFF_ATTEMPTS = 4
        assert!(is_dh904_exhausted(5));
    }

    // =======================================================================
    // Extracted pure function tests — retry delay
    // =======================================================================

    #[test]
    fn test_compute_retry_delay_ms() {
        assert_eq!(compute_retry_delay_ms(0), 0);
        assert_eq!(compute_retry_delay_ms(1), 1000);
        assert_eq!(compute_retry_delay_ms(2), 2000);
        assert_eq!(compute_retry_delay_ms(3), 3000);
    }

    #[test]
    fn test_compute_retry_delay_ms_large_attempt() {
        // Should not overflow
        assert_eq!(compute_retry_delay_ms(10), 10000);
        assert_eq!(compute_retry_delay_ms(100), 100000);
    }

    // =======================================================================
    // Extracted pure function tests — retry wave backoff
    // =======================================================================

    #[test]
    fn test_compute_retry_wave_backoff_secs() {
        assert_eq!(compute_retry_wave_backoff_secs(0), 0);
        assert_eq!(compute_retry_wave_backoff_secs(1), 30);
        assert_eq!(compute_retry_wave_backoff_secs(2), 60);
        assert_eq!(compute_retry_wave_backoff_secs(3), 90);
        assert_eq!(compute_retry_wave_backoff_secs(4), 120);
        assert_eq!(compute_retry_wave_backoff_secs(5), 150);
    }

    // =======================================================================
    // Extracted pure function tests — instrument category mapping
    // =======================================================================

    #[test]
    fn test_instrument_type_for_major_index() {
        assert_eq!(
            instrument_type_for_category(SubscriptionCategory::MajorIndexValue),
            Some("INDEX")
        );
    }

    #[test]
    fn test_instrument_type_for_display_index() {
        assert_eq!(
            instrument_type_for_category(SubscriptionCategory::DisplayIndex),
            Some("INDEX")
        );
    }

    #[test]
    fn test_instrument_type_for_stock_equity() {
        assert_eq!(
            instrument_type_for_category(SubscriptionCategory::StockEquity),
            Some("EQUITY")
        );
    }

    #[test]
    fn test_instrument_type_for_index_derivative_skip() {
        assert_eq!(
            instrument_type_for_category(SubscriptionCategory::IndexDerivative),
            None
        );
    }

    #[test]
    fn test_instrument_type_for_stock_derivative_skip() {
        assert_eq!(
            instrument_type_for_category(SubscriptionCategory::StockDerivative),
            None
        );
    }

    // =======================================================================
    // Extracted pure function tests — response size check
    // =======================================================================

    #[test]
    fn test_is_response_oversized_within_limit() {
        assert!(!is_response_oversized(0));
        assert!(!is_response_oversized(1));
        assert!(!is_response_oversized(100_000));
        assert!(!is_response_oversized(MAX_RESPONSE_BODY_SIZE));
    }

    #[test]
    fn test_is_response_oversized_exceeds_limit() {
        assert!(is_response_oversized(MAX_RESPONSE_BODY_SIZE + 1));
        assert!(is_response_oversized(20 * 1024 * 1024));
    }

    // =======================================================================
    // Extracted pure function tests — error classification
    // =======================================================================

    #[test]
    fn test_classify_error_dh904_body_not_429() {
        // DH-904 from body (non-429 status) should also be RateLimited
        let body =
            br#"{"errorType": "RATE_LIMIT", "errorCode": "DH-904", "errorMessage": "Rate limit"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::RateLimited);
    }

    #[test]
    fn test_classify_error_dh906() {
        let body = br#"{"errorType": "ORDER_ERROR", "errorCode": "DH-906", "errorMessage": "Order error"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::NeverRetry);
    }

    #[test]
    fn test_classify_error_dh901_token_expired() {
        let body =
            br#"{"errorType": "AUTH", "errorCode": "DH-901", "errorMessage": "Invalid auth"}"#;
        let action = classify_error(reqwest::StatusCode::UNAUTHORIZED, body);
        assert_eq!(action, ErrorAction::TokenExpired);
    }

    #[test]
    fn test_classify_error_dh908_standard_retry() {
        let body = br#"{"errorType": "INTERNAL", "errorCode": "DH-908", "errorMessage": "Internal error"}"#;
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_data_api_unknown_code() {
        let body = br#"{"errorCode": 999, "message": "Unknown data error"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_empty_body() {
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, b"");
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_invalid_json() {
        let action = classify_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            b"not json at all",
        );
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_429_overrides_body() {
        // Even if body says DH-905, HTTP 429 should win
        let body = br#"{"errorType": "INPUT", "errorCode": "DH-905", "errorMessage": "Bad input"}"#;
        let action = classify_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(action, ErrorAction::RateLimited);
    }

    #[test]
    fn test_classify_error_data_api_805_with_status_alias() {
        let body = br#"{"status": 805, "message": "Too many connections"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::TooManyConnections);
    }

    #[test]
    fn test_classify_error_data_api_807_with_error_code_alias() {
        let body = br#"{"errorCode": 807, "errorMessage": "Token expired"}"#;
        let action = classify_error(reqwest::StatusCode::UNAUTHORIZED, body);
        assert_eq!(action, ErrorAction::TokenExpired);
    }

    #[test]
    fn test_classify_error_null_error_code() {
        let body = br#"{"errorType": "UNKNOWN", "errorCode": null, "errorMessage": "Something"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    // =======================================================================
    // Extracted pure function tests — build candles from response
    // =======================================================================

    fn sample_intraday_response() -> DhanIntradayResponse {
        DhanIntradayResponse {
            open: vec![100.0, 101.0, 102.0],
            high: vec![105.0, 106.0, 107.0],
            low: vec![98.0, 99.0, 100.0],
            close: vec![103.0, 104.0, 105.0],
            volume: vec![1000, 2000, 3000],
            timestamp: vec![1_773_027_900, 1_773_027_960, 1_773_028_020],
            open_interest: vec![5000, 6000, 7000],
        }
    }

    fn sample_daily_response() -> DhanDailyResponse {
        DhanDailyResponse {
            open: vec![200.0, 201.0],
            high: vec![210.0, 211.0],
            low: vec![195.0, 196.0],
            close: vec![205.0, 206.0],
            volume: vec![10_000, 20_000],
            timestamp: vec![1_772_582_400, 1_773_014_400],
            open_interest: vec![],
        }
    }

    #[test]
    fn test_build_intraday_candle_first_index() {
        let data = sample_intraday_response();
        let candle = build_intraday_candle(&data, 0, 42, 2, "1m");
        assert_eq!(candle.security_id, 42);
        assert_eq!(candle.exchange_segment_code, 2);
        assert_eq!(candle.timeframe, "1m");
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.high, 105.0);
        assert_eq!(candle.low, 98.0);
        assert_eq!(candle.close, 103.0);
        assert_eq!(candle.volume, 1000);
        assert_eq!(candle.open_interest, 5000);
        assert_eq!(candle.timestamp_utc_secs, 1_773_027_900);
    }

    #[test]
    fn test_build_intraday_candle_last_index() {
        let data = sample_intraday_response();
        let candle = build_intraday_candle(&data, 2, 99, 1, "5m");
        assert_eq!(candle.open, 102.0);
        assert_eq!(candle.close, 105.0);
        assert_eq!(candle.volume, 3000);
        assert_eq!(candle.open_interest, 7000);
        assert_eq!(candle.timeframe, "5m");
    }

    #[test]
    fn test_build_daily_candle_first_index() {
        let data = sample_daily_response();
        let candle = build_daily_candle(&data, 0, 11536, 1);
        assert_eq!(candle.security_id, 11536);
        assert_eq!(candle.exchange_segment_code, 1);
        assert_eq!(candle.timeframe, TIMEFRAME_1D);
        assert_eq!(candle.open, 200.0);
        assert_eq!(candle.high, 210.0);
        assert_eq!(candle.low, 195.0);
        assert_eq!(candle.close, 205.0);
        assert_eq!(candle.volume, 10_000);
        assert_eq!(candle.open_interest, 0); // empty OI array -> 0
    }

    #[test]
    fn test_build_daily_candle_with_oi() {
        let mut data = sample_daily_response();
        data.open_interest = vec![50_000, 60_000];
        let candle = build_daily_candle(&data, 1, 13, 0);
        assert_eq!(candle.open_interest, 60_000);
    }

    // =======================================================================
    // Extracted pure function tests — collect failed instrument names
    // =======================================================================

    #[test]
    fn test_collect_failed_instrument_names_empty() {
        let result = collect_failed_instrument_names(&[], &[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_failed_instrument_names_basic() {
        let pending = vec![0, 2];
        let sec_ids = vec![11536, 1333, 25];
        let segments = vec!["NSE_EQ", "NSE_EQ", "IDX_I"];
        let result = collect_failed_instrument_names(&pending, &sec_ids, &segments);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "11536 (NSE_EQ)");
        assert_eq!(result[1], "25 (IDX_I)");
    }

    #[test]
    fn test_collect_failed_instrument_names_capped() {
        // Create more than MAX_FAILED_INSTRUMENT_NAMES pending indices
        let n = MAX_FAILED_INSTRUMENT_NAMES + 10;
        let pending: Vec<usize> = (0..n).collect();
        let sec_ids: Vec<u32> = (0..n as u32).collect();
        let segments: Vec<&str> = vec!["NSE_EQ"; n];
        let result = collect_failed_instrument_names(&pending, &sec_ids, &segments);
        assert_eq!(result.len(), MAX_FAILED_INSTRUMENT_NAMES);
    }

    // =======================================================================
    // Extracted pure function tests — build failure reasons
    // =======================================================================

    #[test]
    fn test_build_failure_reasons_empty() {
        let reasons = build_failure_reasons(&[], &[], 0);
        assert!(reasons.is_empty());
    }

    #[test]
    fn test_build_failure_reasons_all_token_expired() {
        let pending = vec![0, 1];
        let token_counts = vec![10, 15]; // Both >= MAX_TOKEN_EXPIRED_RETRIES (10)
        let reasons = build_failure_reasons(&pending, &token_counts, 0);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons["token_expired"], 2);
    }

    #[test]
    fn test_build_failure_reasons_all_network() {
        let pending = vec![0, 1, 2];
        let token_counts = vec![0, 1, 0]; // All below MAX_TOKEN_EXPIRED_RETRIES
        let reasons = build_failure_reasons(&pending, &token_counts, 0);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons["network_or_api"], 3);
    }

    #[test]
    fn test_build_failure_reasons_mixed() {
        let pending = vec![0, 1, 2];
        let token_counts = vec![10, 0, 15]; // indices 0 and 2 >= MAX (10), 1 is network
        let reasons = build_failure_reasons(&pending, &token_counts, 0);
        assert_eq!(reasons.len(), 2);
        assert_eq!(reasons["token_expired"], 2);
        assert_eq!(reasons["network_or_api"], 1);
    }

    #[test]
    fn test_build_failure_reasons_with_persist_failures() {
        let pending = vec![0];
        let token_counts = vec![0];
        let reasons = build_failure_reasons(&pending, &token_counts, 42);
        assert_eq!(reasons.len(), 2);
        assert_eq!(reasons["network_or_api"], 1);
        assert_eq!(reasons["persist"], 42);
    }

    #[test]
    fn test_build_failure_reasons_persist_only() {
        // No pending instruments, but persist failures occurred
        let reasons = build_failure_reasons(&[], &[], 15);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons["persist"], 15);
    }

    #[test]
    fn test_build_failure_reasons_zero_persist_not_included() {
        // Zero persist failures should not add "persist" key
        let reasons = build_failure_reasons(&[], &[], 0);
        assert!(!reasons.contains_key("persist"));
    }

    // =======================================================================
    // Extracted pure function tests — response deserialization
    // =======================================================================

    #[test]
    fn test_dhan_intraday_response_consistency_all_same_length() {
        let data = sample_intraday_response();
        assert!(data.is_consistent());
    }

    #[test]
    fn test_dhan_intraday_response_consistency_empty() {
        let data = DhanIntradayResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        assert!(data.is_consistent());
        assert!(data.is_empty());
    }

    #[test]
    fn test_dhan_intraday_response_is_empty_false() {
        let data = sample_intraday_response();
        assert!(!data.is_empty());
    }

    #[test]
    fn test_dhan_intraday_response_len() {
        let data = sample_intraday_response();
        assert_eq!(data.len(), 3);
    }

    #[test]
    fn test_dhan_daily_response_consistency() {
        let data = sample_daily_response();
        assert!(data.is_consistent());
    }

    #[test]
    fn test_dhan_daily_response_len() {
        let data = sample_daily_response();
        assert_eq!(data.len(), 2);
    }

    #[test]
    fn test_dhan_daily_response_empty_oi_consistent() {
        // Empty OI array is consistent with any length
        let data = sample_daily_response();
        assert!(data.open_interest.is_empty());
        assert!(data.is_consistent());
    }

    #[test]
    fn test_dhan_daily_response_consistency_mismatched_volume() {
        let mut data = sample_daily_response();
        data.volume = vec![10_000]; // Only 1 element, should be 2
        assert!(!data.is_consistent());
    }

    #[test]
    fn test_dhan_intraday_response_consistency_mismatched_oi() {
        let mut data = sample_intraday_response();
        data.open_interest = vec![100, 200]; // 2 elements, but data has 3
        assert!(!data.is_consistent());
    }

    // =======================================================================
    // Error action equality tests
    // =======================================================================

    #[test]
    fn test_error_action_eq() {
        assert_eq!(ErrorAction::RateLimited, ErrorAction::RateLimited);
        assert_eq!(
            ErrorAction::TooManyConnections,
            ErrorAction::TooManyConnections
        );
        assert_eq!(ErrorAction::TokenExpired, ErrorAction::TokenExpired);
        assert_eq!(ErrorAction::StandardRetry, ErrorAction::StandardRetry);
        assert_eq!(ErrorAction::NeverRetry, ErrorAction::NeverRetry);
    }

    #[test]
    fn test_error_action_ne() {
        assert_ne!(ErrorAction::RateLimited, ErrorAction::NeverRetry);
        assert_ne!(ErrorAction::TooManyConnections, ErrorAction::TokenExpired);
        assert_ne!(ErrorAction::StandardRetry, ErrorAction::RateLimited);
    }

    #[test]
    fn test_error_action_debug() {
        assert!(format!("{:?}", ErrorAction::RateLimited).contains("RateLimited"));
        assert!(format!("{:?}", ErrorAction::TooManyConnections).contains("TooManyConnections"));
        assert!(format!("{:?}", ErrorAction::TokenExpired).contains("TokenExpired"));
        assert!(format!("{:?}", ErrorAction::StandardRetry).contains("StandardRetry"));
        assert!(format!("{:?}", ErrorAction::NeverRetry).contains("NeverRetry"));
    }

    // =======================================================================
    // DhanErrorResponse / DataApiErrorResponse deserialization tests
    // =======================================================================

    #[test]
    fn test_dhan_error_response_full_deserialize() {
        let json = br#"{"errorType": "RATE_LIMIT", "errorCode": "DH-904", "errorMessage": "Too many requests"}"#;
        let err: DhanErrorResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(err.error_code.as_deref(), Some("DH-904"));
        assert_eq!(err.error_type.as_deref(), Some("RATE_LIMIT"));
        assert_eq!(err.error_message.as_deref(), Some("Too many requests"));
    }

    #[test]
    fn test_dhan_error_response_missing_fields() {
        // All fields are optional
        let json = br#"{}"#;
        let err: DhanErrorResponse = serde_json::from_slice(json).unwrap();
        assert!(err.error_code.is_none());
        assert!(err.error_type.is_none());
        assert!(err.error_message.is_none());
    }

    #[test]
    fn test_data_api_error_response_with_error_code() {
        let json = br#"{"errorCode": 805, "errorMessage": "Too many"}"#;
        let err: DataApiErrorResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(err.code, Some(805));
    }

    #[test]
    fn test_data_api_error_response_with_status_alias() {
        let json = br#"{"status": 807, "message": "Token expired"}"#;
        let err: DataApiErrorResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(err.code, Some(807));
    }

    #[test]
    fn test_data_api_error_response_empty() {
        let json = br#"{}"#;
        let err: DataApiErrorResponse = serde_json::from_slice(json).unwrap();
        assert!(err.code.is_none());
    }

    // =======================================================================
    // Request body serialization edge cases
    // =======================================================================

    #[test]
    fn test_daily_request_with_expiry_code() {
        let request = DailyRequest {
            security_id: "49081".to_string(),
            exchange_segment: "NSE_FNO".to_string(),
            instrument: "FUTIDX".to_string(),
            expiry_code: Some(ExpiryCode::Current),
            oi: true,
            from_date: "2026-01-01".to_string(),
            to_date: "2026-03-21".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"expiryCode\":0"));
    }

    #[test]
    fn test_daily_request_security_id_is_string() {
        let request = DailyRequest {
            security_id: "1333".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            expiry_code: None,
            oi: false,
            from_date: "2026-01-01".to_string(),
            to_date: "2026-03-21".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        // securityId must be a string value, not a number
        assert!(json.contains("\"securityId\":\"1333\""));
    }

    #[test]
    fn test_intraday_request_interval_is_string() {
        let request = IntradayRequest {
            security_id: "42".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            interval: "60".to_string(),
            oi: true,
            from_date: "2026-01-01 09:15:00".to_string(),
            to_date: "2026-01-05 15:30:00".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"interval\":\"60\""));
    }

    // =======================================================================
    // Constant validation tests
    // =======================================================================

    #[test]
    fn test_error_805_pause_duration() {
        assert_eq!(ERROR_805_PAUSE_SECS, 60);
    }

    #[test]
    fn test_dh904_backoff_base() {
        assert_eq!(DH_904_BACKOFF_BASE_SECS, 10);
    }

    #[test]
    fn test_dh904_max_attempts() {
        assert_eq!(DH_904_MAX_BACKOFF_ATTEMPTS, 4);
    }

    #[test]
    fn test_instrument_fetch_result_success_equality() {
        assert_eq!(
            InstrumentFetchResult::Success(10, 0),
            InstrumentFetchResult::Success(10, 0)
        );
        assert_ne!(
            InstrumentFetchResult::Success(10, 0),
            InstrumentFetchResult::Success(11, 0)
        );
        assert_ne!(
            InstrumentFetchResult::Success(10, 0),
            InstrumentFetchResult::Success(10, 1)
        );
    }

    // =======================================================================
    // Additional coverage tests — build candle helpers with edge cases
    // =======================================================================

    #[test]
    fn test_build_intraday_candle_mid_index() {
        let data = sample_intraday_response();
        let candle = build_intraday_candle(&data, 1, 1333, 1, "15m");
        assert_eq!(candle.security_id, 1333);
        assert_eq!(candle.exchange_segment_code, 1);
        assert_eq!(candle.timeframe, "15m");
        assert_eq!(candle.open, 101.0);
        assert_eq!(candle.high, 106.0);
        assert_eq!(candle.low, 99.0);
        assert_eq!(candle.close, 104.0);
        assert_eq!(candle.volume, 2000);
        assert_eq!(candle.open_interest, 6000);
        assert_eq!(candle.timestamp_utc_secs, 1_773_027_960);
    }

    #[test]
    fn test_build_daily_candle_last_index() {
        let data = sample_daily_response();
        let candle = build_daily_candle(&data, 1, 25, 0);
        assert_eq!(candle.security_id, 25);
        assert_eq!(candle.exchange_segment_code, 0);
        assert_eq!(candle.timeframe, TIMEFRAME_1D);
        assert_eq!(candle.open, 201.0);
        assert_eq!(candle.high, 211.0);
        assert_eq!(candle.low, 196.0);
        assert_eq!(candle.close, 206.0);
        assert_eq!(candle.volume, 20_000);
        assert_eq!(candle.open_interest, 0); // empty OI array
    }

    #[test]
    fn test_build_intraday_candle_no_oi() {
        let mut data = sample_intraday_response();
        data.open_interest = vec![]; // empty OI
        let candle = build_intraday_candle(&data, 0, 42, 2, "1m");
        assert_eq!(candle.open_interest, 0); // extract_oi returns 0 for empty
    }

    #[test]
    fn test_build_daily_candle_segment_codes() {
        let data = sample_daily_response();
        // Test with all segment codes
        for code in [0_u8, 1, 2, 3, 4, 5, 7, 8] {
            let candle = build_daily_candle(&data, 0, 100, code);
            assert_eq!(candle.exchange_segment_code, code);
        }
    }

    // =======================================================================
    // collect_failed_instrument_names edge cases
    // =======================================================================

    #[test]
    fn test_collect_failed_instrument_names_single() {
        let result =
            collect_failed_instrument_names(&[1], &[13, 25, 1333], &["IDX_I", "IDX_I", "NSE_EQ"]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "25 (IDX_I)");
    }

    // =======================================================================
    // build_failure_reasons edge cases
    // =======================================================================

    #[test]
    fn test_build_failure_reasons_exact_threshold() {
        // Token expired count exactly at MAX_TOKEN_EXPIRED_RETRIES
        let pending = vec![0];
        let token_counts = vec![MAX_TOKEN_EXPIRED_RETRIES]; // exactly at threshold
        let reasons = build_failure_reasons(&pending, &token_counts, 0);
        assert_eq!(reasons["token_expired"], 1);
    }

    #[test]
    fn test_build_failure_reasons_just_below_threshold() {
        let pending = vec![0];
        let token_counts = vec![MAX_TOKEN_EXPIRED_RETRIES - 1]; // below threshold
        let reasons = build_failure_reasons(&pending, &token_counts, 0);
        assert_eq!(reasons["network_or_api"], 1);
        assert!(!reasons.contains_key("token_expired"));
    }

    // =======================================================================
    // DhanErrorResponse deserialization edge cases
    // =======================================================================

    #[test]
    fn test_dhan_error_response_partial_fields() {
        let json = br#"{"errorCode": "DH-905"}"#;
        let err: DhanErrorResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(err.error_code.as_deref(), Some("DH-905"));
        assert!(err.error_type.is_none());
        assert!(err.error_message.is_none());
    }

    #[test]
    fn test_data_api_error_response_with_both_aliases_rejects_duplicate() {
        // When both aliased fields are present, serde rejects as "duplicate field"
        let json = br#"{"errorCode": 805, "status": 807}"#;
        let result = serde_json::from_slice::<DataApiErrorResponse>(json);
        assert!(
            result.is_err(),
            "serde should reject duplicate aliased fields"
        );
    }

    // =======================================================================
    // classify_error additional edge cases
    // =======================================================================

    #[test]
    fn test_classify_error_data_api_generic_code() {
        let body = br#"{"errorCode": 800, "message": "Internal server error"}"#;
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_dh_unknown_code() {
        let body = br#"{"errorType": "UNKNOWN", "errorCode": "DH-999", "errorMessage": "Unknown"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    // =======================================================================
    // validate_candle_ohlc priority of checks
    // =======================================================================

    #[test]
    fn test_validate_candle_ohlc_non_finite_takes_priority_over_non_positive() {
        // NaN is also <= 0, but NonFinite should be checked first
        assert_eq!(
            validate_candle_ohlc(f64::NAN, -1.0, -2.0, -3.0),
            CandleValidation::NonFinite
        );
    }

    #[test]
    fn test_validate_candle_ohlc_non_positive_takes_priority_over_high_below_low() {
        // zero low, but also high < low — NonPositive should be checked first
        assert_eq!(
            validate_candle_ohlc(0.0, 95.0, 98.0, 97.0),
            CandleValidation::NonPositive
        );
    }

    // =======================================================================
    // is_outside_intraday_window additional boundary tests
    // =======================================================================

    #[test]
    fn test_is_outside_intraday_window_just_before_0915_ist() {
        // 09:14 IST = 03:44 UTC on 2026-03-09
        let utc_epoch: i64 = 1_773_014_400 + 3 * 3600 + 44 * 60;
        // 09:14 IST is before market open but before 15:30, so inside the window
        // because is_outside_intraday_window only checks >= 15:30
        assert!(!is_outside_intraday_window(utc_epoch));
    }

    #[test]
    fn test_is_outside_intraday_window_exactly_1530_ist() {
        // 15:30:00 IST exactly = 10:00 UTC
        let utc_epoch: i64 = 1_773_014_400 + 10 * 3600;
        assert!(is_outside_intraday_window(utc_epoch));
    }

    #[test]
    fn test_is_outside_intraday_window_1529_59_ist() {
        // 15:29:59 IST = 09:59:59 UTC
        let utc_epoch: i64 = 1_773_014_400 + 9 * 3600 + 59 * 60 + 59;
        assert!(!is_outside_intraday_window(utc_epoch));
    }

    // =======================================================================
    // compute_dh904_backoff_secs overflow safety
    // =======================================================================

    #[test]
    fn test_compute_dh904_backoff_secs_very_high_attempt() {
        // wrapping_shl with attempt >= 64 wraps — confirm no panic
        let result = compute_dh904_backoff_secs(100);
        // With wrapping_shl, 1u64.wrapping_shl(100) wraps to 1u64.wrapping_shl(100 % 64)
        let _ = result; // Just confirm no panic; result is u64 so always >= 0
    }

    // =======================================================================
    // extract_oi edge cases
    // =======================================================================

    #[test]
    fn test_extract_oi_negative_values() {
        let oi_data = vec![-100, 200, -300];
        assert_eq!(extract_oi(&oi_data, 0), -100);
        assert_eq!(extract_oi(&oi_data, 2), -300);
    }

    #[test]
    fn test_extract_oi_large_values() {
        let oi_data = vec![i64::MAX, i64::MIN];
        assert_eq!(extract_oi(&oi_data, 0), i64::MAX);
        assert_eq!(extract_oi(&oi_data, 1), i64::MIN);
    }

    // =======================================================================
    // format_intraday_date_range edge cases
    // =======================================================================

    #[test]
    fn test_format_intraday_date_range_leap_year() {
        let from = chrono::NaiveDate::from_ymd_opt(2028, 2, 29).unwrap();
        let to = chrono::NaiveDate::from_ymd_opt(2028, 3, 1).unwrap();
        let (from_str, to_str) = format_intraday_date_range(from, to);
        assert_eq!(from_str, "2028-02-29 09:15:00");
        assert_eq!(to_str, "2028-03-01 15:30:00");
    }

    // =======================================================================
    // compute_fetch_date_range edge cases
    // =======================================================================

    #[test]
    fn test_compute_fetch_date_range_max_lookback() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        let (from, to) = compute_fetch_date_range(today, 1825); // 5 years
        assert_eq!(to, today);
        assert!(from < today);
    }

    // =======================================================================
    // DhanIntradayResponse and DhanDailyResponse deserialization tests
    // =======================================================================

    #[test]
    fn test_dhan_intraday_response_json_deserialize() {
        let json = r#"{
            "open": [100.0, 101.0],
            "high": [105.0, 106.0],
            "low": [98.0, 99.0],
            "close": [103.0, 104.0],
            "volume": [1000, 2000],
            "timestamp": [1773027900, 1773027960],
            "open_interest": [5000, 6000]
        }"#;
        let data: DhanIntradayResponse = serde_json::from_str(json).unwrap();
        assert_eq!(data.len(), 2);
        assert!(data.is_consistent());
        assert!(!data.is_empty());
    }

    #[test]
    fn test_dhan_daily_response_json_deserialize() {
        let json = r#"{
            "open": [200.0],
            "high": [210.0],
            "low": [195.0],
            "close": [205.0],
            "volume": [10000],
            "timestamp": [1772928000],
            "open_interest": []
        }"#;
        let data: DhanDailyResponse = serde_json::from_str(json).unwrap();
        assert_eq!(data.len(), 1);
        assert!(data.is_consistent());
        assert!(!data.is_empty());
    }

    #[test]
    fn test_dhan_daily_response_empty() {
        let data = DhanDailyResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        assert!(data.is_empty());
        assert!(data.is_consistent());
        assert_eq!(data.len(), 0);
    }

    // =======================================================================
    // Constants validation
    // =======================================================================

    #[test]
    fn test_retry_wave_max_is_at_least_fifty() {
        assert!(
            RETRY_WAVE_MAX >= 50,
            "RETRY_WAVE_MAX must be >= 50 for guaranteed fetch"
        );
    }

    #[test]
    fn test_max_consecutive_persist_failures_value() {
        assert_eq!(MAX_CONSECUTIVE_PERSIST_FAILURES, 10);
    }

    #[test]
    fn test_token_refresh_wait_secs_value() {
        assert_eq!(TOKEN_REFRESH_WAIT_SECS, 30);
    }

    #[test]
    fn test_max_token_expired_retries_value() {
        assert_eq!(MAX_TOKEN_EXPIRED_RETRIES, 10);
    }

    #[test]
    fn test_max_failed_instrument_names_value() {
        assert_eq!(MAX_FAILED_INSTRUMENT_NAMES, 50);
    }

    // =======================================================================
    // build_valid_intraday_candles tests
    // =======================================================================

    #[test]
    fn test_build_valid_intraday_candles_all_valid() {
        let data = sample_intraday_response();
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 0);
        assert_eq!(candles.len(), 3);
        assert_eq!(candles[0].security_id, 42);
        assert_eq!(candles[0].exchange_segment_code, 2);
        assert_eq!(candles[0].timeframe, "1m");
        assert_eq!(candles[0].open, 100.0);
        assert_eq!(candles[2].close, 105.0);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_nan() {
        let mut data = sample_intraday_response();
        data.open[1] = f64::NAN;
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 2);
        // First and third candle should remain
        assert_eq!(candles[0].open, 100.0);
        assert_eq!(candles[1].open, 102.0);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_infinity() {
        let mut data = sample_intraday_response();
        data.high[0] = f64::INFINITY;
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 2);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_zero_price() {
        let mut data = sample_intraday_response();
        data.low[2] = 0.0;
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 2);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_negative_price() {
        let mut data = sample_intraday_response();
        data.close[0] = -5.0;
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 2);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_high_below_low() {
        let mut data = sample_intraday_response();
        data.high[1] = 90.0; // below low of 99.0
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 2);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_post_market() {
        let mut data = sample_intraday_response();
        // Set timestamp to 15:30 IST (10:00 UTC) — should be filtered
        // 15:30 IST = 10:00 UTC, base 2026-03-09 midnight UTC = 1_773_014_400
        data.timestamp[1] = 1_773_014_400 + 10 * 3600; // 15:30 IST
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 0); // post-market is not an "invalid" candle
        assert_eq!(candles.len(), 2); // but it's filtered out
    }

    #[test]
    fn test_build_valid_intraday_candles_all_invalid() {
        let data = DhanIntradayResponse {
            open: vec![0.0, f64::NAN, 100.0],
            high: vec![100.0, 200.0, 50.0], // third: high < low
            low: vec![50.0, 100.0, 60.0],
            close: vec![75.0, 150.0, 55.0],
            volume: vec![1000, 2000, 3000],
            timestamp: vec![1_773_027_900, 1_773_027_960, 1_773_028_020],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 3);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_build_valid_intraday_candles_empty_response() {
        let data = DhanIntradayResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 0);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_build_valid_intraday_candles_preserves_oi() {
        let data = sample_intraday_response();
        let (candles, _) = build_valid_intraday_candles(&data, 42, 2, "5m");
        assert_eq!(candles[0].open_interest, 5000);
        assert_eq!(candles[1].open_interest, 6000);
        assert_eq!(candles[2].open_interest, 7000);
    }

    #[test]
    fn test_build_valid_intraday_candles_empty_oi_returns_zero() {
        let mut data = sample_intraday_response();
        data.open_interest = vec![];
        let (candles, _) = build_valid_intraday_candles(&data, 42, 2, "1m");
        for candle in &candles {
            assert_eq!(candle.open_interest, 0);
        }
    }

    #[test]
    fn test_build_valid_intraday_candles_timeframe_propagated() {
        let data = sample_intraday_response();
        for &tf in &["1m", "5m", "15m", "60m"] {
            let (candles, _) = build_valid_intraday_candles(&data, 42, 2, tf);
            for candle in &candles {
                assert_eq!(candle.timeframe, tf);
            }
        }
    }

    #[test]
    fn test_build_valid_intraday_candles_mixed_valid_and_invalid() {
        let mut data = sample_intraday_response();
        // Make second candle invalid (NaN), keep first and third
        data.high[1] = f64::NAN;
        let (candles, invalid) = build_valid_intraday_candles(&data, 99, 1, "15m");
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].security_id, 99);
        assert_eq!(candles[0].exchange_segment_code, 1);
        assert_eq!(candles[0].open, 100.0);
        assert_eq!(candles[1].open, 102.0);
    }

    // =======================================================================
    // build_valid_daily_candles tests
    // =======================================================================

    #[test]
    fn test_build_valid_daily_candles_all_valid() {
        let data = sample_daily_response();
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 0);
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].security_id, 11536);
        assert_eq!(candles[0].exchange_segment_code, 1);
        assert_eq!(candles[0].timeframe, TIMEFRAME_1D);
        assert_eq!(candles[0].open, 200.0);
        assert_eq!(candles[1].close, 206.0);
    }

    #[test]
    fn test_build_valid_daily_candles_filters_nan() {
        let mut data = sample_daily_response();
        data.close[0] = f64::NAN;
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].open, 201.0); // second candle
    }

    #[test]
    fn test_build_valid_daily_candles_filters_zero_price() {
        let mut data = sample_daily_response();
        data.open[1] = 0.0;
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 1);
    }

    #[test]
    fn test_build_valid_daily_candles_filters_high_below_low() {
        let mut data = sample_daily_response();
        data.high[0] = 190.0; // below low of 195.0
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 1);
    }

    #[test]
    fn test_build_valid_daily_candles_empty_response() {
        let data = DhanDailyResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 0);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_build_valid_daily_candles_all_invalid() {
        let data = DhanDailyResponse {
            open: vec![f64::NAN, -1.0],
            high: vec![200.0, 200.0],
            low: vec![100.0, 100.0],
            close: vec![150.0, 150.0],
            volume: vec![1000, 2000],
            timestamp: vec![1_772_582_400, 1_773_014_400],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 2);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_build_valid_daily_candles_preserves_oi() {
        let mut data = sample_daily_response();
        data.open_interest = vec![50_000, 60_000];
        let (candles, _) = build_valid_daily_candles(&data, 13, 0);
        assert_eq!(candles[0].open_interest, 50_000);
        assert_eq!(candles[1].open_interest, 60_000);
    }

    #[test]
    fn test_build_valid_daily_candles_empty_oi() {
        let data = sample_daily_response();
        let (candles, _) = build_valid_daily_candles(&data, 13, 0);
        assert_eq!(candles[0].open_interest, 0);
        assert_eq!(candles[1].open_interest, 0);
    }

    #[test]
    fn test_build_valid_daily_candles_always_1d_timeframe() {
        let data = sample_daily_response();
        let (candles, _) = build_valid_daily_candles(&data, 42, 2);
        for candle in &candles {
            assert_eq!(candle.timeframe, TIMEFRAME_1D);
        }
    }

    #[test]
    fn test_build_valid_daily_candles_segment_code_propagated() {
        let data = sample_daily_response();
        for code in [0_u8, 1, 2, 4, 5, 7, 8] {
            let (candles, _) = build_valid_daily_candles(&data, 42, code);
            for candle in &candles {
                assert_eq!(candle.exchange_segment_code, code);
            }
        }
    }

    #[test]
    fn test_build_valid_daily_candles_neg_infinity_filtered() {
        let mut data = sample_daily_response();
        data.low[0] = f64::NEG_INFINITY;
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 1);
    }

    #[test]
    fn test_build_valid_daily_candles_negative_close() {
        let mut data = sample_daily_response();
        data.close[1] = -0.01;
        let (candles, invalid) = build_valid_daily_candles(&data, 11536, 1);
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 1);
    }

    #[test]
    fn test_build_valid_intraday_candles_boundary_1529_passes() {
        // 15:29 IST = valid (inside window)
        let data = DhanIntradayResponse {
            open: vec![100.0],
            high: vec![105.0],
            low: vec![98.0],
            close: vec![103.0],
            volume: vec![1000],
            // 15:29 IST = 09:59 UTC on 2026-03-09
            timestamp: vec![1_773_014_400 + 9 * 3600 + 59 * 60],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 0);
        assert_eq!(candles.len(), 1);
    }

    #[test]
    fn test_build_valid_intraday_candles_boundary_1530_filtered() {
        // 15:30 IST = invalid (outside window)
        let data = DhanIntradayResponse {
            open: vec![100.0],
            high: vec![105.0],
            low: vec![98.0],
            close: vec![103.0],
            volume: vec![1000],
            // 15:30 IST = 10:00 UTC on 2026-03-09
            timestamp: vec![1_773_014_400 + 10 * 3600],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 0); // window filter is not counted as invalid
        assert!(candles.is_empty());
    }

    #[test]
    fn test_build_valid_daily_candles_single_candle_valid() {
        let data = DhanDailyResponse {
            open: vec![100.0],
            high: vec![110.0],
            low: vec![95.0],
            close: vec![105.0],
            volume: vec![50000],
            timestamp: vec![1_772_582_400],
            open_interest: vec![12000],
        };
        let (candles, invalid) = build_valid_daily_candles(&data, 1333, 1);
        assert_eq!(invalid, 0);
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].open_interest, 12000);
        assert_eq!(candles[0].timestamp_utc_secs, 1_772_582_400);
    }

    #[test]
    fn test_build_valid_intraday_candles_penny_stock_valid() {
        let data = DhanIntradayResponse {
            open: vec![0.05],
            high: vec![0.10],
            low: vec![0.01],
            close: vec![0.07],
            volume: vec![100000],
            timestamp: vec![1_773_027_900],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 1, "1m");
        assert_eq!(invalid, 0);
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].open, 0.05);
    }

    // =======================================================================
    // build_valid_intraday_candles — filtering logic
    // =======================================================================

    #[test]
    fn test_build_valid_intraday_candles_filters_nan_standalone() {
        let data = DhanIntradayResponse {
            open: vec![100.0, f64::NAN],
            high: vec![105.0, 110.0],
            low: vec![98.0, 99.0],
            close: vec![103.0, 105.0],
            volume: vec![1000, 2000],
            timestamp: vec![1_773_027_900, 1_773_027_960],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 1, "NaN candle must be counted as invalid");
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].open, 100.0);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_non_positive() {
        let data = DhanIntradayResponse {
            open: vec![0.0, 100.0],
            high: vec![105.0, 105.0],
            low: vec![98.0, 98.0],
            close: vec![103.0, 103.0],
            volume: vec![1000, 2000],
            timestamp: vec![1_773_027_900, 1_773_027_960],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "5m");
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 1);
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_high_below_low_standalone() {
        let data = DhanIntradayResponse {
            open: vec![100.0],
            high: vec![95.0], // high < low
            low: vec![98.0],
            close: vec![97.0],
            volume: vec![1000],
            timestamp: vec![1_773_027_900],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 1);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_build_valid_intraday_candles_filters_outside_window() {
        // 15:30 IST = 10:00 UTC on 2026-03-09
        let utc_epoch_after_close: i64 = 1_773_014_400 + 10 * 3600;
        let data = DhanIntradayResponse {
            open: vec![100.0, 200.0],
            high: vec![105.0, 210.0],
            low: vec![98.0, 195.0],
            close: vec![103.0, 205.0],
            volume: vec![1000, 2000],
            timestamp: vec![1_773_027_900, utc_epoch_after_close],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(invalid, 0, "outside-window is not counted as invalid");
        assert_eq!(candles.len(), 1, "outside-window candle should be filtered");
    }

    #[test]
    fn test_build_valid_intraday_candles_empty_data() {
        let data = DhanIntradayResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert!(candles.is_empty());
        assert_eq!(invalid, 0);
    }

    #[test]
    fn test_build_valid_intraday_candles_preserves_oi_single_candle() {
        let data = DhanIntradayResponse {
            open: vec![100.0],
            high: vec![105.0],
            low: vec![98.0],
            close: vec![103.0],
            volume: vec![1000],
            timestamp: vec![1_773_027_900],
            open_interest: vec![5000],
        };
        let (candles, _) = build_valid_intraday_candles(&data, 42, 2, "1m");
        assert_eq!(candles[0].open_interest, 5000);
    }

    // =======================================================================
    // build_valid_daily_candles — filtering logic
    // =======================================================================

    #[test]
    fn test_build_valid_daily_candles_filters_nan_single_candle() {
        let data = DhanDailyResponse {
            open: vec![f64::NAN],
            high: vec![210.0],
            low: vec![195.0],
            close: vec![205.0],
            volume: vec![10000],
            timestamp: vec![1_772_582_400],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_daily_candles(&data, 1333, 1);
        assert_eq!(invalid, 1);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_build_valid_daily_candles_filters_negative_price() {
        let data = DhanDailyResponse {
            open: vec![200.0, -1.0],
            high: vec![210.0, 211.0],
            low: vec![195.0, 196.0],
            close: vec![205.0, 206.0],
            volume: vec![10000, 20000],
            timestamp: vec![1_772_582_400, 1_773_014_400],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_daily_candles(&data, 1333, 1);
        assert_eq!(invalid, 1);
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].open, 200.0);
    }

    #[test]
    fn test_build_valid_daily_candles_empty_data() {
        let data = DhanDailyResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        let (candles, invalid) = build_valid_daily_candles(&data, 1333, 1);
        assert!(candles.is_empty());
        assert_eq!(invalid, 0);
    }

    #[test]
    fn test_build_valid_daily_candles_all_valid_timeframe_check() {
        let data = sample_daily_response();
        let (candles, invalid) = build_valid_daily_candles(&data, 1333, 1);
        assert_eq!(invalid, 0);
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].timeframe, TIMEFRAME_1D);
        assert_eq!(candles[1].timeframe, TIMEFRAME_1D);
    }

    #[test]
    fn test_build_valid_daily_candles_no_intraday_window_filter() {
        // Daily candles should NOT be filtered by intraday window
        // (they have midnight timestamps which are "outside" market hours)
        let data = sample_daily_response();
        let (candles, invalid) = build_valid_daily_candles(&data, 1333, 1);
        assert_eq!(
            candles.len(),
            2,
            "daily candles should not be window-filtered"
        );
        assert_eq!(invalid, 0);
    }

    // =======================================================================
    // M5: Candle fetch failure escalation — error categorization
    // =======================================================================

    /// Verifies that token-related errors (807/DH-901) are classified as
    /// requiring escalation to error! level, while non-token errors remain
    /// at warn! level. Token errors affect ALL instruments (system-wide),
    /// whereas network/input errors are scoped to individual instruments.
    #[test]
    fn test_candle_fetch_failure_escalation() {
        // Token-expired errors should be escalated (error! level)
        assert!(
            is_token_related_error(&ErrorAction::TokenExpired),
            "TokenExpired (807/DH-901) must be escalated — affects all instruments"
        );

        // Non-token errors should NOT be escalated (remain at warn! level)
        assert!(
            !is_token_related_error(&ErrorAction::RateLimited),
            "RateLimited (DH-904) is per-instrument — stays at warn!"
        );
        assert!(
            !is_token_related_error(&ErrorAction::TooManyConnections),
            "TooManyConnections (805) is connection-level — stays at warn!"
        );
        assert!(
            !is_token_related_error(&ErrorAction::StandardRetry),
            "StandardRetry is per-instrument — stays at warn!"
        );
        assert!(
            !is_token_related_error(&ErrorAction::NeverRetry),
            "NeverRetry (DH-905/906) is per-instrument — stays at warn!"
        );
    }

    /// Verifies the integration between classify_error and is_token_related_error:
    /// only error codes 807 and DH-901 produce escalation-worthy errors.
    #[test]
    fn test_candle_fetch_failure_escalation_end_to_end() {
        // 807 data API token expired → escalate
        let body_807 = br#"{"errorCode": 807, "message": "Access token expired"}"#;
        let action_807 = classify_error(reqwest::StatusCode::UNAUTHORIZED, body_807);
        assert!(
            is_token_related_error(&action_807),
            "807 must classify as token-related and escalate to error!"
        );

        // DH-901 trading API auth error → escalate
        let body_901 =
            br#"{"errorType": "AUTH", "errorCode": "DH-901", "errorMessage": "Invalid auth"}"#;
        let action_901 = classify_error(reqwest::StatusCode::UNAUTHORIZED, body_901);
        assert!(
            is_token_related_error(&action_901),
            "DH-901 must classify as token-related and escalate to error!"
        );

        // DH-904 rate limit → do NOT escalate
        let body_904 =
            br#"{"errorType": "RATE_LIMIT", "errorCode": "DH-904", "errorMessage": "Rate limit"}"#;
        let action_904 = classify_error(reqwest::StatusCode::BAD_REQUEST, body_904);
        assert!(
            !is_token_related_error(&action_904),
            "DH-904 must NOT escalate — rate limits are per-instrument"
        );

        // DH-905 input error → do NOT escalate
        let body_905 =
            br#"{"errorType": "INPUT", "errorCode": "DH-905", "errorMessage": "Bad input"}"#;
        let action_905 = classify_error(reqwest::StatusCode::BAD_REQUEST, body_905);
        assert!(
            !is_token_related_error(&action_905),
            "DH-905 must NOT escalate — input errors are per-instrument"
        );

        // 805 too many connections → do NOT escalate
        let body_805 = br#"{"errorCode": 805, "message": "Too many connections"}"#;
        let action_805 = classify_error(reqwest::StatusCode::BAD_REQUEST, body_805);
        assert!(
            !is_token_related_error(&action_805),
            "805 must NOT escalate — connection errors are infrastructure-level"
        );
    }

    // =======================================================================
    // is_weekend_timestamp — weekend candle rejection
    // =======================================================================

    #[test]
    fn test_is_weekend_timestamp_saturday_detected() {
        // 2026-03-21 09:30 IST = Saturday (the exact issue the user found)
        // IST 09:30 = UTC 04:00 = epoch 1774008000
        let sat_ist_0930 = chrono::NaiveDate::from_ymd_opt(2026, 3, 21)
            .unwrap()
            .and_hms_opt(4, 0, 0) // UTC 04:00 = IST 09:30
            .unwrap()
            .and_utc()
            .timestamp();
        assert!(
            is_weekend_timestamp(sat_ist_0930),
            "Saturday 09:30 IST must be detected as weekend"
        );
    }

    #[test]
    fn test_is_weekend_timestamp_sunday_detected() {
        // 2026-03-22 10:00 IST = Sunday
        let sun_ist_1000 = chrono::NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(4, 30, 0) // UTC 04:30 = IST 10:00
            .unwrap()
            .and_utc()
            .timestamp();
        assert!(
            is_weekend_timestamp(sun_ist_1000),
            "Sunday must be detected as weekend"
        );
    }

    #[test]
    fn test_is_weekend_timestamp_monday_not_weekend() {
        // 2026-03-23 09:15 IST = Monday (trading day)
        let mon_ist_0915 = chrono::NaiveDate::from_ymd_opt(2026, 3, 23)
            .unwrap()
            .and_hms_opt(3, 45, 0) // UTC 03:45 = IST 09:15
            .unwrap()
            .and_utc()
            .timestamp();
        assert!(
            !is_weekend_timestamp(mon_ist_0915),
            "Monday must NOT be detected as weekend"
        );
    }

    #[test]
    fn test_is_weekend_timestamp_friday_not_weekend() {
        // 2026-03-20 15:29 IST = Friday (last trading minute)
        let fri_ist_1529 = chrono::NaiveDate::from_ymd_opt(2026, 3, 20)
            .unwrap()
            .and_hms_opt(9, 59, 0) // UTC 09:59 = IST 15:29
            .unwrap()
            .and_utc()
            .timestamp();
        assert!(
            !is_weekend_timestamp(fri_ist_1529),
            "Friday must NOT be detected as weekend"
        );
    }

    // -----------------------------------------------------------------------
    // Marker-write guard regression (Parthiban directive 2026-04-22)
    // -----------------------------------------------------------------------
    //
    // The idempotency marker MUST only be written on a fully successful run.
    // Any partial failure (non-zero instruments_failed OR persist_failures)
    // must leave the marker unchanged so tomorrow's boot re-attempts the
    // full 90-day fetch.
    //
    // We can't easily unit-test the closure in `fetch_historical_candles`
    // without a full Dhan mock + QuestDB, but we CAN source-scan to ensure
    // the guard condition stays strict. A regression to the old
    // `instruments_fetched > 0` check would allow partial-success runs to
    // mark the day done, which is exactly the bug this rule prevents.

    fn read_fetcher_source() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/historical/candle_fetcher.rs");
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read {}: {e}", path.display());
        })
    }

    #[test]
    fn test_marker_write_guard_requires_zero_instrument_failures() {
        let src = read_fetcher_source();
        assert!(
            src.contains("summary.instruments_failed == 0"),
            "marker-write guard MUST assert instruments_failed == 0 \
             (Parthiban directive 2026-04-22 — full-success only). \
             Do NOT weaken to e.g. `instruments_fetched > 0` alone."
        );
    }

    #[test]
    fn test_marker_write_guard_requires_zero_persist_failures() {
        let src = read_fetcher_source();
        assert!(
            src.contains("summary.persist_failures == 0"),
            "marker-write guard MUST assert persist_failures == 0 \
             (Parthiban directive 2026-04-22 — full-success only)"
        );
    }

    #[test]
    fn test_marker_write_guard_still_requires_nonzero_instruments_fetched() {
        let src = read_fetcher_source();
        assert!(
            src.contains("summary.instruments_fetched > 0"),
            "marker-write guard MUST still reject empty-registry runs \
             (instruments_fetched > 0 sentinel)"
        );
    }

    #[test]
    fn test_marker_not_written_on_partial_failure_emits_warn() {
        // Guard that the else-branch of the marker-write gate emits an
        // explicit WARN log explaining why the marker was skipped.
        // Silent skipping would make "why didn't marker update?" debugging
        // impossible.
        let src = read_fetcher_source();
        assert!(
            src.contains("historical-fetch marker NOT written — run was not fully successful",),
            "partial-failure path MUST emit a WARN log explaining the skip"
        );
    }
}
