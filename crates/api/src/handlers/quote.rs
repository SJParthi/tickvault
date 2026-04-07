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

/// Builds a reqwest client with the given timeout.
///
/// `reqwest::Client::builder().build()` only fails if the TLS backend
/// cannot be initialised, which never happens at runtime. The function
/// returns a default client as ultimate fallback.
fn build_questdb_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_default()
}

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
    // SECURITY: defense-in-depth guard against invalid security_id.
    // The u32 type from Axum's Path extractor already prevents SQL injection,
    // but we reject 0 as an invalid security_id (no instrument has id=0).
    if security_id == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid security_id: must be non-zero"})),
        )
            .into_response();
    }

    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);

    let client = build_questdb_client(QUESTDB_QUOTE_TIMEOUT_SECS);

    match query_latest_tick(&client, &base_url, security_id).await {
        Some(quote) => match serde_json::to_value(&quote) {
            Ok(json) => (StatusCode::OK, Json(json)).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to serialize quote response"})),
            )
                .into_response(),
        },
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
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
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

    #[tokio::test]
    async fn test_check_questdb_reachable_returns_true() {
        let body = r#"{"columns":["tableName"],"dataset":[["ticks"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = check_questdb_reachable(&client, &base_url).await;
        assert!(result);
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_fields() {
        // Row with too few fields — should return None via bounds check
        let body = r#"{"dataset":[[12345]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_with_default_fallback_values() {
        // Row with some null fields — tests unwrap_or fallback paths (ltq, volume, OHLC)
        let body = r#"{"dataset":[[12345,2,1500.5,null,null,null,null,null,null,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_some());
        let quote = result.unwrap();
        assert_eq!(quote.last_traded_quantity, 0); // unwrap_or(0)
        assert_eq!(quote.volume, 0);
        assert!((quote.day_open - 0.0).abs() < f64::EPSILON); // unwrap_or(0.0)
    }

    // -----------------------------------------------------------------------
    // Helper: builds a SharedAppState pointing to a specific mock port
    // -----------------------------------------------------------------------

    fn mock_state(http_port: u16) -> crate::state::SharedAppState {
        use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

        crate::state::SharedAppState::new(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port,
                pg_port: 1,
                ilp_port: 1,
            },
            DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
                sandbox_base_url: String::new(),
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        )
    }

    /// Multi-request mock server that serves different responses for successive connections.
    async fn start_multi_mock_server(responses: Vec<&'static str>) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
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

    // -----------------------------------------------------------------------
    // get_quote handler: QuestDB unreachable → 503
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_questdb_unreachable_returns_503() {
        // Port 1 is unreachable — both query_latest_tick and check_questdb_reachable fail
        let state = mock_state(1);
        let response = get_quote(State(state), Path(12345)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // -----------------------------------------------------------------------
    // get_quote handler: QuestDB reachable but no data → 404
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_no_data_returns_404() {
        // First request: query_latest_tick returns empty dataset (None)
        // Second request: check_questdb_reachable succeeds (reachable)
        let responses = vec![r#"{"dataset":[]}"#, r#"{"dataset":[["ticks"]]}"#];
        let base_url = start_multi_mock_server(responses).await;
        let port: u16 = base_url
            .rsplit(':')
            .next()
            .expect("port should exist")
            .parse()
            .expect("port should parse");

        let state = mock_state(port);
        let response = get_quote(State(state), Path(99999)).await.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------
    // get_quote handler: QuestDB reachable and has data → 200
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_with_valid_data_returns_200() {
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let port: u16 = base_url
            .rsplit(':')
            .next()
            .expect("port should exist")
            .parse()
            .expect("port should parse");

        let state = mock_state(port);
        let response = get_quote(State(state), Path(12345)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // -----------------------------------------------------------------------
    // get_quote handler: timestamp field missing → None from query_latest_tick
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_null_timestamp_returns_fallback() {
        // Timestamp field is null — unwrap_or("") handles it
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000,1490.0,1510.0,1485.0,1495.0,null]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_some());
        let quote = result.expect("quote should be present");
        assert!(quote.timestamp.is_empty());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-numeric security_id field → None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_numeric_security_id_returns_none() {
        let body =
            r#"{"dataset":[["not_a_number",2,1500.5,100,50000,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-numeric exchange_segment_code → None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_numeric_exchange_code_returns_none() {
        let body = r#"{"dataset":[[12345,"x",1500.5,100,50000,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-numeric LTP → None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_numeric_ltp_returns_none() {
        let body = r#"{"dataset":[[12345,2,"bad",100,50000,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // QUESTDB_QUOTE_TIMEOUT_SECS constant check
    // -----------------------------------------------------------------------

    #[test]
    fn test_questdb_quote_timeout_secs_is_reasonable() {
        assert!(QUESTDB_QUOTE_TIMEOUT_SECS > 0);
        assert!(QUESTDB_QUOTE_TIMEOUT_SECS <= 30);
    }

    // -----------------------------------------------------------------------
    // query_latest_tick: partial row coverage — each ? branch exercised
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_2() {
        // Row with 2 elements — row.get(2)? returns None (ltp missing)
        let body = r#"{"dataset":[[12345,2]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_3() {
        // Row with 3 elements — row.get(3)? returns None (ltq missing)
        let body = r#"{"dataset":[[12345,2,1500.5]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_4() {
        // Row with 4 elements — row.get(4)? returns None (volume missing)
        let body = r#"{"dataset":[[12345,2,1500.5,100]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_5() {
        // Row with 5 elements — row.get(5)? returns None (day_open missing)
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_6() {
        // Row with 6 elements — row.get(6)? returns None (day_high missing)
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000,1490.0]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_7() {
        // Row with 7 elements — row.get(7)? returns None (day_low missing)
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000,1490.0,1510.0]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_8() {
        // Row with 8 elements — row.get(8)? returns None (day_close missing)
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000,1490.0,1510.0,1485.0]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_field_at_index_9() {
        // Row with 9 elements — row.get(9)? returns None (timestamp missing)
        let body = r#"{"dataset":[[12345,2,1500.5,100,50000,1490.0,1510.0,1485.0,1495.0]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // query_latest_tick: empty row (row.first()? returns None)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_empty_row_returns_none() {
        // Row is empty array — row.first()? returns None
        let body = r#"{"dataset":[[]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // query_latest_tick: dataset element not an array
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_dataset_element_not_array() {
        // dataset first element is a number, not an array
        let body = r#"{"dataset":[42]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // query_latest_tick: dataset is not an array
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_dataset_not_array() {
        let body = r#"{"dataset":"not_array"}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // build_questdb_client: success path
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_client_success() {
        let _client = build_questdb_client(3);
    }

    // -----------------------------------------------------------------------
    // build_questdb_client: various timeout values
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_client_zero_timeout() {
        // Zero timeout is valid for reqwest — it means no timeout
        let _client = build_questdb_client(0);
    }

    #[test]
    fn test_build_questdb_client_large_timeout() {
        let _client = build_questdb_client(3600);
    }

    // -----------------------------------------------------------------------
    // QuoteResponse: all default fallback fields (volume=0, OHLC=0.0)
    // -----------------------------------------------------------------------

    #[test]
    fn test_quote_response_with_all_zero_defaults() {
        let quote = QuoteResponse {
            security_id: 1,
            exchange_segment_code: 0,
            last_traded_price: 0.0,
            last_traded_quantity: 0,
            volume: 0,
            day_open: 0.0,
            day_high: 0.0,
            day_low: 0.0,
            day_close: 0.0,
            timestamp: String::new(),
        };
        let json = serde_json::to_string(&quote).unwrap();
        assert!(json.contains("\"security_id\":1"));
        assert!(json.contains("\"timestamp\":\"\""));
    }

    #[test]
    fn test_build_questdb_client_succeeds() {
        let _client = build_questdb_client(3);
    }

    #[test]
    fn test_build_questdb_client_with_various_timeouts() {
        let _c1 = build_questdb_client(1);
        let _c2 = build_questdb_client(30);
        let _c3 = build_questdb_client(0); // zero timeout still builds
    }

    // -----------------------------------------------------------------------
    // get_quote handler: security_id == 0 → 400 Bad Request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_zero_security_id_returns_400() {
        let state = mock_state(1);
        let response = get_quote(State(state), Path(0)).await.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
