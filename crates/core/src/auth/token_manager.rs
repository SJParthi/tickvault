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
use super::types::{
    DhanAuthResponse, DhanCredentials, GenerateTokenRequest, RenewTokenRequest, TokenState,
};

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

    /// Dhan REST API base URL (from config).
    rest_api_base_url: String,

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
    #[instrument(skip_all)]
    async fn acquire_token(&self) -> Result<(), ApplicationError> {
        let totp_code = generate_totp_code(&self.credentials.totp_secret)?;

        let url = format!("{}{}", self.rest_api_base_url, DHAN_GENERATE_TOKEN_PATH);

        let request_body = GenerateTokenRequest {
            client_id: self.credentials.client_id.expose_secret().to_string(),
            client_secret: self.credentials.client_secret.expose_secret().to_string(),
            totp: totp_code,
        };

        let response = self
            .http_client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!("generateAccessToken request failed: {err}"),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!("generateAccessToken HTTP {status}: {body_text}"),
            });
        }

        let body: DhanAuthResponse =
            response
                .json()
                .await
                .map_err(|err| ApplicationError::AuthenticationFailed {
                    reason: format!("failed to parse auth response (HTTP {status}): {err}"),
                })?;

        if body.status != "success" {
            let remarks = body.remarks.unwrap_or_else(|| "no details".to_string());
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "Dhan API returned status='{}', remarks='{}'",
                    body.status, remarks
                ),
            });
        }

        let response_data = body
            .data
            .ok_or_else(|| ApplicationError::AuthenticationFailed {
                reason: "auth response has status=success but no data payload".to_string(),
            })?;

        if response_data.expires_in == 0 {
            return Err(ApplicationError::AuthenticationFailed {
                reason: "auth response has expires_in=0 — token would expire immediately"
                    .to_string(),
            });
        }

        let token_state = TokenState::from_response(&response_data);
        self.token.store(Arc::new(Some(token_state)));

        Ok(())
    }

    /// Attempts token renewal via the `renewToken` endpoint.
    ///
    /// Uses the current valid token + fresh TOTP. This is lighter than
    /// full authentication since it doesn't require client_secret.
    #[instrument(skip_all)]
    async fn try_renew_token(&self) -> Result<(), ApplicationError> {
        let totp_code = generate_totp_code(&self.credentials.totp_secret)?;

        let current_token = self.token.load();
        let token_state = current_token.as_ref().as_ref().ok_or_else(|| {
            ApplicationError::AuthenticationFailed {
                reason: "cannot renew — no current token".to_string(),
            }
        })?;

        let url = format!("{}{}", self.rest_api_base_url, DHAN_RENEW_TOKEN_PATH);

        let request_body = RenewTokenRequest {
            client_id: self.credentials.client_id.expose_secret().to_string(),
            totp: totp_code,
        };

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", token_state.access_token().expose_secret())
            .json(&request_body)
            .send()
            .await
            .map_err(|err| ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!("renewToken request failed: {err}"),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!("renewToken HTTP {status}: {body_text}"),
            });
        }

        let body: DhanAuthResponse =
            response
                .json()
                .await
                .map_err(|err| ApplicationError::TokenRenewalFailed {
                    attempts: 0,
                    reason: format!("failed to parse renewal response (HTTP {status}): {err}"),
                })?;

        if body.status != "success" {
            let remarks = body.remarks.unwrap_or_else(|| "no details".to_string());
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!(
                    "Dhan API returned status='{}', remarks='{}'",
                    body.status, remarks
                ),
            });
        }

        let response_data = body
            .data
            .ok_or_else(|| ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: "renewal response has status=success but no data payload".to_string(),
            })?;

        if response_data.expires_in == 0 {
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: "renewal response has expires_in=0 — token would expire immediately"
                    .to_string(),
            });
        }

        let new_token_state = TokenState::from_response(&response_data);
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
}
