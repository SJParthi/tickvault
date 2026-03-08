//! Public IP verification against AWS SSM-stored static IP.
//!
//! Ensures the machine's outbound public IP (as seen by the internet)
//! matches the BSNL static IP stored in SSM and whitelisted with Dhan.
//!
//! # Boot Chain Position
//!
//! `Config → Observability → Logging → Notification → ★ IP Verification ★ → Auth → ...`
//!
//! Runs AFTER notification init (so Telegram alerts fire on failure)
//! and BEFORE auth (so no Dhan API call ever happens from a wrong IP).
//!
//! # Failure Modes — All Hard Blocks
//!
//! | Failure                         | Action                              |
//! |---------------------------------|-------------------------------------|
//! | SSM unreachable                 | Block boot, alert Telegram          |
//! | checkip + fallback both fail    | Block boot, alert Telegram          |
//! | IP mismatch                     | Block boot, alert Telegram (both)   |
//! | SSM parameter missing           | Block boot, alert Telegram          |
//! | IP format invalid               | Block boot, alert Telegram          |

use std::net::Ipv4Addr;
use std::time::Duration;

use secrecy::ExposeSecret;
use tracing::{error, info, instrument, warn};

use dhan_live_trader_common::constants::{
    PUBLIC_IP_CHECK_FALLBACK_URL, PUBLIC_IP_CHECK_MAX_RETRIES, PUBLIC_IP_CHECK_PRIMARY_URL,
    PUBLIC_IP_CHECK_TIMEOUT_SECS, SSM_NETWORK_SERVICE, STATIC_IP_SECRET,
};
use dhan_live_trader_common::error::ApplicationError;

use crate::auth::secret_manager::{
    build_ssm_path, create_ssm_client, fetch_secret, resolve_environment,
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result of a successful IP verification.
#[derive(Debug, Clone)]
pub struct IpVerificationResult {
    /// The verified public IP that matched SSM.
    pub verified_ip: String,
}

/// Verifies the machine's public IP matches the expected static IP from SSM.
///
/// 1. Fetches expected IP from SSM (`/dlt/<env>/network/static-ip`)
/// 2. Detects actual public IP via HTTPS (primary + fallback)
/// 3. Compares — match = Ok, mismatch = Err (hard block)
///
/// # Errors
///
/// Returns `ApplicationError::IpVerificationFailed` on any failure:
/// - SSM unreachable or parameter missing
/// - Public IP detection failed (both primary and fallback)
/// - IP mismatch
/// - Invalid IP format in SSM
#[instrument(skip_all, name = "ip_verification")]
pub async fn verify_public_ip() -> Result<IpVerificationResult, ApplicationError> {
    // Step 1: Fetch expected IP from SSM
    let expected_ip = fetch_expected_ip_from_ssm().await?;
    info!(expected_ip_masked = %mask_ip(&expected_ip), "expected static IP fetched from SSM");

    // Step 2: Validate expected IP format
    validate_ipv4_format(&expected_ip).map_err(|reason| {
        error!(reason = %reason, "SSM static IP is not a valid IPv4 address");
        ApplicationError::IpVerificationFailed { reason }
    })?;

    // Step 3: Detect actual public IP
    let actual_ip = detect_public_ip().await.map_err(|reason| {
        error!(reason = %reason, "public IP detection failed");
        ApplicationError::IpVerificationFailed { reason }
    })?;
    info!(actual_ip_masked = %mask_ip(&actual_ip), "actual public IP detected");

    // Step 4: Compare
    if actual_ip != expected_ip {
        let reason = format!(
            "IP MISMATCH — expected {} (SSM), got {} (actual). \
             Dhan will reject API calls from this IP. \
             Check: (1) correct network/ISP (2) SSM value is current (3) VPN not active",
            mask_ip(&expected_ip),
            mask_ip(&actual_ip),
        );
        error!(
            expected = %mask_ip(&expected_ip),
            actual = %mask_ip(&actual_ip),
            "public IP does not match SSM static IP — BLOCKING BOOT"
        );
        return Err(ApplicationError::IpVerificationFailed { reason });
    }

    info!(
        verified_ip = %mask_ip(&actual_ip),
        "public IP verification PASSED — matches SSM static IP"
    );

    Ok(IpVerificationResult {
        verified_ip: actual_ip,
    })
}

// ---------------------------------------------------------------------------
// SSM Fetch
// ---------------------------------------------------------------------------

/// Fetches the expected static IP from AWS SSM Parameter Store.
///
/// Path: `/dlt/<env>/network/static-ip`
async fn fetch_expected_ip_from_ssm() -> Result<String, ApplicationError> {
    let environment = resolve_environment()?;
    let ssm_client = create_ssm_client().await;

    let path = build_ssm_path(&environment, SSM_NETWORK_SERVICE, STATIC_IP_SECRET);
    info!(path = %path, "fetching expected static IP from SSM");

    let secret = fetch_secret(&ssm_client, &path).await?;
    let ip = secret.expose_secret().trim().to_string();

    if ip.is_empty() {
        return Err(ApplicationError::IpVerificationFailed {
            reason: format!("SSM parameter '{path}' exists but is empty"),
        });
    }

    Ok(ip)
}

// ---------------------------------------------------------------------------
// Public IP Detection
// ---------------------------------------------------------------------------

/// Detects the machine's public IP via external HTTPS services.
///
/// Tries primary (checkip.amazonaws.com) first with retries,
/// then falls back to api.ipify.org with retries.
///
/// Both services return the IP as plain text — no JSON parsing needed.
async fn detect_public_ip() -> Result<String, String> {
    let timeout = Duration::from_secs(PUBLIC_IP_CHECK_TIMEOUT_SECS);

    // Try primary with retries
    for attempt in 1..=PUBLIC_IP_CHECK_MAX_RETRIES {
        match fetch_ip_from_url(PUBLIC_IP_CHECK_PRIMARY_URL, timeout).await {
            Ok(ip) => return Ok(ip),
            Err(err) => {
                warn!(
                    url = PUBLIC_IP_CHECK_PRIMARY_URL,
                    attempt,
                    max_retries = PUBLIC_IP_CHECK_MAX_RETRIES,
                    error = %err,
                    "primary IP check failed"
                );
                if attempt < PUBLIC_IP_CHECK_MAX_RETRIES {
                    // Exponential backoff: 1s, 2s, 4s
                    let delay = Duration::from_secs(1 << (attempt - 1));
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    info!("primary IP check exhausted — trying fallback");

    // Try fallback with retries
    for attempt in 1..=PUBLIC_IP_CHECK_MAX_RETRIES {
        match fetch_ip_from_url(PUBLIC_IP_CHECK_FALLBACK_URL, timeout).await {
            Ok(ip) => return Ok(ip),
            Err(err) => {
                warn!(
                    url = PUBLIC_IP_CHECK_FALLBACK_URL,
                    attempt,
                    max_retries = PUBLIC_IP_CHECK_MAX_RETRIES,
                    error = %err,
                    "fallback IP check failed"
                );
                if attempt < PUBLIC_IP_CHECK_MAX_RETRIES {
                    let delay = Duration::from_secs(1 << (attempt - 1));
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    Err(format!(
        "all IP detection attempts exhausted — primary ({}) and fallback ({}) both failed after {} retries each",
        PUBLIC_IP_CHECK_PRIMARY_URL, PUBLIC_IP_CHECK_FALLBACK_URL, PUBLIC_IP_CHECK_MAX_RETRIES
    ))
}

/// Fetches the public IP from a single URL.
///
/// Expects a plain-text response containing only the IP address.
async fn fetch_ip_from_url(url: &str, timeout: Duration) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| format!("HTTP client build failed: {err}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("HTTP request to {url} failed: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {} from {url}", response.status()));
    }

    let body = response
        .text()
        .await
        .map_err(|err| format!("failed to read response body from {url}: {err}"))?;

    let ip = body.trim().to_string();

    // Validate it looks like an IP address
    validate_ipv4_format(&ip).map_err(|reason| {
        format!("response from {url} is not a valid IPv4 address: '{ip}' — {reason}")
    })?;

    Ok(ip)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validates that a string is a valid IPv4 address.
fn validate_ipv4_format(ip: &str) -> Result<(), String> {
    ip.parse::<Ipv4Addr>()
        .map(|_| ())
        .map_err(|err| format!("invalid IPv4 address '{ip}': {err}"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Masks an IP address for safe logging.
///
/// `"203.0.113.42"` → `"203.0.XXX.XX"` — preserves first two octets for
/// debugging network issues while hiding the full address from logs.
fn mask_ip(ip: &str) -> String {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.XXX.XX", parts[0], parts[1])
    } else {
        "XXX.XXX.XXX.XXX".to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // validate_ipv4_format
    // -----------------------------------------------------------------------

    #[test]
    fn test_valid_ipv4_passes() {
        assert!(validate_ipv4_format("192.168.1.1").is_ok());
        assert!(validate_ipv4_format("10.0.0.1").is_ok());
        assert!(validate_ipv4_format("203.0.113.42").is_ok());
        assert!(validate_ipv4_format("255.255.255.255").is_ok());
        assert!(validate_ipv4_format("0.0.0.0").is_ok());
    }

    #[test]
    fn test_invalid_ipv4_fails() {
        assert!(validate_ipv4_format("").is_err());
        assert!(validate_ipv4_format("not-an-ip").is_err());
        assert!(validate_ipv4_format("256.0.0.1").is_err());
        assert!(validate_ipv4_format("192.168.1").is_err());
        assert!(validate_ipv4_format("192.168.1.1.1").is_err());
        assert!(validate_ipv4_format("::1").is_err()); // IPv6 not supported
        assert!(validate_ipv4_format("192.168.1.1:8080").is_err()); // port
    }

    #[test]
    fn test_ipv4_with_whitespace_fails() {
        assert!(validate_ipv4_format(" 192.168.1.1").is_err());
        assert!(validate_ipv4_format("192.168.1.1 ").is_err());
        assert!(validate_ipv4_format(" 192.168.1.1 ").is_err());
    }

    #[test]
    fn test_ipv4_with_newline_fails() {
        assert!(validate_ipv4_format("192.168.1.1\n").is_err());
    }

    // -----------------------------------------------------------------------
    // mask_ip
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_ip_standard() {
        assert_eq!(mask_ip("203.0.113.42"), "203.0.XXX.XX");
    }

    #[test]
    fn test_mask_ip_private() {
        assert_eq!(mask_ip("192.168.1.100"), "192.168.XXX.XX");
    }

    #[test]
    fn test_mask_ip_invalid_format() {
        assert_eq!(mask_ip("not-an-ip"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_empty() {
        assert_eq!(mask_ip(""), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_partial() {
        assert_eq!(mask_ip("10.0.0"), "XXX.XXX.XXX.XXX");
    }

    // -----------------------------------------------------------------------
    // SSM path construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_ssm_path_for_static_ip_dev() {
        let path = build_ssm_path("dev", SSM_NETWORK_SERVICE, STATIC_IP_SECRET);
        assert_eq!(path, "/dlt/dev/network/static-ip");
    }

    #[test]
    fn test_ssm_path_for_static_ip_prod() {
        let path = build_ssm_path("prod", SSM_NETWORK_SERVICE, STATIC_IP_SECRET);
        assert_eq!(path, "/dlt/prod/network/static-ip");
    }

    // -----------------------------------------------------------------------
    // fetch_expected_ip_from_ssm — error path (no real SSM)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_expected_ip_fails_without_real_ssm() {
        let result = fetch_expected_ip_from_ssm().await;
        assert!(result.is_err(), "must fail without real SSM connectivity");
    }

    // -----------------------------------------------------------------------
    // verify_public_ip — error path (no real SSM)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_verify_public_ip_fails_without_real_ssm() {
        let result = verify_public_ip().await;
        assert!(
            result.is_err(),
            "verify_public_ip must fail without real SSM"
        );
    }

    // -----------------------------------------------------------------------
    // Constants verification
    // -----------------------------------------------------------------------

    #[test]
    fn test_primary_url_is_https() {
        assert!(
            PUBLIC_IP_CHECK_PRIMARY_URL.starts_with("https://"),
            "primary IP check URL must use HTTPS"
        );
    }

    #[test]
    fn test_fallback_url_is_https() {
        assert!(
            PUBLIC_IP_CHECK_FALLBACK_URL.starts_with("https://"),
            "fallback IP check URL must use HTTPS"
        );
    }

    #[allow(clippy::assertions_on_constants)] // APPROVED: compile-time constant validation
    #[test]
    fn test_timeout_is_reasonable() {
        assert!(
            PUBLIC_IP_CHECK_TIMEOUT_SECS >= 5 && PUBLIC_IP_CHECK_TIMEOUT_SECS <= 30,
            "IP check timeout should be 5-30s, got {}",
            PUBLIC_IP_CHECK_TIMEOUT_SECS
        );
    }

    #[allow(clippy::assertions_on_constants)] // APPROVED: compile-time constant validation
    #[test]
    fn test_max_retries_is_reasonable() {
        assert!(
            PUBLIC_IP_CHECK_MAX_RETRIES >= 1 && PUBLIC_IP_CHECK_MAX_RETRIES <= 5,
            "max retries should be 1-5, got {}",
            PUBLIC_IP_CHECK_MAX_RETRIES
        );
    }

    // -----------------------------------------------------------------------
    // IpVerificationResult
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_verification_result_debug() {
        let result = IpVerificationResult {
            verified_ip: "203.0.113.42".to_string(),
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("203.0.113.42"));
    }

    #[test]
    fn test_ip_verification_result_clone() {
        let result = IpVerificationResult {
            verified_ip: "10.0.0.1".to_string(),
        };
        let cloned = result.clone();
        assert_eq!(cloned.verified_ip, "10.0.0.1");
    }
}
