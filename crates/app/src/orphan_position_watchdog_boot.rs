//! Phase 0 Item 20 — orphan-position 15:25 IST watchdog **boot runner**.
//!
//! The strategy is intraday option-buying — NO overnight derivative
//! positions are allowed. At **15:25:00 IST** every trading day this
//! supervised task:
//!
//!  1. Reads the live JWT (degrades — never panics — if absent).
//!  2. Calls Dhan REST `GET /v2/positions` (one retry on transient error).
//!  3. Runs the pure `evaluate_orphan_positions` verdict.
//!  4. Orphans → `error!(code = ORPHAN-POSITION-01)` + Telegram CRITICAL
//!     `OrphanPositionDetected` (dry-run = alert-only). Flat → Telegram
//!     INFO `OrphanPositionsClean` (positive ping, audit Rule 11).
//!  5. Fetch-failure / missing-token → CRITICAL degraded Telegram — it is
//!     **NEVER** reported as "clean" (false-OK avoidance, audit Rule 11).
//!
//! The 5-minute headroom before the 15:30 close lets the operator act.
//!
//! **Why this lives in `app` (not `trading`):** like the retired 15:31
//! cross-verify runner was (`cross_verify_1m_boot.rs`, deleted PR-C3
//! 2026-07-14), the async runner is the composition root — it wires `trading`'s pure
//! evaluator + `OrderApiClient` to `core`'s `TokenHandle`/`NotificationService`
//! and `common`'s `TradingCalendar`. The pure pieces stay in
//! `tickvault_trading::orphan_position_watchdog`.
//!
//! ## Daily compliance gate — NOT edge-triggered
//! Unlike the depth-stale alert, orphans page CRITICAL **every** day they
//! exist (the operator must exit before close each day; edge-suppression
//! would silently swallow day-2 of an ongoing overnight-risk breach).
//!
//! ## Safety contract (3-agent adversarial review, 2026-06-13)
//! - **No busy-loop:** a 60s floor-sleep after every run advances past the
//!   15:25:00 boundary before the next boundary is recomputed (the clock
//!   helper returns 0 at exactly 15:25:00).
//! - **No silent death:** the loop body never panics/`unwrap`s and is wrapped
//!   in a respawn supervisor (mirrors the WS-GAP-05 / DISK-WATCHER-01 pattern)
//!   — a daily safety gate must not disappear on a single transient fault.
//! - **No secret leak:** the JWT is exposed once in a short-lived block and
//!   never logged; every REST error body is routed through
//!   `capture_rest_error_body` before it reaches a log or Telegram.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use secrecy::ExposeSecret;
use secrecy::zeroize::Zeroizing;
use tracing::{error, info, warn};

use tickvault_api::feed_state::FeedRuntimeState;
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::auth::token_manager::{TokenHandle, global_token_manager};
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_trading::oms::OrderApiClient;
use tickvault_trading::orphan_position_watchdog::{
    OrphanPositionVerdict, evaluate_orphan_positions, sleep_duration_until_orphan_watchdog,
};

/// Floor sleep after each run (and each non-trading-day skip) before the
/// next 15:25 boundary is recomputed. MUST exceed the 1-second window where
/// `seconds_until_orphan_watchdog_ist(...) == 0`, so we use a full minute —
/// this closes the boundary busy-loop (3-agent CRITICAL finding).
const ORPHAN_WATCHDOG_POST_RUN_FLOOR_SECS: u64 = 60;
/// Delay before the single `get_positions` retry on a transient error.
const ORPHAN_WATCHDOG_FETCH_RETRY_DELAY_SECS: u64 = 2;
/// Backoff before the supervisor respawns a dead loop.
const ORPHAN_WATCHDOG_RESPAWN_BACKOFF_SECS: u64 = 30;
/// HTTP connect timeout for the positions REST call.
const ORPHAN_WATCHDOG_HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
/// HTTP overall timeout for the positions REST call.
const ORPHAN_WATCHDOG_HTTP_TIMEOUT_SECS: u64 = 30;
/// Max sample symbols carried in the Telegram payload (full list is in logs).
const ORPHAN_WATCHDOG_MAX_SAMPLE_SYMBOLS: usize = 5;

/// IST calendar date for a given UTC epoch-seconds instant. Pure helper so
/// the trading-day gate is unit-testable without a wall clock.
#[must_use]
pub fn ist_date_from_utc(now_utc_secs: i64) -> NaiveDate {
    let utc = DateTime::from_timestamp(now_utc_secs, 0).unwrap_or_default();
    (utc + ChronoDuration::seconds(i64::from(IST_UTC_OFFSET_SECONDS))).date_naive()
}

/// How the 15:25 IST check obtains its Dhan session (JWT + client id).
///
/// Re-home 2026-07-14: the watchdog previously ran ONLY from the Dhan-gated
/// `spawn_post_market_tasks` family in `main.rs` — dead on `dhan_enabled =
/// false` boots (the production default since the 2026-07-13 Dhan live-WS
/// retirement). The process-global prefix spawn cannot thread a
/// `TokenHandle`/client id at boot (the Dhan REST stack mints asynchronously
/// in its Phase 2), so this enum lets each call site choose:
#[derive(Clone)]
pub enum WatchdogAuth {
    /// Boot-time handles, byte-identical behaviour to the pre-2026-07-14
    /// signature. Used by the fast crash-recovery arm's
    /// `spawn_post_market_tasks` family (both are in scope there).
    Static {
        /// O(1) atomic JWT handle from the lane-owned `TokenManager`.
        token_handle: TokenHandle,
        /// Dhan client id (plain header value — not secret-classed).
        client_id: String,
    },
    /// Resolve the Dhan session at each 15:25 fire: PREFER the live
    /// lane-owned `TokenManager` on `FeedRuntimeState` (fresh across a
    /// runtime lane stop→re-start — the D2c `gauge_token_headroom_secs`
    /// pattern in main.rs), FALL BACK to `global_token_manager()` (the
    /// OnceLock is registered by BOTH the Dhan lane and the Dhan REST
    /// stack's Phase 2, so a dhan-off boot has it well before 15:25 — but
    /// it is set-once and would pin a dead boot manager across a runtime
    /// stop→re-start, hence the live-first order). Neither present at fire
    /// time = LOUD degraded Critical page — never a clean signal.
    GlobalAtFireTime {
        /// Runtime feed state carrying the live lane-owned manager slot.
        feed_runtime: Arc<FeedRuntimeState>,
    },
}

/// Fire-time manager preference for [`WatchdogAuth::GlobalAtFireTime`]: the
/// LIVE lane-owned manager wins over the boot-time global `OnceLock` (the
/// D2c `gauge_token_headroom_secs` pattern — the OnceLock is set-once at
/// first boot and forever points at the dead boot-time manager after a
/// runtime lane stop→re-start; the live slot tracks the CURRENT manager).
/// Generic + pure so the preference order is unit-testable without a real
/// `TokenManager` (its only test constructor is `#[cfg(test)]`-gated inside
/// `tickvault-core`).
fn prefer_live_session<T>(live: Option<T>, global_fallback: Option<T>) -> Option<T> {
    live.or(global_fallback)
}

/// Resolve the (JWT handle, client id) pair for one 15:25 fire. `Static` is
/// byte-identical to the pre-2026-07-14 signature; `GlobalAtFireTime`
/// prefers the live lane-owned manager and falls back to the global
/// OnceLock (both reads deliberately live HERE, not in main.rs — main.rs
/// ratchets exactly one `global_token_manager()` read in its production
/// region). `None` = no broker session anywhere — the caller fires the
/// degraded arm.
fn resolve_watchdog_session(auth: &WatchdogAuth) -> Option<(TokenHandle, String)> {
    match auth {
        WatchdogAuth::Static {
            token_handle,
            client_id,
        } => Some((token_handle.clone(), client_id.clone())),
        WatchdogAuth::GlobalAtFireTime { feed_runtime } => prefer_live_session(
            feed_runtime.live_token_manager(),
            global_token_manager().cloned(),
        )
        .map(|tm| (tm.token_handle(), tm.client_id_string())),
    }
}

/// Degraded-arm Telegram wording: fire-time resolution found NO broker
/// session anywhere (no live lane manager AND no global manager). Pure so
/// the operator-facing wording is unit-pinned (the 10 Telegram
/// commandments — plain English, one decision, action verb).
fn degraded_no_session_message() -> String {
    "🆘 3:25 PM open-position check could NOT run — no broker session exists. \
     Check your Dhan app manually before the 3:30 PM close."
        .to_string()
}

/// Degraded-arm Telegram wording: a manager exists but holds no active
/// token at fire time (mint failed / token invalidated). Pure — see
/// [`degraded_no_session_message`].
fn degraded_no_token_message() -> String {
    "🆘 3:25 PM open-position check SKIPPED — no active session token. \
     Check your Dhan positions manually before the 3:30 PM close."
        .to_string()
}

/// Degraded-arm Telegram wording: the positions REST fetch failed after one
/// retry. `safe_reason` MUST already be redacted via
/// `capture_rest_error_body` (the caller's contract). Pure — see
/// [`degraded_no_session_message`].
fn degraded_fetch_failed_message(safe_reason: &str) -> String {
    format!(
        "🆘 3:25 PM open-position check FAILED ({safe_reason}). Could not read \
         your positions — check Dhan manually before the 3:30 PM close."
    )
}

/// Process-global once-guard: the watchdog family may be spawned from BOTH
/// the process-global prefix and the fast crash-recovery arm's
/// `spawn_post_market_tasks`. The two call sites are mutually exclusive by
/// control flow today (the fast arm early-returns before the process-global
/// block) — this guard is belt-and-braces against future refactors making
/// both reachable (a double 15:25 page would violate the one-decision-per-
/// Telegram commandment).
static ORPHAN_WATCHDOG_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim the single orphan-watchdog spawn slot. `true` = this caller owns
/// the spawn; `false` = another call site already spawned it.
fn claim_orphan_watchdog_once() -> bool {
    ORPHAN_WATCHDOG_CLAIMED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Test-only reset so the once-guard is exercisable deterministically.
#[cfg(test)]
fn reset_orphan_watchdog_claim() {
    ORPHAN_WATCHDOG_CLAIMED.store(false, Ordering::SeqCst);
}

/// Spawn the supervised orphan-position watchdog. The supervisor catches a
/// panicked/exited inner loop and respawns it after a backoff, so the daily
/// 15:25 IST compliance gate can never silently disappear.
///
/// `dry_run` only shapes the Telegram wording — this runner is alert-only by
/// construction (no order/cancel call exists on any path; auto-exit is a
/// future Phase-1 change, out of scope until the sandbox window ends).
// Pure decision logic is covered by `ist_date_from_utc` tests + the
// trading-crate `evaluate_orphan_positions` / clock-helper tests; source-scan
// wiring is pinned by `crates/app/tests/orphan_position_watchdog_wiring_guard.rs`.
// TEST-EXEMPT: live-token + REST + tokio supervisor wiring (no pure logic here).
pub fn spawn_supervised_orphan_position_watchdog(
    auth: WatchdogAuth,
    notifier: Arc<NotificationService>,
    calendar: Arc<TradingCalendar>,
    rest_api_base_url: String,
    dry_run: bool,
) -> tokio::task::JoinHandle<()> {
    if !claim_orphan_watchdog_once() {
        info!(
            "orphan watchdog already spawned by another call site — skipping \
             duplicate spawn (once-guard)"
        );
        return tokio::spawn(async {});
    }
    tokio::spawn(async move {
        loop {
            let inner = tokio::spawn(orphan_watchdog_loop(
                auth.clone(),
                Arc::clone(&notifier),
                Arc::clone(&calendar),
                rest_api_base_url.clone(),
                dry_run,
            ));
            match inner.await {
                // The inner loop never returns normally; a clean return means
                // it broke out unexpectedly — respawn it.
                Ok(()) => {
                    error!(
                        "orphan watchdog loop exited unexpectedly — respawning \
                         the daily 15:25 IST open-position safety gate"
                    );
                }
                Err(join_err) if join_err.is_panic() => {
                    error!(
                        "orphan watchdog loop PANICKED — respawning the daily \
                         15:25 IST open-position safety gate"
                    );
                }
                // Cancelled (graceful shutdown) — stop supervising.
                Err(_) => break,
            }
            metrics::counter!("tv_orphan_position_watchdog_respawns_total").increment(1);
            tokio::time::sleep(Duration::from_secs(ORPHAN_WATCHDOG_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// The perpetual daily loop. Never panics, never `unwrap`s — every fault
/// path logs + (where operator-actionable) pages, then continues.
async fn orphan_watchdog_loop(
    auth: WatchdogAuth,
    notifier: Arc<NotificationService>,
    calendar: Arc<TradingCalendar>,
    rest_api_base_url: String,
    dry_run: bool,
) {
    // Build the HTTP client once, outside the loop (the per-fire
    // `OrderApiClient` shares it via reqwest's internal Arc — no per-fire
    // TLS/resolver init, the HTTP-CLIENT-01 lesson).
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(
            ORPHAN_WATCHDOG_HTTP_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(Duration::from_secs(ORPHAN_WATCHDOG_HTTP_TIMEOUT_SECS))
        .build()
        .unwrap_or_default();

    loop {
        let sleep_dur = sleep_duration_until_orphan_watchdog(chrono::Utc::now().timestamp());
        info!(
            secs_until = sleep_dur.as_secs(),
            "orphan watchdog: sleeping until next 15:25:00 IST open-position check"
        );
        tokio::time::sleep(sleep_dur).await;

        let today_ist = ist_date_from_utc(chrono::Utc::now().timestamp());
        if calendar.is_trading_day(today_ist) {
            // Resolve the Dhan session at fire time. `Static` behaves
            // exactly as the pre-2026-07-14 signature; `GlobalAtFireTime`
            // prefers the live lane-owned manager and falls back to the
            // global OnceLock (see `resolve_watchdog_session` — the reads
            // deliberately live in THIS module, not main.rs).
            match resolve_watchdog_session(&auth) {
                Some((token_handle, client_id)) => {
                    let api =
                        OrderApiClient::new(http.clone(), rest_api_base_url.clone(), client_id);
                    run_orphan_check_once(&api, &token_handle, &notifier, dry_run).await;
                }
                None => {
                    // Degraded arm (fire-time, no broker session at all —
                    // fully-dhan-less boot, or the Dhan REST stack parked /
                    // still pre-Phase-2). NEVER a clean signal (audit Rule
                    // 11) — mirrors the existing no-token degraded arm's
                    // un-coded Custom-Critical style.
                    error!(
                        "orphan watchdog: no broker session at 15:25 IST — the \
                         orphan-position check could NOT run (no live-lane or \
                         global token manager registered); operator must check \
                         Dhan manually before the 15:30 close (degraded — NOT \
                         reported as flat)"
                    );
                    notifier.notify(NotificationEvent::Custom {
                        message: degraded_no_session_message(),
                    });
                    metrics::counter!("tv_orphan_position_watchdog_fetch_failures_total")
                        .increment(1);
                }
            }
        } else {
            info!(
                date = %today_ist,
                "orphan watchdog: non-trading day — skipping 15:25 IST check"
            );
        }

        // Floor-sleep past the 15:25:00 minute before recomputing the next
        // boundary — closes the busy-loop (clock helper returns 0 at 15:25:00).
        tokio::time::sleep(Duration::from_secs(ORPHAN_WATCHDOG_POST_RUN_FLOOR_SECS)).await;
    }
}

/// One 15:25 IST compliance check. Alert-only (dry-run/sandbox). Never
/// reports "clean" on a token/fetch failure (false-OK avoidance).
async fn run_orphan_check_once(
    api: &OrderApiClient,
    token_handle: &TokenHandle,
    notifier: &Arc<NotificationService>,
    dry_run: bool,
) {
    metrics::counter!("tv_orphan_position_watchdog_runs_total").increment(1);

    // Expose the JWT once, in a short-lived block; never logged.
    let token = {
        let guard = token_handle.load();
        match guard.as_ref() {
            Some(state) => Zeroizing::new(state.access_token().expose_secret().to_string()),
            None => {
                error!(
                    "orphan watchdog: no active session token at 15:25 IST — cannot \
                     verify open positions; operator must check Dhan manually before \
                     the 15:30 close (degraded — NOT reported as flat)"
                );
                notifier.notify(NotificationEvent::Custom {
                    message: degraded_no_token_message(),
                });
                metrics::counter!("tv_orphan_position_watchdog_fetch_failures_total").increment(1);
                return;
            }
        }
    };

    let positions = match fetch_positions_with_retry(api, &token).await {
        Ok(positions) => positions,
        Err(safe_reason) => {
            error!(
                reason = %safe_reason,
                "orphan watchdog: get_positions FAILED after one retry at 15:25 IST — \
                 operator must verify open positions manually before the 15:30 close \
                 (degraded — NOT reported as flat)"
            );
            notifier.notify(NotificationEvent::Custom {
                message: degraded_fetch_failed_message(&safe_reason),
            });
            metrics::counter!("tv_orphan_position_watchdog_fetch_failures_total").increment(1);
            return;
        }
    };

    match evaluate_orphan_positions(&positions) {
        OrphanPositionVerdict::NoOrphans => {
            info!("orphan watchdog: account flat at 15:25 IST — no open positions");
            notifier.notify(NotificationEvent::OrphanPositionsClean);
        }
        OrphanPositionVerdict::OrphansDetected {
            count,
            total_abs_net_qty,
            orphans,
        } => {
            let sample_symbols: Vec<String> = orphans
                .iter()
                .take(ORPHAN_WATCHDOG_MAX_SAMPLE_SYMBOLS)
                .map(|orphan| orphan.trading_symbol.clone())
                .collect();
            error!(
                code = ErrorCode::OrphanPosition01Detected.code_str(),
                count,
                total_abs_net_qty,
                "orphan watchdog: OPEN positions detected at 15:25 IST — operator must \
                 exit before the 15:30 close"
            );
            notifier.notify(NotificationEvent::OrphanPositionDetected {
                count,
                total_abs_net_qty,
                sample_symbols,
                dry_run,
            });
        }
    }
}

/// `get_positions` with one retry on a transient error. The Dhan error body
/// is redacted via `capture_rest_error_body` before it is ever surfaced.
async fn fetch_positions_with_retry(
    api: &OrderApiClient,
    token: &str,
) -> Result<Vec<tickvault_trading::oms::types::DhanPositionResponse>, String> {
    match api.get_positions(token).await {
        Ok(positions) => return Ok(positions),
        Err(err) => {
            let safe = capture_rest_error_body(&err.to_string()).to_string();
            warn!(
                reason = %safe,
                "orphan watchdog: get_positions failed — retrying once"
            );
        }
    }
    tokio::time::sleep(Duration::from_secs(ORPHAN_WATCHDOG_FETCH_RETRY_DELAY_SECS)).await;
    api.get_positions(token)
        .await
        .map_err(|err| capture_rest_error_body(&err.to_string()).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ist_date_from_utc_at_utc_midnight_is_same_day_ist() {
        // UTC 1970-01-01 00:00:00 → IST 05:30 same day.
        assert_eq!(
            ist_date_from_utc(0),
            NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date")
        );
    }

    #[test]
    fn test_ist_date_from_utc_late_utc_rolls_to_next_ist_day() {
        // UTC 2026-06-12 20:00:00 → IST 2026-06-13 01:30 → next day.
        let utc = DateTime::parse_from_rfc3339("2026-06-12T20:00:00Z")
            .expect("parse")
            .timestamp();
        assert_eq!(
            ist_date_from_utc(utc),
            NaiveDate::from_ymd_opt(2026, 6, 13).expect("valid date")
        );
    }

    #[test]
    fn test_ist_date_from_utc_before_ist_midnight_same_utc_day() {
        // UTC 2026-06-12 10:00:00 → IST 2026-06-12 15:30 → same day.
        let utc = DateTime::parse_from_rfc3339("2026-06-12T10:00:00Z")
            .expect("parse")
            .timestamp();
        assert_eq!(
            ist_date_from_utc(utc),
            NaiveDate::from_ymd_opt(2026, 6, 12).expect("valid date")
        );
    }

    #[test]
    fn test_floor_sleep_exceeds_boundary_zero_window() {
        // The post-run floor MUST be > 1s so we advance past the 1-second
        // window where the clock helper returns 0 at exactly 15:25:00.
        assert!(ORPHAN_WATCHDOG_POST_RUN_FLOOR_SECS > 1);
    }

    #[test]
    fn test_orphan_watchdog_claim_once_guard() {
        // Single test owns the guard (no parallel-test race): claim wins
        // once, the second claim is refused, and the test-only reset re-arms.
        reset_orphan_watchdog_claim();
        assert!(claim_orphan_watchdog_once(), "first claim must win");
        assert!(
            !claim_orphan_watchdog_once(),
            "second claim must be refused (once-guard)"
        );
        reset_orphan_watchdog_claim();
        assert!(claim_orphan_watchdog_once(), "claim must win after reset");
        reset_orphan_watchdog_claim();
    }

    #[test]
    fn test_watchdog_auth_enum_arms_construct() {
        // Compile-path pin: both arms are constructible and Clone — the
        // supervisor respawn loop clones the auth per inner spawn.
        let handle: TokenHandle = Arc::new(arc_swap::ArcSwap::new(Arc::new(None)));
        let static_arm = WatchdogAuth::Static {
            token_handle: handle,
            client_id: "test-client-id".to_string(),
        };
        let global_arm = WatchdogAuth::GlobalAtFireTime {
            feed_runtime: Arc::new(FeedRuntimeState::default()),
        };
        match static_arm.clone() {
            WatchdogAuth::Static { client_id, .. } => assert_eq!(client_id, "test-client-id"),
            WatchdogAuth::GlobalAtFireTime { .. } => panic!("expected Static arm"),
        }
        assert!(matches!(
            global_arm.clone(),
            WatchdogAuth::GlobalAtFireTime { .. }
        ));
    }

    #[test]
    fn test_prefer_live_session_prefers_live_when_both_present() {
        // The live lane-owned manager MUST win over the boot-time global
        // OnceLock (the D2c staleness class: the OnceLock pins the dead
        // boot manager forever after a runtime lane stop→re-start).
        assert_eq!(
            prefer_live_session(Some("live"), Some("global")),
            Some("live")
        );
    }

    #[test]
    fn test_prefer_live_session_falls_back_to_global_when_live_absent() {
        // Boot-OFF / Groww-only / pre-start: no live lane manager — the
        // global OnceLock (registered by the Dhan REST stack's Phase 2)
        // is the fallback.
        assert_eq!(prefer_live_session(None, Some("global")), Some("global"));
    }

    #[test]
    fn test_prefer_live_session_none_when_both_absent() {
        // Fully-dhan-less shape: no session anywhere — the caller must
        // fire the degraded arm (never a clean signal).
        assert_eq!(prefer_live_session::<&str>(None, None), None);
    }

    #[test]
    fn test_resolve_watchdog_session_static_arm_returns_boot_handles() {
        // `Static` must be byte-identical to the pre-2026-07-14 behaviour:
        // the boot-time handle + client id come back unchanged.
        let handle: TokenHandle = Arc::new(arc_swap::ArcSwap::new(Arc::new(None)));
        let auth = WatchdogAuth::Static {
            token_handle: handle,
            client_id: "static-client-id".to_string(),
        };
        let (_token_handle, client_id) =
            resolve_watchdog_session(&auth).expect("Static arm always resolves");
        assert_eq!(client_id, "static-client-id");
    }

    #[test]
    fn test_resolve_watchdog_session_global_arm_none_when_no_managers() {
        // Mirror of main.rs's `gauge_token_headroom_falls_back_to_global_...`
        // test: in this test binary the global OnceLock is never set and a
        // fresh FeedRuntimeState has no live manager, so resolution is None
        // (the degraded-arm trigger). Precondition asserted so a future
        // test that installs the global fails HERE with a clear message.
        let feed_runtime = Arc::new(FeedRuntimeState::default());
        assert!(
            feed_runtime.live_token_manager().is_none(),
            "fresh FeedRuntimeState must carry no live lane manager"
        );
        assert!(
            global_token_manager().is_none(),
            "precondition: the global TokenManager OnceLock must be unset in \
             this test binary (no lib test installs it)"
        );
        let auth = WatchdogAuth::GlobalAtFireTime { feed_runtime };
        assert!(
            resolve_watchdog_session(&auth).is_none(),
            "no live + no global manager must resolve to None (degraded arm)"
        );
    }

    #[test]
    fn test_degraded_no_session_message_wording() {
        // Operator-facing wording pin (10 Telegram commandments): severity
        // emoji, plain English, the manual action, the 3:30 PM deadline.
        let msg = degraded_no_session_message();
        assert!(msg.starts_with("🆘"), "severity emoji must lead: {msg}");
        assert!(
            msg.contains("could NOT run — no broker session exists"),
            "must say the check could not run: {msg}"
        );
        assert!(
            msg.contains("Check your Dhan app manually before the 3:30 PM close"),
            "must carry the manual action + deadline: {msg}"
        );
    }

    #[test]
    fn test_degraded_no_token_message_wording() {
        let msg = degraded_no_token_message();
        assert!(msg.starts_with("🆘"), "severity emoji must lead: {msg}");
        assert!(
            msg.contains("SKIPPED — no active session token"),
            "must say why the check was skipped: {msg}"
        );
        assert!(
            msg.contains("before the 3:30 PM close"),
            "must carry the deadline: {msg}"
        );
    }

    #[test]
    fn test_degraded_fetch_failed_message_embeds_redacted_reason() {
        let msg = degraded_fetch_failed_message("timeout after 30s");
        assert!(msg.starts_with("🆘"), "severity emoji must lead: {msg}");
        assert!(
            msg.contains("FAILED (timeout after 30s)"),
            "must embed the redacted reason: {msg}"
        );
        assert!(
            msg.contains("check Dhan manually before the 3:30 PM close"),
            "must carry the manual action + deadline: {msg}"
        );
    }
}
