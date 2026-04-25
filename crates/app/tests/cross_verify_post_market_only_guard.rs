//! 2026-04-24 — cross-verify post-market-only ratchet.
//!
//! Locks in the Parthiban directive 2026-04-24:
//!
//!   "cross verification should be done only one time, until or unless
//!    it is fully successful it should run, that too only on working
//!    trading days post-market alone only".
//!
//! Translates to four invariants that this test source-scans
//! `crates/app/src/main.rs` and `crates/core/src/historical/cross_verify.rs`
//! to enforce:
//!
//! 1. **`main.rs` calls `from_now_post_market_only`** — not `from_now`.
//!    The original `from_now` returned Some during mid-session boots
//!    (09:15+ IST) which produced partial-coverage cross-match failures;
//!    the new constructor blocks until 15:30 IST.
//! 2. **`CROSS_MATCH_MIN_COVERAGE_PCT == 100`** — was 90 (PR #341),
//!    now 100 because post-market-only means the entire session is
//!    settled. Anything less than full coverage is a real failure,
//!    not a "session edge slop" artifact.
//! 3. **`from_utc_post_market_only` is defined in `cross_verify.rs`**
//!    — protects the constructor from being silently deleted in a
//!    future refactor.
//! 4. **The test asserting the constant is == 100** — guards against
//!    a future PR bumping the constant back down to 90 without
//!    reviewing the policy.
//!
//! These are source-scan checks, not behavioural tests. Behavioural
//! verification of the gate semantics lives in
//! `crates/core/src/historical/cross_verify.rs::tests::test_post_market_only_*`
//! (6 tests covering pre-market, mid-session, boundary, late-evening,
//! and same-day idempotency).

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/app has parent")
        .parent()
        .expect("crates has parent")
        .to_path_buf()
}

fn read_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

#[test]
fn main_rs_uses_post_market_only_constructor_for_cross_verify_window() {
    let main_src = read_file("crates/app/src/main.rs");
    assert!(
        main_src.contains("from_now_post_market_only()"),
        "crates/app/src/main.rs MUST call \
         `TodayIstWindow::from_now_post_market_only()` for the cross-verify \
         window, not the unrestricted `from_now()`. The Parthiban directive \
         2026-04-24 requires cross-verify to run ONLY post-market on \
         trading days. Mid-session boots (09:15..15:30 IST) MUST silent-skip."
    );

    // Hard guard — the today_window assignment block must NOT call
    // the unrestricted `from_now()`. We walk lines starting at the
    // assignment line and check the next ~10 lines.
    //
    // (Other call sites of `from_now()` exist in main.rs for the
    // today_ist date lookup in the historical-fetch decision tree —
    // those are in a DIFFERENT block ~100 lines earlier. They are
    // not the cross-verify window. See PR #353 / #356 for the
    // design.)
    let lines: Vec<&str> = main_src.lines().collect();
    let assignment_line_idx = lines
        .iter()
        .position(|l| l.contains("let today_window = if is_trading_day {"))
        .expect("today_window assignment line must exist in main.rs");
    let window_end = (assignment_line_idx + 10).min(lines.len());
    let window_block_lines = &lines[assignment_line_idx..window_end];
    let window_block = window_block_lines.join("\n");

    // The block must call `from_now_post_market_only`. The `_post_market_only`
    // suffix is the discriminator from the banned `from_now()` call.
    assert!(
        window_block.contains("from_now_post_market_only"),
        "today_window assignment block must call \
         `from_now_post_market_only()`. Window block:\n{window_block}"
    );

    // Negative ratchet — the block must NOT contain a literal
    // `::from_now()` call (with the parens, no `_post_market_only`
    // suffix). Use `::from_now()` rather than `from_now()` so we
    // don't false-positive on the suffix.
    assert!(
        !window_block.contains("::from_now()"),
        "today_window assignment block must NOT call the unrestricted \
         `TodayIstWindow::from_now()` — must use \
         `::from_now_post_market_only()`. Window block:\n{window_block}"
    );
}

#[test]
fn cross_verify_min_coverage_constant_is_100_not_90() {
    let src = read_file("crates/core/src/historical/cross_verify.rs");
    assert!(
        src.contains("pub const CROSS_MATCH_MIN_COVERAGE_PCT: u8 = 100"),
        "CROSS_MATCH_MIN_COVERAGE_PCT MUST be 100 per Parthiban directive \
         2026-04-24. Anything less is a partial-coverage failure that should \
         fire HIGH, not a Skipped/OK. Found:\n{}",
        src.lines()
            .filter(|l| l.contains("CROSS_MATCH_MIN_COVERAGE_PCT"))
            .take(3)
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Negative ratchet — the old 90 value MUST NOT reappear.
    assert!(
        !src.contains("CROSS_MATCH_MIN_COVERAGE_PCT: u8 = 90"),
        "CROSS_MATCH_MIN_COVERAGE_PCT must NOT regress to 90."
    );
}

#[test]
fn cross_verify_module_exposes_post_market_only_constructor() {
    let src = read_file("crates/core/src/historical/cross_verify.rs");
    assert!(
        src.contains("pub fn from_utc_post_market_only(now_utc:"),
        "TodayIstWindow MUST expose `pub fn from_utc_post_market_only` — \
         the post-market-only constructor used by main.rs. Deleting it would \
         silently regress cross-verify back to mid-session execution."
    );
    assert!(
        src.contains("pub fn from_now_post_market_only()"),
        "TodayIstWindow MUST expose `pub fn from_now_post_market_only` — \
         the system-clock wrapper called from main.rs."
    );
}

#[test]
fn cross_verify_post_market_only_blocks_mid_session_in_unit_test() {
    // Behavioural cross-check: the unit test
    // `test_post_market_only_blocks_mid_session` MUST exist in
    // cross_verify.rs's test module. If a future contributor deletes
    // it, this ratchet fails so the regression is caught BEFORE the
    // mid-session-boot bug returns.
    let src = read_file("crates/core/src/historical/cross_verify.rs");
    for needle in [
        "fn test_post_market_only_blocks_pre_market",
        "fn test_post_market_only_blocks_mid_session",
        "fn test_post_market_only_blocks_at_session_close_boundary",
        "fn test_post_market_only_admits_at_session_close_exact",
        "fn test_post_market_only_admits_late_evening",
        "fn test_post_market_only_idempotent_end_across_runs_same_day",
    ] {
        assert!(
            src.contains(needle),
            "behavioural ratchet missing: {needle} must exist in \
             crates/core/src/historical/cross_verify.rs's test module. \
             It enforces the post-market-only gate's semantics under \
             pre-market / mid-session / boundary / late-evening / \
             same-day-idempotency conditions."
        );
    }
}
