//! Quote endpoint — returns the latest tick for a given security from QuestDB.
//!
//! Cold-path HTTP endpoint. Queries QuestDB via its HTTP SQL API.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::state::SharedAppState;

/// Timeout for QuestDB quote queries (cold path, not tick processing).
const QUESTDB_QUOTE_TIMEOUT_SECS: u64 = 3;

/// Latest quote response for a single security.
#[derive(Debug, Serialize)]
pub struct QuoteResponse {
    pub security_id: u32,
    pub exchange_segment_code: u8,
    pub last_traded_price: f64,
    pub last_traded_quantity: u64,
    pub volume: u64,
    pub day_open: f64,
    pub day_high: f64,
    pub day_low: f64,
    pub day_close: f64,
    pub timestamp: String,
}

/// `GET /api/quote/:security_id` — fetch the latest tick from QuestDB.
pub async fn get_quote(
    State(state): State<SharedAppState>,
    Path(security_id): Path<u32>,
) -> impl IntoResponse {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_QUOTE_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to build HTTP client"})),
            )
                .into_response();
        }
    };

    match query_latest_tick(&client, &base_url, security_id).await {
        Some(quote) => (
            StatusCode::OK,
            Json(serde_json::to_value(&quote).unwrap_or_default()),
        )
            .into_response(),
        None => {
            // Distinguish between QuestDB unreachable and no data.
            // Try a simple connectivity check.
            let reachable = check_questdb_reachable(&client, &base_url).await;
            if reachable {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "no tick data found for this security_id"})),
                )
                    .into_response()
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "QuestDB is unreachable"})),
                )
                    .into_response()
            }
        }
    }
}

/// Queries QuestDB for the latest tick for a given security_id.
async fn query_latest_tick(
    client: &reqwest::Client,
    base_url: &str,
    security_id: u32,
) -> Option<QuoteResponse> {
    let sql = format!(
        "SELECT security_id, exchange_segment_code, last_traded_price, \
         last_traded_quantity, total_traded_volume, day_open, day_high, \
         day_low, day_close, ts \
         FROM ticks WHERE security_id = {security_id} \
         LATEST ON ts PARTITION BY security_id"
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
    let row = dataset.first()?.as_array()?;

    // Column order matches SELECT: security_id(0), exchange_segment_code(1),
    // ltp(2), ltq(3), volume(4), open(5), high(6), low(7), close(8), ts(9).
    Some(QuoteResponse {
        security_id: row.first()?.as_u64()? as u32,
        exchange_segment_code: row.get(1)?.as_u64()? as u8,
        last_traded_price: row.get(2)?.as_f64()?,
        last_traded_quantity: row.get(3)?.as_u64().unwrap_or(0),
        volume: row.get(4)?.as_u64().unwrap_or(0),
        day_open: row.get(5)?.as_f64().unwrap_or(0.0),
        day_high: row.get(6)?.as_f64().unwrap_or(0.0),
        day_low: row.get(7)?.as_f64().unwrap_or(0.0),
        day_close: row.get(8)?.as_f64().unwrap_or(0.0),
        timestamp: row.get(9)?.as_str().unwrap_or("").to_string(),
    })
}

/// Simple connectivity check — tries SHOW TABLES on QuestDB.
async fn check_questdb_reachable(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{base_url}/exec");
    client
        .get(&url)
        .query(&[("query", "SHOW TABLES")])
        .send()
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_response_serialization() {
        let quote = QuoteResponse {
            security_id: 12345,
            exchange_segment_code: 2,
            last_traded_price: 1500.50,
            last_traded_quantity: 100,
            volume: 50000,
            day_open: 1490.0,
            day_high: 1510.0,
            day_low: 1485.0,
            day_close: 1495.0,
            timestamp: "2026-03-08T10:30:00.000000Z".to_string(),
        };
        let json = serde_json::to_string(&quote).expect("serialization should succeed");
        assert!(json.contains("\"security_id\":12345"));
        assert!(json.contains("\"last_traded_price\":1500.5"));
        assert!(json.contains("\"timestamp\":\"2026-03-08T10:30:00.000000Z\""));
    }

    #[test]
    fn test_quote_response_debug_impl() {
        let quote = QuoteResponse {
            security_id: 99999,
            exchange_segment_code: 3,
            last_traded_price: 250.75,
            last_traded_quantity: 50,
            volume: 10000,
            day_open: 248.0,
            day_high: 252.0,
            day_low: 247.5,
            day_close: 249.0,
            timestamp: "2026-03-08T11:00:00.000000Z".to_string(),
        };
        let debug = format!("{quote:?}");
        assert!(debug.contains("QuoteResponse"));
        assert!(debug.contains("99999"));
    }

    #[tokio::test]
    async fn test_query_latest_tick_unreachable() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .expect("client build should succeed");
        let result = query_latest_tick(&client, "http://127.0.0.1:1", 12345).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_check_questdb_reachable_unreachable() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .expect("client build should succeed");
        let result = check_questdb_reachable(&client, "http://127.0.0.1:1").await;
        assert!(!result);
    }

    /// Starts a minimal HTTP server on a random port that responds with `body`.
    async fn start_mock_server(body: &'static str) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        base_url
    }

    #[tokio::test]
    async fn test_query_latest_tick_with_valid_data() {
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_some());
        let quote = result.expect("quote should be present");
        assert_eq!(quote.security_id, 12345);
        assert!((quote.last_traded_price - 1500.5).abs() < f64::EPSILON);
        assert_eq!(quote.volume, 50000);
    }

    #[tokio::test]
    async fn test_query_latest_tick_empty_dataset() {
        let body = r#"{"dataset":[]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 99999).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_malformed_json() {
        let body = "not json";
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_missing_dataset_key() {
        let body = r#"{"error":"table not found"}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }
}
