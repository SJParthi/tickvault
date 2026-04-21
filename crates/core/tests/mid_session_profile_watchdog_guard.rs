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
// (3) Event variant declared with Critical severity + clear message
// ---------------------------------------------------------------------------

#[test]
fn mid_session_profile_event_declared_critical() {
    let src = read("crates/core/src/notification/events.rs");
    assert!(
        src.contains("MidSessionProfileInvalidated"),
        "events.rs must declare MidSessionProfileInvalidated variant."
    );
    assert!(
        src.contains("Self::MidSessionProfileInvalidated { .. } => Severity::Critical"),
        "MidSessionProfileInvalidated must be Critical severity — operator \
         MUST be paged immediately. Downgrading risks silent mid-session \
         data loss."
    );
}

#[test]
fn mid_session_profile_event_message_carries_diagnostics() {
    let src = read("crates/core/src/notification/events.rs");
    // Telegram text must tell the operator EXACTLY what to do next.
    assert!(
        src.contains("CRITICAL: Mid-session profile INVALIDATED"),
        "event message header must clearly identify this as a mid-session \
         failure (distinct from boot-time HALT)."
    );
    assert!(
        src.contains("dataPlan == \\\"Active\\\""),
        "message must tell the operator to verify dataPlan."
    );
    assert!(
        src.contains("Derivative"),
        "message must call out the Derivative segment check."
    );
    assert!(
        src.contains("Live WS still running"),
        "message must clarify that the app did NOT HALT mid-session — \
         operator must understand that the fix is manual."
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
