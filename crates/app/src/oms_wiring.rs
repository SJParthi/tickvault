//! Shared OMS wiring — the `TokenHandle` → `TokenProvider` adapter and the
//! pinned-timeout HTTP client builder used by EVERY OMS construction site
//! (the dhan-on `trading_pipeline` and the dhan-off `order_runtime`).
//!
//! Extracted from `trading_pipeline.rs` (order-runtime dry-run PR,
//! 2026-07-14) so the two sites can never drift on the `OMS_HTTP_*`
//! constant usage (`test_oms_http_timeout_is_pinned_at_5s` pins the
//! constants themselves in `crates/common/src/constants.rs`).

use secrecy::{ExposeSecret, SecretString};

use tickvault_core::auth::token_manager::TokenHandle;
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
/// (Phase 0 Item 22a — 5s request / connect, pooled idle). Falls back to
/// the reqwest default client if the builder fails (cold path; the
/// dry-run OMS never issues HTTP anyway).
#[must_use]
pub fn build_oms_http_client() -> reqwest::Client {
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
        .build()
        .unwrap_or_default()
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

    /// The shared client builder must produce a client (pinned timeouts are
    /// asserted at the constants level by
    /// `test_oms_http_timeout_is_pinned_at_5s`).
    #[test]
    fn test_build_oms_http_client_constructs() {
        let _client = build_oms_http_client();
    }
}
