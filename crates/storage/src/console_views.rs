//! Human-readable analyst console views — `ticks_named` + `candles_named`.
//!
//! Plain (non-materialized) QuestDB views that LEFT JOIN the raw
//! `ticks` / `candles_1m` tables against the `instrument_lifecycle`
//! master so an operator pasting `SELECT * FROM ticks_named WHERE
//! symbol_name = 'NIFTY'` into the QuestDB web console gets a
//! human-readable answer without hand-writing the composite join.
//!
//! **Cold-path analyst tooling ONLY.** A plain view is computed at
//! query time — O(join) at SELECT time, honestly O(N), NEVER claimed
//! O(1). No writer / indicator / strategy / risk code reads these
//! views (the RAM-first hot-path SELECT ban is untouched); the boot
//! cost is a handful of idempotent, timeout-bounded HTTP GETs.
//!
//! ## Join correctness
//!
//! * **Duplicate-safety HONEST ENVELOPE (review round 2):** the
//!   lifecycle master's designated `ts` is a PINNED epoch-0 constant
//!   (`lifecycle_designated_ts_nanos() -> 0`, I-P1-08), so **while its
//!   DEDUP is live** the key `(ts, security_id, exchange_segment,
//!   feed)` collapses to exactly ONE row per `(security_id,
//!   exchange_segment, feed)` and the join cannot multiply rows. That
//!   precondition is CONDITIONAL — two already-documented degraded
//!   windows leave PERSISTENT same-key duplicates that QuestDB's later
//!   `DEDUP ENABLE` does NOT retro-collapse (it applies to future
//!   writes only), and then every joined fact row is N-fold
//!   multiplied in the views:
//!   1. the lifecycle HTTP-CLIENT-01 degrade arm (`lifecycle_ensure`
//!      site, `instrument_lifecycle_persistence.rs`) — the table is
//!      ILP-auto-created WITHOUT dedup keys and every reconcile
//!      UPSERT in that window appends full duplicate row sets;
//!   2. a partial `run_lifecycle_feed_self_heal` boot where the
//!      NULL→'dhan' backfill step fails but `DEDUP ENABLE` succeeds —
//!      the NEXT boot's backfill mints same-key duplicates.
//!   Detector (copy-paste form in the runbook; QuestDB does not
//!   support standard SQL HAVING, so this is the documented
//!   subquery + WHERE equivalent):
//!   `SELECT * FROM (SELECT security_id, exchange_segment, feed,
//!   count() AS n FROM instrument_lifecycle GROUP BY security_id,
//!   exchange_segment, feed) WHERE n > 1;`
//!   Deliberately NO `LATEST ON` collapse in the view: it is un-probed
//!   QuestDB-9.3.5-inside-a-VIEW territory (evidence discipline —
//!   zero-loss charter §4), and silently collapsing would HIDE the
//!   master-table corruption the detector exists to surface (audit
//!   Rule 11: the duplicates are the bug; remediation is the manual
//!   dedup sweep already named by `http-client-error-codes.md` §1,
//!   an operator decision under the lifecycle no-DELETE lock).
//! * Join predicate is the I-P1-11 composite `(security_id, segment)`
//!   PLUS `feed` (feed-in-key everywhere, operator 2026-06-28): a
//!   Dhan row and a Groww row for the same instrument coexist by
//!   design, and Groww bit-62 synthetic index ids exist only under
//!   `feed = 'groww'`.
//! * LEFT JOIN + `dry_run = false` INSIDE the dimension subquery (an
//!   outer WHERE would break LEFT semantics; §27 dry-run isolation).
//!   An instrument absent from the lifecycle master surfaces as a
//!   NULL `symbol_name` row — itself a diagnostic (audit Rule 11: a
//!   vanishing row would be a false-OK), never a dropped row.
//!
//! ## Idempotency + convergence
//!
//! The DDL is `CREATE OR REPLACE VIEW` (probe-Verified on the pinned
//! QuestDB 9.3.5): every boot CONVERGES the deployed definition to the
//! code, so a future definition change self-heals on the next boot with
//! zero manual prod steps (`aws-budget.md` rule 8). `IF NOT EXISTS`
//! would 2xx-no-op on a changed definition — a stale-definition
//! false-OK (audit Rule 11) — and is ratchet-banned.
//!
//! ## Call sites (every feed-enabled boot mode)
//!
//! 1. `crates/app/src/main.rs` FAST crash-recovery arm (Dhan)
//! 2. `crates/app/src/main.rs::start_dhan_lane` slow arm (Dhan,
//!    incl. the D2b runtime cold-start)
//! 3. `crates/app/src/groww_activation.rs::activate_groww_lane` — so a
//!    Groww-only boot (`feeds.dhan_enabled = false`, the scale-test /
//!    groww-only lab mode) creates the views too. Double execution on
//!    dual-feed boots is harmless (convergent DDL).
//!
//! A boot with BOTH feeds disabled ensures no base tables and creates
//! no views — nothing streams in that mode, so there is nothing to name.
//!
//! Runbook: `docs/runbooks/questdb-console-queries.md`.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::QUESTDB_TABLE_TICKS;

/// Wire-format name of the human-readable ticks console view.
pub const VIEW_TICKS_NAMED: &str = "ticks_named";
/// Wire-format name of the human-readable 1m-candles console view.
pub const VIEW_CANDLES_NAMED: &str = "candles_named";
/// Live 1m candle base table (Engine B plain table per #T1a — matches
/// `TfIndex::table_name()` for M1; storage cannot depend on trading,
/// so the name is pinned here).
pub const NAMED_VIEW_CANDLES_BASE: &str = "candles_1m";
/// Mirrors `instrument_lifecycle_persistence::QUESTDB_TABLE_INSTRUMENT_LIFECYCLE`.
/// (Historically hardcoded because that module was feature-gated; the
/// `daily_universe_fetcher` feature was deleted in PR-C3, 2026-07-14.)
/// Equality is pinned by the ratchet test
/// `test_lifecycle_dim_matches_persistence_const`.
const NAMED_VIEW_LIFECYCLE_DIM: &str = "instrument_lifecycle";
/// DDL HTTP timeout (same value as every other boot-DDL ensure site).
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// The shared lifecycle dimension subquery both view DDLs join against.
///
/// `dry_run = false` lives INSIDE the subquery so the outer LEFT JOIN
/// semantics are preserved (§27 dry-run isolation; an outer WHERE on a
/// dimension column would silently drop unmapped fact rows).
fn lifecycle_dim_subquery() -> String {
    format!(
        "(SELECT security_id, exchange_segment, feed, symbol_name, display_name, \
         instrument_type FROM {NAMED_VIEW_LIFECYCLE_DIM} WHERE dry_run = false) il"
    )
}

/// DDL for the `ticks_named` view: every `ticks` column that matters to
/// an analyst (only `payload_hash` plumbing omitted), identity-first
/// column order, LEFT-joined against the lifecycle master.
pub fn ticks_named_view_ddl() -> String {
    let dim = lifecycle_dim_subquery();
    format!(
        "CREATE OR REPLACE VIEW {VIEW_TICKS_NAMED} AS \
         SELECT t.ts, il.symbol_name, il.display_name, il.instrument_type, \
         t.ltp, t.open, t.high, t.low, t.close, t.volume, t.oi, t.avg_price, \
         t.last_trade_qty, t.total_buy_qty, t.total_sell_qty, \
         t.feed, t.segment, t.security_id, t.exchange_timestamp, t.received_at, \
         t.capture_seq \
         FROM {QUESTDB_TABLE_TICKS} t \
         LEFT JOIN {dim} \
         ON t.security_id = il.security_id \
         AND t.segment = il.exchange_segment \
         AND t.feed = il.feed;"
    )
}

/// DDL for the `candles_named` view: all 15 `candles_1m` columns,
/// identity-first column order, LEFT-joined against the lifecycle master.
pub fn candles_named_view_ddl() -> String {
    let dim = lifecycle_dim_subquery();
    format!(
        "CREATE OR REPLACE VIEW {VIEW_CANDLES_NAMED} AS \
         SELECT c.ts, il.symbol_name, il.display_name, il.instrument_type, \
         c.open, c.high, c.low, c.close, c.volume, c.oi, c.tick_count, \
         c.feed, c.segment, c.security_id, \
         c.change_pct, c.close_pct_from_prev_day, c.open_pct, c.open_gap_pct \
         FROM {NAMED_VIEW_CANDLES_BASE} c \
         LEFT JOIN {dim} \
         ON c.security_id = il.security_id \
         AND c.segment = il.exchange_segment \
         AND c.feed = il.feed;"
    )
}

/// Issue one view-DDL statement to QuestDB's `/exec` endpoint.
///
/// Mirrors `shadow_persistence::run_ddl` levels exactly: `/exec` is
/// GET-only (POST 405s per the #T1a regression note); 2xx → `info!`,
/// non-2xx → `warn!` (retries next boot — idempotent), transport
/// `Err` → `error!`.
async fn run_view_ddl(client: &Client, base_url: &str, view: &str, ddl: &str) {
    match client.get(base_url).query(&[("query", ddl)]).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(view, "named console view ready");
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body_prefix: String = body.chars().take(200).collect();
            warn!(view, %status, body = %body_prefix, "named view DDL non-2xx — retries next boot");
        }
        Err(err) => {
            error!(view, ?err, "named view DDL request failed");
        }
    }
}

/// Idempotently create-or-converge the `ticks_named` + `candles_named`
/// analyst console views (`CREATE OR REPLACE VIEW` — every boot converges
/// the deployed definition to the code). Fail-soft: never `Result`, never
/// panic — a failure logs and the next boot re-runs (house `ensure_*` norm).
///
/// Call ONLY after the base-table ensures (`ensure_tick_table_dedup_keys`
/// + `ensure_shadow_candle_tables`, or the Groww delegating wrappers) have
/// completed, so `ticks` + `candles_1m` exist before CREATE VIEW validates
/// its column references. Safe to call from multiple feed lanes — the DDL
/// is convergent, so double execution on dual-feed boots is harmless.
// TEST-EXEMPT: requires a running QuestDB; the DDL strings are ratcheted by the pure-builder unit tests below; exercised by boot integration + `make doctor`.
pub async fn ensure_named_views(questdb_config: &QuestDbConfig) {
    // FEATURE-GATE FIX: the lifecycle dimension table must exist before
    // CREATE VIEW validates the join. Prod app builds enable
    // `daily_universe_fetcher` by default (crates/app/Cargo.toml); without
    // the feature the lifecycle table never exists and the CREATE VIEW
    // warn-fails harmlessly each boot (documented; those builds never
    // write lifecycle rows anyway). The daily-universe cold path's own
    // lifecycle ensure runs too late / conditionally for this call site.
    crate::instrument_lifecycle_persistence::ensure_instrument_lifecycle_table(questdb_config)
        .await;

    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    // C2 pattern (2026-07-03): panic-free client build — Client::new()
    // panics on TLS/resolver/fd init failure (silent tokio-task death).
    // Degrade: skip the view DDL this boot; the next boot re-runs it
    // (idempotent) — read-only projections, no data path affected.
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — named-view DDL skipped: \
                 ticks_named/candles_named not created this boot; analyst console views \
                 unavailable until the next successful boot (idempotent re-run; read-only \
                 projections — no data path affected, no duplicate-row window)"
            );
            metrics::counter!(
                "tv_http_client_build_failed_total",
                "site" => "named_views_ensure"
            )
            .increment(1);
            return;
        }
    };

    // Both views attempted independently — one failing never blocks the other.
    run_view_ddl(
        &client,
        &base_url,
        VIEW_TICKS_NAMED,
        &ticks_named_view_ddl(),
    )
    .await;
    run_view_ddl(
        &client,
        &base_url,
        VIEW_CANDLES_NAMED,
        &candles_named_view_ddl(),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Ratchet tests — pure DDL-string pins (house string-assert pattern).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn both_ddls() -> [(&'static str, String); 2] {
        [
            ("ticks_named", ticks_named_view_ddl()),
            ("candles_named", candles_named_view_ddl()),
        ]
    }

    #[test]
    fn test_ticks_named_view_ddl_is_single_terminated_statement() {
        let ddl = ticks_named_view_ddl();
        assert!(ddl.ends_with(';'), "ticks_named DDL must end with ';'");
        assert_eq!(
            ddl.matches(';').count(),
            1,
            "ticks_named DDL must be exactly ONE statement (no injection surface)"
        );
    }

    #[test]
    fn test_candles_named_view_ddl_is_single_terminated_statement() {
        let ddl = candles_named_view_ddl();
        assert!(ddl.ends_with(';'), "candles_named DDL must end with ';'");
        assert_eq!(
            ddl.matches(';').count(),
            1,
            "candles_named DDL must be exactly ONE statement (no injection surface)"
        );
    }

    #[test]
    fn test_view_name_constants_stable() {
        // Wire-format stability: operators + the runbook reference these
        // names verbatim; renaming is a breaking console-surface change.
        assert_eq!(VIEW_TICKS_NAMED, "ticks_named");
        assert_eq!(VIEW_CANDLES_NAMED, "candles_named");
        assert_eq!(NAMED_VIEW_CANDLES_BASE, "candles_1m");
    }

    #[test]
    fn test_both_ddls_use_create_or_replace_view() {
        // CONVERGENT idempotency (review round 1 fix): bare CREATE VIEW is
        // NOT idempotent on QuestDB ("view already exists" on re-boot), and
        // `CREATE VIEW IF NOT EXISTS` no-ops with 2xx when the definition
        // CHANGED — a future DDL edit would deploy green while every
        // already-provisioned DB silently kept the OLD definition forever
        // (audit Rule 11 false-OK class; the "ready" info! would lie).
        // `CREATE OR REPLACE VIEW` (probe-Verified on the pinned QuestDB
        // 9.3.5: ddl OK + definition actually replaced) converges the
        // deployed definition to the code on EVERY boot — the house
        // every-boot self-heal pattern. Regressing to either weaker form
        // is a stale-definition / boot-error regression class.
        for (name, ddl) in both_ddls() {
            assert!(
                ddl.starts_with("CREATE OR REPLACE VIEW "),
                "{name} DDL must start with CREATE OR REPLACE VIEW: {ddl}"
            );
            assert!(
                !ddl.contains("IF NOT EXISTS"),
                "{name} DDL must not use IF NOT EXISTS (stale-definition false-OK): {ddl}"
            );
        }
    }

    #[test]
    fn test_both_ddls_left_join_on_composite_feed_key() {
        // I-P1-11 composite key + feed-in-key: dropping `feed` from the
        // predicate is the row-multiplication / cross-feed-mislabel
        // regression class; a non-LEFT join would drop unmapped rows
        // (audit Rule 11 false-OK class).
        for (name, ddl) in both_ddls() {
            assert!(ddl.contains("LEFT JOIN"), "{name} DDL must LEFT JOIN");
            assert!(
                ddl.contains("security_id = il.security_id"),
                "{name} DDL join must include security_id"
            );
            assert!(
                ddl.contains("= il.exchange_segment"),
                "{name} DDL join must include exchange_segment"
            );
            assert!(
                ddl.contains("= il.feed"),
                "{name} DDL join must include feed"
            );
            // Every ` JOIN ` occurrence must carry a `LEFT ` prefix.
            let mut search_from = 0;
            while let Some(rel) = ddl[search_from..].find(" JOIN ") {
                let idx = search_from + rel;
                let prefix_start = idx.saturating_sub(4);
                assert_eq!(
                    &ddl[prefix_start..idx],
                    "LEFT",
                    "{name} DDL contains a non-LEFT JOIN at byte {idx}: {ddl}"
                );
                search_from = idx + " JOIN ".len();
            }
        }
    }

    #[test]
    fn test_ddls_filter_dry_run_inside_subquery() {
        // §27 dry-run isolation: the filter must live INSIDE the
        // dimension subquery — an outer WHERE would break LEFT-join
        // semantics (unmapped fact rows would vanish).
        for (name, ddl) in both_ddls() {
            let where_idx = ddl
                .find("WHERE dry_run = false")
                .unwrap_or_else(|| panic!("{name} DDL must filter dry_run = false: {ddl}"));
            let subquery_close_idx = ddl
                .find(") il")
                .unwrap_or_else(|| panic!("{name} DDL must alias the subquery as il: {ddl}"));
            assert!(
                where_idx < subquery_close_idx,
                "{name} DDL dry_run filter must be INSIDE the `( ... ) il` subquery"
            );
        }
    }

    #[test]
    fn test_ddls_never_select_star() {
        // Schema-drift honesty: an explicit column list surfaces a
        // renamed/dropped base column as a loud view error instead of
        // silently changing the view shape. `t.*`-in-view is also
        // un-probed QuestDB territory.
        for (name, ddl) in both_ddls() {
            assert!(!ddl.contains("SELECT *"), "{name} DDL must not SELECT *");
            assert!(
                !ddl.contains(".*"),
                "{name} DDL must not use a .* projection"
            );
        }
    }

    #[test]
    fn test_ddls_select_identity_columns_first_from_correct_bases() {
        // UX pin: identity-first column order (ts, symbol_name,
        // display_name, instrument_type, then numbers, ids/plumbing last)
        // and each view reads its correct base table.
        for (name, ddl) in both_ddls() {
            for col in ["il.symbol_name", "il.display_name", "il.instrument_type"] {
                assert!(ddl.contains(col), "{name} DDL must select {col}: {ddl}");
            }
        }
        let ticks = ticks_named_view_ddl();
        let name_idx = ticks.find("il.symbol_name").unwrap();
        let ltp_idx = ticks.find("t.ltp").unwrap();
        assert!(
            name_idx < ltp_idx,
            "ticks_named must list symbol_name before ltp (identity-first)"
        );
        assert!(
            ticks.contains("FROM ticks t"),
            "ticks_named must read FROM ticks: {ticks}"
        );
        let candles = candles_named_view_ddl();
        let name_idx = candles.find("il.symbol_name").unwrap();
        let open_idx = candles.find("c.open").unwrap();
        assert!(
            name_idx < open_idx,
            "candles_named must list symbol_name before open (identity-first)"
        );
        assert!(
            candles.contains("FROM candles_1m c"),
            "candles_named must read FROM candles_1m: {candles}"
        );
    }

    #[test]
    fn test_duplicate_dim_honest_envelope_ratchet() {
        // Review round 2 (LOW): the original unconditional join-safety
        // claim held ONLY while lifecycle DEDUP was live — duplicate
        // lifecycle rows from the documented HTTP-CLIENT-01 auto-create
        // window / partial feed self-heal N-fold-multiply view output,
        // and DEDUP ENABLE does not retro-collapse them. This ratchet pins (a) the honest-envelope
        // wording + the operator detector query in the runbook, and
        // (b) that neither the runbook, this module, nor the plan record
        // regresses to the unconditional claim.
        //
        // Review round 3 (MEDIUM): QuestDB does not support standard SQL
        // HAVING (questdb.com/docs/concepts/sql-extensions), so the
        // detector is pinned in the documented subquery + WHERE form and
        // the broken HAVING form is banned from the runbook.
        let runbook_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/runbooks/questdb-console-queries.md"
        );
        let runbook = std::fs::read_to_string(runbook_path)
            .unwrap_or_else(|e| panic!("runbook must exist at {runbook_path}: {e}"));
        // Detector query is copy-paste present, in QuestDB's subquery form.
        for fragment in [
            "GROUP BY security_id, exchange_segment, feed",
            ") WHERE n > 1",
            "count() AS n",
            "FROM instrument_lifecycle",
        ] {
            assert!(
                runbook.contains(fragment),
                "runbook must carry the duplicate-dim detector fragment {fragment:?}"
            );
        }
        // The HAVING form errors verbatim on QuestDB — the copy-paste
        // detector must never regress to it (mentioning the keyword in
        // prose to explain the limitation is fine; the executable
        // `HAVING count()` fragment is what is banned).
        assert!(
            !runbook.contains("HAVING count()"),
            "runbook regressed to the QuestDB-unsupported HAVING detector form"
        );
        // Honest envelope named; unconditional claim banned. The banned
        // phrase is built dynamically so this test's own source never
        // matches the scan.
        let banned = format!("{} {}", "never", "multiply");
        let normalize = |s: &str| {
            s.to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let runbook_norm = normalize(&runbook);
        assert!(
            runbook_norm.contains("while dedup is live"),
            "runbook must state the WHILE-DEDUP-is-live honest envelope"
        );
        assert!(
            !runbook_norm.contains(&banned),
            "runbook regressed to the unconditional '{banned}' claim"
        );
        let module_norm = normalize(include_str!("console_views.rs"));
        assert!(
            !module_norm.contains(&banned),
            "console_views.rs regressed to the unconditional '{banned}' claim"
        );
        // Review round 3: the operator-approved plan record (active now,
        // archived after merge per plan-enforcement.md) is a PR-facing
        // surface too — it must carry the conditional envelope and never
        // regress to the unconditional claim.
        let plans_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.claude/plans");
        let mut plan_paths: Vec<std::path::PathBuf> = Vec::new();
        let active = std::path::Path::new(plans_dir).join("active-plan-questdb-named-views.md");
        if active.is_file() {
            plan_paths.push(active);
        }
        if let Ok(entries) = std::fs::read_dir(std::path::Path::new(plans_dir).join("archive")) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .contains("questdb-named-views")
                {
                    plan_paths.push(entry.path());
                }
            }
        }
        assert!(
            !plan_paths.is_empty(),
            "the questdb-named-views plan must exist (active or archived) so its wording stays scanned"
        );
        for path in plan_paths {
            let plan = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("plan {} unreadable: {e}", path.display()));
            let plan_norm = normalize(&plan);
            assert!(
                !plan_norm.contains(&banned),
                "plan {} regressed to the unconditional '{banned}' claim",
                path.display()
            );
            assert!(
                plan_norm.contains("while dedup is live"),
                "plan {} must state the WHILE-DEDUP-is-live honest envelope",
                path.display()
            );
        }
    }

    /// Pins the hardcoded lifecycle-table mirror against the persistence
    /// module's canonical constant (unconditional since PR-C3, 2026-07-14 —
    /// the `daily_universe_fetcher` feature was deleted).
    #[test]
    fn test_lifecycle_dim_matches_persistence_const() {
        assert_eq!(
            NAMED_VIEW_LIFECYCLE_DIM,
            crate::instrument_lifecycle_persistence::QUESTDB_TABLE_INSTRUMENT_LIFECYCLE,
            "NAMED_VIEW_LIFECYCLE_DIM drifted from the canonical lifecycle table name"
        );
    }
}
