//! Shared OMS wiring — the `TokenHandle` → `TokenProvider` adapter and the
//! pinned-timeout HTTP client builder used by EVERY OMS construction site
//! (the dhan-on `trading_pipeline` and the dhan-off `order_runtime`).
//!
//! Extracted from `trading_pipeline.rs` (order-runtime dry-run PR,
//! 2026-07-14) so the two sites can never drift on the `OMS_HTTP_*`
//! constant usage (`test_oms_http_timeout_is_pinned_at_5s` pins the
//! constants themselves in `crates/common/src/constants.rs`).

use secrecy::{ExposeSecret, SecretString};

use tickvault_common::error_code::ErrorCode;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_storage::http_client::{HttpClientBuildError, client_from_build_result};
use tickvault_trading::oms::{OmsError, TokenProvider};

/// Bridges the core crate's `TokenHandle` (arc-swap) to the trading crate's
/// `TokenProvider` trait. Cold path — called once per order placement.
pub struct TokenHandleBridge {
    /// The live arc-swap token handle (core crate).
    pub handle: TokenHandle,
}

impl TokenProvider for TokenHandleBridge {
    fn get_access_token(&self) -> Result<SecretString, OmsError> {
        let guard = self.handle.load();
        match guard.as_ref() {
            Some(token_state) => {
                // Reject expired tokens — prevents sending orders with stale
                // JWT that would return DH-901 and enter a retry loop.
                if !token_state.is_valid() {
                    return Err(OmsError::TokenExpired);
                }
                let token_str = token_state.access_token().expose_secret();
                if token_str.is_empty() {
                    return Err(OmsError::NoToken);
                }
                Ok(SecretString::from(token_str.to_owned()))
            }
            None => Err(OmsError::NoToken),
        }
    }
}

/// Builds the OMS REST client with the pinned `OMS_HTTP_*` timeouts
/// (Phase 0 Item 22a — 5s request / connect, pooled idle).
///
/// HTTP-CLIENT-01 discipline (post-#1562 audit fix, 2026-07-17): the old
/// `.unwrap_or_default()` tail invoked `reqwest::Client::default()` ==
/// `Client::new()`, which PANICS under exactly the fd/TLS/resolver
/// pressure that makes the builder's `Err` arm reachable — with the
/// workspace release profile's `panic = "abort"`, a whole-process abort
/// at every order-runtime (re)spawn, for a dry-run client that never
/// issues HTTP. A builder failure now degrades through the storage
/// crate's typed [`HttpClientBuildError`] core (userinfo-redacted): this
/// fn logs ONE coded `error!` + increments
/// `tv_http_client_build_failed_total{site="oms_wiring"}` (static label)
/// and returns `Err`; callers DEGRADE loudly — `run_order_runtime` skips
/// the spawn attempt (the supervisor's backoff respawn retries),
/// `run_trading_pipeline` exits before OMS construction. Never a panic.
/// Runbook: `.claude/rules/project/http-client-error-codes.md`.
///
/// # Errors
///
/// Returns [`HttpClientBuildError`] when `ClientBuilder::build()` fails
/// (TLS backend init, DNS resolver init, fd exhaustion).
pub fn build_oms_http_client() -> Result<reqwest::Client, HttpClientBuildError> {
    let built = client_from_build_result(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(
                tickvault_common::constants::OMS_HTTP_TIMEOUT_SECS,
            ))
            .connect_timeout(std::time::Duration::from_secs(
                tickvault_common::constants::OMS_HTTP_CONNECT_TIMEOUT_SECS,
            ))
            .pool_idle_timeout(std::time::Duration::from_secs(
                tickvault_common::constants::OMS_HTTP_POOL_IDLE_TIMEOUT_SECS,
            ))
            .build(),
    );
    if let Err(err) = &built {
        tracing::error!(
            code = ErrorCode::HttpClient01BuildFailed.code_str(),
            site = "oms_wiring",
            error = %err,
            "HTTP-CLIENT-01: OMS HTTP client build failed — the caller degrades \
             loudly (order runtime: spawn attempt skipped, supervisor backoff \
             retries; trading pipeline: exits before OMS construction); never a \
             panic-class client fallback"
        );
        metrics::counter!("tv_http_client_build_failed_total", "site" => "oms_wiring").increment(1);
    }
    built
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bridge with an EMPTY handle must return NoToken — never panic,
    /// never a stale token.
    #[test]
    fn test_token_handle_bridge_empty_handle_is_no_token() {
        let handle: TokenHandle = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None));
        let bridge = TokenHandleBridge { handle };
        assert!(matches!(bridge.get_access_token(), Err(OmsError::NoToken)));
    }

    /// The shared client builder must produce a client on a healthy host
    /// (pinned timeouts are asserted at the constants level by
    /// `test_oms_http_timeout_is_pinned_at_5s`); the Err arm is the typed
    /// HTTP-CLIENT-01 degrade, exercised via `client_from_build_result`'s
    /// own unit tests in `tickvault_storage::http_client`.
    #[test]
    fn test_build_oms_http_client_constructs() {
        let client = build_oms_http_client();
        assert!(client.is_ok(), "normal-env build must succeed: {client:?}");
    }
}
