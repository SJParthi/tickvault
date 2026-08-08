//! Dhan access-token minter — the box-independent daily minter.
//!
//! # Why this exists
//!
//! Dhan permits **exactly ONE active access token per account**
//! (`docs/dhan-ref/02-authentication.md`: "One active token at a time —
//! generating new token invalidates the old one"). Until 2026-08-08 the only
//! thing that minted a Dhan token was the tickvault app itself, running on the
//! prod EC2 box. That coupled token freshness to the box being UP.
//!
//! On 2026-08-06 the box stopped starting (`ap-south-1a` capacity exhaustion —
//! see `daily-universe-scope-expansion-2026-05-27.md` §0 Quote 12). With the box
//! down, nothing minted, and `/tickvault/<env>/dhan/access-token` went stale —
//! its last write was 25 July. Every consumer reading that parameter got
//! `HTTP 401 / DH-901 "Client ID or user generated access token is invalid or
//! expired"` on every request.
//!
//! Groww never had this failure mode: its minter is a **Lambda**, which runs
//! whether or not the box does. This module is the Dhan mirror of that design —
//! the shape recorded in `groww-shared-token-minter-2026-07-02.md` §10.
//!
//! # The contract
//!
//! Read three credential parameters from SSM → generate a TOTP code → POST
//! `generateAccessToken` → write the resulting JWT back to
//! `/tickvault/<env>/dhan/access-token` as a `SecureString`. Consumers READ that
//! parameter; they never mint. Exactly one minter per broker means there is
//! nothing left to invalidate.
//!
//! # Safety properties
//!
//! - **The token value never reaches a log line.** Only its length and a
//!   shape verdict are ever logged.
//! - **The PIN and TOTP secret never reach a log line**, and the mint URL is
//!   logged without its query string (which carries both).
//! - **Fail-loud, never fail-silent.** A failed mint returns `Err`, which the
//!   bin surfaces to Lambda (visible as an invocation error + alarm), rather
//!   than writing a bad value over a good one.
//! - **A bad mint never overwrites a good token.** The shape check runs BEFORE
//!   the SSM write, so an error body can never land in the parameter.
//!
//! Cold path — one invocation per day on an EventBridge cron.

use std::time::Duration;

use aws_sdk_ssm::Client as SsmClient;
use aws_sdk_ssm::types::ParameterType;
use secrecy::{ExposeSecret, SecretString};
use tracing::{info, warn};

/// Environment variable carrying Dhan's auth host.
///
/// Deliberately NOT defaulted in code: the house rule bans hardcoded URLs in
/// production source (`.claude/hooks/banned-pattern-scanner.sh` category 3), and
/// terraform is the config source of truth — it injects this value into the
/// function's environment. A missing value fails the mint LOUDLY (the errors
/// alarm pages) rather than silently reaching for a guessed host.
pub const DHAN_AUTH_BASE_URL_ENV: &str = "DHAN_AUTH_BASE_URL";

/// Path appended to the auth base URL to mint a token.
pub const DHAN_GENERATE_TOKEN_PATH: &str = "/app/generateAccessToken";

/// SSM service segment for every Dhan parameter.
pub const SSM_DHAN_SERVICE: &str = "dhan";

/// The parameter this Lambda WRITES — the shared token consumers read.
pub const DHAN_ACCESS_TOKEN_SECRET: &str = "access-token";

/// The three credential parameters this Lambda READS.
pub const DHAN_CLIENT_ID_SECRET: &str = "client-id";
pub const DHAN_CLIENT_SECRET_SECRET: &str = "client-secret";
pub const DHAN_TOTP_SECRET: &str = "totp-secret";

/// Bound on the mint request. Dhan's auth endpoint answers in well under a
/// second; a hang must not burn the Lambda's whole timeout.
pub const MINT_TIMEOUT_SECS: u64 = 20;

/// What went wrong, in terms an operator can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    /// A credential parameter could not be read from SSM.
    CredentialRead(String),
    /// The TOTP code could not be generated (bad base32 secret).
    TotpGeneration(String),
    /// The HTTP request itself failed (DNS, TLS, timeout).
    Transport(String),
    /// Dhan answered with a non-2xx status.
    HttpStatus { status: u16, body: String },
    /// Dhan answered 200 but the body carries `{"status":"error",...}`.
    /// Dhan does this for wrong PIN / wrong TOTP / rate limits.
    DhanError(String),
    /// The body parsed but carried no usable `accessToken`.
    NoToken(String),
    /// The returned value does not look like a JWT — refuse to publish it.
    MalformedToken(String),
    /// A required environment variable is missing (deployment error).
    Configuration(String),
    /// The SSM write failed. The token was minted but not published, so the
    /// previous token is now dead and the parameter is stale — loud failure.
    Publish(String),
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(reason) => write!(f, "misconfigured: {reason}"),
            Self::CredentialRead(reason) => {
                write!(f, "could not read Dhan credentials from SSM: {reason}")
            }
            Self::TotpGeneration(reason) => write!(f, "TOTP generation failed: {reason}"),
            Self::Transport(reason) => write!(f, "generateAccessToken request failed: {reason}"),
            Self::HttpStatus { status, body } => {
                write!(f, "generateAccessToken HTTP {status}: {body}")
            }
            Self::DhanError(message) => write!(f, "Dhan rejected the mint: {message}"),
            Self::NoToken(reason) => write!(f, "mint response carried no accessToken: {reason}"),
            Self::MalformedToken(reason) => {
                write!(f, "refusing to publish a malformed token: {reason}")
            }
            Self::Publish(reason) => write!(
                f,
                "token minted but NOT published — the parameter is now stale \
                 and the previous token is dead: {reason}"
            ),
        }
    }
}

impl std::error::Error for MintError {}

// ---------------------------------------------------------------------------
// Pure helpers — no I/O, fully unit-testable
// ---------------------------------------------------------------------------

/// Builds `/tickvault/<environment>/<service>/<secret>`.
///
/// Mirrors `tickvault_core::auth::secret_manager::build_ssm_path` exactly; it
/// is re-derived here rather than imported so this Lambda stays independent of
/// the app crate (it must build and deploy even when the app does not).
#[must_use]
pub fn build_ssm_path(environment: &str, service: &str, secret_name: &str) -> String {
    format!("/tickvault/{environment}/{service}/{secret_name}")
}

/// Builds the mint URL, tolerating a base URL with or without a trailing slash.
#[must_use]
pub fn build_mint_url(auth_base_url: &str) -> String {
    format!(
        "{}{DHAN_GENERATE_TOKEN_PATH}",
        auth_base_url.trim_end_matches('/')
    )
}

/// Is this plausibly a Dhan JWT?
///
/// Dhan access tokens are JWTs — three base64url segments separated by dots,
/// conventionally starting `eyJ`. This is a SHAPE check, not a validity check:
/// its job is to stop an error string or an empty value from being written over
/// a working token, not to prove the token authenticates.
#[must_use]
pub fn looks_like_jwt(token: &str) -> bool {
    let token = token.trim();
    if token.len() < 40 {
        return false;
    }
    let segments: Vec<&str> = token.split('.').collect();
    segments.len() == 3 && segments.iter().all(|segment| !segment.is_empty())
}

/// Truncates an upstream body so a huge or hostile response cannot flood the
/// logs, and so no unbounded third-party text lands in an error message.
#[must_use]
pub fn capture_body(body: &str, max_len: usize) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }
    let mut cut = max_len;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… [truncated]", &trimmed[..cut])
}

/// Extracts the access token from a `generateAccessToken` response body.
///
/// Dhan returns **HTTP 200 even for failures**, signalling the real outcome in
/// the body as `{"status":"error","message":"Invalid Pin"}`. That case is
/// classified as [`MintError::DhanError`] so a wrong PIN or a skewed TOTP reads
/// as exactly that, instead of as a generic parse failure.
///
/// # Errors
///
/// Returns [`MintError::DhanError`] for Dhan's in-body error envelope,
/// [`MintError::NoToken`] when the body is unparsable or carries no
/// `accessToken`, and [`MintError::MalformedToken`] when the value present is
/// not JWT-shaped.
pub fn parse_mint_response(body: &str) -> Result<SecretString, MintError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|err| MintError::NoToken(format!("body is not JSON: {err}")))?;

    // Dhan's 200-with-error envelope. Checked FIRST — the body may carry both
    // an error status and an empty/absent token field.
    if parsed.get("status").and_then(serde_json::Value::as_str) == Some("error") {
        let message = parsed
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no message supplied");
        return Err(MintError::DhanError(message.to_string()));
    }

    let token = parsed
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MintError::NoToken("no `accessToken` field in the response".to_string()))?;

    if !looks_like_jwt(token) {
        return Err(MintError::MalformedToken(format!(
            "value is {} chars and is not a 3-segment JWT",
            token.len()
        )));
    }

    Ok(SecretString::from(token.to_string()))
}

/// Generates the current 6-digit TOTP code from a base32 secret.
///
/// SHA-1 / 6 digits / 30-second period, matching Dhan's 2FA requirement and
/// `tickvault_core::auth::totp_generator` exactly.
///
/// # Errors
///
/// Returns [`MintError::TotpGeneration`] if the secret is not valid base32 or
/// the code cannot be produced.
pub fn generate_totp_code(totp_secret: &SecretString) -> Result<String, MintError> {
    use totp_rs::{Algorithm, Secret as TotpSecret, TOTP};

    let secret_bytes = TotpSecret::Encoded(totp_secret.expose_secret().to_string())
        .to_bytes()
        .map_err(|err| MintError::TotpGeneration(format!("invalid base32 secret: {err}")))?;

    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret_bytes,
        Some("tickvault".to_string()),
        "dhan-auth".to_string(),
    )
    .map_err(|err| MintError::TotpGeneration(format!("TOTP initialization failed: {err}")))?;

    totp.generate_current()
        .map_err(|err| MintError::TotpGeneration(format!("code generation failed: {err}")))
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

/// The three Dhan credentials, read fresh from SSM on every invocation so a
/// rotation is picked up without redeploying this Lambda.
pub struct DhanCredentials {
    pub client_id: SecretString,
    /// Dhan calls this the `pin` — our SSM `client-secret` holds the 6-digit
    /// trading PIN.
    pub pin: SecretString,
    pub totp_secret: SecretString,
}

/// Reads one SSM parameter, decrypted.
async fn read_secret(ssm: &SsmClient, path: &str) -> Result<SecretString, MintError> {
    let response = ssm
        .get_parameter()
        .name(path)
        .with_decryption(true)
        .send()
        .await
        .map_err(|err| MintError::CredentialRead(format!("{path}: {err}")))?;

    let value = response
        .parameter()
        .and_then(aws_sdk_ssm::types::Parameter::value)
        .ok_or_else(|| MintError::CredentialRead(format!("{path}: parameter has no value")))?;

    if value.trim().is_empty() {
        return Err(MintError::CredentialRead(format!("{path}: value is empty")));
    }

    Ok(SecretString::from(value.to_string()))
}

/// Reads all three Dhan credentials concurrently.
///
/// # Errors
///
/// Returns [`MintError::CredentialRead`] if any parameter is missing or empty.
// TEST-EXEMPT: live AWS SSM fetch — mirrors secret_manager.rs::fetch_dhan_credentials; the path construction it depends on is covered by build_ssm_path tests.
pub async fn fetch_credentials(
    ssm: &SsmClient,
    environment: &str,
) -> Result<DhanCredentials, MintError> {
    let client_id_path = build_ssm_path(environment, SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET);
    let pin_path = build_ssm_path(environment, SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET);
    let totp_path = build_ssm_path(environment, SSM_DHAN_SERVICE, DHAN_TOTP_SECRET);

    info!(
        client_id_path = %client_id_path,
        pin_path = %pin_path,
        totp_path = %totp_path,
        "reading Dhan credentials from SSM"
    );

    let (client_id, pin, totp_secret) = tokio::try_join!(
        read_secret(ssm, &client_id_path),
        read_secret(ssm, &pin_path),
        read_secret(ssm, &totp_path),
    )?;

    Ok(DhanCredentials {
        client_id,
        pin,
        totp_secret,
    })
}

/// Mints a fresh Dhan access token via the TOTP flow.
///
/// # Errors
///
/// Returns the [`MintError`] describing which stage failed.
// TEST-EXEMPT: live Dhan HTTP call — its two decision points (URL building, response classification) are covered by build_mint_url + parse_mint_response tests.
pub async fn mint_token(
    http: &reqwest::Client,
    auth_base_url: &str,
    credentials: &DhanCredentials,
) -> Result<SecretString, MintError> {
    let totp_code = generate_totp_code(&credentials.totp_secret)?;
    let url = build_mint_url(auth_base_url);

    // The credentials travel as query params (Dhan's documented contract), so
    // `url` is logged WITHOUT them — it carries no query string by construction.
    info!(url = %url, "minting a fresh Dhan access token");

    let response = http
        .post(&url)
        .query(&[
            ("dhanClientId", credentials.client_id.expose_secret()),
            ("pin", credentials.pin.expose_secret()),
            ("totp", &totp_code),
        ])
        .send()
        .await
        .map_err(|err| {
            // reqwest errors can embed the full URL including the query
            // string, which carries the PIN. Report the error KIND only.
            MintError::Transport(describe_transport_error(&err))
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(MintError::HttpStatus {
            status: status.as_u16(),
            body: capture_body(&body, 300),
        });
    }

    parse_mint_response(&body)
}

/// Describes a transport failure WITHOUT echoing the URL.
///
/// `reqwest::Error`'s `Display` includes the request URL, and this request's
/// URL carries the PIN and the TOTP code in its query string. Classifying by
/// kind keeps those out of CloudWatch entirely.
#[must_use]
// TEST-EXEMPT: reqwest::Error cannot be constructed outside reqwest, so this classifier is unreachable from a unit test; it exists specifically to avoid rendering that error's URL-bearing Display.
pub fn describe_transport_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "timed out".to_string()
    } else if err.is_connect() {
        "connection failed".to_string()
    } else if err.is_request() {
        "request could not be sent".to_string()
    } else if err.is_body() || err.is_decode() {
        "response body could not be read".to_string()
    } else {
        "transport error".to_string()
    }
}

/// Publishes the token to the shared SSM parameter consumers read.
///
/// # Errors
///
/// Returns [`MintError::Publish`] if the write fails — a loud condition: the
/// mint already invalidated the previous token, so a failed write leaves every
/// consumer holding a dead one.
// TEST-EXEMPT: live AWS SSM write — the path it targets is covered by build_ssm_path tests and pinned against terraform by the wiring guard.
pub async fn publish_token(
    ssm: &SsmClient,
    environment: &str,
    token: &SecretString,
) -> Result<String, MintError> {
    let path = build_ssm_path(environment, SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET);

    ssm.put_parameter()
        .name(&path)
        .value(token.expose_secret())
        .r#type(ParameterType::SecureString)
        .overwrite(true)
        .send()
        .await
        .map_err(|err| MintError::Publish(format!("{path}: {err}")))?;

    Ok(path)
}

/// The full daily job: read credentials → mint → publish.
///
/// Returns the parameter path written, for the invocation log.
///
/// # Errors
///
/// Returns the [`MintError`] describing which stage failed. Every failure is
/// loud: the Lambda invocation fails, which is what the operator alarm keys on.
// TEST-EXEMPT: pure composition of three live-AWS/HTTP steps, each individually exempted above; end-to-end proof is the first live invocation.
pub async fn run_mint_and_publish(
    ssm: &SsmClient,
    http: &reqwest::Client,
    environment: &str,
    auth_base_url: &str,
) -> Result<String, MintError> {
    let credentials = fetch_credentials(ssm, environment).await?;
    let token = mint_token(http, auth_base_url, &credentials).await?;

    // Length only — never the value.
    info!(
        token_len = token.expose_secret().len(),
        "mint succeeded; publishing to SSM"
    );

    let path = publish_token(ssm, environment, &token).await?;

    info!(
        parameter = %path,
        "Dhan access token published — consumers reading this parameter are now live"
    );

    Ok(path)
}

/// Builds the HTTP client used for the mint.
///
/// # Errors
///
/// Returns [`MintError::Transport`] if the client cannot be constructed.
pub fn build_http_client() -> Result<reqwest::Client, MintError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(MINT_TIMEOUT_SECS))
        .build()
        .map_err(|err| MintError::Transport(format!("could not build HTTP client: {err}")))
}

/// Resolves the target environment from `TV_ENVIRONMENT`, defaulting to `prod`.
///
/// Defaulting is deliberate: this Lambda is deployed per-environment by
/// terraform, and a missing variable must not silently mint into the wrong
/// namespace — `prod` is the only environment that exists (operator 2026-06-30,
/// single-real-env consolidation).
#[must_use]
pub fn resolve_environment() -> String {
    match std::env::var("TV_ENVIRONMENT") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => {
            warn!("TV_ENVIRONMENT unset — defaulting to `prod`");
            "prod".to_string()
        }
    }
}

/// Resolves the Dhan auth base URL from the environment.
///
/// There is no in-code default (see [`DHAN_AUTH_BASE_URL_ENV`]). Terraform
/// injects the value; an absent or empty one is a deployment error, and failing
/// here is strictly better than minting against a guessed host.
///
/// # Errors
///
/// Returns [`MintError::Configuration`] if the variable is unset or empty.
pub fn resolve_auth_base_url() -> Result<String, MintError> {
    match std::env::var(DHAN_AUTH_BASE_URL_ENV) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        _ => Err(MintError::Configuration(format!(
            "{DHAN_AUTH_BASE_URL_ENV} is unset or empty — terraform must inject \
             the Dhan auth host; refusing to guess it"
        ))),
    }
}

/// Lambda entry point.
///
/// Mints and publishes, then reports which parameter was written. A failure
/// returns `Err`, which surfaces as a Lambda invocation error — the signal the
/// operator alarm keys on. Deliberately NOT swallowed: a silent failure here
/// leaves every consumer on a dead token, which is exactly the class of bug
/// this Lambda exists to end.
///
/// # Errors
///
/// Returns the boxed [`MintError`] from whichever stage failed.
// TEST-EXEMPT: Lambda entry point — builds live AWS clients; the logic it delegates to is unit-tested and the failure mapping is covered by the stage-label test.
pub async fn handle(_event: serde_json::Value) -> Result<serde_json::Value, lambda_runtime::Error> {
    let environment = resolve_environment();
    let auth_base_url = resolve_auth_base_url()?;

    let config = crate::clients::sdk_config().await;
    let ssm = crate::clients::ssm(&config);
    let http = build_http_client()?;

    match run_mint_and_publish(&ssm, &http, &environment, &auth_base_url).await {
        Ok(path) => Ok(serde_json::json!({
            "status": "ok",
            "environment": environment,
            "parameter": path,
        })),
        Err(err) => {
            // One coded ERROR line, then propagate. The message carries the
            // failing stage and never the token, PIN or TOTP.
            tracing::error!(
                stage = err.stage(),
                reason = %err,
                "Dhan token mint FAILED — consumers will keep serving a stale token \
                 until this succeeds"
            );
            Err(lambda_runtime::Error::from(err.to_string()))
        }
    }
}

impl MintError {
    /// A short, stable stage label for log filters and alarms.
    #[must_use]
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "configuration",
            Self::CredentialRead(_) => "credential_read",
            Self::TotpGeneration(_) => "totp",
            Self::Transport(_) => "transport",
            Self::HttpStatus { .. } => "http_status",
            Self::DhanError(_) => "dhan_rejected",
            Self::NoToken(_) => "no_token",
            Self::MalformedToken(_) => "malformed_token",
            Self::Publish(_) => "publish",
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A JWT-shaped string long enough to pass the shape gate.
    fn sample_jwt() -> String {
        format!("eyJhbGciOiJIUzI1NiJ9.{}.{}", "a".repeat(40), "b".repeat(20))
    }

    #[test]
    fn test_build_ssm_path_matches_the_house_layout() {
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET),
            "/tickvault/prod/dhan/access-token"
        );
    }

    #[test]
    fn test_published_path_is_the_one_consumers_read() {
        // This exact string is what brutex and every other consumer reads. If
        // it ever drifts, the minter writes into the void and every consumer
        // silently keeps a dead token — the failure this Lambda exists to end.
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET),
            "/tickvault/prod/dhan/access-token"
        );
    }

    #[test]
    fn test_dhan_path_never_collides_with_the_groww_token_path() {
        // Both brokers name the secret `access-token`; only the service
        // segment separates them. Writing the Dhan token over the Groww one
        // would 401 the Groww REST legs.
        let dhan = build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET);
        let groww = build_ssm_path("prod", "groww", DHAN_ACCESS_TOKEN_SECRET);
        assert_ne!(dhan, groww);
        assert!(dhan.contains("/dhan/"));
        assert!(groww.contains("/groww/"));
    }

    #[test]
    fn test_build_mint_url_tolerates_a_trailing_slash() {
        assert_eq!(
            build_mint_url("https://auth.dhan.co"),
            "https://auth.dhan.co/app/generateAccessToken"
        );
        assert_eq!(
            build_mint_url("https://auth.dhan.co/"),
            "https://auth.dhan.co/app/generateAccessToken"
        );
    }

    #[test]
    fn test_build_mint_url_carries_no_query_string() {
        // The credentials are attached by reqwest as query params; the URL we
        // LOG must never contain them.
        assert!(!build_mint_url("https://auth.example.test").contains('?'));
    }

    #[test]
    fn test_looks_like_jwt_accepts_a_real_shape() {
        assert!(looks_like_jwt(&sample_jwt()));
    }

    #[test]
    fn test_looks_like_jwt_rejects_junk() {
        for junk in [
            "",
            "   ",
            "short",
            "no-dots-at-all-but-quite-long-indeed-padding-padding",
            "only.two-segments-but-long-enough-to-pass-the-length-gate",
            "..",
        ] {
            assert!(!looks_like_jwt(junk), "should have rejected {junk:?}");
        }
    }

    #[test]
    fn test_looks_like_jwt_rejects_empty_segments() {
        assert!(!looks_like_jwt(
            "eyJhbGciOiJIUzI1NiJ9..bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ));
    }

    #[test]
    fn test_parse_mint_response_extracts_the_token() {
        let jwt = sample_jwt();
        let body = format!(r#"{{"accessToken":"{jwt}","dhanClientId":"1106656882"}}"#);
        let parsed = parse_mint_response(&body).expect("valid body must parse");
        assert_eq!(parsed.expose_secret(), jwt);
    }

    #[test]
    fn test_parse_mint_response_classifies_dhans_200_with_error_envelope() {
        // Dhan answers 200 for a wrong PIN. Misclassifying this as a parse
        // failure would send an operator hunting the wrong problem.
        let body = r#"{"status":"error","message":"Invalid Pin"}"#;
        match parse_mint_response(body) {
            Err(MintError::DhanError(message)) => assert_eq!(message, "Invalid Pin"),
            other => panic!("expected DhanError, got {:?}", other.map(|_| "<token>")),
        }
    }

    #[test]
    fn test_parse_mint_response_handles_an_error_envelope_with_no_message() {
        let body = r#"{"status":"error"}"#;
        match parse_mint_response(body) {
            Err(MintError::DhanError(message)) => assert!(message.contains("no message")),
            other => panic!("expected DhanError, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_mint_response_rejects_a_missing_token_field() {
        match parse_mint_response(r#"{"dhanClientId":"1106656882"}"#) {
            Err(MintError::NoToken(reason)) => assert!(reason.contains("accessToken")),
            other => panic!("expected NoToken, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_mint_response_rejects_non_json() {
        match parse_mint_response("<html>502 Bad Gateway</html>") {
            Err(MintError::NoToken(reason)) => assert!(reason.contains("not JSON")),
            other => panic!("expected NoToken, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_mint_response_refuses_a_malformed_token() {
        // The critical safety property: a garbage value must NEVER reach the
        // SSM write, or it silently replaces a working token with junk.
        match parse_mint_response(r#"{"accessToken":"not-a-jwt"}"#) {
            Err(MintError::MalformedToken(reason)) => assert!(reason.contains("not a 3-segment")),
            other => panic!("expected MalformedToken, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_mint_response_refuses_an_empty_token() {
        assert!(matches!(
            parse_mint_response(r#"{"accessToken":""}"#),
            Err(MintError::MalformedToken(_))
        ));
    }

    #[test]
    fn test_error_envelope_wins_over_a_present_token_field() {
        // If Dhan somehow sends both, the error is authoritative.
        let jwt = sample_jwt();
        let body =
            format!(r#"{{"status":"error","message":"Rate limited","accessToken":"{jwt}"}}"#);
        assert!(matches!(
            parse_mint_response(&body),
            Err(MintError::DhanError(_))
        ));
    }

    #[test]
    fn test_capture_body_truncates_long_bodies() {
        let long = "x".repeat(5_000);
        let captured = capture_body(&long, 300);
        assert!(captured.len() < 400);
        assert!(captured.ends_with("… [truncated]"));
    }

    #[test]
    fn test_capture_body_leaves_short_bodies_intact() {
        assert_eq!(capture_body("  short body  ", 300), "short body");
    }

    #[test]
    fn test_capture_body_does_not_split_a_multibyte_char() {
        // A naive slice at a byte index would panic mid-codepoint.
        let body = "नमस्ते".repeat(100);
        let captured = capture_body(&body, 17);
        assert!(captured.ends_with("… [truncated]"));
    }

    #[test]
    fn test_generate_totp_code_produces_six_digits() {
        // 32 base32 chars = a valid 160-bit secret.
        let secret = SecretString::from("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP".to_string());
        let code = generate_totp_code(&secret).expect("valid base32 must generate");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_generate_totp_code_rejects_an_invalid_secret() {
        let secret = SecretString::from("!!! not base32 !!!".to_string());
        assert!(matches!(
            generate_totp_code(&secret),
            Err(MintError::TotpGeneration(_))
        ));
    }

    #[test]
    fn test_auth_base_url_has_no_in_code_default() {
        // The house rule bans hardcoded URLs in production source, and
        // terraform owns this value. Asserted by NAME so a future session
        // cannot quietly reintroduce a literal default.
        assert_eq!(DHAN_AUTH_BASE_URL_ENV, "DHAN_AUTH_BASE_URL");
        let source = include_str!("dhan_token_minter.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or_default();
        assert!(
            !production.contains("//auth.dhan.co"),
            "production code must not hardcode the Dhan auth host — terraform \
             injects DHAN_AUTH_BASE_URL"
        );
    }

    #[test]
    fn test_mint_error_messages_are_operator_actionable() {
        // Each variant must say what failed in words an operator can act on.
        assert!(
            MintError::DhanError("Invalid Pin".to_string())
                .to_string()
                .contains("Invalid Pin")
        );
        assert!(
            MintError::Publish("denied".to_string())
                .to_string()
                .contains("NOT published")
        );
        assert!(
            MintError::MalformedToken("bad".to_string())
                .to_string()
                .contains("refusing to publish")
        );
    }

    #[test]
    fn test_no_error_variant_echoes_a_secret_bearing_url() {
        // Every message an operator sees must be free of query strings, which
        // is where the PIN and TOTP travel.
        for error in [
            MintError::Transport("timed out".to_string()),
            MintError::HttpStatus {
                status: 401,
                body: "unauthorized".to_string(),
            },
            MintError::DhanError("Invalid Pin".to_string()),
        ] {
            let rendered = error.to_string();
            assert!(!rendered.contains("pin="), "leaked a PIN: {rendered}");
            assert!(!rendered.contains("totp="), "leaked a TOTP: {rendered}");
        }
    }
    // -----------------------------------------------------------------------
    // Env-dependent resolvers.
    //
    // `std::env::set_var` is process-global, so these serialize behind a shared
    // mutex and restore the prior value on drop — the house pattern from
    // `crates/api/tests/tv_api_token_prod_guard.rs`, added there after parallel
    // test threads flipped an asserted branch.
    // -----------------------------------------------------------------------

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let previous = std::env::var(key).ok();
            match value {
                // SAFETY: serialized by ENV_LOCK, restored on drop.
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn test_resolve_auth_base_url_reads_the_injected_value() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set(DHAN_AUTH_BASE_URL_ENV, Some("  https://stub.test  "));
        assert_eq!(
            resolve_auth_base_url().expect("a set value must resolve"),
            "https://stub.test",
            "surrounding whitespace must be trimmed"
        );
    }

    #[test]
    fn test_resolve_auth_base_url_refuses_to_guess_when_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set(DHAN_AUTH_BASE_URL_ENV, None);
        assert!(
            matches!(resolve_auth_base_url(), Err(MintError::Configuration(_))),
            "an unset host must FAIL, never fall back to a guessed default"
        );
    }

    #[test]
    fn test_resolve_auth_base_url_treats_empty_as_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set(DHAN_AUTH_BASE_URL_ENV, Some("   "));
        assert!(matches!(
            resolve_auth_base_url(),
            Err(MintError::Configuration(_))
        ));
    }

    #[test]
    fn test_resolve_environment_reads_the_injected_value() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set("TV_ENVIRONMENT", Some("prod"));
        assert_eq!(resolve_environment(), "prod");
    }

    #[test]
    fn test_resolve_environment_defaults_to_prod_when_unset() {
        // Unlike the auth host, defaulting here is safe and deliberate: prod is
        // the only environment that exists (2026-06-30 single-env consolidation).
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set("TV_ENVIRONMENT", None);
        assert_eq!(resolve_environment(), "prod");
    }

    #[test]
    fn test_build_http_client_succeeds_and_is_bounded() {
        assert!(
            build_http_client().is_ok(),
            "the HTTP client must construct — a failure here means no mint is \
             possible at all"
        );
        assert_eq!(
            MINT_TIMEOUT_SECS, 20,
            "the mint must stay bounded well inside the Lambda's 60s timeout"
        );
    }

    #[test]
    fn test_stage_labels_are_stable_and_distinct() {
        // These labels are what log filters and future alarms key on, so they
        // are part of the operator contract, not an implementation detail.
        let labelled = [
            (MintError::Configuration(String::new()), "configuration"),
            (MintError::CredentialRead(String::new()), "credential_read"),
            (MintError::TotpGeneration(String::new()), "totp"),
            (MintError::Transport(String::new()), "transport"),
            (
                MintError::HttpStatus {
                    status: 500,
                    body: String::new(),
                },
                "http_status",
            ),
            (MintError::DhanError(String::new()), "dhan_rejected"),
            (MintError::NoToken(String::new()), "no_token"),
            (MintError::MalformedToken(String::new()), "malformed_token"),
            (MintError::Publish(String::new()), "publish"),
        ];
        for (error, expected) in &labelled {
            assert_eq!(error.stage(), *expected);
        }
        let mut seen: Vec<&str> = labelled.iter().map(|(_, label)| *label).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "stage labels must be distinct");
    }
}
