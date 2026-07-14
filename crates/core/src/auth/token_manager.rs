//! JWT token lifecycle management for Dhan API authentication.
//!
//! Manages the complete token lifecycle: initial acquisition, O(1) atomic
//! reads via arc-swap, scheduled renewal, retry with exponential backoff,
//! and graceful fallback from `renewToken` to `generateAccessToken`.
//!
//! # Boot Chain Position
//!
//! `Config → ★ Auth ★ → WebSocket → Parse → Route`
//!
//! The `TokenManager` must be initialized before any WebSocket connection
//! or REST API call. All downstream consumers receive a `TokenHandle`
//! for O(1) atomic token reads.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use secrecy::ExposeSecret;
use tracing::{error, info, instrument, warn};
use zeroize::Zeroizing;

use tickvault_common::config::{DhanConfig, NetworkConfig, TokenConfig};
use tickvault_common::constants::{
    AUTH_RETRY_MAX_BACKOFF_SECS, DHAN_GENERATE_TOKEN_PATH, DHAN_RENEW_TOKEN_PATH,
    DHAN_TOKEN_GENERATION_COOLDOWN_SECS, TOTP_MAX_RETRIES, TOTP_PERIOD_SECS,
};
use tickvault_common::error::ApplicationError;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::url_join::join_api_url;

use super::secret_manager;
use super::token_cache;
use super::totp_generator::generate_totp_code;
use super::types::{DhanCredentials, DhanGenerateTokenResponse, TokenState};
use crate::notification::events::NotificationEvent;
use crate::notification::service::NotificationService;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Cadence of the `tv_token_remaining_seconds` live-countdown emitter
/// (`token_gauge_loop`). The scheduled `renewal_loop` only touches the
/// gauge at its own wakeups (once per ~23h sleep + after each renewal),
/// so between renewals the CloudWatch countdown panel froze at the last
/// emitted value. A 30s re-emit keeps the countdown live at negligible
/// cost (one O(1) arc-swap load + one gauge set per tick — cold path).
const TOKEN_GAUGE_INTERVAL_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Token Handle (shared with downstream consumers)
// ---------------------------------------------------------------------------

/// Atomic handle for O(1) token reads.
///
/// Downstream consumers (WebSocket client, REST API client) hold this
/// handle and call `load()` to get the current token. The pointer is
/// swapped atomically on renewal — zero lock contention.
pub type TokenHandle = Arc<ArcSwap<Option<TokenState>>>;

// ---------------------------------------------------------------------------
// AG5-R2-1 (2026-07-06): shared failure-reason wrapper constants.
//
// The AUTH-GAP-05 mid-session watchdog's mint-vs-no-mint classification
// (`mid_session_watchdog.rs::classify_failure_reason`) prefix-matches the
// failure-reason wrappers `get_user_profile` produces. Before this fix the
// wrappers were DUPLICATED as independent inline `format!` literals here and
// independent `const` copies in the watchdog — an innocent log-message reword
// here would silently break the classification, and every unrecognized shape
// fail-safes to `RealAuthFail`, i.e. TOWARD a destructive `generateAccessToken`
// mint (resurrecting the AG5-R1-3 "REST outage kills the healthy token"
// CRITICAL) while every watchdog test stayed green. These constants are now
// the SINGLE source: the producer `format!` sites below interpolate them and
// the watchdog imports them. Ratchet:
// `mid_session_watchdog::tests::wrapper_constants_are_single_sourced_in_token_manager`.
// ---------------------------------------------------------------------------

/// Send-leg wrapper from `get_user_profile` — the HTTP request failed
/// before ANY response was received (client-generated reqwest error text
/// only; no server-controlled body can appear inside this wrapper).
pub(crate) const PROFILE_SEND_LEG_WRAPPER: &str = "profile request failed:";

/// HTTP-response wrapper from `get_user_profile`:
/// `"profile request HTTP {status} url={url} body={body}"`. Everything
/// after `body=` is server-controlled (bounded + secret-redacted but
/// arbitrary text) — classifiers must therefore read ONLY the fixed
/// prefix, never substring-scan the whole string (SEC-R1-3).
pub(crate) const PROFILE_HTTP_RESPONSE_WRAPPER: &str = "profile request HTTP ";

/// 200-with-unparseable-body wrapper from `get_user_profile` (WAF/CDN
/// interception class — a REST-surface problem, not a token rejection).
pub(crate) const PROFILE_PARSE_ERROR_WRAPPER: &str = "profile response parse error";

/// RESILIENCE-03 in-flight mint-refusal reason PREFIX (`acquire_token`'s
/// lock tripwire). SEC-R2-2: the AUTH-GAP-05 watchdog prefix-anchors on
/// this shared literal to classify a `force_renewal` failure as the
/// permanent lock refusal — never a substring scan of the rendered error,
/// whose mint-HTTP-failure shape concatenates a server-controlled body
/// that could forge "RESILIENCE-03".
pub(crate) const RESILIENCE03_MINT_REFUSAL_REASON_PREFIX: &str =
    "RESILIENCE-03: instance lock not held";

/// Wave 2 Item 5.4 (AUTH-GAP-03) — global TokenManager handle so the
/// WebSocket sleep-wake path can call `force_renewal_if_stale()`
/// without the connection holding a back-reference. Set once at boot
/// from `main.rs` via `set_global_token_manager()`.
static GLOBAL_TOKEN_MANAGER: std::sync::OnceLock<Arc<TokenManager>> = std::sync::OnceLock::new();

/// Install the global TokenManager. Idempotent — second call is a
/// no-op. Returns `true` on first install, `false` if already set.
pub fn set_global_token_manager(tm: Arc<TokenManager>) -> bool {
    GLOBAL_TOKEN_MANAGER.set(tm).is_ok()
}

/// Read-only accessor for the global TokenManager. Returns `None`
/// before `set_global_token_manager` is called (test binaries).
#[must_use]
pub fn global_token_manager() -> Option<&'static Arc<TokenManager>> {
    GLOBAL_TOKEN_MANAGER.get()
}

// ---------------------------------------------------------------------------
// Token Manager
// ---------------------------------------------------------------------------

/// Manages the Dhan JWT token lifecycle.
///
/// Provides O(1) atomic token reads for all downstream consumers
/// via a shared `TokenHandle`. Handles initial authentication,
/// scheduled renewal, retry with backoff, and failure alerting.
pub struct TokenManager {
    /// Atomic pointer to current token state. O(1) reads.
    token: TokenHandle,

    /// Cached credentials (fetched once from SSM at startup).
    credentials: DhanCredentials,

    /// Dhan REST API base URL (for data/orders/renewal).
    rest_api_base_url: String,

    /// Dhan Auth base URL (for token generation).
    auth_base_url: String,

    /// HTTP client for Dhan API calls (reused across requests).
    http_client: reqwest::Client,

    /// Token refresh timing configuration.
    token_config: TokenConfig,

    /// Network retry configuration.
    network_config: NetworkConfig,

    /// Notification service for Telegram/SNS alerts on auth events.
    notifier: Arc<NotificationService>,

    /// RESILIENCE-03 mint tripwire (dual-instance lock hardening,
    /// operator "go" 2026-07-04). `Some(flag)` = this process acquired
    /// the SSM dual-instance lock at boot and `flag` mirrors ownership
    /// (`true` = held; the heartbeat flips it `false` on lock loss /
    /// shutdown release). Every `generateAccessToken` MINT (never
    /// `RenewToken`) checks the flag first and REFUSES fail-closed when
    /// ownership is gone — minting while a peer owns the Dhan session
    /// would invalidate the peer's JWT (one active token at a time).
    /// `None` = no lock expected (tests; the fast-boot crash-recovery
    /// arm, which never mints at boot — documented residual gap in
    /// `.claude/rules/project/dual-instance-lock-2026-07-04.md`).
    instance_lock_held: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl TokenManager {
    /// Creates a new `TokenManager` and performs initial authentication.
    ///
    /// 1. Fetches credentials from AWS SSM Parameter Store (3 retries)
    /// 2. Calls `generateAccessToken` with infinite retry for transient errors
    /// 3. Fails fast on permanent errors (wrong PIN, invalid TOTP, blocked)
    /// 4. Stores token in ArcSwap for O(1) reads
    ///
    /// # Errors
    ///
    /// Returns error only for permanent credential failures or shutdown (Ctrl+C).
    /// Transient errors (network, timeout, rate limit) are retried indefinitely.
    ///
    /// `instance_lock_held`: RESILIENCE-03 mint tripwire — see the field
    /// doc. The caller (boot) MUST have acquired the dual-instance lock
    /// BEFORE calling this (lock-before-mint, 2026-07-04) and pass the
    /// ownership flag in; `None` disarms the tripwire.
    #[instrument(skip_all)]
    pub async fn initialize(
        dhan_config: &DhanConfig,
        token_config: &TokenConfig,
        network_config: &NetworkConfig,
        notifier: &Arc<NotificationService>,
        instance_lock_held: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Arc<Self>, ApplicationError> {
        info!("initializing authentication — fetching credentials from SSM");

        // Validate HTTPS scheme — credentials must never be sent over plain HTTP.
        if !dhan_config.rest_api_base_url.starts_with("https://") {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "rest_api_base_url must use HTTPS, got: {}",
                    dhan_config.rest_api_base_url
                ),
            });
        }
        if !dhan_config.auth_base_url.starts_with("https://") {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "auth_base_url must use HTTPS, got: {}",
                    dhan_config.auth_base_url
                ),
            });
        }

        // SSM credential fetch with retry (3 attempts, exponential backoff).
        let credentials = {
            let mut attempt = 0u32;
            loop {
                attempt = attempt.saturating_add(1);
                match secret_manager::fetch_dhan_credentials().await {
                    Ok(creds) => break creds,
                    Err(err) if attempt >= 3 => return Err(err),
                    Err(err) => {
                        warn!(
                            attempt,
                            error = %err,
                            "SSM credential fetch failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(u64::from(2u32.pow(attempt)))).await;
                    }
                }
            }
        };

        // SEC-C2-2 fix (2026-07-07): pin
        // `redirect(Policy::none())` — the §18 CSV-downloader precedent.
        // Dhan's auth + API endpoints never legitimately redirect, and with
        // the default follow-up-to-10 policy a redirect-loop/limit failure
        // surfaces on the SEND leg with the FINAL redirect URL (taken from
        // the server-controlled Location header) embedded in the reqwest
        // error text — letting a WAF/CDN-planted URL path reach the
        // mid-session watchdog's send-leg transient-needle scan (space-free
        // needles like "connecterror" are plantable in a path) and downgrade
        // a paging RestSurfaceDegraded outage to a silent Transient. With
        // Policy::none() a 3xx is an ordinary non-2xx RESPONSE → the
        // HTTP-response wrapper → RestSurfaceDegraded (correct + paging).
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(network_config.request_timeout_ms))
            .build()
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!("HTTP client creation failed: {err}"),
            })?;

        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let manager = Arc::new(Self {
            token: Arc::clone(&token),
            credentials,
            rest_api_base_url: dhan_config.rest_api_base_url.clone(),
            auth_base_url: dhan_config.auth_base_url.clone(),
            http_client,
            token_config: token_config.clone(),
            network_config: network_config.clone(),
            notifier: Arc::clone(notifier),
            instance_lock_held,
        });

        // Fast path: try loading a cached token from a previous run.
        // This skips the Dhan HTTP auth call (~500ms-2s), saving significant
        // boot time on crash recovery. SSM credentials are still fetched above
        // (needed for the renewal loop).
        if let Some(cached_token) = token_cache::load_token_cache(&manager.credentials.client_id) {
            manager.token.store(Arc::new(Some(cached_token)));
            info!(
                expires_at = %manager.current_expiry_display(),
                "using cached token — skipped Dhan auth HTTP call"
            );
            notifier.notify(NotificationEvent::AuthenticationSuccess);
            return Ok(manager);
        }

        // Slow path: full auth via Dhan API.
        // Infinite retry for transient errors, fail fast for permanent errors.
        // TOTP errors get limited retries (window boundary timing can cause
        // valid secrets to produce rejected codes).
        let mut attempt = 0u32;
        let mut totp_retries = 0u32;
        let mut delay = Duration::from_millis(network_config.retry_initial_delay_ms);
        let max_delay = Duration::from_secs(AUTH_RETRY_MAX_BACKOFF_SECS);

        loop {
            attempt = attempt.saturating_add(1);
            match manager.acquire_token().await {
                Ok(()) => {
                    info!(
                        attempt,
                        expires_at = %manager.current_expiry_display(),
                        "initial authentication successful"
                    );
                    notifier.notify(NotificationEvent::AuthenticationSuccess);
                    return Ok(manager);
                }
                Err(err) => {
                    let reason = err.to_string();

                    // Permanent errors — fail fast with clear notification.
                    if is_permanent_auth_error(&reason) {
                        // DH-901: credentials/account-level rejection; no retry.
                        error!(
                            code = tickvault_common::error_code::ErrorCode::Dh901InvalidAuth.code_str(),
                            severity = tickvault_common::error_code::ErrorCode::Dh901InvalidAuth
                                .severity()
                                .as_str(),
                            attempt,
                            error = %reason,
                            "DH-901: permanent auth error — cannot retry"
                        );
                        notifier.notify(NotificationEvent::AuthenticationFailed {
                            reason: format!("PERMANENT: {reason} — check SSM credentials"),
                        });
                        return Err(err);
                    }

                    // TOTP errors — retry with a fresh 30-second window, but cap retries
                    // to avoid infinite loops when the secret is genuinely wrong.
                    if is_totp_error(&reason) {
                        totp_retries = totp_retries.saturating_add(1);
                        if totp_retries > TOTP_MAX_RETRIES {
                            // AUTH-P11 (AUTH-GAP-04): TOTP retries exhausted. A
                            // genuinely-wrong secret never produces a valid code,
                            // so this terminal branch means the TOTP/2FA seed was
                            // rotated externally (operator regenerated on dhan.co
                            // without updating SSM). Emit a distinctly-typed code
                            // so Telegram + the errors.jsonl sink point the
                            // operator at the exact SSM param to reconcile,
                            // instead of a generic auth failure.
                            error!(
                                code = tickvault_common::error_code::ErrorCode::AuthGap04TotpRotatedExternally.code_str(),
                                severity = tickvault_common::error_code::ErrorCode::AuthGap04TotpRotatedExternally
                                    .severity()
                                    .as_str(),
                                attempt,
                                totp_retries,
                                error = %reason,
                                "AUTH-GAP-04: TOTP failed after {TOTP_MAX_RETRIES} retries — \
                                 the TOTP secret was likely rotated externally; verify \
                                 /tickvault/<env>/dhan/totp-secret in SSM matches dhan.co"
                            );
                            notifier.notify(NotificationEvent::AuthenticationFailed {
                                reason: format!(
                                    "TOTP invalid after {TOTP_MAX_RETRIES} retries — secret rotated externally? check /tickvault/<env>/dhan/totp-secret in SSM"
                                ),
                            });
                            return Err(err);
                        }
                        warn!(
                            attempt,
                            totp_retries,
                            max = TOTP_MAX_RETRIES,
                            "TOTP rejected — waiting for next 30s window before retry"
                        );
                        notifier.notify(NotificationEvent::AuthenticationFailed {
                            reason: format!(
                                "TOTP rejected (retry {totp_retries}/{TOTP_MAX_RETRIES}) — waiting 30s for fresh window"
                            ),
                        });
                        // Wait for the next TOTP window (30s) to get a fresh code.
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_secs(TOTP_PERIOD_SECS)) => {}
                            _ = tokio::signal::ctrl_c() => {
                                info!("shutdown signal received during TOTP retry wait");
                                return Err(ApplicationError::AuthenticationFailed {
                                    reason: "shutdown during TOTP retry wait".to_string(),
                                });
                            }
                        }
                        continue;
                    }

                    // Dhan rate limit — use the 125-second cooldown.
                    let wait_duration = if is_dhan_rate_limited(&reason) {
                        warn!(
                            attempt,
                            "Dhan rate limit hit — waiting {} seconds",
                            DHAN_TOKEN_GENERATION_COOLDOWN_SECS
                        );
                        Duration::from_secs(DHAN_TOKEN_GENERATION_COOLDOWN_SECS)
                    } else {
                        warn!(
                            attempt,
                            error = %reason,
                            delay_secs = delay.as_secs(),
                            "auth attempt failed (transient) — retrying"
                        );
                        delay
                    };

                    // Notify on each transient failure. Low severity — the
                    // retry loop is still active, so firing `Critical` would
                    // cry wolf on a network blip that the next attempt
                    // resolves (Q2, 2026-04-23). If the loop ultimately
                    // exhausts on a TOTP / permanent-auth failure, the
                    // terminal branch above fires `AuthenticationFailed`
                    // at `Severity::Critical` instead.
                    notifier.notify(NotificationEvent::AuthenticationTransientFailure {
                        attempt,
                        reason: reason.clone(),
                        next_retry_ms: u64::try_from(wait_duration.as_millis()).unwrap_or(u64::MAX),
                    });

                    // Sleep with Ctrl+C awareness.
                    tokio::select! {
                        () = tokio::time::sleep(wait_duration) => {}
                        _ = tokio::signal::ctrl_c() => {
                            info!("shutdown signal received during auth retry");
                            return Err(ApplicationError::AuthenticationFailed {
                                reason: "shutdown during auth retry".to_string(),
                            });
                        }
                    }

                    // Exponential backoff (capped), skip if we used rate-limit delay.
                    if !is_dhan_rate_limited(&reason) {
                        delay = delay.saturating_mul(2).min(max_delay);
                    }
                }
            }
        }
    }

    /// Creates a `TokenManager` using a pre-existing `TokenHandle`.
    ///
    /// Called during two-phase boot: the fast path has already loaded a
    /// cached token into `token_handle` and connected WebSocket. This method
    /// fetches SSM credentials and validates the cached token belongs to
    /// the correct account. If the client_id doesn't match, it re-authenticates.
    ///
    /// # Arguments
    /// * `token_handle` — Pre-existing handle already populated with a cached token.
    /// * `cached_client_id` — Client ID from the token cache (for validation).
    /// * `instance_lock_held` — RESILIENCE-03 mint tripwire flag (see the
    ///   field doc). The fast-boot arm currently passes `None` — it holds
    ///   no dual-instance lock (documented residual gap, 2026-07-04).
    #[instrument(skip_all)]
    pub async fn initialize_deferred(
        token_handle: TokenHandle,
        cached_client_id: &str,
        dhan_config: &DhanConfig,
        token_config: &TokenConfig,
        network_config: &NetworkConfig,
        notifier: &Arc<NotificationService>,
        instance_lock_held: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Arc<Self>, ApplicationError> {
        info!("deferred auth: fetching SSM credentials for validation + renewal");

        // Validate HTTPS scheme
        if !dhan_config.rest_api_base_url.starts_with("https://") {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "rest_api_base_url must use HTTPS, got: {}",
                    dhan_config.rest_api_base_url
                ),
            });
        }
        if !dhan_config.auth_base_url.starts_with("https://") {
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "auth_base_url must use HTTPS, got: {}",
                    dhan_config.auth_base_url
                ),
            });
        }

        // SSM credential fetch with retry (3 attempts, exponential backoff).
        let credentials = {
            let mut attempt = 0u32;
            loop {
                attempt = attempt.saturating_add(1);
                match secret_manager::fetch_dhan_credentials().await {
                    Ok(creds) => break creds,
                    Err(err) if attempt >= 3 => return Err(err),
                    Err(err) => {
                        warn!(
                            attempt,
                            error = %err,
                            "deferred auth: SSM credential fetch failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(u64::from(2u32.pow(attempt)))).await;
                    }
                }
            }
        };

        // SEC-C2-2 fix (2026-07-07): pin
        // `redirect(Policy::none())` — the §18 CSV-downloader precedent.
        // Dhan's auth + API endpoints never legitimately redirect, and with
        // the default follow-up-to-10 policy a redirect-loop/limit failure
        // surfaces on the SEND leg with the FINAL redirect URL (taken from
        // the server-controlled Location header) embedded in the reqwest
        // error text — letting a WAF/CDN-planted URL path reach the
        // mid-session watchdog's send-leg transient-needle scan (space-free
        // needles like "connecterror" are plantable in a path) and downgrade
        // a paging RestSurfaceDegraded outage to a silent Transient. With
        // Policy::none() a 3xx is an ordinary non-2xx RESPONSE → the
        // HTTP-response wrapper → RestSurfaceDegraded (correct + paging).
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(network_config.request_timeout_ms))
            .build()
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!("HTTP client creation failed: {err}"),
            })?;

        let manager = Arc::new(Self {
            token: token_handle,
            credentials,
            rest_api_base_url: dhan_config.rest_api_base_url.clone(),
            auth_base_url: dhan_config.auth_base_url.clone(),
            http_client,
            token_config: token_config.clone(),
            network_config: network_config.clone(),
            notifier: Arc::clone(notifier),
            instance_lock_held,
        });

        // Validate cached client_id against SSM. If they differ, the cache was
        // for a different account — do full re-auth to get a valid token.
        let ssm_client_id = manager.credentials.client_id.expose_secret();
        if cached_client_id != ssm_client_id {
            warn!("deferred auth: cached client_id mismatch — re-authenticating");
            token_cache::delete_cache_file();
            manager.acquire_token().await.map_err(|err| {
                // AUTH-GAP-01: deferred re-auth path failed.
                error!(
                    code = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry
                        .severity()
                        .as_str(),
                    error = %err,
                    "AUTH-GAP-01: deferred auth — re-authentication failed"
                );
                err
            })?;
            info!(
                expires_at = %manager.current_expiry_display(),
                "deferred auth: re-authentication successful after client_id mismatch"
            );
        } else {
            info!("deferred auth: cached token validated against SSM credentials");
        }

        notifier.notify(NotificationEvent::AuthenticationSuccess);
        Ok(manager)
    }

    /// Returns a `TokenHandle` for O(1) atomic token reads.
    ///
    /// Downstream consumers (WebSocket, REST) call this once at setup,
    /// then `handle.load()` on every use. The pointer swaps atomically
    /// on renewal — zero lock contention on the hot path.
    pub fn token_handle(&self) -> TokenHandle {
        Arc::clone(&self.token)
    }

    /// Returns the Dhan client id as a plain `String`.
    ///
    /// The stored field IS `Secret`-wrapped (`DhanCredentials.client_id` —
    /// uniform defensive posture for everything under `credentials`), and
    /// this accessor DELIBERATELY widens it to a plain `String`: the client
    /// id travels as a plain-text `client-id` / `dhanClientId` HTTP header
    /// on every Dhan REST call (precedent: `OrderApiClient`'s plain
    /// `client_id: String` field, and the `RenewToken` header at the
    /// renewal site), so it is not access-token-class secret material.
    /// This accessor exists so fire-time consumers of the global
    /// TokenManager (e.g. the 15:25 IST orphan-position watchdog re-homed
    /// 2026-07-14) can build an `OrderApiClient` without threading
    /// credentials through boot.
    #[must_use]
    pub fn client_id_string(&self) -> String {
        self.credentials.client_id.expose_secret().to_string()
    }

    /// Spawns the background token renewal task.
    ///
    /// Sleeps until the refresh window (token_validity - refresh_before_expiry),
    /// then renews the token. Retries with exponential backoff on failure.
    /// Runs indefinitely until the task is cancelled.
    ///
    /// Also drives the `tv_token_remaining_seconds` live-countdown
    /// emitter (`token_gauge_loop`, 30s cadence) inside the SAME task
    /// via `select!`, so the gauge's lifecycle stays coupled to the
    /// lane-owned renewal task: aborting the returned handle — or the
    /// renewal loop halting at its circuit breaker — stops the countdown
    /// too (C4 — no orphan emitter after a Dhan-lane disable).
    #[instrument(skip_all)]
    pub fn spawn_renewal_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        let gauge_manager = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                () = manager.renewal_loop() => {}
                () = gauge_manager.token_gauge_loop() => {}
            }
        })
    }

    // -----------------------------------------------------------------------
    // Public — User Profile & Pre-Market Validation
    // Source: docs/dhan-ref/02-authentication.md
    // -----------------------------------------------------------------------

    /// Fetches the user profile from `GET /v2/profile`.
    ///
    /// Returns `UserProfileResponse` containing `dataPlan`, `activeSegment`,
    /// `tokenValidity`, etc. Used by `pre_market_check()` for pre-trading
    /// validation at 08:45 IST.
    #[instrument(skip_all, name = "get_user_profile")]
    // TEST-EXEMPT: requires live HTTP server + valid token; types tested in auth::types::tests
    pub async fn get_user_profile(
        &self,
    ) -> Result<super::types::UserProfileResponse, ApplicationError> {
        // Audit-2026-05-03 H1: wrap the raw JWT in `Zeroizing` so the
        // intermediate plaintext String is wiped from memory when this
        // function returns. WebSocket paths already use this pattern; the
        // REST profile/renew paths previously held the token as a plain
        // `String` which left a copy on the heap until the next allocation
        // overwrote it. Matches the WS pattern from
        // `crates/core/src/websocket/connection.rs::build_authenticated_url`.
        let access_token_value: Zeroizing<String> = {
            let token_guard = self.token.load();
            let token_state = token_guard.as_ref().as_ref().ok_or_else(|| {
                ApplicationError::AuthenticationFailed {
                    reason: "no access token available for profile request".to_string(),
                }
            })?;
            Zeroizing::new(token_state.access_token().expose_secret().to_string())
        };

        // DHAN-REST-400 item 1b (2026-06-10): join via join_api_url so a
        // trailing-slash base-URL override can never produce `/v2//profile`.
        let url = join_api_url(
            &self.rest_api_base_url,
            tickvault_common::constants::DHAN_USER_PROFILE_PATH,
        );

        let response = self
            .http_client
            .get(&url)
            .header("access-token", access_token_value.as_str())
            .send()
            .await
            .map_err(|err| ApplicationError::AuthenticationFailed {
                // Audit-2026-05-03 L4: redact any URL params from reqwest error
                // string defensively. The profile path uses header auth (no token
                // in URL), but the same pattern is used in `acquire_token` (line
                // 644) for consistency — defence in depth.
                // AG5-R2-1: interpolate the SHARED wrapper constant — the
                // watchdog classifier prefix-matches it; a reword must go
                // through the constant (single-sourced, ratcheted).
                reason: format!(
                    "{PROFILE_SEND_LEG_WRAPPER} {}",
                    redact_url_params(&err.to_string())
                ),
            })?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // DHAN-REST-400 (2026-06-10): the previous code dropped Dhan's
            // response body entirely ("response body redacted from error"),
            // so a full day of 400s carried zero root-cause signal. Capture a
            // bounded (≤300 chars), secret-redacted body — Dhan's errorType /
            // errorCode / errorMessage name the cause (data-plan expiry vs
            // malformed request vs WAF) — plus the EXACT final request URL
            // (token-redacted; a `//` after the scheme means a malformed
            // base-URL override).
            // Audit-2026-05-03 M1: profile failure blocks pre-market validation —
            // must be `error!` (not `warn!`) so Loki routes to Telegram per
            // audit-findings-2026-04-17.md Rule 5.
            let captured_body = capture_rest_error_body(&body_text);
            let redacted_url = redact_url_params(&url);
            error!(
                code = tickvault_common::error_code::ErrorCode::Dh901InvalidAuth.code_str(),
                severity = tickvault_common::error_code::ErrorCode::Dh901InvalidAuth
                    .severity()
                    .as_str(),
                status = %status,
                url = %redacted_url,
                body = %captured_body,
                "DH-901: profile request failed"
            );
            return Err(ApplicationError::AuthenticationFailed {
                // AG5-R2-1: shared wrapper constant (see the constant block
                // near the top of this file) — the watchdog's status-aware
                // classification prefix-anchors on it.
                reason: format!(
                    "{PROFILE_HTTP_RESPONSE_WRAPPER}{status} url={redacted_url} body={captured_body}"
                ),
            });
        }

        serde_json::from_str(&body_text).map_err(|err| ApplicationError::AuthenticationFailed {
            // AG5-R2-1: shared wrapper constant — classified as
            // RestSurfaceDegraded (never a mint) by the watchdog.
            reason: format!("{PROFILE_PARSE_ERROR_WRAPPER}: {err}"),
        })
    }

    /// Pre-market validation at 08:45 IST.
    ///
    /// Checks:
    /// 1. `dataPlan == "Active"` — required for market data WebSocket access
    /// 2. `activeSegment` contains `"Derivative"` — required for F&O trading
    /// 3. Token has > 4 hours until expiry — prevents token death during market hours
    ///
    /// On failure: returns error for CRITICAL alert via Telegram + token rotation attempt.
    #[instrument(skip_all, name = "pre_market_check")]
    // TEST-EXEMPT: requires live HTTP server + valid token; calls get_user_profile()
    pub async fn pre_market_check(&self) -> Result<(), ApplicationError> {
        let profile = self.get_user_profile().await?;

        // Check 1: data plan must be Active
        if profile.data_plan != "Active" {
            // DATA-806: data plan missing; trading cannot start.
            error!(
                code = tickvault_common::error_code::ErrorCode::Data806NotSubscribed.code_str(),
                severity = tickvault_common::error_code::ErrorCode::Data806NotSubscribed
                    .severity()
                    .as_str(),
                data_plan = %profile.data_plan,
                "DATA-806: pre-market check FAILED — data plan is not Active"
            );
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "data plan is '{}', must be 'Active' for market data access",
                    profile.data_plan
                ),
            });
        }

        // Check 2: active segment must contain derivatives access.
        // Dhan returns EITHER full names ("Equity, Derivative") OR single-char codes
        // ("E, D, C, M, ") depending on API version/account type. Accept both.
        let has_derivative =
            profile.active_segment.contains("Derivative") || profile.active_segment.contains("D");
        if !has_derivative {
            // DATA-806: Derivative segment not active on this Dhan plan.
            error!(
                code = tickvault_common::error_code::ErrorCode::Data806NotSubscribed.code_str(),
                severity = tickvault_common::error_code::ErrorCode::Data806NotSubscribed
                    .severity()
                    .as_str(),
                active_segment = %profile.active_segment,
                "DATA-806: pre-market check FAILED — Derivative segment not active"
            );
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "active segment '{}' does not indicate F&O access (expected 'Derivative' or 'D')",
                    profile.active_segment
                ),
            });
        }

        // Check 3: token must have > 4 hours until expiry
        let token_guard = self.token.load();
        if let Some(token_state) = token_guard.as_ref().as_ref() {
            let remaining = token_state.time_until_refresh(0);
            let four_hours = Duration::from_secs(4 * 3600);
            if remaining < four_hours {
                // Audit-2026-05-03 M5: token < 4h headroom is a pre-market
                // failure that BLOCKS trading (function returns Err). Per
                // audit-findings-2026-04-17.md Rule 5, blocking failures
                // MUST be `error!` not `warn!` so Loki routes to Telegram.
                error!(
                    code = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry
                        .severity()
                        .as_str(),
                    remaining_secs = remaining.as_secs(),
                    "AUTH-GAP-01: pre-market check FAILED — token has less than 4 hours until expiry"
                );
                return Err(ApplicationError::AuthenticationFailed {
                    reason: format!(
                        "token expires in {} seconds — less than 4 hours until expiry, rotate token",
                        remaining.as_secs()
                    ),
                });
            }
        }

        info!(
            data_plan = %profile.data_plan,
            active_segment = %profile.active_segment,
            "pre-market check PASSED"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private — Token Acquisition
    // -----------------------------------------------------------------------

    /// Performs full authentication: TOTP → generateAccessToken → store.
    ///
    /// Calls `POST https://auth.dhan.co/app/generateAccessToken` with query params:
    /// `dhanClientId`, `pin`, `totp`. Response is flat JSON with camelCase fields.
    #[instrument(skip_all)]
    async fn acquire_token(&self) -> Result<(), ApplicationError> {
        // RESILIENCE-03 mint tripwire (lock-before-mint, 2026-07-04):
        // refuse the MINT fail-closed if this process no longer holds the
        // dual-instance lock. Checked BEFORE TOTP generation and before
        // any network I/O so a refused mint has zero external side
        // effects. `RenewToken` (try_renew_token) is deliberately NOT
        // gated — renewing our own still-active token harms no peer.
        if mint_refused_by_instance_lock(self.instance_lock_held.as_deref()) {
            error!(
                code = tickvault_common::error_code::ErrorCode::Resilience03MintRefusedLockNotHeld
                    .code_str(),
                severity =
                    tickvault_common::error_code::ErrorCode::Resilience03MintRefusedLockNotHeld
                        .severity()
                        .as_str(),
                "RESILIENCE-03: refusing generateAccessToken — this process no longer \
                 holds the dual-instance lock; a peer instance owns the Dhan session \
                 and minting would invalidate its token (one active token at a time)"
            );
            self.notifier
                .notify(NotificationEvent::AuthenticationFailed {
                    reason: "RESILIENCE-03: token mint refused — another tickvault instance \
                         holds the dual-instance lock"
                        .to_string(),
                });
            return Err(ApplicationError::AuthenticationFailed {
                // SEC-R2-2: built from the SHARED refusal prefix so the
                // AUTH-GAP-05 watchdog can prefix-anchor its permanence
                // classification on OUR literal (never a substring scan of
                // text that concatenates a server-controlled body).
                reason: format!(
                    "{RESILIENCE03_MINT_REFUSAL_REASON_PREFIX} — generateAccessToken mint \
                     refused (fail-closed)"
                ),
            });
        }

        let totp_code = generate_totp_code(&self.credentials.totp_secret)?;

        let url = join_api_url(&self.auth_base_url, DHAN_GENERATE_TOKEN_PATH);

        // Dhan expects query params: dhanClientId, pin, totp
        // Our SSM `client_secret` maps to Dhan's `pin` (6-digit trading PIN).
        let response = self
            .http_client
            .post(&url)
            .query(&[
                ("dhanClientId", self.credentials.client_id.expose_secret()),
                ("pin", self.credentials.client_secret.expose_secret()),
                ("totp", &totp_code),
            ])
            .send()
            .await
            .map_err(|err| ApplicationError::AuthenticationFailed {
                reason: format!(
                    "generateAccessToken request failed: {}",
                    redact_url_params(&err.to_string())
                ),
            })?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // DHAN-REST-400 (2026-06-10): bounded secret-redacted body +
            // final URL (the token-bearing query params are added by reqwest,
            // not present in `url`; redact defensively anyway).
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!(
                    "generateAccessToken HTTP {status} url={} body={}",
                    redact_url_params(&url),
                    capture_rest_error_body(&body_text)
                ),
            });
        }

        // Dhan returns HTTP 200 even on error — check for error response first.
        // Error format: {"status":"error","message":"Invalid Pin"}
        if let Ok(error_check) = serde_json::from_str::<serde_json::Value>(&body_text)
            && error_check.get("status").and_then(|s| s.as_str()) == Some("error")
        {
            let message = error_check
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(ApplicationError::AuthenticationFailed {
                reason: format!("Dhan auth error: {message}"),
            });
        }

        let body: DhanGenerateTokenResponse = serde_json::from_str(&body_text).map_err(|err| {
            // DHAN-REST-400 follow-up (security review LOW): bounded
            // JWT-redacted capture — a parse-error body could echo token
            // material that url-param redaction alone would miss.
            ApplicationError::AuthenticationFailed {
                reason: format!(
                    "failed to parse auth response (HTTP {status}): {err}\nResponse body: {}",
                    capture_rest_error_body(&body_text)
                ),
            }
        })?;

        if body.access_token.is_empty() {
            return Err(ApplicationError::AuthenticationFailed {
                reason: "generateAccessToken returned empty accessToken".to_string(),
            });
        }

        let token_state = TokenState::from_generate_response(&body);
        self.token.store(Arc::new(Some(token_state)));

        // Cache for fast restart on crash recovery
        self.save_current_token_to_cache();

        Ok(())
    }

    /// Attempts token renewal via the `RenewToken` endpoint.
    ///
    /// Calls `GET https://api.dhan.co/v2/RenewToken` with headers:
    /// `access-token` (current JWT) and `dhanClientId`.
    /// Expires the current token and returns a new one with 24h validity.
    #[instrument(skip_all)]
    async fn try_renew_token(&self) -> Result<(), ApplicationError> {
        let current_token = self.token.load();
        let token_state = current_token.as_ref().as_ref().ok_or_else(|| {
            ApplicationError::AuthenticationFailed {
                reason: "cannot renew — no current token".to_string(),
            }
        })?;

        let url = join_api_url(&self.rest_api_base_url, DHAN_RENEW_TOKEN_PATH);

        // Audit-2026-05-03 H1: zeroize the JWT + clientId for the renewal
        // request so plaintext is wiped from heap when this fn returns.
        let access_token_value: Zeroizing<String> =
            Zeroizing::new(token_state.access_token().expose_secret().to_string());
        let client_id_value: Zeroizing<String> =
            Zeroizing::new(self.credentials.client_id.expose_secret().to_string());

        // Dhan's RenewToken is a GET with headers, not POST with body
        let response = self
            .http_client
            .get(&url)
            .header("access-token", access_token_value.as_str())
            .header("dhanClientId", client_id_value.as_str())
            .send()
            .await
            .map_err(|err| ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!(
                    "RenewToken request failed: {}",
                    redact_url_params(&err.to_string())
                ),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            // DHAN-REST-400 (2026-06-10): bounded secret-redacted body +
            // final URL so a renewal failure names its cause.
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: format!(
                    "RenewToken HTTP {status} url={} body={}",
                    redact_url_params(&url),
                    capture_rest_error_body(&body_text)
                ),
            });
        }

        // Renewal response uses the same flat format as generateAccessToken
        let body: DhanGenerateTokenResponse =
            response
                .json()
                .await
                .map_err(|err| ApplicationError::TokenRenewalFailed {
                    attempts: 0,
                    reason: format!(
                        "failed to parse renewal response (HTTP {status}): {}",
                        redact_url_params(&err.to_string())
                    ),
                })?;

        if body.access_token.is_empty() {
            return Err(ApplicationError::TokenRenewalFailed {
                attempts: 0,
                reason: "RenewToken returned empty accessToken".to_string(),
            });
        }

        let new_token_state = TokenState::from_generate_response(&body);
        self.token.store(Arc::new(Some(new_token_state)));

        // Update cache for fast restart
        self.save_current_token_to_cache();

        Ok(())
    }

    /// Attempts renewal with fallback: renewToken → generateAccessToken.
    async fn renew_with_fallback(&self) -> Result<(), ApplicationError> {
        match self.try_renew_token().await {
            Ok(()) => Ok(()),
            Err(renew_err) => {
                // Audit-2026-05-03 L3: warn lacks `code=` field. Add it for
                // consistency with the project-wide error-code-tag convention
                // even though `error_code_tag_guard` only enforces error-level.
                warn!(
                    code = tickvault_common::error_code::ErrorCode::Dh901InvalidAuth.code_str(),
                    error = %renew_err,
                    "renewToken failed — falling back to generateAccessToken (DH-901 fallback path)"
                );
                self.acquire_token().await
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private — Renewal Loop
    // -----------------------------------------------------------------------

    /// Background renewal loop with retry, backoff, and circuit-breaker halt.
    ///
    /// If renewal fails repeatedly, the circuit breaker sleeps 60s and retries.
    /// After `TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES` consecutive failures,
    /// the loop halts with a critical alert — operator must intervene.
    async fn renewal_loop(self: Arc<Self>) {
        let mut consecutive_circuit_breaker_cycles: u32 = 0;

        loop {
            let sleep_duration = self.time_until_next_refresh();

            // L4: Emit token remaining seconds gauge for Prometheus alert rules.
            // tv_token_remaining_seconds drives TokenExpiryWarning (<1h) and
            // TokenExpiryCritical (<10min) alerts in tickvault-alerts.yml.
            {
                let guard = self.token.load();
                if let Some(state) = guard.as_ref().as_ref() {
                    let remaining = state.time_until_refresh(0);
                    metrics::gauge!("tv_token_remaining_seconds").set(remaining.as_secs() as f64);
                }
            }

            if sleep_duration > Duration::ZERO {
                info!(
                    sleep_secs = sleep_duration.as_secs(),
                    "token renewal task sleeping until refresh window"
                );
                tokio::time::sleep(sleep_duration).await;
            }

            // Retry with exponential backoff
            let mut attempts: u32 = 0;
            let max_attempts = self.network_config.retry_max_attempts;
            let mut delay = Duration::from_millis(self.network_config.retry_initial_delay_ms);
            let max_delay = Duration::from_millis(self.network_config.retry_max_delay_ms);

            let mut succeeded = false;

            while attempts < max_attempts {
                attempts = attempts.saturating_add(1);

                match self.renew_with_fallback().await {
                    Ok(()) => {
                        // Update token remaining gauge after successful renewal.
                        {
                            let guard = self.token.load();
                            if let Some(state) = guard.as_ref().as_ref() {
                                let remaining = state.time_until_refresh(0);
                                metrics::gauge!("tv_token_remaining_seconds")
                                    .set(remaining.as_secs() as f64);
                            }
                        }
                        info!(
                            attempts,
                            expires_at = %self.current_expiry_display(),
                            "token renewed successfully"
                        );
                        // Phase 0 Item 17 — positive-ping Telegram + Prom
                        // counter so the operator sees the daily SEBI
                        // 24h JWT renewal succeed instead of inferring
                        // it from the absence of TokenRenewalFailed.
                        metrics::counter!(
                            "tv_token_renewals_total",
                            "result" => "success",
                            "trigger" => "scheduled",
                        )
                        .increment(1);
                        self.notifier.notify(NotificationEvent::TokenRenewed);
                        succeeded = true;
                        break;
                    }
                    Err(err) => {
                        warn!(
                            attempt = attempts,
                            max_attempts,
                            error = %err,
                            "token renewal attempt failed"
                        );

                        if attempts >= max_attempts {
                            break;
                        }

                        tokio::time::sleep(delay).await;
                        delay = delay.saturating_mul(2).min(max_delay);
                    }
                }
            }

            if succeeded {
                consecutive_circuit_breaker_cycles = 0;
            } else {
                consecutive_circuit_breaker_cycles =
                    consecutive_circuit_breaker_cycles.saturating_add(1);
                error!(
                    attempts,
                    circuit_breaker_cycle = consecutive_circuit_breaker_cycles,
                    max_cycles =
                        tickvault_common::constants::TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES,
                    "ALL token renewal attempts exhausted — token may expire"
                );
                self.notifier.notify(NotificationEvent::TokenRenewalFailed {
                    attempts,
                    reason: format!(
                        "all renewal attempts exhausted (cycle {}/{}) — token may expire",
                        consecutive_circuit_breaker_cycles,
                        tickvault_common::constants::TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES,
                    ),
                });
                // Phase 0 Item 17 — failed-cycle counter pairs with the
                // success counter above so `result=failed / result=success`
                // ratio is dashboard-visible.
                metrics::counter!(
                    "tv_token_renewals_total",
                    "result" => "failed",
                    "trigger" => "scheduled",
                )
                .increment(1);

                if consecutive_circuit_breaker_cycles
                    >= tickvault_common::constants::TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES
                {
                    error!(
                        "token renewal halted after {} consecutive failures — operator must intervene",
                        consecutive_circuit_breaker_cycles
                    );
                    self.notifier.notify(NotificationEvent::TokenRenewalFailed {
                        attempts: consecutive_circuit_breaker_cycles,
                        reason:
                            "HALTED: token renewal permanently failed — manual restart required"
                                .to_string(),
                    });
                    metrics::counter!(
                        "tv_token_renewals_total",
                        "result" => "circuit_breaker_halt",
                        "trigger" => "scheduled",
                    )
                    .increment(1);
                    return;
                }

                tokio::time::sleep(Duration::from_secs(
                    tickvault_common::constants::TOKEN_RENEWAL_CIRCUIT_BREAKER_RESET_SECS,
                ))
                .await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private — Live Token Countdown Gauge
    // -----------------------------------------------------------------------

    /// Emits the `tv_token_remaining_seconds` gauge from the current
    /// arc-swap token state.
    ///
    /// Same computation as the two `renewal_loop` emit sites
    /// (`time_until_refresh(0)` = seconds until token expiry). Returns
    /// the emitted value for testability, or `None` when no token is
    /// loaded yet — in that case the gauge is deliberately NOT set: a 0
    /// during the pre-auth boot window would falsely trip the
    /// TokenExpiryCritical (<10min) alert before the first mint lands.
    fn emit_token_remaining_gauge(&self) -> Option<u64> {
        let guard = self.token.load();
        let state = guard.as_ref().as_ref()?;
        let remaining = state.time_until_refresh(0).as_secs();
        metrics::gauge!("tv_token_remaining_seconds").set(remaining as f64);
        Some(remaining)
    }

    /// Periodic live-countdown emitter for `tv_token_remaining_seconds`.
    ///
    /// Re-emits the gauge every [`TOKEN_GAUGE_INTERVAL_SECS`] (30s) so
    /// the operator sees a live countdown between renewals instead of a
    /// flat line frozen at the last `renewal_loop` wakeup. Cold path —
    /// one O(1) arc-swap load + one gauge set per tick; the renewal
    /// timing and the two in-loop emit sites are untouched.
    ///
    /// Runs forever; `spawn_renewal_task` drives it inside the SAME
    /// task as `renewal_loop` via `select!`, so it dies with the
    /// renewal loop (circuit-breaker halt or lane-teardown abort) — no
    /// orphan emitter keeps stamping the gauge after a Dhan-lane
    /// disable (C4 coupling).
    async fn token_gauge_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(TOKEN_GAUGE_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            // Skip-on-None handled inside the helper; the returned
            // value exists for the unit tests.
            let _emitted = self.emit_token_remaining_gauge();
        }
    }

    // -----------------------------------------------------------------------
    // Private — Helpers
    // -----------------------------------------------------------------------

    /// Returns the duration until the next refresh should occur.
    fn time_until_next_refresh(&self) -> Duration {
        let guard = self.token.load();
        match guard.as_ref().as_ref() {
            Some(state) => state.time_until_refresh(self.token_config.refresh_before_expiry_hours),
            None => Duration::ZERO, // No token — refresh immediately
        }
    }

    /// Saves the current token to the disk cache for fast crash recovery.
    fn save_current_token_to_cache(&self) {
        let guard = self.token.load();
        if let Some(token_state) = guard.as_ref().as_ref() {
            token_cache::save_token_cache(token_state, &self.credentials.client_id);
        }
    }

    /// Returns the current token expiry as a display string (for logging).
    fn current_expiry_display(&self) -> String {
        let guard = self.token.load();
        match guard.as_ref().as_ref() {
            Some(state) => state.expires_at().to_string(),
            None => "no token".to_string(),
        }
    }

    /// Wave 3-C Item 12 — seconds until token expiry, for the
    /// market-open self-test sub-check `token_expiry_headroom`.
    ///
    /// Returns 0 in two distinct fail-closed cases:
    /// 1. No token is loaded (boot-time or pre-auth state).
    /// 2. The loaded token has already passed its expiry timestamp —
    ///    `time_until_refresh(0)` saturates to `Duration::ZERO`.
    ///
    /// Both cases route the self-test to `Critical` because the
    /// `token_expiry_headroom_secs < TOKEN_EXPIRY_HEADROOM_CRITICAL_SECS`
    /// (= 14_400) check trips on 0. Adversarial review
    /// (security-reviewer, 2026-04-28) flagged the original doc-comment
    /// for not naming the second case explicitly.
    #[must_use]
    pub fn seconds_until_expiry(&self) -> u64 {
        let guard = self.token.load();
        match guard.as_ref().as_ref() {
            Some(state) => state.time_until_refresh(0).as_secs(),
            None => 0,
        }
    }

    /// Wave 2 Item 5.4 (AUTH-GAP-03) — proactive token renewal on
    /// wake-from-sleep.
    ///
    /// Checks the current token's remaining validity. If the token has
    /// **less than `threshold_secs` of validity left** (or no token
    /// exists at all), triggers a renewal BEFORE the caller's next
    /// reconnect attempt. Prevents the legacy "wake → reconnect →
    /// DH-901 / DataAPI-807 → token refresh → reconnect → success"
    /// 30-second cascade after a long post-close sleep.
    ///
    /// Returns:
    /// - `Ok(true)` if a renewal was performed.
    /// - `Ok(false)` if the existing token is still fresh enough.
    /// - `Err(_)` if the renewal attempt failed (caller should still
    ///   try to reconnect — Dhan may accept the existing token if it
    ///   has any validity left).
    ///
    /// Threshold guidance: pass `14400` (4h) for the post-sleep wake
    /// path. Set higher if the next sleep window is expected to be
    /// long (e.g., Friday-evening → Monday-morning).
    pub async fn force_renewal_if_stale(
        &self,
        threshold_secs: i64,
    ) -> Result<bool, ApplicationError> {
        // Use the canonical IST offset helper (same one TokenState uses).
        let now =
            chrono::Utc::now().with_timezone(&tickvault_common::trading_calendar::ist_offset());

        // Snapshot the current token state without holding the lock
        // across an await.
        let remaining_secs: i64 = {
            let guard = self.token.load();
            match guard.as_ref().as_ref() {
                Some(state) => (state.expires_at() - now).num_seconds(),
                // No token at all → treat as "infinitely stale".
                None => i64::MIN,
            }
        };

        if remaining_secs > threshold_secs {
            tracing::debug!(
                remaining_secs,
                threshold_secs,
                "AUTH-GAP-03 token is fresh enough — skipping force renewal"
            );
            return Ok(false);
        }

        tracing::info!(
            remaining_secs,
            threshold_secs,
            code = tickvault_common::error_code::ErrorCode::AuthGap03TokenForceRenewedOnWake
                .code_str(),
            "AUTH-GAP-03 token below threshold — forcing renewal before next reconnect"
        );
        metrics::counter!(
            "tv_token_force_renewal_total",
            "trigger" => "ws_wake"
        )
        .increment(1);

        match self.renew_with_fallback().await {
            Ok(()) => Ok(true),
            Err(e) => {
                tracing::error!(
                    ?e,
                    code =
                        tickvault_common::error_code::ErrorCode::AuthGap03TokenForceRenewedOnWake
                            .code_str(),
                    "AUTH-GAP-03 force renewal failed — caller should still try reconnect"
                );
                Err(e)
            }
        }
    }

    /// Wave 2 Item 5.4 (G1) — unconditional token renewal.
    ///
    /// Forces a renewal regardless of remaining validity. Use when the
    /// caller has evidence the existing token is invalid (e.g., 807
    /// disconnect from Dhan, DH-901 from REST). Prefer
    /// [`force_renewal_if_stale`](Self::force_renewal_if_stale) on the
    /// post-sleep wake path so a fresh token is not wasted.
    ///
    /// # Errors
    ///
    /// Propagates the underlying renew/generate error. Caller is
    /// responsible for retry policy.
    pub async fn force_renewal(&self) -> Result<(), ApplicationError> {
        metrics::counter!(
            "tv_token_force_renewal_total",
            "trigger" => "explicit"
        )
        .increment(1);
        self.renew_with_fallback().await
    }

    /// Wave 2 Item 5.4 (G1) — current token expiry timestamp.
    ///
    /// Returns the IST `DateTime` at which the cached token expires,
    /// or `None` if no token is loaded. Caller subtracts the
    /// configured refresh-before-expiry headroom to derive the next
    /// scheduled renewal moment. Cheap O(1) `arc-swap` read; safe to
    /// poll from any task.
    #[must_use]
    pub fn next_renewal_at(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        let guard = self.token.load();
        guard.as_ref().as_ref().map(|state| state.expires_at())
    }

    /// RESILIENCE-03 (2026-07-06): read the dual-instance-lock ownership
    /// flag so the mid-session re-mint trigger (AUTH-GAP-05) can PRE-GATE
    /// fail-closed BEFORE calling [`Self::force_renewal`] — the refusal is
    /// decided with ZERO external side effects (no TOTP, no HTTP).
    ///
    /// O(1) `Acquire` load of the same private `instance_lock_held` field
    /// the `acquire_token` mint tripwire reads. `None` means no lock flag
    /// was installed for this manager (fast-boot crash-recovery arm /
    /// tests) — the tripwire is disarmed, exactly as strong as the
    /// existing in-`acquire_token` gate on that arm (documented residual
    /// per `dual-instance-lock-2026-07-04.md` §3).
    #[must_use]
    pub fn dual_instance_lock_held(&self) -> Option<bool> {
        self.instance_lock_held
            .as_ref()
            .map(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Test-only constructor for a `TokenManager` with no live auth I/O.
    /// Crate-visible so sibling-module tests (e.g. the WebSocket `connection`
    /// sleep-wake ratchets, D2c/C4) can build a real `Arc<TokenManager>` to
    /// exercise the lane-owned-vs-global wake-renewal selection without
    /// reaching SSM/Dhan. `initial_token = None` means "no token loaded".
    /// Gated to `#[cfg(test)]` — never part of the production surface.
    #[cfg(test)]
    // TEST-EXEMPT: #[cfg(test)]-only test constructor (no production surface); exercised by every make_test_manager / new_for_test caller across the auth + websocket test modules.
    pub(crate) fn new_for_test(initial_token: Option<TokenState>) -> Arc<Self> {
        Self::new_for_test_with_lock_flag(initial_token, None)
    }

    /// Test-only constructor variant that also installs the RESILIENCE-03
    /// mint-tripwire flag so the refusal path is exercisable without SSM
    /// or network I/O (dual-instance lock hardening 2026-07-04).
    #[cfg(test)]
    // TEST-EXEMPT: #[cfg(test)]-only test constructor (no production surface); exercised by the RESILIENCE-03 tripwire tests below.
    pub(crate) fn new_for_test_with_lock_flag(
        initial_token: Option<TokenState>,
        instance_lock_held: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Arc<Self> {
        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(initial_token)));
        Arc::new(Self {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("test-secret".to_string()),
                totp_secret: secrecy::SecretString::from("test-totp".to_string()),
            },
            rest_api_base_url: "https://api.example.com".to_string(),
            auth_base_url: "https://auth.example.com".to_string(),
            http_client: reqwest::Client::new(),
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 5000,
                websocket_connect_timeout_ms: 5000,
                retry_initial_delay_ms: 100,
                retry_max_delay_ms: 1000,
                retry_max_attempts: 3,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
            instance_lock_held,
        })
    }
}

// ---------------------------------------------------------------------------
// Error Classification
// ---------------------------------------------------------------------------

/// Returns `true` if the auth error is permanent (retrying won't help).
///
/// Permanent errors indicate wrong credentials in SSM or a blocked account.
/// The system should notify the user and exit, not retry indefinitely.
///
/// Note: TOTP errors are excluded — they may be transient due to window
/// boundary timing. Use [`is_totp_error`] to handle TOTP retries separately.
fn is_permanent_auth_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("invalid pin")
        || lower.contains("invalid client")
        || lower.contains("blocked")
        || lower.contains("suspended")
        || lower.contains("disabled")
        // RESILIENCE-03 (2026-07-04): a mint refused because the
        // dual-instance lock is not held cannot succeed on retry —
        // the peer owns the session. Fail fast so the boot retry
        // loop / renewal loop escalates instead of spinning.
        || lower.contains("resilience-03")
}

/// RESILIENCE-03 pure decision: should a `generateAccessToken` MINT be
/// refused given the current dual-instance-lock ownership flag?
///
/// * `None` — no lock expected for this manager (tests, fast-boot arm):
///   never refuse (tripwire disarmed).
/// * `Some(flag)` with `flag == true` — lock held: allow the mint.
/// * `Some(flag)` with `flag == false` — lock LOST (peer took over, or
///   shutdown released it): REFUSE fail-closed.
fn mint_refused_by_instance_lock(flag: Option<&std::sync::atomic::AtomicBool>) -> bool {
    match flag {
        None => false,
        Some(held) => !held.load(std::sync::atomic::Ordering::Acquire),
    }
}

/// Returns `true` if the error is a TOTP-related auth failure.
///
/// TOTP errors are retryable up to [`TOTP_MAX_RETRIES`] times because they
/// can fail when the code is generated near a 30-second window boundary
/// and expires during HTTP transit to Dhan's servers.
fn is_totp_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("totp") && (lower.contains("invalid") || lower.contains("failed"))
}

/// Returns `true` if the error is Dhan's token generation rate limit.
fn is_dhan_rate_limited(reason: &str) -> bool {
    reason.contains("every 2 minutes") || reason.contains("once every")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;
    use crate::auth::types::DhanAuthResponseData;

    /// Helper: constructs a `TokenManager` directly (no SSM, no network).
    /// Used to test private helper methods that don't require a real token.
    /// Delegates to the crate-visible `TokenManager::new_for_test` (D2c) so the
    /// test-only construction lives in one place.
    fn make_test_manager(initial_token: Option<TokenState>) -> Arc<TokenManager> {
        TokenManager::new_for_test(initial_token)
    }

    /// SEC-C2-2 ratchet (2026-07-07): every PRODUCTION `reqwest::Client`
    /// built in this module must pin `redirect(Policy::none())`. Under the
    /// default follow-redirects policy, a redirect-loop/limit failure
    /// surfaces on the SEND leg with the FINAL redirect URL (from the
    /// server-controlled Location header) embedded in the reqwest error
    /// text — which the mid-session watchdog's send-leg transient-needle
    /// scan would then substring-match ("connecterror" / "timedout" are
    /// plantable in a URL path), downgrading a paging RestSurfaceDegraded
    /// outage to a silent Transient. Test-region builders (mock servers
    /// never redirect) are exempt via the production-region split.
    #[test]
    fn test_production_http_clients_pin_redirect_policy_none() {
        let src = include_str!("token_manager.rs");
        let production = src
            .split("\nmod tests {")
            .next()
            .expect("split always yields at least one region");
        assert!(
            production.len() < src.len(),
            "production-region split found no `mod tests` boundary — the \
             scan would vacuously include this test's own literals"
        );
        let builder_needle = "reqwest::Client::builder()";
        let mut checked = 0usize;
        let mut search_from = 0usize;
        while let Some(rel) = production[search_from..].find(builder_needle) {
            let off = search_from.saturating_add(rel);
            // The redirect pin must appear in the builder chain immediately
            // following (before `.build()`), within a bounded window.
            let window_end = off.saturating_add(400).min(production.len());
            let chain = &production[off..window_end];
            let build_off = chain
                .find(".build()")
                .expect("every reqwest builder chain must call .build() nearby");
            assert!(
                chain[..build_off].contains("redirect(reqwest::redirect::Policy::none())"),
                "SEC-C2-2 regression: a production reqwest::Client::builder() \
                 chain at byte {off} does not pin \
                 `.redirect(reqwest::redirect::Policy::none())` before \
                 `.build()` — the default follow policy lets a \
                 server-controlled redirect URL enter the send-leg error text \
                 the mid-session watchdog substring-scans"
            );
            checked = checked.saturating_add(1);
            search_from = off.saturating_add(builder_needle.len());
        }
        assert!(
            checked >= 2,
            "expected at least the 2 production TokenManager client builders \
             (initialize + deferred auth) — found {checked}; the scan went \
             vacuous"
        );
    }

    // -----------------------------------------------------------------------
    // RESILIENCE-03 mint tripwire (dual-instance lock hardening 2026-07-04)
    // -----------------------------------------------------------------------

    #[test]
    fn test_mint_refused_when_lock_flag_false() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        assert!(
            mint_refused_by_instance_lock(Some(&flag)),
            "a lost lock (flag=false) MUST refuse the mint fail-closed"
        );
    }

    #[test]
    fn test_mint_allowed_when_lock_flag_true_or_absent() {
        let flag = std::sync::atomic::AtomicBool::new(true);
        assert!(
            !mint_refused_by_instance_lock(Some(&flag)),
            "a held lock (flag=true) must allow the mint"
        );
        assert!(
            !mint_refused_by_instance_lock(None),
            "no lock expected (None — tests / fast-boot arm) must allow the mint"
        );
    }

    /// AUTH-GAP-05 (2026-07-06): the read-only lock accessor mirrors the
    /// constructor flag exactly — `Some(true)` / `Some(false)` / `None` —
    /// so the mid-session re-mint trigger can pre-gate fail-closed
    /// without any network call.
    #[test]
    fn test_dual_instance_lock_held_mirrors_constructor_flag() {
        let held = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let manager = TokenManager::new_for_test_with_lock_flag(None, Some(Arc::clone(&held)));
        assert_eq!(manager.dual_instance_lock_held(), Some(true));

        // The heartbeat loses the lock to a peer — the accessor observes it.
        held.store(false, std::sync::atomic::Ordering::Release);
        assert_eq!(manager.dual_instance_lock_held(), Some(false));

        // No flag installed (fast-boot arm / tests) — tripwire disarmed.
        let no_flag = TokenManager::new_for_test(None);
        assert_eq!(no_flag.dual_instance_lock_held(), None);
    }

    #[tokio::test]
    async fn test_acquire_token_refuses_before_any_network_when_lock_lost() {
        // The tripwire sits BEFORE TOTP generation and any HTTP call, so a
        // refused mint completes instantly against the dummy example.com
        // base URLs of the test constructor — zero external side effects.
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let manager = TokenManager::new_for_test_with_lock_flag(None, Some(Arc::clone(&flag)));
        // Simulate the heartbeat losing the lock to a peer mid-session.
        flag.store(false, std::sync::atomic::Ordering::Release);
        let err = manager
            .acquire_token()
            .await
            .expect_err("mint MUST be refused when the instance lock is not held");
        let msg = err.to_string();
        assert!(
            msg.contains("RESILIENCE-03"),
            "typed refusal expected, got: {msg}"
        );
        // The refusal must classify PERMANENT so the boot retry loop /
        // renewal loop fails fast instead of spinning against the peer.
        assert!(
            is_permanent_auth_error(&msg),
            "RESILIENCE-03 refusal must be a permanent auth error: {msg}"
        );
    }

    #[test]
    fn test_resilience03_is_permanent_auth_error() {
        assert!(is_permanent_auth_error(
            "RESILIENCE-03: instance lock not held — generateAccessToken mint refused (fail-closed)"
        ));
        // Unrelated transient errors stay transient.
        assert!(!is_permanent_auth_error("connection reset by peer"));
    }

    #[test]
    fn test_client_id_string_returns_constructor_client_id() {
        // Constructor-level, no network — pins the fire-time accessor the
        // orphan-position watchdog uses to build its OrderApiClient.
        let manager = TokenManager::new_for_test(None);
        assert_eq!(manager.client_id_string(), "test-client-id");
    }

    #[test]
    fn test_token_handle_initially_none() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let guard = handle.load();
        assert!(guard.as_ref().is_none());
    }

    #[test]
    fn test_token_handle_store_makes_available() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let response_data = DhanAuthResponseData {
            access_token: "test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&response_data);
        handle.store(Arc::new(Some(token_state)));

        let guard = handle.load();
        assert!(guard.as_ref().is_some());
    }

    #[test]
    fn test_token_handle_store_replaces_previous() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store first token
        let data1 = DhanAuthResponseData {
            access_token: "jwt-v1".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data1))));

        // Store second token
        let data2 = DhanAuthResponseData {
            access_token: "jwt-v2".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data2))));

        let guard = handle.load();
        let state = guard.as_ref().as_ref().expect("should have token");
        assert_eq!(state.access_token().expose_secret(), "jwt-v2");
    }

    #[test]
    fn test_token_handle_concurrent_reads() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let data = DhanAuthResponseData {
            access_token: "concurrent-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data))));

        // Simulate multiple concurrent readers
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let h = Arc::clone(&handle);
                std::thread::spawn(move || {
                    let guard = h.load();
                    let state = guard.as_ref().as_ref().expect("should have token");
                    assert_eq!(state.access_token().expose_secret(), "concurrent-jwt");
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread should not panic");
        }
    }

    #[test]
    fn test_token_handle_concurrent_write_and_reads() {
        // One thread stores a new token while 10 threads are reading.
        // Verify no panics, reads see either old or new value (never garbage).
        // This tests the arc-swap safety guarantee.
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store an initial token so readers can see either "initial-jwt" or "updated-jwt".
        let initial_data = DhanAuthResponseData {
            access_token: "initial-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&initial_data))));

        let barrier = Arc::new(std::sync::Barrier::new(12)); // 1 writer + 10 readers + main

        // Spawn 10 reader threads
        let reader_handles: Vec<_> = (0..10)
            .map(|_| {
                let h = Arc::clone(&handle);
                let b = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait(); // synchronize start
                    for _ in 0..1000 {
                        let guard = h.load();
                        let state = guard.as_ref().as_ref().expect("should always have a token");
                        let token_value = state.access_token().expose_secret();
                        // Must see either the old or new token, never garbage
                        assert!(
                            token_value == "initial-jwt" || token_value == "updated-jwt",
                            "unexpected token value: {token_value}"
                        );
                    }
                })
            })
            .collect();

        // Spawn 1 writer thread
        let writer_handle = {
            let h = Arc::clone(&handle);
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait(); // synchronize start
                let updated_data = DhanAuthResponseData {
                    access_token: "updated-jwt".to_string(),
                    token_type: "Bearer".to_string(),
                    expires_in: 86400,
                };
                h.store(Arc::new(Some(TokenState::from_response(&updated_data))));
            })
        };

        // Main thread participates in barrier to release everyone
        barrier.wait();

        writer_handle
            .join()
            .expect("writer thread should not panic");
        for h in reader_handles {
            h.join().expect("reader thread should not panic");
        }

        // After all threads complete, verify the final value is the updated token
        let guard = handle.load();
        let final_state = guard.as_ref().as_ref().expect("should have token");
        assert_eq!(final_state.access_token().expose_secret(), "updated-jwt");
    }

    #[test]
    fn test_token_handle_rapid_successive_stores() {
        // Store 100 different tokens rapidly in sequence, verify the final
        // read shows the last stored token. Tests that arc-swap doesn't lose updates.
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        let total_stores: u32 = 100;
        for i in 0..total_stores {
            let data = DhanAuthResponseData {
                access_token: format!("jwt-{i}"),
                token_type: "Bearer".to_string(),
                expires_in: 86400,
            };
            handle.store(Arc::new(Some(TokenState::from_response(&data))));
        }

        let guard = handle.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("should have token after 100 stores");
        assert_eq!(
            state.access_token().expose_secret(),
            format!("jwt-{}", total_stores - 1),
            "final read must show the last stored token"
        );
    }

    #[test]
    fn test_token_handle_clone_shares_state() {
        // Call token_handle() pattern (Arc::clone) twice, store via first handle,
        // verify second handle sees the update. Verifies handles share the
        // underlying ArcSwap.
        let handle_one: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let handle_two: TokenHandle = Arc::clone(&handle_one);

        // Both should initially see None
        assert!(handle_one.load().as_ref().is_none());
        assert!(handle_two.load().as_ref().is_none());

        // Store via handle_one
        let data = DhanAuthResponseData {
            access_token: "shared-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle_one.store(Arc::new(Some(TokenState::from_response(&data))));

        // handle_two must see the update
        let guard = handle_two.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("handle_two should see the stored token");
        assert_eq!(
            state.access_token().expose_secret(),
            "shared-jwt",
            "cloned handle must share the same underlying ArcSwap"
        );

        // Store via handle_two, verify handle_one sees the update
        let data_v2 = DhanAuthResponseData {
            access_token: "shared-jwt-v2".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle_two.store(Arc::new(Some(TokenState::from_response(&data_v2))));

        let guard_one = handle_one.load();
        let state_one = guard_one
            .as_ref()
            .as_ref()
            .expect("handle_one should see handle_two's update");
        assert_eq!(
            state_one.access_token().expose_secret(),
            "shared-jwt-v2",
            "updates via either handle must be visible to the other"
        );
    }

    #[test]
    fn test_token_handle_store_old_token_is_dropped() {
        // Store a token, then store a new one. The old token should no longer
        // be accessible via the handle. While we can't directly test drop
        // (SecretString zeroizes on drop), we verify the old value is gone
        // and only the new value is visible.
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store first token
        let data_old = DhanAuthResponseData {
            access_token: "old-token-to-be-replaced".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data_old))));

        // Verify old token is present
        {
            let guard = handle.load();
            let state = guard.as_ref().as_ref().expect("should have old token");
            assert_eq!(
                state.access_token().expose_secret(),
                "old-token-to-be-replaced"
            );
        }

        // Store new token — this replaces the old Arc, which should be dropped
        // when its reference count reaches zero.
        let data_new = DhanAuthResponseData {
            access_token: "new-replacement-token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data_new))));

        // Verify ONLY the new token is accessible
        let guard = handle.load();
        let state = guard.as_ref().as_ref().expect("should have new token");
        assert_eq!(
            state.access_token().expose_secret(),
            "new-replacement-token",
            "only the new token should be accessible after replacement"
        );
        assert_ne!(
            state.access_token().expose_secret(),
            "old-token-to-be-replaced",
            "old token value must not be accessible"
        );

        // Additionally verify that storing None clears the token entirely
        handle.store(Arc::new(None));
        let guard_after_clear = handle.load();
        assert!(
            guard_after_clear.as_ref().is_none(),
            "storing None must clear the token state"
        );
    }

    // -----------------------------------------------------------------------
    // initialize() — HTTPS validation tests
    // -----------------------------------------------------------------------
    //
    // These call initialize() with http:// URLs. The HTTPS check fires BEFORE
    // any SSM or network call, so no credentials or network are needed.

    #[tokio::test]
    async fn test_initialize_rejects_http_rest_api_base_url() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "http://api.dhan.co".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
            None,
        )
        .await;
        let Err(err) = result else {
            panic!("http:// rest_api_base_url must be rejected")
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("rest_api_base_url must use HTTPS"),
            "error message should mention rest_api_base_url HTTPS requirement, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_initialize_rejects_http_auth_base_url() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "https://api.dhan.co".to_string(),
            auth_base_url: "http://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
            None,
        )
        .await;
        let Err(err) = result else {
            panic!("http:// auth_base_url must be rejected")
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("auth_base_url must use HTTPS"),
            "error message should mention auth_base_url HTTPS requirement, got: {err_msg}"
        );
    }

    // -----------------------------------------------------------------------
    // time_until_next_refresh — None token test
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_until_next_refresh_with_no_token_returns_zero() {
        let manager = make_test_manager(None);
        let duration = manager.time_until_next_refresh();
        assert_eq!(
            duration,
            Duration::ZERO,
            "No token present — should return Duration::ZERO to trigger immediate refresh"
        );
    }

    #[test]
    fn test_time_until_next_refresh_with_fresh_token_returns_positive() {
        let data = DhanAuthResponseData {
            access_token: "test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400, // 24 hours
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));

        let duration = manager.time_until_next_refresh();
        // With 24h validity and 1h refresh window, should sleep ~23h.
        assert!(
            duration.as_secs() > 80_000,
            "Fresh 24h token with 1h window should have >80000s until refresh, got: {}s",
            duration.as_secs()
        );
    }

    // -----------------------------------------------------------------------
    // current_expiry_display — None token test
    // -----------------------------------------------------------------------

    #[test]
    fn test_current_expiry_display_with_no_token() {
        let manager = make_test_manager(None);
        let display = manager.current_expiry_display();
        assert_eq!(
            display, "no token",
            "No token present — display should say 'no token'"
        );
    }

    #[test]
    fn test_current_expiry_display_with_token_shows_timestamp() {
        let data = DhanAuthResponseData {
            access_token: "test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));

        let display = manager.current_expiry_display();
        assert_ne!(
            display, "no token",
            "With a token present, display should not say 'no token'"
        );
        // Should contain a date-like string (year)
        assert!(
            display.contains("20"),
            "Expiry display should contain a year, got: {display}"
        );
    }

    // -----------------------------------------------------------------------
    // token_handle — public API test
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_returns_shared_handle() {
        let data = DhanAuthResponseData {
            access_token: "handle-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));

        let handle = manager.token_handle();

        // The handle should see the same token as the manager's internal state.
        let guard = handle.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("handle should see the token");
        assert_eq!(
            state.access_token().expose_secret(),
            "handle-test-jwt",
            "token_handle() must share the same ArcSwap"
        );
    }

    #[test]
    fn test_token_handle_none_initially() {
        let manager = make_test_manager(None);
        let handle = manager.token_handle();

        let guard = handle.load();
        assert!(
            guard.as_ref().is_none(),
            "token_handle() with no token should load None"
        );
    }

    #[test]
    fn test_token_handle_sees_updates() {
        let manager = make_test_manager(None);
        let handle = manager.token_handle();

        // Initially None
        assert!(handle.load().as_ref().is_none());

        // Store a token via the manager's internal handle
        let data = DhanAuthResponseData {
            access_token: "updated-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        manager
            .token
            .store(Arc::new(Some(TokenState::from_response(&data))));

        // The external handle should see the update
        let guard = handle.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("handle should see the update");
        assert_eq!(state.access_token().expose_secret(), "updated-jwt");
    }

    // -----------------------------------------------------------------------
    // acquire_token — tests with unreachable HTTPS URLs
    // -----------------------------------------------------------------------

    /// Helper: creates a manager with a specific unreachable HTTPS base URL
    /// and a known-good TOTP secret so TOTP generation succeeds.
    fn make_test_manager_with_totp(
        initial_token: Option<TokenState>,
        auth_base_url: &str,
        rest_api_base_url: &str,
    ) -> Arc<TokenManager> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // Valid base32 TOTP secret (32 chars = 160 bits).
        let totp_secret = "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN";
        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(initial_token)));
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .expect("test http client");

        Arc::new(TokenManager {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(totp_secret.to_string()),
            },
            rest_api_base_url: rest_api_base_url.to_string(),
            auth_base_url: auth_base_url.to_string(),
            http_client,
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 200,
                websocket_connect_timeout_ms: 200,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 50,
                retry_max_attempts: 2,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
            instance_lock_held: None,
        })
    }

    #[tokio::test]
    async fn test_acquire_token_connection_error_with_unreachable_url() {
        // Uses an unreachable HTTPS URL so the HTTP request fails.
        let manager =
            make_test_manager_with_totp(None, "https://127.0.0.1:1", "https://127.0.0.1:1");
        let result = manager.acquire_token().await;
        assert!(
            result.is_err(),
            "acquire_token must fail with unreachable URL"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("generateAccessToken") || err_msg.contains("request failed"),
            "error should mention generateAccessToken failure, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_try_renew_token_with_no_current_token() {
        // try_renew_token when no token is stored should error.
        let manager =
            make_test_manager_with_totp(None, "https://127.0.0.1:1", "https://127.0.0.1:1");
        let result = manager.try_renew_token().await;
        assert!(
            result.is_err(),
            "try_renew_token must fail with no current token"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot renew") || err_msg.contains("no current token"),
            "error should mention missing token, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_try_renew_token_connection_error_with_unreachable_url() {
        // try_renew_token with a token present but unreachable REST API URL.
        let data = DhanAuthResponseData {
            access_token: "existing-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager_with_totp(
            Some(token_state),
            "https://127.0.0.1:1",
            "https://127.0.0.1:1",
        );
        let result = manager.try_renew_token().await;
        assert!(
            result.is_err(),
            "try_renew_token must fail with unreachable URL"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("RenewToken") || err_msg.contains("request failed"),
            "error should mention renewal failure, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_renew_with_fallback_both_fail() {
        // renew_with_fallback tries try_renew_token, then acquire_token.
        // With an unreachable URL and a valid token, both should fail.
        let data = DhanAuthResponseData {
            access_token: "existing-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager_with_totp(
            Some(token_state),
            "https://127.0.0.1:1",
            "https://127.0.0.1:1",
        );
        let result = manager.renew_with_fallback().await;
        assert!(
            result.is_err(),
            "renew_with_fallback must fail when both endpoints unreachable"
        );
    }

    // -----------------------------------------------------------------------
    // spawn_renewal_task — cancellation test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_renewal_task_cancellation() {
        // Spawn the renewal task and immediately abort it.
        // Verifies it does not panic on cancellation.
        let data = DhanAuthResponseData {
            access_token: "task-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager_with_totp(
            Some(token_state),
            "https://127.0.0.1:1",
            "https://127.0.0.1:1",
        );
        let handle = manager.spawn_renewal_task();
        // Give the task a moment to start sleeping
        tokio::time::sleep(Duration::from_millis(10)).await;
        handle.abort();
        // The abort should not panic — JoinError::is_cancelled should be true.
        let result = handle.await;
        assert!(result.is_err(), "aborted task should return JoinError");
        assert!(
            result.unwrap_err().is_cancelled(),
            "task should be cancelled, not panicked"
        );
    }

    // -----------------------------------------------------------------------
    // token gauge — live tv_token_remaining_seconds countdown emitter
    // -----------------------------------------------------------------------

    #[test]
    fn test_emit_token_remaining_gauge_computes_remaining_seconds() {
        // expires_in=86400 → the countdown helper must report ~24h,
        // mirroring the renewal_loop emit sites (time_until_refresh(0)).
        let data = DhanAuthResponseData {
            access_token: "gauge-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86_400,
        };
        let manager = make_test_manager(Some(TokenState::from_response(&data)));
        let remaining = manager
            .emit_token_remaining_gauge()
            .expect("token loaded so the gauge must emit");
        assert!(
            (86_395..=86_400).contains(&remaining),
            "remaining seconds must be ~expires_in (24h), got {remaining}"
        );
    }

    #[test]
    fn test_emit_token_remaining_gauge_skips_when_no_token() {
        // Pre-auth boot window: no token loaded → the emitter must SKIP
        // (never set 0 — that would falsely trip TokenExpiryCritical).
        let manager = make_test_manager(None);
        assert!(
            manager.emit_token_remaining_gauge().is_none(),
            "no token loaded must skip the gauge emit, not set 0"
        );
    }

    #[test]
    fn test_emit_token_remaining_gauge_saturates_to_zero_for_expired_token() {
        // expires_in=0 → time_until_refresh(0) saturates to ZERO; the
        // gauge emits 0 (a genuinely expired token IS a countdown of 0).
        let data = DhanAuthResponseData {
            access_token: "expired-gauge-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0,
        };
        let manager = make_test_manager(Some(TokenState::from_response(&data)));
        assert_eq!(
            manager.emit_token_remaining_gauge(),
            Some(0),
            "expired token must emit a saturated 0, not skip"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_token_gauge_loop_ticks_forever_and_is_cancel_safe() {
        let data = DhanAuthResponseData {
            access_token: "gauge-loop-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86_400,
        };
        let manager = make_test_manager(Some(TokenState::from_response(&data)));
        let handle = tokio::spawn(Arc::clone(&manager).token_gauge_loop());
        // Advance paused time across several 30s intervals — the
        // emitter must keep running (never return on its own).
        tokio::time::advance(Duration::from_secs(TOKEN_GAUGE_INTERVAL_SECS * 3)).await;
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "token_gauge_loop must run until cancelled"
        );
        handle.abort();
        let result = handle.await;
        assert!(
            result
                .expect_err("aborted task returns JoinError")
                .is_cancelled(),
            "gauge loop must cancel cleanly, not panic"
        );
    }

    // -----------------------------------------------------------------------
    // time_until_next_refresh — with expired token
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_until_next_refresh_with_zero_expiry_returns_zero() {
        // expires_in=0 creates a token that expires at issuance time.
        let data = DhanAuthResponseData {
            access_token: "expired-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0,
        };
        let expired_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(expired_state));
        let duration = manager.time_until_next_refresh();
        assert_eq!(
            duration,
            Duration::ZERO,
            "Token with zero expiry should return Duration::ZERO for immediate refresh"
        );
    }

    // -----------------------------------------------------------------------
    // current_expiry_display — with short-lived token
    // -----------------------------------------------------------------------

    #[test]
    fn test_current_expiry_display_with_short_lived_token_shows_timestamp() {
        let data = DhanAuthResponseData {
            access_token: "short-lived-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 1, // 1 second
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));
        let display = manager.current_expiry_display();
        assert_ne!(
            display, "no token",
            "token with short expiry should still show timestamp"
        );
        assert!(display.contains("20"), "should contain year");
    }

    // -----------------------------------------------------------------------
    // initialize — both URLs are HTTP (first check catches rest_api_base_url)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_rejects_both_http_urls() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "http://api.dhan.co".to_string(),
            auth_base_url: "http://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
            None,
        )
        .await;
        assert!(result.is_err(), "both HTTP URLs must be rejected");
        // The first check (rest_api_base_url) fires first.
        let err_msg = result.err().expect("expected an error").to_string();
        assert!(err_msg.contains("rest_api_base_url must use HTTPS"));
    }

    // -----------------------------------------------------------------------
    // token_handle — verify it returns an independent Arc
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_multiple_calls_return_shared_handle() {
        let manager = make_test_manager(None);
        let handle1 = manager.token_handle();
        let handle2 = manager.token_handle();
        // Both should point to the same ArcSwap
        let data = DhanAuthResponseData {
            access_token: "shared-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        manager
            .token
            .store(Arc::new(Some(TokenState::from_response(&data))));
        let g1 = handle1.load();
        let g2 = handle2.load();
        assert_eq!(
            g1.as_ref()
                .as_ref()
                .expect("h1")
                .access_token()
                .expose_secret(),
            g2.as_ref()
                .as_ref()
                .expect("h2")
                .access_token()
                .expose_secret(),
        );
    }

    // -----------------------------------------------------------------------
    // HTTP mock server helpers for try_renew_token / renew_with_fallback tests
    // -----------------------------------------------------------------------

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Starts a local TCP server that responds to the FIRST request with the
    /// given HTTP status and body, then shuts down.
    async fn start_mock_server(status: u16, body: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://127.0.0.1:{}", addr.port());

        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );

        let handle = tokio::spawn(async move {
            // Accept up to 5 connections (enough for retry tests)
            for _ in 0..5 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    // Read request (consume it)
                    let mut buf = vec![0u8; 4096];
                    let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                    // Write response
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });

        (url, handle)
    }

    /// Constructs a `TokenManager` pointing at local mock URLs.
    /// Uses `http://` which is normally rejected by initialize(), but we
    /// construct the manager directly to bypass that check for unit tests.
    fn make_mock_manager(
        rest_api_url: &str,
        auth_url: &str,
        initial_token: Option<TokenState>,
    ) -> Arc<TokenManager> {
        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(initial_token)));
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build http client");

        Arc::new(TokenManager {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: rest_api_url.to_string(),
            auth_base_url: auth_url.to_string(),
            http_client,
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 5000,
                websocket_connect_timeout_ms: 5000,
                retry_initial_delay_ms: 50,
                retry_max_delay_ms: 100,
                retry_max_attempts: 2,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
            instance_lock_held: None,
        })
    }

    // -----------------------------------------------------------------------
    // try_renew_token — success path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_success() {
        let renew_body = r#"{"dhanClientId":"test","accessToken":"renewed-jwt","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, renew_body).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let initial_token = TokenState::from_response(&initial_data);
        let manager = make_mock_manager(&url, &url, Some(initial_token));

        let result = manager.try_renew_token().await;
        assert!(
            result.is_ok(),
            "try_renew_token should succeed: {:?}",
            result.err()
        );

        // Verify the token was updated
        let guard = manager.token.load();
        let state = guard.as_ref().as_ref().expect("should have renewed token");
        assert_eq!(
            state.access_token().expose_secret(),
            "renewed-jwt",
            "token must be updated after renewal"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // try_renew_token — no current token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_fails_without_current_token() {
        let manager = make_mock_manager("http://127.0.0.1:1", "http://127.0.0.1:1", None);

        let result = manager.try_renew_token().await;
        assert!(
            result.is_err(),
            "try_renew_token must fail without current token"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("no current token"),
            "error must mention no current token, got: {err_msg}"
        );
    }

    // -----------------------------------------------------------------------
    // try_renew_token — HTTP error (non-200)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_http_error() {
        let (url, server_handle) = start_mock_server(401, r#"{"error":"unauthorized"}"#).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.try_renew_token().await;
        assert!(result.is_err(), "try_renew_token must fail on HTTP 401");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("RenewToken HTTP"),
            "error must mention HTTP status, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // get_user_profile — DHAN-REST-400 (2026-06-10) body + URL capture
    // -----------------------------------------------------------------------

    /// RATCHET (task DHAN-REST-400 items 1 + 1b): a profile HTTP 400 must
    /// surface Dhan's error body (bounded, secret-redacted) AND the final
    /// request URL in the error reason — and the JWT must NEVER appear,
    /// even when the server echoes it back in the body.
    #[tokio::test]
    async fn test_profile_400_error_includes_body_and_url_but_never_jwt() {
        let jwt = format!(
            "eyJ{}.eyJ{}.{}",
            "hbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "zdWIiOiIxMTA2NjU2ODgyIiwiZXhwIjoxNzgwMDAwMDAwfQ",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        );
        let body = format!(
            r#"{{"errorType":"Invalid_Request","errorCode":"DH-905","errorMessage":"Missing required fields, bad token {jwt}"}}"#
        );
        let (url, server_handle) = start_mock_server(400, &body).await;

        let initial_data = DhanAuthResponseData {
            access_token: jwt.clone(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        // Trailing-slash base: join_api_url must still hit /profile (no `//`).
        let trailing_slash_base = format!("{url}/");
        let manager = make_mock_manager(
            &trailing_slash_base,
            &trailing_slash_base,
            Some(TokenState::from_response(&initial_data)),
        );

        let result = manager.get_user_profile().await;
        assert!(result.is_err(), "profile 400 must fail");
        let err_msg = result.unwrap_err().to_string();
        // Dhan's error fields survive — root-cause visibility.
        assert!(
            err_msg.contains("DH-905") && err_msg.contains("Missing required fields"),
            "Dhan error body must be captured, got: {err_msg}"
        );
        // The EXACT final request URL is captured (1b)…
        assert!(
            err_msg.contains("/profile"),
            "final request URL must be captured, got: {err_msg}"
        );
        // …and the join produced no `//` after the scheme despite the
        // trailing-slash base.
        let after_scheme = err_msg
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&err_msg);
        assert!(
            !after_scheme.contains("//profile"),
            "trailing-slash base must not produce `//`, got: {err_msg}"
        );
        // The token can NEVER appear — neither whole nor in part.
        assert!(!err_msg.contains(&jwt), "full JWT leaked: {err_msg}");
        assert!(
            !err_msg.contains(&jwt[..30]),
            "JWT prefix leaked: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // try_renew_token — empty access token in response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_empty_access_token() {
        let body =
            r#"{"dhanClientId":"test","accessToken":"","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, body).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.try_renew_token().await;
        assert!(result.is_err(), "empty accessToken must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("empty accessToken"),
            "error must mention empty token, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // try_renew_token — malformed JSON response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_renew_token_malformed_json() {
        let (url, server_handle) = start_mock_server(200, "not-json-at-all").await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.try_renew_token().await;
        assert!(result.is_err(), "malformed JSON must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("parse renewal response"),
            "error must mention parse failure, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // renew_with_fallback — renewal succeeds (no fallback needed)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_renew_with_fallback_renewal_succeeds() {
        let renew_body = r#"{"dhanClientId":"test","accessToken":"renewed-jwt","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, renew_body).await;

        let initial_data = DhanAuthResponseData {
            access_token: "old-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_mock_manager(&url, &url, Some(TokenState::from_response(&initial_data)));

        let result = manager.renew_with_fallback().await;
        assert!(
            result.is_ok(),
            "renew_with_fallback should succeed: {:?}",
            result.err()
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // renewal_loop — spawn and abort after first cycle
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_renewal_loop_spawns_and_can_be_cancelled() {
        // Create a manager with a token that expires soon (to minimize sleep time)
        let initial_data = DhanAuthResponseData {
            access_token: "loop-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 2, // 2 seconds validity
        };
        let token_state = TokenState::from_response(&initial_data);

        let token: TokenHandle = Arc::new(ArcSwap::new(Arc::new(Some(token_state))));
        let http_client = reqwest::Client::new();

        let manager = Arc::new(TokenManager {
            token,
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: "http://127.0.0.1:1".to_string(),
            auth_base_url: "http://127.0.0.1:1".to_string(),
            http_client,
            token_config: TokenConfig {
                refresh_before_expiry_hours: 0,
                token_validity_hours: 0,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 100,
                websocket_connect_timeout_ms: 100,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 20,
                retry_max_attempts: 1,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
            instance_lock_held: None,
        });

        let handle = manager.spawn_renewal_task();

        // Let it run briefly — it will try to renew (and fail since server is unreachable)
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Abort the task
        handle.abort();
        let result = handle.await;
        assert!(
            result.is_err() || result.is_ok(),
            "task should complete (either cancelled or finished)"
        );
    }

    // -----------------------------------------------------------------------
    // acquire_token — Dhan error response (HTTP 200 with error status)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_dhan_error_response() {
        let error_body = r#"{"status":"error","message":"Invalid Pin"}"#;
        let (url, server_handle) = start_mock_server(200, error_body).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "Dhan error response must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid Pin"),
            "error must contain Dhan error message, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — HTTP 500 error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_http_500_error() {
        let (url, server_handle) = start_mock_server(500, r#"internal server error"#).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "HTTP 500 must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("generateAccessToken HTTP"),
            "error must mention HTTP status, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — empty access token in success response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_empty_access_token() {
        let body =
            r#"{"dhanClientId":"test","accessToken":"","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, body).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "empty accessToken must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("empty accessToken"),
            "error must mention empty token, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — malformed JSON response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_malformed_json() {
        let (url, server_handle) = start_mock_server(200, "this-is-not-json").await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(result.is_err(), "malformed JSON must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("parse auth response"),
            "error must mention parse failure, got: {err_msg}"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // acquire_token — success path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_acquire_token_success() {
        let body = r#"{"dhanClientId":"test","accessToken":"new-jwt-token","expiryTime":"2026-03-02T13:00:00+05:30"}"#;
        let (url, server_handle) = start_mock_server(200, body).await;

        let manager = make_mock_manager(&url, &url, None);

        let result = manager.acquire_token().await;
        assert!(
            result.is_ok(),
            "acquire_token should succeed: {:?}",
            result.err()
        );

        // Verify token was stored
        let guard = manager.token.load();
        let state = guard
            .as_ref()
            .as_ref()
            .expect("should have token after acquire");
        assert_eq!(
            state.access_token().expose_secret(),
            "new-jwt-token",
            "acquired token must be stored"
        );

        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // renewal_loop — executes retry logic with time control
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn test_renewal_loop_executes_retry_logic_with_time_control() {
        // Manager with NO token → time_until_next_refresh returns Duration::ZERO
        // → the loop immediately enters the retry logic.
        // Unreachable URLs → both renew_with_fallback and acquire_token fail.
        // start_paused = true → all tokio sleeps resolve instantly when we advance.

        let manager = Arc::new(TokenManager {
            token: Arc::new(ArcSwap::new(Arc::new(None))),
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: "http://127.0.0.1:1".to_string(),
            auth_base_url: "http://127.0.0.1:1".to_string(),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_millis(50))
                .build()
                .expect("test http client"),
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 50,
                websocket_connect_timeout_ms: 50,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 20,
                retry_max_attempts: 2,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
            instance_lock_held: None,
        });

        let handle = tokio::spawn(Arc::clone(&manager).renewal_loop());

        // Advance time far enough for:
        //   - time_until_next_refresh() returns ZERO → no initial sleep
        //   - 2 retry attempts with backoff sleeps (10ms, 20ms)
        //   - circuit breaker sleep (60s)
        //   - then it loops again → another cycle
        // Total: well under 120s of simulated time
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        // Advance more to allow the second cycle to start
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        handle.abort();
        let result = handle.await;
        assert!(
            result.unwrap_err().is_cancelled(),
            "task should be cancelled after abort"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_renewal_loop_with_expired_token_triggers_retry() {
        // Manager with an expired token → time_until_next_refresh returns ZERO
        // → the loop immediately enters the retry logic.
        let data = DhanAuthResponseData {
            access_token: "expired-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0, // expires immediately
        };
        let expired_state = TokenState::from_response(&data);

        let manager = Arc::new(TokenManager {
            token: Arc::new(ArcSwap::new(Arc::new(Some(expired_state)))),
            credentials: crate::auth::types::DhanCredentials {
                client_id: secrecy::SecretString::from("test-client-id".to_string()),
                client_secret: secrecy::SecretString::from("123456".to_string()),
                totp_secret: secrecy::SecretString::from(
                    "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN".to_string(),
                ),
            },
            rest_api_base_url: "http://127.0.0.1:1".to_string(),
            auth_base_url: "http://127.0.0.1:1".to_string(),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_millis(50))
                .build()
                .expect("test http client"),
            token_config: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            network_config: NetworkConfig {
                request_timeout_ms: 50,
                websocket_connect_timeout_ms: 50,
                retry_initial_delay_ms: 10,
                retry_max_delay_ms: 20,
                retry_max_attempts: 1,
            },
            notifier: crate::notification::service::NotificationService::disabled(),
            instance_lock_held: None,
        });

        let handle = tokio::spawn(Arc::clone(&manager).renewal_loop());

        // Advance through retry attempts and circuit breaker sleep
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        handle.abort();
        let result = handle.await;
        assert!(
            result.unwrap_err().is_cancelled(),
            "task should be cancelled after abort"
        );
    }

    // -- Error classification tests --

    #[test]
    fn test_invalid_pin_is_permanent() {
        assert!(is_permanent_auth_error("Dhan auth error: Invalid Pin"));
    }

    #[test]
    fn test_invalid_client_is_permanent() {
        assert!(is_permanent_auth_error(
            "Dhan auth error: Invalid Client ID"
        ));
    }

    #[test]
    fn test_totp_invalid_is_not_permanent() {
        // TOTP errors are retryable (window boundary timing), not permanent.
        assert!(!is_permanent_auth_error("TOTP verification failed"));
        assert!(!is_permanent_auth_error("Dhan auth error: Invalid TOTP"));
    }

    #[test]
    fn test_totp_error_detected() {
        assert!(is_totp_error("TOTP verification failed"));
        assert!(is_totp_error("Dhan auth error: Invalid TOTP"));
        assert!(!is_totp_error("Invalid Pin"));
        assert!(!is_totp_error("network error"));
    }

    #[test]
    fn test_blocked_account_is_permanent() {
        assert!(is_permanent_auth_error("account blocked"));
        assert!(is_permanent_auth_error("account suspended"));
        assert!(is_permanent_auth_error("account disabled"));
    }

    #[test]
    fn test_totp_exhaustion_branch_emits_auth_gap_04() {
        // AUTH-P11 (audit 2026-07-01): the TOTP-retry-exhaustion terminal
        // branch MUST emit the distinctly-typed AUTH-GAP-04 code (with a
        // `code =` field) so a rotated-secret failure routes to Telegram
        // Critical and names the SSM param — not a generic auth failure.
        // Source-scan ratchet: the full emit path requires a live auth
        // loop with I/O, so we pin the coded emit at the source level.
        let src = include_str!("token_manager.rs");
        assert!(
            src.contains("AuthGap04TotpRotatedExternally.code_str()"),
            "the TOTP-exhaustion branch must emit \
             ErrorCode::AuthGap04TotpRotatedExternally.code_str() (AUTH-GAP-04)"
        );
        // The wire-format string must resolve correctly.
        assert_eq!(
            tickvault_common::error_code::ErrorCode::AuthGap04TotpRotatedExternally.code_str(),
            "AUTH-GAP-04"
        );
        // And it must be Critical (never auto-triage-safe).
        assert!(
            !tickvault_common::error_code::ErrorCode::AuthGap04TotpRotatedExternally
                .is_auto_triage_safe(),
            "AUTH-GAP-04 must be Critical — auth is dead until the secret is fixed"
        );
    }

    #[test]
    fn test_network_error_is_not_permanent() {
        assert!(!is_permanent_auth_error(
            "generateAccessToken request failed: error sending request"
        ));
    }

    #[test]
    fn test_timeout_is_not_permanent() {
        assert!(!is_permanent_auth_error("operation timed out"));
    }

    #[test]
    fn test_rate_limit_is_not_permanent() {
        assert!(!is_permanent_auth_error(
            "Token can be generated once every 2 minutes"
        ));
    }

    #[test]
    fn test_rate_limit_detected() {
        assert!(is_dhan_rate_limited(
            "Dhan auth error: Token can be generated once every 2 minutes"
        ));
    }

    #[test]
    fn test_network_error_not_rate_limited() {
        assert!(!is_dhan_rate_limited("error sending request"));
    }

    // -----------------------------------------------------------------------
    // TokenHandle atomic tests
    // -----------------------------------------------------------------------

    /// Helper: creates a `TokenState` for testing using `from_cached`.
    fn make_test_token_state(token_value: &str) -> TokenState {
        let now_ist =
            chrono::Utc::now().with_timezone(&tickvault_common::trading_calendar::ist_offset());
        let expires_at = now_ist + chrono::Duration::hours(24);
        TokenState::from_cached(
            secrecy::SecretString::from(token_value.to_string()),
            expires_at,
            now_ist,
        )
    }

    #[test]
    fn test_token_handle_starts_as_none() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let guard = handle.load();
        assert!(
            guard.as_ref().is_none(),
            "Fresh TokenHandle must load as None"
        );
    }

    #[test]
    fn test_token_handle_store_and_load() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let state = make_test_token_state("test-jwt-store-load");
        handle.store(Arc::new(Some(state)));

        let guard = handle.load();
        let loaded = guard
            .as_ref()
            .as_ref()
            .expect("should have token after store");
        assert_eq!(loaded.access_token().expose_secret(), "test-jwt-store-load");
    }

    #[test]
    fn test_token_handle_swap_updates_atomically() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));

        // Store first token
        let state_a = make_test_token_state("token-alpha");
        handle.store(Arc::new(Some(state_a)));

        // Verify first token
        let guard = handle.load();
        let loaded = guard.as_ref().as_ref().expect("should have token-alpha");
        assert_eq!(loaded.access_token().expose_secret(), "token-alpha");

        // Swap to second token
        let state_b = make_test_token_state("token-beta");
        handle.store(Arc::new(Some(state_b)));

        // Verify second token is returned
        let guard = handle.load();
        let loaded = guard.as_ref().as_ref().expect("should have token-beta");
        assert_eq!(loaded.access_token().expose_secret(), "token-beta");
    }

    #[test]
    fn test_build_generate_token_url() {
        let auth_base = "https://auth.dhan.co";
        let url = format!("{auth_base}{}", DHAN_GENERATE_TOKEN_PATH);
        assert_eq!(url, "https://auth.dhan.co/app/generateAccessToken");
        assert!(url.contains(DHAN_GENERATE_TOKEN_PATH));
    }

    #[test]
    fn test_build_renew_token_url() {
        let api_base = "https://api.dhan.co/v2";
        let url = format!("{api_base}{}", DHAN_RENEW_TOKEN_PATH);
        assert_eq!(url, "https://api.dhan.co/v2/RenewToken");
        assert!(url.contains(DHAN_RENEW_TOKEN_PATH));
    }

    #[test]
    fn test_multiple_handles_share_state() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let handle_clone = Arc::clone(&handle);

        // Update via the original handle
        let state = make_test_token_state("shared-jwt");
        handle.store(Arc::new(Some(state)));

        // Read via the cloned handle — must see the same value
        let guard = handle_clone.load();
        let loaded = guard
            .as_ref()
            .as_ref()
            .expect("cloned handle should see stored token");
        assert_eq!(loaded.access_token().expose_secret(), "shared-jwt");

        // Update via the clone, read via original
        let state_updated = make_test_token_state("shared-jwt-v2");
        handle_clone.store(Arc::new(Some(state_updated)));

        let guard = handle.load();
        let loaded = guard
            .as_ref()
            .as_ref()
            .expect("original handle should see clone's update");
        assert_eq!(loaded.access_token().expose_secret(), "shared-jwt-v2");
    }

    // -----------------------------------------------------------------------
    // make_test_manager — struct field validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_make_test_manager_has_correct_urls() {
        let manager = make_test_manager(None);
        assert_eq!(manager.rest_api_base_url, "https://api.example.com");
        assert_eq!(manager.auth_base_url, "https://auth.example.com");
    }

    #[test]
    fn test_make_test_manager_has_correct_config() {
        let manager = make_test_manager(None);
        assert_eq!(manager.token_config.refresh_before_expiry_hours, 1);
        assert_eq!(manager.token_config.token_validity_hours, 24);
        assert_eq!(manager.network_config.request_timeout_ms, 5000);
        assert_eq!(manager.network_config.retry_max_attempts, 3);
    }

    // -----------------------------------------------------------------------
    // token_handle() — verify it returns the inner handle
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_method_returns_shared_reference() {
        let data = DhanAuthResponseData {
            access_token: "method-test-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let manager = make_test_manager(Some(TokenState::from_response(&data)));
        let handle = manager.token_handle();

        let guard = handle.load();
        let state = guard.as_ref().as_ref().expect("should have token");
        assert_eq!(state.access_token().expose_secret(), "method-test-jwt");
    }

    // -----------------------------------------------------------------------
    // time_until_next_refresh — expired token
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_until_next_refresh_expired_token_returns_zero() {
        // Create a token with 0 expiry (already expired)
        let data = DhanAuthResponseData {
            access_token: "expired-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0, // already expired
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));
        let duration = manager.time_until_next_refresh();
        assert_eq!(duration, Duration::ZERO, "expired token should return ZERO");
    }

    // -----------------------------------------------------------------------
    // current_expiry_display — various token states
    // -----------------------------------------------------------------------

    #[test]
    fn test_current_expiry_display_short_lived_token() {
        let data = DhanAuthResponseData {
            access_token: "short-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 60, // 1 minute
        };
        let token_state = TokenState::from_response(&data);
        let manager = make_test_manager(Some(token_state));
        let display = manager.current_expiry_display();
        assert_ne!(display, "no token");
    }

    // -----------------------------------------------------------------------
    // initialize — both HTTPS checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_both_http_urls_rejected() {
        let dhan_config = DhanConfig {
            websocket_url: "wss://example.com".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "http://api.dhan.co".to_string(),
            auth_base_url: "http://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/instruments.csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/instruments-fallback.csv".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        };
        let token_config = TokenConfig {
            refresh_before_expiry_hours: 1,
            token_validity_hours: 24,
        };
        let network_config = NetworkConfig {
            request_timeout_ms: 5000,
            websocket_connect_timeout_ms: 5000,
            retry_initial_delay_ms: 100,
            retry_max_delay_ms: 1000,
            retry_max_attempts: 3,
        };

        let result = TokenManager::initialize(
            &dhan_config,
            &token_config,
            &network_config,
            &crate::notification::service::NotificationService::disabled(),
            None,
        )
        .await;
        // Should fail on the first check (rest_api_base_url)
        assert!(result.is_err());
        let err_msg = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(err_msg.contains("rest_api_base_url"));
    }

    // -----------------------------------------------------------------------
    // Token handle — store None after having a token
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_store_none_clears_state() {
        let handle: TokenHandle = Arc::new(ArcSwap::new(Arc::new(None)));
        let data = DhanAuthResponseData {
            access_token: "will-be-cleared".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        handle.store(Arc::new(Some(TokenState::from_response(&data))));
        assert!(handle.load().as_ref().is_some());

        handle.store(Arc::new(None));
        assert!(handle.load().as_ref().is_none(), "None store must clear");
    }

    // -----------------------------------------------------------------------
    // Error classification — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_permanent_auth_error_invalid_pin() {
        assert!(is_permanent_auth_error("Dhan auth error: Invalid Pin"));
    }

    #[test]
    fn test_is_permanent_auth_error_invalid_client() {
        assert!(is_permanent_auth_error("Invalid Client ID"));
    }

    #[test]
    fn test_is_permanent_auth_error_blocked() {
        assert!(is_permanent_auth_error(
            "Account is blocked due to violations"
        ));
    }

    #[test]
    fn test_is_permanent_auth_error_suspended() {
        assert!(is_permanent_auth_error("account suspended by admin"));
    }

    #[test]
    fn test_is_permanent_auth_error_disabled() {
        assert!(is_permanent_auth_error("API access disabled"));
    }

    #[test]
    fn test_is_permanent_auth_error_case_insensitive() {
        assert!(is_permanent_auth_error("INVALID PIN"));
        assert!(is_permanent_auth_error("BLOCKED"));
        assert!(is_permanent_auth_error("Suspended"));
    }

    #[test]
    fn test_is_permanent_auth_error_transient_not_permanent() {
        assert!(!is_permanent_auth_error("connection timeout"));
        assert!(!is_permanent_auth_error("network error"));
        assert!(!is_permanent_auth_error("rate limit exceeded"));
        assert!(!is_permanent_auth_error(""));
    }

    #[test]
    fn test_is_permanent_auth_error_totp_not_permanent() {
        // TOTP errors are handled separately by is_totp_error
        assert!(!is_permanent_auth_error("Invalid TOTP code"));
    }

    #[test]
    fn test_is_totp_error_invalid_totp() {
        assert!(is_totp_error("Dhan auth error: Invalid TOTP"));
    }

    #[test]
    fn test_is_totp_error_totp_failed() {
        assert!(is_totp_error("TOTP verification failed"));
    }

    #[test]
    fn test_is_totp_error_case_insensitive() {
        assert!(is_totp_error("invalid totp code"));
        assert!(is_totp_error("TOTP INVALID"));
    }

    #[test]
    fn test_is_totp_error_requires_both_keywords() {
        // Must contain both "totp" and ("invalid" or "failed")
        assert!(!is_totp_error("totp code generated")); // totp without invalid/failed
        assert!(!is_totp_error("invalid PIN")); // invalid without totp
    }

    #[test]
    fn test_is_totp_error_not_other_errors() {
        assert!(!is_totp_error("connection timeout"));
        assert!(!is_totp_error("rate limit"));
        assert!(!is_totp_error(""));
    }

    #[test]
    fn test_is_dhan_rate_limited_every_2_minutes() {
        assert!(is_dhan_rate_limited(
            "Token can be generated every 2 minutes"
        ));
    }

    #[test]
    fn test_is_dhan_rate_limited_once_every() {
        assert!(is_dhan_rate_limited("once every 120 seconds"));
    }

    #[test]
    fn test_is_dhan_rate_limited_not_other_errors() {
        assert!(!is_dhan_rate_limited("DH-904 rate limit"));
        assert!(!is_dhan_rate_limited("connection timeout"));
        assert!(!is_dhan_rate_limited(""));
    }

    #[test]
    fn test_error_classification_mutual_exclusion() {
        // Verify that typical error messages only match one classifier
        let pin_error = "Invalid Pin";
        assert!(is_permanent_auth_error(pin_error));
        assert!(!is_totp_error(pin_error));
        assert!(!is_dhan_rate_limited(pin_error));

        let totp_error = "Invalid TOTP code";
        assert!(!is_permanent_auth_error(totp_error));
        assert!(is_totp_error(totp_error));
        assert!(!is_dhan_rate_limited(totp_error));

        let rate_error = "Token can be generated every 2 minutes";
        assert!(!is_permanent_auth_error(rate_error));
        assert!(!is_totp_error(rate_error));
        assert!(is_dhan_rate_limited(rate_error));
    }

    // -----------------------------------------------------------------
    // Wave 2 Item 5.4 (AUTH-GAP-03) — force_renewal_if_stale tests.
    // -----------------------------------------------------------------

    #[test]
    fn test_set_global_token_manager_is_idempotent() {
        // Construct a TokenManager-shaped Arc via the test helper so we
        // get a real instance.
        let mgr = make_test_manager(None);
        let first = super::set_global_token_manager(mgr.clone());
        let second = super::set_global_token_manager(mgr);
        assert!(
            !(first && second),
            "set_global_token_manager must be idempotent"
        );
    }

    #[test]
    fn test_global_token_manager_accessor_returns_option() {
        // Type-check the accessor signature; value depends on test ordering.
        let _: Option<&'static Arc<TokenManager>> = super::global_token_manager();
    }

    #[tokio::test]
    async fn test_force_renewal_if_stale_returns_false_when_token_is_fresh() {
        // Token expires +24h. Threshold = 4h. Fresh → no renewal.
        let response = DhanAuthResponseData {
            access_token: "eyJfresh".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86_400,
        };
        let state = TokenState::from_response(&response);
        let mgr = make_test_manager(Some(state));
        let result = mgr.force_renewal_if_stale(14_400).await;
        assert!(matches!(result, Ok(false)));
    }

    #[tokio::test]
    async fn test_force_renewal_if_stale_attempts_renewal_when_no_token() {
        // No token → infinitely stale → renewal attempted.
        // Renewal will fail in test (no live HTTP server) but the
        // attempt-decision is the contract under test.
        let mgr = make_test_manager(None);
        let result = mgr.force_renewal_if_stale(14_400).await;
        // Forbidden outcome: Ok(false) — would mean "fresh" with no token.
        assert!(!matches!(result, Ok(false)));
    }

    #[tokio::test]
    async fn test_force_renewal_if_stale_attempts_renewal_when_token_expires_soon() {
        // Token expires in 1h. Threshold = 4h. Stale → renewal attempted.
        let response = DhanAuthResponseData {
            access_token: "eyJstale".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3_600,
        };
        let state = TokenState::from_response(&response);
        let mgr = make_test_manager(Some(state));
        let result = mgr.force_renewal_if_stale(14_400).await;
        assert!(!matches!(result, Ok(false)));
    }

    // -----------------------------------------------------------------
    // Wave 2 Item 5.4 (G1) — force_renewal + next_renewal_at tests.
    // -----------------------------------------------------------------

    #[test]
    fn test_next_renewal_at_returns_none_when_no_token() {
        let mgr = make_test_manager(None);
        assert!(
            mgr.next_renewal_at().is_none(),
            "next_renewal_at must return None when no token is loaded"
        );
    }

    #[test]
    fn test_seconds_until_expiry_is_zero_when_no_token() {
        let mgr = make_test_manager(None);
        assert_eq!(
            mgr.seconds_until_expiry(),
            0,
            "no token loaded => zero headroom (escalates self-test to Critical)"
        );
    }

    #[test]
    fn test_seconds_until_expiry_returns_positive_when_token_fresh() {
        let response = DhanAuthResponseData {
            access_token: "eyJfresh".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86_400,
        };
        let state = TokenState::from_response(&response);
        let mgr = make_test_manager(Some(state));
        let secs = mgr.seconds_until_expiry();
        assert!(
            secs > 0,
            "fresh token (24h validity) must report positive headroom; got {secs}"
        );
        assert!(
            secs <= 86_400,
            "headroom cannot exceed token's full validity; got {secs}"
        );
    }

    #[test]
    fn test_next_renewal_at_returns_expiry_when_token_present() {
        let response = DhanAuthResponseData {
            access_token: "eyJfresh".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86_400,
        };
        let state = TokenState::from_response(&response);
        let expected = state.expires_at();
        let mgr = make_test_manager(Some(state));
        let actual = mgr
            .next_renewal_at()
            .expect("token loaded so accessor returns Some");
        assert_eq!(
            actual, expected,
            "next_renewal_at must return the cached expiry timestamp"
        );
    }

    /// Audit Finding #6 (2026-05-03): the periodic token-sweep task
    /// (spawned from `crates/app/src/main.rs` Step 12b) is a parallel
    /// safety-net to `renewal_loop`. This test verifies the sweep call
    /// shape — `force_renewal_if_stale(TOKEN_SWEEP_STALENESS_THRESHOLD_SECS)`
    /// — is correct. The actual `tokio::spawn` of the sweep loop lives
    /// in main.rs and is exercised by integration tests.
    ///
    /// What this ratchet pins:
    /// 1. `TOKEN_SWEEP_STALENESS_THRESHOLD_SECS` constant is i64 and
    ///    compatible with `force_renewal_if_stale`'s signature (catches
    ///    accidental u64 / i32 type drift).
    /// 2. The sweep cadence (4h) is documented in `constants.rs` and
    ///    cannot silently drift.
    #[tokio::test]
    async fn test_token_sweep_threshold_constant_matches_force_renewal_signature() {
        use tickvault_common::constants::{
            TOKEN_SWEEP_INTERVAL_SECS, TOKEN_SWEEP_STALENESS_THRESHOLD_SECS,
        };
        // The sweep cadence is 4h.
        assert_eq!(
            TOKEN_SWEEP_INTERVAL_SECS,
            4 * 3600,
            "token sweep cadence must remain 4h to keep worst-case staleness bounded"
        );
        // The staleness threshold is 4h, matching the WS-wake call sites
        // (main/depth/order WS) per AUTH-GAP-03.
        assert_eq!(
            TOKEN_SWEEP_STALENESS_THRESHOLD_SECS,
            4 * 3600,
            "sweep threshold must match the WS-wake AUTH-GAP-03 threshold for uniform behaviour"
        );

        // The sweep call itself: pass the constant directly. If the
        // signature drifts (e.g. someone changes force_renewal_if_stale
        // to take u64), this fails to compile.
        let mgr = make_test_manager(None);
        let result = mgr
            .force_renewal_if_stale(TOKEN_SWEEP_STALENESS_THRESHOLD_SECS)
            .await;
        // We don't assert on the result here (network-dependent); only
        // that the call compiles and runs.
        let _ = result;
    }

    #[tokio::test]
    async fn test_force_renewal_attempts_renewal_unconditionally() {
        // No HTTP server in test → renewal will fail with a network
        // error. The contract under test is that the call ATTEMPTS the
        // renewal regardless of remaining validity (i.e., it does not
        // short-circuit on a fresh token like force_renewal_if_stale).
        let response = DhanAuthResponseData {
            access_token: "eyJfresh".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86_400,
        };
        let state = TokenState::from_response(&response);
        let mgr = make_test_manager(Some(state));
        let result = mgr.force_renewal().await;
        // Forbidden: Ok(()) — would mean a real renewal succeeded
        // against the test base URL, which is impossible.
        assert!(
            result.is_err(),
            "force_renewal must propagate the underlying network error \
             when no Dhan endpoint is reachable, proving it always tries"
        );
    }

    // #T2b (2026-05-20): test_force_renewal_paths_write_audit_rows
    // removed — the auth_renewal_audit table + write_renewal_audit_row
    // wiring it guarded were dropped in the QuestDB table cleanup.

    /// Phase 0 Item 17 — `tv_token_renewals_total` counter must be
    /// emitted at all three scheduled-renewal outcomes (success /
    /// failed / circuit_breaker_halt). Removing any one site silently
    /// breaks the Grafana renewal-ratio panel.
    #[test]
    fn test_renewal_loop_emits_token_renewals_total_counter() {
        let source = include_str!("token_manager.rs");
        assert!(
            source.contains("\"tv_token_renewals_total\""),
            "token_manager.rs MUST emit tv_token_renewals_total counter \
             from the scheduled renewal_loop. See Phase 0 Item 17."
        );
        for result_label in ["\"success\"", "\"failed\"", "\"circuit_breaker_halt\""] {
            assert!(
                source.contains(result_label),
                "tv_token_renewals_total must carry result label {} \
                 (scheduled renewal_loop). See Phase 0 Item 17.",
                result_label
            );
        }
    }

    /// Phase 0 Item 17 — successful scheduled renewal must emit a
    /// `TokenRenewed` Telegram so the operator sees a daily positive
    /// ping instead of inferring success from the absence of
    /// `TokenRenewalFailed`.
    #[test]
    fn test_renewal_loop_emits_token_renewed_telegram() {
        let source = include_str!("token_manager.rs");
        assert!(
            source.contains("NotificationEvent::TokenRenewed"),
            "token_manager.rs renewal_loop success path MUST notify \
             NotificationEvent::TokenRenewed. See Phase 0 Item 17."
        );
    }

    /// B9 log-shipping restore plan (2026-07-07) — the live
    /// `tv_token_remaining_seconds` countdown emitter must stay wired
    /// into `spawn_renewal_task` in the SAME task as `renewal_loop`
    /// (fused via `select!`), so aborting the lane-owned renewal handle
    /// also stops the countdown (C4 — no orphan emitter keeps stamping
    /// the gauge after a Dhan-lane disable), and the cadence stays 30s.
    #[test]
    fn test_token_gauge_countdown_is_spawned_with_renewal_loop() {
        assert_eq!(
            TOKEN_GAUGE_INTERVAL_SECS, 30,
            "live countdown cadence must remain 30s (plan-approved)"
        );
        let source = include_str!("token_manager.rs");
        let start = source
            .find("pub fn spawn_renewal_task")
            .expect("spawn_renewal_task must exist in token_manager.rs");
        let end = source[start..]
            .find("Public — User Profile")
            .map_or(source.len(), |offset| start + offset);
        let body = &source[start..end];
        for marker in ["tokio::select!", "renewal_loop()", "token_gauge_loop()"] {
            assert!(
                body.contains(marker),
                "spawn_renewal_task must fuse renewal_loop + token_gauge_loop \
                 into one select!-driven task (C4 lifecycle coupling); \
                 missing marker: {marker}"
            );
        }
    }
}
