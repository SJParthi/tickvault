//! Depth-200 SELF Token Manager — AWS SSM-backed alternate auth path.
//!
//! # Why this module exists
//!
//! Dhan's `wss://full-depth-api.dhan.co/` rejects access tokens whose
//! `tokenConsumerType` claim is `"APP"` (the only type our automated
//! `POST /app/generateAccessToken` flow can mint). Tokens with
//! `tokenConsumerType: "SELF"` (operator-pasted from `web.dhan.co`)
//! stream depth-200 frames continuously. Verified 2026-04-28 with
//! `dhanhq==2.2.0rc1`. Email + GitHub link sent to apihelp@dhan.co.
//!
//! This module is the parallel auth flow for the depth-200 WebSocket
//! ONLY. Main feed / depth-20 / order-update / REST keep using the
//! TOTP/APP token from `TokenManager`. Gated behind
//! `Depth200AuthConfig::is_manual_self_mode() == true`.
//!
//! # Storage
//!
//! Per CLAUDE.md "always real AWS SSM" — there is no local-disk cache
//! for this token. Boot reads `GetParameter` against the configured
//! SSM path (default `/tickvault/dev/dhan/depth_200_self_token`),
//! validates the JWT is `tokenConsumerType: "SELF"`, populates the
//! `TokenHandle`. The renewal task calls `GET /v2/RenewToken` every
//! ~23h, then `PutParameter` to overwrite the SSM value with the
//! extended JWT, then atomic-swaps the new `TokenState` into the
//! handle.
//!
//! # Cross-refs
//!
//! - `.claude/rules/dhan/full-market-depth.md`
//! - `.claude/rules/dhan/authentication.md` rule 5 — `RenewToken`
//! - `docs/dhan-support/2026-04-28-depth-200-app-vs-self-token.md`

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwap;
use aws_sdk_ssm::Client as SsmClient;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{error, info, instrument};

use tickvault_common::config::Depth200AuthConfig;
use tickvault_common::trading_calendar::ist_offset;

use crate::auth::secret_manager::create_ssm_client;
use crate::auth::token_manager::TokenHandle;
use crate::auth::types::TokenState;

/// HTTP timeout for the `GET /v2/RenewToken` call. Generous because
/// Dhan's auth tier occasionally takes 5-10s under load.
const RENEW_TOKEN_HTTP_TIMEOUT_SECS: u64 = 30;

/// Tolerance window for the JWT `iat` claim relative to local clock.
/// Anything beyond this is treated as clock skew or a forged token.
const IAT_CLOCK_SKEW_TOLERANCE_SECS: i64 = 60;

/// JWT claim value indicating an operator-generated token (the kind
/// `wss://full-depth-api.dhan.co/` accepts).
pub const TOKEN_CONSUMER_TYPE_SELF: &str = "SELF";

/// JWT claim value indicating an automation-minted token (the kind
/// depth-200 rejects with `Protocol(ResetWithoutClosingHandshake)`).
pub const TOKEN_CONSUMER_TYPE_APP: &str = "APP";

/// Subset of JWT claims the depth-200 SELF flow cares about.
#[derive(Debug, Clone, Deserialize)]
pub struct ParsedSelfClaims {
    /// MUST be `"SELF"` for a working depth-200 token.
    #[serde(rename = "tokenConsumerType")]
    pub token_consumer_type: String,
    /// Expiry as Unix epoch seconds.
    pub exp: i64,
    /// Issued-at as Unix epoch seconds.
    pub iat: i64,
    /// Dhan client ID — must match the configured client ID at boot.
    #[serde(rename = "dhanClientId")]
    pub dhan_client_id: String,
}

/// Pure base64url decoder (RFC 4648 §5). No padding required, URL-safe
/// alphabet. Used to decode the JWT payload segment.
fn base64url_decode(input: &str) -> Result<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        if b == b'=' {
            break;
        }
        let val: u8 = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => bail!("invalid base64url character: 0x{:02x}", b),
        };
        buffer = (buffer << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            #[allow(clippy::cast_possible_truncation)] // APPROVED: bottom 8 bits only
            decoded.push((buffer >> bits) as u8);
            buffer &= (1u32 << bits) - 1;
        }
    }
    Ok(decoded)
}

/// Pure JWT parser — splits on `.`, base64url-decodes the payload,
/// JSON-deserializes into `ParsedSelfClaims`. Does NOT verify the
/// signature (we don't have Dhan's HS512 signing key — the trust
/// boundary is the SSM SecureString and the TLS-bound RenewToken
/// response).
///
/// # Errors
///
/// Returns `anyhow::Error` for malformed JWT shape, invalid base64url,
/// invalid UTF-8, or JSON missing the four required claims.
pub fn parse_self_jwt(jwt: &str) -> Result<ParsedSelfClaims> {
    let mut parts = jwt.split('.');
    let _header = parts.next().context("JWT missing header segment")?;
    let payload = parts.next().context("JWT missing payload segment")?;
    let _signature = parts.next().context("JWT missing signature segment")?;
    if parts.next().is_some() {
        bail!("JWT has more than 3 segments");
    }
    let decoded = base64url_decode(payload).context("JWT payload base64url decode failed")?;
    let claims: ParsedSelfClaims =
        serde_json::from_slice(&decoded).context("JWT payload JSON parse failed")?;
    Ok(claims)
}

/// Pure validator — confirms a parsed SELF JWT is acceptable for use:
///
/// 1. `tokenConsumerType == "SELF"` (rejects APP tokens hard)
/// 2. `dhanClientId == expected_client_id` (defense against a
///    cross-account leak)
/// 3. `exp` is in the future by at least `min_remaining_secs`
/// 4. `iat <= now` (sanity check — a future iat means clock skew or
///    forged token)
///
/// # Errors
///
/// Returns `anyhow::Error` describing exactly which check failed —
/// the operator's runbook copy-pastes the message.
pub fn validate_self_claims(
    claims: &ParsedSelfClaims,
    expected_client_id: &str,
    min_remaining_secs: i64,
    now_unix_secs: i64,
) -> Result<()> {
    if claims.token_consumer_type != TOKEN_CONSUMER_TYPE_SELF {
        bail!(
            "tokenConsumerType must be \"{}\" for depth-200, got \"{}\" — \
             paste a fresh SELF token from web.dhan.co",
            TOKEN_CONSUMER_TYPE_SELF,
            claims.token_consumer_type
        );
    }
    if claims.dhan_client_id != expected_client_id {
        bail!(
            "dhanClientId in SELF token (\"{}\") does not match expected (\"{}\")",
            claims.dhan_client_id,
            expected_client_id
        );
    }
    let remaining = claims.exp - now_unix_secs;
    if remaining < min_remaining_secs {
        bail!(
            "SELF token has only {} seconds of validity left (minimum {} required) — \
             paste a fresh token at the SSM path",
            remaining,
            min_remaining_secs
        );
    }
    if claims.iat > now_unix_secs + IAT_CLOCK_SKEW_TOLERANCE_SECS {
        bail!(
            "SELF token iat ({}) is more than {}s ahead of now ({}) — clock skew or forged token",
            claims.iat,
            IAT_CLOCK_SKEW_TOLERANCE_SECS,
            now_unix_secs
        );
    }
    Ok(())
}

/// Builds a `TokenState` from a raw SELF JWT string. Combines parse +
/// validate + IST timestamp conversion.
///
/// # Errors
///
/// Returns `anyhow::Error` if parse or validate fails.
pub fn build_token_state_from_self_jwt(
    jwt: &str,
    expected_client_id: &str,
    min_remaining_secs: i64,
) -> Result<TokenState> {
    let claims = parse_self_jwt(jwt)?;
    let now = Utc::now().timestamp();
    validate_self_claims(&claims, expected_client_id, min_remaining_secs, now)?;
    let ist = ist_offset();
    let expires_at: DateTime<FixedOffset> = ist
        .timestamp_opt(claims.exp, 0)
        .single()
        .ok_or_else(|| anyhow!("invalid exp timestamp: {}", claims.exp))?;
    let issued_at: DateTime<FixedOffset> = ist
        .timestamp_opt(claims.iat, 0)
        .single()
        .ok_or_else(|| anyhow!("invalid iat timestamp: {}", claims.iat))?;
    Ok(TokenState::from_cached(
        SecretString::from(jwt.to_string()),
        expires_at,
        issued_at,
    ))
}

/// Owner of the depth-200 SELF token's `TokenHandle`. Backed by AWS
/// SSM Parameter Store — no local cache. Renewal task runs every
/// ~23h.
pub struct Depth200SelfTokenManager {
    handle: TokenHandle,
    ssm_client: SsmClient,
    ssm_parameter_name: String,
    expected_client_id: String,
    renewal_interval: Duration,
    min_remaining_secs_at_boot: i64,
    http_client: reqwest::Client,
    rest_api_base_url: String,
}

impl Depth200SelfTokenManager {
    /// Boot from SSM. Fetches the SELF JWT via `GetParameter`, validates
    /// it's a SELF token for the expected client_id with sufficient
    /// remaining validity, and seeds the `TokenHandle`.
    ///
    /// Does NOT spawn the renewal loop. Wrap in `Arc` and call
    /// `spawn_renewal_task()` separately.
    ///
    /// # Errors
    ///
    /// Returns `anyhow::Error` if SSM is unreachable, the parameter
    /// is missing, or the JWT fails validation. Operator runbook is
    /// keyed on the error text.
    #[instrument(skip_all, fields(ssm_param = %config.ssm_parameter_name))]
    // TEST-EXEMPT: requires real AWS SSM credentials + network — covered by manual operator test on Mac and (future) integration test against a localstack mock.
    pub async fn boot_from_ssm(
        config: &Depth200AuthConfig,
        expected_client_id: String,
        rest_api_base_url: String,
    ) -> Result<Self> {
        if !config.is_manual_self_mode() {
            bail!(
                "Depth200SelfTokenManager::boot_from_ssm called with mode={} — only valid for \"manual_self_with_renewal\"",
                config.mode
            );
        }
        let ssm_client = create_ssm_client().await;
        let jwt = fetch_ssm_secure_string(&ssm_client, &config.ssm_parameter_name)
            .await
            .with_context(|| {
                format!(
                    "failed to fetch SELF JWT from SSM at {}",
                    config.ssm_parameter_name
                )
            })?;
        #[allow(clippy::cast_possible_wrap)] // APPROVED: u64 secs fit in i64 for thousands of years
        let min_remaining = config.min_remaining_secs_at_boot as i64;
        let token_state = build_token_state_from_self_jwt(
            jwt.expose_secret(),
            &expected_client_id,
            min_remaining,
        )?;
        info!(
            ssm_param = %config.ssm_parameter_name,
            client_id = %expected_client_id,
            expires_at = %token_state.expires_at(),
            "depth-200 SELF token loaded from SSM"
        );
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(Some(token_state))));
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(RENEW_TOKEN_HTTP_TIMEOUT_SECS))
            .build()
            .context("failed to build reqwest::Client for depth-200 RenewToken")?;
        Ok(Self {
            handle,
            ssm_client,
            ssm_parameter_name: config.ssm_parameter_name.clone(),
            expected_client_id,
            renewal_interval: Duration::from_secs(config.renewal_interval_secs),
            min_remaining_secs_at_boot: min_remaining,
            http_client,
            rest_api_base_url,
        })
    }

    /// Returns the `TokenHandle` for the depth-200 connection to
    /// consume. Same `Arc<ArcSwap<Option<TokenState>>>` type as the
    /// existing TOTP/APP `TokenManager` exposes, so
    /// `run_two_hundred_depth_connection` does not need any signature
    /// changes.
    #[must_use]
    // TEST-EXEMPT: trivial getter for an internal Arc — exercised transitively by every depth-200 connection that calls it.
    pub fn handle(&self) -> &TokenHandle {
        &self.handle
    }

    /// Spawns the background renewal task. Loops every
    /// `renewal_interval`, calls `GET /v2/RenewToken` with the current
    /// SELF JWT, validates the response is still a SELF token, writes
    /// it back to SSM via `PutParameter`, and atomic-swaps the new
    /// `TokenState` into the handle.
    ///
    /// On error: logs ERROR with `code = ErrorCode::*` (set by the
    /// caller via the notification service when wired into main.rs),
    /// retries on the next interval. Caller is responsible for any
    /// Telegram alerting.
    // TEST-EXEMPT: spawns a long-lived tokio task with HTTP + SSM I/O — covered by manual operator test and future integration test against mocked Dhan + localstack.
    pub fn spawn_renewal_task(self: Arc<Self>) -> JoinHandle<()> {
        let manager = Arc::clone(&self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(manager.renewal_interval).await;
                match manager.renew_once().await {
                    Ok(new_state) => {
                        manager.handle.store(Arc::new(Some(new_state)));
                        info!(
                            ssm_param = %manager.ssm_parameter_name,
                            "depth-200 SELF token renewed and atomic-swapped"
                        );
                    }
                    Err(err) => {
                        error!(
                            error = %err,
                            ssm_param = %manager.ssm_parameter_name,
                            "depth-200 SELF token renewal failed — operator must paste a fresh SELF token at the SSM path before token expires"
                        );
                    }
                }
            }
        })
    }

    /// Single renewal attempt: HTTP GET RenewToken → parse → validate
    /// → write back to SSM. Pure async, no retries (caller's loop
    /// retries on the next interval).
    async fn renew_once(&self) -> Result<TokenState> {
        let current_guard = self.handle.load();
        let current_state = current_guard
            .as_ref()
            .as_ref()
            .ok_or_else(|| anyhow!("depth-200 SELF token handle is empty — cannot renew"))?;
        let current_jwt = current_state.access_token().expose_secret().to_string();

        let url = format!(
            "{}/RenewToken",
            self.rest_api_base_url.trim_end_matches('/')
        );
        let response = self
            .http_client
            .get(&url)
            .header("access-token", &current_jwt)
            .header("dhanClientId", &self.expected_client_id)
            .send()
            .await
            .context("RenewToken HTTP request failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("RenewToken returned HTTP {} body={}", status, body);
        }

        #[derive(Deserialize)]
        struct RenewResponse {
            #[serde(rename = "accessToken")]
            access_token: String,
        }
        let parsed: RenewResponse = response
            .json()
            .await
            .context("RenewToken response JSON parse failed")?;

        // Renewed token must STILL be SELF type. If Dhan's RenewToken
        // strips the consumer type, this is a Dhan-side change we need
        // to know about immediately.
        let new_state = build_token_state_from_self_jwt(
            &parsed.access_token,
            &self.expected_client_id,
            self.min_remaining_secs_at_boot,
        )
        .context(
            "renewed token failed SELF validation — Dhan may have changed RenewToken semantics",
        )?;

        self.write_back_to_ssm(&parsed.access_token)
            .await
            .context("failed to PutParameter back to SSM after RenewToken")?;
        Ok(new_state)
    }

    /// Overwrites the SSM SecureString with the new SELF JWT.
    async fn write_back_to_ssm(&self, jwt: &str) -> Result<()> {
        self.ssm_client
            .put_parameter()
            .name(&self.ssm_parameter_name)
            .value(jwt)
            .r#type(aws_sdk_ssm::types::ParameterType::SecureString)
            .overwrite(true)
            .send()
            .await
            .map_err(|err| anyhow!("PutParameter failed: {err}"))?;
        Ok(())
    }
}

/// Internal SSM GET helper. `secret_manager::fetch_secret` is
/// `pub(crate)` but we keep this local copy so this module is
/// self-contained for unit testability.
async fn fetch_ssm_secure_string(client: &SsmClient, path: &str) -> Result<SecretString> {
    let response = client
        .get_parameter()
        .name(path)
        .with_decryption(true)
        .send()
        .await
        .map_err(|err| anyhow!("GetParameter failed for {path}: {err}"))?;
    let parameter = response
        .parameter()
        .ok_or_else(|| anyhow!("GetParameter returned no parameter for {path}"))?;
    let value = parameter
        .value()
        .ok_or_else(|| anyhow!("GetParameter returned no value for {path}"))?;
    Ok(SecretString::from(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a syntactically-valid JWT (header.payload.signature) for
    /// tests. Signature is fixed garbage — callers that don't verify
    /// signatures (we don't) accept this.
    fn make_test_jwt(consumer_type: &str, client_id: &str, exp: i64, iat: i64) -> String {
        // base64url-encode helper for tests.
        fn b64url_encode(bytes: &[u8]) -> String {
            const ALPHA: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = Vec::new();
            for chunk in bytes.chunks(3) {
                let n = chunk.len();
                let b0 = chunk[0];
                let b1 = if n > 1 { chunk[1] } else { 0 };
                let b2 = if n > 2 { chunk[2] } else { 0 };
                out.push(ALPHA[(b0 >> 2) as usize]);
                out.push(ALPHA[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize]);
                if n > 1 {
                    out.push(ALPHA[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize]);
                }
                if n > 2 {
                    out.push(ALPHA[(b2 & 0b111111) as usize]);
                }
            }
            String::from_utf8(out).expect("base64url alphabet is ASCII") // APPROVED: test
        }

        let header = b64url_encode(br#"{"typ":"JWT","alg":"HS512"}"#);
        let payload = format!(
            r#"{{"iss":"dhan","partnerId":"","exp":{exp},"iat":{iat},"tokenConsumerType":"{consumer_type}","webhookUrl":"","dhanClientId":"{client_id}"}}"#
        );
        let payload_b64 = b64url_encode(payload.as_bytes());
        let signature = "FAKE_SIGNATURE_FOR_TESTS_ONLY";
        format!("{header}.{payload_b64}.{signature}")
    }

    #[test]
    fn test_base64url_decode_known_vector() {
        // RFC 4648 §10: "Man" (3 bytes) → "TWFu"
        assert_eq!(base64url_decode("TWFu").unwrap(), b"Man"); // APPROVED: test
        // "Ma" (2 bytes) → "TWE" (no padding needed)
        assert_eq!(base64url_decode("TWE").unwrap(), b"Ma"); // APPROVED: test
        // URL-safe characters: bytes 0xfb 0xff → "-_8"... actually let's keep simple
        // Empty input → empty output
        assert_eq!(base64url_decode("").unwrap(), Vec::<u8>::new()); // APPROVED: test
    }

    #[test]
    fn test_base64url_decode_rejects_invalid_char() {
        assert!(base64url_decode("Ma!").is_err());
    }

    #[test]
    fn test_parse_self_jwt_extracts_known_claims() {
        let jwt = make_test_jwt("SELF", "1106656882", 1_777_457_811, 1_777_371_411);
        let claims = parse_self_jwt(&jwt).expect("parse must succeed"); // APPROVED: test
        assert_eq!(claims.token_consumer_type, "SELF");
        assert_eq!(claims.dhan_client_id, "1106656882");
        assert_eq!(claims.exp, 1_777_457_811);
        assert_eq!(claims.iat, 1_777_371_411);
    }

    #[test]
    fn test_parse_self_jwt_distinguishes_app_from_self() {
        let jwt = make_test_jwt("APP", "1106656882", 1_777_457_811, 1_777_371_411);
        let claims = parse_self_jwt(&jwt).expect("parse should still succeed"); // APPROVED: test
        // Parser doesn't reject — it surfaces the value. The validator rejects.
        assert_eq!(claims.token_consumer_type, "APP");
    }

    #[test]
    fn test_parse_self_jwt_rejects_two_segment_token() {
        assert!(parse_self_jwt("only.two").is_err());
    }

    #[test]
    fn test_parse_self_jwt_rejects_four_segment_token() {
        assert!(parse_self_jwt("a.b.c.d").is_err());
    }

    #[test]
    fn test_parse_self_jwt_rejects_empty() {
        assert!(parse_self_jwt("").is_err());
    }

    #[test]
    fn test_validate_self_claims_accepts_valid_token() {
        let claims = ParsedSelfClaims {
            token_consumer_type: "SELF".to_string(),
            exp: 2_000_000_000,
            iat: 1_999_900_000,
            dhan_client_id: "1106656882".to_string(),
        };
        assert!(validate_self_claims(&claims, "1106656882", 3_600, 1_999_990_000).is_ok());
    }

    #[test]
    fn test_validate_rejects_app_consumer_type() {
        let claims = ParsedSelfClaims {
            token_consumer_type: "APP".to_string(),
            exp: 2_000_000_000,
            iat: 1_999_900_000,
            dhan_client_id: "1106656882".to_string(),
        };
        let err = validate_self_claims(&claims, "1106656882", 3_600, 1_999_990_000)
            .expect_err("APP must be rejected"); // APPROVED: test
        let msg = format!("{err}");
        assert!(msg.contains("SELF"), "error must mention SELF: {msg}");
    }

    #[test]
    fn test_validate_rejects_wrong_client_id() {
        let claims = ParsedSelfClaims {
            token_consumer_type: "SELF".to_string(),
            exp: 2_000_000_000,
            iat: 1_999_900_000,
            dhan_client_id: "9999999999".to_string(),
        };
        assert!(validate_self_claims(&claims, "1106656882", 3_600, 1_999_990_000).is_err());
    }

    #[test]
    fn test_validate_rejects_token_with_too_little_remaining() {
        let claims = ParsedSelfClaims {
            token_consumer_type: "SELF".to_string(),
            exp: 1_999_991_000,
            iat: 1_999_900_000,
            dhan_client_id: "1106656882".to_string(),
        };
        // remaining = 1000s, min = 3600s → reject
        assert!(validate_self_claims(&claims, "1106656882", 3_600, 1_999_990_000).is_err());
    }

    #[test]
    fn test_validate_rejects_expired_token() {
        let claims = ParsedSelfClaims {
            token_consumer_type: "SELF".to_string(),
            exp: 1_999_980_000,
            iat: 1_999_900_000,
            dhan_client_id: "1106656882".to_string(),
        };
        // now > exp → negative remaining → reject
        assert!(validate_self_claims(&claims, "1106656882", 3_600, 1_999_990_000).is_err());
    }

    #[test]
    fn test_validate_rejects_future_iat_beyond_skew() {
        let claims = ParsedSelfClaims {
            token_consumer_type: "SELF".to_string(),
            exp: 2_000_000_000,
            iat: 1_999_990_500, // 500s in the future, > 60s skew
            dhan_client_id: "1106656882".to_string(),
        };
        assert!(validate_self_claims(&claims, "1106656882", 3_600, 1_999_990_000).is_err());
    }

    #[test]
    fn test_build_token_state_from_self_jwt_roundtrip() {
        let now = Utc::now().timestamp();
        let exp = now + 23 * 3600;
        let iat = now - 60;
        let jwt = make_test_jwt("SELF", "1106656882", exp, iat);
        let state = build_token_state_from_self_jwt(&jwt, "1106656882", 3_600)
            .expect("build must succeed for fresh SELF token"); // APPROVED: test
        assert_eq!(state.expires_at().timestamp(), exp);
        assert_eq!(state.issued_at().timestamp(), iat);
        // Token value preserved verbatim
        assert_eq!(state.access_token().expose_secret(), &jwt);
    }

    #[test]
    fn test_build_token_state_rejects_app_token() {
        let now = Utc::now().timestamp();
        let jwt = make_test_jwt("APP", "1106656882", now + 23 * 3600, now);
        assert!(build_token_state_from_self_jwt(&jwt, "1106656882", 3_600).is_err());
    }

    #[test]
    fn test_token_consumer_type_constants_are_distinct() {
        assert_ne!(TOKEN_CONSUMER_TYPE_SELF, TOKEN_CONSUMER_TYPE_APP);
        assert_eq!(TOKEN_CONSUMER_TYPE_SELF, "SELF");
        assert_eq!(TOKEN_CONSUMER_TYPE_APP, "APP");
    }
}
