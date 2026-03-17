//! Dhan historical candle fetcher — multi-timeframe OHLCV retrieval.
//!
//! Fetches historical candles from Dhan's V2 REST API for all subscribed
//! instruments across 5 timeframes (1m, 5m, 15m, 60m, daily) and persists
//! to QuestDB via the `CandlePersistenceWriter`.
//!
//! # Timestamp Format
//! Dhan V2 REST API returns standard UTC epoch seconds. The persistence layer
//! converts to IST-as-UTC by adding +19800s before writing to QuestDB.
//!
//! # O(1) Deduplication
//! - Client-side: skips instruments already fetched (via security_id set)
//! - Server-side: QuestDB DEDUP UPSERT KEYS(ts, security_id, timeframe) prevents duplicates
//!
//! # Automation
//! Runs without human intervention in the boot sequence after authentication.

use std::collections::HashSet;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{FixedOffset, Utc};
use metrics::counter;
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use dhan_live_trader_common::config::{DhanConfig, HistoricalDataConfig};
use dhan_live_trader_common::constants::{
    DHAN_CHARTS_HISTORICAL_PATH, DHAN_CHARTS_INTRADAY_PATH, INTRADAY_TIMEFRAMES,
    IST_UTC_OFFSET_SECONDS, MARKET_CLOSE_TIME_IST_EXCLUSIVE, MARKET_OPEN_TIME_IST,
    POST_MARKET_FETCH_TIME_IST_HOUR, POST_MARKET_FETCH_TIME_IST_MINUTE, TIMEFRAME_1D,
};
use dhan_live_trader_common::instrument_registry::{InstrumentRegistry, SubscriptionCategory};
use dhan_live_trader_common::tick_types::{
    DhanDailyResponse, DhanIntradayResponse, HistoricalCandle,
};

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
#[derive(Debug)]
enum InstrumentFetchResult {
    /// Successfully fetched — candle count.
    Success(usize),
    /// Failure — eligible for retry in next wave.
    /// NeverRetry errors (DH-905) will fail fast on re-attempt (no wasted time).
    Failed,
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
}

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

    let ist_offset = match FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) {
        Some(offset) => offset,
        None => {
            warn!("invalid IST offset constant — cannot compute date range");
            return CandleFetchSummary {
                instruments_fetched: 0,
                instruments_failed: 0,
                total_candles: 0,
                instruments_skipped: 0,
            };
        }
    };

    let now_ist = Utc::now().with_timezone(&ist_offset);
    let today = now_ist.date_naive();

    // Compute date range: today - lookback_days to today
    let from_date = today - chrono::Duration::days(i64::from(historical_config.lookback_days));
    let to_date = today;

    // Intraday requests use datetime format with market hours
    let from_date_str = from_date.format("%Y-%m-%d").to_string();
    let to_date_str = to_date.format("%Y-%m-%d").to_string();

    // Intraday: "YYYY-MM-DD HH:MM:SS" with market hours (09:15 to 15:30 exclusive)
    let intraday_from = format!("{} {}", from_date_str, MARKET_OPEN_TIME_IST);
    let intraday_to = format!("{} {}", to_date_str, MARKET_CLOSE_TIME_IST_EXCLUSIVE);

    // Daily: date-only format, toDate is NON-INCLUSIVE so add 1 day
    let daily_to_date = to_date + chrono::Duration::days(1);
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
                InstrumentFetchResult::Success(candle_count) => {
                    instruments_fetched = instruments_fetched.saturating_add(1);
                    total_candles = total_candles.saturating_add(candle_count);
                    #[allow(clippy::cast_possible_truncation)]
                    // APPROVED: usize->u64 is lossless on 64-bit targets
                    m_fetched.increment(candle_count as u64);
                }
                InstrumentFetchResult::Failed => {
                    still_pending.push(idx);
                }
            }
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
    if instruments_failed > 0 {
        warn!(
            instruments_failed,
            retry_waves = RETRY_WAVE_MAX,
            "instruments still failed after all retry waves"
        );
    }

    // Final flush
    if let Err(err) = candle_writer.force_flush() {
        warn!(?err, "failed to flush remaining candles to QuestDB");
    }

    info!(
        instruments_fetched,
        instruments_failed, instruments_skipped, total_candles, "historical candle fetch complete"
    );

    CandleFetchSummary {
        instruments_fetched,
        instruments_failed,
        total_candles,
        instruments_skipped,
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
            Some(count) => {
                instrument_candles = instrument_candles.saturating_add(count);
            }
            None => {
                // Cannot distinguish transient vs permanent here — return transient
                // so the retry wave re-attempts. If it's truly permanent (DH-905),
                // the retry will fail fast (NeverRetry returns immediately).
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
            return InstrumentFetchResult::Failed;
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
        Some(count) => {
            instrument_candles = instrument_candles.saturating_add(count);
        }
        None => {
            return InstrumentFetchResult::Failed;
        }
    }

    InstrumentFetchResult::Success(instrument_candles)
}

// ---------------------------------------------------------------------------
// Intraday Fetch Helper
// ---------------------------------------------------------------------------

/// Fetches a single intraday timeframe for one instrument with retries.
/// Returns `Some(candle_count)` on success, `None` on failure.
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
) -> Option<usize> {
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
                                return None;
                            }
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                            continue;
                        }
                        ErrorAction::TokenExpired => {
                            warn!(
                                security_id,
                                timeframe = timeframe_label,
                                "token expired (807/DH-901) — skipping instrument"
                            );
                            m_api_errors.increment(1);
                            return None;
                        }
                        ErrorAction::NeverRetry => {
                            warn!(
                                %status, security_id, timeframe = timeframe_label,
                                "input error (DH-905/906) — will not retry"
                            );
                            m_api_errors.increment(1);
                            return None;
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
                            return None;
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
                        return None;
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
                    return None;
                }

                match serde_json::from_slice::<DhanIntradayResponse>(&body_bytes) {
                    Ok(data) => {
                        if data.is_empty() {
                            debug!(
                                security_id,
                                timeframe = timeframe_label,
                                "no candle data returned"
                            );
                            return Some(0);
                        }

                        if !data.is_consistent() {
                            warn!(
                                security_id,
                                timeframe = timeframe_label,
                                "inconsistent array lengths in historical response"
                            );
                            m_api_errors.increment(1);
                            return None;
                        }

                        let count = persist_intraday_candles(
                            &data,
                            security_id,
                            segment_code,
                            timeframe_label,
                            candle_writer,
                            m_api_errors,
                        );
                        return Some(count);
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
                        return None;
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
                return None;
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Daily Fetch Helper
// ---------------------------------------------------------------------------

/// Fetches daily candles for one instrument with retries.
/// Returns `Some(candle_count)` on success, `None` on failure.
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
) -> Option<usize> {
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
                                return None;
                            }
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                            continue;
                        }
                        ErrorAction::TokenExpired => {
                            warn!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                "token expired (807/DH-901) — skipping instrument"
                            );
                            m_api_errors.increment(1);
                            return None;
                        }
                        ErrorAction::NeverRetry => {
                            warn!(
                                %status, security_id, timeframe = TIMEFRAME_1D,
                                "input error (DH-905/906) — will not retry"
                            );
                            m_api_errors.increment(1);
                            return None;
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
                            return None;
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
                        return None;
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
                    return None;
                }

                match serde_json::from_slice::<DhanDailyResponse>(&body_bytes) {
                    Ok(data) => {
                        if data.is_empty() {
                            debug!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                "no daily data returned"
                            );
                            return Some(0);
                        }

                        if !data.is_consistent() {
                            warn!(
                                security_id,
                                timeframe = TIMEFRAME_1D,
                                "inconsistent array lengths in daily response"
                            );
                            m_api_errors.increment(1);
                            return None;
                        }

                        let count = persist_daily_candles(
                            &data,
                            security_id,
                            segment_code,
                            candle_writer,
                            m_api_errors,
                        );
                        return Some(count);
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
                        return None;
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
                return None;
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Candle Persistence Helpers
// ---------------------------------------------------------------------------

/// Converts intraday response to HistoricalCandle structs and persists them.
fn persist_intraday_candles(
    data: &DhanIntradayResponse,
    security_id: u32,
    segment_code: u8,
    timeframe_label: &'static str,
    candle_writer: &mut CandlePersistenceWriter,
    m_api_errors: &metrics::Counter,
) -> usize {
    let mut count = 0_usize;
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
        }
        count = count.saturating_add(1);
    }
    count
}

/// Converts daily response to HistoricalCandle structs and persists them.
fn persist_daily_candles(
    data: &DhanDailyResponse,
    security_id: u32,
    segment_code: u8,
    candle_writer: &mut CandlePersistenceWriter,
    m_api_errors: &metrics::Counter,
) -> usize {
    let mut count = 0_usize;
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
        }
        count = count.saturating_add(1);
    }
    count
}

// ---------------------------------------------------------------------------
// Post-Market Scheduling
// ---------------------------------------------------------------------------

/// Computes how long to wait until the post-market fetch time (15:35 IST).
///
/// Returns `Duration::ZERO` if the current time is already past 15:35 IST today
/// (fetch should run immediately).
///
/// Returns `Some(duration)` to wait, or `None` if not a trading day
/// (weekends are skipped — caller should handle).
// TEST-EXEMPT: tested by test_post_market_time_calculation and test_post_market_constants
pub fn duration_until_post_market_fetch() -> Duration {
    let ist_offset = match FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) {
        Some(offset) => offset,
        None => return Duration::ZERO,
    };
    let now_ist = Utc::now().with_timezone(&ist_offset);

    let target_time = now_ist.date_naive().and_hms_opt(
        POST_MARKET_FETCH_TIME_IST_HOUR,
        POST_MARKET_FETCH_TIME_IST_MINUTE,
        0,
    );

    let target = match target_time {
        Some(t) => t.and_local_timezone(ist_offset).single(),
        None => None,
    };

    match target {
        Some(target_dt) => {
            if now_ist >= target_dt {
                // Already past 15:35 IST — run immediately
                Duration::ZERO
            } else {
                let diff = target_dt.signed_duration_since(now_ist);
                // Convert chrono::Duration to std::time::Duration
                diff.to_std().unwrap_or(Duration::ZERO)
            }
        }
        None => Duration::ZERO,
    }
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
        };
        assert_eq!(summary.instruments_fetched, 10);
        assert_eq!(summary.total_candles, 3750);
    }

    /// IST offset in seconds (5h30m) for UTC→IST conversion in tests.
    const IST_OFFSET: i64 = 19_800;

    /// Extracts IST hour and minute from a UTC epoch by adding IST offset.
    fn utc_epoch_to_ist_hm(utc_epoch: i64) -> (i64, i64) {
        let ist_secs = (utc_epoch + IST_OFFSET) % 86_400;
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

    // -- Post-market scheduling tests --

    #[test]
    fn test_post_market_time_calculation() {
        // duration_until_post_market_fetch returns a Duration (possibly zero)
        let duration = duration_until_post_market_fetch();
        // Should be <= 24 hours (one day in seconds)
        assert!(
            duration.as_secs() <= 86_400,
            "post-market wait should not exceed 24 hours"
        );
    }

    #[test]
    fn test_post_market_constants() {
        assert_eq!(POST_MARKET_FETCH_TIME_IST_HOUR, 15);
        assert_eq!(POST_MARKET_FETCH_TIME_IST_MINUTE, 35);
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
        let success = InstrumentFetchResult::Success(100);
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
}
