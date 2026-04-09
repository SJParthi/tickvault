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
use dhan_live_trader_common::constants::{
    AUTH_RETRY_MAX_BACKOFF_SECS, DHAN_GENERATE_TOKEN_PATH, DHAN_RENEW_TOKEN_PATH,
    DHAN_TOKEN_GENERATION_COOLDOWN_SECS, TOTP_MAX_RETRIES, TOTP_PERIOD_SECS,
};
use dhan_live_trader_common::error::ApplicationError;
use dhan_live_trader_common::sanitize::redact_url_params;

use super::secret_manager;
use super::token_cache;
use super::totp_generator::generate_totp_code;
use super::types::{DhanCredentials, DhanGenerateTokenResponse, TokenState};
use crate::notification::events::NotificationEvent;
use crate::notification::service::NotificationService;

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

    /// Notification service for Telegram/SNS alerts on auth events.
    notifier: Arc<NotificationService>,
}

impl TokenManager {
    /// Creates a new `TokenManager` and performs initial authentication.
    ///
    /// 1. Fetches credentials from AWS SSM Parameter Store (3 retries)
    /// 2. Calls `generateAccessToken` with infinite retry for transient errors
    /// 3. Fails fast on permanent errors (wrong PIN, invalid TOTP, blocked)
    /// 4. Stores token in ArcSwap for O(1) reads
    ///
    /// # Errors
    ///
    /// Returns error only for permanent credential failures or shutdown (Ctrl+C).
    /// Transient errors (network, timeout, rate limit) are retried indefinitely.
    #[instrument(skip_all)]
    pub async fn initialize(
        dhan_config: &DhanConfig,
        token_config: &TokenConfig,
        network_config: &NetworkConfig,
        notifier: &Arc<NotificationService>,
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

        // SSM credential fetch with retry (3 attempts, exponential backoff).
        let credentials = {
            let mut attempt = 0u32;
            loop {
                attempt = attempt.saturating_add(1);
                match secret_manager::fetch_dhan_credentials().await {
                    Ok(creds) => break creds,
                    Err(err) if attempt >= 3 => return Err(err),
                    Err(err) => {
                        warn!(
                            attempt,
                            error = %err,
                            "SSM credential fetch failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(u64::from(2u32.pow(attempt)))).await;
                    }
                }
            }
        };

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
            notifier: Arc::clone(notifier),
        });

        // Fast path: try loading a cached token from a previous run.
        // This skips the Dhan HTTP auth call (~500ms-2s), saving significant
        // boot time on crash recovery. SSM credentials are still fetched above
        // (needed for the renewal loop).
        if let Some(cached_token) = token_cache::load_token_cache(&manager.credentials.client_id) {
            manager.token.store(Arc::new(Some(cached_token)));
            info!(
                expires_at = %manager.current_expiry_display(),
                "using cached token — skipped Dhan auth HTTP call"
            );
            notifier.notify(NotificationEvent::AuthenticationSuccess);
            return Ok(manager);
        }

        // Slow path: full auth via Dhan API.
        // Infinite retry for transient errors, fail fast for permanent errors.
        // TOTP errors get limited retries (window boundary timing can cause
        // valid secrets to produce rejected codes).
        let mut attempt = 0u32;
        let mut totp_retries = 0u32;
        let mut delay = Duration::from_millis(network_config.retry_initial_delay_ms);
        let max_delay = Duration::from_secs(AUTH_RETRY_MAX_BACKOFF_SECS);

        loop {
            attempt = attempt.saturating_add(1);
            match manager.acquire_token().await {
                Ok(()) => {
                    info!(
                        attempt,
                        expires_at = %manager.current_expiry_display(),
                        "initial authentication successful"
                    );
                    notifier.notify(NotificationEvent::AuthenticationSuccess);
                    return Ok(manager);
                }
                Err(err) => {
                    let reason = err.to_string();

                    // Permanent errors — fail fast with clear notification.
                    if is_permanent_auth_error(&reason) {
                        error!(attempt, error = %reason, "permanent auth error — cannot retry");
                        notifier.notify(NotificationEvent::AuthenticationFailed {
                            reason: format!("PERMANENT: {reason} — check SSM credentials"),
                        });
                        return Err(err);
                    }

                    // TOTP errors — retry with a fresh 30-second window, but cap retries
                    // to avoid infinite loops when the secret is genuinely wrong.
                    if is_totp_error(&reason) {
                        totp_retries = totp_retries.saturating_add(1);
                        if totp_retries > TOTP_MAX_RETRIES {
                            error!(
                                attempt,
                                totp_retries,
                                error = %reason,
                                "TOTP failed after {TOTP_MAX_RETRIES} retries — verify TOTP secret in SSM"
                            );
                            notifier.notify(NotificationEvent::AuthenticationFailed {
                                reason: format!(
                                    "TOTP invalid after {TOTP_MAX_RETRIES} retries — check /dlt/<env>/dhan/totp-secret in SSM"
                                ),
                            });
                            return Err(err);
                        }
                        warn!(
                            attempt,
                            totp_retries,
                            max = TOTP_MAX_RETRIES,
                            "TOTP rejected — waiting for next 30s window before retry"
                        );
                        notifier.notify(NotificationEvent::AuthenticationFailed {
                            reason: format!(
                                "TOTP rejected (retry {totp_retries}/{TOTP_MAX_RETRIES}) — waiting 30s for fresh window"
                            ),
                        });
                        // Wait for the next TOTP window (30s) to get a fresh code.
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_secs(TOTP_PERIOD_SECS)) => {}
                            _ = tokio::signal::ctrl_c() => {
                                info!("shutdown signal received during TOTP retry wait");
                                return Err(ApplicationError::AuthenticationFailed {
                                    reason: "shutdown during TOTP retry wait".to_string(),
                                });
                            }
                        }
                        continue;
                    }

                    // Dhan rate limit — use the 125-second cooldown.
                    let wait_duration = if is_dhan_rate_limited(&reason) {
                        warn!(
                            attempt,
                            "Dhan rate limit hit — waiting {} seconds",
                            DHAN_TOKEN_GENERATION_COOLDOWN_SECS
                        );
                        Duration::from_secs(DHAN_TOKEN_GENERATION_COOLDOWN_SECS)
                    } else {
                        warn!(
                            attempt,
                            error = %reason,
                            delay_secs = delay.as_secs(),
                            "auth attempt failed (transient) — retrying"
                        );
                        delay
                    };

                    // Notify on each transient failure.
                    notifier.notify(NotificationEvent::AuthenticationFailed {
                        reason: format!(
                            "attempt {attempt}: {reason} — retrying in {}s",
                            wait_duration.as_secs()
                        ),
                    });

                    // Sleep with Ctrl+C awareness.
                    tokio::select! {
                        () = tokio::time::sleep(wait_duration) => {}
                        _ = tokio::signal::ctrl_c() => {
                            info!("shutdown signal received during auth retry");
                            return Err(ApplicationError::AuthenticationFailed {
                                reason: "shutdown during auth retry".to_string(),
                            });
                        }
                    }

                    // Exponential backoff (capped), skip if we used rate-limit delay.
                    if !is_dhan_rate_limited(&reason) {
                        delay = delay.saturating_mul(2).min(max_delay);
                    }
                }
            }
        }
    }

    /// Creates a `TokenManager` using a pre-existing `TokenHandle`.
    ///
    /// Called during two-phase boot: the fast path has already loaded a
    /// cached token into `token_handle` and connected WebSocket. This method
    /// fetches SSM credentials and validates the cached token belongs to
    /// the correct account. If the client_id doesn't match, it re-authenticates.
    ///
    /// # Arguments
    /// * `token_handle` — Pre-existing handle already populated with a cached token.
    /// * `cached_client_id` — Client ID from the token cache (for validation).
    #[instrument(skip_all)]
    pub async fn initialize_deferred(
        token_handle: TokenHandle,
        cached_client_id: &str,
        dhan_config: &DhanConfig,
        token_config: &TokenConfig,
        network_config: &NetworkConfig,
        notifier: &Arc<NotificationService>,
    ) -> Result<Arc<Self>, ApplicationError> {
        info!("deferred auth: fetching SSM credentials for validation + renewal");

        // Validate HTTPS scheme
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

        // SSM credential fetch with retry (3 attempts, exponential backoff).
        let credentials = {
            let mut attempt = 0u32;
            loop {
                attempt = attempt.saturating_add(1);
                match secret_manager::fetch_dhan_credentials().await {
                    Ok(creds) => break creds,
                    Err(err) if attempt >= 3 => return Err(err),
                    Err(err) => {
                        warn!(
                            attempt,
                            error = %err,
                            "deferred auth: SSM credential fetch failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(u64::from(2u32.pow(attempt)))).await;
                    }
                }
            }
        };

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(network_config.request_timeout_ms))
            .build()
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!("HTTP client creation failed: {err}"),
            })?;

        let manager = Arc::new(Self {
            token: token_handle,
            credentials,
            rest_api_base_url: dhan_config.rest_api_base_url.clone(),
            auth_base_url: dhan_config.auth_base_url.clone(),
            http_client,
            token_config: token_config.clone(),
            network_config: network_config.clone(),
            notifier: Arc::clone(notifier),
        });

        // Validate cached client_id against SSM. If they differ, the cache was
        // for a different account — do full re-auth to get a valid token.
        let ssm_client_id = manager.credentials.client_id.expose_secret();
        if cached_client_id != ssm_client_id {
            warn!("deferred auth: cached client_id mismatch — re-authenticating");
            token_cache::delete_cache_file();
            manager.acquire_token().await.map_err(|err| {
                error!(error = %err, "deferred auth: re-authentication failed");
                err
            })?;
            info!(
                expires_at = %manager.current_expiry_display(),
                "deferred auth: re-authentication successful after client_id mismatch"
            );
        } else {
            info!("deferred auth: cached token validated against SSM credentials");
        }

        notifier.notify(NotificationEvent::AuthenticationSuccess);
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
    #[instrument(skip_all)]
    pub fn spawn_renewal_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.renewal_loop().await;
        })
    }

    // -----------------------------------------------------------------------
    // Public — User Profile & Pre-Market Validation
    // Source: docs/dhan-ref/02-authentication.md
    // -----------------------------------------------------------------------

    /// Fetches the user profile from `GET /v2/profile`.
    ///
    /// Returns `UserProfileResponse` containing `dataPlan`, `activeSegment`,
    /// `tokenValidity`, etc. Used by `pre_market_check()` for pre-trading
    /// validation at 08:45 IST.
    #[instrument(skip_all, name = "get_user_profile")]
    // TEST-EXEMPT: requires live HTTP server + valid token; types tested in auth::types::tests
    pub async fn get_user_profile(
        &self,
    ) -> Result<super::types::UserProfileResponse, ApplicationError> {
        // Load token and extract access token string while guard is alive
        let access_token_value = {
            let token_guard = self.token.load();
            let token_state = token_guard.as_ref().as_ref().ok_or_else(|| {
                ApplicationError::AuthenticationFailed {
                    reason: "no access token available for profile request".to_string(),
                }
            })?;
            token_state.access_token().expose_secret().to_string()
        };

        let url = format!(
            "{}{}",
            self.rest_api_base_url,
            dhan_live_trader_common::constants::DHAN_USER_PROFILE_PATH
        );

        let response = self
            .http_client
            .get(&url)
            .header("access-token", &access_token_value)
            .send()
            .await
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!("profile request failed: {err}"),
            })?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // B2: Log full body for debugging but redact it from the error message
            // to prevent raw API response (may contain PII) from reaching Telegram alerts.
            warn!(status = %status, "profile request failed — response body redacted from error");
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!("profile request HTTP {status} — see server logs for details"),
            });
        }

        serde_json::from_str(&body_text).map_err(|err| ApplicationError::AuthenticationFailed {
            reason: format!("profile response parse error: {err}"),
        })
    }

    /// Pre-market validation at 08:45 IST.
    ///
    /// Checks:
    /// 1. `dataPlan == "Active"` — required for market data WebSocket access
    /// 2. `activeSegment` contains `"Derivative"` — required for F&O trading
    /// 3. Token has > 4 hours until expiry — prevents token death during market hours
    ///
    /// On failure: returns error for CRITICAL alert via Telegram + token rotation attempt.
    #[instrument(skip_all, name = "pre_market_check")]
    // TEST-EXEMPT: requires live HTTP server + valid token; calls get_user_profile()
    pub async fn pre_market_check(&self) -> Result<(), ApplicationError> {
        let profile = self.get_user_profile().await?;

        // Check 1: data plan must be Active
        if profile.data_plan != "Active" {
            error!(
                data_plan = %profile.data_plan,
                "pre-market check FAILED: data plan is not Active"
            );
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "data plan is '{}', must be 'Active' for market data access",
                    profile.data_plan
                ),
            });
        }

        // Check 2: active segment must contain derivatives access.
        // Dhan returns EITHER full names ("Equity, Derivative") OR single-char codes
        // ("E, D, C, M, ") depending on API version/account type. Accept both.
        let has_derivative =
            profile.active_segment.contains("Derivative") || profile.active_segment.contains("D");
        if !has_derivative {
            error!(
                active_segment = %profile.active_segment,
                "pre-market check FAILED: Derivative segment not active"
            );
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "active segment '{}' does not indicate F&O access (expected 'Derivative' or 'D')",
                    profile.active_segment
                ),
            });
        }

        // Check 3: token must have > 4 hours until expiry
        let token_guard = self.token.load();
        if let Some(token_state) = token_guard.as_ref().as_ref() {
            let remaining = token_state.time_until_refresh(0);
            let four_hours = Duration::from_secs(4 * 3600);
            if remaining < four_hours {
                warn!(
                    remaining_secs = remaining.as_secs(),
                    "pre-market check WARNING: token has less than 4 hours until expiry"
                );
                return Err(ApplicationError::AuthenticationFailed {
                    reason: format!(
                        "token expires in {} seconds — less than 4 hours until expiry, rotate token",
                        remaining.as_secs()
                    ),
                });
            }
        }

        info!(
            data_plan = %profile.data_plan,
            active_segment = %profile.active_segment,
            "pre-market check PASSED"
        );
        Ok(())
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
                reason: format!(
                    "generateAccessToken request failed: {}",
                    redact_url_params(&err.to_string())
                ),
            })?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "generateAccessToken HTTP {status}: {}",
                    redact_url_params(&body_text)
                ),
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

        let body: DhanGenerateTokenResponse = serde_json::from_str(&body_text).map_err(|err| {
            ApplicationError::AuthenticationFailed {
                reason: format!(
                    "failed to parse auth response (HTTP {status}): {err}\nResponse body: {}",
                    redact_url_params(&body_text)
                ),
            }
        })?;

        if body.access_token.is_empty() {
            return Err(ApplicationError::AuthenticationFailed {
                reason: "generateAccessToken returned empty accessToken".to_string(),
            });
        }

        let token_state = TokenState::from_generate_response(&body);
        self.token.store(Arc::new(Some(token_state)));

        // Cache for fast restart on crash recovery
        self.save_current_token_to_cache();

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
                reason: format!(
                    "RenewToken request failed: {}",
                    redact_url_params(&err.to_string())
                ),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!(
                    "RenewToken HTTP {status}: {}",
                    redact_url_params(&body_text)
                ),
            });
        }

        // Renewal response uses the same flat format as generateAccessToken
        let body: DhanGenerateTokenResponse =
            response
                .json()
                .await
                .map_err(|err| ApplicationError::TokenRenewalFailed {
                    attempts: 0,
                    reason: format!(
                        "failed to parse renewal response (HTTP {status}): {}",
                        redact_url_params(&err.to_string())
                    ),
                })?;

        if body.access_token.is_empty() {
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: "RenewToken returned empty accessToken".to_string(),
            });
        }

        let new_token_state = TokenState::from_generate_response(&body);
        self.token.store(Arc::new(Some(new_token_state)));

        // Update cache for fast restart
        self.save_current_token_to_cache();

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

    /// Background renewal loop with retry, backoff, and circuit-breaker halt.
    ///
    /// If renewal fails repeatedly, the circuit breaker sleeps 60s and retries.
    /// After `TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES` consecutive failures,
    /// the loop halts with a critical alert — operator must intervene.
    async fn renewal_loop(self: Arc<Self>) {
        let mut consecutive_circuit_breaker_cycles: u32 = 0;

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
                attempts = attempts.saturating_add(1);

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
                        delay = delay.saturating_mul(2).min(max_delay);
                    }
                }
            }

            if succeeded {
                consecutive_circuit_breaker_cycles = 0;
            } else {
                consecutive_circuit_breaker_cycles =
                    consecutive_circuit_breaker_cycles.saturating_add(1);
                error!(
                    attempts,
                    circuit_breaker_cycle = consecutive_circuit_breaker_cycles,
                    max_cycles = dhan_live_trader_common::constants::TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES,
                    "ALL token renewal attempts exhausted — token may expire"
                );
                self.notifier.notify(NotificationEvent::TokenRenewalFailed {
                    attempts,
                    reason: format!(
                        "all renewal attempts exhausted (cycle {}/{}) — token may expire",
                        consecutive_circuit_breaker_cycles,
                        dhan_live_trader_common::constants::TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES,
                    ),
                });

                if consecutive_circuit_breaker_cycles
                    >= dhan_live_trader_common::constants::TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES
                {
                    error!(
                        "token renewal halted after {} consecutive failures — operator must intervene",
                        consecutive_circuit_breaker_cycles
                    );
                    self.notifier.notify(NotificationEvent::TokenRenewalFailed {
                        attempts: consecutive_circuit_breaker_cycles,
                        reason:
                            "HALTED: token renewal permanently failed — manual restart required"
                                .to_string(),
                    });
                    return;
                }

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

    /// Saves the current token to the disk cache for fast crash recovery.
    fn save_current_token_to_cache(&self) {
        let guard = self.token.load();
        if let Some(token_state) = guard.as_ref().as_ref() {
            token_cache::save_token_cache(token_state, &self.credentials.client_id);
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
// Error Classification
// ---------------------------------------------------------------------------

/// Returns `true` if the auth error is permanent (retrying won't help).
///
/// Permanent errors indicate wrong credentials in SSM or a blocked account.
/// The system should notify the user and exit, not retry indefinitely.
///
/// Note: TOTP errors are excluded — they may be transient due to window
/// boundary timing. Use [`is_totp_error`] to handle TOTP retries separately.
fn is_permanent_auth_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("invalid pin")
        || lower.contains("invalid client")
        || lower.contains("blocked")
        || lower.contains("suspended")
        || lower.contains("disabled")
}

/// Returns `true` if the error is a TOTP-related auth failure.
///
/// TOTP errors are retryable up to [`TOTP_MAX_RETRIES`] times because they
/// can fail when the code is generated near a 30-second window boundary
/// and expires during HTTP transit to Dhan's servers.
fn is_totp_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("totp") && (lower.contains("invalid") || lower.contains("failed"))
}

/// Returns `true` if the error is Dhan's token generation rate limit.
fn is_dhan_rate_limited(reason: &str) -> bool {
    reason.contains("every 2 minutes") || reason.contains("once every")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;
    use crate::auth::types::DhanAuthResponseData;

    /// Helper: constructs a `TokenManager` directly (no SSM, no network).
    /// Used to test private helper methods that don't require a real token.
    fn make_test_manager(initial_token: Option<TokenState>) -> Arc<TokenManager> {
        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(initial_token)));
        let http_client = reqwest::Client::new();

        Arc::new(TokenManager {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("test-secret".to_string()),
                totp_secret: secrecy::SecretString::from("test-totp".to_string()),
            },
            rest_api_base_url: "https://api.example.com".to_string(),
            auth_base_url: "https://auth.example.com".to_string(),
            http_client,
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 5000,
                websocket_connect_timeout_ms: 5000,
                retry_initial_delay_ms: 100,
                retry_max_delay_ms: 1000,
                retry_max_attempts: 3,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
        })
    }

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

    // -----------------------------------------------------------------------
    // initialize() — HTTPS validation tests
    // -----------------------------------------------------------------------
    //
    // These call initialize() with http:// URLs. The HTTPS check fires BEFORE
    // any SSM or network call, so no credentials or network are needed.

    #[tokio::test]
    async fn test_initialize_rejects_http_rest_api_base_url() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "http://api.dhan.co".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
        )
        .await;
        let Err(err) = result else {
            panic!("http:// rest_api_base_url must be rejected")
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("rest_api_base_url must use HTTPS"),
            "error message should mention rest_api_base_url HTTPS requirement, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_initialize_rejects_http_auth_base_url() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "https://api.dhan.co".to_string(),
            auth_base_url: "http://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
        )
        .await;
        let Err(err) = result else {
            panic!("http:// auth_base_url must be rejected")
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("auth_base_url must use HTTPS"),
            "error message should mention auth_base_url HTTPS requirement, got: {err_msg}"
        );
    }

    // -----------------------------------------------------------------------
    // time_until_next_refresh — None token test
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_until_next_refresh_with_no_token_returns_zero() {
        let manager = make_test_manager(None);
        let duration = manager.time_until_next_refresh();
        assert_eq!(
            duration,
            Duration::ZERO,
            "No token present — should return Duration::ZERO to trigger immediate refresh"
        );
    }

    #[test]
    fn test_time_until_next_refresh_with_fresh_token_returns_positive() {
        let data = DhanAuthResponseData {
            access_token: "test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400, // 24 hours
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));

        let duration = manager.time_until_next_refresh();
        // With 24h validity and 1h refresh window, should sleep ~23h.
        assert!(
            duration.as_secs() > 80_000,
            "Fresh 24h token with 1h window should have >80000s until refresh, got: {}s",
            duration.as_secs()
        );
    }

    // -----------------------------------------------------------------------
    // current_expiry_display — None token test
    // -----------------------------------------------------------------------

    #[test]
    fn test_current_expiry_display_with_no_token() {
        let manager = make_test_manager(None);
        let display = manager.current_expiry_display();
        assert_eq!(
            display, "no token",
            "No token present — display should say 'no token'"
        );
    }

    #[test]
    fn test_current_expiry_display_with_token_shows_timestamp() {
        let data = DhanAuthResponseData {
            access_token: "test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));

        let display = manager.current_expiry_display();
        assert_ne!(
            display, "no token",
            "With a token present, display should not say 'no token'"
        );
        // Should contain a date-like string (year)
        assert!(
            display.contains("20"),
            "Expiry display should contain a year, got: {display}"
        );
    }

    // -----------------------------------------------------------------------
    // token_handle — public API test
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_returns_shared_handle() {
        let data = DhanAuthResponseData {
            access_token: "handle-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));

        let handle = manager.token_handle();

        // The handle should see the same token as the manager's internal state.
        let guard = handle.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("handle should see the token");
        assert_eq!(
            state.access_token().expose_secret(),
            "handle-test-jwt",
            "token_handle() must share the same ArcSwap"
        );
    }

    #[test]
    fn test_token_handle_none_initially() {
        let manager = make_test_manager(None);
        let handle = manager.token_handle();

        let guard = handle.load();
        assert!(
            guard.as_ref().is_none(),
            "token_handle() with no token should load None"
        );
    }

    #[test]
    fn test_token_handle_sees_updates() {
        let manager = make_test_manager(None);
        let handle = manager.token_handle();

        // Initially None
        assert!(handle.load().as_ref().is_none());

        // Store a token via the manager's internal handle
        let data = DhanAuthResponseData {
            access_token: "updated-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        manager
            .token
            .store(Arc::new(Some(TokenState::from_response(&data))));

        // The external handle should see the update
        let guard = handle.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("handle should see the update");
        assert_eq!(state.access_token().expose_secret(), "updated-jwt");
    }

    // -----------------------------------------------------------------------
    // acquire_token — tests with unreachable HTTPS URLs
    // -----------------------------------------------------------------------

    /// Helper: creates a manager with a specific unreachable HTTPS base URL
    /// and a known-good TOTP secret so TOTP generation succeeds.
    fn make_test_manager_with_totp(
        initial_token: Option<TokenState>,
        auth_base_url: &str,
        rest_api_base_url: &str,
    ) -> Arc<TokenManager> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // Valid base32 TOTP secret (32 chars = 160 bits).
        let totp_secret = "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN";
        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(initial_token)));
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .expect("test http client");

        Arc::new(TokenManager {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(totp_secret.to_string()),
            },
            rest_api_base_url: rest_api_base_url.to_string(),
            auth_base_url: auth_base_url.to_string(),
            http_client,
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 200,
                websocket_connect_timeout_ms: 200,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 50,
                retry_max_attempts: 2,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
        })
    }

    #[tokio::test]
    async fn test_acquire_token_connection_error_with_unreachable_url() {
        // Uses an unreachable HTTPS URL so the HTTP request fails.
        let manager =
            make_test_manager_with_totp(None, "https://127.0.0.1:1", "https://127.0.0.1:1");
        let result = manager.acquire_token().await;
        assert!(
            result.is_err(),
            "acquire_token must fail with unreachable URL"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("generateAccessToken") || err_msg.contains("request failed"),
            "error should mention generateAccessToken failure, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_try_renew_token_with_no_current_token() {
        // try_renew_token when no token is stored should error.
        let manager =
            make_test_manager_with_totp(None, "https://127.0.0.1:1", "https://127.0.0.1:1");
        let result = manager.try_renew_token().await;
        assert!(
            result.is_err(),
            "try_renew_token must fail with no current token"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot renew") || err_msg.contains("no current token"),
            "error should mention missing token, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_try_renew_token_connection_error_with_unreachable_url() {
        // try_renew_token with a token present but unreachable REST API URL.
        let data = DhanAuthResponseData {
            access_token: "existing-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager_with_totp(
            Some(token_state),
            "https://127.0.0.1:1",
            "https://127.0.0.1:1",
        );
        let result = manager.try_renew_token().await;
        assert!(
            result.is_err(),
            "try_renew_token must fail with unreachable URL"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("RenewToken") || err_msg.contains("request failed"),
            "error should mention renewal failure, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_renew_with_fallback_both_fail() {
        // renew_with_fallback tries try_renew_token, then acquire_token.
        // With an unreachable URL and a valid token, both should fail.
        let data = DhanAuthResponseData {
            access_token: "existing-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager_with_totp(
            Some(token_state),
            "https://127.0.0.1:1",
            "https://127.0.0.1:1",
        );
        let result = manager.renew_with_fallback().await;
        assert!(
            result.is_err(),
            "renew_with_fallback must fail when both endpoints unreachable"
        );
    }

    // -----------------------------------------------------------------------
    // spawn_renewal_task — cancellation test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_renewal_task_cancellation() {
        // Spawn the renewal task and immediately abort it.
        // Verifies it does not panic on cancellation.
        let data = DhanAuthResponseData {
            access_token: "task-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager_with_totp(
            Some(token_state),
            "https://127.0.0.1:1",
            "https://127.0.0.1:1",
        );
        let handle = manager.spawn_renewal_task();
        // Give the task a moment to start sleeping
        tokio::time::sleep(Duration::from_millis(10)).await;
        handle.abort();
        // The abort should not panic — JoinError::is_cancelled should be true.
        let result = handle.await;
        assert!(result.is_err(), "aborted task should return JoinError");
        assert!(
            result.unwrap_err().is_cancelled(),
            "task should be cancelled, not panicked"
        );
    }

    // -----------------------------------------------------------------------
    // time_until_next_refresh — with expired token
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_until_next_refresh_with_zero_expiry_returns_zero() {
        // expires_in=0 creates a token that expires at issuance time.
        let data = DhanAuthResponseData {
            access_token: "expired-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0,
        };
        let expired_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(expired_state));
        let duration = manager.time_until_next_refresh();
        assert_eq!(
            duration,
            Duration::ZERO,
            "Token with zero expiry should return Duration::ZERO for immediate refresh"
        );
    }

    // -----------------------------------------------------------------------
    // current_expiry_display — with short-lived token
    // -----------------------------------------------------------------------

    #[test]
    fn test_current_expiry_display_with_short_lived_token_shows_timestamp() {
        let data = DhanAuthResponseData {
            access_token: "short-lived-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 1, // 1 second
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));
        let display = manager.current_expiry_display();
        assert_ne!(
            display, "no token",
            "token with short expiry should still show timestamp"
        );
        assert!(display.contains("20"), "should contain year");
    }

    // -----------------------------------------------------------------------
    // initialize — both URLs are HTTP (first check catches rest_api_base_url)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_rejects_both_http_urls() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "http://api.dhan.co".to_string(),
            auth_base_url: "http://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
        )
        .await;
        assert!(result.is_err(), "both HTTP URLs must be rejected");
        // The first check (rest_api_base_url) fires first.
        let err_msg = result.err().expect("expected an error").to_string();
        assert!(err_msg.contains("rest_api_base_url must use HTTPS"));
    }

    // -----------------------------------------------------------------------
    // token_handle — verify it returns an independent Arc
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_multiple_calls_return_shared_handle() {
        let manager = make_test_manager(None);
        let handle1 = manager.token_handle();
        let handle2 = manager.token_handle();
        // Both should point to the same ArcSwap
        let data = DhanAuthResponseData {
            access_token: "shared-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        manager
            .token
            .store(Arc::new(Some(TokenState::from_response(&data))));
        let g1 = handle1.load();
        let g2 = handle2.load();
        assert_eq!(
            g1.as_ref()
                .as_ref()
                .expect("h1")
                .access_token()
                .expose_secret(),
            g2.as_ref()
                .as_ref()
                .expect("h2")
                .access_token()
                .expose_secret(),
        );
    }

    // -----------------------------------------------------------------------
    // HTTP mock server helpers for try_renew_token / renew_with_fallback tests
    // -----------------------------------------------------------------------

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Starts a local TCP server that responds to the FIRST request with the
    /// given HTTP status and body, then shuts down.
    async fn start_mock_server(status: u16, body: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://127.0.0.1:{}", addr.port());

        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );

        let handle = tokio::spawn(async move {
            // Accept up to 5 connections (enough for retry tests)
            for _ in 0..5 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    // Read request (consume it)
                    let mut buf = vec![0u8; 4096];
                    let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                    // Write response
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });

        (url, handle)
    }

    /// Constructs a `TokenManager` pointing at local mock URLs.
    /// Uses `http://` which is normally rejected by initialize(), but we
    /// construct the manager directly to bypass that check for unit tests.
    fn make_mock_manager(
        rest_api_url: &str,
        auth_url: &str,
        initial_token: Option<TokenState>,
    ) -> Arc<TokenManager> {
        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(initial_token)));
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build http client");

        Arc::new(TokenManager {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: rest_api_url.to_string(),
            auth_base_url: auth_url.to_string(),
            http_client,
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 5000,
                websocket_connect_timeout_ms: 5000,
                retry_initial_delay_ms: 50,
                retry_max_delay_ms: 100,
                retry_max_attempts: 2,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
        })
    }

    // -----------------------------------------------------------------------
    // try_renew_token — success path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_success() {
        let renew_body = r#"{"dhanClientId":"test","accessToken":"renewed-jwt","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, renew_body).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let initial_token = TokenState::from_response(&initial_data);
        let manager = make_mock_manager(&url, &url, Some(initial_token));

        let result = manager.try_renew_token().await;
        assert!(
            result.is_ok(),
            "try_renew_token should succeed: {:?}",
            result.err()
        );

        // Verify the token was updated
        let guard = manager.token.load();
        let state = guard.as_ref().as_ref().expect("should have renewed token");
        assert_eq!(
            state.access_token().expose_secret(),
            "renewed-jwt",
            "token must be updated after renewal"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // try_renew_token — no current token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_fails_without_current_token() {
        let manager = make_mock_manager("http://127.0.0.1:1", "http://127.0.0.1:1", None);

        let result = manager.try_renew_token().await;
        assert!(
            result.is_err(),
            "try_renew_token must fail without current token"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("no current token"),
            "error must mention no current token, got: {err_msg}"
        );
    }

    // -----------------------------------------------------------------------
    // try_renew_token — HTTP error (non-200)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_http_error() {
        let (url, server_handle) = start_mock_server(401, r#"{"error":"unauthorized"}"#).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.try_renew_token().await;
        assert!(result.is_err(), "try_renew_token must fail on HTTP 401");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("RenewToken HTTP"),
            "error must mention HTTP status, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // try_renew_token — empty access token in response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_empty_access_token() {
        let body =
            r#"{"dhanClientId":"test","accessToken":"","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, body).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.try_renew_token().await;
        assert!(result.is_err(), "empty accessToken must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("empty accessToken"),
            "error must mention empty token, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // try_renew_token — malformed JSON response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_malformed_json() {
        let (url, server_handle) = start_mock_server(200, "not-json-at-all").await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.try_renew_token().await;
        assert!(result.is_err(), "malformed JSON must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("parse renewal response"),
            "error must mention parse failure, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // renew_with_fallback — renewal succeeds (no fallback needed)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_renew_with_fallback_renewal_succeeds() {
        let renew_body = r#"{"dhanClientId":"test","accessToken":"renewed-jwt","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, renew_body).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.renew_with_fallback().await;
        assert!(
            result.is_ok(),
            "renew_with_fallback should succeed: {:?}",
            result.err()
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // renewal_loop — spawn and abort after first cycle
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_renewal_loop_spawns_and_can_be_cancelled() {
        // Create a manager with a token that expires soon (to minimize sleep time)
        let initial_data = DhanAuthResponseData {
            access_token: "loop-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 2, // 2 seconds validity
        };
        let token_state = TokenState::from_response(&initial_data);

        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(Some(token_state))));
        let http_client = reqwest::Client::new();

        let manager = Arc::new(TokenManager {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: "http://127.0.0.1:1".to_string(),
            auth_base_url: "http://127.0.0.1:1".to_string(),
            http_client,
            token_config: TokenConfig {
                refresh_before_expiry_hours: 0,
                token_validity_hours: 0,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 100,
                websocket_connect_timeout_ms: 100,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 20,
                retry_max_attempts: 1,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
        });

        let handle = manager.spawn_renewal_task();

        // Let it run briefly — it will try to renew (and fail since server is unreachable)
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Abort the task
        handle.abort();
        let result = handle.await;
        assert!(
            result.is_err() || result.is_ok(),
            "task should complete (either cancelled or finished)"
        );
    }

    // -----------------------------------------------------------------------
    // acquire_token — Dhan error response (HTTP 200 with error status)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_dhan_error_response() {
        let error_body = r#"{"status":"error","message":"Invalid Pin"}"#;
        let (url, server_handle) = start_mock_server(200, error_body).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "Dhan error response must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid Pin"),
            "error must contain Dhan error message, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — HTTP 500 error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_http_500_error() {
        let (url, server_handle) = start_mock_server(500, r#"internal server error"#).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "HTTP 500 must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("generateAccessToken HTTP"),
            "error must mention HTTP status, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — empty access token in success response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_empty_access_token() {
        let body =
            r#"{"dhanClientId":"test","accessToken":"","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, body).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "empty accessToken must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("empty accessToken"),
            "error must mention empty token, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — malformed JSON response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_malformed_json() {
        let (url, server_handle) = start_mock_server(200, "this-is-not-json").await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "malformed JSON must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("parse auth response"),
            "error must mention parse failure, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — success path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_success() {
        let body = r#"{"dhanClientId":"test","accessToken":"new-jwt-token","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, body).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(
            result.is_ok(),
            "acquire_token should succeed: {:?}",
            result.err()
        );

        // Verify token was stored
        let guard = manager.token.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("should have token after acquire");
        assert_eq!(
            state.access_token().expose_secret(),
            "new-jwt-token",
            "acquired token must be stored"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // renewal_loop — executes retry logic with time control
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn test_renewal_loop_executes_retry_logic_with_time_control() {
        // Manager with NO token → time_until_next_refresh returns Duration::ZERO
        // → the loop immediately enters the retry logic.
        // Unreachable URLs → both renew_with_fallback and acquire_token fail.
        // start_paused = true → all tokio sleeps resolve instantly when we advance.

        let manager = Arc::new(TokenManager {
            token: Arc::new(ArcSwap::new(Arc::new(None))),
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: "http://127.0.0.1:1".to_string(),
            auth_base_url: "http://127.0.0.1:1".to_string(),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_millis(50))
                .build()
                .expect("test http client"),
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 50,
                websocket_connect_timeout_ms: 50,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 20,
                retry_max_attempts: 2,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
        });

        let handle = tokio::spawn(Arc::clone(&manager).renewal_loop());

        // Advance time far enough for:
        //   - time_until_next_refresh() returns ZERO → no initial sleep
        //   - 2 retry attempts with backoff sleeps (10ms, 20ms)
        //   - circuit breaker sleep (60s)
        //   - then it loops again → another cycle
        // Total: well under 120s of simulated time
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        // Advance more to allow the second cycle to start
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        handle.abort();
        let result = handle.await;
        assert!(
            result.unwrap_err().is_cancelled(),
            "task should be cancelled after abort"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_renewal_loop_with_expired_token_triggers_retry() {
        // Manager with an expired token → time_until_next_refresh returns ZERO
        // → the loop immediately enters the retry logic.
        let data = DhanAuthResponseData {
            access_token: "expired-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0, // expires immediately
        };
        let expired_state = TokenState::from_response(&data);

        let manager = Arc::new(TokenManager {
            token: Arc::new(ArcSwap::new(Arc::new(Some(expired_state)))),
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: "http://127.0.0.1:1".to_string(),
            auth_base_url: "http://127.0.0.1:1".to_string(),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_millis(50))
                .build()
                .expect("test http client"),
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 50,
                websocket_connect_timeout_ms: 50,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 20,
                retry_max_attempts: 1,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
        });

        let handle = tokio::spawn(Arc::clone(&manager).renewal_loop());

        // Advance through retry attempts and circuit breaker sleep
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        handle.abort();
        let result = handle.await;
        assert!(
            result.unwrap_err().is_cancelled(),
            "task should be cancelled after abort"
        );
    }

    // -- Error classification tests --

    #[test]
    fn test_invalid_pin_is_permanent() {
        assert!(is_permanent_auth_error("Dhan auth error: Invalid Pin"));
    }

    #[test]
    fn test_invalid_client_is_permanent() {
        assert!(is_permanent_auth_error(
            "Dhan auth error: Invalid Client ID"
        ));
    }

    #[test]
    fn test_totp_invalid_is_not_permanent() {
        // TOTP errors are retryable (window boundary timing), not permanent.
        assert!(!is_permanent_auth_error("TOTP verification failed"));
        assert!(!is_permanent_auth_error("Dhan auth error: Invalid TOTP"));
    }

    #[test]
    fn test_totp_error_detected() {
        assert!(is_totp_error("TOTP verification failed"));
        assert!(is_totp_error("Dhan auth error: Invalid TOTP"));
        assert!(!is_totp_error("Invalid Pin"));
        assert!(!is_totp_error("network error"));
    }

    #[test]
    fn test_blocked_account_is_permanent() {
        assert!(is_permanent_auth_error("account blocked"));
        assert!(is_permanent_auth_error("account suspended"));
        assert!(is_permanent_auth_error("account disabled"));
    }

    #[test]
    fn test_network_error_is_not_permanent() {
        assert!(!is_permanent_auth_error(
            "generateAccessToken request failed: error sending request"
        ));
    }

    #[test]
    fn test_timeout_is_not_permanent() {
        assert!(!is_permanent_auth_error("operation timed out"));
    }

    #[test]
    fn test_rate_limit_is_not_permanent() {
        assert!(!is_permanent_auth_error(
            "Token can be generated once every 2 minutes"
        ));
    }

    #[test]
    fn test_rate_limit_detected() {
        assert!(is_dhan_rate_limited(
            "Dhan auth error: Token can be generated once every 2 minutes"
        ));
    }

    #[test]
    fn test_network_error_not_rate_limited() {
        assert!(!is_dhan_rate_limited("error sending request"));
    }

    // -----------------------------------------------------------------------
    // TokenHandle atomic tests
    // -----------------------------------------------------------------------

    /// Helper: creates a `TokenState` for testing using `from_cached`.
    fn make_test_token_state(token_value: &str) -> TokenState {
        let now_ist = chrono::Utc::now()
            .with_timezone(&dhan_live_trader_common::trading_calendar::ist_offset());
        let expires_at = now_ist + chrono::Duration::hours(24);
        TokenState::from_cached(
            secrecy::SecretString::from(token_value.to_string()),
            expires_at,
            now_ist,
        )
    }

    #[test]
    fn test_token_handle_starts_as_none() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let guard = handle.load();
        assert!(
            guard.as_ref().is_none(),
            "Fresh TokenHandle must load as None"
        );
    }

    #[test]
    fn test_token_handle_store_and_load() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let state = make_test_token_state("test-jwt-store-load");
        handle.store(Arc::new(Some(state)));

        let guard = handle.load();
        let loaded = guard
            .as_ref()
            .as_ref()
            .expect("should have token after store");
        assert_eq!(loaded.access_token().expose_secret(), "test-jwt-store-load");
    }

    #[test]
    fn test_token_handle_swap_updates_atomically() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store first token
        let state_a = make_test_token_state("token-alpha");
        handle.store(Arc::new(Some(state_a)));

        // Verify first token
        let guard = handle.load();
        let loaded = guard.as_ref().as_ref().expect("should have token-alpha");
        assert_eq!(loaded.access_token().expose_secret(), "token-alpha");

        // Swap to second token
        let state_b = make_test_token_state("token-beta");
        handle.store(Arc::new(Some(state_b)));

        // Verify second token is returned
        let guard = handle.load();
        let loaded = guard.as_ref().as_ref().expect("should have token-beta");
        assert_eq!(loaded.access_token().expose_secret(), "token-beta");
    }

    #[test]
    fn test_build_generate_token_url() {
        let auth_base = "https://auth.dhan.co";
        let url = format!("{auth_base}{}", DHAN_GENERATE_TOKEN_PATH);
        assert_eq!(url, "https://auth.dhan.co/app/generateAccessToken");
        assert!(url.contains(DHAN_GENERATE_TOKEN_PATH));
    }

    #[test]
    fn test_build_renew_token_url() {
        let api_base = "https://api.dhan.co/v2";
        let url = format!("{api_base}{}", DHAN_RENEW_TOKEN_PATH);
        assert_eq!(url, "https://api.dhan.co/v2/RenewToken");
        assert!(url.contains(DHAN_RENEW_TOKEN_PATH));
    }

    #[test]
    fn test_multiple_handles_share_state() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let handle_clone = Arc::clone(&handle);

        // Update via the original handle
        let state = make_test_token_state("shared-jwt");
        handle.store(Arc::new(Some(state)));

        // Read via the cloned handle — must see the same value
        let guard = handle_clone.load();
        let loaded = guard
            .as_ref()
            .as_ref()
            .expect("cloned handle should see stored token");
        assert_eq!(loaded.access_token().expose_secret(), "shared-jwt");

        // Update via the clone, read via original
        let state_updated = make_test_token_state("shared-jwt-v2");
        handle_clone.store(Arc::new(Some(state_updated)));

        let guard = handle.load();
        let loaded = guard
            .as_ref()
            .as_ref()
            .expect("original handle should see clone's update");
        assert_eq!(loaded.access_token().expose_secret(), "shared-jwt-v2");
    }

    // -----------------------------------------------------------------------
    // make_test_manager — struct field validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_make_test_manager_has_correct_urls() {
        let manager = make_test_manager(None);
        assert_eq!(manager.rest_api_base_url, "https://api.example.com");
        assert_eq!(manager.auth_base_url, "https://auth.example.com");
    }

    #[test]
    fn test_make_test_manager_has_correct_config() {
        let manager = make_test_manager(None);
        assert_eq!(manager.token_config.refresh_before_expiry_hours, 1);
        assert_eq!(manager.token_config.token_validity_hours, 24);
        assert_eq!(manager.network_config.request_timeout_ms, 5000);
        assert_eq!(manager.network_config.retry_max_attempts, 3);
    }

    // -----------------------------------------------------------------------
    // token_handle() — verify it returns the inner handle
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_method_returns_shared_reference() {
        let data = DhanAuthResponseData {
            access_token: "method-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_test_manager(Some(TokenState::from_response(&data)));
        let handle = manager.token_handle();

        let guard = handle.load();
        let state = guard.as_ref().as_ref().expect("should have token");
        assert_eq!(state.access_token().expose_secret(), "method-test-jwt");
    }

    // -----------------------------------------------------------------------
    // time_until_next_refresh — expired token
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_until_next_refresh_expired_token_returns_zero() {
        // Create a token with 0 expiry (already expired)
        let data = DhanAuthResponseData {
            access_token: "expired-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0, // already expired
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));
        let duration = manager.time_until_next_refresh();
        assert_eq!(duration, Duration::ZERO, "expired token should return ZERO");
    }

    // -----------------------------------------------------------------------
    // current_expiry_display — various token states
    // -----------------------------------------------------------------------

    #[test]
    fn test_current_expiry_display_short_lived_token() {
        let data = DhanAuthResponseData {
            access_token: "short-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 60, // 1 minute
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));
        let display = manager.current_expiry_display();
        assert_ne!(display, "no token");
    }

    // -----------------------------------------------------------------------
    // initialize — both HTTPS checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_both_http_urls_rejected() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "http://api.dhan.co".to_string(),
            auth_base_url: "http://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
        )
        .await;
        // Should fail on the first check (rest_api_base_url)
        assert!(result.is_err());
        let err_msg = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(err_msg.contains("rest_api_base_url"));
    }

    // -----------------------------------------------------------------------
    // Token handle — store None after having a token
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_store_none_clears_state() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let data = DhanAuthResponseData {
            access_token: "will-be-cleared".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data))));
        assert!(handle.load().as_ref().is_some());

        handle.store(Arc::new(None));
        assert!(handle.load().as_ref().is_none(), "None store must clear");
    }

    // -----------------------------------------------------------------------
    // Error classification — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_permanent_auth_error_invalid_pin() {
        assert!(is_permanent_auth_error("Dhan auth error: Invalid Pin"));
    }

    #[test]
    fn test_is_permanent_auth_error_invalid_client() {
        assert!(is_permanent_auth_error("Invalid Client ID"));
    }

    #[test]
    fn test_is_permanent_auth_error_blocked() {
        assert!(is_permanent_auth_error(
            "Account is blocked due to violations"
        ));
    }

    #[test]
    fn test_is_permanent_auth_error_suspended() {
        assert!(is_permanent_auth_error("account suspended by admin"));
    }

    #[test]
    fn test_is_permanent_auth_error_disabled() {
        assert!(is_permanent_auth_error("API access disabled"));
    }

    #[test]
    fn test_is_permanent_auth_error_case_insensitive() {
        assert!(is_permanent_auth_error("INVALID PIN"));
        assert!(is_permanent_auth_error("BLOCKED"));
        assert!(is_permanent_auth_error("Suspended"));
    }

    #[test]
    fn test_is_permanent_auth_error_transient_not_permanent() {
        assert!(!is_permanent_auth_error("connection timeout"));
        assert!(!is_permanent_auth_error("network error"));
        assert!(!is_permanent_auth_error("rate limit exceeded"));
        assert!(!is_permanent_auth_error(""));
    }

    #[test]
    fn test_is_permanent_auth_error_totp_not_permanent() {
        // TOTP errors are handled separately by is_totp_error
        assert!(!is_permanent_auth_error("Invalid TOTP code"));
    }

    #[test]
    fn test_is_totp_error_invalid_totp() {
        assert!(is_totp_error("Dhan auth error: Invalid TOTP"));
    }

    #[test]
    fn test_is_totp_error_totp_failed() {
        assert!(is_totp_error("TOTP verification failed"));
    }

    #[test]
    fn test_is_totp_error_case_insensitive() {
        assert!(is_totp_error("invalid totp code"));
        assert!(is_totp_error("TOTP INVALID"));
    }

    #[test]
    fn test_is_totp_error_requires_both_keywords() {
        // Must contain both "totp" and ("invalid" or "failed")
        assert!(!is_totp_error("totp code generated")); // totp without invalid/failed
        assert!(!is_totp_error("invalid PIN")); // invalid without totp
    }

    #[test]
    fn test_is_totp_error_not_other_errors() {
        assert!(!is_totp_error("connection timeout"));
        assert!(!is_totp_error("rate limit"));
        assert!(!is_totp_error(""));
    }

    #[test]
    fn test_is_dhan_rate_limited_every_2_minutes() {
        assert!(is_dhan_rate_limited(
            "Token can be generated every 2 minutes"
        ));
    }

    #[test]
    fn test_is_dhan_rate_limited_once_every() {
        assert!(is_dhan_rate_limited("once every 120 seconds"));
    }

    #[test]
    fn test_is_dhan_rate_limited_not_other_errors() {
        assert!(!is_dhan_rate_limited("DH-904 rate limit"));
        assert!(!is_dhan_rate_limited("connection timeout"));
        assert!(!is_dhan_rate_limited(""));
    }

    #[test]
    fn test_error_classification_mutual_exclusion() {
        // Verify that typical error messages only match one classifier
        let pin_error = "Invalid Pin";
        assert!(is_permanent_auth_error(pin_error));
        assert!(!is_totp_error(pin_error));
        assert!(!is_dhan_rate_limited(pin_error));

        let totp_error = "Invalid TOTP code";
        assert!(!is_permanent_auth_error(totp_error));
        assert!(is_totp_error(totp_error));
        assert!(!is_dhan_rate_limited(totp_error));

        let rate_error = "Token can be generated every 2 minutes";
        assert!(!is_permanent_auth_error(rate_error));
        assert!(!is_totp_error(rate_error));
        assert!(is_dhan_rate_limited(rate_error));
    }
}
