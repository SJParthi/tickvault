//! Console-view RETIREMENT sweep — this module creates NO view.
//!
//! ## 2026-09-22 — every view is gone, by operator directive
//!
//! Until 2026-09-22 this module created four QuestDB views: `ticks_named`,
//! `candles_named` and `market_depth_named` (each a LEFT JOIN of a raw table
//! against `instrument_lifecycle`, so a console query could read a name), and
//! `candles_10m` (a `SAMPLE BY 10m` derivation over `candles_1m`).
//!
//! The operator retired all four, in his words (2026-09-22):
//!
//! > "hy the fuck candle 10 m is a view bro it should be the real table right
//! > why the Fuck do we need even these views bro why"
//!
//! > "I clealry told you to put either contract or symbol name right in each
//! > and every table to see it precisely right dude espeically when I want to
//! > query it manually"
//!
//! So the two jobs the views did move INTO the tables:
//!
//! | View | Replaced by |
//! |---|---|
//! | `candles_10m` | a real `candles_10m` TABLE written by the fold (`TfIndex::M10`) |
//! | `ticks_named` / `candles_named` / `market_depth_named` | a `contract` column on the table itself, filled at write time |
//!
//! The authorization and the cost table live in
//! `websocket-connection-scope-lock.md` §"2026-09-22 (SECOND) — NO VIEWS
//! ANYWHERE".
//!
//! ## What this module does now
//!
//! [`drop_retired_views`] issues `DROP VIEW IF EXISTS` for every view this
//! repository ever created, once per boot, BEFORE any table DDL. Two reasons,
//! and the second is why the ORDER is load-bearing:
//!
//! 1. A box that still carries the old views converges to "no views" without a
//!    manual step.
//! 2. **A `candles_10m` VIEW occupies the name the `candles_10m` TABLE needs.**
//!    `CREATE TABLE IF NOT EXISTS candles_10m` against a name held by a view is
//!    refused — the same `400` shape the legacy materialized views caused for
//!    `candles_2m` / `candles_3m` (`shadow_persistence::drop_legacy_candle_objects`).
//!    If the drop ran after the ensure, the table would never exist, the seal
//!    writer's first 10m row would ILP-auto-create it WITHOUT its DEDUP key, and
//!    every replayed bar would duplicate into it for the life of the table.
//!
//! ## Why a name that is now a TABLE is still in the sweep
//!
//! `candles_10m` is both a retired view name AND a live table name. On the
//! first boot after deploy it is a view and the drop removes it. On every boot
//! after that it is a table, and `DROP VIEW IF EXISTS` against a table is not
//! something this repository has been able to probe — QuestDB may accept it as
//! a no-op or refuse it as "not a view" (port 9000 is unreachable from the
//! build container). Either answer is fine, and a refusal on a name in
//! [`VIEW_NAMES_NOW_TABLES`] is counted as `expected_refusal` at debug level,
//! never as a failure — a warn on every boot forever is the shape that trains
//! an operator to ignore a counter.
//!
//! **What that classification can hide, stated so it is not mistaken for
//! safe:** if the drop is refused while `candles_10m` is STILL a view, the
//! refusal also reads `expected_refusal`. That case is not silent — the very
//! next step, `ensure_shadow_candle_tables`, is refused on the same name, is
//! retried, and exhausts into a coded `HOT-PATH-02` error naming the missing
//! DEDUP key. The loud signal is one step later, not absent.
//!
//! ## Never a prefix sweep
//!
//! Every entry is a literal. `top_volume_rank` — a legacy TABLE — is a strict
//! prefix of four entries, and `candles_` is a prefix of every live candle
//! table. A sweep written as "drop everything starting with X" would put real
//! tables in its blast radius; `the_sweep_names_no_live_table_except_the_declared_one`
//! pins it.
//!
//! Runbook: `docs/runbooks/questdb-console-queries.md`.

use std::time::Duration;

use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;

/// Every view this repository ever created and now removes.
///
/// A HISTORICAL list, fixed forever: it names what was once created, never a
/// mirror of any enum. A new view is not "added here" — creating a view is a
/// REJECT under the 2026-09-22 scope-lock section.
pub const RETIRED_CONSOLE_VIEWS: [&str; 8] = [
    // The three joined "named" faces (2026-08 → 2026-09-22).
    "ticks_named",
    "candles_named",
    "market_depth_named",
    // The derived 10-minute frame (2026-09-18 → 2026-09-22). Now a TABLE.
    "candles_10m",
    // The four per-cadence faces under the pre-2026-09-12 base name.
    "top_volume_rank_1s",
    "top_volume_rank_3s",
    "top_volume_rank_5s",
    "top_volume_rank_1m",
];

/// Retired view names that are LIVE TABLE names today. A refusal to drop one
/// of these as a view is the expected answer once the table exists.
pub const VIEW_NAMES_NOW_TABLES: [&str; 1] = ["candles_10m"];

/// DDL HTTP timeout (same value as every other boot-DDL ensure site).
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Counter: retired-view DROP statements at boot, by `outcome`.
///
/// Not loss-shaped and not EMF-selected: a view is a read projection, so its
/// presence or absence loses no data. The number answers "did the sweep run",
/// locally on `/metrics`.
pub const VIEW_DDL_COUNTER: &str = "tv_console_view_ddl_total";

/// Every outcome [`VIEW_DDL_COUNTER`] carries, for pre-registration.
pub const VIEW_DDL_OUTCOMES: [&str; 4] = ["ok", "non_2xx", "transport", "expected_refusal"];

/// The one DROP statement for a retired view name.
///
/// A function rather than an inline `format!`, so the tests read the exact
/// string the sweep sends — a test that builds its own copy is testing itself.
#[must_use]
pub fn retired_view_drop_ddl(view: &str) -> String {
    format!("DROP VIEW IF EXISTS {view};")
}

/// True when a non-2xx for `view` is the expected answer (the name is a table).
#[must_use]
pub fn is_expected_view_drop_refusal(view: &str) -> bool {
    VIEW_NAMES_NOW_TABLES.contains(&view)
}

/// Seeds every outcome series at zero so the first refusal is a delta the
/// scrape can see rather than a dropped first sample.
pub fn pre_register_view_ddl_counter() {
    for outcome in VIEW_DDL_OUTCOMES {
        metrics::counter!(VIEW_DDL_COUNTER, "outcome" => outcome).increment(0);
    }
}

/// Issue one DROP to QuestDB's `/exec` endpoint (GET-only — POST 405s).
async fn run_view_drop(client: &Client, base_url: &str, view: &str) {
    let ddl = retired_view_drop_ddl(view);
    match client
        .get(base_url)
        .query(&[("query", ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            metrics::counter!(VIEW_DDL_COUNTER, "outcome" => "ok").increment(1);
            debug!(view, "retired console view: DROP VIEW IF EXISTS accepted");
        }
        Ok(resp) if is_expected_view_drop_refusal(view) => {
            metrics::counter!(VIEW_DDL_COUNTER, "outcome" => "expected_refusal").increment(1);
            debug!(
                view,
                status = %resp.status(),
                "retired view name is a live TABLE now — DROP VIEW refused as expected"
            );
        }
        Ok(resp) => {
            metrics::counter!(VIEW_DDL_COUNTER, "outcome" => "non_2xx").increment(1);
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body_prefix: String = body.chars().take(200).collect();
            warn!(view, %status, body = %body_prefix, "retired view DROP non-2xx — retries next boot");
        }
        Err(err) => {
            metrics::counter!(VIEW_DDL_COUNTER, "outcome" => "transport").increment(1);
            error!(
                view,
                ?err,
                "retired view DROP request failed — retries next boot"
            );
        }
    }
}

/// Drop every retired console view. Fail-soft: never `Result`, never panic —
/// a failure logs and the next boot re-runs (every statement is idempotent).
///
/// Call BEFORE `shadow_persistence::ensure_shadow_candle_tables`: a surviving
/// `candles_10m` VIEW would refuse the `candles_10m` TABLE.
// TEST-EXEMPT: requires a running QuestDB; the statement builder and the refusal classifier are unit-tested below and the boot ordering is pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs.
pub async fn drop_retired_views(questdb_config: &QuestDbConfig) {
    pre_register_view_ddl_counter();
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    // Panic-free client build — Client::new() panics on TLS/resolver/fd init
    // failure (silent tokio-task death). Degrade: skip the sweep this boot.
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — retired-view sweep skipped \
                 this boot. If an old `candles_10m` VIEW survives, the `candles_10m` TABLE \
                 CREATE is refused on the next step and reports its own coded error; the \
                 next boot re-runs this sweep."
            );
            metrics::counter!(
                "tv_http_client_build_failed_total",
                "site" => "retired_views_drop"
            )
            .increment(1);
            return;
        }
    };
    for view in RETIRED_CONSOLE_VIEWS {
        run_view_drop(&client, &base_url, view).await;
    }
    info!(
        views = RETIRED_CONSOLE_VIEWS.len(),
        "retired console-view sweep attempted — this build creates no view"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::top_volume_rank_persistence::SnapshotCadence;

    #[test]
    fn retired_view_drop_ddl_is_idempotent_one_statement_and_never_a_table() {
        for view in RETIRED_CONSOLE_VIEWS {
            let ddl = retired_view_drop_ddl(view);
            assert_eq!(ddl, format!("DROP VIEW IF EXISTS {view};"));
            assert_eq!(ddl.matches(';').count(), 1, "{view}: one statement");
            assert!(
                !ddl.contains("TABLE"),
                "{view}: drops a VIEW, never a table"
            );
        }
    }

    #[test]
    fn is_expected_view_drop_refusal_only_for_names_that_became_tables() {
        assert!(is_expected_view_drop_refusal("candles_10m"));
        for view in RETIRED_CONSOLE_VIEWS {
            if view != "candles_10m" {
                assert!(!is_expected_view_drop_refusal(view), "{view}");
            }
        }
        assert!(!is_expected_view_drop_refusal("candles_1m"));
        // Every "now a table" name is also in the sweep — otherwise the old
        // view would never be dropped and the table could never be created.
        for t in VIEW_NAMES_NOW_TABLES {
            assert!(RETIRED_CONSOLE_VIEWS.contains(&t), "{t}");
        }
    }

    /// The sweep can never name a live TABLE, except the one declared name
    /// whose view must be removed so the table can exist.
    #[test]
    fn the_sweep_names_no_live_table_except_the_declared_one() {
        let mut live: Vec<&str> = crate::shadow_persistence::candle_table_names().to_vec();
        for c in SnapshotCadence::ALL {
            live.push(c.table_name());
        }
        live.push(crate::top_volume_rank_persistence::LEGACY_TOP_VOLUME_RANK_TABLE);
        live.push(crate::top_volume_rank_persistence::LEGACY_TOP_VOLUME_TABLE);
        live.extend([
            "ticks",
            "market_depth",
            "instrument_lifecycle",
            "instrument_lifecycle_audit",
            "order_audit",
        ]);
        for view in RETIRED_CONSOLE_VIEWS {
            if is_expected_view_drop_refusal(view) {
                assert!(
                    live.contains(&view),
                    "{view} is declared a table but no table carries that name"
                );
                continue;
            }
            assert!(!live.contains(&view), "{view} is a live table name");
        }
    }

    #[test]
    fn the_retired_list_is_history_fixed_and_duplicate_free() {
        assert_eq!(RETIRED_CONSOLE_VIEWS.len(), 8);
        let mut sorted = RETIRED_CONSOLE_VIEWS;
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate entry in the sweep");
        }
    }

    #[test]
    fn pre_register_view_ddl_counter_covers_every_outcome_and_does_not_panic() {
        pre_register_view_ddl_counter();
        pre_register_view_ddl_counter();
        assert_eq!(VIEW_DDL_OUTCOMES.len(), 4);
        assert!(VIEW_DDL_OUTCOMES.contains(&"expected_refusal"));
        assert!(VIEW_DDL_COUNTER.ends_with("_total"));
    }

    /// The module creates nothing. A `CREATE` literal anywhere in the
    /// production half is a view (or table) creation, which the 2026-09-22
    /// scope-lock section bans in this module.
    #[test]
    fn drop_retired_views_module_contains_no_create_statement() {
        let src = include_str!("console_views.rs");
        let prod = src
            .split(concat!("#[cfg(test)]\n", "mod tests"))
            .next()
            .expect("prod half");
        assert!(!prod.contains(concat!("\"CRE", "ATE")), "a CREATE literal");
        assert!(
            !prod.contains(concat!("CREATE OR ", "REPLACE")),
            "a view DDL"
        );
    }

    #[tokio::test]
    async fn drop_retired_views_degrades_against_an_unreachable_port() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Must return (transport arm per view), never panic.
        drop_retired_views(&cfg).await;
    }
}
