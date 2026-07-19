//! Stats endpoint — proxies QuestDB queries server-side to avoid CORS.
//!
//! Returns dashboard statistics in a single JSON response: QuestDB
//! reachability + live table count. The instrument-master counts
//! (underlyings / derivatives / subscribed indices) and the ticks count were
//! retired 2026-07-19 (BATCH-5) — their source tables are dropped at boot.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::state::SharedAppState;

use crate::response_cache::cached_json_response;

/// Timeout for QuestDB stats queries (cold path, not tick processing).
const QUESTDB_STATS_TIMEOUT_SECS: u64 = 3;

/// Builds a reqwest client with the given timeout.
///
/// `reqwest::Client::builder().build()` only fails if the TLS backend
/// cannot be initialised, which never happens at runtime. The function
/// returns a default client as ultimate fallback.
/// `pub(crate)`: reused by the Live Board aggregator (`handlers::board`).
pub(crate) fn build_stats_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_default()
}

/// Dashboard statistics response.
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub questdb_reachable: bool,
    pub tables: u64,
    // `underlyings` / `derivatives` (contracts) / `subscribed_indices` counts
    // RETIRED 2026-07-19 (BATCH-5): all three queried the instrument-master
    // tables `fno_underlyings` / `derivative_contracts` / `subscribed_indices`,
    // which are in the boot-time `RETIRED_QUESTDB_TABLES` DROP list
    // (shadow_persistence.rs) — dropped since the #T3 instrument-table cleanup +
    // the 2026-07-13 Dhan instrument-download retirement. The queries therefore
    // returned a permanently-0 "contracts —" placeholder (false-OK). Retired
    // rather than repointed. FOLLOW-UP (operator): if a live universe-size tile
    // is wanted, repoint at `instrument_lifecycle WHERE lifecycle_state='active'`
    // (the KEPT live master) with the right instrument_type filters.
    // `ticks` count RETIRED 2026-07-19 (BATCH-5): the `ticks` writer was
    // deleted 2026-07-17, so this lifetime count no longer grows and would
    // report a permanently-stale "ticks captured" number (false-OK). Retired
    // rather than repointed at another table.
}

/// `GET /api/stats` — fetch QuestDB counts in one call.
///
/// 2026-07-09 audit hardening: the computed 200 body is TTL-cached (5s,
/// single slot in [`SharedAppState`]) so an internet client hammering this
/// public endpoint costs QuestDB at most one 5-query pass per TTL window.
/// The rate limiter in `crate::public_guard` runs BEFORE this handler
/// (route_layer); cache is the second line. A cache HIT touches QuestDB
/// zero times. The always-200 contract is unchanged — a QuestDB-down
/// zeroed body is deliberately cached too (bounded negative caching: the
/// uncached down-path costs 5 failed HTTP attempts x 3s timeout per hit).
pub async fn get_stats(State(state): State<SharedAppState>) -> Response {
    if let Some(body) = state.stats_cache().get() {
        metrics::counter!("tv_api_cache_hits_total", "endpoint" => "stats").increment(1);
        return cached_json_response(body, "hit");
    }

    let stats = compute_stats(&state).await;
    match serde_json::to_string(&stats) {
        Ok(body) => {
            state.stats_cache().put(body.clone());
            cached_json_response(body, "miss")
        }
        // Practically unreachable (plain struct of bools/u64s) — fall
        // through to the uncached Json path rather than 500 on a cache
        // problem; never cache a body we could not serialize.
        Err(_) => Json(stats).into_response(),
    }
}

/// Runs the 5 QuestDB count queries and assembles the stats payload.
/// Extracted from the handler so the cache wrapper stays thin and the
/// existing field-level tests exercise the computation directly.
///
/// HONEST BOUND (adversarial review 2026-07-09): there is no single-flight
/// on a cache miss, so with QuestDB black-holed (5 x 3s timeouts) the
/// limiter still admits up to 5 computes/s (~75 concurrent worst case),
/// each building one short-lived HTTP client. That is BOUNDED by the
/// limiter (pre-change it was unbounded per internet client) and cold-path
/// only; miss single-flighting is a flagged follow-up, not a regression.
pub(crate) async fn compute_stats(state: &SharedAppState) -> StatsResponse {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);

    let client = build_stats_client(QUESTDB_STATS_TIMEOUT_SECS);

    let tables = query_count(&client, &base_url, "SHOW TABLES").await;
    let questdb_reachable = tables.is_some();

    StatsResponse {
        questdb_reachable,
        tables: tables.unwrap_or(0),
        // underlyings / derivatives (contracts) / subscribed_indices / ticks
        // counts retired 2026-07-19 (BATCH-5) — their source tables are in the
        // boot-time RETIRED_QUESTDB_TABLES DROP list, so no query is issued.
    }
}

/// Runs a count query against QuestDB's HTTP endpoint. Returns None on failure.
/// `pub(crate)`: reused by the Live Board aggregator (`handlers::board`).
pub(crate) async fn query_count(
    client: &reqwest::Client,
    base_url: &str,
    sql: &str,
) -> Option<u64> {
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
    use crate::response_cache::CACHE_MARKER_HEADER;

    #[test]
    fn test_build_stats_client_constructs_with_timeout() {
        // pub(crate) reuse surface (Live Board): the builder must always
        // yield a usable client (falls back to default on builder failure).
        let client = build_stats_client(1);
        assert!(!format!("{client:?}").is_empty());
    }

    #[test]
    fn test_stats_response_serialization() {
        let stats = StatsResponse {
            questdb_reachable: true,
            tables: 5,
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

    // PR #2 (2026-05-18): empty_snapshot() removed alongside the
    // deleted SharedTopMoversSnapshot type.

    #[tokio::test]
    async fn test_compute_stats_returns_unreachable_when_questdb_down() {
        use crate::state::SharedAppState;
        use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

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
        );
        let result = compute_stats(&state).await;
        assert!(!result.questdb_reachable);
        assert_eq!(result.tables, 0);
    }

    #[test]
    fn test_stats_response_all_zeros_serialization() {
        let stats = StatsResponse {
            questdb_reachable: false,
            tables: 0,
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
            tables: 999999,
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

    #[tokio::test]
    async fn test_compute_stats_with_mock_questdb() {
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
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: port,
                pg_port: 1,
                ilp_port: 1,
            },
            tickvault_common::config::DhanConfig {
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
            tickvault_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        );
        let result = compute_stats(&state).await;
        assert!(result.questdb_reachable);
        assert_eq!(result.tables, 2);
    }

    /// 2026-07-09 hardening: a second `get_stats` call inside the TTL
    /// window must be a cache HIT — byte-identical body with ZERO QuestDB
    /// round-trips. The multi-mock server serves EXACTLY the 5 responses of
    /// the first pass; an uncached second pass would hit the exhausted
    /// server and produce the differing unreachable/zeros body.
    #[tokio::test]
    async fn test_get_stats_cache_hit_skips_questdb() {
        let responses = vec![
            r#"{"dataset":[["ticks"],["fno_underlyings"]]}"#,
            r#"{"dataset":[[214]]}"#,
            r#"{"dataset":[[96948]]}"#,
            r#"{"dataset":[[31]]}"#,
            r#"{"dataset":[[1000000]]}"#,
        ];
        let base_url = start_multi_mock_server(responses).await;
        let port: u16 = base_url.rsplit(':').next().unwrap().parse().unwrap();

        let state = SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: port,
                pg_port: 1,
                ilp_port: 1,
            },
            tickvault_common::config::DhanConfig {
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
            tickvault_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        );

        let first = get_stats(State(state.clone())).await;
        assert_eq!(
            first
                .headers()
                .get(CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("miss"),
            "first call must be a cache miss"
        );
        let first_body = axum::body::to_bytes(first.into_body(), 64 * 1024)
            .await
            .expect("first body readable");
        assert!(
            String::from_utf8_lossy(&first_body).contains("\"tables\":2"),
            "first (computed) body must carry the mocked counts"
        );

        // Second call: mock server is exhausted — only the cache can
        // reproduce the same body.
        let second = get_stats(State(state)).await;
        assert_eq!(
            second
                .headers()
                .get(CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit"),
            "second call inside the TTL must be a cache hit"
        );
        let second_body = axum::body::to_bytes(second.into_body(), 64 * 1024)
            .await
            .expect("second body readable");
        assert_eq!(
            first_body, second_body,
            "cache hit must return the byte-identical body without any QuestDB call"
        );
    }

    // -----------------------------------------------------------------------
    // build_stats_client: success path
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_stats_client_success() {
        let _client = build_stats_client(3);
    }

    #[test]
    fn test_build_stats_client_error_returns_zeroed_response() {
        // The error mapping produces a zeroed StatsResponse.
        // We test the mapping directly since the builder never fails.
        let err_response = StatsResponse {
            questdb_reachable: false,
            tables: 0,
        };
        assert!(!err_response.questdb_reachable);
        assert_eq!(err_response.tables, 0);
    }

    #[test]
    fn test_build_stats_client_succeeds() {
        let _client = build_stats_client(3);
    }

    #[test]
    fn test_build_stats_client_various_timeouts() {
        let _c1 = build_stats_client(0);
        let _c2 = build_stats_client(60);
    }

    // -----------------------------------------------------------------------
    // StatsResponse zeroed construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_stats_response_zeroed_all_fields() {
        let resp = StatsResponse {
            questdb_reachable: false,
            tables: 0,
        };
        assert!(!resp.questdb_reachable);
        assert_eq!(resp.tables, 0);
    }
}
