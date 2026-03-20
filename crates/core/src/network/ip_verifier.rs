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
                    let delay = Duration::from_secs(
                        1_u64.checked_shl(attempt.saturating_sub(1)).unwrap_or(1),
                    );
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
                    let delay = Duration::from_secs(
                        1_u64.checked_shl(attempt.saturating_sub(1)).unwrap_or(1),
                    );
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
// Dhan Static IP API Methods
// Source: docs/dhan-ref/02-authentication.md Section 2.4
// ---------------------------------------------------------------------------

/// Sets a static IP for Dhan Order APIs.
///
/// Endpoint: `POST /v2/ip/setIP`
/// Dhan requires static IP whitelisting for all Order API endpoints.
#[instrument(skip_all, name = "set_ip")]
// TEST-EXEMPT: requires live Dhan API; request/response types tested in auth::types::tests
pub async fn set_ip(
    rest_api_base_url: &str,
    access_token: &str,
    request: &crate::auth::types::SetIpRequest,
) -> Result<crate::auth::types::SetIpResponse, ApplicationError> {
    let url = format!(
        "{}{}",
        rest_api_base_url,
        dhan_live_trader_common::constants::DHAN_SET_IP_PATH
    );

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .header("access-token", access_token)
        .header("Content-Type", "application/json")
        .json(request)
        .send()
        .await
        .map_err(|err| ApplicationError::IpVerificationFailed {
            reason: format!("set_ip request failed: {err}"),
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(ApplicationError::IpVerificationFailed {
            reason: format!("set_ip HTTP {status}: {body}"),
        });
    }

    serde_json::from_str(&body).map_err(|err| ApplicationError::IpVerificationFailed {
        reason: format!("set_ip response parse error: {err}"),
    })
}

/// Modifies the whitelisted static IP for Dhan Order APIs.
///
/// Endpoint: `PUT /v2/ip/modifyIP`
/// WARNING: 7-day cooldown after modification. Do NOT call during live trading.
#[instrument(skip_all, name = "modify_ip")]
// TEST-EXEMPT: requires live Dhan API; request/response types tested in auth::types::tests
pub async fn modify_ip(
    rest_api_base_url: &str,
    access_token: &str,
    request: &crate::auth::types::ModifyIpRequest,
) -> Result<crate::auth::types::ModifyIpResponse, ApplicationError> {
    let url = format!(
        "{}{}",
        rest_api_base_url,
        dhan_live_trader_common::constants::DHAN_MODIFY_IP_PATH
    );

    warn!("modifying Dhan IP — 7-day cooldown applies after this operation");

    let client = reqwest::Client::new();
    let response = client
        .put(&url)
        .header("access-token", access_token)
        .header("Content-Type", "application/json")
        .json(request)
        .send()
        .await
        .map_err(|err| ApplicationError::IpVerificationFailed {
            reason: format!("modify_ip request failed: {err}"),
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(ApplicationError::IpVerificationFailed {
            reason: format!("modify_ip HTTP {status}: {body}"),
        });
    }

    serde_json::from_str(&body).map_err(|err| ApplicationError::IpVerificationFailed {
        reason: format!("modify_ip response parse error: {err}"),
    })
}

/// Gets the current static IP configuration from Dhan.
///
/// Endpoint: `GET /v2/ip/getIP`
#[instrument(skip_all, name = "get_ip")]
// TEST-EXEMPT: requires live Dhan API; response type tested in auth::types::tests
pub async fn get_ip(
    rest_api_base_url: &str,
    access_token: &str,
) -> Result<crate::auth::types::GetIpResponse, ApplicationError> {
    let url = format!(
        "{}{}",
        rest_api_base_url,
        dhan_live_trader_common::constants::DHAN_GET_IP_PATH
    );

    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .header("access-token", access_token)
        .send()
        .await
        .map_err(|err| ApplicationError::IpVerificationFailed {
            reason: format!("get_ip request failed: {err}"),
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(ApplicationError::IpVerificationFailed {
            reason: format!("get_ip HTTP {status}: {body}"),
        });
    }

    serde_json::from_str(&body).map_err(|err| ApplicationError::IpVerificationFailed {
        reason: format!("get_ip response parse error: {err}"),
    })
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
        if crate::test_support::has_aws_credentials() {
            // Dev machine with real AWS credentials — SSM is reachable.
            assert!(
                result.is_ok(),
                "with real AWS credentials, SSM IP fetch should succeed"
            );
        } else {
            assert!(result.is_err(), "must fail without real SSM connectivity");
        }
    }

    // -----------------------------------------------------------------------
    // verify_public_ip — error path (no real SSM)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_verify_public_ip_fails_without_real_ssm() {
        let result = verify_public_ip().await;
        if crate::test_support::has_aws_credentials() {
            // Dev machine with real AWS credentials — SSM + public IP check both succeed.
            assert!(
                result.is_ok(),
                "with real AWS credentials, IP verification should succeed"
            );
        } else {
            assert!(
                result.is_err(),
                "verify_public_ip must fail without real SSM"
            );
        }
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

    // -----------------------------------------------------------------------
    // validate_ipv4_format — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_whitespace_only_ip_fails() {
        assert!(validate_ipv4_format("   ").is_err());
        assert!(validate_ipv4_format("\t").is_err());
        assert!(validate_ipv4_format("\n\n").is_err());
    }

    #[test]
    fn test_validate_ipv4_error_contains_ip() {
        let err = validate_ipv4_format("garbage").unwrap_err();
        assert!(
            err.contains("garbage"),
            "error should contain the invalid IP"
        );
    }

    #[test]
    fn test_validate_ipv4_leading_zeros_rejected() {
        // Leading zeros in octets: Rust's Ipv4Addr::parse rejects them (since Rust 1.76)
        assert!(validate_ipv4_format("192.168.001.001").is_err());
    }

    // -----------------------------------------------------------------------
    // mask_ip — additional cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_ip_loopback() {
        assert_eq!(mask_ip("127.0.0.1"), "127.0.XXX.XX");
    }

    #[test]
    fn test_mask_ip_five_octets() {
        assert_eq!(mask_ip("1.2.3.4.5"), "XXX.XXX.XXX.XXX");
    }

    // -----------------------------------------------------------------------
    // fetch_ip_from_url — error path coverage (no real network needed)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_ip_from_url_invalid_url() {
        let result = fetch_ip_from_url("not-a-url", Duration::from_millis(100)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_ip_from_url_connection_refused() {
        // Port 1 is unlikely to be listening
        let result = fetch_ip_from_url("http://127.0.0.1:1/ip", Duration::from_millis(200)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("127.0.0.1:1"), "error should mention the URL");
    }

    #[tokio::test]
    async fn test_fetch_ip_from_url_non_success_status() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let result = fetch_ip_from_url(
            &format!("http://127.0.0.1:{port}/ip"),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("HTTP 500"));
    }

    #[tokio::test]
    async fn test_fetch_ip_from_url_invalid_ip_in_response() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "not-an-ip-address";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let result = fetch_ip_from_url(
            &format!("http://127.0.0.1:{port}/ip"),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("not a valid IPv4"), "error: {err}");
    }

    // -----------------------------------------------------------------------
    // Dhan IP API — request/response format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_set_ip_request_format() {
        use crate::auth::types::SetIpRequest;

        let req = SetIpRequest {
            dhan_client_id: "1000000001".to_string(),
            ip: "203.0.113.42".to_string(),
            ip_flag: "PRIMARY".to_string(),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("dhanClientId"));
        assert!(json.contains("203.0.113.42"));
        assert!(json.contains("PRIMARY"));
    }

    #[test]
    fn test_get_ip_response_parsing() {
        use crate::auth::types::GetIpResponse;

        let json = r#"{
            "ip": "203.0.113.42",
            "ipFlag": "PRIMARY",
            "modifyDatePrimary": "2026-01-15",
            "modifyDateSecondary": ""
        }"#;

        let response: GetIpResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.ip, "203.0.113.42");
        assert_eq!(response.ip_flag, "PRIMARY");
        assert_eq!(response.modify_date_primary, "2026-01-15");
        assert!(response.modify_date_secondary.is_empty());
    }
}
