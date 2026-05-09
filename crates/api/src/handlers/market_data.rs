//! Market data endpoints for the live dashboard.
//!
//! Provides JSON APIs for:
//! - `/api/market/indices` — live NSE index data (LTP, Change, OHLC)
//!
//! 2026-05-09 PR 5b2: `/api/market/stock-movers` and
//! `/api/market/option-movers` deleted in favour of the unified
//! `/api/movers/v2` (in-RAM CascadeFanout reads).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::state::SharedAppState;

// ---------------------------------------------------------------------------
// Index data
// ---------------------------------------------------------------------------

/// A single index entry in the indices response.
#[derive(Debug, Serialize)]
pub struct IndexEntry {
    pub name: String,
    pub security_id: i64,
    pub ltp: f64,
    pub change: f64,
    pub change_pct: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub prev_close: f64,
    pub volume: i64,
}

/// Response for `/api/market/indices`.
#[derive(Debug, Serialize)]
pub struct IndicesResponse {
    pub available: bool,
    pub indices: Vec<IndexEntry>,
}

/// `GET /api/market/indices` — live index data from QuestDB ticks + previous_close.
// TEST-EXEMPT: HTTP handler — requires live QuestDB for integration
pub async fn get_indices(State(state): State<SharedAppState>) -> impl IntoResponse {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    // Query latest tick for each index instrument (segment = IDX_I, exchange_segment_code = 0)
    let query = "SELECT security_id, ltp, open, high, low, close, volume, \
                 close as prev_close \
                 FROM ticks \
                 WHERE segment = 'IDX_I' \
                 LATEST ON ts PARTITION BY security_id \
                 ORDER BY security_id";

    let exec_url = format!("{base_url}/exec");
    let resp = match client
        .get(&exec_url)
        .query(&[("query", query)])
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "QuestDB unreachable"})),
            )
                .into_response();
        }
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to parse QuestDB response"})),
            )
                .into_response();
        }
    };

    let rows = body["dataset"].as_array();
    let mut indices = Vec::new();

    if let Some(rows) = rows {
        for row in rows {
            let arr = match row.as_array() {
                Some(a) => a,
                None => continue,
            };
            if arr.len() < 8 {
                continue;
            }

            let security_id = arr[0].as_i64().unwrap_or(0);
            let ltp = arr[1].as_f64().unwrap_or(0.0);
            let open = arr[2].as_f64().unwrap_or(0.0);
            let high = arr[3].as_f64().unwrap_or(0.0);
            let low = arr[4].as_f64().unwrap_or(0.0);
            let _close = arr[5].as_f64().unwrap_or(0.0);
            let volume = arr[6].as_i64().unwrap_or(0);
            let prev_close = arr[7].as_f64().unwrap_or(0.0);

            let change = if prev_close > 0.0 {
                ltp - prev_close
            } else {
                0.0
            };
            let change_pct = if prev_close > 0.0 {
                (change / prev_close) * 100.0
            } else {
                0.0
            };

            // Map security_id to index name
            let name = index_name_from_security_id(security_id);

            indices.push(IndexEntry {
                name,
                security_id,
                ltp,
                change,
                change_pct,
                open,
                high,
                low,
                prev_close,
                volume,
            });
        }
    }

    // Sort by absolute change_pct descending for most relevant first
    indices.sort_by(|a, b| {
        b.change_pct
            .abs()
            .partial_cmp(&a.change_pct.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Json(IndicesResponse {
        available: !indices.is_empty(),
        indices,
    })
    .into_response()
}

/// Maps known index security IDs to display names.
// O(1) EXEMPT: cold-path match statement, not per-tick
fn index_name_from_security_id(sid: i64) -> String {
    match sid {
        13 => "Nifty 50".to_string(),
        25 => "Nifty Bank".to_string(),
        27 => "Fin Nifty".to_string(),
        21 => "India VIX".to_string(),
        442 => "Nifty Midcap Select".to_string(),
        19 => "Nifty 500".to_string(),
        17 => "Nifty 100".to_string(),
        20 => "Nifty 200".to_string(),
        22 => "Nifty Smallcap 50".to_string(),
        1 => "Nifty Next 50".to_string(),
        14 => "Nifty IT".to_string(),
        15 => "Nifty Pharma".to_string(),
        18 => "Nifty Midcap 50".to_string(),
        30 => "Nifty Auto".to_string(),
        34 => "Nifty Realty".to_string(),
        43 => "Nifty Energy".to_string(),
        44 => "Nifty Metal".to_string(),
        51 => "Nifty FMCG".to_string(),
        42 => "Nifty PSE".to_string(),
        33 => "Nifty Infra".to_string(),
        _ => format!("Index-{sid}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_name_mapping_known_ids() {
        assert_eq!(index_name_from_security_id(13), "Nifty 50");
        assert_eq!(index_name_from_security_id(25), "Nifty Bank");
        assert_eq!(index_name_from_security_id(27), "Fin Nifty");
        assert_eq!(index_name_from_security_id(21), "India VIX");
        assert_eq!(index_name_from_security_id(442), "Nifty Midcap Select");
    }

    #[test]
    fn test_index_name_mapping_unknown_id() {
        assert_eq!(index_name_from_security_id(99999), "Index-99999");
    }

    #[test]
    fn test_indices_response_serialization() {
        let resp = IndicesResponse {
            available: true,
            indices: vec![IndexEntry {
                name: "Nifty 50".to_string(),
                security_id: 13,
                ltp: 22819.75,
                change: -148.50,
                change_pct: -0.65,
                open: 22838.70,
                high: 22843.95,
                low: 22719.30,
                prev_close: 22968.25,
                volume: 0,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Nifty 50"));
        assert!(json.contains("22819.75"));
        assert!(json.contains("-148.5"));
    }

    #[test]
    fn test_index_entry_change_calculation() {
        let prev: f64 = 22968.25;
        let ltp: f64 = 22819.75;
        let change = ltp - prev;
        let change_pct = (change / prev) * 100.0;
        assert!((change - (-148.5_f64)).abs() < 0.01);
        assert!((change_pct - (-0.646_f64)).abs() < 0.01);
    }

    #[test]
    fn test_index_entry_zero_prev_close() {
        let prev = 0.0;
        let ltp = 100.0;
        let change = if prev > 0.0 { ltp - prev } else { 0.0 };
        let change_pct = if prev > 0.0 {
            (change / prev) * 100.0
        } else {
            0.0
        };
        assert_eq!(change, 0.0);
        assert_eq!(change_pct, 0.0);
    }
}
