//! Integration test: API router smoke tests.
//!
//! Builds the full axum router with test state and verifies
//! that the health endpoint responds correctly. No live services needed.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use dhan_live_trader_api::build_router;
use dhan_live_trader_api::state::SharedAppState;
use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};
use dhan_live_trader_common::tick_types::ParsedTick;

fn test_state() -> SharedAppState {
    let (tx, _) = tokio::sync::broadcast::channel::<ParsedTick>(16);
    SharedAppState::new(
        QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        },
        DhanConfig {
            websocket_url: "wss://test".to_string(),
            rest_api_base_url: "https://test".to_string(),
            auth_base_url: "https://test".to_string(),
            instrument_csv_url: "https://test/csv".to_string(),
            instrument_csv_fallback_url: "https://test/csv2".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
        },
        InstrumentConfig {
            daily_download_time: "06:30:00".to_string(),
            csv_cache_directory: "/tmp".to_string(),
            csv_cache_filename: "test.csv".to_string(),
            csv_download_timeout_secs: 30,
            build_window_start: "06:00:00".to_string(),
            build_window_end: "09:15:00".to_string(),
        },
        tx,
    )
}

#[tokio::test]
async fn health_endpoint_returns_200() {
    let router = build_router(test_state());
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let router = build_router(test_state());
    let request = Request::builder()
        .uri("/nonexistent")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn portal_endpoint_returns_200() {
    let router = build_router(test_state());
    let request = Request::builder()
        .uri("/portal")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
