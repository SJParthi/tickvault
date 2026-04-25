//! GAP-SEC-01: Exhaustive tests for API bearer token authentication middleware.
//!
//! Tests cover:
//! - Config construction (all states)
//! - Middleware passthrough when disabled
//! - Valid token acceptance
//! - Every rejection scenario (missing, wrong, malformed, case-sensitive)
//! - Edge cases (extra spaces, empty bearer, no space after prefix)

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use tickvault_api::middleware::{ApiAuthConfig, require_bearer_auth};
use tower::ServiceExt;

fn auth_app(config: ApiAuthConfig) -> Router {
    Router::new()
        .route("/protected", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            config.clone(),
            require_bearer_auth,
        ))
        .with_state(config)
}

// -- Config construction -------------------------------------------------

#[test]
fn config_new_enabled() {
    let c = ApiAuthConfig::new("secret".to_owned());
    assert!(c.enabled);
    assert_eq!(c.token_value_for_test(), "secret");
}

#[test]
fn config_new_empty_disabled() {
    let c = ApiAuthConfig::new(String::new());
    assert!(!c.enabled);
}

#[test]
fn config_disabled() {
    let c = ApiAuthConfig::disabled();
    assert!(!c.enabled);
    assert!(c.token_value_for_test().is_empty());
}

#[test]
fn config_clone_preserves_all() {
    let c = ApiAuthConfig::new("tok".to_owned());
    let cl = c.clone();
    assert_eq!(cl.token_value_for_test(), "tok");
    assert!(cl.enabled);
}

#[test]
fn config_debug_format() {
    let c = ApiAuthConfig::new("s".to_owned());
    let dbg = format!("{c:?}");
    assert!(dbg.contains("ApiAuthConfig"));
}

// -- Middleware: disabled passthrough ------------------------------------

#[tokio::test]
async fn disabled_allows_no_header() {
    let app = auth_app(ApiAuthConfig::disabled());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn disabled_allows_garbage_header() {
    let app = auth_app(ApiAuthConfig::disabled());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "garbage")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::OK);
}

// -- Middleware: enabled + valid token -----------------------------------

#[tokio::test]
async fn valid_token_passes() {
    let app = auth_app(ApiAuthConfig::new("secret123".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "Bearer secret123")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn valid_token_with_special_chars() {
    let token = "xK9#mP2!qR7@vN5$";
    let app = auth_app(ApiAuthConfig::new(token.to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::OK);
}

// -- Middleware: enabled + invalid scenarios -----------------------------

#[tokio::test]
async fn missing_header_returns_401() {
    let app = auth_app(ApiAuthConfig::new("tok".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_returns_401() {
    let app = auth_app(ApiAuthConfig::new("correct".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "Bearer wrong")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn basic_auth_returns_401() {
    let app = auth_app(ApiAuthConfig::new("tok".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "Basic dXNlcjpwYXNz")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn empty_bearer_returns_401() {
    let app = auth_app(ApiAuthConfig::new("tok".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "Bearer ")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_lowercase_returns_401() {
    let app = auth_app(ApiAuthConfig::new("tok".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "bearer tok")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn no_space_after_bearer_returns_401() {
    let app = auth_app(ApiAuthConfig::new("tok".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "Bearertok")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn double_space_after_bearer_returns_401() {
    let app = auth_app(ApiAuthConfig::new("tok".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "Bearer  tok")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// Note: HTTP headers cannot contain newlines — hyper rejects them
// at the builder level before reaching middleware. No test needed.

#[tokio::test]
async fn empty_authorization_header_returns_401() {
    let app = auth_app(ApiAuthConfig::new("tok".to_owned()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header("Authorization", "")
                .body(Body::empty())
                .unwrap(), // APPROVED: test
        )
        .await
        .unwrap(); // APPROVED: test
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
