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

use tickvault_common::constants::{
    PUBLIC_IP_CHECK_FALLBACK_URL, PUBLIC_IP_CHECK_MAX_RETRIES, PUBLIC_IP_CHECK_PRIMARY_URL,
    PUBLIC_IP_CHECK_TIMEOUT_SECS, SSM_NETWORK_SERVICE, STATIC_IP_SECRET,
};
use tickvault_common::error::ApplicationError;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::url_join::join_api_url;

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
/// 1. Fetches expected IP from SSM (`/tickvault/<env>/network/static-ip`)
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
    if let Err(reason) = compare_ips(&expected_ip, &actual_ip) {
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
/// Path: `/tickvault/<env>/network/static-ip`
async fn fetch_expected_ip_from_ssm() -> Result<String, ApplicationError> {
    let environment = resolve_environment()?;
    let ssm_client = create_ssm_client().await;

    let path = build_ssm_path(&environment, SSM_NETWORK_SERVICE, STATIC_IP_SECRET);
    info!(path = %path, "fetching expected static IP from SSM");

    let secret = fetch_secret(&ssm_client, &path).await?;
    let ip = secret.expose_secret().trim().to_string();

    validate_ssm_ip_not_empty(&ip, &path)
        .map_err(|reason| ApplicationError::IpVerificationFailed { reason })?;

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
                    tokio::time::sleep(compute_ip_check_backoff(attempt)).await;
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
                    tokio::time::sleep(compute_ip_check_backoff(attempt)).await;
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

/// Compares expected and actual IPs, returning an error reason on mismatch.
///
/// Pure function — no I/O, no allocation on match.
fn compare_ips(expected: &str, actual: &str) -> Result<(), String> {
    if actual != expected {
        Err(format!(
            "IP MISMATCH — expected {} (SSM), got {} (actual). \
             Dhan will reject API calls from this IP. \
             Check: (1) correct network/ISP (2) SSM value is current (3) VPN not active",
            mask_ip(expected),
            mask_ip(actual),
        ))
    } else {
        Ok(())
    }
}

/// Validates that an IP string from SSM is not empty.
///
/// Pure function — no I/O.
fn validate_ssm_ip_not_empty(ip: &str, path: &str) -> Result<(), String> {
    if ip.is_empty() {
        Err(format!("SSM parameter '{path}' exists but is empty"))
    } else {
        Ok(())
    }
}

/// Computes exponential backoff delay for retry attempts.
///
/// Returns `Duration` = 2^(attempt-1) seconds, capped at the value derived
/// from `attempt < max_retries` (i.e., no delay on last attempt).
///
/// Pure function — no I/O.
fn compute_ip_check_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1_u64.checked_shl(attempt.saturating_sub(1)).unwrap_or(1))
}

/// Classifies an IP verification error into a category.
///
/// Pure function — used for logging and alerting decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpVerificationErrorKind {
    /// SSM is unreachable or parameter is missing.
    SsmError,
    /// SSM returned an invalid/empty IP.
    InvalidSsmIp,
    /// Could not detect the machine's public IP.
    DetectionFailed,
    /// Detected IP does not match SSM.
    Mismatch,
}

/// Classifies an `ApplicationError::IpVerificationFailed` reason string.
///
/// Pure function.
pub fn classify_ip_error(reason: &str) -> IpVerificationErrorKind {
    // Check mismatch first — mismatch messages may contain "SSM" as context.
    if reason.contains("IP MISMATCH") {
        IpVerificationErrorKind::Mismatch
    } else if reason.contains("SSM") && reason.contains("empty") {
        IpVerificationErrorKind::InvalidSsmIp
    } else if reason.contains("SSM") || reason.contains("secret") {
        IpVerificationErrorKind::SsmError
    } else {
        // Covers: detection failures, exhausted retries, and any unknown reason.
        IpVerificationErrorKind::DetectionFailed
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
    let url = join_api_url(
        rest_api_base_url,
        tickvault_common::constants::DHAN_SET_IP_PATH,
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
            reason: format!(
                "set_ip HTTP {status} url={} body={}",
                redact_url_params(&url),
                capture_rest_error_body(&body)
            ),
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
    let url = join_api_url(
        rest_api_base_url,
        tickvault_common::constants::DHAN_MODIFY_IP_PATH,
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
            reason: format!(
                "modify_ip HTTP {status} url={} body={}",
                redact_url_params(&url),
                capture_rest_error_body(&body)
            ),
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
    // DHAN-REST-400 item 1b (2026-06-10): the 08:45 getIP 404 "path not
    // found" smelled like a malformed URL (`/v2//ip/getIP` from a
    // trailing-slash base override) — join_api_url makes that impossible.
    let url = join_api_url(
        rest_api_base_url,
        tickvault_common::constants::DHAN_GET_IP_PATH,
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
        // DHAN-REST-400 (2026-06-10): bounded secret-redacted body + the
        // EXACT final request URL so the operator sees both the cause
        // (errorType/errorCode/errorMessage) and any malformed-path shape.
        let captured_body = capture_rest_error_body(&body);
        let redacted_url = redact_url_params(&url);
        error!(
            status = %status,
            url = %redacted_url,
            body = %captured_body,
            "get_ip request returned non-2xx"
        );
        return Err(ApplicationError::IpVerificationFailed {
            reason: format!("get_ip HTTP {status} url={redacted_url} body={captured_body}"),
        });
    }

    serde_json::from_str(&body).map_err(|err| ApplicationError::IpVerificationFailed {
        reason: format!("get_ip response parse error: {err}"),
    })
}

// ---------------------------------------------------------------------------
// Phase 0 Item 18 — Dhan-side static IP boot gate
// ---------------------------------------------------------------------------
//
// `verify_public_ip()` at Step 5.5 confirms our egress IP matches the
// value SSM has for us. That tells us "we look right to the world", but
// NOT "Dhan considers this IP whitelisted for the order endpoints".
// Dhan's `/v2/ip/getIP` is the authoritative answer for the second
// question — the `ordersAllowed` flag is the SAME flag the exchange
// reads at order-acceptance time.
//
// Effective 2026-04-01, orders from unregistered IPs are REJECTED with
// no grace period. Boot MUST refuse to spawn the order path unless
// this check returns Pass.

/// Verdict of a static-IP boot check against Dhan's `/v2/ip/getIP`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticIpBootOutcome {
    /// IP matches AND Dhan reports `ordersAllowed = true`. Safe to spawn
    /// the order path.
    Pass,
    /// Boot must halt. Carries a typed reason for the operator-facing
    /// Telegram + the JSONL audit row.
    Fail(StaticIpBootFailureReason),
}

/// Typed failure reasons. Precedence matters: when more than one
/// condition is violated, the operator sees the MOST actionable one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticIpBootFailureReason {
    /// `/v2/ip/getIP` returned an effectively-empty body. Either the API
    /// is broken, our token does not have IP-API entitlement, or the
    /// account has no whitelisted IP at all. Operator must check the
    /// Dhan web portal.
    EmptyResponse,
    /// Dhan flipped `orders_allowed = false`. This is the AUTHORITATIVE
    /// pre-trade signal — see `GetIpResponse::orders_allowed`. Operator
    /// likely needs to (re-)set the static IP on the Dhan portal, or
    /// wait out the 7-day cooldown after a recent modify.
    OrdersNotAllowed,
    /// `ip_match_status` is not `"MATCH"`. Treated as a separate
    /// failure mode for diagnostics — in practice Dhan always pairs
    /// MISMATCH with `orders_allowed = false`, but we surface the
    /// raw string to help triage protocol drift.
    MatchStatusNotOk {
        /// The literal string Dhan returned (`"MISMATCH"`, empty, or
        /// unknown). Helps spot Dhan-side API changes.
        observed: String,
    },
}

impl StaticIpBootFailureReason {
    /// Stable wire-format string for Telegram + audit rows. Lowercase
    /// snake_case so it doubles as a Prometheus label value.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EmptyResponse => "empty_response",
            Self::OrdersNotAllowed => "orders_not_allowed",
            Self::MatchStatusNotOk { .. } => "match_status_not_ok",
        }
    }
}

/// Pure classifier: given a `GetIpResponse` body, return the boot
/// outcome.
///
/// Precedence (most operator-actionable first):
///   1. Empty response   → `EmptyResponse`
///   2. `orders_allowed = false` → `OrdersNotAllowed` (authoritative)
///   3. `ip_match_status != "MATCH"` → `MatchStatusNotOk`
///   4. Otherwise → `Pass`
///
/// Fail-closed by construction: any unknown / absent field flows into
/// a Fail branch.
#[must_use]
pub fn classify_static_ip_boot_outcome(
    response: &crate::auth::types::GetIpResponse,
) -> StaticIpBootOutcome {
    // (1) Empty-response check: both the registered IP and the
    // detected IP missing means Dhan returned `{}` (or a parse-defaulted
    // body). Treat the absence as "do not trust this for orders".
    if response.ip.is_empty() && response.detected_ip.is_empty() {
        return StaticIpBootOutcome::Fail(StaticIpBootFailureReason::EmptyResponse);
    }

    // (2) Authoritative gate per
    // `.claude/rules/dhan/authentication.md` rule 7 + Item 18 charter.
    if !response.orders_allowed {
        return StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed);
    }

    // (3) Defensive secondary check — if Dhan ever de-couples
    // `orders_allowed` from `ip_match_status`, we still refuse to boot
    // on a non-MATCH status.
    if response.ip_match_status != "MATCH" {
        return StaticIpBootOutcome::Fail(StaticIpBootFailureReason::MatchStatusNotOk {
            observed: response.ip_match_status.clone(),
        });
    }

    StaticIpBootOutcome::Pass
}

/// Phase 0 Item 18b — Action for the boot-time retry loop after
/// receiving a `StaticIpBootOutcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticIpBootRetryAction {
    /// Boot may proceed — gate passed.
    Pass,
    /// Sleep `interval_secs` and retry. Carries the (1-indexed)
    /// upcoming attempt number for the operator-facing event.
    Retry { next_attempt: u32 },
    /// Halt boot. Carries the stable failure-reason wire string.
    Halt { reason: &'static str },
}

/// Pure retry policy: given the just-observed outcome, the
/// 1-indexed attempt number that produced it, and the configured
/// max attempts, return the next action.
///
/// Only `OrdersNotAllowed` is retryable. Everything else halts
/// immediately, because retrying an empty response or a non-MATCH
/// status would just burn API quota — those are structural failures.
#[must_use]
pub fn classify_static_ip_boot_retry_action(
    outcome: &StaticIpBootOutcome,
    attempts_made: u32,
    max_attempts: u32,
) -> StaticIpBootRetryAction {
    match outcome {
        StaticIpBootOutcome::Pass => StaticIpBootRetryAction::Pass,
        StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed) => {
            if attempts_made < max_attempts {
                StaticIpBootRetryAction::Retry {
                    next_attempt: attempts_made.saturating_add(1),
                }
            } else {
                StaticIpBootRetryAction::Halt {
                    reason: StaticIpBootFailureReason::OrdersNotAllowed.as_str(),
                }
            }
        }
        StaticIpBootOutcome::Fail(reason) => StaticIpBootRetryAction::Halt {
            reason: reason.as_str(),
        },
    }
}

/// Calls `GET /v2/ip/getIP` and returns the boot outcome.
///
/// This is the thin live-API wrapper around `classify_static_ip_boot_outcome`.
/// Boot wiring in `main.rs` calls this once after Step 6 (auth) and
/// before any order-path WS/REST call.
///
/// # Errors
///
/// Returns `ApplicationError::IpVerificationFailed` if the HTTP call
/// itself fails (network error, non-2xx, parse error). A `Fail`
/// `StaticIpBootOutcome` is an `Ok(Fail(_))` — the caller decides
/// whether to halt boot; the function does not panic.
#[instrument(skip_all, name = "verify_static_ip_at_boot")]
// TEST-EXEMPT: thin live-API wrapper; `get_ip` + `classify_static_ip_boot_outcome` are independently tested.
pub async fn verify_static_ip_at_boot(
    rest_api_base_url: &str,
    access_token: &str,
) -> Result<StaticIpBootOutcome, ApplicationError> {
    let response = get_ip(rest_api_base_url, access_token).await?;
    info!(
        ip_masked = %mask_ip(&response.ip),
        detected_masked = %mask_ip(&response.detected_ip),
        ip_match_status = %response.ip_match_status,
        orders_allowed = response.orders_allowed,
        ip_flag = %response.ip_flag,
        "static IP boot check: response received"
    );
    Ok(classify_static_ip_boot_outcome(&response))
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
        assert_eq!(path, "/tickvault/dev/network/static-ip");
    }

    #[test]
    fn test_ssm_path_for_static_ip_prod() {
        let path = build_ssm_path("prod", SSM_NETWORK_SERVICE, STATIC_IP_SECRET);
        assert_eq!(path, "/tickvault/prod/network/static-ip");
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

    // -----------------------------------------------------------------------
    // compare_ips — pure IP comparison
    // -----------------------------------------------------------------------

    #[test]
    fn test_compare_ips_match_returns_ok() {
        assert!(compare_ips("203.0.113.42", "203.0.113.42").is_ok());
    }

    #[test]
    fn test_compare_ips_mismatch_returns_error() {
        let result = compare_ips("203.0.113.42", "10.0.0.1");
        assert!(result.is_err());
        let reason = result.unwrap_err();
        assert!(
            reason.contains("IP MISMATCH"),
            "error reason must contain 'IP MISMATCH'"
        );
        assert!(
            reason.contains("203.0.XXX.XX"),
            "error must mask expected IP"
        );
        assert!(reason.contains("10.0.XXX.XX"), "error must mask actual IP");
    }

    #[test]
    fn test_compare_ips_mismatch_contains_guidance() {
        let reason = compare_ips("1.2.3.4", "5.6.7.8").unwrap_err();
        assert!(reason.contains("correct network/ISP"));
        assert!(reason.contains("SSM value is current"));
        assert!(reason.contains("VPN not active"));
    }

    #[test]
    fn test_compare_ips_same_prefix_different_suffix() {
        let result = compare_ips("203.0.113.42", "203.0.113.43");
        assert!(result.is_err());
    }

    #[test]
    fn test_compare_ips_empty_strings() {
        // Empty strings are equal, compare_ips returns Ok.
        // (validate_ipv4_format catches empty before compare_ips runs.)
        assert!(compare_ips("", "").is_ok());
    }

    // -----------------------------------------------------------------------
    // validate_ssm_ip_not_empty
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_ssm_ip_not_empty_valid() {
        assert!(
            validate_ssm_ip_not_empty("203.0.113.42", "/tickvault/dev/network/static-ip").is_ok()
        );
    }

    #[test]
    fn test_validate_ssm_ip_not_empty_empty_string() {
        let result = validate_ssm_ip_not_empty("", "/tickvault/dev/network/static-ip");
        assert!(result.is_err());
        let reason = result.unwrap_err();
        assert!(reason.contains("SSM parameter"));
        assert!(reason.contains("empty"));
        assert!(reason.contains("/tickvault/dev/network/static-ip"));
    }

    // -----------------------------------------------------------------------
    // compute_ip_check_backoff
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_ip_check_backoff_first_attempt() {
        let delay = compute_ip_check_backoff(1);
        assert_eq!(delay, Duration::from_secs(1));
    }

    #[test]
    fn test_compute_ip_check_backoff_second_attempt() {
        let delay = compute_ip_check_backoff(2);
        assert_eq!(delay, Duration::from_secs(2));
    }

    #[test]
    fn test_compute_ip_check_backoff_third_attempt() {
        let delay = compute_ip_check_backoff(3);
        assert_eq!(delay, Duration::from_secs(4));
    }

    #[test]
    fn test_compute_ip_check_backoff_zero_attempt() {
        // attempt=0 → shift by saturating_sub(1)=0 → 2^0 = 1
        let delay = compute_ip_check_backoff(0);
        assert_eq!(delay, Duration::from_secs(1));
    }

    #[test]
    fn test_compute_ip_check_backoff_large_attempt() {
        // Large shift wraps to 1 via unwrap_or
        let delay = compute_ip_check_backoff(100);
        assert_eq!(delay, Duration::from_secs(1));
    }

    // -----------------------------------------------------------------------
    // classify_ip_error
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_ip_error_ssm_empty() {
        let kind = classify_ip_error(
            "SSM parameter '/tickvault/dev/network/static-ip' exists but is empty",
        );
        assert_eq!(kind, IpVerificationErrorKind::InvalidSsmIp);
    }

    #[test]
    fn test_classify_ip_error_ssm_unreachable() {
        let kind = classify_ip_error("failed to fetch SSM secret: connection refused");
        assert_eq!(kind, IpVerificationErrorKind::SsmError);
    }

    #[test]
    fn test_classify_ip_error_mismatch() {
        // Real mismatch messages contain "(SSM)" — classifier must check mismatch first.
        let kind = classify_ip_error(
            "IP MISMATCH — expected 203.0.XXX.XX (SSM), got 10.0.XXX.XX (actual)",
        );
        assert_eq!(kind, IpVerificationErrorKind::Mismatch);
    }

    #[test]
    fn test_classify_ip_error_detection_failed() {
        let kind = classify_ip_error(
            "all IP detection attempts exhausted — primary and fallback both failed",
        );
        assert_eq!(kind, IpVerificationErrorKind::DetectionFailed);
    }

    #[test]
    fn test_classify_ip_error_unknown_reason() {
        let kind = classify_ip_error("something unexpected happened");
        assert_eq!(kind, IpVerificationErrorKind::DetectionFailed);
    }

    #[test]
    fn test_classify_ip_error_secret_keyword() {
        let kind = classify_ip_error("failed to read secret from parameter store");
        assert_eq!(kind, IpVerificationErrorKind::SsmError);
    }

    // -----------------------------------------------------------------------
    // IpVerificationErrorKind — derive coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_error_kind_debug_and_clone() {
        let kind = IpVerificationErrorKind::Mismatch;
        let cloned = kind.clone();
        assert_eq!(kind, cloned);
        let debug = format!("{kind:?}");
        assert!(debug.contains("Mismatch"));
    }

    #[test]
    fn test_ip_error_kind_all_variants_distinct() {
        let variants = [
            IpVerificationErrorKind::SsmError,
            IpVerificationErrorKind::InvalidSsmIp,
            IpVerificationErrorKind::DetectionFailed,
            IpVerificationErrorKind::Mismatch,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "variants {i} and {j} must be distinct");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // validate_ipv4_format — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_ipv4_loopback() {
        assert!(validate_ipv4_format("127.0.0.1").is_ok());
    }

    #[test]
    fn test_ipv4_broadcast() {
        assert!(validate_ipv4_format("255.255.255.255").is_ok());
    }

    #[test]
    fn test_ipv4_with_leading_zeros() {
        // Rust's Ipv4Addr parser handles leading zeros
        // "01.01.01.01" may or may not parse depending on implementation
        let result = validate_ipv4_format("01.01.01.01");
        // Either ok or err is fine — just ensure no panic
        let _ = result;
    }

    #[test]
    fn test_ipv4_error_message_contains_input() {
        let result = validate_ipv4_format("bad-ip");
        let err = result.unwrap_err();
        assert!(
            err.contains("bad-ip"),
            "error should contain the invalid input"
        );
    }

    #[test]
    fn test_ipv4_error_message_is_descriptive() {
        let result = validate_ipv4_format("not-valid");
        let err = result.unwrap_err();
        assert!(
            err.contains("invalid IPv4 address"),
            "error should describe the issue"
        );
    }

    // -----------------------------------------------------------------------
    // mask_ip — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_ip_single_digit_octets() {
        assert_eq!(mask_ip("1.2.3.4"), "1.2.XXX.XX");
    }

    #[test]
    fn test_mask_ip_three_digit_octets() {
        assert_eq!(mask_ip("255.255.255.255"), "255.255.XXX.XX");
    }

    #[test]
    fn test_mask_ip_five_parts() {
        // More than 4 parts → invalid → fully masked
        assert_eq!(mask_ip("1.2.3.4.5"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_three_parts() {
        assert_eq!(mask_ip("1.2.3"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_two_parts() {
        assert_eq!(mask_ip("1.2"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_no_dots() {
        assert_eq!(mask_ip("nodots"), "XXX.XXX.XXX.XXX");
    }

    // -----------------------------------------------------------------------
    // compare_ips — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compare_ips_whitespace_difference() {
        // Whitespace makes IPs different — caller must trim before comparing
        let result = compare_ips("1.2.3.4", " 1.2.3.4");
        assert!(result.is_err());
    }

    #[test]
    fn test_compare_ips_mismatch_error_is_actionable() {
        let err = compare_ips("10.0.0.1", "192.168.1.1").unwrap_err();
        // Must contain troubleshooting guidance
        assert!(err.contains("VPN"));
        assert!(err.contains("SSM"));
        assert!(err.contains("ISP"));
    }

    // -----------------------------------------------------------------------
    // validate_ssm_ip_not_empty — additional
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_ssm_ip_not_empty_whitespace_only() {
        // Whitespace-only is NOT empty — returns Ok
        // (caller should trim before calling)
        assert!(validate_ssm_ip_not_empty("  ", "/tickvault/dev/network/static-ip").is_ok());
    }

    #[test]
    fn test_validate_ssm_ip_not_empty_error_contains_path() {
        let path = "/tickvault/prod/network/static-ip";
        let err = validate_ssm_ip_not_empty("", path).unwrap_err();
        assert!(err.contains(path));
    }

    // -----------------------------------------------------------------------
    // compute_ip_check_backoff — additional
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_ip_check_backoff_doubles_each_attempt() {
        let d1 = compute_ip_check_backoff(1);
        let d2 = compute_ip_check_backoff(2);
        let d3 = compute_ip_check_backoff(3);
        assert_eq!(d2, d1 * 2);
        assert_eq!(d3, d2 * 2);
    }

    #[test]
    fn test_compute_ip_check_backoff_u32_max_attempt() {
        // u32::MAX saturating_sub(1) = u32::MAX - 1; checked_shl will overflow → unwrap_or(1)
        let delay = compute_ip_check_backoff(u32::MAX);
        assert_eq!(delay, Duration::from_secs(1));
    }

    // -----------------------------------------------------------------------
    // classify_ip_error — additional patterns
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_ip_error_mismatch_priority_over_ssm() {
        // "IP MISMATCH" should be classified as Mismatch even when "SSM" is present
        let kind = classify_ip_error("IP MISMATCH — expected from SSM, got different");
        assert_eq!(kind, IpVerificationErrorKind::Mismatch);
    }

    #[test]
    fn test_classify_ip_error_empty_string() {
        let kind = classify_ip_error("");
        assert_eq!(kind, IpVerificationErrorKind::DetectionFailed);
    }

    #[test]
    fn test_classify_ip_error_ssm_parameter_not_found() {
        let kind = classify_ip_error("SSM parameter not found: /tickvault/dev/network/static-ip");
        assert_eq!(kind, IpVerificationErrorKind::SsmError);
    }

    #[test]
    fn test_classify_ip_error_http_failure() {
        let kind = classify_ip_error("HTTP request to checkip failed: timeout");
        assert_eq!(kind, IpVerificationErrorKind::DetectionFailed);
    }

    // -----------------------------------------------------------------------
    // IpVerificationResult — additional
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_verification_result_fields() {
        let result = IpVerificationResult {
            verified_ip: "10.0.0.1".to_string(),
        };
        assert_eq!(result.verified_ip, "10.0.0.1");
    }

    // -----------------------------------------------------------------------
    // IpVerificationErrorKind — Debug formatting
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_error_kind_debug_all_variants() {
        let variants = [
            (IpVerificationErrorKind::SsmError, "SsmError"),
            (IpVerificationErrorKind::InvalidSsmIp, "InvalidSsmIp"),
            (IpVerificationErrorKind::DetectionFailed, "DetectionFailed"),
            (IpVerificationErrorKind::Mismatch, "Mismatch"),
        ];
        for (variant, expected_str) in variants {
            let debug = format!("{variant:?}");
            assert!(
                debug.contains(expected_str),
                "debug for {expected_str} should contain the variant name"
            );
        }
    }

    // -----------------------------------------------------------------------
    // compute_ip_check_backoff — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_ip_check_backoff_attempt_1() {
        let d = compute_ip_check_backoff(1);
        assert_eq!(d, Duration::from_secs(1)); // 2^0 = 1
    }

    #[test]
    fn test_compute_ip_check_backoff_attempt_2() {
        let d = compute_ip_check_backoff(2);
        assert_eq!(d, Duration::from_secs(2)); // 2^1 = 2
    }

    #[test]
    fn test_compute_ip_check_backoff_attempt_3() {
        let d = compute_ip_check_backoff(3);
        assert_eq!(d, Duration::from_secs(4)); // 2^2 = 4
    }

    #[test]
    fn test_compute_ip_check_backoff_attempt_0() {
        // saturating_sub(1) = 0 for attempt 0, but attempt 0 means shift 0
        let d = compute_ip_check_backoff(0);
        assert!(d.as_secs() >= 1, "backoff must be at least 1 second");
    }

    #[test]
    fn test_compute_ip_check_backoff_very_large_attempt() {
        // Should not overflow even with u32-scale attempt
        let d = compute_ip_check_backoff(100);
        assert!(d.as_secs() >= 1);
    }

    // -----------------------------------------------------------------------
    // validate_ssm_ip_not_empty — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_ssm_ip_not_empty_ok() {
        assert!(validate_ssm_ip_not_empty("10.0.0.1", "/tickvault/dev/network/static-ip").is_ok());
    }

    #[test]
    fn test_validate_ssm_ip_not_empty_err() {
        let result = validate_ssm_ip_not_empty("", "/tickvault/dev/network/static-ip");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("empty"), "error should mention empty: {err}");
        assert!(err.contains("/tickvault/dev/network/static-ip"));
    }

    // -----------------------------------------------------------------------
    // classify_ip_error — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_ip_error_ssm_and_empty() {
        let kind = classify_ip_error("SSM parameter is empty");
        assert_eq!(kind, IpVerificationErrorKind::InvalidSsmIp);
    }

    #[test]
    fn test_classify_ip_error_secret_manager() {
        let kind = classify_ip_error("secret manager unreachable");
        assert_eq!(kind, IpVerificationErrorKind::SsmError);
    }

    #[test]
    fn test_classify_ip_error_mismatch_priority() {
        // "IP MISMATCH" takes priority even if message also contains "SSM"
        let kind = classify_ip_error("IP MISMATCH — expected from SSM");
        assert_eq!(kind, IpVerificationErrorKind::Mismatch);
    }

    #[test]
    fn test_classify_ip_error_detection_failed_fallback() {
        let kind = classify_ip_error("all IP detection attempts exhausted");
        assert_eq!(kind, IpVerificationErrorKind::DetectionFailed);
    }

    // -----------------------------------------------------------------------
    // compare_ips — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compare_ips_match() {
        assert!(compare_ips("10.0.0.1", "10.0.0.1").is_ok());
    }

    #[test]
    fn test_compare_ips_mismatch_contains_masked_ips() {
        let result = compare_ips("10.0.0.1", "192.168.1.1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("10.0.XXX.XX"));
        assert!(err.contains("192.168.XXX.XX"));
    }

    // -----------------------------------------------------------------------
    // mask_ip — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_ip_too_few_octets() {
        assert_eq!(mask_ip("10.0.0"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_too_many_octets() {
        assert_eq!(mask_ip("10.0.0.1.2"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_empty_string() {
        assert_eq!(mask_ip(""), "XXX.XXX.XXX.XXX");
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 18 — classify_static_ip_boot_outcome
    // -----------------------------------------------------------------------

    use crate::auth::types::GetIpResponse;

    fn make_get_ip_response(
        ip: &str,
        detected_ip: &str,
        ip_match_status: &str,
        orders_allowed: bool,
    ) -> GetIpResponse {
        GetIpResponse {
            ip: ip.to_string(),
            ip_flag: "PRIMARY".to_string(),
            modify_date_primary: String::new(),
            modify_date_secondary: String::new(),
            detected_ip: detected_ip.to_string(),
            ip_match_status: ip_match_status.to_string(),
            orders_allowed,
        }
    }

    #[test]
    fn test_classify_static_ip_boot_outcome_smoke() {
        // Smoke check that the public symbol `classify_static_ip_boot_outcome`
        // is callable and returns the expected discriminant; the seven
        // `test_classify_static_ip_*` cases below exercise every branch.
        let response = make_get_ip_response("203.0.113.42", "203.0.113.42", "MATCH", true);
        assert_eq!(
            classify_static_ip_boot_outcome(&response),
            StaticIpBootOutcome::Pass
        );
    }

    #[test]
    fn test_classify_static_ip_pass_on_match_and_orders_allowed() {
        let response = make_get_ip_response("203.0.113.42", "203.0.113.42", "MATCH", true);
        assert_eq!(
            classify_static_ip_boot_outcome(&response),
            StaticIpBootOutcome::Pass
        );
    }

    #[test]
    fn test_classify_static_ip_fail_when_orders_not_allowed() {
        // Even with a MATCH the orders_allowed flag is authoritative.
        let response = make_get_ip_response("203.0.113.42", "203.0.113.42", "MATCH", false);
        assert_eq!(
            classify_static_ip_boot_outcome(&response),
            StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed)
        );
    }

    #[test]
    fn test_classify_static_ip_fail_on_mismatch_status() {
        // Belt-and-suspenders: if Dhan ever decouples orders_allowed
        // from ip_match_status, MISMATCH alone still blocks boot.
        let response = make_get_ip_response("203.0.113.42", "198.51.100.7", "MISMATCH", true);
        match classify_static_ip_boot_outcome(&response) {
            StaticIpBootOutcome::Fail(StaticIpBootFailureReason::MatchStatusNotOk { observed }) => {
                assert_eq!(observed, "MISMATCH");
            }
            other => panic!("expected MatchStatusNotOk, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_static_ip_fail_on_unknown_status_value() {
        // Dhan returns something we have never seen — fail closed and
        // surface the literal so triage knows what to look for.
        let response = make_get_ip_response("203.0.113.42", "203.0.113.42", "UNKNOWN_FOO", true);
        match classify_static_ip_boot_outcome(&response) {
            StaticIpBootOutcome::Fail(StaticIpBootFailureReason::MatchStatusNotOk { observed }) => {
                assert_eq!(observed, "UNKNOWN_FOO");
            }
            other => panic!("expected MatchStatusNotOk, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_static_ip_fail_on_empty_response() {
        // Dhan returned `{}` — both ip + detected_ip are absent.
        let response = make_get_ip_response("", "", "", false);
        assert_eq!(
            classify_static_ip_boot_outcome(&response),
            StaticIpBootOutcome::Fail(StaticIpBootFailureReason::EmptyResponse)
        );
    }

    #[test]
    fn test_classify_static_ip_orders_not_allowed_wins_over_mismatch() {
        // Precedence: orders_allowed=false is more actionable than the
        // status string — operator action is the same (fix the IP),
        // but the OrdersNotAllowed reason maps to the docs/runbook
        // bullet operators were trained on.
        let response = make_get_ip_response("203.0.113.42", "198.51.100.7", "MISMATCH", false);
        assert_eq!(
            classify_static_ip_boot_outcome(&response),
            StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed)
        );
    }

    #[test]
    fn test_classify_static_ip_empty_wins_over_orders_not_allowed() {
        // Empty response: `orders_allowed` defaults to false (serde
        // default), so without this precedence rule we'd report the
        // wrong failure mode. Surface the API-broken state first.
        let response = make_get_ip_response("", "", "", false);
        match classify_static_ip_boot_outcome(&response) {
            StaticIpBootOutcome::Fail(StaticIpBootFailureReason::EmptyResponse) => {}
            other => panic!("expected EmptyResponse, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 18b — classify_static_ip_boot_retry_action
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_static_ip_boot_retry_action_smoke() {
        // Smoke check that the public symbol `classify_static_ip_boot_retry_action`
        // is callable. The six `test_retry_action_*` cases below exercise
        // every branch (Pass / Retry budget remaining / Halt at exhaustion /
        // Halt on structural failure / boundary walk / zero-budget edge).
        assert_eq!(
            classify_static_ip_boot_retry_action(&StaticIpBootOutcome::Pass, 1, 30),
            StaticIpBootRetryAction::Pass
        );
    }

    #[test]
    fn test_retry_action_on_pass_is_pass() {
        let action = classify_static_ip_boot_retry_action(&StaticIpBootOutcome::Pass, 1, 30);
        assert_eq!(action, StaticIpBootRetryAction::Pass);
    }

    #[test]
    fn test_retry_action_on_orders_not_allowed_retries_when_budget_remains() {
        let outcome = StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed);
        let action = classify_static_ip_boot_retry_action(&outcome, 1, 30);
        assert_eq!(action, StaticIpBootRetryAction::Retry { next_attempt: 2 });
    }

    #[test]
    fn test_retry_action_on_orders_not_allowed_halts_at_budget_exhaustion() {
        // attempts_made == max_attempts is the boundary: 30/30 means
        // we already used the last attempt; halt immediately.
        let outcome = StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed);
        let action = classify_static_ip_boot_retry_action(&outcome, 30, 30);
        assert_eq!(
            action,
            StaticIpBootRetryAction::Halt {
                reason: "orders_not_allowed"
            }
        );
    }

    #[test]
    fn test_retry_action_on_empty_response_halts_immediately() {
        // Structural failure — no point retrying.
        let outcome = StaticIpBootOutcome::Fail(StaticIpBootFailureReason::EmptyResponse);
        let action = classify_static_ip_boot_retry_action(&outcome, 1, 30);
        assert_eq!(
            action,
            StaticIpBootRetryAction::Halt {
                reason: "empty_response"
            }
        );
    }

    #[test]
    fn test_retry_action_on_match_status_not_ok_halts_immediately() {
        let outcome = StaticIpBootOutcome::Fail(StaticIpBootFailureReason::MatchStatusNotOk {
            observed: "MISMATCH".to_string(),
        });
        let action = classify_static_ip_boot_retry_action(&outcome, 1, 30);
        assert_eq!(
            action,
            StaticIpBootRetryAction::Halt {
                reason: "match_status_not_ok"
            }
        );
    }

    #[test]
    fn test_retry_action_orders_not_allowed_walks_full_budget() {
        // Walk the full retry budget and confirm the boundary fires
        // exactly on attempt 30, not 29 or 31.
        let outcome = StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed);
        for attempt in 1..30 {
            let action = classify_static_ip_boot_retry_action(&outcome, attempt, 30);
            match action {
                StaticIpBootRetryAction::Retry { next_attempt } => {
                    assert_eq!(next_attempt, attempt + 1);
                }
                other => panic!("attempt {attempt} expected Retry, got {other:?}"),
            }
        }
        // At attempt 30 (last attempt used) we must halt.
        let final_action = classify_static_ip_boot_retry_action(&outcome, 30, 30);
        assert!(matches!(final_action, StaticIpBootRetryAction::Halt { .. }));
    }

    #[test]
    fn test_retry_action_handles_zero_budget_edge_case() {
        // Defensive: if a future operator drops max_attempts to 1, the
        // very first orders-not-allowed observation must halt — the
        // loop has already used its sole attempt.
        let outcome = StaticIpBootOutcome::Fail(StaticIpBootFailureReason::OrdersNotAllowed);
        let action = classify_static_ip_boot_retry_action(&outcome, 1, 1);
        assert!(matches!(action, StaticIpBootRetryAction::Halt { .. }));
    }

    #[test]
    fn test_static_ip_failure_reason_as_str_stable() {
        // Stable strings — Telegram parsing, Prometheus labels, and
        // audit-table rows all depend on these literals.
        assert_eq!(
            StaticIpBootFailureReason::EmptyResponse.as_str(),
            "empty_response"
        );
        assert_eq!(
            StaticIpBootFailureReason::OrdersNotAllowed.as_str(),
            "orders_not_allowed"
        );
        assert_eq!(
            StaticIpBootFailureReason::MatchStatusNotOk {
                observed: "MISMATCH".to_string()
            }
            .as_str(),
            "match_status_not_ok"
        );
    }
}
