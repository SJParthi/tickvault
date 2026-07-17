//! Groww per-session socket-token mint — the push-channel AUTH REST call.
//! Restored byte-faithful (2026-07-16, order-push Stage B) from the retired
//! live-feed transport (`dd7eaa5e^:crates/core/src/feed/groww/native/
//! socket_token.rs`); the endpoint const now lives HERE (deleted from
//! `constants.rs` 2026-07-15 with the live feed — deliberately not re-added).
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
use tracing::info;

/// The per-session socket-token mint endpoint — byte-identical to the
/// growwapi-1.5.0 wheel's canonical form (`client.py::_GROWW_GENERATE_SOCKET_TOKEN_URL`,
/// trailing slash included). Defined LOCALLY in this module: the old
/// `tickvault_common::constants::GROWW_SOCKET_TOKEN_URL` was deleted with the
/// Groww live feed 2026-07-15 and is deliberately NOT re-added to
/// constants.rs — the push transport is the const's only consumer.
pub const GROWW_SOCKET_TOKEN_URL: &str = "https://api.groww.in/v1/api/apex/v1/socket/token/create/"; // APPROVED: wheel-canonical mint endpoint — byte-identity to growwapi-1.5.0 is load-bearing (deliberately not config)

/// The ONLY host the mint call may talk to. The bounded one-hop redirect
/// follow below refuses any Location that leaves this host (or downgrades
/// off https) — the §18-class no-redirect hardening stays intact for every
/// other target.
pub const GROWW_API_HOST: &str = "api.groww.in";

/// Max redirect hops the mint POST will follow (307/308, same host only).
///
/// Live evidence (operator run 2026-07-04 15:10–15:12 IST): the server
/// answers the canonical SDK URL with HTTP 307 (server-side canonicalization
/// we cannot see — our URL/headers/body are byte-identical to the
/// growwapi-1.5.0 wheel, which succeeds only because Python `requests`
/// follows redirects). One validated hop is the minimum that makes the mint
/// work without re-enabling blanket redirect following.
const MAX_REDIRECT_HOPS: u8 = 1;

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
    /// The account-scoped subscription id — the KEY of the order/position
    /// update subjects (`…updates.apex.<subscriptionId>`, see
    /// [`super::subjects`]). Account-scoped routing material: treated as
    /// sensitive-adjacent (never logged as anything but length), though not a
    /// credential.
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
    /// A 3xx redirect we refused to follow — cross-host / http-downgrade
    /// Location, a non-method-preserving code (301/302/303 mutate POST→GET),
    /// a missing Location, or the one-hop budget exhausted. Carries the
    /// SANITIZED Location (query + fragment stripped) so a future redirect is
    /// self-explaining in the GROWW-PUSH-02 error line without leaking any
    /// query-string payload.
    Redirect {
        /// The 3xx status code observed.
        status: u16,
        /// The sanitized (query/fragment-stripped) Location target.
        location: String,
    },
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
            Self::Redirect { status, location } => write!(
                f,
                "groww socket-token: http status {status} redirect to {location} (not followed)"
            ),
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

/// Sanitize a Location header value for error strings / logs: strip the
/// query string and fragment (a hostile Location query could carry payloads
/// we must never echo). Pure + total.
#[must_use]
pub fn sanitize_location(location: &str) -> String {
    let no_fragment = location.split('#').next().unwrap_or("");
    no_fragment.split('?').next().unwrap_or("").to_string()
}

/// Resolve a 307/308 `Location` into a followable target for the mint POST.
///
/// Returns `Some(url)` ONLY when the target stays on [`GROWW_API_HOST`] over
/// https — relative Locations resolve against the request URL (same host by
/// construction); absolute Locations must match the host exactly. Cross-host,
/// http-downgrade, empty, and unparseable Locations all refuse (`None`).
/// Pure + total (no I/O), so the same-host validation is unit-testable.
#[must_use]
pub fn same_host_redirect_target(base_url: &str, location: &str) -> Option<String> {
    if location.trim().is_empty() {
        return None;
    }
    let base = reqwest::Url::parse(base_url).ok()?;
    // `join` handles both absolute Locations (replaces base entirely) and
    // relative ones (resolved against the request URL) per RFC 3986.
    let target = base.join(location).ok()?;
    if target.scheme() != "https" {
        return None;
    }
    if target.host_str() != Some(GROWW_API_HOST) {
        return None;
    }
    Some(target.to_string())
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
///
/// Redirect handling (2026-07-05, Monday-critical fix): the server answers
/// the canonical SDK URL with HTTP 307 (server-side canonicalization — our
/// URL/headers/body are byte-identical to the growwapi-1.5.0 wheel, which
/// only succeeds because Python `requests` auto-follows). We follow at most
/// [`MAX_REDIRECT_HOPS`] hop, ONLY for the method-preserving codes 307/308,
/// ONLY when [`same_host_redirect_target`] validates the Location stays on
/// `https://api.groww.in` — re-sending the same method/headers/body per 307
/// semantics. Every other 3xx surfaces as [`SocketTokenError::Redirect`]
/// carrying the sanitized Location. The reqwest client itself keeps
/// `redirect::Policy::none()` — the one hop is explicit, validated
/// application code, not blanket redirect following.
/// The pure request-body builder + the auth-status classifier + the redirect
/// validators + the response shape are unit-tested below; no test may call
/// the real Groww endpoint.
// TEST-EXEMPT: live-HTTP orchestration over unit-tested pure builders/classifiers.
pub async fn mint_socket_token(
    client: &reqwest::Client,
    access_token: &SecretString,
    nkey_public: &str,
) -> Result<SocketToken, SocketTokenError> {
    let mut url = GROWW_SOCKET_TOKEN_URL.to_string();
    let mut hops: u8 = 0;
    loop {
        let response = send_mint_post(client, &url, access_token, nkey_public).await?;

        let status = response.status();
        if status.is_redirection() {
            let status_code = status.as_u16();
            let location_raw = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let sanitized = sanitize_location(&location_raw);
            // Only 307/308 preserve POST + body; 301/302/303 would mutate the
            // request (POST→GET) and are never followed.
            if matches!(status_code, 307 | 308)
                && hops < MAX_REDIRECT_HOPS
                && let Some(target) = same_host_redirect_target(&url, &location_raw)
            {
                info!(
                    status = status_code,
                    location = %sanitized,
                    "groww socket-token: following one same-host redirect hop"
                );
                url = target;
                hops += 1;
                continue;
            }
            return Err(SocketTokenError::Redirect {
                status: status_code,
                location: sanitized,
            });
        }
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
        return Ok(SocketToken {
            jwt: SecretString::from(parsed.token),
            subscription_id: parsed.subscription_id.unwrap_or_default(),
        });
    }
}

/// One mint POST to the given URL — same method + headers + body on every
/// hop, per 307 semantics (the host is pre-validated same-host, so the
/// `Authorization` header never leaves `api.groww.in`).
async fn send_mint_post(
    client: &reqwest::Client,
    url: &str,
    access_token: &SecretString,
    nkey_public: &str,
) -> Result<reqwest::Response, SocketTokenError> {
    client
        .post(url)
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
        .map_err(|e| SocketTokenError::Transport(e.to_string()))
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
    /// does not — including a refused redirect.
    #[test]
    fn test_is_auth_status() {
        assert!(is_auth_status(&SocketTokenError::Status(401)));
        assert!(is_auth_status(&SocketTokenError::Status(403)));
        assert!(!is_auth_status(&SocketTokenError::Status(429)));
        assert!(!is_auth_status(&SocketTokenError::Status(500)));
        assert!(!is_auth_status(&SocketTokenError::Transport(String::new())));
        assert!(!is_auth_status(&SocketTokenError::MalformedResponse));
        assert!(!is_auth_status(&SocketTokenError::Redirect {
            status: 307,
            location: String::new(),
        }));
    }

    /// The mint URL is byte-identical to the growwapi-1.5.0 wheel's canonical
    /// form — verified 2026-07-05 against the wheel extraction:
    ///   growwapi/groww/client.py:117  (_GROWW_GENERATE_SOCKET_TOKEN_URL)
    ///   growwapi/groww/feed.py:125    (_GROWW_GENERATE_SOCKET_TOKEN_URL)
    /// Both read `https://api.groww.in/v1/api/apex/v1/socket/token/create/`
    /// (trailing slash included). The live 307 (operator run 2026-07-04
    /// 15:10 IST) is therefore server-side canonicalization, not a URL diff —
    /// which is why the bounded same-host one-hop follow exists.
    #[test]
    fn test_socket_token_url_matches_sdk_wheel_canonical_form() {
        assert_eq!(
            GROWW_SOCKET_TOKEN_URL,
            "https://api.groww.in/v1/api/apex/v1/socket/token/create/"
        );
        // The host lock the redirect validator enforces must be the URL's own host.
        let parsed = reqwest::Url::parse(GROWW_SOCKET_TOKEN_URL).expect("constant parses");
        assert_eq!(parsed.host_str(), Some(GROWW_API_HOST));
        assert_eq!(parsed.scheme(), "https");
    }

    /// An absolute same-host https Location is followable (query preserved on
    /// the wire — sanitization applies only to logs/errors).
    #[test]
    fn test_same_host_redirect_target_accepts_absolute_same_host() {
        let target = same_host_redirect_target(
            GROWW_SOCKET_TOKEN_URL,
            "https://api.groww.in/v1/api/apex/v1/socket/token/create",
        );
        assert_eq!(
            target.as_deref(),
            Some("https://api.groww.in/v1/api/apex/v1/socket/token/create")
        );
    }

    /// A relative Location resolves against the request URL (same host by
    /// construction) and is followable.
    #[test]
    fn test_same_host_redirect_target_resolves_relative_path() {
        let target = same_host_redirect_target(
            GROWW_SOCKET_TOKEN_URL,
            "/v1/api/apex/v1/socket/token/create",
        );
        assert_eq!(
            target.as_deref(),
            Some("https://api.groww.in/v1/api/apex/v1/socket/token/create")
        );
    }

    /// A cross-host Location is REFUSED — the Authorization header must never
    /// leave api.groww.in (DNS-poison / hostile-redirect defense, §18 class).
    #[test]
    fn test_same_host_redirect_target_refuses_cross_host() {
        assert!(
            same_host_redirect_target(
                GROWW_SOCKET_TOKEN_URL,
                "https://evil.example/v1/api/apex/v1/socket/token/create/",
            )
            .is_none()
        );
        // Subdomain is a DIFFERENT host — also refused.
        assert!(
            same_host_redirect_target(
                GROWW_SOCKET_TOKEN_URL,
                "https://api.groww.in.evil.example/x"
            )
            .is_none()
        );
    }

    /// An http:// downgrade is REFUSED even on the same host.
    #[test]
    fn test_same_host_redirect_target_refuses_http_downgrade() {
        assert!(
            same_host_redirect_target(
                GROWW_SOCKET_TOKEN_URL,
                "http://api.groww.in/v1/api/apex/v1/socket/token/create/",
            )
            .is_none()
        );
    }

    /// An empty / whitespace Location is REFUSED (never re-POST the same URL
    /// on a vacuous redirect).
    #[test]
    fn test_same_host_redirect_target_refuses_empty() {
        assert!(same_host_redirect_target(GROWW_SOCKET_TOKEN_URL, "").is_none());
        assert!(same_host_redirect_target(GROWW_SOCKET_TOKEN_URL, "   ").is_none());
    }

    /// Sanitization strips the query string and fragment — a hostile Location
    /// query payload can never be echoed into an error line.
    #[test]
    fn test_sanitize_location_strips_query_and_fragment() {
        assert_eq!(
            sanitize_location("https://api.groww.in/v1/x?session=SECRET&y=2#frag"),
            "https://api.groww.in/v1/x"
        );
        assert_eq!(sanitize_location("/relative/path?q=1"), "/relative/path");
        assert_eq!(sanitize_location(""), "");
    }

    /// The refused-redirect error names the sanitized target so a future 3xx
    /// is self-explaining in the GROWW-PUSH-02 line.
    #[test]
    fn test_redirect_error_display_includes_sanitized_location() {
        let e = SocketTokenError::Redirect {
            status: 307,
            location: sanitize_location("https://elsewhere.example/path?tok=x"),
        };
        assert_eq!(
            e.to_string(),
            "groww socket-token: http status 307 redirect to https://elsewhere.example/path (not followed)"
        );
    }

    /// Error Display never embeds a token value (variants carry status /
    /// transport text only — this pins the contract by construction).
    #[test]
    fn test_error_display_carries_no_secret() {
        let e = SocketTokenError::Status(401);
        assert_eq!(e.to_string(), "groww socket-token: http status 401");
    }

    /// The hardened one-purpose client constructs (TLS roots load) — the
    /// redirect policy / timeout hardening is pinned by construction in
    /// `mint_client` itself; a build failure here is the HTTP-CLIENT-01 class.
    #[test]
    fn test_mint_client_builds() {
        assert!(mint_client().is_ok());
    }

    /// `mint_socket_token` live-HTTP orchestration contract: no test may call
    /// the real Groww endpoint, so the contract is pinned via its pure parts —
    /// the URL const it targets, the request body it sends, and the redirect
    /// validators it consults (each unit-tested above). This test pins the
    /// composition inputs so a drift in any of them names the orchestrator.
    #[test]
    fn test_mint_socket_token_composition_inputs_pinned() {
        // the orchestrator's entry URL
        assert!(GROWW_SOCKET_TOKEN_URL.starts_with("https://api.groww.in/"));
        // the body builder it uses
        assert_eq!(socket_token_request_body("UKEY"), r#"{"socketKey":"UKEY"}"#);
        // the redirect budget is exactly one validated same-host hop
        assert_eq!(MAX_REDIRECT_HOPS, 1);
    }
}
