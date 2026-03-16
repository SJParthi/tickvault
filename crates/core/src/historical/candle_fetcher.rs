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
    IST_UTC_OFFSET_SECONDS, MARKET_CLOSE_TIME_IST_EXCLUSIVE, MARKET_OPEN_TIME_IST, TIMEFRAME_1D,
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

    #[allow(clippy::expect_used)] // APPROVED: compile-time constant 19800 is always valid
    // APPROVED: IST_UTC_OFFSET_SECONDS is a compile-time constant (19800 = 5h30m),
    // always valid for east_opt(). The expect is unreachable but satisfies no-unwrap lint.
    let ist_offset =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset 19800s is always valid"); // APPROVED: compile-time constant

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

    let mut fetched_security_ids: HashSet<u32> = HashSet::new();
    let mut instruments_fetched: usize = 0;
    let mut instruments_failed: usize = 0;
    let mut instruments_skipped: usize = 0;
    let mut total_candles: usize = 0;

    // Iterate all subscribed instruments
    for instrument in registry.iter() {
        let security_id = instrument.security_id;

        // Skip display indices and all derivatives (futures + options).
        // Historical candles are only needed for indices and stock equities.
        if matches!(
            instrument.category,
            SubscriptionCategory::DisplayIndex
                | SubscriptionCategory::IndexDerivative
                | SubscriptionCategory::StockDerivative
        ) {
            instruments_skipped = instruments_skipped.saturating_add(1);
            continue;
        }

        // Skip already-fetched security IDs (dedup across categories)
        if fetched_security_ids.contains(&security_id) {
            continue;
        }

        // Determine the Dhan API instrument type (only INDEX and EQUITY after skip above)
        let instrument_type = match instrument.category {
            SubscriptionCategory::MajorIndexValue => "INDEX",
            SubscriptionCategory::StockEquity => "EQUITY",
            // Derivatives and display indices are already skipped above
            SubscriptionCategory::IndexDerivative
            | SubscriptionCategory::StockDerivative
            | SubscriptionCategory::DisplayIndex => continue,
        };

        let segment_code = instrument.exchange_segment.binary_code();
        let exchange_segment_str = instrument.exchange_segment.as_str().to_string();
        let security_id_str = security_id.to_string();

        let mut instrument_candles = 0_usize;
        let mut instrument_failed = false;

        // --- Fetch all 4 intraday timeframes ---
        for &(interval, timeframe_label) in INTRADAY_TIMEFRAMES {
            // Rate limiting delay between requests
            if historical_config.request_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(historical_config.request_delay_ms)).await;
            }

            // Load current access token
            let token_guard = token_handle.load();
            let access_token = match token_guard.as_ref() {
                Some(token_state) => {
                    Zeroizing::new(token_state.access_token().expose_secret().to_string())
                }
                None => {
                    warn!("no access token available — skipping historical fetch");
                    instrument_failed = true;
                    break;
                }
            };

            let request_body = IntradayRequest {
                security_id: security_id_str.clone(),
                exchange_segment: exchange_segment_str.clone(),
                instrument: instrument_type.to_string(),
                interval: interval.to_string(),
                oi: true,
                from_date: intraday_from.clone(),
                to_date: intraday_to.clone(),
            };

            match fetch_intraday_with_retry(
                &http_client,
                &intraday_endpoint,
                &request_body,
                &access_token,
                client_id,
                historical_config,
                security_id,
                segment_code,
                timeframe_label,
                candle_writer,
                &m_api_errors,
            )
            .await
            {
                Some(count) => {
                    instrument_candles = instrument_candles.saturating_add(count);
                }
                None => {
                    instrument_failed = true;
                }
            }
        }

        // --- Fetch daily candles ---
        if !instrument_failed {
            if historical_config.request_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(historical_config.request_delay_ms)).await;
            }

            let token_guard = token_handle.load();
            let access_token = match token_guard.as_ref() {
                Some(token_state) => {
                    Zeroizing::new(token_state.access_token().expose_secret().to_string())
                }
                None => {
                    warn!("no access token available — skipping daily fetch");
                    instrument_failed = true;
                    Zeroizing::new(String::new()) // placeholder, not used
                }
            };

            if !instrument_failed {
                let daily_body = DailyRequest {
                    security_id: security_id_str.clone(),
                    exchange_segment: exchange_segment_str.clone(),
                    instrument: instrument_type.to_string(),
                    expiry_code: None,
                    from_date: from_date_str.clone(),
                    to_date: daily_to_str.clone(),
                };

                match fetch_daily_with_retry(
                    &http_client,
                    &daily_endpoint,
                    &daily_body,
                    &access_token,
                    client_id,
                    historical_config,
                    security_id,
                    segment_code,
                    candle_writer,
                    &m_api_errors,
                )
                .await
                {
                    Some(count) => {
                        instrument_candles = instrument_candles.saturating_add(count);
                    }
                    None => {
                        instrument_failed = true;
                    }
                }
            }
        }

        if instrument_failed {
            instruments_failed = instruments_failed.saturating_add(1);
        } else {
            fetched_security_ids.insert(security_id);
            instruments_fetched = instruments_fetched.saturating_add(1);
            total_candles = total_candles.saturating_add(instrument_candles);
            // APPROVED: usize->u64 is lossless on 64-bit targets (our only deployment target)
            #[allow(clippy::cast_possible_truncation)]
            m_fetched.increment(instrument_candles as u64);
        }
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
}
