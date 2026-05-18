//! Phase 0 Item 20 — orphan position 15:25 IST watchdog audit persistence.
//!
//! Strategy is intraday option-buying — NO overnight positions allowed.
//! At 15:25:00 IST every trading day the watchdog queries `/v2/positions`
//! and writes one audit row per open position (`net_qty != 0`) plus one
//! `NoOrphans` row when the green-path clean-flat condition holds. The
//! audit row gives SEBI auditors + the operator a forensic answer to
//! "did the operator carry any overnight positions on date X?".
//!
//! Phase 0 keeps the watchdog in DRY-RUN mode — it DETECTs + AUDITs +
//! Telegrams but does NOT place exit orders. Phase 1+ flips
//! `strategy.dry_run = false` and the watchdog cancels Super Order legs
//! + places market exits before writing the audit row with outcome
//! `AutoClosed`.
//!
//! DEDUP key `(trading_date_ist, security_id, exchange_segment, ts)`
//! per I-P1-11 composite identity rule plus the `ts` designated-
//! timestamp requirement (2026-04-28 regression). One orphan per
//! `(date, sid, segment)` survives even if the watchdog re-runs.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_ORPHAN_POSITION_AUDIT: &str = "orphan_position_audit";

// QuestDB requires the designated timestamp column in every DEDUP key
// (regression 2026-04-28). I-P1-11 requires `(security_id, exchange_segment)`
// as composite identity. `trading_date_ist` gives daily idempotency.
pub const DEDUP_KEY_ORPHAN_POSITION_AUDIT: &str =
    "trading_date_ist, security_id, exchange_segment, ts";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format outcome strings for the `outcome` SYMBOL column.
/// Kept as a typed enum so callers can't accidentally pass `"DETECTED"`
/// (uppercase) or `"detected_orphan"` and silently fragment the SYMBOL
/// set Grafana queries depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanPositionAuditOutcome {
    /// Green-path daily marker — no open positions found at 15:25 IST.
    /// Written with `security_id = 0` so the panel can `SELECT
    /// last(outcome) WHERE security_id = 0` to confirm the watchdog ran.
    NoOrphans,
    /// Orphan found; in Phase 0 (dry-run) this is the terminal outcome.
    /// In Phase 1+ this row is overwritten by `AutoClosed` /
    /// `ExitFailed` after the exit attempt completes.
    Detected,
    /// Phase 1+ live: orphan was auto-exited successfully via market
    /// order. Replaces the earlier `Detected` row for the same DEDUP key.
    AutoClosed,
    /// Phase 1+ live: orphan auto-exit attempt failed (REST error, rate
    /// limit, etc.). Operator must manually intervene.
    ExitFailed,
}

impl OrphanPositionAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoOrphans => "no_orphans",
            Self::Detected => "detected",
            Self::AutoClosed => "auto_closed",
            Self::ExitFailed => "exit_failed",
        }
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_orphan_position_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_ORPHAN_POSITION_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id LONG, \
            exchange_segment SYMBOL, \
            trading_symbol SYMBOL, \
            product_type SYMBOL, \
            outcome SYMBOL, \
            reason SYMBOL, \
            net_qty LONG, \
            buy_qty LONG, \
            sell_qty LONG, \
            realized_profit DOUBLE, \
            unrealized_profit DOUBLE, \
            dry_run BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_ORPHAN_POSITION_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_ORPHAN_POSITION_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_ORPHAN_POSITION_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_ORPHAN_POSITION_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one orphan-position audit row.
///
/// `ts_nanos_ist` and `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — the function divides them down to microseconds
/// before embedding in the INSERT statement because QuestDB
/// `TIMESTAMP` columns store microseconds since epoch (2026-04-28
/// nanos-to-micros regression).
// APPROVED: 15 wire-format columns plus QuestDB config — same audit-row shape as sibling self_trade_audit / sl_replacement_audit / order_update_ws_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_orphan_position_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    security_id: i64,
    exchange_segment: &str,
    trading_symbol: &str,
    product_type: &str,
    outcome: OrphanPositionAuditOutcome,
    reason: &str,
    net_qty: i64,
    buy_qty: i64,
    sell_qty: i64,
    realized_profit: f64,
    unrealized_profit: f64,
    dry_run: bool,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let outcome_str = outcome.as_str();
    let exchange_segment_esc = sanitize_audit_string(exchange_segment);
    let trading_symbol_esc = sanitize_audit_string(trading_symbol);
    let product_type_esc = sanitize_audit_string(product_type);
    let reason_esc = sanitize_audit_string(reason);
    // QuestDB TIMESTAMP columns store microseconds since epoch — divide
    // nanos to micros before embedding so the value stays in range.
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let dry_run_str = if dry_run { "true" } else { "false" };
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_ORPHAN_POSITION_AUDIT} \
         (ts, trading_date_ist, security_id, exchange_segment, trading_symbol, \
          product_type, outcome, reason, net_qty, buy_qty, sell_qty, \
          realized_profit, unrealized_profit, dry_run) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, {security_id}, \
          '{exchange_segment_esc}', '{trading_symbol_esc}', '{product_type_esc}', \
          '{outcome_str}', '{reason_esc}', {net_qty}, {buy_qty}, {sell_qty}, \
          {realized_profit}, {unrealized_profit}, {dry_run_str});"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("orphan_position_audit insert non-2xx ({status}): {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_ORPHAN_POSITION_AUDIT, "orphan_position_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + audit queries depend on these.
        assert_eq!(OrphanPositionAuditOutcome::NoOrphans.as_str(), "no_orphans");
        assert_eq!(OrphanPositionAuditOutcome::Detected.as_str(), "detected");
        assert_eq!(
            OrphanPositionAuditOutcome::AutoClosed.as_str(),
            "auto_closed"
        );
        assert_eq!(
            OrphanPositionAuditOutcome::ExitFailed.as_str(),
            "exit_failed"
        );
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated
    /// timestamp column in every DEDUP UPSERT KEYS clause. Without
    /// `ts` the CREATE TABLE returns 400 Bad Request and the table
    /// is silently missing for the whole session.
    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        assert!(
            DEDUP_KEY_ORPHAN_POSITION_AUDIT.contains("ts"),
            "orphan_position_audit DEDUP key must include `ts` (designated \
             timestamp); QuestDB rejects DDL otherwise. \
             Got: {DEDUP_KEY_ORPHAN_POSITION_AUDIT}"
        );
    }

    /// I-P1-11: `security_id` alone is not unique across segments.
    /// The DEDUP key MUST pair `security_id` with `exchange_segment`.
    #[test]
    fn test_dedup_key_pairs_security_id_with_exchange_segment() {
        assert!(DEDUP_KEY_ORPHAN_POSITION_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_ORPHAN_POSITION_AUDIT.contains("exchange_segment"));
        assert!(DEDUP_KEY_ORPHAN_POSITION_AUDIT.contains("trading_date_ist"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class
    /// (see `selftest_audit_persistence.rs`). Source-scan ratchet
    /// locks the conversion line.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("orphan_position_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }

    /// The DDL must reflect the operator-facing column set so future
    /// schema drift fails the test loudly instead of silently breaking
    /// downstream queries.
    #[test]
    fn test_ddl_contains_expected_columns() {
        let src = include_str!("orphan_position_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "security_id LONG",
            "exchange_segment SYMBOL",
            "trading_symbol SYMBOL",
            "product_type SYMBOL",
            "outcome SYMBOL",
            "reason SYMBOL",
            "net_qty LONG",
            "buy_qty LONG",
            "sell_qty LONG",
            "realized_profit DOUBLE",
            "unrealized_profit DOUBLE",
            "dry_run BOOLEAN",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break \
                 Grafana queries"
            );
        }
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        // Port 1 is reserved — guaranteed connection refusal.
        let cfg = test_cfg(1);
        let result = append_orphan_position_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            72271, // sample SecurityId
            "NSE_FNO",
            "NIFTY-Mar2026-24500-CE",
            "INTRADAY",
            OrphanPositionAuditOutcome::Detected,
            "dry_run=true",
            50,
            50,
            0,
            0.0,
            -1250.50,
            true,
        )
        .await;
        assert!(
            result.is_err(),
            "append must surface connection errors to caller"
        );
    }

    #[tokio::test]
    async fn test_append_no_orphans_green_path_row() {
        // The green-path daily marker carries security_id = 0 so the
        // Grafana panel can SELECT last(outcome) WHERE security_id = 0
        // to confirm the watchdog ran today.
        let cfg = test_cfg(1);
        let result = append_orphan_position_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            0, // green-path marker
            "",
            "",
            "",
            OrphanPositionAuditOutcome::NoOrphans,
            "",
            0,
            0,
            0,
            0.0,
            0.0,
            true,
        )
        .await;
        assert!(result.is_err(), "still err on unreachable QuestDB");
    }
}
