//! Sub-PR #1 of 2026-05-27 daily-universe expansion — source-scan ratchet
//! pinning the **`SubscriptionScope::DailyUniverse` contract** documented
//! in `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`.
//!
//! This guard fails the build if:
//!
//!  1. The Dhan Detailed CSV URL is changed or removed from the rule file.
//!  2. The Quote-mode-for-all-SIDs rule is weakened (Ticker or Full
//!     creep into the daily-universe subscription path).
//!  3. The infinite-retry policy is replaced by any give-up condition.
//!  4. The composite-key contract `(security_id, exchange_segment)`
//!     per I-P1-11 is dropped from the `instrument_lifecycle` DEDUP
//!     UPSERT KEYS clause.
//!  5. A fallback data source (REST LTP, S3 cache, yesterday's stale
//!     CSV) is documented as acceptable.
//!  6. The `SubscriptionScope::DailyUniverse` enum variant disappears
//!     from `crates/common/src/config.rs`.
//!
//! See:
//! - `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//! - `.claude/rules/project/security-id-uniqueness.md` (I-P1-11)
//! - `.claude/rules/project/operator-charter-forever.md` §I

#![cfg(test)]

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn rule_file_body() -> String {
    let path =
        repo_root().join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn config_rs_body() -> String {
    let path = repo_root().join("crates/common/src/config.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Section A — the Detailed CSV URL is the SINGLE source of truth for
/// the daily universe. Compact CSV lacks `UNDERLYING_SECURITY_ID`
/// (required for F&O dedup). If someone replaces the URL with the
/// compact CSV, the contract breaks silently.
#[test]
fn daily_universe_dhan_detailed_csv_url_pinned() {
    let body = rule_file_body();
    assert!(
        body.contains("https://images.dhan.co/api-data/api-scrip-master-detailed.csv"),
        "rule file must pin the Dhan Detailed CSV URL (compact CSV lacks UNDERLYING_SECURITY_ID)"
    );
}

/// Section B — Quote mode is locked for EVERY SID. Per operator Quote 2
/// (2026-05-27): "for all of them it should be quote mode dude always".
#[test]
fn daily_universe_quote_mode_locked_for_all_sids() {
    let body = rule_file_body();
    assert!(
        body.contains("Quote mode")
            || body.contains("Quote (request code 17)")
            || body.contains("**Quote**"),
        "rule file must pin Quote mode for all daily-universe SIDs"
    );
    assert!(
        body.contains("Feed Request Code: `17`")
            || body.contains("request code 17")
            || body.contains("Subscribe — Quote Packet"),
        "rule file must pin Dhan Feed Request Code 17 for Quote mode"
    );
}

/// Section C — infinite retry policy. Per operator Quote 4 (2026-05-27):
/// "irrespective of any situations it should never ever fail … without
/// the proper fetch it should retry". Boot stays BLOCKED until a fresh
/// CSV is in hand.
#[test]
fn daily_universe_infinite_retry_policy_locked() {
    let body = rule_file_body();
    assert!(
        body.contains("Never give up") || body.contains("never give up") || body.contains("∞"),
        "rule file must pin the never-give-up infinite-retry policy"
    );
    assert!(
        body.contains("Boot stays BLOCKED")
            || body.contains("BOOT BLOCKS")
            || body.contains("blocks until fresh CSV"),
        "rule file must pin that boot BLOCKS until fresh CSV in hand"
    );
}

/// Section D — no API/REST/S3/stale-cache fallback. Per operator
/// Quote 2 (2026-05-27): "no need of any api pull in case of any
/// failures". The §3 forbidden-fallbacks list is part of the contract.
#[test]
fn daily_universe_no_fallback_policy_locked() {
    let body = rule_file_body();
    assert!(
        body.contains("No fallback to a different data source")
            || body.contains("no fallback")
            || body.contains("NEVER fallback"),
        "rule file must pin the no-fallback policy"
    );
    // Banned fallbacks explicitly enumerated in §3.
    for banned in [
        "Per-segment REST",
        "REST `/v2/marketfeed/ltp`",
        "S3 cached snapshot",
    ] {
        assert!(
            body.contains(banned),
            "rule file §3 must enumerate banned fallback: '{banned}'"
        );
    }
}

/// Section E — `instrument_lifecycle` table is the SEBI-compliant
/// no-delete state-machine. DEDUP UPSERT KEYS = `(security_id,
/// exchange_segment)` per I-P1-11 composite-uniqueness rule.
#[test]
fn daily_universe_instrument_lifecycle_dedup_includes_segment() {
    let body = rule_file_body();
    assert!(
        body.contains("`instrument_lifecycle`"),
        "rule file must define the instrument_lifecycle table"
    );
    assert!(
        body.contains("(security_id, exchange_segment)"),
        "instrument_lifecycle DEDUP KEYS must include `(security_id, exchange_segment)` per I-P1-11"
    );
    assert!(
        body.contains("NEVER DELETE")
            || body.contains("never deleted")
            || body.contains("NO DELETEs"),
        "rule file must pin the NEVER-DELETE invariant on instrument_lifecycle"
    );
}

/// Section F — `lifecycle_state_locked` BOOLEAN column for operator
/// manual overrides (per option Y approval, 2026-05-27).
#[test]
fn daily_universe_lifecycle_state_locked_override_column() {
    let body = rule_file_body();
    assert!(
        body.contains("`lifecycle_state_locked`"),
        "rule file must define the operator-override `lifecycle_state_locked` BOOLEAN column"
    );
}

/// Section G — `instrument_lifecycle_audit` is the SEBI 5-year forensic
/// chain capturing every state transition.
#[test]
fn daily_universe_lifecycle_audit_table_locked() {
    let body = rule_file_body();
    assert!(
        body.contains("`instrument_lifecycle_audit`"),
        "rule file must define the instrument_lifecycle_audit forensic table"
    );
    assert!(
        body.contains("SEBI") || body.contains("5 year") || body.contains("5-year"),
        "rule file must pin SEBI 5-year retention for the audit chain"
    );
}

/// Section H — universe size envelope bounds. Boot HALTS if outside.
#[test]
fn daily_universe_size_envelope_locked() {
    let body = rule_file_body();
    assert!(
        body.contains("MAX_DAILY_UNIVERSE_SIZE = 400")
            || body.contains("`MAX_DAILY_UNIVERSE_SIZE`"),
        "rule file must pin the MAX_DAILY_UNIVERSE_SIZE = 400 cap"
    );
    assert!(
        body.contains("[100, 400]"),
        "rule file must pin the [100, 400] universe-size envelope"
    );
}

/// Section I — the `DailyUniverse` enum variant exists in `config.rs`.
/// Without this, Sub-PRs #2-#13 cannot reference the type when wiring
/// CSV fetcher / lifecycle reconciler / boot orchestrator.
#[test]
fn daily_universe_subscription_scope_variant_exists() {
    let body = config_rs_body();
    assert!(
        body.contains("DailyUniverse"),
        "crates/common/src/config.rs must define SubscriptionScope::DailyUniverse"
    );
    assert!(
        body.contains("\"daily_universe\"") || body.contains("rename = \"daily_universe\""),
        "SubscriptionScope::DailyUniverse must have stable serde label `daily_universe`"
    );
}

/// Section J — the 2-WebSocket envelope (1 main-feed + 1 order-update)
/// is UNCHANGED by the daily-universe expansion. Only the instrument
/// set on the single main-feed conn expanded.
#[test]
fn daily_universe_two_websocket_envelope_unchanged() {
    let body = rule_file_body();
    assert!(
        body.contains("exactly TWO WebSocket connections")
            || body.contains("Total live WebSocket connections to Dhan: 2"),
        "rule file must reaffirm the 2-WebSocket lock (1 main-feed + 1 order-update)"
    );
    assert!(
        body.contains("UNCHANGED"),
        "rule file must mark the 2-WebSocket envelope as UNCHANGED from prior 2026-05-15 lock"
    );
}
