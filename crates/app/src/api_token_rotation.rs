//! API bearer-token SSM re-read / rotation task (W2#7, 2026-07-10).
//!
//! Closes audit row 13: the API bearer token
//! (`/tickvault/<env>/api/bearer-token`) was read ONCE at boot and held for
//! the process lifetime — rotating it in SSM required an app restart.
//!
//! This module runs a SUPERVISED periodic loop (WS-GAP-05 / DISK-WATCHER-01
//! house pattern) that re-reads the token from SSM every
//! [`API_TOKEN_RELOAD_INTERVAL_SECS`] (READ-ONLY `GetParameter` via the
//! existing `fetch_api_bearer_token` — this path NEVER writes SSM) and swaps
//! it into the shared lock-free holder via
//! `ApiAuthConfig::rotate_bearer_token`. A well-formed-but-mismatched bearer
//! on the request path additionally hints an out-of-band re-read (floored at
//! 60s inside the api crate), which this loop consumes via
//! `ApiAuthConfig::wait_oob_reload`.
//!
//! # Fail-open contract (coordinator-approved)
//! - SSM outage → the CURRENT token keeps working; the cycle is counted
//!   (`tv_api_token_reloads_total{outcome="failed"}`) and logged at `debug!`;
//!   an edge-latched `warn!` fires once after
//!   [`API_TOKEN_RELOAD_FAILURE_WARN_THRESHOLD`] consecutive failures
//!   (re-armed on the next successful read). Deliberately NO new ErrorCode:
//!   a failing re-read leaves auth WORKING on the old token —
//!   degraded-not-broken, not an operator page.
//! - Empty / shape-invalid SSM value → swap REJECTED
//!   (`outcome="rejected"` + edge-latched `warn!`); never blanks the live
//!   token, never becomes accept-all.
//!
//! # Honest rotation window
//! Operator updates SSM → new token live within ≤ ~5 minutes (periodic
//! cadence), or ~60s under active mismatched use (the OOB hint). The OLD
//! token dies at the swap instant — single-value semantics, no dual-accept
//! window.

use std::time::Duration;

use tickvault_api::middleware::{ApiAuthConfig, TokenReloadOutcome};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tracing::{debug, info, warn};

/// Periodic SSM re-read cadence. 5 minutes keeps the worst-case rotation
/// window small while staying trivially inside the SSM request budget
/// (~288 GetParameter calls/day).
pub const API_TOKEN_RELOAD_INTERVAL_SECS: u64 = 300;

/// Consecutive read failures before the single edge-latched `warn!` fires.
/// Below this, transient SSM blips stay at `debug!` + counter.
pub const API_TOKEN_RELOAD_FAILURE_WARN_THRESHOLD: u32 = 3;

/// Supervisor backoff between respawns of a dead reload loop.
const API_TOKEN_RELOAD_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Pure edge-latch decision for the consecutive-failure `warn!`: fires
/// exactly once per failure episode, at the threshold crossing, and stays
/// silent until the episode is cleared by a successful read.
fn should_warn_failure_streak(consecutive_failures: u32, already_warned: bool) -> bool {
    consecutive_failures >= API_TOKEN_RELOAD_FAILURE_WARN_THRESHOLD && !already_warned
}

/// Spawns the SUPERVISED reload task. The supervisor mirrors the
/// WS-GAP-05 / DISK-WATCHER-01 pattern: whenever the inner loop's
/// `JoinHandle` resolves (panic in unwind builds / cancel / impossible
/// clean return), it classifies the exit via the shared
/// `classify_join_exit`, increments
/// `tv_api_token_reload_respawn_total{reason}`, backs off 5s, and respawns
/// — so token rotation can never die silently. Auth itself is unaffected
/// while the loop is down: requests keep validating against the current
/// holder value.
///
/// Call sites: BOTH main.rs boot arms (fast crash-recovery + slow), gated
/// on `config.enabled` — a disabled dev config gets no task. Ratchet:
/// `crates/app/tests/api_token_rotation_wiring_guard.rs`.
pub fn spawn_supervised_api_token_reload(config: ApiAuthConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let loop_config = config.clone();
            let inner = tokio::spawn(run_api_token_reload_loop(loop_config));
            let join_result = inner.await;
            let reason = classify_join_exit(&join_result);
            metrics::counter!("tv_api_token_reload_respawn_total", "reason" => reason).increment(1);
            // Degraded-not-broken: auth keeps serving from the current
            // token, so this is a warn! (visibility), not an error! page.
            warn!(
                reason,
                "GAP-SEC-01: API token reload task exited — respawning \
                 (auth continues on the current token)"
            );
            tokio::time::sleep(Duration::from_secs(API_TOKEN_RELOAD_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// The inner reload loop: `select!` on the periodic tick OR the
/// 401-triggered out-of-band hint; each wake does ONE read-only SSM fetch
/// and swaps only a valid, genuinely-new value.
async fn run_api_token_reload_loop(config: ApiAuthConfig) {
    // First-sample-baseline discipline: pre-register every outcome series
    // at 0 so a delta consumer's dropped first sample is the harmless 0
    // (feed-stall round-5 lesson). Spawned from main.rs AFTER
    // observability::init_metrics installs the recorder, so these bind to
    // the real recorder, not the no-op.
    for outcome in ["ok", "unchanged", "failed", "rejected"] {
        metrics::counter!("tv_api_token_reloads_total", "outcome" => outcome).increment(0);
    }

    let mut consecutive_failures: u32 = 0;
    let mut warned_failure_episode = false;
    let mut warned_rejected_episode = false;

    let mut interval = tokio::time::interval(Duration::from_secs(API_TOKEN_RELOAD_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick — boot just read the token from SSM;
    // re-reading it instantly would be a wasted call.
    interval.tick().await;

    info!(
        cadence_secs = API_TOKEN_RELOAD_INTERVAL_SECS,
        "GAP-SEC-01: API bearer-token SSM re-read loop running — operator can \
         rotate /tickvault/<env>/api/bearer-token without a restart"
    );

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = config.wait_oob_reload() => {
                debug!("GAP-SEC-01: out-of-band token re-read hint received (mismatched bearer)");
            }
        }

        match tickvault_core::auth::secret_manager::fetch_api_bearer_token().await {
            Ok(token) => {
                consecutive_failures = 0;
                warned_failure_episode = false;
                match config.rotate_bearer_token(token) {
                    TokenReloadOutcome::Rotated => {
                        warned_rejected_episode = false;
                        metrics::counter!("tv_api_token_reloads_total", "outcome" => "ok")
                            .increment(1);
                        // One line per ACTUAL rotation — rare,
                        // operator-meaningful. Never logs the token value.
                        info!(
                            "GAP-SEC-01: API bearer token rotated from SSM — the \
                             previous token is no longer accepted"
                        );
                    }
                    TokenReloadOutcome::Unchanged => {
                        warned_rejected_episode = false;
                        metrics::counter!("tv_api_token_reloads_total", "outcome" => "unchanged")
                            .increment(1);
                        // The common cycle — debug! only, no log spam.
                        debug!("GAP-SEC-01: API bearer token re-read — unchanged");
                    }
                    TokenReloadOutcome::RejectedInvalid => {
                        metrics::counter!("tv_api_token_reloads_total", "outcome" => "rejected")
                            .increment(1);
                        // Edge-latched: a mis-seeded SSM value is
                        // operator-actionable, but repeating the warn every
                        // 5 minutes is spam. Re-armed by any non-rejected
                        // outcome.
                        if !warned_rejected_episode {
                            warned_rejected_episode = true;
                            // Names ALL rejection causes so an operator whose
                            // token boots fine but never rotates (>512 chars
                            // or embedded whitespace — the boot-vs-rotation
                            // shape asymmetry, 2026-07-10 hostile review
                            // MEDIUM #2) can self-diagnose from this line.
                            warn!(
                                "GAP-SEC-01: SSM returned a shape-invalid API bearer \
                                 token (empty, longer than 512 chars, or containing \
                                 whitespace/control chars) — swap rejected; auth \
                                 continues on the previously loaded token (fix \
                                 /tickvault/<env>/api/bearer-token)"
                            );
                        }
                    }
                }
            }
            Err(err) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                metrics::counter!("tv_api_token_reloads_total", "outcome" => "failed").increment(1);
                debug!(
                    consecutive = consecutive_failures,
                    error = %err,
                    "GAP-SEC-01: API bearer token SSM re-read failed — auth \
                     continues on the current token (fail-open)"
                );
                if should_warn_failure_streak(consecutive_failures, warned_failure_episode) {
                    warned_failure_episode = true;
                    warn!(
                        consecutive = consecutive_failures,
                        "GAP-SEC-01: API bearer token SSM re-read failing repeatedly — \
                         token rotation is degraded (auth still works on the \
                         previously loaded token); check SSM reachability"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reload_constants_pinned() {
        // 5-minute cadence: the coordinator-approved rotation window bound.
        assert_eq!(API_TOKEN_RELOAD_INTERVAL_SECS, 300);
        // 3 consecutive failures before the single warn.
        assert_eq!(API_TOKEN_RELOAD_FAILURE_WARN_THRESHOLD, 3);
        // The periodic cadence must dominate the api-side OOB floor, so the
        // OOB path is strictly a fast-path, never the dominant SSM load.
        assert!(API_TOKEN_RELOAD_INTERVAL_SECS > tickvault_api::middleware::OOB_RELOAD_FLOOR_SECS);
    }

    #[test]
    fn test_failure_warn_edge_latch_fires_once_at_threshold() {
        // Below threshold: never warns.
        assert!(!should_warn_failure_streak(1, false));
        assert!(!should_warn_failure_streak(2, false));
        // At threshold, not yet warned: fires.
        assert!(should_warn_failure_streak(3, false));
        // Past threshold but already warned this episode: stays silent.
        assert!(!should_warn_failure_streak(4, true));
        assert!(!should_warn_failure_streak(100, true));
        // A fresh episode (latch re-armed by a success) fires again.
        assert!(should_warn_failure_streak(3, false));
    }

    /// TEST-EXEMPT companions: `spawn_supervised_api_token_reload` and the
    /// inner loop are infinite supervised tasks against live SSM — their
    /// wiring + behavior are pinned by the pure-fn tests above, the
    /// middleware rotation tests (`crates/api/src/middleware.rs`), and the
    /// source-scan ratchet `crates/app/tests/api_token_rotation_wiring_guard.rs`
    /// (both main.rs boot arms spawn it; the loop reads SSM and rotates).
    #[test]
    fn test_spawn_supervised_api_token_reload_is_wired_into_main() {
        let main_src = include_str!("main.rs");
        let spawn_calls = main_src
            .matches("spawn_supervised_api_token_reload(")
            .count();
        assert!(
            spawn_calls >= 2,
            "both boot arms (fast crash-recovery + slow) must spawn the \
             supervised API token reload task; found {spawn_calls} call site(s)"
        );
    }
}
