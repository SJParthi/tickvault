//! Ratchet for PR #793 (operator-locked 2026-05-25): pins the spawn
//! site of the pre-market yesterday's-1d fetch scheduler in main.rs.
//!
//! Operator verbatim 2026-05-25 14:30 IST:
//! > "see this should always happen onl yat the time of pre market dude
//! >  okay? Fetch yesterday's 1d for 4 IDX_I SIDs"
//!
//! Authority: `.claude/rules/project/live-feed-purity.md` rule 8.
//!
//! The pre-market scheduler MUST be wired into the boot path. Silently
//! removing the spawn call would leave rule 8 documented but never
//! triggered — this ratchet blocks that regression.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read_main() -> String {
    let p = workspace_root().join("crates/app/src/main.rs");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_spawn_function_defined_in_main() {
    let body = read_main();
    assert!(
        body.contains("fn spawn_yesterdays_1d_pre_market_fetch_scheduler"),
        "main.rs MUST define spawn_yesterdays_1d_pre_market_fetch_scheduler (PR #793)"
    );
}

#[test]
fn test_spawn_function_invoked_from_boot() {
    let body = read_main();
    // Boot wiring — count call sites. We expect TWO (fast boot + slow
    // boot paths), but any non-zero count satisfies the wiring
    // invariant (rule 8 must trigger from at least one boot path).
    let call_count = body
        .matches("spawn_yesterdays_1d_pre_market_fetch_scheduler(")
        .count();
    // 1 definition site + at least 1 call site = 2 occurrences minimum.
    assert!(
        call_count >= 2,
        "main.rs MUST invoke spawn_yesterdays_1d_pre_market_fetch_scheduler from at least one \
         boot path (definition + call). Found {call_count} occurrence(s)."
    );
}

#[test]
fn test_pre_market_scheduler_uses_separate_marker_path() {
    let body = read_main();
    // The marker path MUST be different from the post-market full-fetch
    // marker (default: data/state/historical_fetch_done.json). Sharing
    // markers would let one scheduler block the other.
    assert!(
        body.contains("data/state/yesterdays_1d_pre_market_done.json"),
        "pre-market scheduler MUST use a SEPARATE marker path so it does not \
         collide with the 15:30 IST post-market full fetch marker"
    );
}

#[test]
fn test_pre_market_scheduler_pins_lookback_to_one_day() {
    let body = read_main();
    // The lookback override MUST pin lookback_days = 1 so we fetch only
    // yesterday's bar, NOT the full 90-day window pre-market.
    assert!(
        body.contains("bg_historical_config.lookback_days = 1"),
        "pre-market scheduler MUST pin lookback_days = 1 — wider lookback \
         would violate rule 8's narrow ~20 REST-call scope and risk DH-904 \
         on the live path pre-market"
    );
}

#[test]
fn test_pre_market_scheduler_uses_daily_1d_fire_constant() {
    let body = read_main();
    // The scheduler MUST sleep until next 09:00:30 IST using the canonical
    // constant — not a magic number.
    assert!(
        body.contains("secs_until_next_daily_1d_fire"),
        "pre-market scheduler MUST use the canonical secs_until_next_daily_1d_fire \
         helper from cross_verify_scheduler — hardcoded sleep math would drift \
         from the operator-locked 09:00:30 IST schedule"
    );
}

#[test]
fn test_pre_market_scheduler_gates_on_trading_day() {
    let body = read_main();
    // Audit-findings Rule 3: market-hours / trading-day aware.
    // The scheduler MUST consult the trading calendar after sleeping.
    assert!(
        body.contains("is_trading_day(today_after_sleep)"),
        "pre-market scheduler MUST gate on is_trading_day after sleeping — \
         firing on weekends/holidays would waste Dhan REST calls and may hit \
         DH-904 (audit-findings Rule 3)"
    );
}
