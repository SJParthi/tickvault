//! B3 (2026-07-03) + round-2 (adversarial review, same day): source-scan
//! ratchets for the health-state token block + self-test `recent_tick`
//! wirings in `crates/app/src/main.rs` (mirrors `health_counter_fix7_guard.rs`).
//!
//! 1. Token headroom (round-2 HIGH + MEDIUM-1): the shared
//!    `SystemHealthStatus` token block (`token_remaining_secs` +
//!    `token_valid`) is written by the DEDICATED, UNCONDITIONAL
//!    `spawn_token_health_writer` lane task — NOT by the SLO loop (which is
//!    gated on the unrelated `realtime_guarantee_score` feature flag: round 1
//!    put the stores there, so flipping that flag off would have re-ghosted
//!    the 09:14 IST READY Telegram, the 15:31:30 IST EOD digest and
//!    `GET /health`, and pinned `overall_status()` "degraded"). The writer's
//!    JoinHandle is lane-registered, and `teardown_dhan_lane_tasks` resets
//!    the block to 0/false so a runtime Dhan disable cannot leave an orphan
//!    writer reporting `token_valid=true` off the dead boot manager.
//!
//! 2. Market-open self-test `recent_tick`: fed by the FEED-level
//!    freshest-REAL-tick age (`TickGapDetector::freshest_tick_age_secs`),
//!    never the worst per-SID gap (`scan_gaps_top_n(_, 1)`), with the
//!    `u64::MAX` fail-safe for "no real tick ever observed".
//!
//! Round-2 LOW-1: every assertion is SCOPED to its block (anchored bounded
//! slice) so unrelated occurrences elsewhere in the ~10K-line main.rs cannot
//! satisfy a positive ratchet or trip a negative one.
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

/// Bounded block of `src` from `start_anchor` (inclusive) to the FIRST
/// occurrence of `end_anchor` after it (exclusive). Panics loudly if either
/// anchor moved, so a refactor updates the guard instead of hollowing it.
fn block_between<'a>(src: &'a str, start_anchor: &str, end_anchor: &str) -> &'a str {
    let start = src
        .find(start_anchor)
        .unwrap_or_else(|| panic!("anchor `{start_anchor}` not found in app/src/main.rs"));
    let rest = &src[start..];
    let end = rest[start_anchor.len()..]
        .find(end_anchor)
        .map(|i| i + start_anchor.len())
        .unwrap_or_else(|| panic!("end anchor `{end_anchor}` not found after `{start_anchor}`"));
    &rest[..end]
}

/// True iff `block` contains ANY `scan_gaps_top_n(...)` call whose final
/// argument is the literal `1` (the worst-per-SID single-entry form),
/// regardless of the `now` variable name or formatting (whitespace/newlines
/// stripped before the suffix check) — round-2 LOW-1 hardening of the old
/// exact-string negative ratchet.
fn contains_worst_gap_top1_scan(block: &str) -> bool {
    let needle = "scan_gaps_top_n(";
    let mut rest = block;
    while let Some(idx) = rest.find(needle) {
        let after = &rest[idx + needle.len()..];
        let Some(close) = after.find(')') else {
            return false;
        };
        let args: String = after[..close]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if args.ends_with(",1") || args.ends_with(",1,") {
            return true;
        }
        rest = &after[close..];
    }
    false
}

/// Round-2 HIGH: the token block writer is a dedicated fn whose body carries
/// BOTH setter calls, it is spawned in `start_dhan_lane` (lane-registered),
/// and the SLO loop's Token_freshness block no longer writes the setters (so
/// the wiring is NOT behind the `realtime_guarantee_score` feature gate).
#[test]
fn test_token_health_writer_is_dedicated_and_unconditional() {
    let src = read_app_main_src();

    // (a) The dedicated writer fn exists and its body does both stores.
    let fn_body = block_between(&src, "fn spawn_token_health_writer(", "\n}");
    assert!(
        fn_body.contains("health.set_token_remaining_secs(secs)"),
        "round-2 regression: spawn_token_health_writer no longer writes \
         `set_token_remaining_secs` — the READY Telegram / EOD digest / \
         GET /health token headroom re-ghosts to a permanent 0.0h"
    );
    assert!(
        fn_body.contains("health.set_token_valid(token_health_writer_valid(secs, profile_ok))"),
        "round-2/F15 regression: spawn_token_health_writer no longer writes \
         `set_token_valid` via the pure profile-truth-aware derivation — \
         either /health reads 'invalid'/'degraded' forever, or (the F15 \
         split-brain) a Dhan-KILLED but locally-unexpired token reads \
         valid on /health while tv_token_valid honestly reads 0.0"
    );
    // F15 (2026-07-08): the pure derivation ANDs local headroom with the
    // mid-session watchdog's profile-truth flag (mirror of tv_token_valid).
    let valid_fn = block_between(&src, "fn token_health_writer_valid(", "\n}");
    assert!(
        valid_fn.contains("secs > 0 && profile_valid"),
        "F15 regression: token_health_writer_valid must AND local headroom \
         (`secs > 0`, strictly-greater fail-closed) with the profile-truth \
         flag — dropping either leg re-opens a token-validity false-OK"
    );
    assert!(
        fn_body.contains("gauge_token_headroom_secs(&feed_runtime)"),
        "round-2 regression: the writer must source the headroom from \
         `gauge_token_headroom_secs` (live lane manager preferred over the \
         global boot manager)"
    );

    // (b) It is spawned at the lane spawn site (registered into
    // DhanLaneRunHandles right below), not a detached ad-hoc spawn.
    assert!(
        src.contains("let token_health_handle = spawn_token_health_writer("),
        "round-2 regression: `spawn_token_health_writer` is no longer spawned \
         (and handle-registered) in start_dhan_lane — the token block has \
         zero production writers again"
    );
    assert!(
        src.contains("token_health_handle: Some(token_health_handle)"),
        "round-2 regression: the writer's JoinHandle is no longer registered \
         in DhanLaneRunHandles — a runtime Dhan disable orphans the writer \
         (MEDIUM-1: stale token_valid=true for up to 24h while Dhan is OFF)"
    );

    // (c) The SLO loop's Token_freshness block must NOT write the setters —
    // that placement was gated on `config.features.realtime_guarantee_score`
    // (the round-1 HIGH finding). Scoped to the Token_freshness→Spill_health
    // slice so unrelated setter call sites can't trip this.
    let slo_token_block = block_between(&src, "// ---- Token_freshness", "// ---- Spill_health");
    assert!(
        !slo_token_block.contains("set_token_remaining_secs")
            && !slo_token_block.contains("set_token_valid"),
        "round-2 regression: the SLO loop's Token_freshness block writes the \
         health-state token block again — that loop is feature-gated on \
         `realtime_guarantee_score`, so flipping that unrelated flag off \
         silently re-ghosts READY/EOD//health (the round-1 HIGH finding)"
    );
}

/// Round-2 MEDIUM-1: the lane teardown aborts the writer AND resets the token
/// block to the honest deliberate-lane-off state (0 / false — the same as the
/// pre-B3 perpetual state, so no NEW alarm class).
#[test]
fn test_lane_teardown_resets_token_health_block() {
    let src = read_app_main_src();
    let teardown = block_between(&src, "async fn teardown_dhan_lane_tasks", "\n// ===");
    assert!(
        teardown.contains("lane.token_health_handle.take()"),
        "round-2 regression: teardown_dhan_lane_tasks no longer aborts the \
         token-health writer — a runtime Dhan disable leaves an orphan \
         writer reporting the dead boot manager's token_valid=true"
    );
    assert!(
        teardown.contains("set_token_remaining_secs(0)")
            && teardown.contains("set_token_valid(false)"),
        "round-2 regression: teardown_dhan_lane_tasks no longer resets the \
         token block to 0/false — a deliberate Dhan-off shows a stale \
         'valid' token block instead of the honest pre-wiring state"
    );
}

/// Ratchets that the market-open self-test's `recent_tick` input is the
/// FEED-level freshest-REAL-tick age (seeds excluded), NOT the worst per-SID
/// gap that failed on illiquid SIDs. All assertions scoped to the self-test
/// spawn block (round-2 LOW-1).
#[test]
fn test_self_test_recent_tick_uses_feed_level_freshest_age() {
    let src = read_app_main_src();
    let self_test_block = block_between(
        &src,
        "if market_open_one_shots_first_spawn && config.features.market_open_self_test {",
        "config.features.realtime_guarantee_score",
    );

    assert!(
        self_test_block
            .contains(".and_then(|d| d.freshest_tick_age_secs(std::time::Instant::now()))")
            && self_test_block.contains("last_tick_age_secs: feed_freshest_age_secs"),
        "B3 regression: the market-open self-test no longer feeds \
         `last_tick_age_secs` from `TickGapDetector::freshest_tick_age_secs` \
         (feed-level real-tick liveness). Reverting to a worst-per-SID gap \
         makes SELFTEST-02 page Critical at 09:16:30 IST on a single \
         always-silent illiquid SID — the exact false alarm B3 closed."
    );

    // Negative ratchet (round-2 LOW-1 hardened): NO `scan_gaps_top_n(.., 1)`
    // form — with ANY variable name / formatting — may reappear inside the
    // self-test block. (The 60s coalescer digest and the SLO
    // fractional-coverage scan live OUTSIDE this block and use different
    // arguments — intentionally untouched.)
    assert!(
        !contains_worst_gap_top1_scan(self_test_block),
        "B3 regression: a worst-per-SID single-entry gap scan \
         (`scan_gaps_top_n(<now>, 1)`) reappeared inside the self-test \
         block — this is the exact form that fed recent_tick before B3."
    );

    // Fail-safe ratchet: "no real tick ever observed" must map to a stale
    // sentinel (fails the check), never to 0 (false-pass).
    assert!(
        self_test_block.contains(".unwrap_or(u64::MAX)"),
        "B3 regression: the self-test's freshest-age input lost its \
         fail-safe `unwrap_or(u64::MAX)` — an in-market self-test with \
         zero real-tick evidence must FAIL recent_tick, not pass."
    );
}
