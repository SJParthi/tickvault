//! Ratchet tests: `TV_API_TOKEN` missing in LIVE mode must fire CRITICAL +
//! fail-closed auto-generate a random bearer (queue item I6).
//!
//! # Why this exists (2026-04-21)
//!
//! Live production logs at 13:06 IST today showed:
//! ```
//! "message":"GAP-SEC-01: TV_API_TOKEN not set — API authentication disabled (dry-run mode)"
//! ```
//! This is the WARN path in `ApiAuthConfig::from_env` for `dry_run = true`.
//! The PROD path (`dry_run = false`) MUST fire a distinct ERROR + Telegram
//! alert AND fail-closed by generating a random UUID bearer so that no
//! mutating endpoint can be called without an explicit `TV_API_TOKEN`.
//!
//! These ratchet tests scan the source of `crates/api/src/middleware.rs`
//! and exercise `ApiAuthConfig::from_env` directly. They fail the build
//! if the prod-vs-dev branch disappears, the CRITICAL log string changes,
//! or the fail-closed random-token fallback is weakened.

use tickvault_api::middleware::ApiAuthConfig;

// ---------------------------------------------------------------------------
// Behavioural ratchets — exercise `from_env` directly
// ---------------------------------------------------------------------------

/// Dry-run mode without `TV_API_TOKEN` must produce a DISABLED auth config.
/// Matches the live 2026-04-21 13:06 IST log line.
#[test]
fn from_env_dry_run_unset_token_disables_auth() {
    // SAFETY: test-only env var manipulation; `.env` files are banned in prod.
    // Clear the env and run in dry-run mode.
    // O(1) EXEMPT: test setup
    // SAFETY: unsafe block required by Rust 2024 edition for std::env::set_var.
    unsafe {
        std::env::remove_var("TV_API_TOKEN");
    }
    let cfg = ApiAuthConfig::from_env(true);
    assert!(
        !cfg.enabled,
        "dry-run + no TV_API_TOKEN must disable auth (development passthrough)"
    );
}

/// Live mode without `TV_API_TOKEN` must FAIL-CLOSED: a random bearer is
/// generated and auth is ENABLED. Without this ratchet, accidentally
/// shipping an unauthenticated prod config would expose mutating
/// endpoints to any caller.
#[test]
fn from_env_live_unset_token_fails_closed() {
    // SAFETY: unsafe block required by Rust 2024 edition for std::env::set_var.
    unsafe {
        std::env::remove_var("TV_API_TOKEN");
    }
    let cfg = ApiAuthConfig::from_env(false);
    assert!(
        cfg.enabled,
        "live + no TV_API_TOKEN MUST enable auth with a fail-closed random \
         bearer token — otherwise mutating endpoints are wide-open."
    );
}

/// Token explicitly set — trivial happy-path coverage so the ratchet
/// also exercises the "enabled with operator-provided token" branch.
#[test]
fn from_env_token_set_enables_auth() {
    // SAFETY: unsafe block required by Rust 2024 edition for std::env::set_var.
    unsafe {
        std::env::set_var("TV_API_TOKEN", "test-bearer-token-I6-ratchet");
    }
    let cfg = ApiAuthConfig::from_env(false);
    assert!(cfg.enabled, "explicit TV_API_TOKEN must enable auth");
    // SAFETY: unsafe block required by Rust 2024 edition for std::env::set_var.
    unsafe {
        std::env::remove_var("TV_API_TOKEN");
    }
}

// ---------------------------------------------------------------------------
// Source ratchets — pin the CRITICAL log path so it can't regress to WARN
// ---------------------------------------------------------------------------

use std::fs;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn middleware_source_fires_critical_error_in_live_mode() {
    let src = read("crates/api/src/middleware.rs");
    assert!(
        src.contains("GAP-SEC-01 CRITICAL: TV_API_TOKEN not set in LIVE mode"),
        "middleware.rs MUST fire an ERROR-level log with the 'CRITICAL' \
         phrase in live mode — this is what routes the event to Telegram \
         via Loki's ERROR pipeline. Regression = silent prod deployment \
         with unprotected mutating endpoints."
    );
    assert!(
        src.contains("ErrorCode::GapSecApiAuth"),
        "middleware.rs CRITICAL log MUST carry the GapSecApiAuth error code \
         so Loki routes it correctly and the error-code taxonomy stays \
         complete."
    );
}

#[test]
fn middleware_source_generates_random_bearer_when_live_and_unset() {
    let src = read("crates/api/src/middleware.rs");
    assert!(
        src.contains("uuid::Uuid::new_v4()"),
        "middleware.rs live + unset-token path MUST auto-generate a random \
         UUID v4 as bearer so the API fails CLOSED (no caller has the \
         generated token until operator fixes TV_API_TOKEN)."
    );
}

#[test]
fn middleware_source_distinguishes_dry_run_from_live() {
    let src = read("crates/api/src/middleware.rs");
    assert!(
        src.contains("if dry_run"),
        "middleware.rs must branch on `dry_run` to distinguish dev passthrough \
         from live fail-closed behaviour."
    );
}
