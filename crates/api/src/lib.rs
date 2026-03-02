// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

//! HTTP API server — axum endpoints for health, stats, and portal.
//!
//! # Endpoints
//! - `GET /health` — health check
//! - `GET /api/stats` — QuestDB table counts
//! - `GET /portal` — DLT Control Panel (links to all monitoring services)
//!
//! # Boot Sequence Position
//! Pipeline → **API Server**

pub mod handlers;
pub mod state;

use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use state::SharedAppState;

/// Builds the full axum router with all routes and middleware.
pub fn build_router(state: SharedAppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route(
            "/health",
            axum::routing::get(handlers::health::health_check),
        )
        .route("/api/stats", axum::routing::get(handlers::stats::get_stats))
        .route("/portal", axum::routing::get(handlers::static_file::portal))
        .layer(cors)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use dhan_live_trader_common::config::QuestDbConfig;
    use tower::ServiceExt;

    fn make_test_state() -> SharedAppState {
        SharedAppState::new(QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        })
    }

    #[tokio::test]
    async fn test_build_router_health_endpoint() {
        let router = build_router(make_test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_build_router_portal_endpoint() {
        let router = build_router(make_test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/portal")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_build_router_stats_endpoint_returns_response() {
        let router = build_router(make_test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Stats endpoint will try to connect to QuestDB and fail, but should still return JSON
        assert_eq!(response.status(), StatusCode::OK);
    }
}
