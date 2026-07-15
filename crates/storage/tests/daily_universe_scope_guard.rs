//! §36 FUTIDX + storage-primitive ratchets — the SURVIVING half of the
//! daily-universe scope guard.
//!
//! PR-C3 (2026-07-14, operator retirement directive 2026-07-13 —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B): the
//! daily-universe SUBSCRIPTION contract this guard originally pinned
//! (Sections A–J: the Dhan Detailed CSV URL, Quote-mode-for-all, the §4
//! infinite retry, the no-fallback policy, the [100, 1200] size envelope,
//! the `SubscriptionScope::DailyUniverse` variant, plus the F&O-master /
//! batched-reconcile / O(1)-warm-skip code pins) is RETIRED — the fetch
//! chain, the reconcile chain, `DailyUniverse`, and the `SubscriptionScope`
//! enum are DELETED (Q3: "hereafter no Dhan instrument download/parsing").
//! The rule file's sections survive as dated historical audit; pinning a
//! retired contract's text would freeze history, so those tests retired
//! WITH their subjects.
//!
//! What KEEPS pinning (still-live contracts):
//!
//! - the §36/§36.7 FUTIDX scope (4 underlyings, all monthly serials,
//!   never-roll, the un-feature-gateable shared selector) — the GROWW
//!   futures leg stands after the Dhan retirement;
//! - the storage SEBI-table primitives (`instrument_lifecycle` bulk ILP
//!   append fns + the O(1) warm-bump/fetch-audit-SHA primitives) — the
//!   tables are never-delete and the Groww shared-master writer is their
//!   live producer.

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

fn src(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// The daily-universe rule file — its §36/§36.7 FUTIDX sections remain the
/// LIVE authority for the Groww futures leg (the rest of the file is dated
/// historical audit per its 2026-07-13 retirement banner).
fn rule_file_body() -> String {
    let path =
        repo_root().join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

#[test]
fn lifecycle_persistence_uses_ilp_not_exec_url() {
    let body = src("crates/storage/src/instrument_lifecycle_persistence.rs");
    assert!(
        body.contains("pub async fn append_instrument_lifecycle_rows")
            && body.contains("pub async fn append_instrument_lifecycle_audit_rows"),
        "storage must expose the bulk lifecycle/audit append fns"
    );
    // The bulk writers MUST ingest via QuestDB ILP (port 9009) — the real
    // ingestion pipe, like ticks/candles — NOT the /exec SQL/URL door. The
    // /exec URL query string is capped by QuestDB's request buffer and
    // overflowed at the ~79K-row F&O master (the "79190 write error(s)" /
    // "connection closed" boot halts, 2026-05-29). ILP has no URL limit.
    assert!(
        body.contains("build_ilp_conf_string")
            && body.contains("Sender::from_conf")
            && body.contains("build_lifecycle_ilp_row")
            && body.contains("build_lifecycle_audit_ilp_row"),
        "bulk lifecycle/audit writes must go through ILP (Sender/Buffer), not /exec URL"
    );
    // Regression: the old size-bounded /exec SQL path MUST be gone — its
    // presence means someone reverted ILP back to the URL door.
    assert!(
        !body.contains("build_size_bounded_inserts")
            && !body.contains("LIFECYCLE_INSERT_MAX_SQL_BYTES"),
        "the /exec URL batch path must stay deleted — bulk writes are ILP-only"
    );
}

// ---------------------------------------------------------------------------
// O(1) WARM-BOOT storage primitives (2026-05-29). PR-C3 (2026-07-14): the
// app-side warm-skip orchestrator test (`reconcile_has_o1_warm_skip_on_
// unchanged_sha`) retired with the deleted `lifecycle_reconcile_orchestrator`;
// the storage primitives it consumed stay pinned below (retained SEBI-table
// surface — the C4 sweep owns any caller-less-fn cleanup decision).
// ---------------------------------------------------------------------------
#[test]
fn storage_exposes_o1_warm_primitives() {
    let lc = src("crates/storage/src/instrument_lifecycle_persistence.rs");
    assert!(
        lc.contains("pub fn build_bump_active_last_seen_sql")
            && lc.contains("pub async fn bump_active_last_seen"),
        "storage must expose the single-statement last_seen bump"
    );
    // The bump is ONE UPDATE scoped to active rows — never a per-row INSERT.
    assert!(
        lc.contains("WHERE lifecycle_state = '{active}'"),
        "the warm bump must be a single active-scoped UPDATE"
    );
    let fa = src("crates/storage/src/instrument_fetch_audit_persistence.rs");
    assert!(
        fa.contains("pub async fn read_last_fetch_audit_sha")
            && fa.contains("ORDER BY ts DESC LIMIT 1"),
        "storage must read the last fetch-audit SHA in one bounded SELECT"
    );
}

// RETIRED 2026-07-14 (PR-C2 — Dhan live-WS lane deletion): the
// `boot_timing_proof_is_measured_and_surfaced` test pinned the boot-timing
// surfacing chain (`tv_instrument_load_*` gauges + the demoted Telegram
// formatter) that lived in main.rs' deleted Dhan boot arms. With the lane
// gone, main.rs no longer runs the daily-universe boot and no longer emits
// those gauges — the pins can never hold again. The dormant
// `daily_universe_boot.rs` (still timing the load via `elapsed_ms`) and the
// WHOLE universe fetch chain — this guard file with it — are deleted in
// Phase C3 per `websocket-connection-scope-lock.md` "2026-07-13 Amendment"
// §B item 3. The other tests in this file keep pinning the still-live §36
// FUTIDX + storage-primitive contracts until C3.

// ---------------------------------------------------------------------------
// Section FUTIDX (§36 2026-07-08; §36.7 2026-07-10) — the index-futures grant
// is pinned to exactly 4 underlyings, all monthly expiries `>= today`
// (§36.7), never-roll, Quote mode. Any widening (5th underlying, OPTIDX,
// non-monthly-serial instrument, expiry < today, breaking the
// per-underlying serial envelope, intraday resubscribe) requires a fresh
// dated operator quote + a rule-file edit FIRST.
// ---------------------------------------------------------------------------

fn index_futures_rs_body() -> String {
    let path = repo_root().join("crates/core/src/instrument/index_futures.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// PRODUCTION region only — split at the first `#[cfg(test)]` (the house
/// AGGREGATOR-SEAL-01 2026-07-06 precedent: a whole-file scan is satisfiable
/// by test-fixture literals, making the membership pin vacuous — hostile-
/// review round 3, 2026-07-08). Fails loudly if the marker ever disappears
/// (a marker-less file would silently regress to the whole-file scan).
fn index_futures_rs_production_region() -> String {
    let body = index_futures_rs_body();
    let Some((prod, _tests)) = body.split_once("#[cfg(test)]") else {
        panic!("index_futures.rs must carry a #[cfg(test)] marker — production-region split");
    };
    prod.to_string()
}

#[test]
fn futidx_scope_pinned_to_4_underlyings_all_monthly_expiries() {
    let body = rule_file_body();
    for phrase in [
        "FUTIDX",
        "NIFTY",
        "BANKNIFTY",
        "MIDCPNIFTY",
        "SENSEX",
        "nearest expiry",
        "NEVER roll",
        "Quote mode",
        // §36.7 (2026-07-10): the all-months grant + its fail-closed
        // envelope must stay recorded in the rule file.
        "ALL available monthly",
        "MonthlySerialFlood",
        "2026-07-10",
    ] {
        assert!(
            body.contains(phrase),
            "rule file §36/§36.7 must pin the FUTIDX contract phrase: {phrase:?}"
        );
    }
    // Hostile-review round 3 (2026-07-08): membership pins scan the
    // PRODUCTION region ONLY — the whole-file scan was satisfiable by
    // test-fixture literals (four_rows()/proptest lists carry the same
    // quoted canonicals PLUS "FINNIFTY"), so a membership swap in the
    // production const kept this ratchet green.
    let src = index_futures_rs_production_region();
    assert!(
        src.contains("[IndexFutureUnderlying; 4]"),
        "INDEX_FUTURES_UNDERLYINGS must be an arity-4 array — a 5th underlying needs a rule edit"
    );
    // §36.7: the per-underlying serial envelope + the derived total bound.
    assert!(
        src.contains("MAX_MONTHLY_EXPIRIES_PER_UNDERLYING: usize = 6"),
        "MAX_MONTHLY_EXPIRIES_PER_UNDERLYING must stay 6 — widening needs a rule edit"
    );
    assert!(
        src.contains("INDEX_FUTURES_UNDERLYINGS.len() * MAX_MONTHLY_EXPIRIES_PER_UNDERLYING"),
        "MAX_INDEX_FUTURE_TARGETS must stay derived (4 underlyings × serial envelope)"
    );
    for canonical in ["\"NIFTY\"", "\"BANKNIFTY\"", "\"MIDCPNIFTY\"", "\"SENSEX\""] {
        assert!(
            src.contains(canonical),
            "index_futures.rs PRODUCTION region must carry the canonical literal {canonical}"
        );
    }
    // Non-vacuity self-test: the production region must EXCLUDE the
    // test-only literals (FINNIFTY/BANKEX live in fixtures + the §36.2
    // REJECT doc only) — proves the split actually removed the test region
    // and the membership pin bites on the production const.
    assert!(
        !src.contains("\"FINNIFTY\""),
        "production region must not carry \"FINNIFTY\" — the split is vacuous if it does"
    );
}

#[test]
fn futidx_scope_rule_file_pins_forbidden_remainder() {
    let body = rule_file_body();
    for phrase in [
        "OPTIDX",
        "FUTSTK",
        "OPTSTK",
        "master-only",
        "NO intraday resubscribe",
        "beyond the §36 grant",
    ] {
        assert!(
            body.contains(phrase),
            "rule file must keep the §36 forbidden-remainder phrase: {phrase:?}"
        );
    }
}

#[test]
fn futidx_scope_never_roll_source_pin() {
    // §36.7: the PLURAL fn owns the `>= today` filter (the never-roll rule
    // lives in exactly ONE place) AND has NO TradingCalendar parameter
    // (accidental stock-style T-0 roll activation must stay
    // unrepresentable); the SINGULAR still exists and DELEGATES to the
    // plural, so the rule cannot fork.
    let src = index_futures_rs_body();
    assert!(
        src.contains("pub fn select_index_future_expiries("),
        "the shared plural boundary fn must exist"
    );
    let fn_start = src
        .find("pub fn select_index_future_expiries(")
        .expect("fn present");
    let fn_body = &src[fn_start..fn_start + 500];
    assert!(
        fn_body.contains(">= today_ist"),
        "ALL monthly expiries >= today (the never-roll rule, §36.7)"
    );
    assert!(
        !fn_body.contains("TradingCalendar"),
        "select_index_future_expiries must NOT take a calendar — the calendar arm exists only \
         to trigger the banned stock T-0 roll"
    );
    // Delegation pin: the singular exists and calls the plural.
    assert!(
        src.contains("pub fn select_index_future_expiry("),
        "the singular nearest-month fn must still exist (SLO exclusion set + docs)"
    );
    let singular_start = src
        .find("pub fn select_index_future_expiry(")
        .expect("singular fn present");
    let singular_body = &src[singular_start..singular_start + 500];
    assert!(
        singular_body.contains("select_index_future_expiries("),
        "the singular must DELEGATE to the plural — the >= today rule has ONE implementation"
    );
}

#[test]
fn futidx_scope_legacy_gate_still_false() {
    // PR-C3 (2026-07-14): the legacy planner gate DIED with
    // `subscription_planner.rs` (scope-lock amendment §B item 2) — a
    // stronger state than "returns false": the fn no longer exists at all.
    // This pin flips direction: the planner file (and with it any
    // `should_subscribe_*` gate) must STAY deleted. Re-introducing a Dhan
    // subscription planner requires a fresh dated operator quote in
    // websocket-connection-scope-lock.md FIRST (§D of the amendment).
    let path = repo_root().join("crates/core/src/instrument/subscription_planner.rs");
    assert!(
        !path.exists(),
        "subscription_planner.rs must stay DELETED (PR-C3, 2026-07-14 — \
         operator retirement directive 2026-07-13); found it recreated at {}",
        path.display()
    );
}
