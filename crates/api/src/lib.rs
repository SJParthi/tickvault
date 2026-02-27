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
