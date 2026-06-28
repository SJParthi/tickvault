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
// APPROVED: fixed Groww auth endpoint — the verified wire contract (docs/groww-ref/01-introduction-auth.md), a single hardcoded const with no path joining, mirrors the Dhan auth URL consts.
pub const GROWW_TOKEN_ENDPOINT_URL: &str = "https://api.groww.in/v1/token/api/access";

/// Groww API version header name (required on the access-token request).
pub const GROWW_API_VERSION_HEADER: &str = "x-api-version";

/// Groww API version header value.
pub const GROWW_API_VERSION_VALUE: &str = "1.0";

/// `key_type` for the TOTP auth flow (vs the daily-approval `"approval"` flow).
pub const GROWW_KEY_TYPE_TOTP: &str = "totp";

/// Typed outcome of the boot/activation Groww auth smoke-check, so the CALLER can
/// distinguish a credential REJECTION (Groww answered with a non-2xx) from a
/// TRANSPORT failure (could not reach Groww) from a CREDENTIALS failure (SSM
/// fetch failed). The activation watcher uses this to set the feed-health
/// `auth_rejected` flag ONLY on a genuine rejection — never on a network blip.
///
/// SECURITY: no variant ever carries the api-key or TOTP. `Rejected.body` is the
/// Groww response body already passed through [`capture_rest_error_body`]
/// (JWT/PIN/TOTP/clientId redacted); `Transport`/`Credentials` carry only the
/// underlying error's `Display`, which our auth path constructs without secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrowwAuthSmokeError {
    /// Groww RECEIVED the request and rejected the credential (HTTP non-2xx).
    /// `status` is the HTTP status code; `body` is the redacted, truncated
    /// response body (Groww's own error text, e.g. "Token key not found or
    /// inactive"). This is the actionable "refresh the api-key" case.
    Rejected { status: u16, body: String },
    /// Could not reach Groww (DNS, TLS, timeout, connection reset). Transient —
    /// the api-key is NOT to blame, so the caller MUST NOT mark auth-rejected.
    Transport(String),
    /// SSM credential fetch or HTTP client build failed before any request went
    /// out. Not a Groww-side rejection.
    Credentials(String),
}

impl std::fmt::Display for GrowwAuthSmokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { status, body } => {
                write!(f, "Groww rejected the credential (HTTP {status}): {body}")
            }
            Self::Transport(reason) => write!(f, "could not reach Groww: {reason}"),
            Self::Credentials(reason) => write!(f, "Groww credential setup failed: {reason}"),
        }
    }
}

impl std::error::Error for GrowwAuthSmokeError {}

/// Pure classifier: map a non-2xx Groww HTTP status + (already-redacted) response
/// body to the [`GrowwAuthSmokeError::Rejected`] variant. Kept pure so the
/// reject-vs-not decision is unit-tested offline. The caller is responsible for
/// passing a body that has already been through [`capture_rest_error_body`].
///
/// Every non-2xx from Groww's access-token endpoint is a credential rejection
/// (Groww answered — the request reached it; it just refused the key/TOTP), so
/// this always returns `Rejected`. It exists as a named, tested seam so future
/// status-specific handling (e.g. distinguishing 429 throttling) has one place
/// to live.
#[must_use]
pub fn classify_groww_auth_failure(status: u16, redacted_body: &str) -> GrowwAuthSmokeError {
    GrowwAuthSmokeError::Rejected {
        status,
        body: redacted_body.to_string(),
    }
}

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
    // SECURITY: a 2xx body carries the secret token, and `serde_json::Error`'s
    // Display can embed a quoted fragment of the input (the raw token). NEVER
    // interpolate the serde error — use a FIXED `&'static str` reason. The
    // operator action ("the response shape was wrong") is identical regardless
    // of the serde detail.
    let parsed: AccessTokenResponse =
        serde_json::from_str(body).map_err(|_| ApplicationError::AuthenticationFailed {
            reason: "Groww access-token response was not valid JSON".to_string(),
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
/// Returns a typed [`GrowwAuthSmokeError`] so the caller can distinguish a
/// credential REJECTION (Groww answered non-2xx, or a 2xx with an unusable body)
/// from a TRANSPORT failure (could not reach Groww) from a CREDENTIALS failure
/// (local TOTP / serialization fault). The HTTP request (URL, Bearer scheme,
/// `x-api-version`, body) is unchanged — only the error typing changed.
///
/// # Errors
///
/// [`GrowwAuthSmokeError::Credentials`] on TOTP-generation / serialization
/// failure, [`GrowwAuthSmokeError::Transport`] on a send failure, and
/// [`GrowwAuthSmokeError::Rejected`] on a non-2xx response or an unusable token.
#[instrument(skip_all)]
// TEST-EXEMPT: network call to the Groww access-token endpoint; the pure request body + response parsing + classify_groww_auth_failure are unit-tested.
pub async fn obtain_groww_access_token(
    http: &reqwest::Client,
    creds: &GrowwCredentials,
) -> Result<SecretString, GrowwAuthSmokeError> {
    // TOTP failure is a LOCAL credential problem (bad secret), not a Groww-side
    // rejection — classify Credentials so the caller does not blame the api-key.
    let totp_code = generate_totp_code(&creds.totp_secret)
        .map_err(|err| GrowwAuthSmokeError::Credentials(err.to_string()))?;

    let request_body = serde_json::to_string(&AccessTokenRequest {
        key_type: GROWW_KEY_TYPE_TOTP,
        totp: &totp_code,
    })
    .map_err(|err| {
        GrowwAuthSmokeError::Credentials(format!(
            "failed to serialize Groww access-token request: {err}"
        ))
    })?;

    let response = http
        .post(GROWW_TOKEN_ENDPOINT_URL)
        .bearer_auth(creds.api_key.expose_secret())
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(request_body)
        .send()
        .await
        // A send failure = we never reached Groww (DNS/TLS/timeout/reset).
        .map_err(|err| GrowwAuthSmokeError::Transport(err.to_string()))?;

    let status = response.status();
    // A body-read failure means the connection broke mid-response (the read leg
    // failed) — a TRANSPORT problem, not a credential rejection. Propagate it so
    // the caller does NOT silently treat an unreadable body as an empty one and
    // mis-classify it. SECURITY: a body-read transport error carries no token, so
    // its Display is safe to surface.
    let body_text = response
        .text()
        .await
        .map_err(|err| GrowwAuthSmokeError::Transport(err.to_string()))?;

    if !status.is_success() {
        // Groww answered + refused: a credential rejection. The body is safe to
        // capture (no token on a failure) — redact + truncate per the same helper
        // the Dhan REST canary uses, then classify.
        return Err(classify_groww_auth_failure(
            status.as_u16(),
            &capture_rest_error_body(&body_text),
        ));
    }

    // 2xx but an unparseable/empty token is still a Groww-side problem with the
    // response — surface it as a rejection so the operator investigates. SECURITY:
    // a 2xx body carries the secret token, so we DELIBERATELY do NOT interpolate
    // the parse error (`serde_json::Error` can carry a quoted body fragment) into
    // the captured `body` — only a FIXED message. The operator action ("the 2xx
    // response did not contain a usable token") is identical regardless of the
    // serde detail; the real diagnosis is the response shape, never the value.
    parse_access_token_response(&body_text).map_err(|_| GrowwAuthSmokeError::Rejected {
        status: status.as_u16(),
        body: "2xx response did not contain a usable token".to_string(),
    })
}

/// Boot-time Groww auth smoke-check: fetch credentials from SSM, obtain an
/// access token, and confirm the credential chain works. Invoked from boot ONLY
/// when `feeds.groww_enabled` is true (the live feed connector lands in a later
/// PR; this verifies auth end-to-end first so the operator gets a clear signal).
///
/// Returns a typed [`GrowwAuthSmokeError`] so the caller (the activation watcher)
/// can distinguish a genuine credential REJECTION (Groww answered non-2xx) — the
/// "refresh the SSM api-key" case that should light up the Feed Control page —
/// from a TRANSPORT blip (must NOT blame the key) and a CREDENTIALS (SSM) failure.
/// The HTTP request itself (URL, Bearer scheme, `x-api-version`, body) is
/// unchanged from [`obtain_groww_access_token`]; this is the diagnosis-aware
/// wrapper.
///
/// # Errors
///
/// [`GrowwAuthSmokeError::Credentials`] on SSM fetch / HTTP-client-build failure,
/// [`GrowwAuthSmokeError::Transport`] on a send failure (could not reach Groww),
/// [`GrowwAuthSmokeError::Rejected`] on a non-2xx Groww response.
#[instrument(skip_all)]
// TEST-EXEMPT: boot orchestration (SSM fetch + HTTP client build + live network token request); the pure classifier classify_groww_auth_failure + the request/response parsing are unit-tested.
pub async fn run_groww_auth_smoke_check(
    request_timeout_ms: u64,
) -> Result<(), GrowwAuthSmokeError> {
    let creds = fetch_groww_credentials()
        .await
        .map_err(|err| GrowwAuthSmokeError::Credentials(err.to_string()))?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(request_timeout_ms))
        .build()
        .map_err(|err| {
            GrowwAuthSmokeError::Credentials(format!("Groww HTTP client creation failed: {err}"))
        })?;

    // Single source of truth for the request + outcome classification.
    obtain_groww_access_token(&http, &creds).await?;

    info!(
        "Groww access-token auth OK — credentials valid; live ticks stream via \
         the Python growwapi sidecar (the native-Rust NATS connector is deferred)"
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

    #[test]
    fn test_parse_access_token_response_error_does_not_echo_input_bytes() {
        // SECURITY: a malformed 2xx body could be (or carry a fragment of) the
        // secret token. The error message MUST be a fixed string — it must NOT
        // interpolate the serde error, which can quote the offending input.
        // Feed a unique marker and assert it never appears in the error's
        // Display or Debug rendering.
        const MARKER: &str = "SECRETMARKER123";
        let malformed = format!(r#"{{"token": {MARKER} broken json"#);
        let err = parse_access_token_response(&malformed).unwrap_err();

        let display = err.to_string();
        let debug = format!("{err:?}");
        assert!(
            !display.contains(MARKER),
            "error Display leaked input bytes: {display}"
        );
        assert!(
            !debug.contains(MARKER),
            "error Debug leaked input bytes: {debug}"
        );
        assert!(matches!(err, ApplicationError::AuthenticationFailed { .. }));
    }

    #[test]
    fn test_classify_groww_auth_failure_is_rejected_with_status_and_body() {
        // A non-2xx Groww response (the body is already redacted by the caller)
        // maps to the actionable Rejected variant carrying the verbatim status.
        let err = classify_groww_auth_failure(400, "Token key not found or inactive");
        match err {
            GrowwAuthSmokeError::Rejected { status, body } => {
                assert_eq!(status, 400);
                assert_eq!(body, "Token key not found or inactive");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_groww_auth_failure_preserves_any_status() {
        for status in [401u16, 403, 429, 500] {
            assert!(matches!(
                classify_groww_auth_failure(status, "x"),
                GrowwAuthSmokeError::Rejected { status: s, .. } if s == status
            ));
        }
    }

    #[test]
    fn test_groww_auth_smoke_error_display_never_panics_and_carries_status() {
        // Display is what the activation watcher logs — confirm each variant
        // renders and the Rejected variant surfaces the status for triage.
        let rejected = GrowwAuthSmokeError::Rejected {
            status: 400,
            body: "redacted body".to_string(),
        };
        let s = rejected.to_string();
        assert!(s.contains("400"), "display: {s}");
        assert!(s.contains("redacted body"), "display: {s}");

        let transport = GrowwAuthSmokeError::Transport("dns failure".to_string());
        assert!(transport.to_string().contains("could not reach Groww"));

        let creds = GrowwAuthSmokeError::Credentials("ssm down".to_string());
        assert!(creds.to_string().contains("credential setup failed"));
    }

    #[test]
    fn test_classify_distinct_from_transport_and_credentials() {
        // The three outcomes are distinct so the caller can act differently:
        // only Rejected should mark the feed auth-rejected.
        let rejected = classify_groww_auth_failure(400, "x");
        assert_ne!(rejected, GrowwAuthSmokeError::Transport("x".to_string()));
        assert_ne!(rejected, GrowwAuthSmokeError::Credentials("x".to_string()));
    }
}
