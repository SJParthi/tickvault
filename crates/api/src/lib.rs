//! HTTP API server — axum endpoints for health, stats, portal, and instruments.
//!
//! # Endpoints
//! - `GET /health` — health check
//! - `GET /api/stats` — QuestDB table counts
//! - `GET /api/quote/{security_id}` — latest tick for a security (from QuestDB)
//! - `GET /api/top-movers` — top gainers, losers, most active
//! - `GET /portal` — DLT Control Panel (links to all monitoring services)
//! - `POST /api/instruments/rebuild` — one-shot instrument rebuild
//! - `GET /api/instruments/diagnostic` — full instrument system health check
//!
//! # Boot Sequence Position
//! Pipeline → **API Server**

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
#![allow(missing_docs)]

pub mod handlers;
pub mod middleware;
pub mod state;

use axum::Router;
use tower_http::cors::CorsLayer;

use middleware::{ApiAuthConfig, require_bearer_auth};
use state::SharedAppState;

/// Builds the full axum router with all routes and middleware.
///
/// GAP-SEC-01: Mutating endpoints (POST /api/instruments/rebuild) are protected
/// by bearer token auth. Read-only GET endpoints remain unauthenticated.
///
/// # Arguments
/// * `state` — shared application state for handlers
/// * `allowed_origins` — list of allowed CORS origin URLs (from config)
/// * `dry_run` — whether the system is in dry-run mode (relaxed auth when token unset)
// O(1) EXEMPT: begin — cold path, called once at boot
pub fn build_router(state: SharedAppState, allowed_origins: &[String], dry_run: bool) -> Router {
    let cors = build_cors_layer(allowed_origins);

    // GAP-SEC-01: Load auth config from DLT_API_TOKEN env var.
    // In dry_run mode: missing token = passthrough (dev mode).
    // In live mode: missing token = auto-generated token, auth still enforced.
    let auth_config = ApiAuthConfig::from_env(dry_run);

    // Protected routes — mutating endpoints behind bearer token auth
    let protected_routes = Router::new()
        .route(
            "/api/instruments/rebuild",
            axum::routing::post(handlers::instruments::rebuild_instruments),
        )
        .layer(axum::middleware::from_fn_with_state(
            auth_config,
            require_bearer_auth,
        ));

    // Public routes — read-only GET endpoints (no auth required)
    let public_routes = Router::new()
        .route(
            "/health",
            axum::routing::get(handlers::health::health_check),
        )
        .route("/api/stats", axum::routing::get(handlers::stats::get_stats))
        .route(
            "/api/quote/{security_id}",
            axum::routing::get(handlers::quote::get_quote),
        )
        .route(
            "/api/top-movers",
            axum::routing::get(handlers::top_movers::get_top_movers),
        )
        .route(
            "/api/instruments/diagnostic",
            axum::routing::get(handlers::instruments::instrument_diagnostic),
        )
        .route("/portal", axum::routing::get(handlers::static_file::portal))
        .route(
            "/api/index-constituency",
            axum::routing::get(handlers::index_constituency::get_constituency_summary),
        )
        .route(
            "/api/index-constituency/{index_name}",
            axum::routing::get(handlers::index_constituency::get_index_constituents),
        )
        .route(
            "/api/stock-indices/{symbol}",
            axum::routing::get(handlers::index_constituency::get_stock_indices),
        );

    public_routes
        .merge(protected_routes)
        .layer(cors)
        .with_state(state)
}
// O(1) EXEMPT: end

/// Builds a CORS layer from configured allowed origins.
///
/// If the list is empty, falls back to permissive localhost defaults for dev safety.
// O(1) EXEMPT: begin — cold path, called once at boot
fn build_cors_layer(allowed_origins: &[String]) -> CorsLayer {
    use axum::http::HeaderValue;
    use tower_http::cors::Any;

    let origins: Vec<&str> = if allowed_origins.is_empty() {
        vec!["http://localhost:3000", "http://localhost:3001"]
    } else {
        allowed_origins.iter().map(String::as_str).collect()
    };

    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| o.parse::<HeaderValue>().ok())
        .collect();

    if parsed.is_empty() {
        // Fallback: if all origins failed to parse, allow localhost defaults.
        // HeaderValue::from_static is infallible for string literals.
        CorsLayer::new()
            .allow_origin([
                HeaderValue::from_static("http://localhost:3000"),
                HeaderValue::from_static("http://localhost:3001"),
            ])
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_methods(Any)
            .allow_headers(Any)
    }
}
// O(1) EXEMPT: end

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_cors_layer_empty_origins_uses_defaults() {
        // Should not panic — falls back to localhost defaults
        let _cors = build_cors_layer(&[]);
    }

    #[test]
    fn test_build_cors_layer_valid_origins() {
        let origins = vec![
            "http://localhost:3000".to_string(),
            "http://example.com".to_string(),
        ];
        let _cors = build_cors_layer(&origins);
    }

    #[test]
    fn test_build_cors_layer_all_invalid_origins_falls_back() {
        // Invalid origins that can't parse to HeaderValue — should fall back to defaults
        let origins = vec!["\x00invalid".to_string(), "\x01bad".to_string()];
        let _cors = build_cors_layer(&origins);
    }

    #[test]
    fn test_build_cors_layer_mixed_valid_invalid_origins() {
        let origins = vec![
            "http://localhost:3000".to_string(),
            "\x00invalid".to_string(),
        ];
        let _cors = build_cors_layer(&origins);
    }
}
