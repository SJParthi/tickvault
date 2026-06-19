//! Groww access-token authentication (native Rust, TOTP flow).
//!
//! Obtains a Groww access token from `api_key` + a rotating 6-digit TOTP code,
//! mirroring the Dhan auth pattern (`crate::auth`). This is PR-2 of the Groww
//! second-feed sequence — the prerequisite for the NATS live feed (later PRs).
//!
//! Wire contract (see `docs/groww-ref/01-introduction-auth.md` +
//! `docs/groww-ref/README.md` "verified wire protocol"):
//!
//! ```text
//! POST https://api.groww.in/v1/token/api/access
//!   Authorization: Bearer <api_key>
//!   x-api-version: 1.0
//!   Content-Type: application/json
//!   body: {"key_type":"totp","totp":"<6-digit>"}
//!   -> { "token": "<access_token>" }
//! ```
//!
//! The response field name is read from the Groww SDK; we accept `token` plus
//! `access_token`/`accessToken` aliases defensively. The access token is NEVER
//! logged (wrapped in `SecretString`); a non-2xx error body IS captured
//! (truncated, redacted) for operator triage. Default OFF — only invoked when
//! `feeds.groww_enabled`.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::{info, instrument};

use tickvault_common::error::ApplicationError;
use tickvault_common::sanitize::capture_rest_error_body;

use crate::auth::secret_manager::fetch_groww_credentials;
use crate::auth::totp_generator::generate_totp_code;
use crate::auth::types::GrowwCredentials;

/// Groww access-token endpoint (full URL — single hardcoded const, no joining).
pub const GROWW_TOKEN_ENDPOINT_URL: &str = "https://api.groww.in/v1/token/api/access";

/// Groww API version header name (required on the access-token request).
pub const GROWW_API_VERSION_HEADER: &str = "x-api-version";

/// Groww API version header value.
pub const GROWW_API_VERSION_VALUE: &str = "1.0";

/// `key_type` for the TOTP auth flow (vs the daily-approval `"approval"` flow).
pub const GROWW_KEY_TYPE_TOTP: &str = "totp";

/// Request body for the Groww access-token endpoint (TOTP flow).
#[derive(Debug, Serialize)]
struct AccessTokenRequest<'a> {
    key_type: &'a str,
    totp: &'a str,
}

/// Response body for the Groww access-token endpoint. `token` is the documented
/// field; the camelCase `accessToken` alias is accepted defensively (Groww's
/// JSON is camelCase throughout — `tsInMillis`, `growwOrderId`, etc.).
#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    #[serde(alias = "accessToken")]
    token: String,
}

/// Parses the Groww access-token response body into a `SecretString`.
///
/// Pure + offline-testable. On a missing/empty token or malformed JSON returns
/// `ApplicationError::AuthenticationFailed`. The body is NOT echoed into the
/// error (a 2xx body carries the secret token).
fn parse_access_token_response(body: &str) -> Result<SecretString, ApplicationError> {
    let parsed: AccessTokenResponse =
        serde_json::from_str(body).map_err(|err| ApplicationError::AuthenticationFailed {
            reason: format!("Groww access-token response was not valid JSON: {err}"),
        })?;

    if parsed.token.trim().is_empty() {
        return Err(ApplicationError::AuthenticationFailed {
            reason: "Groww access-token response contained an empty token".to_string(),
        });
    }

    Ok(SecretString::from(parsed.token))
}

/// Obtains a Groww access token from `api_key` + a fresh TOTP code.
///
/// Generates the 6-digit code (SHA1/6/30 — same as Dhan), POSTs the access-token
/// request, and parses the token. The token is returned as a `SecretString` and
/// is never logged.
///
/// # Errors
///
/// `ApplicationError::AuthenticationFailed` on TOTP failure, network error,
/// non-2xx status (with the redacted/truncated error body for triage), or an
/// unparseable/empty token.
#[instrument(skip_all)]
// TEST-EXEMPT: network call to the Groww access-token endpoint; the pure request body + response parsing are unit-tested (test_access_token_request_serializes_totp_flow + parse_access_token_response tests).
pub async fn obtain_groww_access_token(
    http: &reqwest::Client,
    creds: &GrowwCredentials,
) -> Result<SecretString, ApplicationError> {
    let totp_code = generate_totp_code(&creds.totp_secret)?;

    let request_body = serde_json::to_string(&AccessTokenRequest {
        key_type: GROWW_KEY_TYPE_TOTP,
        totp: &totp_code,
    })
    .map_err(|err| ApplicationError::AuthenticationFailed {
        reason: format!("failed to serialize Groww access-token request: {err}"),
    })?;

    let response = http
        .post(GROWW_TOKEN_ENDPOINT_URL)
        .bearer_auth(creds.api_key.expose_secret())
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(request_body)
        .send()
        .await
        .map_err(|err| ApplicationError::AuthenticationFailed {
            reason: format!("Groww access-token request failed to send: {err}"),
        })?;

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        // Error body is safe to capture (no token on a failure) — redact +
        // truncate per the same helper the Dhan REST canary uses.
        return Err(ApplicationError::AuthenticationFailed {
            reason: format!(
                "Groww access-token request returned HTTP {}: {}",
                status.as_u16(),
                capture_rest_error_body(&body_text)
            ),
        });
    }

    parse_access_token_response(&body_text)
}

/// Boot-time Groww auth smoke-check: fetch credentials from SSM, obtain an
/// access token, and confirm the credential chain works. Invoked from boot ONLY
/// when `feeds.groww_enabled` is true (the live feed connector lands in a later
/// PR; this verifies auth end-to-end first so the operator gets a clear signal).
///
/// # Errors
///
/// Propagates `ApplicationError` from SSM fetch, HTTP client build, or the
/// access-token request.
#[instrument(skip_all)]
// TEST-EXEMPT: boot orchestration (SSM fetch + HTTP client build + live network token request); no live SSM/Groww endpoint in unit tests.
pub async fn run_groww_auth_smoke_check(request_timeout_ms: u64) -> Result<(), ApplicationError> {
    let creds = fetch_groww_credentials().await?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(request_timeout_ms))
        .build()
        .map_err(|err| ApplicationError::AuthenticationFailed {
            reason: format!("Groww HTTP client creation failed: {err}"),
        })?;

    let _token = obtain_groww_access_token(&http, &creds).await?;

    info!(
        "Groww access-token auth OK — credentials valid; the Groww live-feed \
         connector lands in a later PR of the second-feed sequence"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_access_token_url_and_version_are_groww_v1() {
        assert_eq!(
            GROWW_TOKEN_ENDPOINT_URL,
            "https://api.groww.in/v1/token/api/access"
        );
        assert_eq!(GROWW_API_VERSION_HEADER, "x-api-version");
        assert_eq!(GROWW_API_VERSION_VALUE, "1.0");
        assert_eq!(GROWW_KEY_TYPE_TOTP, "totp");
    }

    #[test]
    fn test_access_token_request_serializes_totp_flow() {
        let body = serde_json::to_string(&AccessTokenRequest {
            key_type: GROWW_KEY_TYPE_TOTP,
            totp: "123456",
        })
        .expect("serialize");
        assert_eq!(body, r#"{"key_type":"totp","totp":"123456"}"#);
    }

    #[test]
    fn test_parse_access_token_response_extracts_token() {
        let token = parse_access_token_response(r#"{"token":"abc.def.ghi"}"#).expect("ok");
        assert_eq!(token.expose_secret(), "abc.def.ghi");
    }

    #[test]
    fn test_parse_access_token_response_accepts_camel_alias() {
        // Groww JSON is camelCase throughout, so `accessToken` is the defensive
        // alias for the documented `token` field.
        assert_eq!(
            parse_access_token_response(r#"{"accessToken":"camel"}"#)
                .expect("camel alias")
                .expose_secret(),
            "camel"
        );
    }

    #[test]
    fn test_parse_access_token_response_missing_token_errors() {
        let err = parse_access_token_response(r#"{"foo":1}"#).unwrap_err();
        assert!(matches!(err, ApplicationError::AuthenticationFailed { .. }));
    }

    #[test]
    fn test_parse_access_token_response_empty_token_errors() {
        let err = parse_access_token_response(r#"{"token":"   "}"#).unwrap_err();
        match err {
            ApplicationError::AuthenticationFailed { reason } => {
                assert!(reason.contains("empty token"), "reason: {reason}");
            }
            other => panic!("expected AuthenticationFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_access_token_response_rejects_garbage() {
        assert!(parse_access_token_response("not json at all").is_err());
        assert!(parse_access_token_response("").is_err());
    }
}
