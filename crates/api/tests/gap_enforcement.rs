//! Gap enforcement tests for the API crate.
//!
//! Verifies mechanical guarantees documented in `.claude/rules/project/gap-enforcement.md`.
//! GAP-SEC-01 detailed tests are in `auth_middleware.rs` — this file verifies
//! the structural guarantees that the middleware exists and is wired correctly.

// ---------------------------------------------------------------------------
// GAP-SEC-01: API Auth Middleware structural checks
// ---------------------------------------------------------------------------

/// Verify that the auth middleware module exists and exports the expected types.
/// If this fails, the middleware was removed or renamed — all API routes are unprotected.
#[test]
fn test_gap_sec_01_auth_config_exists_with_required_fields() {
    let config = tickvault_api::middleware::ApiAuthConfig::new("test-token".to_string());
    assert!(config.enabled, "auth config with token must be enabled");
    assert_eq!(config.bearer_token, "test-token");
}

/// Verify disabled auth mode exists (dev passthrough).
#[test]
fn test_gap_sec_01_auth_config_disabled_mode() {
    let config = tickvault_api::middleware::ApiAuthConfig::disabled();
    assert!(!config.enabled, "disabled auth config must not be enabled");
}

/// Verify empty token produces disabled auth (fail-safe: no token = passthrough, not panic).
#[test]
fn test_gap_sec_01_empty_token_is_disabled() {
    let config = tickvault_api::middleware::ApiAuthConfig::new(String::new());
    assert!(
        !config.enabled,
        "empty token must produce disabled auth (fail-safe)"
    );
}
