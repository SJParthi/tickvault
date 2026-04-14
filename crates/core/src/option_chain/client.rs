//! Dhan Option Chain REST API client with built-in rate limiting.
//!
//! Rate limit: 1 unique request every 3 seconds (Dhan-enforced).
//! Requires both `access-token` AND `client-id` headers.
//!
//! # Usage
//! ```ignore
//! let client = OptionChainClient::new(token_handle, client_id, config);
//! let expiries = client.fetch_expiry_list(13, "IDX_I").await?;
//! let chain = client.fetch_option_chain(13, "IDX_I", "2024-10-31").await?;
//! ```

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use secrecy::ExposeSecret;
use tracing::{debug, warn};

use tickvault_common::constants::{
    OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS, OPTION_CHAIN_REQUEST_TIMEOUT_SECS,
};

/// Maximum number of retry attempts for transient server errors (502, 500, 503).
const MAX_RETRIES: u32 = 3;

/// Initial retry delay in milliseconds (doubles each attempt: 1s, 2s, 4s).
const INITIAL_RETRY_DELAY_MS: u64 = 1000;

use crate::auth::TokenHandle;

use super::types::{
    ExpiryListRequest, ExpiryListResponse, OptionChainRequest, OptionChainResponse,
};

/// Option chain endpoint path.
/// Base URL already includes `/v2` (e.g., `https://api.dhan.co/v2`).
const OPTION_CHAIN_PATH: &str = "/optionchain";

/// Expiry list endpoint path.
/// Base URL already includes `/v2` (e.g., `https://api.dhan.co/v2`).
const EXPIRY_LIST_PATH: &str = "/optionchain/expirylist";

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Dhan Option Chain REST API client with automatic rate limiting.
///
/// Rate limits to 1 request per 3 seconds (Dhan's enforcement limit).
/// Uses the same `access-token` + `client-id` auth as other Dhan APIs.
pub struct OptionChainClient {
    http: Client,
    base_url: String,
    client_id: String,
    token_handle: TokenHandle,
    /// Tracks last request time for rate limiting.
    last_request_at: Option<Instant>,
}

impl OptionChainClient {
    /// Creates a new option chain client.
    ///
    /// # Arguments
    /// * `token_handle` — Arc-swap token for `access-token` header.
    /// * `client_id` — Dhan client ID for `client-id` header.
    /// * `base_url` — Dhan REST API base URL (e.g., `https://api.dhan.co`).
    pub fn new(token_handle: TokenHandle, client_id: String, base_url: String) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(OPTION_CHAIN_REQUEST_TIMEOUT_SECS))
            .build()
            .context("failed to build HTTP client for option chain")?;

        Ok(Self {
            http,
            base_url,
            client_id,
            token_handle,
            last_request_at: None,
        })
    }

    /// Fetches the list of active expiry dates for an underlying.
    ///
    /// Rate-limited: waits if less than 3 seconds since last request.
    /// Retries up to 3 times with exponential backoff on server errors (502/500/503).
    pub async fn fetch_expiry_list(
        &mut self,
        underlying_scrip: u64,
        underlying_seg: &str,
    ) -> Result<ExpiryListResponse> {
        self.enforce_rate_limit().await;

        let url = format!("{}{}", self.base_url, EXPIRY_LIST_PATH);
        let body = ExpiryListRequest {
            underlying_scrip,
            underlying_seg: underlying_seg.to_string(),
        };

        let mut delay_ms = INITIAL_RETRY_DELAY_MS;

        for attempt in 0..=MAX_RETRIES {
            let token = self.get_token()?;
            let send_result = self
                .http
                .post(&url)
                .header("access-token", token)
                .header("client-id", &self.client_id)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await;

            let response = match send_result {
                Ok(r) => r,
                Err(err) => {
                    if attempt < MAX_RETRIES {
                        warn!(
                            attempt = attempt + 1,
                            max = MAX_RETRIES,
                            delay_ms,
                            ?err,
                            "expiry list request failed — retrying"
                        );
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2);
                        continue;
                    }
                    return Err(err).context("expiry list request failed");
                }
            };

            let status = response.status();
            if status.is_success() {
                let resp: ExpiryListResponse = response
                    .json()
                    .await
                    .context("failed to deserialize expiry list response")?;
                debug!(
                    underlying_scrip,
                    expiry_count = resp.data.len(),
                    "expiry list fetched"
                );
                return Ok(resp);
            }

            // Retry on transient server errors (502, 500, 503).
            if is_retryable_status(status) && attempt < MAX_RETRIES {
                let resp_body = response.text().await.unwrap_or_default();
                warn!(
                    attempt = attempt + 1,
                    max = MAX_RETRIES,
                    delay_ms,
                    %status,
                    "expiry list API server error — retrying"
                );
                debug!(resp_body, "server error response body");
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = delay_ms.saturating_mul(2);
                continue;
            }

            // Non-retryable error or retries exhausted.
            let resp_body = response.text().await.unwrap_or_default();
            bail!("expiry list API returned {status}: {resp_body}");
        }

        bail!("expiry list request failed after {MAX_RETRIES} retries")
    }

    /// Fetches the full option chain for an underlying at a specific expiry.
    ///
    /// Rate-limited: waits if less than 3 seconds since last request.
    /// Retries up to 3 times with exponential backoff on server errors (502/500/503).
    pub async fn fetch_option_chain(
        &mut self,
        underlying_scrip: u64,
        underlying_seg: &str,
        expiry: &str,
    ) -> Result<OptionChainResponse> {
        self.enforce_rate_limit().await;

        let url = format!("{}{}", self.base_url, OPTION_CHAIN_PATH);
        let body = OptionChainRequest {
            underlying_scrip,
            underlying_seg: underlying_seg.to_string(),
            expiry: expiry.to_string(),
        };

        let mut delay_ms = INITIAL_RETRY_DELAY_MS;

        for attempt in 0..=MAX_RETRIES {
            let token = self.get_token()?;
            let send_result = self
                .http
                .post(&url)
                .header("access-token", token)
                .header("client-id", &self.client_id)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await;

            let response = match send_result {
                Ok(r) => r,
                Err(err) => {
                    if attempt < MAX_RETRIES {
                        warn!(
                            attempt = attempt + 1,
                            max = MAX_RETRIES,
                            delay_ms,
                            ?err,
                            "option chain request failed — retrying"
                        );
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2);
                        continue;
                    }
                    return Err(err).context("option chain request failed");
                }
            };

            let status = response.status();
            if status.is_success() {
                let resp: OptionChainResponse = response
                    .json()
                    .await
                    .context("failed to deserialize option chain response")?;
                debug!(
                    underlying_scrip,
                    expiry,
                    strikes = resp.data.oc.len(),
                    spot = resp.data.last_price,
                    "option chain fetched"
                );
                return Ok(resp);
            }

            // Retry on transient server errors (502, 500, 503).
            if is_retryable_status(status) && attempt < MAX_RETRIES {
                let resp_body = response.text().await.unwrap_or_default();
                warn!(
                    attempt = attempt + 1,
                    max = MAX_RETRIES,
                    delay_ms,
                    %status,
                    "option chain API server error — retrying"
                );
                debug!(resp_body, "server error response body");
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = delay_ms.saturating_mul(2);
                continue;
            }

            // Non-retryable error or retries exhausted.
            let resp_body = response.text().await.unwrap_or_default();
            bail!("option chain API returned {status}: {resp_body}");
        }

        bail!("option chain request failed after {MAX_RETRIES} retries")
    }

    /// Enforces the 3-second rate limit between requests.
    async fn enforce_rate_limit(&mut self) {
        if let Some(last) = self.last_request_at {
            let elapsed = last.elapsed();
            if elapsed < Duration::from_secs(OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS) {
                let wait = Duration::from_secs(OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS) - elapsed;
                debug!(
                    wait_ms = wait.as_millis(),
                    "rate-limiting option chain request"
                );
                tokio::time::sleep(wait).await;
            }
        }
        self.last_request_at = Some(Instant::now());
    }

    /// Extracts the current access token from the arc-swap handle.
    fn get_token(&self) -> Result<String> {
        let guard = self.token_handle.load();
        let state = guard
            .as_ref()
            .as_ref()
            .context("no token available for option chain API")?;
        Ok(state.access_token().expose_secret().to_string())
    }
}

/// Returns true for HTTP status codes that indicate transient server errors
/// worth retrying: 500 (Internal Server Error), 502 (Bad Gateway),
/// 503 (Service Unavailable).
///
/// Does NOT retry on 4xx (client errors) or DH-904 (rate limit — handled
/// separately with exponential backoff per dhan-api-introduction rule 8).
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 500 | 502 | 503)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_constant() {
        assert_eq!(OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS, 3);
    }

    #[test]
    fn test_endpoint_paths() {
        // Paths must NOT include /v2 — base_url already has it.
        assert_eq!(OPTION_CHAIN_PATH, "/optionchain");
        assert_eq!(EXPIRY_LIST_PATH, "/optionchain/expirylist");
    }

    #[test]
    fn test_full_url_no_double_v2() {
        // Base URL from config includes /v2.
        let base_url = "https://api.dhan.co/v2";
        let chain_url = format!("{}{}", base_url, OPTION_CHAIN_PATH);
        let expiry_url = format!("{}{}", base_url, EXPIRY_LIST_PATH);
        assert_eq!(chain_url, "https://api.dhan.co/v2/optionchain");
        assert_eq!(expiry_url, "https://api.dhan.co/v2/optionchain/expirylist");
        // Must NOT contain double /v2/v2.
        assert!(
            !chain_url.contains("/v2/v2"),
            "option chain URL has double /v2: {chain_url}"
        );
        assert!(
            !expiry_url.contains("/v2/v2"),
            "expiry list URL has double /v2: {expiry_url}"
        );
    }

    #[test]
    fn test_request_timeout() {
        assert_eq!(OPTION_CHAIN_REQUEST_TIMEOUT_SECS, 10);
    }

    #[test]
    fn test_retry_constants() {
        assert_eq!(MAX_RETRIES, 3);
        assert_eq!(INITIAL_RETRY_DELAY_MS, 1000);
    }

    #[test]
    fn test_is_retryable_status_server_errors() {
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
    }

    #[test]
    fn test_is_retryable_status_not_client_errors() {
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn test_is_retryable_status_not_success() {
        assert!(!is_retryable_status(reqwest::StatusCode::OK));
        assert!(!is_retryable_status(reqwest::StatusCode::CREATED));
    }

    #[test]
    fn test_exponential_backoff_sequence() {
        let mut delay = INITIAL_RETRY_DELAY_MS;
        assert_eq!(delay, 1000);
        delay = delay.saturating_mul(2);
        assert_eq!(delay, 2000);
        delay = delay.saturating_mul(2);
        assert_eq!(delay, 4000);
    }

    #[test]
    fn test_is_retryable_status_gateway_timeout_not_retried() {
        // 504 Gateway Timeout is NOT in the retry set — only 500/502/503
        assert!(!is_retryable_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn test_is_retryable_status_redirect_not_retried() {
        assert!(!is_retryable_status(reqwest::StatusCode::MOVED_PERMANENTLY));
        assert!(!is_retryable_status(reqwest::StatusCode::FOUND));
        assert!(!is_retryable_status(
            reqwest::StatusCode::TEMPORARY_REDIRECT
        ));
    }

    #[test]
    fn test_is_retryable_status_informational_not_retried() {
        assert!(!is_retryable_status(reqwest::StatusCode::CONTINUE));
    }

    #[test]
    fn test_is_retryable_status_conflict_not_retried() {
        assert!(!is_retryable_status(reqwest::StatusCode::CONFLICT));
    }

    #[test]
    fn test_is_retryable_status_unprocessable_entity_not_retried() {
        assert!(!is_retryable_status(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        ));
    }

    #[test]
    fn test_exponential_backoff_saturates_at_u64_max() {
        // Verify saturating_mul prevents overflow
        let mut delay = u64::MAX;
        delay = delay.saturating_mul(2);
        assert_eq!(delay, u64::MAX, "saturating_mul must not overflow");
    }

    #[test]
    fn test_exponential_backoff_full_sequence_matches_expected() {
        // After MAX_RETRIES iterations, the delay should be 1000 * 2^3 = 8000
        let mut delay = INITIAL_RETRY_DELAY_MS;
        for _ in 0..MAX_RETRIES {
            delay = delay.saturating_mul(2);
        }
        assert_eq!(delay, 8000, "1000 * 2^3 = 8000");
    }

    #[test]
    fn test_retry_constants_relationship() {
        // MAX_RETRIES * exponential backoff should not exceed a reasonable total wait
        let total_wait_ms: u64 = (0..MAX_RETRIES)
            .map(|i| INITIAL_RETRY_DELAY_MS.saturating_mul(1 << i))
            .sum();
        // 1000 + 2000 + 4000 = 7000ms = 7s total wait
        assert_eq!(total_wait_ms, 7000);
        assert!(
            total_wait_ms < 30_000,
            "total retry wait should be < 30 seconds"
        );
    }

    #[test]
    fn test_option_chain_path_no_trailing_slash() {
        assert!(!OPTION_CHAIN_PATH.ends_with('/'));
        assert!(!EXPIRY_LIST_PATH.ends_with('/'));
    }

    #[test]
    fn test_option_chain_path_starts_with_slash() {
        assert!(OPTION_CHAIN_PATH.starts_with('/'));
        assert!(EXPIRY_LIST_PATH.starts_with('/'));
    }

    #[test]
    fn test_expiry_list_path_is_subpath_of_option_chain() {
        assert!(
            EXPIRY_LIST_PATH.starts_with(OPTION_CHAIN_PATH),
            "expiry list should be under option chain path"
        );
    }

    #[test]
    fn test_is_retryable_status_all_5xx_codes() {
        // Only 500, 502, 503 are retryable. All other 5xx are NOT.
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )); // 500
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_IMPLEMENTED)); // 501
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY)); // 502
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        )); // 503
        assert!(!is_retryable_status(reqwest::StatusCode::GATEWAY_TIMEOUT)); // 504
        assert!(!is_retryable_status(
            reqwest::StatusCode::HTTP_VERSION_NOT_SUPPORTED
        )); // 505
    }

    #[test]
    fn test_is_retryable_status_accepted_not_retried() {
        assert!(!is_retryable_status(reqwest::StatusCode::ACCEPTED));
    }

    #[test]
    fn test_is_retryable_status_no_content_not_retried() {
        assert!(!is_retryable_status(reqwest::StatusCode::NO_CONTENT));
    }

    #[test]
    fn test_is_retryable_status_method_not_allowed_not_retried() {
        assert!(!is_retryable_status(
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        ));
    }

    #[test]
    fn test_is_retryable_status_gone_not_retried() {
        assert!(!is_retryable_status(reqwest::StatusCode::GONE));
    }

    #[test]
    fn test_exponential_backoff_from_large_initial() {
        // Start from a large value — saturating_mul prevents panic
        let mut delay = u64::MAX / 2 + 1;
        delay = delay.saturating_mul(2);
        assert_eq!(delay, u64::MAX, "large value * 2 saturates to MAX");
    }
}
