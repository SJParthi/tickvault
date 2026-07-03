//! `groww_scale_audit` — forensic chain for Groww auto-scale ladder
//! transitions (PR-2 Item 8 of
//! `.claude/plans/active-plan-groww-autoscale.md`; §34 authorization in
//! `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`).
//!
//! One row per ladder transition (advance started / rung verified /
//! rollback / global halve / ceiling halt) answers the SEBI-grade question
//! "what connection count was live at time T, and why did it change?" and
//! feeds restart rehydration (`resume_conns_from_audit` in the app crate):
//! a mid-ladder deploy resumes at the newest VERIFIED-healthy rung, never
//! the unverified probe rung.
//!
//! Best-effort mirror of AUDIT-WS-01: a write failure logs
//! `GROWW-SCALE-04` (Medium) + increments
//! `tv_groww_scale_audit_write_errors_total` at the caller and NEVER gates
//! a ladder decision — the ladder continues on in-memory state. The append
//! is idempotent (`DEDUP UPSERT KEYS`), so a re-run/replay never duplicates.
//! Runbook: `.claude/rules/project/groww-scale-error-codes.md`.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

/// `/exec` HTTP timeout for both DDL and INSERT. Matches the value used by
/// every other `*_audit_persistence` module in this crate.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// Wire-format table name.
pub const QUESTDB_TABLE_GROWW_SCALE_AUDIT: &str = "groww_scale_audit";

/// Composite DEDUP key (phase-0-architecture template rules): the designated
/// timestamp partition column `trading_date_ist` is mandatory; `ts` keeps two
/// rapid same-second transitions distinct; `from_conns`/`to_conns`/`outcome`
/// make each lifecycle-chain row survive its siblings (a rollback row never
/// overwrites the advance row that triggered it). No `security_id` — the
/// ladder is fleet-level, not per-instrument, so I-P1-11 does not apply.
pub const DEDUP_KEY_GROWW_SCALE_AUDIT: &str = "trading_date_ist, ts, from_conns, to_conns, outcome";

/// Ladder-transition outcome labels (SYMBOL column, stable wire format —
/// the rehydration reader in the app crate matches on these strings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleOutcome {
    /// A rung's spawned connections all came up healthy (rung verified).
    VerifiedHealthy,
    /// An advance to a higher rung began (connections spawning, unverified).
    AdvanceStarted,
    /// A rung failed — fleet rolled back to the last healthy count.
    RolledBack,
    /// Fleet-wide simultaneous failure — global cooldown + halve fired.
    GlobalHalved,
    /// The effective ceiling was reached and verified.
    HaltedAtCeiling,
    /// §34 PR-3 (Item 11): the cap-probe run's per-conn-vs-per-account
    /// verdict row — the `reason` column carries the verdict label
    /// (`probe_multi_conn_ok` / `probe_per_account_limited` /
    /// `probe_inconclusive` / `probe_smoke_machinery_validated`).
    ProbeVerdict,
}

impl ScaleOutcome {
    /// Stable wire-format label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedHealthy => "verified_healthy",
            Self::AdvanceStarted => "advance_started",
            Self::RolledBack => "rolled_back",
            Self::GlobalHalved => "global_halved",
            Self::HaltedAtCeiling => "halted_at_ceiling",
            Self::ProbeVerdict => "probe_verdict",
        }
    }

    /// True for the outcomes restart rehydration treats as VERIFIED-healthy
    /// (mirrors `HEALTHY_OUTCOMES` in the app-crate ladder module).
    #[must_use]
    pub const fn is_verified_healthy(self) -> bool {
        matches!(self, Self::VerifiedHealthy | Self::HaltedAtCeiling)
    }
}

/// One ladder-transition audit row.
#[derive(Clone, Debug)]
pub struct GrowwScaleAuditRow<'a> {
    /// Transition instant, IST epoch NANOS (converted to micros at write).
    pub ts_nanos_ist: i64,
    /// IST trading date (midnight), epoch NANOS.
    pub trading_date_ist_nanos: i64,
    /// Connection count before the transition.
    pub from_conns: usize,
    /// Connection count after the transition.
    pub to_conns: usize,
    /// Ladder state label at the transition (`probing` / `holding` /
    /// `advancing` / `rolling_back` / `halted_at_ceiling`) — the app crate's
    /// `LadderState::as_str()`; provably SQL-safe static labels, sanitized
    /// anyway as defense in depth.
    pub state: &'a str,
    /// The transition outcome.
    pub outcome: ScaleOutcome,
    /// Free-text reason (gate failure label, spawn error class). Sanitized.
    pub reason: &'a str,
    /// Compact snapshot of the gate inputs at decision time (JSON-ish,
    /// operator-forensic). Sanitized.
    pub gate_snapshot: &'a str,
}

/// Idempotent DDL: `CREATE TABLE IF NOT EXISTS` with `DEDUP UPSERT KEYS`.
/// Cold path, boot-time only. Degrades (logged) on failure — the ILP/exec
/// auto-create path would lack DEDUP keys until the next successful boot,
/// exactly the documented HTTP-CLIENT-01 / BOOT-01 degrade family.
// TEST-EXEMPT: live-QuestDB side effect; the DDL string + dedup key are pinned by unit tests below and the shared dedup_segment_meta_guard scans the constants.
pub async fn ensure_groww_scale_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — groww_scale_audit DDL skipped \
                 this boot (idempotent re-run next boot)"
            );
            metrics::counter!(
                "tv_http_client_build_failed_total",
                "site" => "groww_scale_audit_ensure"
            )
            .increment(1);
            return;
        }
    };
    let create_ddl = groww_scale_audit_ddl();
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_GROWW_SCALE_AUDIT,
                "audit table ensured"
            );
        }
        Ok(resp) => {
            error!(
                status = %resp.status(),
                table = QUESTDB_TABLE_GROWW_SCALE_AUDIT,
                "groww_scale_audit DDL non-2xx — table may lack DEDUP keys until next boot"
            );
        }
        Err(err) => {
            error!(
                error = %err,
                table = QUESTDB_TABLE_GROWW_SCALE_AUDIT,
                "groww_scale_audit DDL send failed — table may lack DEDUP keys until next boot"
            );
        }
    }
}

/// The full CREATE DDL (extracted for unit pinning).
#[must_use]
pub fn groww_scale_audit_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_GROWW_SCALE_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            from_conns INT, \
            to_conns INT, \
            state SYMBOL, \
            outcome SYMBOL, \
            reason STRING, \
            gate_snapshot STRING\
        ) timestamp(ts) PARTITION BY DAY WAL \
        DEDUP UPSERT KEYS({DEDUP_KEY_GROWW_SCALE_AUDIT});"
    )
}

/// Column list for the INSERT (kept adjacent to the DDL so drift is visible
/// in one screenful).
const INSERT_COLUMN_LIST: &str =
    "ts, trading_date_ist, from_conns, to_conns, state, outcome, reason, gate_snapshot";

/// PURE: one `(...)` VALUES tuple for the row. `outcome` is a compile-time
/// static label (provably SQL-safe); `state`/`reason`/`gate_snapshot` are
/// sanitized (the only injection-reachable inputs). Nanos → micros here.
#[must_use]
pub fn format_scale_row_values_tuple(row: &GrowwScaleAuditRow<'_>) -> String {
    let outcome = row.outcome.as_str();
    let state = sanitize_audit_string(row.state);
    let reason = sanitize_audit_string(row.reason);
    let gate_snapshot = sanitize_audit_string(row.gate_snapshot);
    let ts_micros = row.ts_nanos_ist / 1_000;
    let trading_date_micros = row.trading_date_ist_nanos / 1_000;
    // QuestDB column is INT (i32); the config hard max is 100 conns, so the
    // range is provably safe — debug-assert to surface a future regression.
    debug_assert!(row.from_conns <= i32::MAX as usize);
    debug_assert!(row.to_conns <= i32::MAX as usize);
    let from_conns = row.from_conns;
    let to_conns = row.to_conns;
    format!(
        "({ts_micros}, {trading_date_micros}, {from_conns}, {to_conns}, \
          '{state}', '{outcome}', '{reason}', '{gate_snapshot}')"
    )
}

/// Appends one ladder-transition row. Cold path (a handful of rows per
/// trading day). Idempotent via `DEDUP UPSERT KEYS`.
///
/// # Errors
/// Propagates the `reqwest` transport error, or `Err` on a non-2xx `/exec`
/// response. The CALLER routes the error as GROWW-SCALE-04 (Medium,
/// best-effort — never gates a ladder decision).
// TEST-EXEMPT: live-QuestDB side effect; the pure tuple formatter + DDL are unit-tested below, and the GROWW-SCALE-04 routing is the caller's ratchet.
pub async fn append_groww_scale_audit_row(
    questdb_config: &QuestDbConfig,
    row: &GrowwScaleAuditRow<'_>,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()?;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_GROWW_SCALE_AUDIT} \
         ({INSERT_COLUMN_LIST}) VALUES {tuple};",
        tuple = format_scale_row_values_tuple(row),
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        // Cap the reflected body at 200 chars (PREVCLOSE-04 convention):
        // this string can propagate to a Telegram alert.
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        anyhow::bail!("groww_scale_audit insert non-2xx ({status}): {body}");
    }
    Ok(())
}

/// PURE: parse the QuestDB `/exec` `dataset` for rehydration — each row is
/// `[ts_micros_string, to_conns, outcome]` per [`REHYDRATE_SELECT_SQL`]'s
/// column order. Malformed cells are skipped (fail-soft: rehydration falls
/// back to the first rung when nothing parses — never a panic).
#[must_use]
pub fn parse_scale_audit_dataset(dataset: &serde_json::Value) -> Vec<(i64, usize, String)> {
    let Some(rows) = dataset.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let cells = row.as_array()?;
            // QuestDB /exec returns TIMESTAMP as an ISO string; the reader
            // SELECT casts to microseconds LONG so the cell is numeric.
            let ts_micros = cells.first()?.as_i64()?;
            let to_conns = usize::try_from(cells.get(1)?.as_i64()?).ok()?;
            let outcome = cells.get(2)?.as_str()?.to_string();
            Some((ts_micros.saturating_mul(1_000), to_conns, outcome))
        })
        .collect()
}

/// The rehydration SELECT: today's transitions, numeric micros timestamp
/// first so [`parse_scale_audit_dataset`] stays purely numeric/string.
#[must_use]
pub fn rehydrate_select_sql(trading_date_ist: &str) -> String {
    let date = sanitize_audit_string(trading_date_ist);
    format!(
        "SELECT cast(ts as long) ts_micros, to_conns, outcome \
         FROM {QUESTDB_TABLE_GROWW_SCALE_AUDIT} \
         WHERE trading_date_ist = '{date}' ORDER BY ts_micros"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_GROWW_SCALE_AUDIT, "groww_scale_audit");
    }

    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        // trading_date_ist (partition column) MUST be in every DEDUP key
        // (regression class 2026-04-28) and ts keeps rapid-succession rows
        // distinct; outcome keeps lifecycle-chain rows from overwriting.
        assert!(DEDUP_KEY_GROWW_SCALE_AUDIT.contains("trading_date_ist"));
        assert!(DEDUP_KEY_GROWW_SCALE_AUDIT.contains("ts"));
        assert!(DEDUP_KEY_GROWW_SCALE_AUDIT.contains("outcome"));
        assert!(DEDUP_KEY_GROWW_SCALE_AUDIT.contains("from_conns"));
        assert!(DEDUP_KEY_GROWW_SCALE_AUDIT.contains("to_conns"));
    }

    #[test]
    fn test_outcome_as_str_stable() {
        assert_eq!(ScaleOutcome::VerifiedHealthy.as_str(), "verified_healthy");
        assert_eq!(ScaleOutcome::AdvanceStarted.as_str(), "advance_started");
        assert_eq!(ScaleOutcome::RolledBack.as_str(), "rolled_back");
        assert_eq!(ScaleOutcome::GlobalHalved.as_str(), "global_halved");
        assert_eq!(ScaleOutcome::HaltedAtCeiling.as_str(), "halted_at_ceiling");
        assert_eq!(ScaleOutcome::ProbeVerdict.as_str(), "probe_verdict");
    }

    #[test]
    fn test_verified_healthy_classification() {
        assert!(ScaleOutcome::VerifiedHealthy.is_verified_healthy());
        assert!(ScaleOutcome::HaltedAtCeiling.is_verified_healthy());
        assert!(!ScaleOutcome::AdvanceStarted.is_verified_healthy());
        // A probe verdict is a REPORT, never a resume-here-healthy marker —
        // rehydration must not resume a rung off a verdict row.
        assert!(!ScaleOutcome::ProbeVerdict.is_verified_healthy());
        assert!(!ScaleOutcome::RolledBack.is_verified_healthy());
        assert!(!ScaleOutcome::GlobalHalved.is_verified_healthy());
    }

    #[test]
    fn test_groww_scale_audit_ddl_contains_expected_columns() {
        let ddl = groww_scale_audit_ddl();
        for col in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "from_conns INT",
            "to_conns INT",
            "state SYMBOL",
            "outcome SYMBOL",
            "reason STRING",
            "gate_snapshot STRING",
            "DEDUP UPSERT KEYS",
            "PARTITION BY DAY WAL",
        ] {
            assert!(ddl.contains(col), "DDL missing: {col}\n{ddl}");
        }
    }

    fn sample_row() -> GrowwScaleAuditRow<'static> {
        GrowwScaleAuditRow {
            ts_nanos_ist: 1_751_500_000_000_000_000,
            trading_date_ist_nanos: 1_751_461_800_000_000_000,
            from_conns: 2,
            to_conns: 5,
            state: "advancing",
            outcome: ScaleOutcome::AdvanceStarted,
            reason: "hold_complete",
            gate_snapshot: "{\"conns_ok\":2,\"lag_ms\":120}",
        }
    }

    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let tuple = format_scale_row_values_tuple(&sample_row());
        // nanos / 1000 = micros
        assert!(tuple.contains("1751500000000000, 1751461800000000"));
        assert!(!tuple.contains("1751500000000000000"));
    }

    #[test]
    fn test_format_scale_row_values_tuple_sanitizes_injection() {
        let mut row = sample_row();
        row.reason = "x'); DROP TABLE ticks; --";
        let tuple = format_scale_row_values_tuple(&row);
        assert!(
            !tuple.contains("DROP TABLE ticks;"),
            "reason must be sanitized: {tuple}"
        );
    }

    #[test]
    fn test_parse_scale_audit_dataset_happy_and_malformed() {
        let dataset = serde_json::json!([
            [1751500000000000i64, 2, "verified_healthy"],
            [1751500001000000i64, 5, "advance_started"],
            ["not-a-number", 5, "advance_started"],
            [1751500002000000i64, "bad", "rolled_back"],
        ]);
        let rows = parse_scale_audit_dataset(&dataset);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1751500000000000i64 * 1_000);
        assert_eq!(rows[0].1, 2);
        assert_eq!(rows[0].2, "verified_healthy");
        // Non-array / empty datasets are fail-soft empty.
        assert!(parse_scale_audit_dataset(&serde_json::json!({})).is_empty());
        assert!(parse_scale_audit_dataset(&serde_json::json!([])).is_empty());
    }

    #[test]
    fn test_rehydrate_select_sql_sanitizes_date() {
        let sql = rehydrate_select_sql("2026-07-03' OR '1'='1");
        assert!(
            !sql.contains("' OR '1'='1"),
            "date must be sanitized: {sql}"
        );
        let ok = rehydrate_select_sql("2026-07-03");
        assert!(ok.contains("WHERE trading_date_ist = '2026-07-03'"));
        assert!(ok.contains("ORDER BY ts_micros"));
    }
}
