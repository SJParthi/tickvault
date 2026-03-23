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
use tracing::debug;

use dhan_live_trader_common::constants::{
    OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS, OPTION_CHAIN_REQUEST_TIMEOUT_SECS,
};

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

        let token = self.get_token()?;
        let response = self
            .http
            .post(&url)
            .header("access-token", token)
            .header("client-id", &self.client_id)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("expiry list request failed")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("expiry list API returned {status}: {body}");
        }

        let resp: ExpiryListResponse = response
            .json()
            .await
            .context("failed to deserialize expiry list response")?;

        debug!(
            underlying_scrip,
            expiry_count = resp.data.len(),
            "expiry list fetched"
        );

        Ok(resp)
    }

    /// Fetches the full option chain for an underlying at a specific expiry.
    ///
    /// Rate-limited: waits if less than 3 seconds since last request.
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

        let token = self.get_token()?;
        let response = self
            .http
            .post(&url)
            .header("access-token", token)
            .header("client-id", &self.client_id)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("option chain request failed")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("option chain API returned {status}: {body}");
        }

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

        Ok(resp)
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
}
