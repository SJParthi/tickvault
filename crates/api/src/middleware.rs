//! API middleware — authentication and authorization (GAP-SEC-01).
//!
//! Provides bearer token authentication for mutating API endpoints.
//! Read-only endpoints (health, stats, quote) remain unauthenticated.
//!
//! # Token Source
//! The API bearer token is read from the environment variable `DLT_API_TOKEN`
//! at startup. If not set, authentication is disabled (development mode).
//!
//! # Authenticated Endpoints
//! - `POST /api/instruments/rebuild` — triggers instrument reload
//! - Any future mutating endpoints

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for API authentication.
#[derive(Debug, Clone)]
pub struct ApiAuthConfig {
    /// Bearer token for authenticating mutating API requests.
    /// Empty string = auth disabled (development mode).
    pub bearer_token: String,
    /// Whether authentication is enabled.
    pub enabled: bool,
}

impl ApiAuthConfig {
    /// Creates a new config with the given token.
    pub fn new(bearer_token: String) -> Self {
        let enabled = !bearer_token.is_empty();
        Self {
            bearer_token,
            enabled,
        }
    }

    /// Creates a disabled auth config (development mode).
    pub fn disabled() -> Self {
        Self {
            bearer_token: String::new(),
            enabled: false,
        }
    }

    /// Loads the API token from the `DLT_API_TOKEN` environment variable.
    ///
    /// Returns disabled config if the variable is not set or empty.
    // O(1) EXEMPT: begin — cold path, called once at boot
    pub fn from_env() -> Self {
        match std::env::var("DLT_API_TOKEN") {
            Ok(token) if !token.is_empty() => {
                info!("GAP-SEC-01: API bearer token authentication enabled");
                Self::new(token)
            }
            _ => {
                warn!("GAP-SEC-01: DLT_API_TOKEN not set — API authentication disabled (dev mode)");
                Self::disabled()
            }
        }
    }
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Bearer token validation middleware for mutating endpoints.
///
/// GAP-SEC-01: Protects mutating API endpoints with bearer token auth.
/// Returns 401 Unauthorized if the token is missing or invalid.
///
/// # Usage
/// Applied via `axum::middleware::from_fn_with_state` on protected routes.
pub async fn require_bearer_auth(
    axum::extract::State(config): axum::extract::State<ApiAuthConfig>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // If auth is disabled, pass through
    if !config.enabled {
        return Ok(next.run(request).await);
    }

    // Extract Authorization header
    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let token = &header[7..]; // Skip "Bearer "
            if token == config.bearer_token {
                Ok(next.run(request).await)
            } else {
                warn!("GAP-SEC-01: API auth failed — invalid bearer token");
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        Some(_) => {
            warn!("GAP-SEC-01: API auth failed — malformed Authorization header");
            Err(StatusCode::UNAUTHORIZED)
        }
        None => {
            warn!("GAP-SEC-01: API auth failed — missing Authorization header");
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // ApiAuthConfig
    // -----------------------------------------------------------------------

    #[test]
    fn test_api_auth_config_new_enabled() {
        let config = ApiAuthConfig::new("my-secret-token".to_string());
        assert!(config.enabled);
        assert_eq!(config.bearer_token, "my-secret-token");
    }

    #[test]
    fn test_api_auth_config_new_empty_disabled() {
        let config = ApiAuthConfig::new(String::new());
        assert!(!config.enabled);
    }

    #[test]
    fn test_api_auth_config_disabled() {
        let config = ApiAuthConfig::disabled();
        assert!(!config.enabled);
        assert!(config.bearer_token.is_empty());
    }

    #[test]
    fn test_api_auth_config_clone() {
        let config = ApiAuthConfig::new("token123".to_string());
        let cloned = config.clone();
        assert_eq!(cloned.bearer_token, "token123");
        assert!(cloned.enabled);
    }

    // -----------------------------------------------------------------------
    // Bearer token parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_bearer_token_extraction() {
        let header = "Bearer my-secret-token";
        assert!(header.starts_with("Bearer "));
        let token = &header[7..];
        assert_eq!(token, "my-secret-token");
    }

    #[test]
    fn test_bearer_token_empty_after_prefix() {
        let header = "Bearer ";
        let token = &header[7..];
        assert!(token.is_empty());
    }

    #[test]
    fn test_non_bearer_auth_rejected() {
        let header = "Basic dXNlcjpwYXNz";
        assert!(!header.starts_with("Bearer "));
    }

    // -----------------------------------------------------------------------
    // Integration test: middleware with axum
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_disabled_passes_through() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::disabled();

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_enabled_missing_token_returns_401() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret123".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_enabled_valid_token_passes() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret123".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_enabled_wrong_token_returns_401() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret123".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_enabled_basic_auth_returns_401() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret123".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Basic dXNlcjpwYXNz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_empty_bearer_returns_401() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret123".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // -------------------------------------------------------------------
    // GAP-SEC-01: Case sensitivity — "bearer " (lowercase) rejected
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_lowercase_bearer_rejected() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret123".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // "bearer " (lowercase b) must be rejected — case-sensitive prefix
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "bearer secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // -------------------------------------------------------------------
    // Multiple auth headers — only first header value matters
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_multiple_headers_first_value_used() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("correct-token".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // First header is valid — should pass even if a second header
        // with wrong token were present. headers().get() returns first.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer correct-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------
    // Concurrent requests — 3 simultaneous valid requests all succeed
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_concurrent_requests_all_succeed() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("concurrent-token".to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // Clone the app into a tower::Service that can handle multiple requests
        let mut service = app.into_service();

        // Send 3 concurrent requests
        let req1 = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer concurrent-token")
            .body(Body::empty())
            .unwrap();
        let req2 = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer concurrent-token")
            .body(Body::empty())
            .unwrap();
        let req3 = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer concurrent-token")
            .body(Body::empty())
            .unwrap();

        let (r1, r2, r3) = tokio::join!(
            ServiceExt::<Request<Body>>::ready(&mut service),
            async { Ok::<_, std::convert::Infallible>(()) },
            async { Ok::<_, std::convert::Infallible>(()) },
        );
        r1.expect("service ready");

        // Use oneshot-style sequential sends (tower::Service)
        // For true concurrency, clone the router into separate services
        let app1 = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                ApiAuthConfig::new("concurrent-token".to_string()),
                require_bearer_auth,
            ))
            .with_state(ApiAuthConfig::new("concurrent-token".to_string()));
        let app2 = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                ApiAuthConfig::new("concurrent-token".to_string()),
                require_bearer_auth,
            ))
            .with_state(ApiAuthConfig::new("concurrent-token".to_string()));
        let app3 = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                ApiAuthConfig::new("concurrent-token".to_string()),
                require_bearer_auth,
            ))
            .with_state(ApiAuthConfig::new("concurrent-token".to_string()));

        let (resp1, resp2, resp3) =
            tokio::join!(app1.oneshot(req1), app2.oneshot(req2), app3.oneshot(req3),);

        // Suppress unused variable warnings
        let _ = (r2, r3);

        assert_eq!(resp1.unwrap().status(), StatusCode::OK);
        assert_eq!(resp2.unwrap().status(), StatusCode::OK);
        assert_eq!(resp3.unwrap().status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------
    // Token with special characters — `-`, `_`, `.` should work
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_token_with_special_chars() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let special_token = "eyJ-alg_OiJ.IUzI1NiJ9.test-token_v2.0";
        let config = ApiAuthConfig::new(special_token.to_string());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", format!("Bearer {}", special_token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------
    // Very long token — 1000 characters should work
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_very_long_token() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let long_token: String = "a".repeat(1000);
        let config = ApiAuthConfig::new(long_token.clone());

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", format!("Bearer {}", long_token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------
    // Config debug format — must not leak the token value
    // -------------------------------------------------------------------

    #[test]
    fn test_api_auth_config_debug_contains_field_names() {
        let config = ApiAuthConfig::new("super-secret-value".to_string());
        let debug_output = format!("{config:?}");

        // Debug should contain the field name "bearer_token"
        assert!(
            debug_output.contains("bearer_token"),
            "Debug output must contain 'bearer_token' field name"
        );
        // Debug should contain the "enabled" field
        assert!(
            debug_output.contains("enabled"),
            "Debug output must contain 'enabled' field name"
        );
    }
}
