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
//! **Why this lives in `app` (not `trading`):** like `cross_verify_1m_boot`,
//! the async runner is the composition root — it wires `trading`'s pure
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
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use secrecy::ExposeSecret;
use secrecy::zeroize::Zeroizing;
use tracing::{error, info, warn};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::auth::token_manager::TokenHandle;
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
    token_handle: TokenHandle,
    notifier: Arc<NotificationService>,
    calendar: Arc<TradingCalendar>,
    rest_api_base_url: String,
    client_id: String,
    dry_run: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let inner = tokio::spawn(orphan_watchdog_loop(
                token_handle.clone(),
                Arc::clone(&notifier),
                Arc::clone(&calendar),
                rest_api_base_url.clone(),
                client_id.clone(),
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
    token_handle: TokenHandle,
    notifier: Arc<NotificationService>,
    calendar: Arc<TradingCalendar>,
    rest_api_base_url: String,
    client_id: String,
    dry_run: bool,
) {
    // Build the HTTP client once, outside the loop.
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(
            ORPHAN_WATCHDOG_HTTP_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(Duration::from_secs(ORPHAN_WATCHDOG_HTTP_TIMEOUT_SECS))
        .build()
        .unwrap_or_default();
    let api = OrderApiClient::new(http, rest_api_base_url, client_id);

    loop {
        let sleep_dur = sleep_duration_until_orphan_watchdog(chrono::Utc::now().timestamp());
        info!(
            secs_until = sleep_dur.as_secs(),
            "orphan watchdog: sleeping until next 15:25:00 IST open-position check"
        );
        tokio::time::sleep(sleep_dur).await;

        let today_ist = ist_date_from_utc(chrono::Utc::now().timestamp());
        if calendar.is_trading_day(today_ist) {
            run_orphan_check_once(&api, &token_handle, &notifier, dry_run).await;
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
                    message: "🆘 3:25 PM open-position check SKIPPED — no active session \
                              token. Check your Dhan positions manually before the 3:30 PM \
                              close."
                        .to_string(),
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
                message: format!(
                    "🆘 3:25 PM open-position check FAILED ({safe_reason}). Could not read \
                     your positions — check Dhan manually before the 3:30 PM close."
                ),
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
}
