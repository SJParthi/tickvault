//! Fast-boot cached-token validation (AUTH-GAP-06, 2026-07-08).
//!
//! # Why this exists (operator incident 2026-07-07 — third occurrence)
//!
//! The FAST crash-recovery boot arm (`crates/app/src/main.rs`) restarts
//! during market hours from a CACHED Dhan JWT and spawns the WebSocket
//! pool with ZERO validation of that token. Dhan enforces one active
//! token at a time (`authentication.md` rule 5), so a token minted
//! elsewhere (peer host, manual web login, the daily reset) silently
//! kills the cached one — and the feed sits dead until the mid-session
//! watchdog's ~30-minute AUTH-GAP-05 self-heal or manual intervention.
//! On 2026-07-07 that meant a dead feed from 08:32 to 09:06 IST, the
//! THIRD morning outage of this class.
//!
//! # Design
//!
//! ONE `GET /v2/profile` probe with the cached token, BEFORE any
//! WebSocket spawn:
//!
//! * **2xx** → [`CachedTokenVerdict::Proceed`] — boot continues untouched.
//! * **Prefix-anchored HTTP 401/403** → [`CachedTokenVerdict::Remint`] —
//!   the broker rejected OUR login; force a re-mint through the EXISTING
//!   `TokenManager` machinery (`initialize_deferred` + `force_renewal`).
//!   The RESILIENCE-03 in-flight mint tripwire is preserved; the fast arm
//!   passes `None` for the dual-instance lock flag — the documented §3
//!   residual of `dual-instance-lock-2026-07-04.md`, deliberately
//!   UNCHANGED here (no lock acquisition is added).
//! * **Everything else** (send-leg/network, REST-surface 400/429/5xx,
//!   parse, unknown) → [`CachedTokenVerdict::ProceedDegraded`] — retried
//!   ONCE after a short bound, then boot PROCEEDS with the cached token
//!   plus a loud `error!` (fail-open: a REST-surface outage with a
//!   healthy WS — the 2026-06-10 incident class — must neither block
//!   boot nor trigger a token-destroying mint; the F12 lesson).
//!
//! The probe produces the SAME failure-reason wrapper strings as
//! `TokenManager::get_user_profile` (the shared `PROFILE_*` constants
//! owned by `token_manager.rs`), and the classifier reuses the
//! mid-session watchdog's prefix-anchoring primitives (`reason_core` +
//! `http_response_status`) — never a substring scan of server-controlled
//! body text (SEC-R1-3).
//!
//! # Boundedness (boot must never hang)
//!
//! Every leg is timeout-bounded: the probe by the reqwest client's
//! request timeout, the retry by [`FAST_BOOT_PROFILE_RETRY_DELAY_SECS`],
//! and the remint's `TokenManager` construction by
//! `TOKEN_INIT_TIMEOUT_SECS`. A failed remint falls through LOUDLY to
//! the pre-existing fast-arm semantics (cached token + WS reconnect
//! machinery + the AUTH-GAP-05 watchdog backstop).
//!
//! Cold path only — one HTTP round-trip (+ optional retry) per fast
//! boot; nothing here touches the tick hot path.

use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret;
use tickvault_common::config::{DhanConfig, NetworkConfig, TokenConfig};
use tickvault_common::constants::{DHAN_USER_PROFILE_PATH, TOKEN_INIT_TIMEOUT_SECS};
use tickvault_common::error::ApplicationError;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::url_join::join_api_url;
use tracing::{error, info, warn};
use zeroize::Zeroizing;

use super::mid_session_watchdog::{http_response_status, reason_core};
use super::token_manager::{
    PROFILE_HTTP_RESPONSE_WRAPPER, PROFILE_SEND_LEG_WRAPPER, TokenHandle, TokenManager,
};
use crate::notification::service::NotificationService;

/// Short bound before the SINGLE degraded-probe retry. A transient DNS /
/// connection blip at crash-restart time usually clears within a couple
/// of seconds; anything longer must not delay crash recovery further —
/// boot proceeds degraded (loudly) instead.
pub const FAST_BOOT_PROFILE_RETRY_DELAY_SECS: u64 = 2;

/// Pure verdict of one cached-token `/v2/profile` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedTokenVerdict {
    /// 2xx — the cached token is alive; boot continues untouched.
    Proceed,
    /// Prefix-anchored HTTP 401/403 — the broker rejected OUR login
    /// (the 2026-07-07 incident shape). Force a re-mint BEFORE any
    /// WebSocket spawn.
    Remint,
    /// Any other failure shape — send-leg/network, REST-surface
    /// 400/429/5xx, parse error, unknown. None of these is evidence the
    /// TOKEN is bad, so boot proceeds with the cached token (loudly);
    /// minting on ambiguity can destroy a healthy token (F12) and risks
    /// the cross-host mint ping-pong.
    ProceedDegraded,
}

/// Pure. Classifies one profile-probe result into a [`CachedTokenVerdict`].
///
/// Prefix-anchored on the shared `get_user_profile` wrapper constants via
/// the mid-session watchdog's `reason_core` + `http_response_status`
/// primitives — the HTTP status token is produced by OUR `format!` before
/// any server body is appended, so a hostile body can forge neither an
/// escalation to `Remint` nor a downgrade to `ProceedDegraded`.
#[must_use]
pub fn classify_probe_result(result: &Result<(), ApplicationError>) -> CachedTokenVerdict {
    match result {
        Ok(()) => CachedTokenVerdict::Proceed,
        Err(err) => {
            let rendered = format!("{err}");
            let core = reason_core(&rendered);
            match http_response_status(core) {
                Some(401 | 403) => CachedTokenVerdict::Remint,
                // Any other HTTP status (400/429/5xx/WAF), the send-leg
                // wrapper, the parse wrapper, or an unknown shape: not
                // evidence the token is bad — never mint at boot on it.
                _ => CachedTokenVerdict::ProceedDegraded,
            }
        }
    }
}

/// One `GET /v2/profile` probe with the cached token.
///
/// Produces the SAME failure-reason wrapper strings as
/// `TokenManager::get_user_profile` (shared `PROFILE_*` constants) so
/// [`classify_probe_result`] anchors on one wrapper vocabulary. The body
/// is NOT parsed — a 2xx is sufficient evidence Dhan accepted the token
/// (dataPlan / activeSegment interpretation stays the slow path's
/// `pre_market_check` job; boot must not gain new blocking semantics).
// TEST-EXEMPT: requires a live HTTP server + valid token; the failure-shape
// classification it feeds is fully covered by the classify_probe_result
// unit tests below, and the endpoint wiring is pinned by the non-vacuous
// source-scan ratchet crates/app/tests/fast_boot_token_validation_wiring_guard.rs.
async fn probe_profile_once(
    http_client: &reqwest::Client,
    rest_api_base_url: &str,
    access_token: &Zeroizing<String>,
) -> Result<(), ApplicationError> {
    let url = join_api_url(rest_api_base_url, DHAN_USER_PROFILE_PATH);
    let response = http_client
        .get(&url)
        .header("access-token", access_token.as_str())
        .send()
        .await
        .map_err(|err| ApplicationError::AuthenticationFailed {
            reason: format!(
                "{PROFILE_SEND_LEG_WRAPPER} {}",
                redact_url_params(&err.to_string())
            ),
        })?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let captured_body = capture_rest_error_body(&body_text);
        let redacted_url = redact_url_params(&url);
        return Err(ApplicationError::AuthenticationFailed {
            reason: format!(
                "{PROFILE_HTTP_RESPONSE_WRAPPER}{status} url={redacted_url} body={captured_body}"
            ),
        });
    }
    Ok(())
}

/// Fast-boot cached-token validation — ONE profile check, then act.
///
/// Called by the FAST crash-recovery arm in `crates/app/src/main.rs`
/// AFTER the cached token is loaded and BEFORE the WS-GAP-08 cooldown
/// wait + `create_websocket_pool` (ordering pinned by the
/// `fast_boot_token_validation_wiring_guard` ratchet).
///
/// Returns `Some(TokenManager)` when the Remint path constructed one (the
/// caller's deferred-auth background arm reuses it instead of a duplicate
/// `initialize_deferred`); `None` on Proceed / ProceedDegraded / a failed
/// manager construction. NEVER blocks boot beyond its bounded legs and
/// NEVER returns an error — every failure degrades loudly to the
/// pre-existing fast-arm semantics.
// The decision surface is the unit-tested pure classify_probe_result; the
// main.rs ordering + endpoint/remint wiring are pinned by the non-vacuous
// ratchets in crates/app/tests/fast_boot_token_validation_wiring_guard.rs.
// TEST-EXEMPT: network-bound orchestrator (live Dhan REST + AWS SSM on the remint leg)
pub async fn validate_cached_token_at_fast_boot(
    token_handle: &TokenHandle,
    cached_client_id: &str,
    dhan_config: &DhanConfig,
    token_config: &TokenConfig,
    network_config: &NetworkConfig,
    notifier: &Arc<NotificationService>,
) -> Option<Arc<TokenManager>> {
    // --- Cached token out of the handle (Zeroizing so the plaintext copy
    // is wiped when this fn returns). ---
    let access_token: Zeroizing<String> = {
        let guard = token_handle.load();
        match guard.as_ref().as_ref() {
            Some(state) => Zeroizing::new(state.access_token().expose_secret().to_string()),
            None => {
                // Unreachable on the fast arm by construction (the arm is
                // gated on a valid cache) — degrade loudly, never block.
                error!(
                    code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                    severity = ErrorCode::AuthGap06FastBootCachedTokenValidation
                        .severity()
                        .as_str(),
                    "AUTH-GAP-06: no cached token in the handle at fast-boot validation — \
                     proceeding without validation (existing fast-arm semantics)"
                );
                metrics::counter!("tv_fast_boot_token_validation_total", "outcome" => "degraded")
                    .increment(1);
                return None;
            }
        }
    };

    // --- Dedicated bounded probe client (redirect none — the SEC-C2-2 /
    // §18 precedent: a 3xx must surface as an ordinary non-2xx RESPONSE,
    // never as a send-leg error carrying a server-controlled URL). ---
    let http_client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(network_config.request_timeout_ms))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            // HTTP-CLIENT-01 lesson: a builder failure means host
            // fd/TLS/resolver pressure — degrade the single probe, never
            // panic, never block boot.
            error!(
                code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                severity = ErrorCode::AuthGap06FastBootCachedTokenValidation
                    .severity()
                    .as_str(),
                error = %err,
                "AUTH-GAP-06: probe HTTP client build failed — proceeding with the \
                 cached token unvalidated (fail-open)"
            );
            metrics::counter!("tv_fast_boot_token_validation_total", "outcome" => "degraded")
                .increment(1);
            return None;
        }
    };

    // --- ONE profile check (with a single bounded retry on a degraded
    // first probe — a crash-restart transient usually clears in seconds). ---
    let first =
        probe_profile_once(&http_client, &dhan_config.rest_api_base_url, &access_token).await;
    let mut verdict = classify_probe_result(&first);
    if verdict == CachedTokenVerdict::ProceedDegraded {
        warn!(
            code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
            reason = %first
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default(),
            retry_delay_secs = FAST_BOOT_PROFILE_RETRY_DELAY_SECS,
            "AUTH-GAP-06: cached-token probe degraded (transient/REST-surface) — \
             retrying once before proceeding"
        );
        tokio::time::sleep(Duration::from_secs(FAST_BOOT_PROFILE_RETRY_DELAY_SECS)).await;
        let second =
            probe_profile_once(&http_client, &dhan_config.rest_api_base_url, &access_token).await;
        verdict = classify_probe_result(&second);
        if verdict == CachedTokenVerdict::ProceedDegraded {
            // Fail-open: a REST-surface outage (the 2026-06-10 class —
            // REST dead, WS healthy) must neither block boot nor mint.
            error!(
                code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                severity = ErrorCode::AuthGap06FastBootCachedTokenValidation
                    .severity()
                    .as_str(),
                reason = %second
                    .as_ref()
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default(),
                "AUTH-GAP-06: cached-token validation DEGRADED after retry — \
                 PROCEEDING with the cached token (WS reconnect + the AUTH-GAP-05 \
                 mid-session watchdog remain the backstops)"
            );
            metrics::counter!("tv_fast_boot_token_validation_total", "outcome" => "degraded")
                .increment(1);
            return None;
        }
    }

    match verdict {
        CachedTokenVerdict::Proceed => {
            info!(
                "AUTH-GAP-06: cached token validated against /v2/profile — \
                 fast boot proceeds (one check, no re-mint needed)"
            );
            metrics::counter!("tv_fast_boot_token_validation_total", "outcome" => "proceed")
                .increment(1);
            None
        }
        CachedTokenVerdict::Remint => {
            // The broker rejected the cached token (prefix-anchored
            // 401/403) — the exact 2026-07-07 dead-cached-token shape.
            error!(
                code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                severity = ErrorCode::AuthGap06FastBootCachedTokenValidation
                    .severity()
                    .as_str(),
                "AUTH-GAP-06: cached token REJECTED by Dhan (401/403) at fast boot — \
                 forcing a re-mint via the existing TokenManager machinery BEFORE \
                 any WebSocket spawn"
            );
            metrics::counter!(
                "tv_fast_boot_token_validation_total",
                "outcome" => "remint_triggered"
            )
            .increment(1);

            // Existing machinery: SSM fetch + client_id check + shared
            // TokenHandle. The fast arm holds NO dual-instance lock — pass
            // `None` per dual-instance-lock-2026-07-04.md §3 (documented
            // residual; the RESILIENCE-03 in-flight tripwire inside
            // force_renewal is preserved for lock-carrying constructions).
            let manager = match tokio::time::timeout(
                Duration::from_secs(TOKEN_INIT_TIMEOUT_SECS),
                TokenManager::initialize_deferred(
                    Arc::clone(token_handle),
                    cached_client_id,
                    dhan_config,
                    token_config,
                    network_config,
                    notifier,
                    None,
                ),
            )
            .await
            {
                Ok(Ok(manager)) => manager,
                Ok(Err(err)) => {
                    error!(
                        code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                        severity = ErrorCode::AuthGap06FastBootCachedTokenValidation
                            .severity()
                            .as_str(),
                        error = %err,
                        "AUTH-GAP-06: TokenManager construction for the forced re-mint \
                         FAILED — proceeding with the cached token (fast-arm failure \
                         semantics unchanged; the deferred background arm will retry)"
                    );
                    metrics::counter!(
                        "tv_fast_boot_token_validation_total",
                        "outcome" => "remint_unavailable"
                    )
                    .increment(1);
                    return None;
                }
                Err(_elapsed) => {
                    error!(
                        code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                        severity = ErrorCode::AuthGap06FastBootCachedTokenValidation
                            .severity()
                            .as_str(),
                        timeout_secs = TOKEN_INIT_TIMEOUT_SECS,
                        "AUTH-GAP-06: TokenManager construction for the forced re-mint \
                         TIMED OUT — proceeding with the cached token (boot never hangs)"
                    );
                    metrics::counter!(
                        "tv_fast_boot_token_validation_total",
                        "outcome" => "remint_unavailable"
                    )
                    .increment(1);
                    return None;
                }
            };

            // NOTE: initialize_deferred may ALREADY have re-minted on a
            // client_id mismatch; force_renewal is idempotent-safe here
            // (RenewToken → generateAccessToken fallback) and lands the
            // fresh token in the SHARED handle the WS pool reads.
            match manager.force_renewal().await {
                Ok(()) => {
                    info!(
                        code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                        "AUTH-GAP-06: forced re-mint OK — WebSocket pool will spawn \
                         with the fresh token"
                    );
                    metrics::counter!(
                        "tv_fast_boot_token_validation_total",
                        "outcome" => "remint_ok"
                    )
                    .increment(1);
                }
                Err(err) => {
                    error!(
                        code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str(),
                        severity = ErrorCode::AuthGap06FastBootCachedTokenValidation
                            .severity()
                            .as_str(),
                        error = %err,
                        "AUTH-GAP-06: forced re-mint FAILED — proceeding with the \
                         cached token; existing fast-arm failure semantics apply \
                         (WS reconnect + AUTH-GAP-05 watchdog backstop)"
                    );
                    metrics::counter!(
                        "tv_fast_boot_token_validation_total",
                        "outcome" => "remint_failed"
                    )
                    .increment(1);
                }
            }
            // Return the manager either way — it is fully constructed
            // (SSM-validated), so the deferred background arm reuses it
            // instead of a duplicate initialize_deferred.
            Some(manager)
        }
        CachedTokenVerdict::ProceedDegraded => {
            // Unreachable: the degraded arm returned above after the
            // retry. Kept exhaustive without panics (house rule: no
            // unreachable!()).
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_err(reason: &str) -> Result<(), ApplicationError> {
        Err(ApplicationError::AuthenticationFailed {
            reason: reason.to_string(),
        })
    }

    #[test]
    fn classify_probe_result_ok_is_proceed() {
        let r: Result<(), ApplicationError> = Ok(());
        assert_eq!(classify_probe_result(&r), CachedTokenVerdict::Proceed);
    }

    #[test]
    fn classify_probe_result_http_401_is_remint() {
        // The 2026-07-07 incident shape: Dhan killed yesterday's cached
        // token; the profile probe returns 401.
        let r = auth_err(
            "profile request HTTP 401 Unauthorized url=https://api.dhan.co/v2/profile \
             body={\"errorType\":\"Invalid_Authentication\"}",
        );
        assert_eq!(classify_probe_result(&r), CachedTokenVerdict::Remint);
        // Display-prefixed rendering (how the orchestrator sees it after
        // format!("{err}")) must classify identically.
        let r = auth_err("profile request HTTP 401 Unauthorized");
        let rendered_via_display = classify_probe_result(&r);
        assert_eq!(rendered_via_display, CachedTokenVerdict::Remint);
    }

    #[test]
    fn classify_probe_result_http_403_is_remint() {
        let r = auth_err("profile request HTTP 403 Forbidden url=x body=y");
        assert_eq!(classify_probe_result(&r), CachedTokenVerdict::Remint);
    }

    #[test]
    fn classify_probe_result_http_5xx_is_proceed_degraded() {
        // REST surface down ≠ token dead (2026-06-10 all-day incident
        // class) — must NEVER mint, must NEVER block boot.
        for status_line in ["500 Internal Server Error", "503 Service Unavailable"] {
            let r = auth_err(&format!("profile request HTTP {status_line} url=x body=y"));
            assert_eq!(
                classify_probe_result(&r),
                CachedTokenVerdict::ProceedDegraded,
                "{status_line} must proceed degraded, never mint"
            );
        }
    }

    #[test]
    fn classify_probe_result_http_400_and_429_are_proceed_degraded() {
        for status_line in ["400 Bad Request", "429 Too Many Requests"] {
            let r = auth_err(&format!("profile request HTTP {status_line} url=x body=y"));
            assert_eq!(
                classify_probe_result(&r),
                CachedTokenVerdict::ProceedDegraded
            );
        }
    }

    #[test]
    fn classify_probe_result_send_leg_is_proceed_degraded() {
        // No response was ever received — can never implicate the token.
        let r = auth_err(
            "profile request failed: error sending request for url \
             (https://api.dhan.co/v2/profile): dns error: failed to lookup \
             address information",
        );
        assert_eq!(
            classify_probe_result(&r),
            CachedTokenVerdict::ProceedDegraded
        );
    }

    #[test]
    fn classify_probe_result_parse_error_is_proceed_degraded() {
        let r = auth_err("profile response parse error: expected value at line 1 column 1");
        assert_eq!(
            classify_probe_result(&r),
            CachedTokenVerdict::ProceedDegraded
        );
    }

    #[test]
    fn classify_probe_result_unknown_shape_is_proceed_degraded() {
        // Boot-time fail-safe is the OPPOSITE direction of the watchdog's
        // RealAuthFail default: a mint on an unknown shape can destroy a
        // healthy token with no consecutive-failure tempering, so unknown
        // shapes proceed degraded (loud) instead.
        let r = auth_err("some brand-new failure wording this classifier has never seen");
        assert_eq!(
            classify_probe_result(&r),
            CachedTokenVerdict::ProceedDegraded
        );
    }

    /// SEC-R1-3 in both directions: the status is parsed from the fixed
    /// position after OUR wrapper prefix — a hostile server body can
    /// neither downgrade a 401 (Remint stays Remint) nor escalate a 503
    /// by planting "HTTP 401" in the body.
    #[test]
    fn classify_probe_result_hostile_401_body_cannot_forge_degraded() {
        let r = auth_err(
            "profile request HTTP 401 Unauthorized url=https://api.dhan.co/v2/profile \
             body=profile request failed: connection reset",
        );
        assert_eq!(classify_probe_result(&r), CachedTokenVerdict::Remint);

        let r = auth_err(
            "profile request HTTP 503 Service Unavailable url=x \
             body=profile request HTTP 401 Unauthorized",
        );
        assert_eq!(
            classify_probe_result(&r),
            CachedTokenVerdict::ProceedDegraded,
            "a body-planted 401 must not escalate a 503 to a mint"
        );
    }

    /// The Display prefix (`Dhan authentication failed: `) that the
    /// orchestrator's `format!("{err}")` rendering prepends must not
    /// defeat the prefix anchoring.
    #[test]
    fn classify_probe_result_display_prefixed_401_is_remint() {
        let rendered = format!(
            "{}",
            ApplicationError::AuthenticationFailed {
                reason: "profile request HTTP 401 Unauthorized".to_string(),
            }
        );
        // classify_probe_result renders internally; feed it the raw error.
        let r = auth_err("profile request HTTP 401 Unauthorized");
        assert_eq!(classify_probe_result(&r), CachedTokenVerdict::Remint);
        // And the rendered form is what reason_core strips — sanity-pin it.
        assert!(rendered.starts_with("Dhan authentication failed: "));
    }

    #[test]
    fn retry_delay_constant_is_short() {
        // The retry must not meaningfully delay crash recovery.
        const _: () = assert!(FAST_BOOT_PROFILE_RETRY_DELAY_SECS >= 1);
        const _: () = assert!(FAST_BOOT_PROFILE_RETRY_DELAY_SECS <= 10);
    }
}
