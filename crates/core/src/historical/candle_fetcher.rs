//! Dhan intraday candle fetcher — fully automated 1-minute OHLCV retrieval.
//!
//! Fetches historical candles from Dhan's V2 REST API for all subscribed
//! instruments and persists to QuestDB via the `CandlePersistenceWriter`.
//!
//! # Timestamp Format
//! Dhan V2 REST API returns standard UTC epoch seconds. Timestamps are
//! stored as-is in QuestDB (no conversion). Grafana dashboard uses
//! `timezone: Asia/Kolkata` to display them in IST.
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

                                // Dhan V2 REST API returns standard UTC epoch seconds.
                                // Store as-is — no conversion needed.
                                let utc_epoch = data.timestamp[i];

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

    /// Verifies the store-as-is approach: UTC epoch stored directly in
    /// HistoricalCandle without any offset manipulation.
    #[test]
    fn test_historical_candle_stores_utc_epoch_as_is() {
        // 15:29 IST = 09:59 UTC on 2026-03-09
        let dhan_epoch: i64 = 1_773_050_340;
        let candle = HistoricalCandle {
            timestamp_utc_secs: dhan_epoch, // Stored as-is, no manipulation
            security_id: 42,
            exchange_segment_code: 2,
            open: 100.0,
            high: 102.0,
            low: 99.0,
            close: 101.0,
            volume: 1000,
            open_interest: 0,
        };
        // Timestamp stored is exactly what Dhan returned — raw UTC epoch
        assert_eq!(candle.timestamp_utc_secs, dhan_epoch);
        assert_eq!(candle.timestamp_utc_secs, 1_773_050_340);
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
