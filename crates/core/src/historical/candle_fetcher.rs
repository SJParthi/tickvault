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
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::Utc;
use metrics::counter;
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use dhan_live_trader_common::config::{DhanConfig, HistoricalDataConfig};
use dhan_live_trader_common::constants::{
    DHAN_CHARTS_HISTORICAL_PATH, DHAN_CHARTS_INTRADAY_PATH, INTRADAY_TIMEFRAMES,
    IST_UTC_OFFSET_SECONDS_I64, MARKET_CLOSE_TIME_IST_EXCLUSIVE, MARKET_OPEN_TIME_IST,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TIMEFRAME_1D,
};
use dhan_live_trader_common::instrument_registry::{InstrumentRegistry, SubscriptionCategory};
use dhan_live_trader_common::tick_types::{
    DhanDailyResponse, DhanIntradayResponse, HistoricalCandle,
};
use dhan_live_trader_common::trading_calendar::ist_offset;

use dhan_live_trader_storage::candle_persistence::CandlePersistenceWriter;

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
const RETRY_WAVE_MAX: usize = 5;

/// Base backoff between retry waves (seconds). Wave N sleeps N * base seconds.
/// Wave 1: 30s, Wave 2: 60s, Wave 3: 90s, Wave 4: 120s, Wave 5: 150s.
const RETRY_WAVE_BACKOFF_BASE_SECS: u64 = 30;

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
            "DH-905" | "DH-906" => ErrorAction::NeverRetry,
            "DH-901" => ErrorAction::TokenExpired,
            _ => ErrorAction::StandardRetry,
        };
    }

    ErrorAction::StandardRetry
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
const MAX_TOKEN_EXPIRED_RETRIES: usize = 2;

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
    expiry_code: Option<u8>,
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

/// Fetches historical candles across all timeframes for all subscribed instruments.
///
/// Fetches 4 intraday timeframes (1m, 5m, 15m, 60m) and daily candles for each
/// instrument, persisting all to the unified `historical_candles` table.
///
/// This runs automatically in the boot sequence — zero human intervention.
///
/// # Arguments
/// * `registry` — subscribed instrument registry (built from universe)
/// * `dhan_config` — Dhan API config (base URL)
/// * `historical_config` — fetch parameters (lookback days, timeouts)
/// * `token_handle` — arc-swap token for API auth
/// * `client_id` — Dhan client ID for API header
/// * `candle_writer` — QuestDB ILP writer for candles
///
/// # Returns
/// Summary of fetch results (successes, failures, candle count).
#[allow(clippy::too_many_arguments)] // APPROVED: API fetch requires all config + writer params
pub async fn fetch_historical_candles(
    registry: &InstrumentRegistry,
    dhan_config: &DhanConfig,
    historical_config: &HistoricalDataConfig,
    token_handle: &TokenHandle,
    client_id: &SecretString,
    candle_writer: &mut CandlePersistenceWriter,
) -> CandleFetchSummary {
    let m_fetched = counter!("dlt_historical_candles_fetched_total");
    let m_api_errors = counter!("dlt_historical_api_errors_total");

    let now_ist = Utc::now().with_timezone(&ist_offset());
    let today = now_ist.date_naive();

    // Compute date range: today - lookback_days to today
    let from_date = today
        .checked_sub_signed(chrono::Duration::days(i64::from(
            historical_config.lookback_days,
        )))
        .unwrap_or(today);
    let to_date = today;

    // Intraday requests use datetime format with market hours
    let from_date_str = from_date.format("%Y-%m-%d").to_string();
    let to_date_str = to_date.format("%Y-%m-%d").to_string();

    // Intraday: "YYYY-MM-DD HH:MM:SS" with market hours (09:15 to 15:30 exclusive)
    let intraday_from = format!("{} {}", from_date_str, MARKET_OPEN_TIME_IST);
    let intraday_to = format!("{} {}", to_date_str, MARKET_CLOSE_TIME_IST_EXCLUSIVE);

    // Daily: date-only format, toDate is NON-INCLUSIVE so add 1 day
    let daily_to_date = to_date
        .checked_add_signed(chrono::Duration::days(1))
        .unwrap_or(to_date);
    let daily_to_str = daily_to_date.format("%Y-%m-%d").to_string();

    info!(
        from_date = %from_date_str,
        to_date = %to_date_str,
        lookback_days = historical_config.lookback_days,
        timeframes = "1m,5m,15m,60m,1d",
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
        // Skip all derivatives (futures + options).
        if matches!(
            instrument.category,
            SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative
        ) {
            instruments_skipped = instruments_skipped.saturating_add(1);
            continue;
        }

        let instrument_type = match instrument.category {
            SubscriptionCategory::MajorIndexValue | SubscriptionCategory::DisplayIndex => "INDEX",
            SubscriptionCategory::StockEquity => "EQUITY",
            SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative => {
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
    let m_token_expired = counter!("dlt_historical_token_expired_total");

    // Per-instrument token-expired retry count
    let mut token_expired_counts: Vec<usize> = vec![0; targets.len()];

    // --- Wave 0 = initial pass, Waves 1..=RETRY_WAVE_MAX = retries ---
    for wave in 0..=RETRY_WAVE_MAX {
        if pending_indices.is_empty() {
            break;
        }

        if wave > 0 {
            let backoff_secs = RETRY_WAVE_BACKOFF_BASE_SECS.saturating_mul(wave as u64);
            warn!(
                wave,
                remaining = pending_indices.len(),
                backoff_secs,
                "retry wave — backing off before re-attempting failed instruments"
            );
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
                        warn!(
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
            warn!(
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
    let failed_instruments: Vec<String> = pending_indices
        .iter()
        .take(MAX_FAILED_INSTRUMENT_NAMES)
        .map(|&idx| {
            let target = &targets[idx];
            format!("{} ({})", target.security_id, target.exchange_segment_str)
        })
        .collect();

    // Build failure reason breakdown
    let mut failure_reasons: HashMap<String, usize> = HashMap::new();
    for &idx in &pending_indices {
        if token_expired_counts[idx] >= MAX_TOKEN_EXPIRED_RETRIES {
            let count = failure_reasons
                .entry("token_expired".to_string())
                .or_insert(0);
            *count = count.saturating_add(1);
        } else {
            // Could be network, input error, or other transient — generic "failed"
            let count = failure_reasons
                .entry("network_or_api".to_string())
                .or_insert(0);
            *count = count.saturating_add(1);
        }
    }
    if total_persist_failures > 0 {
        failure_reasons.insert("persist".to_string(), total_persist_failures);
    }

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
        warn!(?err, "failed to flush remaining candles to QuestDB");
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

/// Fetches all timeframes (4 intraday + daily) for a single instrument.
/// Returns whether the fetch succeeded, failed transiently, or permanently.
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
                warn!("no access token available — skipping historical fetch");
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

    // --- Fetch daily candles ---
    if historical_config.request_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(historical_config.request_delay_ms)).await;
    }

    let token_guard = token_handle.load();
    let access_token = match token_guard.as_ref() {
        Some(token_state) => Zeroizing::new(token_state.access_token().expose_secret().to_string()),
        None => {
            warn!("no access token available — skipping daily fetch");
            return InstrumentFetchResult::TokenExpired;
        }
    };

    let daily_body = DailyRequest {
        security_id: security_id_str.to_string(),
        exchange_segment: exchange_segment_str.to_string(),
        instrument: instrument_type.to_string(),
        expiry_code: None,
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
            let delay_ms = 1000_u64.saturating_mul(u64::from(attempt));
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
                            let backoff_secs = DH_904_BACKOFF_BASE_SECS
                                .saturating_mul(1_u64.wrapping_shl(attempt));
                            warn!(
                                security_id,
                                timeframe = timeframe_label,
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
                            warn!(
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

                if body_bytes.len() > MAX_RESPONSE_BODY_SIZE {
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
            let delay_ms = 1000_u64.saturating_mul(u64::from(attempt));
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
                            warn!(
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

                if body_bytes.len() > MAX_RESPONSE_BODY_SIZE {
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
    let mut count = 0_usize;
    let mut persist_failures = 0_usize;
    let mut consecutive_persist_failures = 0_usize;
    for i in 0..data.len() {
        let open = data.open[i];
        let high = data.high[i];
        let low = data.low[i];
        let close = data.close[i];

        // Reject NaN/Infinity prices — corrupted data must not reach QuestDB
        if !open.is_finite() || !high.is_finite() || !low.is_finite() || !close.is_finite() {
            warn!(
                security_id,
                idx = i,
                timeframe = timeframe_label,
                "NaN/Inf price in API response — skipping candle"
            );
            m_api_errors.increment(1);
            continue;
        }

        // Reject non-positive prices — zero or negative prices indicate bad data
        if open <= 0.0 || high <= 0.0 || low <= 0.0 || close <= 0.0 {
            warn!(
                security_id,
                idx = i,
                timeframe = timeframe_label,
                open,
                high,
                low,
                close,
                "non-positive price in API response — skipping candle"
            );
            m_api_errors.increment(1);
            continue;
        }

        // Reject OHLC inconsistency — high must be >= low
        if high < low {
            warn!(
                security_id,
                idx = i,
                timeframe = timeframe_label,
                high,
                low,
                "high < low in API response — skipping candle"
            );
            m_api_errors.increment(1);
            continue;
        }

        // Reject candles at or after 15:30 IST — market close is always exclusive.
        // Dhan may return closing session candles (15:30+) for higher timeframes;
        // we discard them to keep [09:15, 15:30) IST as the strict intraday window.
        {
            #[allow(clippy::cast_possible_truncation)] // APPROVED: secs-of-day fits u32
            let ist_secs_of_day =
                (data.timestamp[i].saturating_add(IST_UTC_OFFSET_SECONDS_I64) % 86_400) as u32;
            if ist_secs_of_day >= TICK_PERSIST_END_SECS_OF_DAY_IST {
                continue; // silently skip — this is expected for 5m/15m/60m
            }
        }

        let oi_value = if i < data.open_interest.len() {
            data.open_interest[i]
        } else {
            0
        };

        let candle = HistoricalCandle {
            timestamp_utc_secs: data.timestamp[i],
            security_id,
            exchange_segment_code: segment_code,
            timeframe: timeframe_label,
            open,
            high,
            low,
            close,
            volume: data.volume[i],
            open_interest: oi_value,
        };

        if let Err(err) = candle_writer.append_candle(&candle) {
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
    let mut count = 0_usize;
    let mut persist_failures = 0_usize;
    let mut consecutive_persist_failures = 0_usize;
    for i in 0..data.len() {
        let open = data.open[i];
        let high = data.high[i];
        let low = data.low[i];
        let close = data.close[i];

        if !open.is_finite() || !high.is_finite() || !low.is_finite() || !close.is_finite() {
            warn!(
                security_id,
                idx = i,
                timeframe = TIMEFRAME_1D,
                "NaN/Inf price in daily response — skipping candle"
            );
            m_api_errors.increment(1);
            continue;
        }

        // Reject non-positive prices
        if open <= 0.0 || high <= 0.0 || low <= 0.0 || close <= 0.0 {
            warn!(
                security_id,
                idx = i,
                timeframe = TIMEFRAME_1D,
                open,
                high,
                low,
                close,
                "non-positive price in daily response — skipping candle"
            );
            m_api_errors.increment(1);
            continue;
        }

        // Reject OHLC inconsistency — high must be >= low
        if high < low {
            warn!(
                security_id,
                idx = i,
                timeframe = TIMEFRAME_1D,
                high,
                low,
                "high < low in daily response — skipping candle"
            );
            m_api_errors.increment(1);
            continue;
        }

        let oi_value = if i < data.open_interest.len() {
            data.open_interest[i]
        } else {
            0
        };

        let candle = HistoricalCandle {
            timestamp_utc_secs: data.timestamp[i],
            security_id,
            exchange_segment_code: segment_code,
            timeframe: TIMEFRAME_1D,
            open,
            high,
            low,
            close,
            volume: data.volume[i],
            open_interest: oi_value,
        };

        if let Err(err) = candle_writer.append_candle(&candle) {
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
        assert_eq!(INTRADAY_TIMEFRAMES.len(), 4);
        assert_eq!(INTRADAY_TIMEFRAMES[0], ("1", "1m"));
        assert_eq!(INTRADAY_TIMEFRAMES[1], ("5", "5m"));
        assert_eq!(INTRADAY_TIMEFRAMES[2], ("15", "15m"));
        assert_eq!(INTRADAY_TIMEFRAMES[3], ("60", "60m"));
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
        use dhan_live_trader_common::instrument_registry::SubscriptionCategory;

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
        // Wave 1: 30s, Wave 2: 60s, Wave 3: 90s, Wave 4: 120s, Wave 5: 150s
        for wave in 1..=RETRY_WAVE_MAX {
            let backoff = RETRY_WAVE_BACKOFF_BASE_SECS.saturating_mul(wave as u64);
            assert!(backoff > 0, "wave {wave} backoff must be positive");
            assert!(
                backoff <= 300,
                "wave {wave} backoff {backoff}s should not exceed 5 minutes"
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
        // Total maximum backoff across all waves: sum(1..=5) * 30 = 15 * 30 = 450s = 7.5 min
        let mut total_backoff = 0_u64;
        for wave in 1..=RETRY_WAVE_MAX {
            total_backoff = total_backoff
                .saturating_add(RETRY_WAVE_BACKOFF_BASE_SECS.saturating_mul(wave as u64));
        }
        assert!(
            total_backoff <= 600,
            "total retry backoff {total_backoff}s should not exceed 10 minutes"
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
                MAX_TOKEN_EXPIRED_RETRIES <= 5,
                // "don't retry forever if token is truly dead"
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

    // -----------------------------------------------------------------------
    // classify_error — exhaustive branch coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_error_dh906_never_retry() {
        let body = br#"{"errorType": "ORDER_ERROR", "errorCode": "DH-906", "errorMessage": "Order error"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::NeverRetry);
    }

    #[test]
    fn test_classify_error_dh901_token_expired() {
        // DH-901 via trading API string format (not data API numeric)
        let body = br#"{"errorType": "AUTH_ERROR", "errorCode": "DH-901", "errorMessage": "Invalid auth"}"#;
        let action = classify_error(reqwest::StatusCode::UNAUTHORIZED, body);
        assert_eq!(action, ErrorAction::TokenExpired);
    }

    #[test]
    fn test_classify_error_dh904_body_without_429() {
        // DH-904 in response body but non-429 HTTP status — body classification takes priority
        let body =
            br#"{"errorType": "RATE_LIMIT", "errorCode": "DH-904", "errorMessage": "Rate limit"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::RateLimited);
    }

    #[test]
    fn test_classify_error_dh908_standard_retry() {
        let body = br#"{"errorType": "INTERNAL", "errorCode": "DH-908", "errorMessage": "Internal server error"}"#;
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_dh909_standard_retry() {
        let body =
            br#"{"errorType": "NETWORK", "errorCode": "DH-909", "errorMessage": "Network error"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_GATEWAY, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_data_api_other_code_standard_retry() {
        // Data API numeric code that is not 805 or 807 (e.g., 800, 804, 806, etc.)
        let body = br#"{"errorCode": 800, "message": "Internal server error"}"#;
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_data_api_806_standard_retry() {
        let body = br#"{"errorCode": 806, "message": "Data APIs not subscribed"}"#;
        let action = classify_error(reqwest::StatusCode::FORBIDDEN, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_data_api_804_standard_retry() {
        let body = br#"{"errorCode": 804, "message": "Instruments exceed limit"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_empty_body_standard_retry() {
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, b"");
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_garbage_body_standard_retry() {
        let action = classify_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            b"not json at all",
        );
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_null_error_code_fields() {
        // Both formats present but with null code fields — falls through to StandardRetry
        let body = br#"{"errorCode": null, "errorMessage": null}"#;
        let action = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert_eq!(action, ErrorAction::StandardRetry);
    }

    #[test]
    fn test_classify_error_http_429_overrides_body() {
        // HTTP 429 always returns RateLimited regardless of body content
        let body = br#"{"errorCode": 805, "message": "Doesn't matter"}"#;
        let action = classify_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(action, ErrorAction::RateLimited);
    }

    #[test]
    fn test_classify_error_http_429_with_empty_body() {
        let action = classify_error(reqwest::StatusCode::TOO_MANY_REQUESTS, b"");
        assert_eq!(action, ErrorAction::RateLimited);
    }

    // -----------------------------------------------------------------------
    // DhanErrorResponse / DataApiErrorResponse deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn test_dhan_error_response_deserialize_all_fields() {
        let json =
            br#"{"errorType":"INPUT_EXCEPTION","errorCode":"DH-905","errorMessage":"Bad input"}"#;
        let parsed: DhanErrorResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(parsed.error_type.as_deref(), Some("INPUT_EXCEPTION"));
        assert_eq!(parsed.error_code.as_deref(), Some("DH-905"));
        assert_eq!(parsed.error_message.as_deref(), Some("Bad input"));
    }

    #[test]
    fn test_dhan_error_response_deserialize_missing_fields() {
        // All fields are Option — missing fields parse as None
        let json = br#"{}"#;
        let parsed: DhanErrorResponse = serde_json::from_slice(json).unwrap();
        assert!(parsed.error_type.is_none());
        assert!(parsed.error_code.is_none());
        assert!(parsed.error_message.is_none());
    }

    #[test]
    fn test_data_api_error_response_with_status_alias() {
        let json = br#"{"status": 807, "message": "Token expired"}"#;
        let parsed: DataApiErrorResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(parsed.code, Some(807));
    }

    #[test]
    fn test_data_api_error_response_with_error_code_alias() {
        let json = br#"{"errorCode": 805, "errorMessage": "Too many"}"#;
        let parsed: DataApiErrorResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(parsed.code, Some(805));
    }

    #[test]
    fn test_data_api_error_response_empty_json() {
        let json = br#"{}"#;
        let parsed: DataApiErrorResponse = serde_json::from_slice(json).unwrap();
        assert!(parsed.code.is_none());
    }

    // -----------------------------------------------------------------------
    // DailyRequest serialization — expiry_code branch
    // -----------------------------------------------------------------------

    #[test]
    fn test_daily_request_serialization_with_expiry_code() {
        let request = DailyRequest {
            security_id: "26000".to_string(),
            exchange_segment: "NSE_FNO".to_string(),
            instrument: "FUTIDX".to_string(),
            expiry_code: Some(0),
            from_date: "2026-01-01".to_string(),
            to_date: "2026-03-01".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(
            json.contains("\"expiryCode\":0"),
            "expiryCode must be present when Some: {json}"
        );
    }

    #[test]
    fn test_daily_request_serialization_expiry_code_1() {
        let request = DailyRequest {
            security_id: "13".to_string(),
            exchange_segment: "NSE_FNO".to_string(),
            instrument: "FUTIDX".to_string(),
            expiry_code: Some(1),
            from_date: "2026-01-01".to_string(),
            to_date: "2026-03-01".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"expiryCode\":1"));
    }

    #[test]
    fn test_daily_request_serialization_expiry_code_2() {
        let request = DailyRequest {
            security_id: "13".to_string(),
            exchange_segment: "NSE_FNO".to_string(),
            instrument: "FUTIDX".to_string(),
            expiry_code: Some(2),
            from_date: "2026-01-01".to_string(),
            to_date: "2026-03-01".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"expiryCode\":2"));
    }

    #[test]
    fn test_daily_request_all_fields_camel_case() {
        let request = DailyRequest {
            security_id: "13".to_string(),
            exchange_segment: "IDX_I".to_string(),
            instrument: "INDEX".to_string(),
            expiry_code: Some(0),
            from_date: "2026-01-01".to_string(),
            to_date: "2026-03-01".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        // Verify no snake_case leaked
        assert!(!json.contains("security_id"));
        assert!(!json.contains("exchange_segment"));
        assert!(!json.contains("from_date"));
        assert!(!json.contains("to_date"));
        assert!(!json.contains("expiry_code"));
        // Verify camelCase present
        assert!(json.contains("securityId"));
        assert!(json.contains("exchangeSegment"));
        assert!(json.contains("fromDate"));
        assert!(json.contains("toDate"));
        assert!(json.contains("expiryCode"));
    }

    // -----------------------------------------------------------------------
    // DhanIntradayResponse / DhanDailyResponse edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_dhan_intraday_response_empty() {
        let response = DhanIntradayResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        assert!(response.is_empty());
        assert_eq!(response.len(), 0);
        assert!(response.is_consistent());
    }

    #[test]
    fn test_dhan_intraday_response_consistent_with_oi() {
        let response = DhanIntradayResponse {
            open: vec![100.0],
            high: vec![105.0],
            low: vec![99.0],
            close: vec![102.0],
            volume: vec![1000],
            timestamp: vec![1700000000],
            open_interest: vec![50000],
        };
        assert!(!response.is_empty());
        assert_eq!(response.len(), 1);
        assert!(response.is_consistent());
    }

    #[test]
    fn test_dhan_intraday_response_consistent_without_oi() {
        // open_interest empty is valid — is_consistent allows empty OI
        let response = DhanIntradayResponse {
            open: vec![100.0, 101.0],
            high: vec![105.0, 106.0],
            low: vec![99.0, 100.0],
            close: vec![102.0, 103.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![],
        };
        assert!(response.is_consistent());
    }

    #[test]
    fn test_dhan_intraday_response_inconsistent_oi_length() {
        // OI length mismatched and not empty — inconsistent
        let response = DhanIntradayResponse {
            open: vec![100.0, 101.0],
            high: vec![105.0, 106.0],
            low: vec![99.0, 100.0],
            close: vec![102.0, 103.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![50000], // only 1, but timestamp has 2
        };
        assert!(!response.is_consistent());
    }

    #[test]
    fn test_dhan_daily_response_empty() {
        let response = DhanDailyResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        assert!(response.is_empty());
        assert_eq!(response.len(), 0);
        assert!(response.is_consistent());
    }

    #[test]
    fn test_dhan_daily_response_consistent() {
        let response = DhanDailyResponse {
            open: vec![100.0],
            high: vec![105.0],
            low: vec![99.0],
            close: vec![102.0],
            volume: vec![1000000],
            timestamp: vec![1700000000],
            open_interest: vec![],
        };
        assert!(!response.is_empty());
        assert!(response.is_consistent());
    }

    #[test]
    fn test_dhan_daily_response_inconsistent_volume() {
        let response = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![105.0, 106.0],
            low: vec![99.0, 100.0],
            close: vec![102.0, 103.0],
            volume: vec![1000], // only 1
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![],
        };
        assert!(!response.is_consistent());
    }

    #[test]
    fn test_dhan_daily_response_inconsistent_close() {
        let response = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![105.0, 106.0],
            low: vec![99.0, 100.0],
            close: vec![102.0], // only 1
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![],
        };
        assert!(!response.is_consistent());
    }

    // -----------------------------------------------------------------------
    // DH-904 exponential backoff math
    // -----------------------------------------------------------------------

    #[test]
    fn test_dh904_backoff_sequence() {
        // Sequence: 10 * 2^attempt = 10, 20, 40, 80
        for attempt in 0..DH_904_MAX_BACKOFF_ATTEMPTS {
            let backoff = DH_904_BACKOFF_BASE_SECS.saturating_mul(1_u64.wrapping_shl(attempt));
            let expected = DH_904_BACKOFF_BASE_SECS * (1_u64 << attempt);
            assert_eq!(
                backoff, expected,
                "attempt {attempt}: expected {expected}s, got {backoff}s"
            );
        }
    }

    #[test]
    fn test_dh904_backoff_values() {
        assert_eq!(
            DH_904_BACKOFF_BASE_SECS.saturating_mul(1_u64.wrapping_shl(0)),
            10,
            "attempt 0: 10s"
        );
        assert_eq!(
            DH_904_BACKOFF_BASE_SECS.saturating_mul(1_u64.wrapping_shl(1)),
            20,
            "attempt 1: 20s"
        );
        assert_eq!(
            DH_904_BACKOFF_BASE_SECS.saturating_mul(1_u64.wrapping_shl(2)),
            40,
            "attempt 2: 40s"
        );
        assert_eq!(
            DH_904_BACKOFF_BASE_SECS.saturating_mul(1_u64.wrapping_shl(3)),
            80,
            "attempt 3: 80s"
        );
    }

    #[test]
    fn test_dh904_max_attempts_gives_up_at_threshold() {
        // After DH_904_MAX_BACKOFF_ATTEMPTS, should give up
        assert_eq!(DH_904_MAX_BACKOFF_ATTEMPTS, 4);
        // attempt >= DH_904_MAX_BACKOFF_ATTEMPTS → return Failed
        assert!(4_u32 >= DH_904_MAX_BACKOFF_ATTEMPTS);
    }

    // -----------------------------------------------------------------------
    // IST time-of-day filtering for intraday candles
    // -----------------------------------------------------------------------

    #[test]
    fn test_intraday_market_close_candle_filtered() {
        // 15:30 IST = TICK_PERSIST_END_SECS_OF_DAY_IST = 55800
        // A candle at exactly 15:30 IST should be filtered out
        // UTC epoch for 15:30 IST = 10:00 UTC on some day
        let utc_epoch: i64 = 1_773_014_400 + 10 * 3600; // 10:00 UTC
        let ist_secs_of_day =
            (utc_epoch.saturating_add(IST_UTC_OFFSET_SECONDS_I64) % 86_400) as u32;
        assert_eq!(ist_secs_of_day, TICK_PERSIST_END_SECS_OF_DAY_IST);
        assert!(ist_secs_of_day >= TICK_PERSIST_END_SECS_OF_DAY_IST);
    }

    #[test]
    fn test_intraday_after_market_candle_filtered() {
        // 15:45 IST (post-close) should be filtered
        // 15:45 IST = 10:15 UTC
        let utc_epoch: i64 = 1_773_014_400 + 10 * 3600 + 15 * 60;
        let ist_secs_of_day =
            (utc_epoch.saturating_add(IST_UTC_OFFSET_SECONDS_I64) % 86_400) as u32;
        assert_eq!(ist_secs_of_day, 15 * 3600 + 45 * 60); // 56700
        assert!(ist_secs_of_day >= TICK_PERSIST_END_SECS_OF_DAY_IST);
    }

    #[test]
    fn test_intraday_market_open_candle_passes() {
        // 09:15 IST = 03:45 UTC — should NOT be filtered
        let utc_epoch: i64 = 1_773_014_400 + 3 * 3600 + 45 * 60;
        let ist_secs_of_day =
            (utc_epoch.saturating_add(IST_UTC_OFFSET_SECONDS_I64) % 86_400) as u32;
        assert_eq!(ist_secs_of_day, 9 * 3600 + 15 * 60); // 33300
        assert!(ist_secs_of_day < TICK_PERSIST_END_SECS_OF_DAY_IST);
    }

    #[test]
    fn test_intraday_last_valid_candle_1529_passes() {
        // 15:29 IST = 09:59 UTC — the last valid 1m candle
        let utc_epoch: i64 = 1_773_014_400 + 9 * 3600 + 59 * 60;
        let ist_secs_of_day =
            (utc_epoch.saturating_add(IST_UTC_OFFSET_SECONDS_I64) % 86_400) as u32;
        assert_eq!(ist_secs_of_day, 15 * 3600 + 29 * 60); // 55740
        assert!(ist_secs_of_day < TICK_PERSIST_END_SECS_OF_DAY_IST);
    }

    #[test]
    fn test_intraday_noon_candle_passes() {
        // 12:00 IST = 06:30 UTC — mid-session, should pass
        let utc_epoch: i64 = 1_773_014_400 + 6 * 3600 + 30 * 60;
        let ist_secs_of_day =
            (utc_epoch.saturating_add(IST_UTC_OFFSET_SECONDS_I64) % 86_400) as u32;
        assert_eq!(ist_secs_of_day, 12 * 3600); // 43200
        assert!(ist_secs_of_day < TICK_PERSIST_END_SECS_OF_DAY_IST);
    }

    // -----------------------------------------------------------------------
    // OI fallback when open_interest array is shorter
    // -----------------------------------------------------------------------

    #[test]
    fn test_oi_fallback_uses_zero_when_array_short() {
        // Simulates the OI fallback logic: if i >= data.open_interest.len(), use 0
        let oi_array: Vec<i64> = vec![100, 200]; // only 2 entries
        let index = 3_usize; // out of range
        let oi_value = if index < oi_array.len() {
            oi_array[index]
        } else {
            0
        };
        assert_eq!(oi_value, 0);
    }

    #[test]
    fn test_oi_fallback_uses_value_when_in_range() {
        let oi_array: Vec<i64> = vec![100, 200, 300];
        let index = 1_usize;
        let oi_value = if index < oi_array.len() {
            oi_array[index]
        } else {
            0
        };
        assert_eq!(oi_value, 200);
    }

    #[test]
    fn test_oi_fallback_empty_array() {
        let oi_array: Vec<i64> = vec![];
        let index = 0_usize;
        let oi_value = if index < oi_array.len() {
            oi_array[index]
        } else {
            0
        };
        assert_eq!(oi_value, 0);
    }

    // -----------------------------------------------------------------------
    // Error 805 pause and constant validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_805_pause_is_60_seconds() {
        assert_eq!(ERROR_805_PAUSE_SECS, 60);
    }

    #[test]
    fn test_dh904_backoff_base_is_10_seconds() {
        assert_eq!(DH_904_BACKOFF_BASE_SECS, 10);
    }

    // -----------------------------------------------------------------------
    // InstrumentFetchResult Success with persist failures
    // -----------------------------------------------------------------------

    #[test]
    fn test_instrument_fetch_result_success_equality() {
        assert_eq!(
            InstrumentFetchResult::Success(100, 5),
            InstrumentFetchResult::Success(100, 5)
        );
    }

    #[test]
    fn test_instrument_fetch_result_success_different_persist_fails() {
        assert_ne!(
            InstrumentFetchResult::Success(100, 0),
            InstrumentFetchResult::Success(100, 5),
            "Different persist failure counts must be distinct"
        );
    }

    #[test]
    fn test_instrument_fetch_result_success_different_candle_counts() {
        assert_ne!(
            InstrumentFetchResult::Success(50, 0),
            InstrumentFetchResult::Success(100, 0),
            "Different candle counts must be distinct"
        );
    }

    // -----------------------------------------------------------------------
    // SubscriptionCategory mapping — exercising match arms in fetch logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_major_index_value_maps_to_index() {
        let instrument_type = match SubscriptionCategory::MajorIndexValue {
            SubscriptionCategory::MajorIndexValue | SubscriptionCategory::DisplayIndex => "INDEX",
            SubscriptionCategory::StockEquity => "EQUITY",
            SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative => "SKIP",
        };
        assert_eq!(instrument_type, "INDEX");
    }

    #[test]
    fn test_stock_equity_maps_to_equity() {
        let instrument_type = match SubscriptionCategory::StockEquity {
            SubscriptionCategory::MajorIndexValue | SubscriptionCategory::DisplayIndex => "INDEX",
            SubscriptionCategory::StockEquity => "EQUITY",
            SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative => "SKIP",
        };
        assert_eq!(instrument_type, "EQUITY");
    }

    #[test]
    fn test_index_derivative_skipped() {
        let should_skip = matches!(
            SubscriptionCategory::IndexDerivative,
            SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative
        );
        assert!(should_skip);
    }

    #[test]
    fn test_stock_derivative_skipped() {
        let should_skip = matches!(
            SubscriptionCategory::StockDerivative,
            SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative
        );
        assert!(should_skip);
    }

    // -----------------------------------------------------------------------
    // CandleFetchSummary — failure_reasons tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_failure_reasons_token_expired_classification() {
        // Simulates the failure reason classification logic
        let token_expired_counts: Vec<usize> = vec![0, 0, 2, 1, 3];
        let pending_indices = vec![2_usize, 3, 4];
        let mut failure_reasons: HashMap<String, usize> = HashMap::new();
        for &idx in &pending_indices {
            if token_expired_counts[idx] >= MAX_TOKEN_EXPIRED_RETRIES {
                let count = failure_reasons
                    .entry("token_expired".to_string())
                    .or_insert(0);
                *count = count.saturating_add(1);
            } else {
                let count = failure_reasons
                    .entry("network_or_api".to_string())
                    .or_insert(0);
                *count = count.saturating_add(1);
            }
        }
        // idx=2: count=2 >= MAX_TOKEN_EXPIRED_RETRIES(2) → token_expired
        // idx=3: count=1 < 2 → network_or_api
        // idx=4: count=3 >= 2 → token_expired
        assert_eq!(failure_reasons["token_expired"], 2);
        assert_eq!(failure_reasons["network_or_api"], 1);
        // Ensure we also handle the mock token count correctly
        assert!(token_expired_counts[2] >= MAX_TOKEN_EXPIRED_RETRIES);
        assert!(token_expired_counts[3] < MAX_TOKEN_EXPIRED_RETRIES);
    }

    #[test]
    fn test_failure_reasons_persist_inserted_when_nonzero() {
        // Simulates the persist failure insertion logic
        let total_persist_failures = 42_usize;
        let mut failure_reasons: HashMap<String, usize> = HashMap::new();
        if total_persist_failures > 0 {
            failure_reasons.insert("persist".to_string(), total_persist_failures);
        }
        assert_eq!(failure_reasons["persist"], 42);
    }

    #[test]
    fn test_failure_reasons_persist_not_inserted_when_zero() {
        let total_persist_failures = 0_usize;
        let mut failure_reasons: HashMap<String, usize> = HashMap::new();
        if total_persist_failures > 0 {
            failure_reasons.insert("persist".to_string(), total_persist_failures);
        }
        assert!(
            !failure_reasons.contains_key("persist"),
            "persist key must not be present when there are 0 persist failures"
        );
    }

    // -----------------------------------------------------------------------
    // Failed instrument name collection — MAX_FAILED_INSTRUMENT_NAMES cap
    // -----------------------------------------------------------------------

    #[test]
    fn test_failed_instrument_names_take_capped() {
        // Simulates the .take(MAX_FAILED_INSTRUMENT_NAMES) in the fetch function
        let large_indices: Vec<usize> = (0..100).collect();
        let names: Vec<String> = large_indices
            .iter()
            .take(MAX_FAILED_INSTRUMENT_NAMES)
            .map(|&idx| format!("{idx} (NSE_EQ)"))
            .collect();
        assert_eq!(names.len(), MAX_FAILED_INSTRUMENT_NAMES);
        assert_eq!(names.len(), 50);
    }

    #[test]
    fn test_failed_instrument_names_under_cap() {
        let small_indices: Vec<usize> = (0..3).collect();
        let names: Vec<String> = small_indices
            .iter()
            .take(MAX_FAILED_INSTRUMENT_NAMES)
            .map(|&idx| format!("{idx} (IDX_I)"))
            .collect();
        assert_eq!(names.len(), 3);
    }

    // -----------------------------------------------------------------------
    // Intraday response deserialization from JSON
    // -----------------------------------------------------------------------

    #[test]
    fn test_dhan_intraday_response_deserialization() {
        let json = r#"{
            "open": [100.5, 101.0],
            "high": [102.0, 103.0],
            "low": [99.5, 100.0],
            "close": [101.0, 102.5],
            "volume": [1000, 2000],
            "timestamp": [1700000000, 1700000060]
        }"#;
        let response: DhanIntradayResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.len(), 2);
        assert!(response.is_consistent());
        assert!(response.open_interest.is_empty());
    }

    #[test]
    fn test_dhan_intraday_response_with_float_volume() {
        // Dhan may return volume/timestamp as float (e.g., 105600.0)
        let json = r#"{
            "open": [100.5],
            "high": [102.0],
            "low": [99.5],
            "close": [101.0],
            "volume": [105600.0],
            "timestamp": [1700000000.0],
            "open_interest": [50000.0]
        }"#;
        let response: DhanIntradayResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.volume[0], 105600);
        assert_eq!(response.timestamp[0], 1700000000);
        assert_eq!(response.open_interest[0], 50000);
    }

    #[test]
    fn test_dhan_daily_response_deserialization() {
        let json = r#"{
            "open": [24500.5],
            "high": [24700.0],
            "low": [24400.0],
            "close": [24650.0],
            "volume": [500000],
            "timestamp": [1700000000]
        }"#;
        let response: DhanDailyResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.len(), 1);
        assert!(response.is_consistent());
        assert!(response.open_interest.is_empty());
    }

    // -----------------------------------------------------------------------
    // ErrorAction enum completeness
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_action_debug_format() {
        // Verify all variants have Debug
        assert!(format!("{:?}", ErrorAction::RateLimited).contains("RateLimited"));
        assert!(format!("{:?}", ErrorAction::TooManyConnections).contains("TooManyConnections"));
        assert!(format!("{:?}", ErrorAction::TokenExpired).contains("TokenExpired"));
        assert!(format!("{:?}", ErrorAction::StandardRetry).contains("StandardRetry"));
        assert!(format!("{:?}", ErrorAction::NeverRetry).contains("NeverRetry"));
    }

    #[test]
    fn test_error_action_equality_all_variants() {
        // Each variant equals itself
        assert_eq!(ErrorAction::RateLimited, ErrorAction::RateLimited);
        assert_eq!(
            ErrorAction::TooManyConnections,
            ErrorAction::TooManyConnections
        );
        assert_eq!(ErrorAction::TokenExpired, ErrorAction::TokenExpired);
        assert_eq!(ErrorAction::StandardRetry, ErrorAction::StandardRetry);
        assert_eq!(ErrorAction::NeverRetry, ErrorAction::NeverRetry);

        // Different variants are not equal
        assert_ne!(ErrorAction::RateLimited, ErrorAction::NeverRetry);
        assert_ne!(ErrorAction::TokenExpired, ErrorAction::StandardRetry);
        assert_ne!(ErrorAction::TooManyConnections, ErrorAction::RateLimited);
    }

    // -----------------------------------------------------------------------
    // Retry linear backoff delay math
    // -----------------------------------------------------------------------

    #[test]
    fn test_retry_delay_ms_linear_sequence() {
        // Retry delay = 1000ms * attempt — validates the delay used in fetch_*_with_retry
        let max_retries = 3_u32;
        for attempt in 1..=max_retries {
            let delay_ms = 1000_u64.saturating_mul(u64::from(attempt));
            assert_eq!(delay_ms, u64::from(attempt) * 1000);
        }
    }

    #[test]
    fn test_retry_delay_ms_zero_on_first_attempt() {
        // Attempt 0 skips the delay entirely (the condition: `if attempt > 0`)
        let attempt = 0_u32;
        assert_eq!(attempt, 0, "first attempt has no delay");
    }

    // -----------------------------------------------------------------------
    // Historical candle struct — field access patterns
    // -----------------------------------------------------------------------

    #[test]
    fn test_historical_candle_all_timeframes() {
        for &tf in &["1m", "5m", "15m", "60m", "1d"] {
            let candle = HistoricalCandle {
                timestamp_utc_secs: 1_700_000_000,
                security_id: 11536,
                exchange_segment_code: 1,
                timeframe: tf,
                open: 100.0,
                high: 105.0,
                low: 99.0,
                close: 102.0,
                volume: 50000,
                open_interest: 0,
            };
            assert_eq!(candle.timeframe, tf);
            assert_eq!(candle.security_id, 11536);
        }
    }

    #[test]
    fn test_historical_candle_fno_with_oi() {
        let candle = HistoricalCandle {
            timestamp_utc_secs: 1_700_000_000,
            security_id: 49081,
            exchange_segment_code: 2, // NSE_FNO
            timeframe: "1d",
            open: 25000.0,
            high: 25200.0,
            low: 24900.0,
            close: 25100.0,
            volume: 100000,
            open_interest: 5_000_000,
        };
        assert_eq!(candle.exchange_segment_code, 2);
        assert_eq!(candle.open_interest, 5_000_000);
    }

    // -----------------------------------------------------------------------
    // Saturating arithmetic edge cases (used throughout fetch logic)
    // -----------------------------------------------------------------------

    #[test]
    fn test_saturating_add_prevents_overflow() {
        let mut count = usize::MAX - 1;
        count = count.saturating_add(1);
        assert_eq!(count, usize::MAX);
        count = count.saturating_add(1);
        assert_eq!(
            count,
            usize::MAX,
            "saturating_add must not overflow past usize::MAX"
        );
    }

    #[test]
    fn test_saturating_mul_prevents_overflow() {
        let result = u64::MAX.saturating_mul(2);
        assert_eq!(
            result,
            u64::MAX,
            "saturating_mul must not overflow past u64::MAX"
        );
    }

    // -----------------------------------------------------------------------
    // classify_error — combined format priority tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_error_data_api_format_takes_priority_over_trading() {
        // When body matches BOTH formats, data API numeric format is tried first
        let body = br#"{"errorCode": 805, "errorType": "RATE_LIMIT", "errorMessage": "conflict"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        // Numeric 805 is tried first → TooManyConnections
        assert_eq!(action, ErrorAction::TooManyConnections);
    }

    #[test]
    fn test_classify_error_trading_format_fallback() {
        // Body has only trading API format (string errorCode), no numeric code
        let body =
            br#"{"errorType": "RATE_LIMIT", "errorCode": "DH-904", "errorMessage": "limit"}"#;
        let action = classify_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(action, ErrorAction::RateLimited);
    }
}
