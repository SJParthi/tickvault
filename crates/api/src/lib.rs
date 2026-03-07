//! HTTP API server — axum endpoints for health, stats, portal, and instruments.
//!
//! # Endpoints
//! - `GET /health` — health check
//! - `GET /api/stats` — QuestDB table counts
//! - `GET /portal` — DLT Control Panel (links to all monitoring services)
//! - `POST /api/instruments/rebuild` — one-shot instrument rebuild
//! - `GET /api/instruments/diagnostic` — full instrument system health check
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
        // Terminal frontend
        .route("/", axum::routing::get(handlers::terminal::terminal_page))
        .route(
            "/static/terminal.js",
            axum::routing::get(handlers::terminal::terminal_js),
        )
        .route(
            "/static/terminal.css",
            axum::routing::get(handlers::terminal::terminal_css),
        )
        // WebSocket tick stream for browser
        .route(
            "/ws/ticks",
            axum::routing::get(handlers::terminal::websocket_ticks),
        )
        // Existing API routes
        .route(
            "/health",
            axum::routing::get(handlers::health::health_check),
        )
        .route("/api/stats", axum::routing::get(handlers::stats::get_stats))
        .route(
            "/api/instruments/rebuild",
            axum::routing::post(handlers::instruments::rebuild_instruments),
        )
        .route(
            "/api/instruments/diagnostic",
            axum::routing::get(handlers::instruments::instrument_diagnostic),
        )
        .route("/portal", axum::routing::get(handlers::static_file::portal))
        .layer(cors)
        .with_state(state)
}
