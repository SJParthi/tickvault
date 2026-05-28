//! Boot-time read-back loader for the `instrument_lifecycle` table.
//!
//! The daily reconciler (the §5 I/O loop, a follow-up) must compare
//! TODAY's validated CSV against YESTERDAY's lifecycle rows to decide
//! each instrument's transition via
//! [`tickvault_storage::lifecycle_reconciler::classify_transition`]. This
//! module is the missing read-side primitive: a SELECT against
//! `instrument_lifecycle` that returns the prior per-instrument
//! attributes the classifier needs, keyed by the I-P1-11 composite
//! `(security_id, exchange_segment)`.
//!
//! Mirrors `bar_cache_loader` / `prev_day_cache_loader`: the **pure**
//! parse + SQL-builder are deterministic and unit-tested without a live
//! QuestDB; the async wrapper that issues the HTTP GET is thin glue.
//!
//! Lives in the `app` crate because it bridges `storage` (the table +
//! `LifecycleState::from_wire`) — `storage` itself cannot host an
//! orchestration loader that the boot path drives.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.
//!
//! ## Schema contract
//!
//! `SELECT security_id, exchange_segment, lifecycle_state,
//! lifecycle_state_locked, instrument_type, lot_size, tick_size,
//! symbol_name FROM instrument_lifecycle WHERE dry_run = false` — the
//! QuestDB `/exec` `dataset` rows are:
//!
//! 1. `security_id` LONG → i64
//! 2. `exchange_segment` SYMBOL → String
//! 3. `lifecycle_state` SYMBOL → [`LifecycleState`] via `from_wire`
//! 4. `lifecycle_state_locked` BOOLEAN → bool
//! 5. `instrument_type` SYMBOL → String
//! 6. `lot_size` INT → i32
//! 7. `tick_size` DOUBLE → f64
//! 8. `symbol_name` SYMBOL → String
//!
//! Only `dry_run = false` rows are read — §27 dry-run rows must never
//! seed the next live reconcile.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tickvault_common::config::QuestDbConfig;
use tickvault_storage::instrument_lifecycle_persistence::{
    LifecycleState, QUESTDB_TABLE_INSTRUMENT_LIFECYCLE,
};

/// HTTP request timeout for the boot read-back SELECT.
const LIFECYCLE_QUERY_TIMEOUT_SECS: u64 = 15;

/// Prior per-instrument attributes the reconciler diffs against today's
/// CSV. Keyed in the returned map by `(security_id, exchange_segment)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PrevLifecycleAttrs {
    pub state: LifecycleState,
    pub locked: bool,
    pub instrument_type: String,
    pub lot_size: i32,
    pub tick_size: f64,
    pub symbol_name: String,
}

/// Composite key per I-P1-11 — `security_id` alone is NOT unique.
pub type LifecycleKey = (i64, String);

/// QuestDB `/exec` JSON response (only the `dataset` field is used).
#[derive(Debug, Deserialize)]
struct QuestDbExecResponse {
    #[serde(default)]
    dataset: Value,
}

/// Result of parsing the read-back dataset.
#[derive(Debug, Default, PartialEq)]
pub struct LifecycleParseResult {
    pub rows: HashMap<LifecycleKey, PrevLifecycleAttrs>,
    /// Rows whose `lifecycle_state` SYMBOL did not map to a known
    /// [`LifecycleState`] (schema drift) — counted, not guessed.
    pub skipped_unknown_state: u32,
    /// Rows with the wrong shape / non-coercible cell types.
    pub skipped_malformed: u32,
}

/// Build the read-back SELECT. Pure. Reads ALL rows (active AND expired)
/// so the reconciler can detect reactivation; excludes §27 dry-run rows.
#[must_use]
pub fn build_lifecycle_select_sql() -> String {
    format!(
        "SELECT security_id, exchange_segment, lifecycle_state, \
         lifecycle_state_locked, instrument_type, lot_size, tick_size, symbol_name \
         FROM {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE} WHERE dry_run = false"
    )
}

/// Parse the QuestDB `dataset` array into the prior-attrs map. PURE —
/// no I/O. On a duplicate composite key the LAST row wins (QuestDB DEDUP
/// already guarantees uniqueness, so this is defensive only).
#[must_use]
pub fn parse_questdb_lifecycle_dataset(dataset: &Value) -> LifecycleParseResult {
    let mut out = LifecycleParseResult::default();
    let Some(rows) = dataset.as_array() else {
        return out;
    };
    for row in rows {
        let Some(cells) = row.as_array() else {
            out.skipped_malformed = out.skipped_malformed.saturating_add(1);
            continue;
        };
        if cells.len() < 8 {
            out.skipped_malformed = out.skipped_malformed.saturating_add(1);
            continue;
        }
        let (
            Some(security_id),
            Some(exchange_segment),
            Some(state_str),
            Some(locked),
            Some(instrument_type),
            Some(lot_size),
            Some(tick_size),
            Some(symbol_name),
        ) = (
            cells[0].as_i64(),
            cells[1].as_str(),
            cells[2].as_str(),
            cells[3].as_bool(),
            cells[4].as_str(),
            cells[5].as_i64(),
            cells[6].as_f64(),
            cells[7].as_str(),
        )
        else {
            out.skipped_malformed = out.skipped_malformed.saturating_add(1);
            continue;
        };
        let Some(state) = LifecycleState::from_wire(state_str) else {
            out.skipped_unknown_state = out.skipped_unknown_state.saturating_add(1);
            continue;
        };
        let lot_size = i32::try_from(lot_size).unwrap_or(0);
        out.rows.insert(
            (security_id, exchange_segment.to_string()),
            PrevLifecycleAttrs {
                state,
                locked,
                instrument_type: instrument_type.to_string(),
                lot_size,
                tick_size,
                symbol_name: symbol_name.to_string(),
            },
        );
    }
    out
}

/// Boot-time entry point: read yesterday's `instrument_lifecycle` rows.
///
/// Best-effort — returns `Err` on QuestDB unavailability so the caller
/// logs + continues (a Day-1 / empty-table read yields an empty map,
/// which the reconciler treats as "every CSV row is `Appeared`").
///
/// Cold path — called once at boot. TEST-EXEMPT for the live HTTP leg;
/// the pure parse + SQL builder are unit-tested below.
// TEST-EXEMPT: requires running QuestDB; the pure parser is tested separately.
pub async fn load_prev_lifecycle_at_boot(
    questdb_config: &QuestDbConfig,
) -> Result<LifecycleParseResult> {
    let url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(LIFECYCLE_QUERY_TIMEOUT_SECS))
        .build()
        .context("build reqwest client for lifecycle read-back SELECT")?;
    let sql = build_lifecycle_select_sql();
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
        .context("HTTP GET lifecycle read-back SELECT against QuestDB")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        anyhow::bail!("QuestDB lifecycle read-back SELECT returned HTTP {status}: {body}");
    }
    let parsed: QuestDbExecResponse = resp
        .json()
        .await
        .context("parse QuestDB JSON dataset for lifecycle read-back")?;
    Ok(parse_questdb_lifecycle_dataset(&parsed.dataset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_unreachable() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    #[test]
    fn test_build_lifecycle_select_sql_shape() {
        let sql = build_lifecycle_select_sql();
        assert!(sql.contains("FROM instrument_lifecycle"));
        assert!(
            sql.contains("WHERE dry_run = false"),
            "must exclude §27 dry-run rows from the live read-back"
        );
        // I-P1-11 composite key both selected.
        assert!(sql.contains("security_id"));
        assert!(sql.contains("exchange_segment"));
        // Fields the reconciler diffs.
        for col in [
            "lifecycle_state",
            "lifecycle_state_locked",
            "instrument_type",
            "lot_size",
            "tick_size",
            "symbol_name",
        ] {
            assert!(sql.contains(col), "SELECT must include `{col}`");
        }
    }

    #[test]
    fn test_parse_well_formed_active_row() {
        let dataset =
            serde_json::json!([[13, "IDX_I", "active", false, "INDEX", 0, 0.05, "NIFTY"]]);
        let r = parse_questdb_lifecycle_dataset(&dataset);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.skipped_malformed, 0);
        assert_eq!(r.skipped_unknown_state, 0);
        let attrs = r.rows.get(&(13, "IDX_I".to_string())).expect("row present");
        assert_eq!(attrs.state, LifecycleState::Active);
        assert!(!attrs.locked);
        assert_eq!(attrs.instrument_type, "INDEX");
        assert_eq!(attrs.lot_size, 0);
        assert!((attrs.tick_size - 0.05).abs() < f64::EPSILON);
        assert_eq!(attrs.symbol_name, "NIFTY");
    }

    #[test]
    fn test_parse_expired_and_locked_rows() {
        let dataset = serde_json::json!([
            [
                99887,
                "NSE_FNO",
                "expired_from_fno",
                true,
                "EQUITY",
                250,
                0.05,
                "TCS"
            ],
            [
                51,
                "IDX_I",
                "expired_index",
                false,
                "INDEX",
                0,
                0.05,
                "SENSEX"
            ]
        ]);
        let r = parse_questdb_lifecycle_dataset(&dataset);
        assert_eq!(r.rows.len(), 2);
        let tcs = r.rows.get(&(99887, "NSE_FNO".to_string())).unwrap();
        assert_eq!(tcs.state, LifecycleState::ExpiredFromFno);
        assert!(tcs.locked);
        assert_eq!(tcs.lot_size, 250);
    }

    #[test]
    fn test_parse_unknown_state_is_counted_not_guessed() {
        let dataset = serde_json::json!([[
            1,
            "NSE_EQ",
            "some_future_state",
            false,
            "EQUITY",
            1,
            0.05,
            "X"
        ]]);
        let r = parse_questdb_lifecycle_dataset(&dataset);
        assert_eq!(r.rows.len(), 0);
        assert_eq!(r.skipped_unknown_state, 1);
        assert_eq!(r.skipped_malformed, 0);
    }

    #[test]
    fn test_parse_malformed_rows_counted() {
        let dataset = serde_json::json!([
            [13, "IDX_I", "active", false, "INDEX", 0, 0.05], // too few cells (7)
            "not_an_array",
            [13, "IDX_I", "active", false, "INDEX", 0, 0.05, "NIFTY"] // good
        ]);
        let r = parse_questdb_lifecycle_dataset(&dataset);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.skipped_malformed, 2);
    }

    #[test]
    fn test_parse_empty_dataset_is_empty_map() {
        let r = parse_questdb_lifecycle_dataset(&serde_json::json!([]));
        assert!(r.rows.is_empty());
        assert_eq!(r.skipped_malformed, 0);
        assert_eq!(r.skipped_unknown_state, 0);
    }

    #[test]
    fn test_parse_non_array_dataset_is_empty() {
        let r = parse_questdb_lifecycle_dataset(&serde_json::json!({"oops": 1}));
        assert!(r.rows.is_empty());
    }

    #[test]
    fn test_parse_bad_cell_type_is_malformed() {
        // security_id is a string, not a number.
        let dataset = serde_json::json!([[
            "not_a_number",
            "IDX_I",
            "active",
            false,
            "INDEX",
            0,
            0.05,
            "NIFTY"
        ]]);
        let r = parse_questdb_lifecycle_dataset(&dataset);
        assert_eq!(r.rows.len(), 0);
        assert_eq!(r.skipped_malformed, 1);
    }

    #[tokio::test]
    async fn test_load_prev_lifecycle_at_boot_errs_when_questdb_unreachable() {
        let cfg = cfg_unreachable();
        let result = load_prev_lifecycle_at_boot(&cfg).await;
        assert!(result.is_err(), "must propagate transport error");
    }
}
