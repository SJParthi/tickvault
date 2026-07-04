//! Groww per-session socket-token mint — the LIVE-FEED-AUTH REST call
//! (KEEP class of `no-rest-except-live-feed-2026-06-27.md` §2/§3, recorded
//! there 2026-07-04).
//!
//! Verified from the growwapi-1.5.0 wheel (`client.py::generate_socket_token`
//! + `_build_headers`): `POST` [`GROWW_SOCKET_TOKEN_URL`] with
//! `Authorization: Bearer <access-token>` and body
//! `{"socketKey": "<nkey U… public key>"}` → `{"token": <NATS user JWT>,
//! "subscriptionId": <id>}`.
//!
//! Lock notes:
//! - This consumes the SSM-READ access token; it does NOT mint the access
//!   token (shared-minter lock 2026-07-02 untouched). It is exactly the call
//!   the Python sidecar's SDK already performs on every feed construction.
//! - Both the access token and the returned socket JWT are [`SecretString`]
//!   end to end; error variants never carry either value.

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tickvault_common::constants::GROWW_SOCKET_TOKEN_URL;

/// `x-client-id` header value the wheel sends.
pub const HEADER_CLIENT_ID: &str = "growwapi";
/// `x-client-platform` header value (mirrors the wheel's SDK identification).
pub const HEADER_CLIENT_PLATFORM: &str = "growwapi-python-client";
/// `x-client-platform-version` header value (the wheel version the protocol
/// was extracted from).
pub const HEADER_CLIENT_PLATFORM_VERSION: &str = "1.5.0";
/// `x-api-version` header value.
pub const HEADER_API_VERSION: &str = "1.0";

/// HTTP timeouts for the mint call (one small POST; bounded, never hangs).
const CONNECT_TIMEOUT_SECS: u64 = 10;
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// The minted per-session NATS credentials.
pub struct SocketToken {
    /// The NATS user JWT bound to this session's nkey public key.
    pub jwt: SecretString,
    /// The account-scoped subscription id (used ONLY by order/position update
    /// subjects — OUT OF SCOPE §33; retained for completeness, never logged
    /// as anything but length).
    pub subscription_id: String,
}

/// Why the socket-token mint failed. NEVER carries the access token, the
/// minted JWT, or a response body (a Groww error body cannot be trusted to be
/// secret-free, so only the status code is kept).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketTokenError {
    /// The HTTP send itself failed (DNS / TLS / timeout). Transport class —
    /// retry with backoff.
    Transport(String),
    /// Non-2xx response. `401`/`403` are the auth class (stale access token →
    /// SSM re-read at the ≥60s floor, never a mint).
    Status(u16),
    /// 2xx but the body did not parse into `{token, subscriptionId}`.
    MalformedResponse,
    /// The client could not even be constructed (HTTP-CLIENT-01 class).
    ClientBuild(String),
}

impl core::fmt::Display for SocketTokenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "groww socket-token: transport: {e}"),
            Self::Status(code) => write!(f, "groww socket-token: http status {code}"),
            Self::MalformedResponse => f.write_str("groww socket-token: malformed response body"),
            Self::ClientBuild(e) => write!(f, "groww socket-token: client build: {e}"),
        }
    }
}

impl std::error::Error for SocketTokenError {}

/// True when the mint failure is AUTH-class (the access token is stale /
/// rejected) — the caller re-reads the token from SSM at the pacing floor
/// instead of hammering the same value. Pure + total.
#[must_use]
pub const fn is_auth_status(err: &SocketTokenError) -> bool {
    matches!(err, SocketTokenError::Status(401 | 403))
}

/// Build the mint request body — pure so the wire shape is pinned by a test.
#[must_use]
pub fn socket_token_request_body(nkey_public: &str) -> String {
    // serde_json escaping via Value keeps this total (an nkey is base32-safe,
    // but never splice strings into JSON by hand).
    serde_json::json!({ "socketKey": nkey_public }).to_string()
}

/// The wire response shape.
#[derive(Deserialize)]
struct SocketTokenResponse {
    token: String,
    #[serde(rename = "subscriptionId")]
    subscription_id: Option<String>,
}

/// Hardened one-purpose HTTP client for the mint call: no redirects
/// (DNS-poison 301 defense per the §18 downloader contract), bounded
/// connect/request timeouts.
// TEST-EXEMPT: constructs a reqwest client (TLS root load); mirrors the audited csv_downloader/instruments.rs hardened_client pattern.
pub fn mint_client() -> Result<reqwest::Client, SocketTokenError> {
    reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| SocketTokenError::ClientBuild(e.to_string()))
}

/// Mint a per-session socket token for the given nkey public key.
///
/// One POST, no internal retry — the caller owns the backoff/re-read ladder
/// (`client.rs`). The access token appears ONLY in the `Authorization` header
/// (never the URL, never a log).
/// The pure request-body builder + the auth-status classifier + the response
/// shape are unit-tested below; no test may call the real Groww endpoint.
// TEST-EXEMPT: live-HTTP orchestration over unit-tested pure builders/classifiers.
pub async fn mint_socket_token(
    client: &reqwest::Client,
    access_token: &SecretString,
    nkey_public: &str,
) -> Result<SocketToken, SocketTokenError> {
    let response = client
        .post(GROWW_SOCKET_TOKEN_URL)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", access_token.expose_secret()),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-request-id", uuid_v4_string())
        .header("x-client-id", HEADER_CLIENT_ID)
        .header("x-client-platform", HEADER_CLIENT_PLATFORM)
        .header("x-client-platform-version", HEADER_CLIENT_PLATFORM_VERSION)
        .header("x-api-version", HEADER_API_VERSION)
        .body(socket_token_request_body(nkey_public))
        .send()
        .await
        .map_err(|e| SocketTokenError::Transport(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(SocketTokenError::Status(status.as_u16()));
    }
    let parsed: SocketTokenResponse = response
        .json()
        .await
        .map_err(|_| SocketTokenError::MalformedResponse)?;
    if parsed.token.trim().is_empty() {
        return Err(SocketTokenError::MalformedResponse);
    }
    Ok(SocketToken {
        jwt: SecretString::from(parsed.token),
        subscription_id: parsed.subscription_id.unwrap_or_default(),
    })
}

/// `x-request-id` value (the wheel sends a fresh uuid4 per request).
fn uuid_v4_string() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire body is exactly `{"socketKey":"U…"}` — pinned so a field
    /// rename can never silently break the mint.
    #[test]
    fn test_socket_token_request_body() {
        let body = socket_token_request_body("UABCDEF234567");
        assert_eq!(body, r#"{"socketKey":"UABCDEF234567"}"#);
    }

    /// A hostile nkey string is JSON-escaped, never spliced.
    #[test]
    fn test_socket_token_request_body_escapes() {
        let body = socket_token_request_body(r#"U"evil"#);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(v["socketKey"], r#"U"evil"#);
    }

    /// Response parsing accepts the documented shape and tolerates a missing
    /// subscriptionId (market-data subjects never use it).
    #[test]
    fn test_socket_token_response_shape() {
        let full: SocketTokenResponse =
            serde_json::from_str(r#"{"token":"eyJ...","subscriptionId":"sub-1"}"#)
                .expect("full shape parses");
        assert_eq!(full.token, "eyJ...");
        assert_eq!(full.subscription_id.as_deref(), Some("sub-1"));

        let minimal: SocketTokenResponse =
            serde_json::from_str(r#"{"token":"eyJ..."}"#).expect("minimal shape parses");
        assert!(minimal.subscription_id.is_none());
    }

    /// 401/403 classify as auth (re-read the access token); everything else
    /// does not.
    #[test]
    fn test_is_auth_status() {
        assert!(is_auth_status(&SocketTokenError::Status(401)));
        assert!(is_auth_status(&SocketTokenError::Status(403)));
        assert!(!is_auth_status(&SocketTokenError::Status(429)));
        assert!(!is_auth_status(&SocketTokenError::Status(500)));
        assert!(!is_auth_status(&SocketTokenError::Transport(String::new())));
        assert!(!is_auth_status(&SocketTokenError::MalformedResponse));
    }

    /// Error Display never embeds a token value (variants carry status /
    /// transport text only — this pins the contract by construction).
    #[test]
    fn test_error_display_carries_no_secret() {
        let e = SocketTokenError::Status(401);
        assert_eq!(e.to_string(), "groww socket-token: http status 401");
    }
}
