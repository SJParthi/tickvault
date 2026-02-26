//! JWT token lifecycle management for Dhan API authentication.
//!
//! Manages the complete token lifecycle: initial acquisition, O(1) atomic
//! reads via arc-swap, scheduled renewal, retry with exponential backoff,
//! and graceful fallback from `renewToken` to `generateAccessToken`.
//!
//! # Boot Chain Position
//!
//! `Config → ★ Auth ★ → WebSocket → Parse → Route`
//!
//! The `TokenManager` must be initialized before any WebSocket connection
//! or REST API call. All downstream consumers receive a `TokenHandle`
//! for O(1) atomic token reads.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use secrecy::ExposeSecret;
use tracing::{error, info, instrument, warn};

use dhan_live_trader_common::config::{DhanConfig, NetworkConfig, TokenConfig};
use dhan_live_trader_common::constants::{DHAN_GENERATE_TOKEN_PATH, DHAN_RENEW_TOKEN_PATH};
use dhan_live_trader_common::error::ApplicationError;

use super::secret_manager;
use super::totp_generator::generate_totp_code;
use super::types::{DhanCredentials, DhanGenerateTokenResponse, TokenState};

// ---------------------------------------------------------------------------
// Token Handle (shared with downstream consumers)
// ---------------------------------------------------------------------------

/// Atomic handle for O(1) token reads.
///
/// Downstream consumers (WebSocket client, REST API client) hold this
/// handle and call `load()` to get the current token. The pointer is
/// swapped atomically on renewal — zero lock contention.
pub type TokenHandle = Arc<ArcSwap<Option<TokenState>>>;

// ---------------------------------------------------------------------------
// Token Manager
// ---------------------------------------------------------------------------

/// Manages the Dhan JWT token lifecycle.
///
/// Provides O(1) atomic token reads for all downstream consumers
/// via a shared `TokenHandle`. Handles initial authentication,
/// scheduled renewal, retry with backoff, and failure alerting.
pub struct TokenManager {
    /// Atomic pointer to current token state. O(1) reads.
    token: TokenHandle,

    /// Cached credentials (fetched once from SSM at startup).
    credentials: DhanCredentials,

    /// Dhan REST API base URL (for data/orders/renewal).
    rest_api_base_url: String,

    /// Dhan Auth base URL (for token generation).
    auth_base_url: String,

    /// HTTP client for Dhan API calls (reused across requests).
    http_client: reqwest::Client,

    /// Token refresh timing configuration.
    token_config: TokenConfig,

    /// Network retry configuration.
    network_config: NetworkConfig,
}

impl TokenManager {
    /// Creates a new `TokenManager` and performs initial authentication.
    ///
    /// 1. Fetches credentials from AWS SSM Parameter Store
    /// 2. Generates TOTP code
    /// 3. Calls `generateAccessToken` to acquire JWT
    /// 4. Stores token in ArcSwap for O(1) reads
    ///
    /// # Errors
    ///
    /// Returns error if SSM retrieval, TOTP generation, or Dhan API call fails.
    /// This is a fatal error — the system cannot start without authentication.
    #[instrument(skip_all)]
    pub async fn initialize(
        dhan_config: &DhanConfig,
        token_config: &TokenConfig,
        network_config: &NetworkConfig,
    ) -> Result<Arc<Self>, ApplicationError> {
        info!("initializing authentication — fetching credentials from SSM");

        // Validate HTTPS scheme — credentials must never be sent over plain HTTP.
        if !dhan_config.rest_api_base_url.starts_with("https://") {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "rest_api_base_url must use HTTPS, got: {}",
                    dhan_config.rest_api_base_url
                ),
            });
        }
        if !dhan_config.auth_base_url.starts_with("https://") {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "auth_base_url must use HTTPS, got: {}",
                    dhan_config.auth_base_url
                ),
            });
        }

        let credentials = secret_manager::fetch_dhan_credentials().await?;

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(network_config.request_timeout_ms))
            .build()
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!("HTTP client creation failed: {err}"),
            })?;

        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let manager = Arc::new(Self {
            token: Arc::clone(&token),
            credentials,
            rest_api_base_url: dhan_config.rest_api_base_url.clone(),
            auth_base_url: dhan_config.auth_base_url.clone(),
            http_client,
            token_config: token_config.clone(),
            network_config: network_config.clone(),
        });

        // Perform initial authentication
        manager.acquire_token().await?;

        info!(
            expires_at = %manager.current_expiry_display(),
            "initial authentication successful"
        );

        Ok(manager)
    }

    /// Returns a `TokenHandle` for O(1) atomic token reads.
    ///
    /// Downstream consumers (WebSocket, REST) call this once at setup,
    /// then `handle.load()` on every use. The pointer swaps atomically
    /// on renewal — zero lock contention on the hot path.
    pub fn token_handle(&self) -> TokenHandle {
        Arc::clone(&self.token)
    }

    /// Spawns the background token renewal task.
    ///
    /// Sleeps until the refresh window (token_validity - refresh_before_expiry),
    /// then renews the token. Retries with exponential backoff on failure.
    /// Runs indefinitely until the task is cancelled.
    pub fn spawn_renewal_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.renewal_loop().await;
        })
    }

    // -----------------------------------------------------------------------
    // Private — Token Acquisition
    // -----------------------------------------------------------------------

    /// Performs full authentication: TOTP → generateAccessToken → store.
    ///
    /// Calls `POST https://auth.dhan.co/app/generateAccessToken` with query params:
    /// `dhanClientId`, `pin`, `totp`. Response is flat JSON with camelCase fields.
    #[instrument(skip_all)]
    async fn acquire_token(&self) -> Result<(), ApplicationError> {
        let totp_code = generate_totp_code(&self.credentials.totp_secret)?;

        let url = format!("{}{}", self.auth_base_url, DHAN_GENERATE_TOKEN_PATH);

        // Dhan expects query params: dhanClientId, pin, totp
        // Our SSM `client_secret` maps to Dhan's `pin` (6-digit trading PIN).
        let response = self
            .http_client
            .post(&url)
            .query(&[
                ("dhanClientId", self.credentials.client_id.expose_secret()),
                ("pin", self.credentials.client_secret.expose_secret()),
                ("totp", &totp_code),
            ])
            .send()
            .await
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!("generateAccessToken request failed: {err}"),
            })?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!("generateAccessToken HTTP {status}: {body_text}"),
            });
        }

        // Dhan returns HTTP 200 even on error — check for error response first.
        // Error format: {"status":"error","message":"Invalid Pin"}
        if let Ok(error_check) = serde_json::from_str::<serde_json::Value>(&body_text)
            && error_check.get("status").and_then(|s| s.as_str()) == Some("error")
        {
            let message = error_check
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!("Dhan auth error: {message}"),
            });
        }

        let body: DhanGenerateTokenResponse =
            serde_json::from_str(&body_text)
                .map_err(|err| ApplicationError::AuthenticationFailed {
                    reason: format!(
                        "failed to parse auth response (HTTP {status}): {err}\nResponse body: {body_text}"
                    ),
                })?;

        if body.access_token.is_empty() {
            return Err(ApplicationError::AuthenticationFailed {
                reason: "generateAccessToken returned empty accessToken".to_string(),
            });
        }

        let token_state = TokenState::from_generate_response(&body);
        self.token.store(Arc::new(Some(token_state)));

        Ok(())
    }

    /// Attempts token renewal via the `RenewToken` endpoint.
    ///
    /// Calls `GET https://api.dhan.co/v2/RenewToken` with headers:
    /// `access-token` (current JWT) and `dhanClientId`.
    /// Expires the current token and returns a new one with 24h validity.
    #[instrument(skip_all)]
    async fn try_renew_token(&self) -> Result<(), ApplicationError> {
        let current_token = self.token.load();
        let token_state = current_token.as_ref().as_ref().ok_or_else(|| {
            ApplicationError::AuthenticationFailed {
                reason: "cannot renew — no current token".to_string(),
            }
        })?;

        let url = format!("{}{}", self.rest_api_base_url, DHAN_RENEW_TOKEN_PATH);

        // Dhan's RenewToken is a GET with headers, not POST with body
        let response = self
            .http_client
            .get(&url)
            .header("access-token", token_state.access_token().expose_secret())
            .header("dhanClientId", self.credentials.client_id.expose_secret())
            .send()
            .await
            .map_err(|err| ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!("RenewToken request failed: {err}"),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!("RenewToken HTTP {status}: {body_text}"),
            });
        }

        // Renewal response uses the same flat format as generateAccessToken
        let body: DhanGenerateTokenResponse =
            response
                .json()
                .await
                .map_err(|err| ApplicationError::TokenRenewalFailed {
                    attempts: 0,
                    reason: format!("failed to parse renewal response (HTTP {status}): {err}"),
                })?;

        if body.access_token.is_empty() {
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: "RenewToken returned empty accessToken".to_string(),
            });
        }

        let new_token_state = TokenState::from_generate_response(&body);
        self.token.store(Arc::new(Some(new_token_state)));

        Ok(())
    }

    /// Attempts renewal with fallback: renewToken → generateAccessToken.
    async fn renew_with_fallback(&self) -> Result<(), ApplicationError> {
        match self.try_renew_token().await {
            Ok(()) => Ok(()),
            Err(renew_err) => {
                warn!(
                    error = %renew_err,
                    "renewToken failed — falling back to generateAccessToken"
                );
                self.acquire_token().await
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private — Renewal Loop
    // -----------------------------------------------------------------------

    /// Background renewal loop with retry and backoff.
    async fn renewal_loop(self: Arc<Self>) {
        loop {
            let sleep_duration = self.time_until_next_refresh();

            if sleep_duration > Duration::ZERO {
                info!(
                    sleep_secs = sleep_duration.as_secs(),
                    "token renewal task sleeping until refresh window"
                );
                tokio::time::sleep(sleep_duration).await;
            }

            // Retry with exponential backoff
            let mut attempts: u32 = 0;
            let max_attempts = self.network_config.retry_max_attempts;
            let mut delay = Duration::from_millis(self.network_config.retry_initial_delay_ms);
            let max_delay = Duration::from_millis(self.network_config.retry_max_delay_ms);

            let mut succeeded = false;

            while attempts < max_attempts {
                attempts += 1;

                match self.renew_with_fallback().await {
                    Ok(()) => {
                        info!(
                            attempts,
                            expires_at = %self.current_expiry_display(),
                            "token renewed successfully"
                        );
                        succeeded = true;
                        break;
                    }
                    Err(err) => {
                        warn!(
                            attempt = attempts,
                            max_attempts,
                            error = %err,
                            "token renewal attempt failed"
                        );

                        if attempts >= max_attempts {
                            break;
                        }

                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(max_delay);
                    }
                }
            }

            if !succeeded {
                error!(
                    attempts,
                    "ALL token renewal attempts exhausted — system should halt"
                );
                // Future: trigger Telegram alert + SNS (Block 13)
                // For now: log CRITICAL and sleep before retrying the entire cycle
                tokio::time::sleep(Duration::from_secs(
                    dhan_live_trader_common::constants::TOKEN_RENEWAL_CIRCUIT_BREAKER_RESET_SECS,
                ))
                .await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private — Helpers
    // -----------------------------------------------------------------------

    /// Returns the duration until the next refresh should occur.
    fn time_until_next_refresh(&self) -> Duration {
        let guard = self.token.load();
        match guard.as_ref().as_ref() {
            Some(state) => state.time_until_refresh(self.token_config.refresh_before_expiry_hours),
            None => Duration::ZERO, // No token — refresh immediately
        }
    }

    /// Returns the current token expiry as a display string (for logging).
    fn current_expiry_display(&self) -> String {
        let guard = self.token.load();
        match guard.as_ref().as_ref() {
            Some(state) => state.expires_at().to_string(),
            None => "no token".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::DhanAuthResponseData;

    #[test]
    fn test_token_handle_initially_none() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let guard = handle.load();
        assert!(guard.as_ref().is_none());
    }

    #[test]
    fn test_token_handle_store_makes_available() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let response_data = DhanAuthResponseData {
            access_token: "test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&response_data);
        handle.store(Arc::new(Some(token_state)));

        let guard = handle.load();
        assert!(guard.as_ref().is_some());
    }

    #[test]
    fn test_token_handle_store_replaces_previous() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store first token
        let data1 = DhanAuthResponseData {
            access_token: "jwt-v1".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data1))));

        // Store second token
        let data2 = DhanAuthResponseData {
            access_token: "jwt-v2".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data2))));

        let guard = handle.load();
        let state = guard.as_ref().as_ref().expect("should have token");
        assert_eq!(state.access_token().expose_secret(), "jwt-v2");
    }

    #[test]
    fn test_token_handle_concurrent_reads() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let data = DhanAuthResponseData {
            access_token: "concurrent-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data))));

        // Simulate multiple concurrent readers
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let h = Arc::clone(&handle);
                std::thread::spawn(move || {
                    let guard = h.load();
                    let state = guard.as_ref().as_ref().expect("should have token");
                    assert_eq!(state.access_token().expose_secret(), "concurrent-jwt");
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread should not panic");
        }
    }

    #[test]
    fn test_token_handle_concurrent_write_and_reads() {
        // One thread stores a new token while 10 threads are reading.
        // Verify no panics, reads see either old or new value (never garbage).
        // This tests the arc-swap safety guarantee.
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store an initial token so readers can see either "initial-jwt" or "updated-jwt".
        let initial_data = DhanAuthResponseData {
            access_token: "initial-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&initial_data))));

        let barrier = Arc::new(std::sync::Barrier::new(12)); // 1 writer + 10 readers + main

        // Spawn 10 reader threads
        let reader_handles: Vec<_> = (0..10)
            .map(|_| {
                let h = Arc::clone(&handle);
                let b = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait(); // synchronize start
                    for _ in 0..1000 {
                        let guard = h.load();
                        let state = guard.as_ref().as_ref().expect("should always have a token");
                        let token_value = state.access_token().expose_secret();
                        // Must see either the old or new token, never garbage
                        assert!(
                            token_value == "initial-jwt" || token_value == "updated-jwt",
                            "unexpected token value: {token_value}"
                        );
                    }
                })
            })
            .collect();

        // Spawn 1 writer thread
        let writer_handle = {
            let h = Arc::clone(&handle);
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait(); // synchronize start
                let updated_data = DhanAuthResponseData {
                    access_token: "updated-jwt".to_string(),
                    token_type: "Bearer".to_string(),
                    expires_in: 86400,
                };
                h.store(Arc::new(Some(TokenState::from_response(&updated_data))));
            })
        };

        // Main thread participates in barrier to release everyone
        barrier.wait();

        writer_handle
            .join()
            .expect("writer thread should not panic");
        for h in reader_handles {
            h.join().expect("reader thread should not panic");
        }

        // After all threads complete, verify the final value is the updated token
        let guard = handle.load();
        let final_state = guard.as_ref().as_ref().expect("should have token");
        assert_eq!(final_state.access_token().expose_secret(), "updated-jwt");
    }

    #[test]
    fn test_token_handle_rapid_successive_stores() {
        // Store 100 different tokens rapidly in sequence, verify the final
        // read shows the last stored token. Tests that arc-swap doesn't lose updates.
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let total_stores: u32 = 100;
        for i in 0..total_stores {
            let data = DhanAuthResponseData {
                access_token: format!("jwt-{i}"),
                token_type: "Bearer".to_string(),
                expires_in: 86400,
            };
            handle.store(Arc::new(Some(TokenState::from_response(&data))));
        }

        let guard = handle.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("should have token after 100 stores");
        assert_eq!(
            state.access_token().expose_secret(),
            format!("jwt-{}", total_stores - 1),
            "final read must show the last stored token"
        );
    }

    #[test]
    fn test_token_handle_clone_shares_state() {
        // Call token_handle() pattern (Arc::clone) twice, store via first handle,
        // verify second handle sees the update. Verifies handles share the
        // underlying ArcSwap.
        let handle_one: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let handle_two: TokenHandle = Arc::clone(&handle_one);

        // Both should initially see None
        assert!(handle_one.load().as_ref().is_none());
        assert!(handle_two.load().as_ref().is_none());

        // Store via handle_one
        let data = DhanAuthResponseData {
            access_token: "shared-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle_one.store(Arc::new(Some(TokenState::from_response(&data))));

        // handle_two must see the update
        let guard = handle_two.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("handle_two should see the stored token");
        assert_eq!(
            state.access_token().expose_secret(),
            "shared-jwt",
            "cloned handle must share the same underlying ArcSwap"
        );

        // Store via handle_two, verify handle_one sees the update
        let data_v2 = DhanAuthResponseData {
            access_token: "shared-jwt-v2".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle_two.store(Arc::new(Some(TokenState::from_response(&data_v2))));

        let guard_one = handle_one.load();
        let state_one = guard_one
            .as_ref()
            .as_ref()
            .expect("handle_one should see handle_two's update");
        assert_eq!(
            state_one.access_token().expose_secret(),
            "shared-jwt-v2",
            "updates via either handle must be visible to the other"
        );
    }

    #[test]
    fn test_token_handle_store_old_token_is_dropped() {
        // Store a token, then store a new one. The old token should no longer
        // be accessible via the handle. While we can't directly test drop
        // (SecretString zeroizes on drop), we verify the old value is gone
        // and only the new value is visible.
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store first token
        let data_old = DhanAuthResponseData {
            access_token: "old-token-to-be-replaced".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data_old))));

        // Verify old token is present
        {
            let guard = handle.load();
            let state = guard.as_ref().as_ref().expect("should have old token");
            assert_eq!(
                state.access_token().expose_secret(),
                "old-token-to-be-replaced"
            );
        }

        // Store new token — this replaces the old Arc, which should be dropped
        // when its reference count reaches zero.
        let data_new = DhanAuthResponseData {
            access_token: "new-replacement-token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data_new))));

        // Verify ONLY the new token is accessible
        let guard = handle.load();
        let state = guard.as_ref().as_ref().expect("should have new token");
        assert_eq!(
            state.access_token().expose_secret(),
            "new-replacement-token",
            "only the new token should be accessible after replacement"
        );
        assert_ne!(
            state.access_token().expose_secret(),
            "old-token-to-be-replaced",
            "old token value must not be accessible"
        );

        // Additionally verify that storing None clears the token entirely
        handle.store(Arc::new(None));
        let guard_after_clear = handle.load();
        assert!(
            guard_after_clear.as_ref().is_none(),
            "storing None must clear the token state"
        );
    }
}
