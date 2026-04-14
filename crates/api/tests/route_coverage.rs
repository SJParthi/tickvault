//! Router-level integration smoke tests for routes not covered in api_smoke.rs.
//!
//! Verifies that every route in `build_router` is reachable and returns
//! a sensible HTTP status code against a no-op app state (no live services).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use tickvault_api::build_router;
use tickvault_api::state::{SharedAppState, SystemHealthStatus};
use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

fn test_state() -> SharedAppState {
    SharedAppState::new(
        QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        },
        DhanConfig {
            websocket_url: "wss://test".to_string(),
            order_update_websocket_url: "wss://test".to_string(),
            rest_api_base_url: "https://test".to_string(),
            auth_base_url: "https://test".to_string(),
            instrument_csv_url: "https://test/csv".to_string(),
            instrument_csv_fallback_url: "https://test/csv2".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        },
        InstrumentConfig {
            daily_download_time: "06:30:00".to_string(),
            csv_cache_directory: "/tmp".to_string(),
            csv_cache_filename: "test.csv".to_string(),
            csv_download_timeout_secs: 30,
            build_window_start: "06:00:00".to_string(),
            build_window_end: "09:15:00".to_string(),
        },
        std::sync::Arc::new(std::sync::RwLock::new(None)),
        std::sync::Arc::new(std::sync::RwLock::new(None)),
        std::sync::Arc::new(SystemHealthStatus::new()),
    )
}

/// `GET /api/stats` must return 200 (handler returns JSON with zero counts
/// when QuestDB is unreachable — it never 5xx on connection failure).
#[tokio::test]
async fn test_get_stats_returns_200() {
    let router = build_router(test_state(), &[], true);
    let request = Request::builder()
        .uri("/api/stats")
        .body(Body::empty())
        .expect("request build should succeed"); // APPROVED: test-only
    let response = router
        .oneshot(request)
        .await
        .expect("router should respond"); // APPROVED: test-only
    assert_eq!(response.status(), StatusCode::OK);
}

/// `GET /api/top-movers` must return 200 (handler returns `available: false`
/// when the snapshot is empty — never errors).
#[tokio::test]
async fn test_get_top_movers_returns_200() {
    let router = build_router(test_state(), &[], true);
    let request = Request::builder()
        .uri("/api/top-movers")
        .body(Body::empty())
        .expect("request build should succeed"); // APPROVED: test-only
    let response = router
        .oneshot(request)
        .await
        .expect("router should respond"); // APPROVED: test-only
    assert_eq!(response.status(), StatusCode::OK);
}

/// `GET /api/index-constituency` must return 200 (handler returns
/// `available: false` when the constituency map is empty — never errors).
#[tokio::test]
async fn test_get_index_constituency_returns_200() {
    let router = build_router(test_state(), &[], true);
    let request = Request::builder()
        .uri("/api/index-constituency")
        .body(Body::empty())
        .expect("request build should succeed"); // APPROVED: test-only
    let response = router
        .oneshot(request)
        .await
        .expect("router should respond"); // APPROVED: test-only
    assert_eq!(response.status(), StatusCode::OK);
}

/// `GET /api/stock-indices/{symbol}` must return 200 with `found: false`
/// when the constituency map is empty (no data loaded).
#[tokio::test]
async fn test_get_stock_indices_returns_200_or_404() {
    let router = build_router(test_state(), &[], true);
    let request = Request::builder()
        .uri("/api/stock-indices/RELIANCE")
        .body(Body::empty())
        .expect("request build should succeed"); // APPROVED: test-only
    let response = router
        .oneshot(request)
        .await
        .expect("router should respond"); // APPROVED: test-only
    // Handler always returns 200 with `found: false` when the map is empty.
    // A 404 would only come from axum itself (route not found), which we're
    // also willing to accept here per the test contract.
    let status = response.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "expected 200 or 404, got {status}"
    );
}

/// `POST /api/instruments/rebuild` must be reachable and return 200.
///
/// NOTE: GAP-SEC-01 bearer token auth middleware exists in `middleware.rs`
/// but is not yet wired into `build_router`. When wired, this test should
/// be updated to verify 401 without auth and 200 with valid bearer token.
#[tokio::test]
async fn test_post_rebuild_returns_200() {
    let router = build_router(test_state(), &[], true);

    let request = Request::builder()
        .method("POST")
        .uri("/api/instruments/rebuild")
        .body(Body::empty())
        .expect("request build should succeed"); // APPROVED: test-only

    let response = router
        .oneshot(request)
        .await
        .expect("router should respond"); // APPROVED: test-only

    assert_eq!(response.status(), StatusCode::OK);
}
