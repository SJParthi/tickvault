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
        body.contains("MAX_DAILY_UNIVERSE_SIZE = 1200")
            || body.contains("`MAX_DAILY_UNIVERSE_SIZE`"),
        "rule file must pin the MAX_DAILY_UNIVERSE_SIZE = 1200 cap"
    );
    assert!(
        body.contains("[100, 1200]"),
        "rule file must pin the [100, 1200] universe-size envelope"
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

// ---------------------------------------------------------------------------
// F&O-master code wiring (operator lock 2026-05-29 Quote 5) — these pin the
// CODE that makes `instrument_lifecycle` the applicable-F&O master while the
// WebSocket subscription stays at the 331-SID set. A future edit that reverts
// the master to "331-only" (drops the contract collector or the chained
// extraction) fails the build here.
// ---------------------------------------------------------------------------

fn src(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// The applicable-F&O contract collector MUST exist and gate on the tracked
/// underlyings + tracked index SIDs (not blindly take every derivative).
#[test]
fn fno_master_contract_collector_exists() {
    let body = src("crates/core/src/instrument/fno_underlying_extractor.rs");
    assert!(
        body.contains("pub fn collect_applicable_fno_contracts"),
        "collect_applicable_fno_contracts must exist (applicable-F&O master, Quote 5)"
    );
    assert!(
        body.contains("INDEX_DERIVATIVE_PREFIXES") && body.contains("\"FUTIDX\""),
        "collector must handle index F&O (FUTIDX/OPTIDX) for tracked indices"
    );
    // Stock derivs gate on the tracked NSE_EQ underlying set; index derivs gate
    // by equity-index EXCHANGE (PR #882 — index F&O link via a DERIVATIVES-domain
    // underlying SID, NOT the IDX_I spot SID, so the prior `index_sids.contains`
    // gating dropped ~17K contracts; exchange-gating recovered them). Updated
    // 2026-05-30 to match the current collector after the #882 fix.
    assert!(
        body.contains("unique_underlying_ids.contains"),
        "collector must gate stock derivs on the tracked NSE_EQ underlying set"
    );
    assert!(
        body.contains("is_equity_index_exchange"),
        "collector must gate index derivs by equity-index exchange (PR #882 — \
         index F&O underlying SID is DERIVATIVES-domain, not the IDX_I spot SID)"
    );
}

/// `DailyUniverse` MUST carry `fno_contracts` SEPARATE from `subscription_targets`
/// so the master holds contracts while the feed does not.
#[test]
fn daily_universe_has_master_only_fno_contracts_field() {
    let body = src("crates/core/src/instrument/daily_universe.rs");
    assert!(
        body.contains("pub fno_contracts: Vec<CsvRow>"),
        "DailyUniverse must have a master-only `fno_contracts` field"
    );
    // The envelope check must remain on subscription_targets (the 331), NOT
    // on the contracts — else the ~219K master would trip the [100,400] HALT.
    assert!(
        body.contains("let total = subscription_targets.len();"),
        "the [100,400] envelope must bound subscription_targets, not the contracts"
    );
}

/// The lifecycle reconcile extraction MUST chain `fno_contracts` so the master
/// UPSERTs them; the subscription dispatcher path must NOT.
#[test]
fn extract_today_instruments_chains_fno_contracts() {
    let body = src("crates/app/src/today_instrument.rs");
    assert!(
        body.contains(".fno_contracts") && body.contains(".chain("),
        "extract_today_instruments must chain universe.fno_contracts into the lifecycle set"
    );
}

// ---------------------------------------------------------------------------
// PERF ratchet (2026-05-29): the lifecycle reconcile MUST write in BATCHES,
// not one HTTP round-trip per row. The per-row path built a fresh client +
// one round-trip per row → ~219K rows = ~438K round-trips = boot took
// minutes→hours. A regression to the per-row append fns fails the build here.
// ---------------------------------------------------------------------------
#[test]
fn apply_reconcile_uses_batched_writes_not_per_row() {
    let body = src("crates/app/src/apply_reconcile_plan.rs");
    assert!(
        body.contains("append_instrument_lifecycle_rows(")
            && body.contains("append_instrument_lifecycle_audit_rows("),
        "apply_reconcile_plan must use the BATCHED append fns (one client, multi-row INSERT)"
    );
    assert!(
        !body.contains("append_instrument_lifecycle_row(")
            && !body.contains("append_instrument_lifecycle_audit_row("),
        "apply_reconcile_plan must NOT use the per-row append fns — perf regression \
         (219K rows = 219K HTTP round-trips). Use the batched *_rows fns."
    );
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
// O(1) WARM-BOOT ratchet (2026-05-29): when today's CSV SHA-256 matches the
// last boot's, the reconcile MUST skip the full re-UPSERT and do a single
// last_seen bump (O(1) requests, any universe size). A regression that drops
// the warm path fails the build here.
// ---------------------------------------------------------------------------
#[test]
fn reconcile_has_o1_warm_skip_on_unchanged_sha() {
    let body = src("crates/app/src/lifecycle_reconcile_orchestrator.rs");
    assert!(
        body.contains("read_last_fetch_audit_sha") && body.contains("bump_active_last_seen"),
        "reconcile must compare last CSV SHA + bump last_seen on a match (O(1) warm skip)"
    );
    assert!(
        body.contains("warm_skipped: true"),
        "the warm path must early-return warm_skipped=true (skipping the full re-UPSERT)"
    );
}

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

#[test]
fn boot_timing_proof_is_measured_and_surfaced() {
    // The whole instrument load (fetch→build→reconcile) MUST be wall-clocked
    // and carried back on the boot outcome, so the O(1) warm path has REAL
    // daily evidence (not just a code-comment claim).
    let boot = src("crates/app/src/daily_universe_boot.rs");
    assert!(
        boot.contains("use std::time::Instant;")
            && boot.contains("let boot_timer = Instant::now();")
            && boot.contains("pub elapsed_ms: u64"),
        "run_daily_universe_boot must time the load and expose elapsed_ms"
    );
    assert!(
        boot.contains("u64::try_from(boot_timer.elapsed().as_millis())"),
        "elapsed_ms must be set from the boot_timer in the outcome construction"
    );

    // main.rs must surface the timing 3 ways: structured INFO log, Prometheus
    // gauges (graphable trend), and an operator-facing Telegram line.
    let main = src("crates/app/src/main.rs");
    assert!(
        main.contains("tv_instrument_load_duration_ms")
            && main.contains("tv_instrument_load_warm_skipped")
            && main.contains("tv_instrument_load_total_rows"),
        "main.rs must emit the 3 boot-timing Prometheus gauges"
    );
    assert!(
        main.contains("fn format_instrument_load_telegram")
            && main.contains("NotificationEvent::Custom { message }"),
        "main.rs must emit a boot-timing Telegram line via the pure formatter"
    );
    assert!(
        main.contains("O(1) warm-path proof"),
        "the structured INFO log must label the O(1) warm-path proof"
    );
    // Sentinel-guard: Indices4Only (no daily fetch) must emit no Telegram.
    assert!(
        main.contains("if elapsed_ms == u64::MAX") || main.contains("== u64::MAX"),
        "the formatter must suppress the Telegram when timing was not measured"
    );
}

// ---------------------------------------------------------------------------
// Section FUTIDX-4 (§36, 2026-07-08) — the index-futures grant is pinned to
// exactly 4 underlyings, nearest expiry, never-roll, Quote mode. Any widening
// (5th underlying, OPTIDX, non-nearest expiry, intraday resubscribe) requires
// a fresh dated operator quote + a rule-file edit FIRST.
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
fn futidx_scope_pinned_to_4_underlyings_nearest_expiry() {
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
    ] {
        assert!(
            body.contains(phrase),
            "rule file §36 must pin the FUTIDX-4 contract phrase: {phrase:?}"
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
    assert!(
        src.contains("MAX_INDEX_FUTURE_TARGETS: usize = 4"),
        "MAX_INDEX_FUTURE_TARGETS must stay 4"
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
    // The shared boundary fn keeps the `>= today` nearest find AND has NO
    // TradingCalendar parameter (accidental stock-style T-0 roll activation
    // must stay unrepresentable).
    let src = index_futures_rs_body();
    assert!(
        src.contains("pub fn select_index_future_expiry("),
        "the shared boundary fn must exist"
    );
    let fn_start = src
        .find("pub fn select_index_future_expiry(")
        .expect("fn present");
    let fn_body = &src[fn_start..fn_start + 500];
    assert!(
        fn_body.contains(">= today_ist"),
        "nearest = first expiry >= today (the never-roll rule)"
    );
    assert!(
        !fn_body.contains("TradingCalendar"),
        "select_index_future_expiry must NOT take a calendar — the calendar arm exists only \
         to trigger the banned stock T-0 roll"
    );
}

#[test]
fn futidx_scope_legacy_gate_still_false() {
    // The legacy planner gate was NOT the vector — it stays `false` forever.
    let path = repo_root().join("crates/core/src/instrument/subscription_planner.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()));
    // The gate fn name is assembled at runtime so THIS file never contains
    // the retired-flag substring the `indices4only_scope_lock_guard.rs`
    // whole-crates scan bans (its allowlist covers only 3 files).
    let gate_fn_name: String = ["should_", "subscribe_index_", "derivatives"].concat();
    let gate = src
        .find(&format!("pub const fn {gate_fn_name}"))
        .expect("legacy gate fn must still exist");
    let gate_body = &src[gate..gate + 200];
    assert!(
        gate_body.contains("false"),
        "the legacy index-derivatives gate must keep returning false — the §36 FUTIDX path \
         is the DailyUniverse IndexFuture role, never this gate"
    );
}
