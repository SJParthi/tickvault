//! B3 (2026-07-03) lineage, re-pointed by PR-C2 (2026-07-13): source-scan
//! ratchets for the health-state token block.
//!
//! PR-C2 deleted the Dhan live-WS lane (operator retirement directive —
//! websocket-connection-scope-lock.md "2026-07-13 Amendment"), and with it
//! the lane-owned `spawn_token_health_writer` + the market-open self-test +
//! the lane teardown reset this guard originally pinned. The token
//! observability writers re-homed into the Dhan REST-only stack
//! (`crates/app/src/dhan_rest_stack.rs`), which owns the process's ONLY
//! TokenManager now:
//!
//! 1. the 15s `tv_token_valid` / `tv_token_remaining_seconds` gauge poller
//!    (`spawn_token_health_gauge_poller` — the AUTH-GAP-05 honest gauges),
//! 2. the 10s /health token-block writer (`set_token_remaining_secs` +
//!    `set_token_valid`, AND-composed with the mid-session watchdog's
//!    profile-truth flag — the F15 derivation).
//!
//! Retired assertions (with the machinery they pinned):
//! - "lane teardown resets the token block" — the stack is process-lifetime;
//!   no teardown path exists, so no reset site is required.
//! - "self-test recent_tick uses the feed-level freshest age" — the
//!   market-open self-test was a lane one-shot, deleted with the lane.
//!
//! Full behavioural coverage needs a live boot which is out of scope for a
//! unit test crate — these guards pin the wiring so it cannot silently
//! regress (audit-findings Rule 13: wired-then-unwired is a bug class).

use std::path::PathBuf;

fn read_dhan_rest_stack_src() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/api -> crates
    path.push("app/src/dhan_rest_stack.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The /health token block MUST keep a production writer: the REST stack's
/// dedicated 10s task writes BOTH setters, off the SAME profile-truth flag
/// the mid-session watchdog maintains.
#[test]
fn test_rest_stack_owns_the_health_token_block_writer() {
    let src = read_dhan_rest_stack_src();
    assert!(
        src.contains("set_token_remaining_secs(secs)")
            && src.contains("set_token_valid(secs > 0 && profile_ok)"),
        "PR-C2 regression: the Dhan REST-only stack no longer writes the \
         /health token block (`set_token_remaining_secs` + the F15 \
         `secs > 0 && profile_ok` AND-composition). Without a production \
         writer, GET /health reports the token invalid forever."
    );
    assert!(
        src.contains("TOKEN_HEALTH_WRITER_INTERVAL_SECS: u64 = 10"),
        "PR-C2 regression: the /health token-block writer cadence drifted \
         from the documented 10s staleness envelope (B3 round-2 lineage)."
    );
}

/// The `tv_token_valid` / `tv_token_remaining_seconds` gauge poller MUST
/// keep a production spawn site (its only pre-C2 sites were the deleted
/// fast arm + lane).
#[test]
fn test_rest_stack_spawns_the_token_health_gauge_poller() {
    let src = read_dhan_rest_stack_src();
    assert!(
        src.contains("spawn_token_health_gauge_poller("),
        "PR-C2 regression: the Dhan REST-only stack no longer spawns \
         `spawn_token_health_gauge_poller` — the tv_token_valid / \
         tv_token_remaining_seconds gauges (AUTH-GAP-05 triage surface) \
         would go dark with no writer."
    );
}

/// The writer must be fed by the profile-truth flag the mid-session
/// watchdog maintains — a Dhan-killed but locally-unexpired token can
/// never read valid (the F15 contract).
#[test]
fn test_rest_stack_wires_profile_truth_into_both_writers() {
    let src = read_dhan_rest_stack_src();
    assert!(
        src.contains("spawn_mid_session_profile_watchdog(") && src.contains("token_profile_valid"),
        "PR-C2 regression: the profile-truth flag (mid-session watchdog) is \
         no longer wired — the token writers would report local-validity \
         only, resurrecting the pre-F15 false-valid class."
    );
}
