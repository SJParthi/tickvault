//! Ratchet for the operator-locked 2026-05-25 pre-market exception in
//! `live-feed-purity.md` rule 8.
//!
//! Operator directive verbatim 2026-05-25 14:30 IST:
//! > "see this should always happen onl yat the time of pre market dude
//! >  okay? Fetch yesterday's 1d for 4 IDX_I SIDs"
//!
//! This explicit override allows yesterday's-1d-only fetch at 09:00:30 IST
//! pre-market, EXCEPTIONAL to the 2026-04-22 directive that otherwise
//! bans pre-market historical fetching.
//!
//! Future Claude sessions could silently remove the override by editing
//! `live-feed-purity.md`. This ratchet pins the rule body so that
//! removal fails the build.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read_rule() -> String {
    let p = workspace_root().join(".claude/rules/project/live-feed-purity.md");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_live_feed_purity_rule_8_exists() {
    let body = read_rule();
    assert!(
        body.contains("Pre-market yesterday's-1d fetch exception"),
        "live-feed-purity.md MUST contain rule 8 documenting the \
         operator-locked 2026-05-25 pre-market exception"
    );
}

#[test]
fn test_live_feed_purity_rule_8_quotes_operator_verbatim() {
    let body = read_rule();
    // Operator's exact 2026-05-25 14:30 IST quote — partial match on the
    // distinctive phrase so we tolerate minor surrounding edits but block
    // silent removal of the authority quote.
    assert!(
        body.contains("only at the time of pre market")
            || body.contains("onl yat the time of pre market"),
        "rule 8 MUST cite the operator's verbatim 2026-05-25 quote so \
         future sessions cannot remove the override without leaving the \
         documented authority chain intact"
    );
    assert!(
        body.contains("Fetch yesterday's 1d for 4 IDX_I SIDs"),
        "rule 8 MUST cite the operator's verbatim scope ('yesterday's 1d \
         for 4 IDX_I SIDs') — narrower scope would change semantics"
    );
}

#[test]
fn test_live_feed_purity_rule_8_pins_09_00_30_ist_trigger() {
    let body = read_rule();
    assert!(
        body.contains("09:00:30 IST"),
        "rule 8 MUST pin the operator-locked 09:00:30 IST trigger time"
    );
    assert!(
        body.contains("DAILY_1D_FIRE_SECS_OF_DAY_IST"),
        "rule 8 MUST cite the canonical schedule constant"
    );
}

#[test]
fn test_live_feed_purity_rule_8_keeps_post_market_lock_for_bulk() {
    let body = read_rule();
    // The exception is NARROW — bulk historical (1m/5m/15m/60m × 90 days)
    // still belongs post-market. If a future PR removes this guard, the
    // ratchet fails so the operator catches the scope expansion.
    assert!(
        body.contains("Bulk historical fetch") && body.contains("POST-MARKET ONLY"),
        "rule 8 MUST explicitly preserve the post-market lock for bulk \
         historical fetches (1m/5m/15m/60m × 90 days). The exception \
         applies ONLY to yesterday's-1d-only — not to bulk."
    );
}
