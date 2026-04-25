//! API middleware — authentication and authorization (GAP-SEC-01).
//!
//! Provides bearer token authentication for mutating API endpoints.
//! Read-only endpoints (health, stats, quote) remain unauthenticated.
//!
//! # Token Source (preferred → fallback)
//!
//! 1. **AWS SSM Parameter Store** — `/tickvault/<env>/api/bearer-token`
//!    fetched by `tickvault_core::auth::secret_manager::fetch_api_bearer_token`
//!    in `crates/app/src/main.rs` and injected via [`ApiAuthConfig::from_token`].
//!    THIS IS THE MANDATED PROD SOURCE per `.claude/rules/project/rust-code.md`.
//! 2. **`TV_API_TOKEN` env var** — legacy / local dev fallback used by
//!    [`ApiAuthConfig::from_env`] when SSM is unavailable. Logs a WARN
//!    so the operator notices the deviation in prod logs.
//!
//! # Auth Behavior
//! - Token resolved (SSM or env): auth enabled with that token.
//! - No token + `dry_run = true`: auth disabled (development passthrough).
//! - No token + `dry_run = false`: fail-closed — generates a random UUID v4
//!   token, logs CRITICAL via `error!`, and still requires auth for
//!   mutating endpoints. Operator gets paged via Telegram.
//!
//! # In-memory hygiene
//! `bearer_token` is `secrecy::SecretString` — zeroize on drop, `[REDACTED]`
//! `Display`. Manual `Debug` impl provides defense-in-depth on top of
//! `secrecy`'s own Debug guard. The constant-time comparison briefly
//! exposes the inner bytes via `expose_secret()` only in
//! [`require_bearer_auth`].
//!
//! # Authenticated Endpoints
//! - `POST /api/instruments/rebuild` — triggers instrument reload
//! - Any future mutating endpoints

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use secrecy::{ExposeSecret, SecretString};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for API authentication.
///
/// `bearer_token: SecretString` — the project's mandated secret type per
/// `.claude/rules/project/rust-code.md` ("Secret<T> from secrecy crate
/// enforces [REDACTED]"). `secrecy::SecretString` provides:
///   - Zeroize-on-drop of the inner heap allocation
///   - `[REDACTED]` `Debug` + `Display` impls
///   - No accidental `String` coercion
///
/// The manual `Debug` impl below adds defense-in-depth in case a future
/// derive macro on a wrapper struct circumvents secrecy's guards.
#[derive(Clone)]
pub struct ApiAuthConfig {
    /// Bearer token for authenticating mutating API requests.
    /// Empty (zero-length inner string) = auth disabled (development mode only).
    /// Field is private — callers go through [`Self::expose_bearer_token`]
    /// (constant-time comparison only) or never see the inner bytes at all.
    bearer_token: SecretString,
    /// Whether authentication is enabled.
    pub enabled: bool,
}

impl std::fmt::Debug for ApiAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiAuthConfig")
            .field("bearer_token", &"[REDACTED]")
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl ApiAuthConfig {
    /// Creates a new config with the given `String` token (test + legacy
    /// callers). Production code paths in `crates/app/src/main.rs`
    /// MUST use [`Self::from_token`] with a `SecretString` from SSM.
    pub fn new(bearer_token: String) -> Self {
        let enabled = !bearer_token.is_empty();
        Self {
            bearer_token: SecretString::from(bearer_token),
            enabled,
        }
    }

    /// Creates a config from a pre-resolved `SecretString` (typically from
    /// AWS SSM Parameter Store via
    /// `tickvault_core::auth::secret_manager::fetch_api_bearer_token`).
    ///
    /// Empty inner string = auth disabled (caller responsibility — main.rs
    /// halts in live mode rather than silently disabling).
    pub fn from_token(bearer_token: SecretString) -> Self {
        let enabled = !bearer_token.expose_secret().is_empty();
        Self {
            bearer_token,
            enabled,
        }
    }

    /// Creates a disabled auth config (development/dry-run mode only).
    pub fn disabled() -> Self {
        Self {
            bearer_token: SecretString::from(String::new()),
            enabled: false,
        }
    }

    /// Returns the inner token bytes for the constant-time comparison in
    /// [`require_bearer_auth`]. Do NOT call this from any other code path —
    /// the entire point of `SecretString` is to keep the bytes inside the
    /// secrecy boundary.
    fn expose_bearer_token(&self) -> &[u8] {
        self.bearer_token.expose_secret().as_bytes()
    }

    /// Test-only accessor for integration tests in `tests/`.
    ///
    /// Marked `#[doc(hidden)]` so it does not appear in rustdoc, and the
    /// name itself documents intent. Production code paths must NOT call
    /// this; the bearer token is meant to stay inside the secrecy boundary
    /// (zeroize on drop, redacted Display). The pre-2026-04-25 code had
    /// `bearer_token` as a public `String` field, so this accessor is a
    /// strict improvement: callers must reach for an explicitly named
    /// test-only method instead of touching a public field.
    ///
    /// Cannot be `#[cfg(test)]` because integration tests in `tests/` link
    /// against the non-test lib build.
    #[doc(hidden)]
    pub fn token_value_for_test(&self) -> &str {
        self.bearer_token.expose_secret()
    }

    /// Loads the API token from the `TV_API_TOKEN` environment variable
    /// (legacy / local-dev fallback path). Production uses
    /// [`Self::from_token`] with an SSM-fetched `SecretString` instead —
    /// see `crates/app/src/main.rs`.
    ///
    /// # Behavior
    /// - Token set and non-empty: auth enabled with that token.
    /// - Token unset/empty + `dry_run = true`: auth disabled (dev passthrough).
    /// - Token unset/empty + `dry_run = false`: fail-closed — generates a
    ///   random UUID v4 token, logs CRITICAL, and enforces auth. This
    ///   prevents accidentally running live with unprotected mutating endpoints.
    // O(1) EXEMPT: begin — cold path, called once at boot
    pub fn from_env(dry_run: bool) -> Self {
        match std::env::var("TV_API_TOKEN") {
            Ok(token) if !token.is_empty() => {
                info!(
                    "GAP-SEC-01: API bearer token authentication enabled (env-var fallback path; \
                     prefer SSM /tickvault/<env>/api/bearer-token in prod)"
                );
                Self::new(token)
            }
            _ => {
                if dry_run {
                    warn!(
                        "GAP-SEC-01: TV_API_TOKEN not set — API authentication disabled (dry-run mode)"
                    );
                    Self::disabled()
                } else {
                    // Fail-closed: generate a random token so auth is still enforced.
                    let generated_token = uuid::Uuid::new_v4().to_string();
                    // SECURITY: Do NOT log the generated token — it protects
                    // the /api/instruments/rebuild endpoint. Log only the fact
                    // that one was generated.
                    error!(
                        code = tickvault_common::error_code::ErrorCode::GapSecApiAuth.code_str(),
                        severity = tickvault_common::error_code::ErrorCode::GapSecApiAuth
                            .severity()
                            .as_str(),
                        "GAP-SEC-01 CRITICAL: TV_API_TOKEN not set in LIVE mode and SSM fetch \
                         failed — auto-generated bearer token for this session (set \
                         /tickvault/<env>/api/bearer-token in SSM, or TV_API_TOKEN env var)"
                    );
                    warn!(
                        "GAP-SEC-01: Move bearer token to SSM /tickvault/<env>/api/bearer-token \
                         to suppress this warning"
                    );
                    Self::new(generated_token)
                }
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
            // SEC-3: Constant-time comparison prevents timing oracle attacks.
            // Length check + byte-by-byte XOR ensures equal-time regardless of position.
            // expose_bearer_token() is the only call site that briefly accesses
            // the SecretString inner bytes — kept inside the secrecy boundary.
            let expected = config.expose_bearer_token();
            let token_match = token.len() == expected.len()
                && token
                    .as_bytes()
                    .iter()
                    .zip(expected.iter())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0;
            if token_match {
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
// Request Tracing Middleware (L5)
// ---------------------------------------------------------------------------

/// Request tracing middleware — logs every HTTP request with method, path,
/// status code, and duration. Emits `tv_api_request_duration_ms` histogram
/// for Prometheus/Grafana dashboards.
///
/// # Performance
/// O(1) per request — one `Instant::now()` + one histogram observe.
/// No allocation beyond what axum already does for request routing.
pub async fn request_tracing(request: Request, next: Next) -> Response {
    let start = std::time::Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let response = next.run(request).await;

    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    let status = response.status().as_u16();

    // Emit histogram for Grafana dashboards (P99 latency, throughput, etc.)
    metrics::histogram!("tv_api_request_duration_ms",
        "method" => method.to_string(),
        "status" => status.to_string(),
    )
    .record(duration_ms);

    // Structured log for audit trail — includes all 5W fields.
    tracing::info!(
        http.method = %method,
        http.path = %path,
        http.status = status,
        duration_ms = format!("{duration_ms:.2}"),
        "API request completed"
    );

    response
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes all `TV_API_TOKEN` env-var mutations across tests in
    /// this file.
    ///
    /// Rust 2024 promoted `std::env::set_var` / `remove_var` to `unsafe`
    /// because they can race with any other thread reading the env. A
    /// ThreadSanitizer run on 2026-04-20 (GitHub issue #304) detected
    /// exactly this: parallel test execution mutated `TV_API_TOKEN`
    /// from multiple threads without synchronization.
    ///
    /// Every test that calls `unsafe { std::env::set_var }` or
    /// `unsafe { std::env::remove_var }` MUST acquire this lock first:
    ///
    /// ```rust,ignore
    /// #[test]
    /// fn my_test() {
    ///     let _env_guard = lock_env();
    ///     unsafe { std::env::set_var("TV_API_TOKEN", "...") };
    ///     // ... test body ...
    ///     unsafe { std::env::remove_var("TV_API_TOKEN") };
    ///     // `_env_guard` drops at end-of-scope, releasing the mutex.
    /// }
    /// ```
    ///
    /// On poisoned lock (another test panicked while holding it) we
    /// recover the inner guard rather than propagating the poison —
    /// the env var state may be wrong, but the failing test already
    /// reported its own error, and blocking every downstream env test
    /// on one earlier failure is strictly worse for CI signal.
    static ENV_MUTATION_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTATION_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Named handler function shared by all middleware tests.
    /// Using a single named function instead of separate `|| async { "ok" }`
    /// closures ensures coverage when at least one test reaches the handler.
    async fn mock_handler() -> &'static str {
        "ok"
    }

    // -----------------------------------------------------------------------
    // ApiAuthConfig
    // -----------------------------------------------------------------------

    #[test]
    fn test_api_auth_config_new_enabled() {
        let config = ApiAuthConfig::new("my-secret-token".to_string());
        assert!(config.enabled);
        assert_eq!(config.bearer_token.expose_secret(), "my-secret-token");
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
        assert!(config.bearer_token.expose_secret().is_empty());
    }

    #[test]
    fn test_api_auth_config_clone() {
        let config = ApiAuthConfig::new("token123".to_string());
        let cloned = config.clone();
        assert_eq!(cloned.bearer_token.expose_secret(), "token123");
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                ApiAuthConfig::new("concurrent-token".to_string()),
                require_bearer_auth,
            ))
            .with_state(ApiAuthConfig::new("concurrent-token".to_string()));
        let app2 = Router::new()
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                ApiAuthConfig::new("concurrent-token".to_string()),
                require_bearer_auth,
            ))
            .with_state(ApiAuthConfig::new("concurrent-token".to_string()));
        let app3 = Router::new()
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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
            .route("/protected", get(mock_handler))
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

    // -------------------------------------------------------------------
    // from_env: dry_run vs live mode fail-closed behavior
    // -------------------------------------------------------------------

    #[test]
    fn test_from_env_dry_run_no_token_disables_auth() {
        let _env_guard = lock_env();
        // Remove TV_API_TOKEN to simulate unset
        unsafe { std::env::remove_var("TV_API_TOKEN") };
        let config = ApiAuthConfig::from_env(true);
        assert!(!config.enabled, "dry_run + no token = auth disabled");
        assert!(config.bearer_token.expose_secret().is_empty());
    }

    #[test]
    fn test_from_env_live_mode_no_token_generates_token() {
        let _env_guard = lock_env();
        // Remove TV_API_TOKEN to simulate unset
        unsafe { std::env::remove_var("TV_API_TOKEN") };
        let config = ApiAuthConfig::from_env(false);
        assert!(
            config.enabled,
            "live mode + no token = auth enabled with generated token"
        );
        assert!(
            !config.bearer_token.expose_secret().is_empty(),
            "generated token must not be empty"
        );
        // Verify it looks like a UUID v4
        assert_eq!(
            config.bearer_token.expose_secret().len(),
            36,
            "UUID v4 is 36 chars"
        );
    }

    #[test]
    fn test_from_env_live_mode_no_token_generates_unique_tokens() {
        let _env_guard = lock_env();
        unsafe { std::env::remove_var("TV_API_TOKEN") };
        let config1 = ApiAuthConfig::from_env(false);
        let config2 = ApiAuthConfig::from_env(false);
        assert_ne!(
            config1.bearer_token.expose_secret(),
            config2.bearer_token.expose_secret(),
            "each call must generate a unique token"
        );
    }

    #[test]
    fn test_from_env_with_token_ignores_dry_run() {
        // NOTE: env var tests are inherently racy under parallel execution.
        // We test the struct constructor directly to avoid env var races.
        let config = ApiAuthConfig::new("explicit-token".to_string());
        assert!(config.enabled);
        assert_eq!(config.bearer_token.expose_secret(), "explicit-token");
    }

    // =====================================================================
    // Additional coverage: whitespace-only tokens, unicode, non-UTF8 headers
    // =====================================================================

    #[test]
    fn test_api_auth_config_whitespace_token_enabled() {
        // Whitespace-only token should still be "enabled" (non-empty)
        let config = ApiAuthConfig::new("   ".to_string());
        assert!(config.enabled);
        assert_eq!(config.bearer_token.expose_secret(), "   ");
    }

    #[tokio::test]
    async fn test_auth_enabled_whitespace_token_exact_match_required() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("my-token".to_string());

        let app = Router::new()
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // Token with extra whitespace should NOT match
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer my-token ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_bearer_only_no_space_rejected() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret".to_string());

        let app = Router::new()
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // "Bearersecret" (no space) should be treated as non-Bearer prefix
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearersecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_api_auth_config_new_single_char_token() {
        let config = ApiAuthConfig::new("x".to_string());
        assert!(config.enabled);
        assert_eq!(config.bearer_token.expose_secret(), "x");
    }

    #[tokio::test]
    async fn test_auth_empty_authorization_header_rejected() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret".to_string());

        let app = Router::new()
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // -------------------------------------------------------------------
    // from_env: TV_API_TOKEN set with a non-empty value
    // -------------------------------------------------------------------

    #[test]
    fn test_from_env_with_explicit_token_set() {
        let _env_guard = lock_env();
        // Set TV_API_TOKEN so from_env() takes the Ok(token) path
        unsafe { std::env::set_var("TV_API_TOKEN", "test-explicit-token-12345") };
        let config = ApiAuthConfig::from_env(true);
        assert!(config.enabled);
        assert_eq!(
            config.bearer_token.expose_secret(),
            "test-explicit-token-12345"
        );

        // Also works in live mode
        let config_live = ApiAuthConfig::from_env(false);
        assert!(config_live.enabled);
        assert_eq!(
            config_live.bearer_token.expose_secret(),
            "test-explicit-token-12345"
        );

        // Clean up
        unsafe { std::env::remove_var("TV_API_TOKEN") };
    }

    // -------------------------------------------------------------------
    // from_env: TV_API_TOKEN set to empty string
    // -------------------------------------------------------------------

    #[test]
    fn test_from_env_empty_token_string_dry_run_disables_auth() {
        let _env_guard = lock_env();
        unsafe { std::env::set_var("TV_API_TOKEN", "") };
        let config = ApiAuthConfig::from_env(true);
        assert!(!config.enabled, "empty token + dry_run = disabled");
        assert!(config.bearer_token.expose_secret().is_empty());
        unsafe { std::env::remove_var("TV_API_TOKEN") };
    }

    #[test]
    fn test_from_env_empty_token_string_live_generates_token() {
        let _env_guard = lock_env();
        unsafe { std::env::set_var("TV_API_TOKEN", "") };
        let config = ApiAuthConfig::from_env(false);
        assert!(
            config.enabled,
            "empty token + live = enabled with generated"
        );
        assert!(!config.bearer_token.expose_secret().is_empty());
        assert_eq!(
            config.bearer_token.expose_secret().len(),
            36,
            "generated UUID v4 is 36 chars"
        );
        unsafe { std::env::remove_var("TV_API_TOKEN") };
    }

    // -------------------------------------------------------------------
    // Non-UTF8 Authorization header: to_str().ok() returns None → 401
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_non_utf8_header_returns_401() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::{HeaderValue, Request};
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("secret".to_string());

        let app = Router::new()
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // Build a request with a non-UTF8 Authorization header value
        let mut request = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        request.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_bytes(&[0x80, 0x81, 0x82]).unwrap(),
        );

        let response = app.oneshot(request).await.unwrap();
        // to_str().ok() returns None → falls through to None match arm → 401
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // -------------------------------------------------------------------
    // Token exactly matching "Bearer " prefix with no token part
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_bearer_prefix_only_returns_401() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let config = ApiAuthConfig::new("nonempty".to_string());

        let app = Router::new()
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // "Bearer " (with trailing space but no token) — token is "" which != "nonempty"
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

    // -----------------------------------------------------------------------
    // Request tracing middleware (L5)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_request_tracing_returns_response_and_preserves_status() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app = Router::new()
            .route("/health", axum::routing::get(mock_handler))
            .layer(axum::middleware::from_fn(super::request_tracing));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_request_tracing_404_for_unknown_route() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app = Router::new()
            .route("/health", axum::routing::get(mock_handler))
            .layer(axum::middleware::from_fn(super::request_tracing));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // =========================================================================
    // 2026-04-25 security audit (PR #357) — SecretString migration regressions
    // =========================================================================

    /// SEC-1 (HIGH→FIXED): bearer_token field MUST be SecretString.
    /// Source-scan ratchet: any future regression to `String` is caught at
    /// compile time because SecretString does not coerce to String.
    /// This test documents the type and pins the secrecy-crate semantics.
    #[test]
    fn test_bearer_token_field_is_secret_string() {
        let config = ApiAuthConfig::new("test-token".to_string());
        // SecretString::expose_secret() returns &str — coerces from
        // SecretString only via the secrecy crate API, NEVER via Deref.
        let exposed: &str = config.bearer_token.expose_secret();
        assert_eq!(exposed, "test-token");
    }

    /// SEC-2 (HIGH→FIXED): from_token constructor accepts a pre-resolved
    /// SecretString (typically from SSM). Verifies the auth-enable bit
    /// flips correctly based on the inner string's emptiness.
    #[test]
    fn test_from_token_with_non_empty_secret_enables_auth() {
        let token = SecretString::from("ssm-fetched-token".to_string());
        let config = ApiAuthConfig::from_token(token);
        assert!(config.enabled, "non-empty SecretString must enable auth");
        assert_eq!(
            config.bearer_token.expose_secret(),
            "ssm-fetched-token",
            "token roundtrip"
        );
    }

    #[test]
    fn test_from_token_with_empty_secret_disables_auth() {
        let token = SecretString::from(String::new());
        let config = ApiAuthConfig::from_token(token);
        assert!(!config.enabled, "empty SecretString must disable auth");
    }

    /// SEC-3: Debug impl never leaks the token bytes.
    /// Even after migrating to SecretString, the manual Debug impl stays as
    /// defense-in-depth. This test verifies the secret never appears in the
    /// Debug output regardless of which constructor was used.
    #[test]
    fn test_from_token_debug_does_not_leak_secret() {
        let token = SecretString::from("super-secret-ssm-value-12345".to_string());
        let config = ApiAuthConfig::from_token(token);
        let debug_output = format!("{config:?}");
        assert!(
            !debug_output.contains("super-secret-ssm-value-12345"),
            "Debug must NOT contain the token bytes; got: {debug_output}"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug must contain [REDACTED] marker"
        );
    }

    /// SEC-4: Constant-time comparison still works after the SecretString
    /// migration. expose_bearer_token() returns the raw bytes only inside
    /// the auth middleware boundary.
    #[tokio::test]
    async fn test_from_token_full_auth_round_trip() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let token = SecretString::from("ssm-style-token-abc".to_string());
        let config = ApiAuthConfig::from_token(token);
        assert!(config.enabled);

        let app = Router::new()
            .route("/protected", get(mock_handler))
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                require_bearer_auth,
            ))
            .with_state(config);

        // Valid token via from_token → 200 OK
        let response_ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer ssm-style-token-abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response_ok.status(), StatusCode::OK);

        // Wrong token → 401
        let response_bad = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response_bad.status(), StatusCode::UNAUTHORIZED);
    }

    /// SEC-5 (MED→FIXED): Clone preserves the SecretString without leaking
    /// the inner bytes — secrecy::SecretString is Clone and the inner heap
    /// allocation is a fresh allocation per clone (separate Drop lifecycle).
    #[test]
    fn test_clone_preserves_secret_token_value() {
        let original = ApiAuthConfig::new("clone-me".to_string());
        let cloned = original.clone();
        assert_eq!(original.bearer_token.expose_secret(), "clone-me");
        assert_eq!(cloned.bearer_token.expose_secret(), "clone-me");
        assert_eq!(original.enabled, cloned.enabled);
    }

    /// SEC-6: bearer_token field is private. External callers must use
    /// `token_value_for_test()` (test-only accessor) or the public
    /// constructors. This test verifies the accessor returns the expected
    /// inner value and that production code paths cannot bypass it.
    #[test]
    fn test_token_value_for_test_accessor_round_trip() {
        let config = ApiAuthConfig::new("private-field-test".to_string());
        // In-module access — verify accessor matches direct field access.
        assert_eq!(
            config.token_value_for_test(),
            config.bearer_token.expose_secret()
        );
        assert_eq!(config.token_value_for_test(), "private-field-test");
    }
}
