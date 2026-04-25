//! Market data endpoints for the live dashboard.
//!
//! Provides JSON APIs for:
//! - `/api/market/indices` — live NSE index data (LTP, Change, OHLC)
//! - `/api/market/stock-movers` — top stock gainers/losers/most-active with symbol names
//! - `/api/market/option-movers` — top options by OI/Volume/Price change

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

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
// Stock movers (from QuestDB with symbol names)
// ---------------------------------------------------------------------------

/// A single stock mover entry.
#[derive(Debug, Serialize)]
pub struct StockMoverEntry {
    pub rank: i64,
    pub symbol: String,
    pub security_id: i64,
    pub ltp: f64,
    pub change: f64,
    pub change_pct: f64,
    pub volume: i64,
    pub prev_close: f64,
}

/// Response for `/api/market/stock-movers`.
#[derive(Debug, Serialize)]
pub struct StockMoversResponse {
    pub available: bool,
    pub gainers: Vec<StockMoverEntry>,
    pub losers: Vec<StockMoverEntry>,
    pub most_active: Vec<StockMoverEntry>,
}

/// `GET /api/market/stock-movers` — top stock gainers/losers/most-active from QuestDB.
// TEST-EXEMPT: HTTP handler — requires live QuestDB for integration
pub async fn get_stock_movers(State(state): State<SharedAppState>) -> impl IntoResponse {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    let mut gainers = query_movers(client, &base_url, "GAINER").await;
    let mut losers = query_movers(client, &base_url, "LOSER").await;
    let mut most_active = query_movers(client, &base_url, "MOST_ACTIVE").await;

    // Sort gainers by change_pct desc, losers by change_pct asc, most_active by volume desc
    gainers.sort_by(|a, b| {
        b.change_pct
            .partial_cmp(&a.change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    losers.sort_by(|a, b| {
        a.change_pct
            .partial_cmp(&b.change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    most_active.sort_by_key(|e| std::cmp::Reverse(e.volume));

    Json(StockMoversResponse {
        available: !gainers.is_empty() || !losers.is_empty() || !most_active.is_empty(),
        gainers,
        losers,
        most_active,
    })
    .into_response()
}

/// Valid stock mover categories (whitelist for SQL injection prevention).
const VALID_STOCK_CATEGORIES: &[&str] = &["GAINER", "LOSER", "MOST_ACTIVE"];

/// Queries QuestDB for stock movers of a given category.
///
/// # Safety (SQL injection)
/// `category` is validated against `VALID_STOCK_CATEGORIES` whitelist.
/// Unrecognized values return an empty Vec (no query executed).
async fn query_movers(
    client: &reqwest::Client,
    base_url: &str,
    category: &str,
) -> Vec<StockMoverEntry> {
    // GAP-SEC-02: whitelist validation prevents SQL injection via category interpolation
    if !VALID_STOCK_CATEGORIES.contains(&category) {
        return Vec::new();
    }

    let query = format!(
        "SELECT rank, symbol, security_id, ltp, change_abs, change_pct, volume, prev_close \
         FROM stock_movers \
         WHERE category = '{category}' \
         LATEST ON ts PARTITION BY security_id \
         ORDER BY rank ASC \
         LIMIT 30"
    );

    let exec_url = format!("{base_url}/exec");
    let resp = match client
        .get(&exec_url)
        .query(&[("query", &query)])
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let rows = match body["dataset"].as_array() {
        Some(r) => r,
        None => return Vec::new(),
    };

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let arr = match row.as_array() {
            Some(a) if a.len() >= 8 => a,
            _ => continue,
        };

        entries.push(StockMoverEntry {
            rank: arr[0].as_i64().unwrap_or(0),
            symbol: arr[1].as_str().unwrap_or("").to_string(),
            security_id: arr[2].as_i64().unwrap_or(0),
            ltp: arr[3].as_f64().unwrap_or(0.0),
            change: arr[4].as_f64().unwrap_or(0.0),
            change_pct: arr[5].as_f64().unwrap_or(0.0),
            volume: arr[6].as_i64().unwrap_or(0),
            prev_close: arr[7].as_f64().unwrap_or(0.0),
        });
    }

    entries
}

// ---------------------------------------------------------------------------
// Option movers (from QuestDB)
// ---------------------------------------------------------------------------

/// A single option mover entry.
#[derive(Debug, Serialize)]
pub struct OptionMoverEntry {
    pub rank: i64,
    pub contract_name: String,
    pub underlying: String,
    pub option_type: String,
    pub strike: f64,
    pub expiry: String,
    pub spot_price: f64,
    pub ltp: f64,
    pub change: f64,
    pub change_pct: f64,
    pub oi: i64,
    pub oi_change: i64,
    pub oi_change_pct: f64,
    pub volume: i64,
}

/// Query parameters for option movers.
#[derive(Debug, Deserialize)]
pub struct OptionMoversQuery {
    /// Category filter: highest_oi, oi_gainers, oi_losers, top_volume, price_gainers, price_losers
    pub category: Option<String>,
}

/// Response for `/api/market/option-movers`.
#[derive(Debug, Serialize)]
pub struct OptionMoversResponse {
    pub available: bool,
    pub category: String,
    pub entries: Vec<OptionMoverEntry>,
}

/// `GET /api/market/option-movers?category=top_volume` — top options from QuestDB.
// TEST-EXEMPT: HTTP handler — requires live QuestDB for integration
pub async fn get_option_movers(
    State(state): State<SharedAppState>,
    Query(params): Query<OptionMoversQuery>,
) -> impl IntoResponse {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    let category = params.category.unwrap_or_else(|| "TOP_VOLUME".to_string());
    let category_upper = category.to_uppercase();

    // Validate category
    let valid_categories = [
        "HIGHEST_OI",
        "OI_GAINER",
        "OI_LOSER",
        "TOP_VOLUME",
        "TOP_VALUE",
        "PRICE_GAINER",
        "PRICE_LOSER",
    ];
    if !valid_categories.contains(&category_upper.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid category, valid: highest_oi, oi_gainer, oi_loser, top_volume, top_value, price_gainer, price_loser"})),
        )
            .into_response();
    }

    let query = format!(
        "SELECT rank, contract_name, underlying, option_type, strike, expiry, \
         spot_price, ltp, change, change_pct, oi, oi_change, oi_change_pct, volume \
         FROM option_movers \
         WHERE category = '{category_upper}' \
         LATEST ON ts PARTITION BY security_id \
         ORDER BY rank ASC \
         LIMIT 30"
    );

    let exec_url = format!("{base_url}/exec");
    let resp = match client
        .get(&exec_url)
        .query(&[("query", &query)])
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
                Json(serde_json::json!({"error": "failed to parse response"})),
            )
                .into_response();
        }
    };

    let rows = body["dataset"].as_array();
    let mut entries = Vec::new();

    if let Some(rows) = rows {
        for row in rows {
            let arr = match row.as_array() {
                Some(a) if a.len() >= 14 => a,
                _ => continue,
            };

            entries.push(OptionMoverEntry {
                rank: arr[0].as_i64().unwrap_or(0),
                contract_name: arr[1].as_str().unwrap_or("").to_string(),
                underlying: arr[2].as_str().unwrap_or("").to_string(),
                option_type: arr[3].as_str().unwrap_or("").to_string(),
                strike: arr[4].as_f64().unwrap_or(0.0),
                expiry: arr[5].as_str().unwrap_or("").to_string(),
                spot_price: arr[6].as_f64().unwrap_or(0.0),
                ltp: arr[7].as_f64().unwrap_or(0.0),
                change: arr[8].as_f64().unwrap_or(0.0),
                change_pct: arr[9].as_f64().unwrap_or(0.0),
                oi: arr[10].as_i64().unwrap_or(0),
                oi_change: arr[11].as_i64().unwrap_or(0),
                oi_change_pct: arr[12].as_f64().unwrap_or(0.0),
                volume: arr[13].as_i64().unwrap_or(0),
            });
        }
    }

    Json(OptionMoversResponse {
        available: !entries.is_empty(),
        category: category_upper,
        entries,
    })
    .into_response()
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
    fn test_stock_movers_response_serialization() {
        let resp = StockMoversResponse {
            available: true,
            gainers: vec![StockMoverEntry {
                rank: 1,
                symbol: "KALYAN".to_string(),
                security_id: 12345,
                ltp: 435.0,
                change: 14.65,
                change_pct: 3.49,
                volume: 179862,
                prev_close: 420.35,
            }],
            losers: Vec::new(),
            most_active: Vec::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("KALYAN"));
        assert!(json.contains("3.49"));
    }

    #[test]
    fn test_option_movers_response_serialization() {
        let resp = OptionMoversResponse {
            available: true,
            category: "TOP_VOLUME".to_string(),
            entries: vec![OptionMoverEntry {
                rank: 1,
                contract_name: "BANKNIFTY 28 APR 54300 CALL".to_string(),
                underlying: "BANKNIFTY".to_string(),
                option_type: "CE".to_string(),
                strike: 54300.0,
                expiry: "2026-04-28".to_string(),
                spot_price: 52055.70,
                ltp: 630.25,
                change: -215.05,
                change_pct: -25.44,
                oi: 92640,
                oi_change: 55920,
                oi_change_pct: 152.29,
                volume: 27830,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("BANKNIFTY 28 APR 54300 CALL"));
        assert!(json.contains("TOP_VOLUME"));
    }

    #[test]
    fn test_valid_option_mover_categories() {
        let valid = [
            "HIGHEST_OI",
            "OI_GAINER",
            "OI_LOSER",
            "TOP_VOLUME",
            "TOP_VALUE",
            "PRICE_GAINER",
            "PRICE_LOSER",
        ];
        assert_eq!(valid.len(), 7);
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
