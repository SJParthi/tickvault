//! Dhan intraday candle fetcher — fully automated 1-minute OHLCV retrieval.
//!
//! Fetches historical candles from Dhan's REST API for all subscribed
//! instruments, converts IST timestamps to UTC, and persists to QuestDB
//! via the `CandlePersistenceWriter`.
//!
//! # O(1) Deduplication
//! - Client-side: skips instruments already fetched (via security_id set)
//! - Server-side: QuestDB DEDUP UPSERT KEYS(ts, security_id) prevents duplicates
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
    DHAN_CANDLE_INTERVAL_1MIN, DHAN_CHARTS_INTRADAY_PATH, IST_UTC_OFFSET_SECONDS,
    IST_UTC_OFFSET_SECONDS_I64,
};
use dhan_live_trader_common::instrument_registry::{InstrumentRegistry, SubscriptionCategory};
use dhan_live_trader_common::tick_types::{DhanIntradayResponse, HistoricalCandle};

use dhan_live_trader_storage::candle_persistence::CandlePersistenceWriter;

use crate::auth::types::TokenState;

/// Type alias for the token handle used across the codebase.
type TokenHandle = std::sync::Arc<ArcSwap<Option<TokenState>>>;

/// Maximum response body size for Dhan intraday API (10 MB).
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

/// Fetches 1-minute candles for all subscribed instruments and persists to QuestDB.
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
#[allow(clippy::too_many_arguments)]
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

    // APPROVED: IST_UTC_OFFSET_SECONDS is a compile-time constant (19800 = 5h30m),
    // always valid for east_opt(). The expect is unreachable but satisfies no-unwrap lint.
    #[allow(clippy::expect_used)]
    // APPROVED: compile-time constant 19800 is always valid for FixedOffset::east_opt
    let ist_offset =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset 19800s is always valid");

    let now_ist = Utc::now().with_timezone(&ist_offset);
    let today = now_ist.date_naive();

    // Compute date range: today - lookback_days to today
    let from_date = today - chrono::Duration::days(i64::from(historical_config.lookback_days));
    let to_date = today;

    let from_str = from_date.format("%Y-%m-%d").to_string();
    let to_str = to_date.format("%Y-%m-%d").to_string();

    info!(
        from_date = %from_str,
        to_date = %to_str,
        lookback_days = historical_config.lookback_days,
        "starting historical candle fetch"
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

    let endpoint = format!(
        "{}{}",
        dhan_config.rest_api_base_url, DHAN_CHARTS_INTRADAY_PATH
    );

    let mut fetched_security_ids: HashSet<u32> = HashSet::new();
    let mut instruments_fetched: usize = 0;
    let mut instruments_failed: usize = 0;
    let mut instruments_skipped: usize = 0;
    let mut total_candles: usize = 0;

    // Iterate all subscribed instruments
    for instrument in registry.iter() {
        let security_id = instrument.security_id;

        // Skip display indices — no candle data available
        if instrument.category == SubscriptionCategory::DisplayIndex {
            instruments_skipped = instruments_skipped.saturating_add(1);
            continue;
        }

        // Skip already-fetched security IDs (dedup across categories)
        if fetched_security_ids.contains(&security_id) {
            continue;
        }

        // Determine the Dhan API instrument type
        let is_index = matches!(
            instrument.category,
            SubscriptionCategory::MajorIndexValue | SubscriptionCategory::IndexDerivative
        );

        let instrument_type = match instrument.category {
            SubscriptionCategory::MajorIndexValue => "INDEX",
            SubscriptionCategory::StockEquity => "EQUITY",
            SubscriptionCategory::IndexDerivative | SubscriptionCategory::StockDerivative => {
                if instrument.option_type.is_some() {
                    if is_index { "OPTIDX" } else { "OPTSTK" }
                } else if is_index {
                    "FUTIDX"
                } else {
                    "FUTSTK"
                }
            }
            SubscriptionCategory::DisplayIndex => continue,
        };

        // Rate limiting delay between requests
        if historical_config.request_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(historical_config.request_delay_ms)).await;
        }

        // Load current access token (Zeroizing ensures the plaintext is wiped from heap on drop)
        let token_guard = token_handle.load();
        let access_token = match token_guard.as_ref() {
            Some(token_state) => {
                Zeroizing::new(token_state.access_token().expose_secret().to_string())
            }
            None => {
                warn!("no access token available — skipping historical fetch");
                instruments_failed = instruments_failed.saturating_add(1);
                continue;
            }
        };

        let request_body = IntradayRequest {
            security_id: security_id.to_string(),
            exchange_segment: instrument.exchange_segment.as_str().to_string(),
            instrument: instrument_type.to_string(),
            interval: DHAN_CANDLE_INTERVAL_1MIN.to_string(),
            oi: true,
            from_date: from_str.clone(),
            to_date: to_str.clone(),
        };

        // Make the API call with retries
        let mut candles_for_instrument = 0_usize;
        let mut success = false;

        for attempt in 0..=historical_config.max_retries {
            if attempt > 0 {
                let delay_ms = 1000_u64.saturating_mul(u64::from(attempt));
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            let result = http_client
                .post(&endpoint)
                .header("access-token", access_token.as_str())
                .header("client-id", client_id.expose_secret())
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(&request_body)
                .send()
                .await;

            match result {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status();
                        if attempt < historical_config.max_retries {
                            debug!(
                                %status,
                                security_id,
                                attempt,
                                "historical API non-success — retrying"
                            );
                            continue;
                        }
                        warn!(
                            %status,
                            security_id,
                            "historical API failed after all retries"
                        );
                        m_api_errors.increment(1);
                        break;
                    }

                    // Read body with size limit to prevent OOM from oversized responses
                    let body_bytes = match response.bytes().await {
                        Ok(b) => b,
                        Err(err) => {
                            if attempt < historical_config.max_retries {
                                debug!(
                                    ?err,
                                    security_id, attempt, "failed to read response body — retrying"
                                );
                                continue;
                            }
                            warn!(
                                ?err,
                                security_id, "failed to read response body after all retries"
                            );
                            m_api_errors.increment(1);
                            break;
                        }
                    };

                    if body_bytes.len() > MAX_RESPONSE_BODY_SIZE {
                        warn!(
                            security_id,
                            body_size = body_bytes.len(),
                            max = MAX_RESPONSE_BODY_SIZE,
                            "response body exceeds size limit — skipping"
                        );
                        m_api_errors.increment(1);
                        break;
                    }

                    match serde_json::from_slice::<DhanIntradayResponse>(&body_bytes) {
                        Ok(data) => {
                            if data.is_empty() {
                                debug!(security_id, "no candle data returned");
                                success = true;
                                break;
                            }

                            if !data.is_consistent() {
                                warn!(
                                    security_id,
                                    "inconsistent array lengths in historical response"
                                );
                                m_api_errors.increment(1);
                                break;
                            }

                            let segment_code = instrument.exchange_segment.binary_code();

                            // Convert parallel arrays to candles and persist
                            for i in 0..data.len() {
                                let open = data.open[i];
                                let high = data.high[i];
                                let low = data.low[i];
                                let close = data.close[i];

                                // Reject NaN/Infinity prices — corrupted data must not reach QuestDB
                                if !open.is_finite()
                                    || !high.is_finite()
                                    || !low.is_finite()
                                    || !close.is_finite()
                                {
                                    warn!(
                                        security_id,
                                        idx = i,
                                        "NaN/Inf price in API response — skipping candle"
                                    );
                                    m_api_errors.increment(1);
                                    continue;
                                }

                                // Convert IST-naive timestamp to UTC
                                let ist_epoch = data.timestamp[i];
                                let utc_epoch =
                                    ist_epoch.saturating_sub(IST_UTC_OFFSET_SECONDS_I64);

                                let oi_value = if i < data.open_interest.len() {
                                    data.open_interest[i]
                                } else {
                                    0
                                };

                                let candle = HistoricalCandle {
                                    timestamp_utc_secs: utc_epoch,
                                    security_id,
                                    exchange_segment_code: segment_code,
                                    open,
                                    high,
                                    low,
                                    close,
                                    volume: data.volume[i],
                                    open_interest: oi_value,
                                };

                                if let Err(err) = candle_writer.append_candle(&candle) {
                                    warn!(?err, security_id, "failed to append candle to QuestDB");
                                }
                                candles_for_instrument = candles_for_instrument.saturating_add(1);
                            }

                            success = true;
                            break;
                        }
                        Err(err) => {
                            if attempt < historical_config.max_retries {
                                debug!(
                                    ?err,
                                    security_id,
                                    attempt,
                                    "failed to parse historical response — retrying"
                                );
                                continue;
                            }
                            warn!(
                                ?err,
                                security_id,
                                "failed to parse historical response after all retries"
                            );
                            m_api_errors.increment(1);
                            break;
                        }
                    }
                }
                Err(err) => {
                    if attempt < historical_config.max_retries {
                        debug!(
                            ?err,
                            security_id, attempt, "historical API request failed — retrying"
                        );
                        continue;
                    }
                    warn!(
                        ?err,
                        security_id, "historical API request failed after all retries"
                    );
                    m_api_errors.increment(1);
                    break;
                }
            }
        }

        if success {
            fetched_security_ids.insert(security_id);
            instruments_fetched = instruments_fetched.saturating_add(1);
            total_candles = total_candles.saturating_add(candles_for_instrument);
            // APPROVED: usize->u64 is lossless on 64-bit targets (our only deployment target)
            #[allow(clippy::cast_possible_truncation)]
            m_fetched.increment(candles_for_instrument as u64);
        } else {
            instruments_failed = instruments_failed.saturating_add(1);
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
            from_date: "2025-01-01".to_string(),
            to_date: "2025-01-05".to_string(),
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
}
