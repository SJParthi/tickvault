//! B3 (2026-07-03): source-scan ratchets for two health-state wirings in
//! `crates/app/src/main.rs` (mirrors `health_counter_fix7_guard.rs`).
//!
//! 1. Token headroom: before B3, `SystemHealthStatus::token_remaining_secs`
//!    and `token_valid` had ZERO production writers — the 09:14 IST READY
//!    Telegram, the 15:31:30 IST EOD digest and `GET /health` rendered a
//!    ghost "Token headroom: 0.0h" / "invalid" forever. The 10s SLO loop
//!    (which already computes the real headroom via
//!    `gauge_token_headroom_secs`) is now the one production writer.
//!
//! 2. Market-open self-test `recent_tick`: before B3 the input was the
//!    WORST per-SID gap (`scan_gaps_top_n(now, 1)`, no exclusions), so
//!    ~33 always-silent illiquid SIDs paged SELFTEST-02 Critical at
//!    09:16:30 IST on pure illiquidity. The input is now the FEED-level
//!    freshest-tick age (`TickGapDetector::freshest_tick_age_secs`).
//!
//! Full behavioural coverage needs a live boot which is out of scope for a
//! unit test crate — these guards pin the wiring so it cannot silently
//! regress (audit-findings Rule 13: wired-then-unwired is a bug class).

use std::path::PathBuf;

fn read_app_main_src() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/api -> crates
    path.push("app/src/main.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Ratchets that the 10s SLO loop writes the real token headroom (and the
/// derived validity flag) into the shared `SystemHealthStatus`, so the
/// READY Telegram, the EOD digest and `GET /health` read a LIVE value.
#[test]
fn test_slo_loop_writes_token_health_state() {
    let src = read_app_main_src();

    assert!(
        src.contains("slo_health.set_token_remaining_secs(token_secs)"),
        "B3 regression: the SLO loop no longer writes \
         `set_token_remaining_secs` into the shared health state. Without \
         this ONE production writer the field is permanently 0 and the \
         09:14 IST READY Telegram / 15:31:30 IST EOD digest / GET /health \
         all show the ghost 'Token headroom: 0.0h' again."
    );

    assert!(
        src.contains("slo_health.set_token_valid(token_secs > 0)"),
        "B3 regression: the SLO loop no longer writes `set_token_valid`. \
         Without it the /health token block and `overall_status()` read \
         'invalid'/'degraded' forever regardless of the real JWT state."
    );
}

/// Ratchets that the market-open self-test's `recent_tick` input is the
/// FEED-level freshest-tick age (min age across the universe), NOT the
/// worst per-SID gap that failed on illiquid SIDs.
#[test]
fn test_self_test_recent_tick_uses_feed_level_freshest_age() {
    let src = read_app_main_src();

    assert!(
        src.contains(".and_then(|d| d.freshest_tick_age_secs(std::time::Instant::now()))")
            && src.contains("last_tick_age_secs: feed_freshest_age_secs"),
        "B3 regression: the market-open self-test no longer feeds \
         `last_tick_age_secs` from `TickGapDetector::freshest_tick_age_secs` \
         (feed-level liveness). Reverting to a worst-per-SID gap makes \
         SELFTEST-02 page Critical at 09:16:30 IST on a single always-silent \
         illiquid SID — the exact false alarm B3 closed."
    );

    // Negative ratchet: the old worst-gap form must not reappear at the
    // self-test site. (Other `scan_gaps_top_n` call sites — the 60s
    // coalescer digest and the SLO fractional-coverage scan — use
    // different arguments and are intentionally untouched.)
    assert!(
        !src.contains("scan_gaps_top_n(std::time::Instant::now(), 1)"),
        "B3 regression: the worst-per-SID single-entry gap scan \
         (`scan_gaps_top_n(now, 1)`) reappeared — this is the exact form \
         that fed the self-test's recent_tick check before B3."
    );

    // Fail-safe ratchet: "no tick ever observed" must map to a stale
    // sentinel (fails the check), never to 0 (false-pass).
    assert!(
        src.contains(".unwrap_or(u64::MAX)"),
        "B3 regression: the self-test's freshest-age input lost its \
         fail-safe `unwrap_or(u64::MAX)` — an in-market self-test with \
         zero tick evidence must FAIL recent_tick, not pass."
    );
}
