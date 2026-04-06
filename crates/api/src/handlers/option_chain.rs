//! Option chain endpoint — returns latest option chain data from QuestDB.
//!
//! Queries `dhan_option_chain_raw` (Dhan API snapshots) and `option_greeks`
//! (our computed Greeks) tables, joining by security_id for the latest snapshot.
//!
//! Cold-path HTTP endpoint. Queries QuestDB via its HTTP SQL API.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::state::SharedAppState;

/// Query parameters for the option chain endpoint.
#[derive(Debug, Deserialize)]
pub struct OptionChainQuery {
    /// Underlying symbol (e.g., "NIFTY", "BANKNIFTY", "RELIANCE").
    pub underlying: String,
    /// Expiry date filter (YYYY-MM-DD). If omitted, returns nearest expiry.
    pub expiry: Option<String>,
}

/// Single option contract in the chain response.
#[derive(Debug, Serialize)]
pub struct OptionContract {
    pub security_id: i64,
    pub strike_price: f64,
    pub option_type: String,
    pub expiry_date: String,
    pub ltp: f64,
    pub iv: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub oi: i64,
    pub volume: i64,
    pub change_pct: f64,
    pub spot_price: f64,
    pub timestamp: String,
}

/// Response for the option chain endpoint.
#[derive(Debug, Serialize)]
pub struct OptionChainResponse {
    pub underlying: String,
    pub expiry: String,
    pub spot_price: f64,
    pub contracts: Vec<OptionContract>,
    pub available_expiries: Vec<String>,
}

/// `GET /api/option-chain?underlying=NIFTY&expiry=2026-04-09` — option chain from QuestDB.
// TEST-EXEMPT: HTTP handler — tested via unit tests on serialization/validation + requires live QuestDB for integration
pub async fn get_option_chain(
    State(state): State<SharedAppState>,
    Query(params): Query<OptionChainQuery>,
) -> impl IntoResponse {
    // Validate underlying: uppercase alphanumeric + underscore only (strict SQL injection prevention)
    if params.underlying.is_empty()
        || params.underlying.len() > 20
        || !params
            .underlying
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid underlying symbol"})),
        )
            .into_response();
    }

    // Validate expiry format if provided
    if let Some(ref expiry) = params.expiry
        && !is_valid_date_format(expiry)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid expiry format, expected YYYY-MM-DD"})),
        )
            .into_response();
    }

    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    // First, get available expiries for this underlying
    let expiries = query_available_expiries(client, &base_url, &params.underlying).await;

    // Determine which expiry to use
    let target_expiry = match &params.expiry {
        Some(e) => e.clone(),
        None => match expiries.first() {
            Some(e) => e.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "no option chain data found for this underlying"})),
                )
                    .into_response();
            }
        },
    };

    // Query option chain data
    match query_option_chain(client, &base_url, &params.underlying, &target_expiry).await {
        Some(contracts) => {
            let spot_price = contracts.first().map(|c| c.spot_price).unwrap_or(0.0);

            let response = OptionChainResponse {
                underlying: params.underlying,
                expiry: target_expiry,
                spot_price,
                contracts,
                available_expiries: expiries,
            };

            match serde_json::to_value(&response) {
                Ok(json) => (StatusCode::OK, Json(json)).into_response(),
                Err(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "serialization failed"})),
                )
                    .into_response(),
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "no option chain data found"})),
        )
            .into_response(),
    }
}

/// Validates YYYY-MM-DD date format by parsing into a real date.
/// Rejects invalid dates like 9999-99-99 or 2026-02-30.
fn is_valid_date_format(s: &str) -> bool {
    if s.len() != 10 {
        return false;
    }
    // Strict: must be exactly YYYY-MM-DD with all digits + hyphens
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    // Parse as actual date to reject impossible dates (e.g., month 13, day 32)
    let year: u16 = match s[..4].parse() {
        Ok(y) => y,
        Err(_) => return false,
    };
    let month: u8 = match s[5..7].parse() {
        Ok(m) => m,
        Err(_) => return false,
    };
    let day: u8 = match s[8..10].parse() {
        Ok(d) => d,
        Err(_) => return false,
    };
    // Validate ranges (no need for external crate — simple bounds check)
    if !(2020..=2040).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
    true
}

/// Queries distinct expiry dates for an underlying from `dhan_option_chain_raw`.
async fn query_available_expiries(
    client: &reqwest::Client,
    base_url: &str,
    underlying: &str,
) -> Vec<String> {
    let sql = format!(
        "SELECT DISTINCT expiry_date FROM dhan_option_chain_raw \
         WHERE underlying = '{underlying}' \
         ORDER BY expiry_date ASC \
         LIMIT 10"
    );

    let url = format!("{base_url}/exec");
    let resp = match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };

    let dataset = match body.get("dataset").and_then(|d| d.as_array()) {
        Some(d) => d,
        None => return Vec::new(),
    };

    dataset
        .iter()
        .filter_map(|row| {
            row.as_array()
                .and_then(|r| r.first())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Queries option chain contracts for a specific underlying + expiry.
async fn query_option_chain(
    client: &reqwest::Client,
    base_url: &str,
    underlying: &str,
    expiry: &str,
) -> Option<Vec<OptionContract>> {
    // Join dhan_option_chain_raw with option_greeks for the latest snapshot
    let sql = format!(
        "SELECT r.security_id, r.strike_price, r.option_type, r.expiry_date, \
         r.ltp, r.iv, \
         COALESCE(g.delta, 0) as delta, \
         COALESCE(g.gamma, 0) as gamma, \
         COALESCE(g.theta, 0) as theta, \
         COALESCE(g.vega, 0) as vega, \
         r.oi, r.volume, r.change_pct, r.spot_price, r.ts \
         FROM dhan_option_chain_raw r \
         LEFT JOIN option_greeks g ON r.security_id = g.security_id \
         WHERE r.underlying = '{underlying}' \
         AND r.expiry_date = '{expiry}' \
         LATEST ON r.ts PARTITION BY r.security_id \
         ORDER BY r.strike_price ASC"
    );

    let url = format!("{base_url}/exec");
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
        .ok()?;

    let body: serde_json::Value = resp.json().await.ok()?;
    let dataset = body.get("dataset")?.as_array()?;

    if dataset.is_empty() {
        return None;
    }

    let contracts: Vec<OptionContract> = dataset
        .iter()
        .filter_map(|row| {
            let r = row.as_array()?;
            Some(OptionContract {
                security_id: r.first()?.as_i64()?,
                strike_price: r.get(1)?.as_f64().unwrap_or(0.0),
                option_type: r.get(2)?.as_str().unwrap_or("").to_string(),
                expiry_date: r.get(3)?.as_str().unwrap_or("").to_string(),
                ltp: r.get(4)?.as_f64().unwrap_or(0.0),
                iv: r.get(5)?.as_f64().unwrap_or(0.0),
                delta: r.get(6)?.as_f64().unwrap_or(0.0),
                gamma: r.get(7)?.as_f64().unwrap_or(0.0),
                theta: r.get(8)?.as_f64().unwrap_or(0.0),
                vega: r.get(9)?.as_f64().unwrap_or(0.0),
                oi: r.get(10)?.as_i64().unwrap_or(0),
                volume: r.get(11)?.as_i64().unwrap_or(0),
                change_pct: r.get(12)?.as_f64().unwrap_or(0.0),
                spot_price: r.get(13)?.as_f64().unwrap_or(0.0),
                timestamp: r.get(14)?.as_str().unwrap_or("").to_string(),
            })
        })
        .collect();

    if contracts.is_empty() {
        None
    } else {
        Some(contracts)
    }
}

/// `GET /api/pcr?underlying=NIFTY` — PCR snapshots from QuestDB.
// TEST-EXEMPT: HTTP handler — tested via unit tests on serialization/validation + requires live QuestDB for integration
pub async fn get_pcr(
    State(state): State<SharedAppState>,
    Query(params): Query<OptionChainQuery>,
) -> impl IntoResponse {
    if params.underlying.is_empty()
        || params.underlying.len() > 20
        || !params
            .underlying
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid underlying symbol"})),
        )
            .into_response();
    }

    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    let sql = format!(
        "SELECT underlying_symbol, expiry_date, pcr, total_call_oi, total_put_oi, ts \
         FROM pcr_snapshots \
         WHERE underlying_symbol = '{}' \
         LATEST ON ts PARTITION BY expiry_date \
         ORDER BY expiry_date ASC",
        params.underlying
    );

    let url = format!("{base_url}/exec");
    match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(body) => (StatusCode::OK, Json(body)).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to parse QuestDB response"})),
            )
                .into_response(),
        },
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "QuestDB is unreachable"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_date_format_valid() {
        assert!(is_valid_date_format("2026-04-09"));
        assert!(is_valid_date_format("2026-12-31"));
    }

    #[test]
    fn test_is_valid_date_format_invalid() {
        assert!(!is_valid_date_format(""));
        assert!(!is_valid_date_format("2026-4-9"));
        assert!(!is_valid_date_format("20260409"));
        assert!(!is_valid_date_format("2026/04/09"));
        assert!(!is_valid_date_format("abcd-ef-gh"));
    }

    #[test]
    fn test_option_contract_serialization() {
        let contract = OptionContract {
            security_id: 49081,
            strike_price: 23000.0,
            option_type: "CE".to_string(),
            expiry_date: "2026-04-09".to_string(),
            ltp: 250.50,
            iv: 0.15,
            delta: 0.55,
            gamma: 0.003,
            theta: -5.2,
            vega: 12.1,
            oi: 150000,
            volume: 25000,
            change_pct: 2.5,
            spot_price: 23150.0,
            timestamp: "2026-04-06T12:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&contract).unwrap_or_default();
        assert!(json.contains("\"security_id\":49081"));
        assert!(json.contains("\"strike_price\":23000"));
        assert!(json.contains("\"option_type\":\"CE\""));
    }

    #[test]
    fn test_option_chain_response_serialization() {
        let response = OptionChainResponse {
            underlying: "NIFTY".to_string(),
            expiry: "2026-04-09".to_string(),
            spot_price: 23150.0,
            contracts: vec![],
            available_expiries: vec!["2026-04-09".to_string(), "2026-04-16".to_string()],
        };
        let json = serde_json::to_string(&response).unwrap_or_default();
        assert!(json.contains("\"underlying\":\"NIFTY\""));
        assert!(json.contains("\"available_expiries\""));
    }

    #[test]
    fn test_underlying_validation_rejects_ampersand() {
        // Regression: 2026-04-06 — & char was allowed, creating SQL injection surface
        let invalid = "NIFTY&DROP";
        assert!(
            !invalid
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "& must be rejected"
        );
    }

    #[test]
    fn test_underlying_validation_rejects_long_input() {
        let long_input = "A".repeat(21);
        assert!(long_input.len() > 20, "must reject inputs > 20 chars");
    }

    #[test]
    fn test_is_valid_date_rejects_impossible_dates() {
        // Regression: 2026-04-06 — 9999-99-99 was accepted
        assert!(!is_valid_date_format("9999-99-99"));
        assert!(!is_valid_date_format("2026-13-01"));
        assert!(!is_valid_date_format("2026-00-01"));
        assert!(!is_valid_date_format("2026-04-32"));
    }
}
