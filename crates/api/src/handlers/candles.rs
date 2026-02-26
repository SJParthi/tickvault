//! Candle data endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use dhan_live_trader_common::tick_types::{TickInterval, Timeframe};

use crate::state::SharedAppState;

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
#[derive(Debug, Serialize)]
pub struct CandleResponse {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u32,
}

/// Available intervals response.
#[derive(Debug, Serialize)]
pub struct IntervalsResponse {
    pub time: Vec<&'static str>,
    pub tick: Vec<String>,
    pub custom: bool,
}

/// GET /api/candles/:security_id — returns historical candle data.
///
/// For MVP, returns an empty array. QuestDB SAMPLE BY queries will be added
/// when live data flows through the pipeline.
pub async fn get_candles(
    State(_state): State<SharedAppState>,
    Path(security_id): Path<u32>,
    Query(query): Query<CandleQuery>,
) -> Result<Json<Vec<CandleResponse>>, StatusCode> {
    let _timeframe = query.timeframe.as_deref().unwrap_or("5m");
    let _from = query.from.unwrap_or(0);
    let _to = query.to.unwrap_or(i64::MAX);

    // MVP: return empty candles. In production, this queries QuestDB:
    // SELECT ts, first(ltp), max(ltp), min(ltp), last(ltp), last(volume)-first(volume)
    // FROM ticks WHERE security_id = $1 AND ts >= $2 AND ts <= $3
    // SAMPLE BY 5m ALIGN TO CALENDAR WITH OFFSET '05:30'
    tracing::debug!(
        security_id,
        timeframe = _timeframe,
        "candle query received (MVP returns empty)"
    );

    Ok(Json(vec![]))
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
}
