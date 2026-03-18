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

pub mod handlers;
pub mod middleware;
pub mod state;

use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use middleware::{ApiAuthConfig, require_bearer_auth};
use state::SharedAppState;

/// Builds the full axum router with all routes and middleware.
///
/// GAP-SEC-01: Mutating endpoints (POST /api/instruments/rebuild) are protected
/// by bearer token auth. Read-only GET endpoints remain unauthenticated.
pub fn build_router(state: SharedAppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // GAP-SEC-01: Load auth config from DLT_API_TOKEN env var.
    // Empty/unset = passthrough (dev mode).
    let auth_config = ApiAuthConfig::from_env();

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
