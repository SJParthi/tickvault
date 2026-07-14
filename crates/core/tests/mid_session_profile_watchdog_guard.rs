//! Ratchet tests: mid-session profile watchdog wiring (queue item I7).
//!
//! Production evidence (2026-04-21): the app booted during market hours,
//! all 5 WS connections reported "connected", but Dhan was not streaming
//! data. The pre-market profile HALT (PR #309) catches this at boot. I7
//! adds the same check as a 15-minute background sweep during market
//! hours — catching a mid-session `dataPlan` / `activeSegment` / token
//! invalidation.
//!
//! These tests scan the source so the wiring cannot regress silently.

#![allow(clippy::assertions_on_constants)]

use std::fs;
use std::path::PathBuf;

fn repo_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    p
}

fn read(rel: &str) -> String {
    let path = repo_path(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// (1) Module exists and exposes the canonical entry point
// ---------------------------------------------------------------------------

#[test]
fn mid_session_watchdog_module_exists() {
    let src = read("crates/core/src/auth/mid_session_watchdog.rs");
    assert!(
        src.contains("pub fn spawn_mid_session_profile_watchdog"),
        "mid_session_watchdog module must expose spawn_mid_session_profile_watchdog — \
         the only wiring contract that main.rs depends on."
    );
    assert!(
        src.contains("MID_SESSION_CHECK_INTERVAL_SECS"),
        "module must expose a tunable interval constant."
    );
    assert!(
        src.contains("is_within_market_hours_ist()"),
        "loop must be market-hours gated (Rule 3)."
    );
    assert!(
        src.contains("currently_failing"),
        "watchdog must be edge-triggered (Rule 4) — track currently_failing \
         state to suppress duplicate CRITICAL alerts on sustained failure."
    );
}

// ---------------------------------------------------------------------------
// (2) main.rs spawns the watchdog after auth is initialized
// ---------------------------------------------------------------------------

#[test]
fn main_rs_spawns_mid_session_watchdog() {
    let src = read("crates/app/src/main.rs");
    assert!(
        src.contains("spawn_mid_session_profile_watchdog"),
        "main.rs must spawn the mid-session profile watchdog — without it, \
         a mid-session dataPlan/segment/token invalidation goes undetected \
         until the next boot, exactly the 2026-04-21 failure mode."
    );
    assert!(
        src.contains("mid-session profile watchdog spawned"),
        "main.rs must log a confirmation line so deployment logs show the \
         watchdog is running."
    );
}

// ---------------------------------------------------------------------------
// (3) SILENT since 2026-07-14 (operator Dhan noise lock,
//     dhan-rest-only-noise-lock-2026-07-14.md): the watchdog's
//     MidSessionProfileInvalidated Critical + TokenForcedRemintTriggered
//     High Telegram pages are DELETED — the probe + the AUTH-GAP-05
//     forced re-mint self-heal silently, and ONLY a TERMINAL re-mint
//     failure pages via the family-(3) AuthenticationFailed Critical.
//     The negative ratchet below blocks the pages from creeping back.
// ---------------------------------------------------------------------------

#[test]
fn mid_session_watchdog_is_silent_except_terminal_auth_failure() {
    let src = read("crates/core/src/auth/mid_session_watchdog.rs");
    for banned in ["MidSessionProfileInvalidated", "TokenForcedRemintTriggered"] {
        // Grep the SOURCE for constructor sites (`NotificationEvent::X`)
        // — the doc-comment mentions of the deleted variants are fine.
        assert!(
            !src.contains(&format!("NotificationEvent::{banned}")),
            "mid_session_watchdog.rs must NOT re-introduce the deleted \
             {banned} Telegram page (dhan-rest-only-noise-lock-2026-07-14.md \
             — a fresh dated operator quote is required first)."
        );
    }
    // The ONE allowed Telegram: the family-(3) token-unobtainable Critical
    // on a terminal forced-re-mint failure.
    assert!(
        src.contains("NotificationEvent::AuthenticationFailed"),
        "the terminal forced-re-mint failure must page the family-(3) \
         AuthenticationFailed Critical — silent terminal failure is a \
         Rule-11 false-OK."
    );
    // GAP-04: the silent latch re-arm must stay wired.
    assert!(
        src.contains("should_rearm_remint_latch"),
        "the GAP-04 latch re-arm (~30-min silent retry cadence) must stay \
         wired — deleting it stalls the self-heal after one attempt."
    );
}

// ---------------------------------------------------------------------------
// (4) Interval bounds are sane
// ---------------------------------------------------------------------------

#[test]
fn mid_session_interval_bounds_are_sane() {
    use tickvault_core::auth::mid_session_watchdog::MID_SESSION_CHECK_INTERVAL_SECS;
    assert!(
        MID_SESSION_CHECK_INTERVAL_SECS >= 300,
        "interval must be >= 5 min to avoid pounding Dhan's /v2/profile \
         (shared with the 1 req/sec Quote rate limit). Current: {MID_SESSION_CHECK_INTERVAL_SECS}s."
    );
    assert!(
        MID_SESSION_CHECK_INTERVAL_SECS <= 1800,
        "interval must be <= 30 min so a mid-session invalidation is \
         caught within one cycle. Current: {MID_SESSION_CHECK_INTERVAL_SECS}s."
    );
}
