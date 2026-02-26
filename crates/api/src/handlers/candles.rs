//! Candle data endpoints.
//!
//! - `GET /api/candles/:security_id` — historical OHLCV candles from QuestDB
//! - `GET /api/intervals` — available chart intervals
//!
//! # How It Works
//! Time-based candles: QuestDB SAMPLE BY on the `ticks` table computes OHLCV
//! at any interval on-demand. Raw ticks are the single source of truth.
//!
//! Tick-based candles: Fetches raw ticks from QuestDB and aggregates into
//! N-tick candles client-side (SAMPLE BY doesn't support tick counts).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use dhan_live_trader_common::tick_types::{TickInterval, Timeframe};
use dhan_live_trader_storage::candle_query::QuestDbCandleQuerier;

use crate::state::SharedAppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeframe when none specified.
const DEFAULT_TIMEFRAME: &str = "5m";

/// Maximum raw ticks to fetch for tick-based candle aggregation.
const MAX_RAW_TICKS_FOR_TICK_CANDLES: usize = 50_000;

// ---------------------------------------------------------------------------
// Request / Response Types
// ---------------------------------------------------------------------------

/// Query parameters for the candles endpoint.
#[derive(Debug, Deserialize)]
pub struct CandleQuery {
    /// Timeframe label (e.g., "5m", "1h", "100T").
    pub timeframe: Option<String>,
    /// Start time as Unix epoch seconds.
    pub from: Option<i64>,
    /// End time as Unix epoch seconds.
    pub to: Option<i64>,
}

/// Candle response for JSON serialization.
/// Matches TradingView Lightweight Charts data format.
#[derive(Debug, Serialize)]
pub struct CandleResponse {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

/// Available intervals response.
#[derive(Debug, Serialize)]
pub struct IntervalsResponse {
    pub time: Vec<&'static str>,
    pub tick: Vec<String>,
    pub custom: bool,
}

/// Error response body.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    error: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/candles/:security_id — returns historical candle data.
///
/// # Query Parameters
/// - `timeframe` — interval label (default: "5m"). Supports time-based ("1s", "5m", "1h")
///   and tick-based ("10T", "100T").
/// - `from` — start epoch seconds (default: 8 hours ago)
/// - `to` — end epoch seconds (default: now)
///
/// # Returns
/// JSON array of OHLCV candles sorted by time ascending.
pub async fn get_candles(
    State(state): State<SharedAppState>,
    Path(security_id): Path<u32>,
    Query(query): Query<CandleQuery>,
) -> Result<Json<Vec<CandleResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let timeframe = query.timeframe.as_deref().unwrap_or(DEFAULT_TIMEFRAME);

    // Default time range: last 8 hours (covers a full trading day)
    let now = chrono::Utc::now().timestamp();
    let from = query.from.unwrap_or(now - 8 * 3600);
    let to = query.to.unwrap_or(now);

    // Get the candle querier
    let querier = state.candle_querier().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "QuestDB candle querier unavailable".to_string(),
            }),
        )
    })?;

    // Determine if this is a tick-based or time-based interval
    if let Some(tick_interval) = TickInterval::from_label(timeframe) {
        // Tick-based: fetch raw ticks and aggregate
        let ticks_per_candle = tick_interval.tick_count();

        let raw_ticks = querier
            .query_raw_ticks(security_id, from, to, MAX_RAW_TICKS_FOR_TICK_CANDLES)
            .await
            .map_err(|err| {
                tracing::warn!(?err, security_id, timeframe, "tick candle query failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("tick query failed: {err}"),
                    }),
                )
            })?;

        let candle_rows =
            QuestDbCandleQuerier::aggregate_tick_candles(&raw_ticks, ticks_per_candle);

        let candles: Vec<CandleResponse> = candle_rows
            .into_iter()
            .map(|c| CandleResponse {
                time: c.timestamp,
                open: c.open,
                high: c.high,
                low: c.low,
                close: c.close,
                volume: c.volume,
            })
            .collect();

        tracing::debug!(
            security_id,
            timeframe,
            candle_count = candles.len(),
            "tick candles served"
        );

        Ok(Json(candles))
    } else {
        // Time-based: use SAMPLE BY
        let candle_rows = querier
            .query_candles(security_id, timeframe, from, to)
            .await
            .map_err(|err| {
                tracing::warn!(?err, security_id, timeframe, "candle query failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("candle query failed: {err}"),
                    }),
                )
            })?;

        let candles: Vec<CandleResponse> = candle_rows
            .into_iter()
            .map(|c| CandleResponse {
                time: c.timestamp,
                open: c.open,
                high: c.high,
                low: c.low,
                close: c.close,
                volume: c.volume,
            })
            .collect();

        tracing::debug!(
            security_id,
            timeframe,
            candle_count = candles.len(),
            "time candles served"
        );

        Ok(Json(candles))
    }
}

/// GET /api/intervals — returns all available chart intervals.
pub async fn get_intervals() -> Json<IntervalsResponse> {
    let time_intervals: Vec<&'static str> = Timeframe::all_standard()
        .iter()
        .map(|tf| tf.as_str())
        .collect();

    let tick_intervals: Vec<String> = TickInterval::all_standard()
        .iter()
        .map(|ti| ti.as_str())
        .collect();

    Json(IntervalsResponse {
        time: time_intervals,
        tick: tick_intervals,
        custom: true,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_intervals_returns_all_standard() {
        let Json(response) = get_intervals().await;
        assert_eq!(response.time.len(), 27);
        assert_eq!(response.tick.len(), 4);
        assert!(response.custom);
        assert!(response.time.contains(&"1s"));
        assert!(response.time.contains(&"1M"));
        assert!(response.tick.contains(&"100T".to_string()));
    }

    #[test]
    fn test_candle_response_serialization() {
        let candle = CandleResponse {
            time: 1772096100,
            open: 24500.0,
            high: 24520.0,
            low: 24480.0,
            close: 24510.0,
            volume: 12345,
        };

        let json = serde_json::to_string(&candle).unwrap();
        assert!(json.contains("\"time\":1772096100"));
        assert!(json.contains("\"open\":24500"));
        assert!(json.contains("\"volume\":12345"));
    }

    #[test]
    fn test_intervals_response_serialization() {
        let response = IntervalsResponse {
            time: vec!["1s", "5m", "1h"],
            tick: vec!["1T".to_string(), "10T".to_string()],
            custom: true,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"custom\":true"));
        assert!(json.contains("\"1s\""));
        assert!(json.contains("\"10T\""));
    }

    #[test]
    fn test_error_response_serialization() {
        let err = ErrorResponse {
            error: "QuestDB unavailable".to_string(),
        };

        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"error\":\"QuestDB unavailable\""));
    }
}
