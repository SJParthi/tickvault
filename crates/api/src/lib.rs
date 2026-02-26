//! HTTP API server — axum endpoints for health, candle data, and WebSocket streaming.
//!
//! # Endpoints
//! - `GET /health` — health check
//! - `GET /api/candles/:security_id` — historical candles from QuestDB
//! - `GET /api/intervals` — available chart intervals
//! - `WS /ws/live` — live candle streaming via WebSocket
//! - `GET /` — static HTML frontend (TradingView chart)
//!
//! # Boot Sequence Position
//! Pipeline → **API Server** → Frontend

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
        .route(
            "/api/candles/{security_id}",
            axum::routing::get(handlers::candles::get_candles),
        )
        .route(
            "/api/intervals",
            axum::routing::get(handlers::candles::get_intervals),
        )
        .route(
            "/ws/live",
            axum::routing::get(handlers::websocket::ws_handler),
        )
        .route("/", axum::routing::get(handlers::static_file::index_html))
        .layer(cors)
        .with_state(state)
}
