//! Stats endpoint — proxies QuestDB queries server-side to avoid CORS.
//!
//! Returns dashboard statistics in a single JSON response:
//! table count, underlyings, derivatives, subscribed indices, ticks.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::SharedAppState;

/// Timeout for QuestDB stats queries (cold path, not tick processing).
const QUESTDB_STATS_TIMEOUT_SECS: u64 = 3;

/// Dashboard statistics response.
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub questdb_reachable: bool,
    pub tables: u64,
    pub underlyings: u64,
    pub derivatives: u64,
    pub subscribed_indices: u64,
    pub ticks: u64,
}

/// `GET /api/stats` — fetch QuestDB counts in one call.
pub async fn get_stats(State(state): State<SharedAppState>) -> Json<StatsResponse> {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_STATS_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return Json(StatsResponse {
                questdb_reachable: false,
                tables: 0,
                underlyings: 0,
                derivatives: 0,
                subscribed_indices: 0,
                ticks: 0,
            });
        }
    };

    let tables = query_count(&client, &base_url, "SHOW TABLES").await;
    let questdb_reachable = tables.is_some();

    Json(StatsResponse {
        questdb_reachable,
        tables: tables.unwrap_or(0),
        underlyings: query_count(&client, &base_url, "SELECT count() FROM fno_underlyings")
            .await
            .unwrap_or(0),
        derivatives: query_count(
            &client,
            &base_url,
            "SELECT count() FROM derivative_contracts",
        )
        .await
        .unwrap_or(0),
        subscribed_indices: query_count(
            &client,
            &base_url,
            "SELECT count() FROM subscribed_indices",
        )
        .await
        .unwrap_or(0),
        ticks: query_count(&client, &base_url, "SELECT count() FROM ticks")
            .await
            .unwrap_or(0),
    })
}

/// Runs a count query against QuestDB's HTTP endpoint. Returns None on failure.
async fn query_count(client: &reqwest::Client, base_url: &str, sql: &str) -> Option<u64> {
    let url = format!("{}/exec", base_url);
    let resp = client
        .get(&url)
        .query(&[("query", sql)])
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let dataset = body.get("dataset")?.as_array()?;

    // SHOW TABLES returns rows of [table_name], count them
    if sql.starts_with("SHOW") {
        return Some(dataset.len() as u64);
    }

    // SELECT count() returns [[N]]
    dataset.first()?.as_array()?.first()?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_response_serialization() {
        let stats = StatsResponse {
            questdb_reachable: true,
            tables: 5,
            underlyings: 214,
            derivatives: 96948,
            subscribed_indices: 31,
            ticks: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"tables\":5"));
        assert!(json.contains("\"questdb_reachable\":true"));
    }

    #[tokio::test]
    async fn test_query_count_returns_none_for_unreachable_server() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        let result = query_count(&client, "http://127.0.0.1:1", "SHOW TABLES").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_stats_returns_unreachable_when_questdb_down() {
        use crate::state::SharedAppState;
        use axum::extract::State;
        use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

        let state = SharedAppState::new(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
        );
        let result = get_stats(State(state)).await;
        assert!(!result.questdb_reachable);
        assert_eq!(result.tables, 0);
        assert_eq!(result.underlyings, 0);
        assert_eq!(result.derivatives, 0);
        assert_eq!(result.subscribed_indices, 0);
        assert_eq!(result.ticks, 0);
    }

    #[test]
    fn test_stats_response_all_zeros_serialization() {
        let stats = StatsResponse {
            questdb_reachable: false,
            tables: 0,
            underlyings: 0,
            derivatives: 0,
            subscribed_indices: 0,
            ticks: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"questdb_reachable\":false"));
        assert!(json.contains("\"tables\":0"));
    }

    /// Starts a minimal HTTP server on a random port that responds with `body`.
    async fn start_mock_server(body: &'static str) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            // Accept one connection, read until double newline, respond with body.
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
    async fn test_query_count_show_tables_counts_rows() {
        // SHOW TABLES returns rows of [table_name] — count them.
        let body = r#"{"dataset":[["ticks"],["fno_underlyings"],["derivative_contracts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SHOW TABLES").await;
        assert_eq!(result, Some(3));
    }

    #[tokio::test]
    async fn test_query_count_select_count_extracts_value() {
        // SELECT count() returns [[N]]
        let body = r#"{"dataset":[[42]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn test_query_count_empty_dataset() {
        // Empty dataset for SHOW TABLES => 0 tables
        let body = r#"{"dataset":[]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SHOW TABLES").await;
        assert_eq!(result, Some(0));
    }

    #[tokio::test]
    async fn test_query_count_select_empty_dataset_returns_none() {
        // SELECT count() with empty dataset — no first element
        let body = r#"{"dataset":[]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_missing_dataset_key_returns_none() {
        // JSON without "dataset" key
        let body = r#"{"error":"table not found"}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM missing").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_malformed_json_returns_none() {
        // Response body is not valid JSON
        let body = "not json at all";
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SHOW TABLES").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_select_non_numeric_value_returns_none() {
        // SELECT count() but the inner value is a string, not u64
        let body = r#"{"dataset":[["not_a_number"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_dataset_not_array_returns_none() {
        // "dataset" exists but is not an array
        let body = r#"{"dataset":"not_an_array"}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SHOW TABLES").await;
        assert_eq!(result, None);
    }

    // -----------------------------------------------------------------------
    // Additional query_count edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_count_select_inner_not_array_returns_none() {
        // SELECT count() but inner element is not an array — [[N]] expected but [N] given.
        let body = r#"{"dataset":[42]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_select_nested_empty_inner_array_returns_none() {
        // SELECT count() with [[]] — inner array is empty.
        let body = r#"{"dataset":[[]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_select_null_value_returns_none() {
        // SELECT count() with [[null]] — null is not u64.
        let body = r#"{"dataset":[[null]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_select_float_value_returns_none() {
        // SELECT count() with [[3.14]] — float is not u64.
        let body = r#"{"dataset":[[3.14]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_query_count_select_large_count() {
        // Large count value.
        let body = r#"{"dataset":[[9999999]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, Some(9999999));
    }

    #[tokio::test]
    async fn test_query_count_show_tables_with_many_rows() {
        // SHOW TABLES with many rows.
        let body = r#"{"dataset":[["t1"],["t2"],["t3"],["t4"],["t5"],["t6"],["t7"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SHOW TABLES").await;
        assert_eq!(result, Some(7));
    }

    #[tokio::test]
    async fn test_query_count_select_zero_count() {
        let body = r#"{"dataset":[[0]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let result = query_count(&client, &base_url, "SELECT count() FROM ticks").await;
        assert_eq!(result, Some(0));
    }

    #[tokio::test]
    async fn test_stats_response_debug_impl() {
        let stats = StatsResponse {
            questdb_reachable: true,
            tables: 3,
            underlyings: 100,
            derivatives: 5000,
            subscribed_indices: 10,
            ticks: 999999,
        };
        let debug = format!("{stats:?}");
        assert!(debug.contains("StatsResponse"));
        assert!(debug.contains("999999"));
    }

    /// Multi-request mock server that serves different responses for successive connections.
    async fn start_multi_mock_server(responses: Vec<&'static str>) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            for body in responses {
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
            }
        });

        base_url
    }

    #[tokio::test]
    async fn test_get_stats_with_mock_questdb() {
        // First query: SHOW TABLES returns 2 tables (reachable=true).
        // Subsequent queries return counts.
        let responses = vec![
            r#"{"dataset":[["ticks"],["fno_underlyings"]]}"#,
            r#"{"dataset":[[214]]}"#,
            r#"{"dataset":[[96948]]}"#,
            r#"{"dataset":[[31]]}"#,
            r#"{"dataset":[[1000000]]}"#,
        ];
        let base_url = start_multi_mock_server(responses).await;

        // We need to extract port from the mock server URL.
        let port: u16 = base_url.rsplit(':').next().unwrap().parse().unwrap();

        let state = SharedAppState::new(
            dhan_live_trader_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: port,
                pg_port: 1,
                ilp_port: 1,
            },
            dhan_live_trader_common::config::DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            dhan_live_trader_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
        );
        let result = get_stats(axum::extract::State(state)).await;
        assert!(result.questdb_reachable);
        assert_eq!(result.tables, 2);
    }
}
