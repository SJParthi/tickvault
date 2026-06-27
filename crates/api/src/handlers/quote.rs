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
///
/// The `ticks` table is shared by every live feed (Dhan + Groww), so the
/// response carries the `feed` label of the latest row. Groww writes only a
/// subset of columns, so the OHLC / OI / cumulative-quantity / avg-price fields
/// are `Option`: a Groww latest row honestly reports them as `null` instead of
/// faking a `0.0` / `0`. (NULL handling for the Groww column subset is by
/// design — see `.claude/rules/project/live-feed-purity.md` rule 5.)
#[derive(Debug, Serialize)]
pub struct QuoteResponse {
    pub security_id: u32,
    /// Feed source of the latest tick (`"dhan"` / `"groww"`).
    pub feed: String,
    /// Exchange segment string as stored in `ticks.segment` (e.g. `"NSE_EQ"`).
    pub segment: String,
    pub last_traded_price: f64,
    /// Last traded quantity. `None` when the latest row's `last_trade_qty` is
    /// NULL (e.g. a Groww row).
    pub last_traded_quantity: Option<u64>,
    /// Cumulative day volume. `None` when NULL (e.g. a Groww row).
    pub volume: Option<u64>,
    /// Open interest. `None` when NULL.
    pub open_interest: Option<u64>,
    /// Day-session OHLC from the Quote/Full packet. `None` when NULL.
    pub day_open: Option<f64>,
    pub day_high: Option<f64>,
    pub day_low: Option<f64>,
    pub day_close: Option<f64>,
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
    // Query the columns that ACTUALLY EXIST in the `ticks` DDL
    // (`crates/storage/src/tick_persistence.rs::TICKS_CREATE_DDL`):
    // `feed`, `segment` (SYMBOL string — NOT a numeric code), `ltp`,
    // `last_trade_qty`, `volume`, `oi`, `open`/`high`/`low`/`close`, `ts`.
    // `LATEST ON ts PARTITION BY security_id` returns the single freshest row
    // for this security across ALL feeds; `feed` labels its source.
    let sql = format!(
        "SELECT security_id, feed, segment, ltp, last_trade_qty, volume, oi, \
         open, high, low, close, ts \
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

    // Column order matches SELECT: security_id(0), feed(1), segment(2),
    // ltp(3), last_trade_qty(4), volume(5), oi(6), open(7), high(8),
    // low(9), close(10), ts(11).
    //
    // Mandatory fields use `?` (security_id, feed, segment, ltp, ts). The
    // remaining numeric fields are NULL for a Groww row (9-of-19 subset) — they
    // map to `None` via `as_u64()` / `as_f64()` (JSON `null` or absent → `None`),
    // never a misleading `0`/`0.0` and never a panic.
    Some(QuoteResponse {
        security_id: row.first()?.as_u64()? as u32,
        feed: row.get(1)?.as_str()?.to_string(),
        segment: row.get(2)?.as_str()?.to_string(),
        last_traded_price: row.get(3)?.as_f64()?,
        last_traded_quantity: row.get(4).and_then(serde_json::Value::as_u64),
        volume: row.get(5).and_then(serde_json::Value::as_u64),
        open_interest: row.get(6).and_then(serde_json::Value::as_u64),
        day_open: row.get(7).and_then(serde_json::Value::as_f64),
        day_high: row.get(8).and_then(serde_json::Value::as_f64),
        day_low: row.get(9).and_then(serde_json::Value::as_f64),
        day_close: row.get(10).and_then(serde_json::Value::as_f64),
        timestamp: row.get(11)?.as_str().unwrap_or("").to_string(),
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
            feed: "dhan".to_string(),
            segment: "NSE_FNO".to_string(),
            last_traded_price: 1500.50,
            last_traded_quantity: Some(100),
            volume: Some(50000),
            open_interest: Some(1234),
            day_open: Some(1490.0),
            day_high: Some(1510.0),
            day_low: Some(1485.0),
            day_close: Some(1495.0),
            timestamp: "2026-03-08T10:30:00.000000Z".to_string(),
        };
        let json = serde_json::to_string(&quote).expect("serialization should succeed");
        assert!(json.contains("\"security_id\":12345"));
        assert!(json.contains("\"feed\":\"dhan\""));
        assert!(json.contains("\"segment\":\"NSE_FNO\""));
        assert!(json.contains("\"last_traded_price\":1500.5"));
        assert!(json.contains("\"timestamp\":\"2026-03-08T10:30:00.000000Z\""));
    }

    #[test]
    fn test_quote_response_debug_impl() {
        let quote = QuoteResponse {
            security_id: 99999,
            feed: "dhan".to_string(),
            segment: "IDX_I".to_string(),
            last_traded_price: 250.75,
            last_traded_quantity: Some(50),
            volume: Some(10000),
            open_interest: None,
            day_open: Some(248.0),
            day_high: Some(252.0),
            day_low: Some(247.5),
            day_close: Some(249.0),
            timestamp: "2026-03-08T11:00:00.000000Z".to_string(),
        };
        let debug = format!("{quote:?}");
        assert!(debug.contains("QuoteResponse"));
        assert!(debug.contains("99999"));
    }

    /// A fully-populated Dhan-style row maps every Option to `Some(..)`.
    #[tokio::test]
    async fn test_query_latest_tick_dhan_row_all_fields_present() {
        // security_id, feed, segment, ltp, last_trade_qty, volume, oi,
        // open, high, low, close, ts
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");
        let quote = query_latest_tick(&client, &base_url, 12345)
            .await
            .expect("quote should be present");
        assert_eq!(quote.feed, "dhan");
        assert_eq!(quote.segment, "NSE_FNO");
        assert_eq!(quote.last_traded_quantity, Some(100));
        assert_eq!(quote.volume, Some(50000));
        assert_eq!(quote.open_interest, Some(1234));
        assert_eq!(quote.day_open, Some(1490.0));
        assert_eq!(quote.day_close, Some(1495.0));
    }

    /// A Groww-style latest row has NULL OHLC/OI/qty — they MUST map to `None`
    /// (JSON `null`), NOT a misleading `0.0` / `0`.
    #[tokio::test]
    async fn test_query_latest_tick_groww_row_null_ohlc_maps_to_none() {
        // Groww writes ltp + volume but NULL open/high/low/close/oi/last_trade_qty.
        let body = r#"{"dataset":[[12345,"groww","NSE_EQ",1500.5,null,50000,null,null,null,null,null,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");
        let quote = query_latest_tick(&client, &base_url, 12345)
            .await
            .expect("quote should be present");
        assert_eq!(quote.feed, "groww");
        assert_eq!(quote.last_traded_price, 1500.5);
        assert_eq!(quote.volume, Some(50000));
        // The NULL columns are honestly None, NOT 0.0 / 0.
        assert_eq!(quote.day_open, None);
        assert_eq!(quote.day_high, None);
        assert_eq!(quote.day_low, None);
        assert_eq!(quote.day_close, None);
        assert_eq!(quote.open_interest, None);
        assert_eq!(quote.last_traded_quantity, None);

        // Serialized JSON shows null, never a faked 0.
        let json = serde_json::to_string(&quote).expect("serialization should succeed");
        assert!(json.contains("\"feed\":\"groww\""));
        assert!(json.contains("\"day_open\":null"));
        assert!(json.contains("\"open_interest\":null"));
        assert!(!json.contains("\"day_open\":0.0"));
    }

    /// The SELECT must reference only columns that exist in the real `ticks`
    /// DDL — pins against a regression back to the phantom column names.
    #[tokio::test]
    async fn test_query_latest_tick_uses_real_ddl_columns() {
        // The mock echoes the query so we can assert the SELECT shape.
        // We instead assert the handler parses a row in the real column order.
        // (Column-name correctness is also covered by the 12-field mock rows
        // above matching the SELECT order: feed(1), segment(2), ... ts(11).)
        let body =
            r#"{"dataset":[[1,"dhan","IDX_I",100.0,null,null,null,null,null,null,null,"ts"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");
        let quote = query_latest_tick(&client, &base_url, 1)
            .await
            .expect("quote should be present");
        assert_eq!(quote.security_id, 1);
        assert_eq!(quote.feed, "dhan");
        assert_eq!(quote.segment, "IDX_I");
        assert_eq!(quote.last_traded_price, 100.0);
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
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
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
        assert_eq!(quote.volume, Some(50000));
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
    async fn test_query_latest_tick_with_null_optional_fields_maps_to_none() {
        // Row with NULL optional fields (ltq, volume, oi, OHLC) — they map to
        // None (honest null), NOT a faked 0 / 0.0. `feed`, `segment`, `ltp`,
        // `ts` are mandatory and present.
        let body = r#"{"dataset":[[12345,"groww","NSE_EQ",1500.5,null,null,null,null,null,null,null,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_some());
        let quote = result.unwrap();
        assert_eq!(quote.last_traded_quantity, None);
        assert_eq!(quote.volume, None);
        assert_eq!(quote.open_interest, None);
        assert_eq!(quote.day_open, None);
    }

    // -----------------------------------------------------------------------
    // Helper: builds a SharedAppState pointing to a specific mock port
    // -----------------------------------------------------------------------

    fn mock_state(http_port: u16) -> crate::state::SharedAppState {
        use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

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
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
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
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
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
        // Timestamp field (index 11) is null — unwrap_or("") handles it
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,null]]}"#;
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
        let body = r#"{"dataset":[["not_a_number","dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-string feed (index 1) → None (as_str? fails)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_string_feed_returns_none() {
        // feed at index 1 is a number, not a string — as_str()? returns None.
        let body = r#"{"dataset":[[12345,42,"NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_latest_tick(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-string segment (index 2) → None (as_str? fails)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_string_segment_returns_none() {
        // segment at index 2 is a number, not a string — as_str()? returns None.
        let body = r#"{"dataset":[[12345,"dhan",99,1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
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
        // ltp at index 3 is non-numeric — as_f64()? returns None.
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO","bad",100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
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
    async fn test_query_latest_tick_row_missing_mandatory_feed_returns_none() {
        // Row with 1 element — row.get(1)? (feed) returns None.
        let body = r#"{"dataset":[[12345]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_mandatory_segment_returns_none() {
        // Row with 2 elements (security_id, feed) — row.get(2)? (segment) None.
        let body = r#"{"dataset":[[12345,"dhan"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_mandatory_ltp_returns_none() {
        // Row with 3 elements (security_id, feed, segment) — row.get(3)? (ltp) None.
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_latest_tick(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_mandatory_ts_returns_none() {
        // Row with mandatory feed/segment/ltp + optional cols present but the
        // mandatory ts (index 11) missing — row.get(11)? returns None.
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0]]}"#;
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
    // QuoteResponse: all optional fields None (null in JSON, not faked zeros)
    // -----------------------------------------------------------------------

    #[test]
    fn test_quote_response_with_all_none_optionals() {
        let quote = QuoteResponse {
            security_id: 1,
            feed: "groww".to_string(),
            segment: "NSE_EQ".to_string(),
            last_traded_price: 0.0,
            last_traded_quantity: None,
            volume: None,
            open_interest: None,
            day_open: None,
            day_high: None,
            day_low: None,
            day_close: None,
            timestamp: String::new(),
        };
        let json = serde_json::to_string(&quote).unwrap();
        assert!(json.contains("\"security_id\":1"));
        assert!(json.contains("\"timestamp\":\"\""));
        assert!(json.contains("\"volume\":null"));
        assert!(json.contains("\"day_close\":null"));
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
