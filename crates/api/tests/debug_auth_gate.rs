//! Security trim 2026-07-04: router-level tests proving the 4 read-only
//! `/api/debug/*` routes are gated behind the bearer-auth middleware.
//!
//! Contract (operator directive, Session A, 2026-07-04):
//! - auth ENABLED + no `Authorization` header  → 401
//! - auth ENABLED + wrong token                → 401
//! - auth ENABLED + correct `Bearer <token>`   → NOT 401 (200 or 404 —
//!   the handlers touch the filesystem, so exact status depends on state)
//! - auth DISABLED (dry-run / no token)        → NOT 401 (passthrough —
//!   preserves the tickvault-logs MCP read-only local contract)
//! - non-debug public routes (`/health`) stay 200 with NO auth even while
//!   auth is enabled.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use tickvault_api::build_router_with_auth;
use tickvault_api::middleware::ApiAuthConfig;
use tickvault_api::state::{SharedAppState, SystemHealthStatus};
use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

const DEBUG_PATHS: [&str; 4] = [
    "/api/debug/logs/summary",
    "/api/debug/logs/jsonl/latest",
    "/api/debug/spill/status",
    "/api/debug/cross-verify/latest",
];

const TEST_TOKEN: &str = "debug-gate-test-token";

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
        std::sync::Arc::new(SystemHealthStatus::new()),
    )
}

/// Router with bearer auth ENABLED (a real token configured).
fn enabled_router() -> axum::Router {
    build_router_with_auth(
        test_state(),
        &[],
        ApiAuthConfig::new(TEST_TOKEN.to_owned()),
        true,
    )
}

/// Router with bearer auth DISABLED (dry-run / no token — passthrough).
fn disabled_router() -> axum::Router {
    build_router_with_auth(test_state(), &[], ApiAuthConfig::disabled(), true)
}

async fn get_status(router: axum::Router, path: &str, auth_header: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().uri(path);
    if let Some(value) = auth_header {
        builder = builder.header("Authorization", value);
    }
    let request = builder
        .body(Body::empty())
        .expect("request build should succeed"); // APPROVED: test-only
    router
        .oneshot(request)
        .await
        .expect("router should respond") // APPROVED: test-only
        .status()
}

/// Auth enabled + NO Authorization header → 401 on every debug route.
#[tokio::test]
async fn test_debug_routes_401_without_token() {
    for path in DEBUG_PATHS {
        let status = get_status(enabled_router(), path, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} must 401 without a token when auth is enabled"
        );
    }
}

/// Auth enabled + WRONG token → 401 on every debug route.
#[tokio::test]
async fn test_debug_routes_401_with_wrong_token() {
    for path in DEBUG_PATHS {
        let status = get_status(enabled_router(), path, Some("Bearer wrong-token")).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} must 401 with a wrong token when auth is enabled"
        );
    }
}

/// Auth enabled + CORRECT Bearer token → passes the gate (200 or 404
/// depending on filesystem state — never 401).
#[tokio::test]
async fn test_debug_routes_pass_auth_with_correct_token() {
    let header = format!("Bearer {TEST_TOKEN}");
    for path in DEBUG_PATHS {
        let status = get_status(enabled_router(), path, Some(&header)).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} must pass the auth gate with the correct token (got 401)"
        );
    }
}

/// Auth DISABLED → passthrough on every debug route (preserves local
/// dry-run + tickvault-logs MCP read-only flows — never 401).
#[tokio::test]
async fn test_debug_routes_pass_when_auth_disabled() {
    for path in DEBUG_PATHS {
        let status = get_status(disabled_router(), path, None).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} must pass through when auth is disabled (got 401)"
        );
    }
}

/// HEAD requests (axum auto-derives HEAD from `get()` handlers) must hit
/// the SAME `route_layer` gate — defense-in-depth pin so a method-variant
/// request can never skip the bearer wall (security review 2026-07-04).
#[tokio::test]
async fn test_debug_routes_401_on_head_without_token() {
    for path in DEBUG_PATHS {
        let request = Request::builder()
            .method(axum::http::Method::HEAD)
            .uri(path)
            .body(Body::empty())
            .expect("request build should succeed"); // APPROVED: test-only
        let status = enabled_router()
            .oneshot(request)
            .await
            .expect("router should respond") // APPROVED: test-only
            .status();
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "HEAD {path} must 401 without a token when auth is enabled"
        );
    }
}

/// Non-debug public route `/health` stays 200 with NO auth header even
/// while bearer auth is enabled — the gate covers ONLY the debug routes.
#[tokio::test]
async fn test_health_stays_public_without_auth() {
    let status = get_status(enabled_router(), "/health", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "/health must remain public (200 without auth) with auth enabled"
    );
}
