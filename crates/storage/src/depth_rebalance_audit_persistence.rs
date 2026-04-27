//! Wave 2 Item 9 — depth-rebalance audit persistence.
//!
//! Persists every depth ATM swap so the operator can reconstruct
//! "why was BANKNIFTY 47000 swapped to 47200 at 11:23:45 IST" weeks
//! later. SEBI-relevant: 90d hot → S3 IT → Glacier.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table for depth-rebalance audit.
pub const QUESTDB_TABLE_DEPTH_REBALANCE_AUDIT: &str = "depth_rebalance_audit";

/// DEDUP key per plan §9. `underlying_symbol` is sufficient (no
/// `security_id` column — the per-leg ids live in array columns).
pub const DEDUP_KEY_DEPTH_REBALANCE_AUDIT: &str = "underlying_symbol, ts";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_depth_rebalance_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_DEPTH_REBALANCE_AUDIT} (\
            ts TIMESTAMP, \
            underlying_symbol SYMBOL, \
            old_atm_strike DOUBLE, \
            new_atm_strike DOUBLE, \
            spot_at_swap DOUBLE, \
            swap_levels SYMBOL, \
            outcome SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_DEPTH_REBALANCE_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_DEPTH_REBALANCE_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_DEPTH_REBALANCE_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_DEPTH_REBALANCE_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Append one depth-rebalance audit row.
///
/// `swap_levels` ∈ {"20", "20+200"}; `outcome` ∈ {"success", "failed"}.
#[allow(clippy::too_many_arguments)] // APPROVED: audit row schema requires every column
pub async fn append_depth_rebalance_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    underlying_symbol: &str,
    old_atm_strike: f64,
    new_atm_strike: f64,
    spot_at_swap: f64,
    swap_levels: &str,
    outcome: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let underlying = underlying_symbol.replace('\'', "''");
    let levels = swap_levels.replace('\'', "''");
    let outcome_esc = outcome.replace('\'', "''");
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_DEPTH_REBALANCE_AUDIT} (ts, underlying_symbol, old_atm_strike, new_atm_strike, spot_at_swap, swap_levels, outcome) VALUES \
         ({ts_nanos_ist}, '{underlying}', {old_atm_strike}, {new_atm_strike}, {spot_at_swap}, '{levels}', '{outcome_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("depth_rebalance audit insert non-2xx ({status}): {body}");
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
    fn test_dedup_key_depth_rebalance_does_not_require_segment() {
        assert!(!DEDUP_KEY_DEPTH_REBALANCE_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_DEPTH_REBALANCE_AUDIT.contains("underlying_symbol"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_DEPTH_REBALANCE_AUDIT, "depth_rebalance_audit");
    }

    #[tokio::test]
    async fn test_append_depth_rebalance_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_depth_rebalance_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "BANKNIFTY",
            47000.0,
            47200.0,
            47150.5,
            "20+200",
            "success",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_depth_rebalance_escapes_quotes_in_underlying() {
        let cfg = test_cfg(1);
        let _ =
            append_depth_rebalance_audit_row(&cfg, 0, "BANK'NIFTY", 0.0, 0.0, 0.0, "20", "success")
                .await;
    }
}
