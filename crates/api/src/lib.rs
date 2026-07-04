//! HTTP API server — axum endpoints for health, stats, quote, and debug.
//!
//! # Endpoints
//! - `GET /` — redirect to `/dashboard`
//! - `GET /dashboard` — comprehensive operator dashboard (feeds + ticks + candles + DB + health)
//! - `GET /health` — health check
//! - `GET /feeds` — operator feed-control webpage (turn feeds on/off, single or multiple)
//! - `GET /api/stats` — QuestDB table counts
//! - `GET /api/quote/{security_id}` — latest tick for a security (from QuestDB)
//! - `GET /api/debug/logs/summary` — Claude MCP read-only log summary
//! - `GET /api/debug/logs/jsonl/latest` — Claude MCP read-only error JSONL
//! - `GET /api/debug/spill/status` — spill disk-health snapshot
//! - `GET /api/debug/cross-verify/latest` — latest 15:31 IST cross-verify CSV + summary
//!
//! # AWS-lifecycle PR #7d (2026-05-19) — frontend retired
//! Every `/portal/*` HTML route + the dead `/api/option-chain`,
//! `/api/pcr`, `/api/market/indices` endpoints were deleted. The
//! replacement surface is Grafana + Telegram + MCP tools + the
//! QuestDB Console at `localhost:9000`.
//!
//! # Boot Sequence Position
//! Pipeline → **API Server**

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(test, allow(clippy::assertions_on_constants))]
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]
#![allow(missing_docs)]
// APPROVED: clippy 1.95 tightened these doc-formatting lints; the codebase
// predates them. Allow rather than churn doc comments for a cosmetic
// markdown-rendering nicety with zero runtime/behavior impact.
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

pub mod feed_state;
pub mod feed_state_persist;
pub mod handlers;
pub mod middleware;
pub mod state;

use axum::Router;
use tower_http::cors::CorsLayer;

use middleware::{ApiAuthConfig, request_tracing, require_bearer_auth};
use state::SharedAppState;

/// Builds the full axum router with all routes and middleware.
///
/// GAP-SEC-01: Mutating endpoints (POST /api/instruments/rebuild) are protected
/// by bearer token auth. Read-only GET endpoints remain unauthenticated.
///
/// Auth source: legacy env-var path via [`ApiAuthConfig::from_env`].
/// Production callers in `crates/app/src/main.rs` should use
/// [`build_router_with_auth`] with a `SecretString` resolved from AWS SSM
/// (path `/tickvault/<env>/api/bearer-token`) so the bearer token never
/// transits a process env var.
///
/// # Arguments
/// * `state` — shared application state for handlers
/// * `allowed_origins` — list of allowed CORS origin URLs (from config)
/// * `dry_run` — whether the system is in dry-run mode (relaxed auth when token unset)
// O(1) EXEMPT: begin — cold path, called once at boot
pub fn build_router(state: SharedAppState, allowed_origins: &[String], dry_run: bool) -> Router {
    // GAP-SEC-01: Load auth config from TV_API_TOKEN env var (legacy fallback).
    // In dry_run mode: missing token = passthrough (dev mode).
    // In live mode: missing token = auto-generated token, auth still enforced.
    let auth_config = ApiAuthConfig::from_env(dry_run);
    // dry-run → tokenless feed-toggle (operator flips feeds on the page without a token).
    build_router_with_auth(state, allowed_origins, auth_config, dry_run)
}

/// Builds the router with a pre-resolved [`ApiAuthConfig`].
///
/// 2026-04-25 security audit (PR #357): production code path. The caller
/// (typically `crates/app/src/main.rs`) is responsible for resolving the
/// bearer token from AWS SSM Parameter Store
/// (`/tickvault/<env>/api/bearer-token` via
/// `tickvault_core::auth::secret_manager::fetch_api_bearer_token`) and
/// constructing `ApiAuthConfig::from_token(SecretString)`. This keeps the
/// bearer token out of process env vars / Docker `environment:` blocks.
///
/// # Arguments
/// * `state` — shared application state for handlers
/// * `allowed_origins` — list of allowed CORS origin URLs (from config)
/// * `auth_config` — pre-resolved auth config (SSM-fetched in prod, env-fallback in dev)
/// * `auth_config` — pre-resolved auth config (SSM-fetched in prod, env-fallback in dev)
/// * `feed_toggle_public` — when `true` (dry-run/sandbox, no real orders) the mutating
///   `POST /api/feeds/{feed}` is PUBLIC so the operator flips feeds tokenless on
///   localhost; when `false` (live trading) it stays bearer-protected. The
///   Dhan-disable safety gate (`can_disable_dhan`) is unchanged either way.
// O(1) EXEMPT: begin — cold path, called once at boot
pub fn build_router_with_auth(
    state: SharedAppState,
    allowed_origins: &[String],
    auth_config: ApiAuthConfig,
    feed_toggle_public: bool,
) -> Router {
    let cors = build_cors_layer(allowed_origins);

    // PR #6b (2026-05-19): /api/instruments/rebuild route RETIRED.
    // Under 4-IDX_I LOCKED_UNIVERSE there is no CSV to rebuild — the
    // universe is a compile-time constant. The endpoint and its handler
    // (handlers::instruments::rebuild_instruments) are deleted.
    // Feed-toggle API (operator AskUserQuestion 2026-06-19): authenticated
    // runtime per-feed enable/disable. `GET /api/feeds` reports state;
    // `POST /api/feeds/{feed}` flips it (Groww only — slice 1). Behind bearer
    // auth so only the operator can change the live feed topology.
    // The read-only status + health GETs carry no secrets (FeedHealthRow is
    // &'static str only) → always PUBLIC (below) so the /feeds page renders with
    // no token. The MUTATING flip `POST /api/feeds/{feed}`:
    //   • dry-run/sandbox (feed_toggle_public=true, no real orders) → PUBLIC, so
    //     the operator flips feeds tokenless on localhost (operator 2026-06-23);
    //   • live trading (false) → bearer-protected.
    // The Dhan-disable safety gate (can_disable_dhan) is unchanged either way.
    // `auth_config` is consumed by the layer below regardless of branch (so no
    // unused-variable lint when the toggle is public and the router is empty).
    const FEED_TOGGLE_PATH: &str = "/api/feeds/{feed}";
    let protected_base: Router<SharedAppState> = if feed_toggle_public {
        Router::new()
    } else {
        Router::new().route(
            FEED_TOGGLE_PATH,
            axum::routing::post(handlers::feeds::set_feed),
        )
    };
    let protected_routes: Router<SharedAppState> = protected_base.layer(
        axum::middleware::from_fn_with_state(auth_config.clone(), require_bearer_auth),
    );

    // Security trim 2026-07-04 (operator directive, Session A): the 4 read-only
    // `/api/debug/*` routes move OFF the public router and behind the SAME
    // bearer-auth middleware — applied UNCONDITIONALLY (deliberately NOT gated
    // on `feed_toggle_public`). `require_bearer_auth` self-passthroughs when
    // auth is disabled (dry-run / no token), so local development and the
    // tickvault-logs MCP read-only contract (local file reads + tokenless
    // localhost API calls with auth disabled) are unchanged. In production
    // (SSM-resolved token, auth enabled) these routes now demand
    // `Authorization: Bearer <token>` and 401 otherwise — fail-closed.
    let debug_routes: Router<SharedAppState> = Router::new()
        // Autonomous-ops Layer 1 (observability): read-only log access
        // for Claude MCP / remote sessions.
        .route(
            "/api/debug/logs/summary",
            axum::routing::get(handlers::debug::logs_summary),
        )
        .route(
            "/api/debug/logs/jsonl/latest",
            axum::routing::get(handlers::debug::logs_jsonl_latest),
        )
        .route(
            "/api/debug/spill/status",
            axum::routing::get(handlers::debug::spill_status),
        )
        // Visibility directive 2026-06-10: latest post-market 1-minute
        // cross-verify artefacts (CSV + summary) for the operator portal
        // Cross-verify card + MCP sessions.
        .route(
            "/api/debug/cross-verify/latest",
            axum::routing::get(handlers::debug::cross_verify_latest),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            auth_config,
            require_bearer_auth,
        ));

    // Public routes — read-only GET endpoints (no auth required).
    //
    // PR #7d (2026-05-19): the entire `/portal/*` HTML route family +
    // `/api/option-chain` + `/api/pcr` + `/api/market/indices` routes
    // have been retired. Their handlers (`static_file`, `option_chain`,
    // `market_data`) are deleted. Replacement surface: Grafana
    // dashboards, Telegram alerts, MCP tools, QuestDB Console.
    //
    // Earlier retirements:
    // - PR #2 (2026-05-18): movers route family.
    // - PR #6a (2026-05-19): 3 index-constituency routes + diagnostic route.
    // - PR #6b (2026-05-19): /api/instruments/rebuild route.
    let public_routes = Router::new()
        // Comprehensive operator dashboard (operator directive 2026-06-29: "the full
        // everything dashboard page"). A single self-contained HTML page showing
        // per-feed status + live tick counts + candles + DB row counts + overall
        // health. Pure VIEW — it client-side fetches the existing `/health`,
        // `/api/feeds`, `/api/feeds/health`, `/api/stats` endpoints (no new backend).
        // The HTML shell is public (no secrets); `GET /` lands the operator here.
        .route(
            "/",
            axum::routing::get(handlers::dashboard_page::root_redirect),
        )
        .route(
            "/dashboard",
            axum::routing::get(handlers::dashboard_page::dashboard_page),
        )
        .route(
            "/health",
            axum::routing::get(handlers::health::health_check),
        )
        // Operator feed-control webpage (operator directive 2026-06-21): a single
        // self-contained HTML page to turn feeds on/off (single or multiple). The
        // HTML shell is public (no secrets); every read/toggle it performs goes
        // through the bearer-auth `/api/feeds` endpoints below.
        .route(
            "/feeds",
            axum::routing::get(handlers::feeds_page::feeds_page),
        )
        // Public read-only feed status + per-feed health (operator 2026-06-23:
        // "public read, authed toggle"). No secrets — FeedHealthRow is
        // &'static str only — so the /feeds page renders these with no token.
        // The MUTATING POST /api/feeds/{feed} stays bearer-protected above.
        .route("/api/feeds", axum::routing::get(handlers::feeds::get_feeds))
        .route(
            "/api/feeds/health",
            axum::routing::get(handlers::feeds::get_feeds_health),
        )
        .route("/api/stats", axum::routing::get(handlers::stats::get_stats))
        .route(
            "/api/quote/{security_id}",
            axum::routing::get(handlers::quote::get_quote),
        );
    // The 4 `/api/debug/*` routes live on `debug_routes` above (bearer-auth
    // gated, security trim 2026-07-04) — no longer on the public router.

    // dry-run/sandbox: the mutating feed-toggle is PUBLIC (tokenless) so the
    // /feeds page can flip feeds without a token on localhost. In live trading
    // this branch is skipped and the toggle lives in `protected_routes` above.
    let public_routes = if feed_toggle_public {
        public_routes.route(
            FEED_TOGGLE_PATH,
            axum::routing::post(handlers::feeds::set_feed),
        )
    } else {
        public_routes
    };

    // PR #2 (2026-05-18): conditional `/api/movers/v2` route + the
    // `cascade_fanout` accessor on AppState retired. See above comment
    // on the merged routes block.
    public_routes
        .merge(debug_routes)
        .merge(protected_routes)
        .layer(axum::middleware::from_fn(request_tracing))
        .layer(cors)
        .with_state(state)
}
// O(1) EXEMPT: end

/// Builds a CORS layer from configured allowed origins.
///
/// If the list is empty, falls back to permissive localhost defaults for dev safety.
// O(1) EXEMPT: begin — cold path, called once at boot
fn build_cors_layer(allowed_origins: &[String]) -> CorsLayer {
    use axum::http::{HeaderValue, Method, header};

    let origins: Vec<&str> = if allowed_origins.is_empty() {
        vec!["http://localhost:3000", "http://localhost:3001"]
    } else {
        allowed_origins.iter().map(String::as_str).collect()
    };

    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| o.parse::<HeaderValue>().ok())
        .collect();

    // B3: Restrict methods to GET/POST/DELETE and headers to Authorization/Content-Type.
    // Prevents CSRF-style attacks from permitted origins using arbitrary methods/headers.
    let methods = [Method::GET, Method::POST, Method::DELETE];
    let headers = [header::AUTHORIZATION, header::CONTENT_TYPE];

    if parsed.is_empty() {
        // Fallback: if all origins failed to parse, allow localhost defaults.
        // HeaderValue::from_static is infallible for string literals.
        CorsLayer::new()
            .allow_origin([
                HeaderValue::from_static("http://localhost:3000"),
                HeaderValue::from_static("http://localhost:3001"),
            ])
            .allow_methods(methods)
            .allow_headers(headers)
    } else {
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_methods(methods)
            .allow_headers(headers)
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

    #[test]
    fn test_build_cors_layer_single_valid_origin() {
        let origins = vec!["https://my-dashboard.example.com".to_string()];
        let _cors = build_cors_layer(&origins);
    }

    // -------------------------------------------------------------------
    // build_router: smoke test — router builds without panic
    // -------------------------------------------------------------------

    /// 2026-04-25 (PR #357): direct smoke test for `build_router_with_auth`,
    /// the production code path that takes a pre-resolved `ApiAuthConfig`
    /// (typically constructed from an SSM-fetched `SecretString` in
    /// `crates/app/src/main.rs`). Verifies the router builds without panic
    /// when given a `from_token`-constructed config — this exercises the
    /// SSM-backed code path that the legacy `build_router` wrapper does not.
    #[test]
    fn test_build_router_with_auth_from_token_smoke() {
        use middleware::ApiAuthConfig;
        use secrecy::SecretString;

        let state = state::SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            std::sync::Arc::new(state::SystemHealthStatus::new()),
        );
        // Production-style call: SecretString → from_token → build_router_with_auth.
        let token = SecretString::from("ssm-fetched-token-test".to_string());
        let auth_config = ApiAuthConfig::from_token(token);
        assert!(auth_config.enabled, "from_token must enable auth");
        let _router = build_router_with_auth(state, &[], auth_config, false);
    }

    #[test]
    fn test_build_router_smoke_test_dry_run() {
        let state = state::SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            std::sync::Arc::new(state::SystemHealthStatus::new()),
        );
        // dry_run=true → TV_API_TOKEN not needed
        let _router = build_router(state, &[], true);
    }

    #[test]
    fn test_build_router_with_custom_origins() {
        let state = state::SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            std::sync::Arc::new(state::SystemHealthStatus::new()),
        );
        let origins = vec![
            "http://localhost:3000".to_string(),
            "https://dashboard.example.com".to_string(),
        ];
        let _router = build_router(state, &origins, true);
    }

    // -------------------------------------------------------------------
    // build_router: request routing — health endpoint responds 200
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_router_health_endpoint_returns_200() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let state = state::SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            std::sync::Arc::new(state::SystemHealthStatus::new()),
        );
        let router = build_router(state, &[], true);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    /// Builds a minimal `SharedAppState` for the auth tests. The Groww feed is
    /// seeded OFF so the feed-toggle endpoints have deterministic state.
    fn auth_test_state() -> state::SharedAppState {
        state::SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            std::sync::Arc::new(state::SystemHealthStatus::new()),
        )
    }

    #[tokio::test]
    async fn test_feeds_get_is_public_200_without_token() {
        use axum::body::Body;
        use axum::http::Request;
        use secrecy::SecretString;
        use tower::ServiceExt;

        // Operator AskUserQuestion 2026-06-23 ("public read, authed toggle"):
        // even with auth ENABLED, the read-only GET /api/feeds carries no secrets
        // and is PUBLIC so the /feeds page renders with no token. Only the
        // mutating POST /api/feeds/{feed} is bearer-gated (next test). Regression
        // ratchet: this must stay 200 without a token.
        let auth = ApiAuthConfig::from_token(SecretString::from("secret-tok".to_string()));
        let router = build_router_with_auth(auth_test_state(), &[], auth, false);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/feeds")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn test_feeds_health_is_public_200_without_token() {
        use axum::body::Body;
        use axum::http::Request;
        use secrecy::SecretString;
        use tower::ServiceExt;

        // Same contract: the read-only per-feed health view is public (no secrets;
        // FeedHealthRow is &'static str only) so the operator can watch live-feed
        // health without a token.
        let auth = ApiAuthConfig::from_token(SecretString::from("secret-tok".to_string()));
        let router = build_router_with_auth(auth_test_state(), &[], auth, false);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/feeds/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn test_feeds_post_requires_auth_401_without_token() {
        use axum::body::Body;
        use axum::http::Request;
        use secrecy::SecretString;
        use tower::ServiceExt;

        let auth = ApiAuthConfig::from_token(SecretString::from("secret-tok".to_string()));
        let router = build_router_with_auth(auth_test_state(), &[], auth, false);

        let response = router
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/api/feeds/groww")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_feeds_post_public_200_without_token_in_dry_run() {
        use axum::body::Body;
        use axum::http::Request;
        use secrecy::SecretString;
        use tower::ServiceExt;

        // operator AskUserQuestion 2026-06-23 ("tokenless toggle in dev"): with
        // feed_toggle_public=true (dry-run/sandbox, no real orders) the mutating
        // POST /api/feeds/{feed} is PUBLIC so the operator flips feeds on the page
        // with no token. Auth is still ENABLED (token present) — proving the route
        // is public, not that auth is off. Regression ratchet: must stay 200.
        let auth = ApiAuthConfig::from_token(SecretString::from("secret-tok".to_string()));
        let router = build_router_with_auth(auth_test_state(), &[], auth, true);

        let response = router
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/api/feeds/groww")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn test_feeds_page_is_public_200_without_auth() {
        use axum::body::Body;
        use axum::http::Request;
        use secrecy::SecretString;
        use tower::ServiceExt;

        // Even with auth ENABLED, the operator feed-control PAGE (HTML shell) is a
        // public route — only the data/toggle `/api/feeds` calls it issues are
        // bearer-gated. The page must load so the operator can paste their token.
        let auth = ApiAuthConfig::from_token(SecretString::from("secret-tok".to_string()));
        let router = build_router_with_auth(auth_test_state(), &[], auth, false);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/feeds")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        // Anti-clickjacking: the page must ship X-Frame-Options (security-review).
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::X_FRAME_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("SAMEORIGIN"),
        );
    }

    #[tokio::test]
    async fn test_build_router_dashboard_endpoint_returns_200_html() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        // Comprehensive operator dashboard (operator 2026-06-29). Public route —
        // the HTML shell holds no secrets; it must serve 200 + text/html with the
        // anti-clickjacking header, even with auth disabled (dry-run).
        let router = build_router(auth_test_state(), &[], true);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("text/html")),
            "dashboard must serve text/html",
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::X_FRAME_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("SAMEORIGIN"),
        );
    }

    #[tokio::test]
    async fn test_build_router_root_redirects_to_dashboard() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        // `GET /` lands the operator on the everything view by redirecting.
        let router = build_router(auth_test_state(), &[], true);
        let response = router
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            response.status().is_redirection(),
            "root must redirect (3xx), got {}",
            response.status()
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/dashboard"),
        );
    }

    // PR #7d (2026-05-19): `test_build_router_portal_endpoint_returns_200`
    // retired. The `/portal` route + every `/portal/*` HTML route was
    // deleted alongside the static_file handler. The 404 case is still
    // exercised by `test_build_router_unknown_route_returns_404` below.

    // -------------------------------------------------------------------
    // build_router: unknown route returns 404
    // -------------------------------------------------------------------

    #[test]
    fn test_build_router_live_mode_with_origins() {
        let state = state::SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            std::sync::Arc::new(state::SystemHealthStatus::new()),
        );
        let origins = vec!["http://localhost:3000".to_string()];
        // dry_run=false exercises the live-mode auth path (auto-generates token)
        let _router = build_router(state, &origins, false);
    }

    #[tokio::test]
    async fn test_build_router_unknown_route_returns_404() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let state = state::SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
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
            std::sync::Arc::new(state::SystemHealthStatus::new()),
        );
        let router = build_router(state, &[], true);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
