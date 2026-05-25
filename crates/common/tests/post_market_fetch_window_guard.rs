//! Ratchets for PR #796 (operator-locked 2026-05-25): post-market
//! fetch window + no-SKIP cross-verify invariants.
//!
//! Operator verbatim 2026-05-25 17:10 IST:
//! > "this cross verification and historical fetch of 90 days and
//! >  current day historical fetch of current day if 1,5,15,60
//! >  should always happen between this timeline alone only dude
//! >  okay which is between 3.30 pm between 11 pm alone only dude
//! >  okay?"
//!
//! ## Pinned invariants
//!
//! 1. Constant `POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST` = 55_800 (15:30 IST)
//! 2. Constant `POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST` = 82_800 (23:00 IST)
//! 3. Helper module `post_market_fetch_window` exists in crates/core/src/historical/
//! 4. Main.rs invokes the 23:00 IST cutoff gate before calling fetch_historical_candles
//! 5. Cross-verify branches do NOT emit `CandleCrossMatchSkipped` for empty live
//!    candles OR partial coverage — both routes go to `CandleCrossMatchFailed`
//!    with a structured reason string.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read_file(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_window_start_constant_is_55800_seconds() {
    let body = read_file("crates/common/src/constants.rs");
    assert!(
        body.contains("POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST: u32 = 55_800"),
        "POST_MARKET_FETCH_WINDOW_START MUST equal 55_800 (15:30 IST) — operator-locked PR #796"
    );
}

#[test]
fn test_window_end_constant_is_82800_seconds() {
    let body = read_file("crates/common/src/constants.rs");
    assert!(
        body.contains("POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST: u32 = 82_800"),
        "POST_MARKET_FETCH_WINDOW_END MUST equal 82_800 (23:00 IST) — operator-locked PR #796"
    );
}

#[test]
fn test_post_market_fetch_window_module_exists() {
    let p = workspace_root().join("crates/core/src/historical/post_market_fetch_window.rs");
    assert!(
        p.exists(),
        "crates/core/src/historical/post_market_fetch_window.rs MUST exist (PR #796)"
    );
}

#[test]
fn test_post_market_fetch_window_module_registered_in_mod_rs() {
    let body = read_file("crates/core/src/historical/mod.rs");
    assert!(
        body.contains("pub mod post_market_fetch_window;"),
        "historical/mod.rs MUST expose `pub mod post_market_fetch_window;` (PR #796)"
    );
}

#[test]
fn test_main_rs_invokes_2300_ist_cutoff_gate_before_fetch() {
    let body = read_file("crates/app/src/main.rs");
    assert!(
        body.contains("classify_post_market_fetch_window"),
        "main.rs MUST call classify_post_market_fetch_window to enforce the \
         23:00 IST cutoff before invoking fetch_historical_candles"
    );
    assert!(
        body.contains("FetchWindowGateOutcome::AfterWindowEnd"),
        "main.rs MUST match on FetchWindowGateOutcome::AfterWindowEnd and \
         return early when the cutoff is reached"
    );
}

#[test]
fn test_no_skip_emission_for_empty_live_or_partial_coverage() {
    let body = read_file("crates/app/src/main.rs");
    // PR #796 deletes the two CandleCrossMatchSkipped emissions for
    // (a) empty live data and (b) partial coverage. Cross-verify now
    // always produces a definitive PASS/FAIL outcome for those cases.
    //
    // The ONE remaining legitimate CandleCrossMatchSkipped emission is
    // the weekend/holiday gate at the top of the cross-verify block —
    // no live data is captured on non-trading days, so cross-verify
    // is genuinely not applicable. That single emission is preserved.
    let skip_emission_count = body
        .matches("NotificationEvent::CandleCrossMatchSkipped")
        .count();
    assert_eq!(
        skip_emission_count, 1,
        "main.rs MUST emit NotificationEvent::CandleCrossMatchSkipped EXACTLY ONCE \
         (weekend/holiday gate only). PR #796 deleted the two SKIP emissions for \
         empty live data + partial coverage. Found {skip_emission_count} emission(s)."
    );
    assert!(
        body.contains("weekend or holiday"),
        "the surviving CandleCrossMatchSkipped emission MUST be the weekend/holiday \
         gate; its reason string is the canary"
    );
}

#[test]
fn test_no_skip_emission_for_empty_candles_tables() {
    let body = read_file("crates/app/src/main.rs");
    // The exact old reason string from the empty-live-data SKIP must
    // NOT appear in main.rs after PR #796.
    assert!(
        !body.contains("no live data in materialized views"),
        "main.rs MUST NOT contain the old 'no live data in materialized views' \
         SKIP reason — PR #796 replaced it with a FAIL outcome. The wording was \
         also misleading: candles_* are TABLES, not materialized views."
    );
}

#[test]
fn test_pr_796_marker_comment_present_in_main() {
    let body = read_file("crates/app/src/main.rs");
    assert!(
        body.contains("PR #796"),
        "main.rs MUST cite PR #796 marker comment so future Claude sessions \
         don't accidentally re-introduce the SKIP branches"
    );
}
