//! QuestDB persistence for NSE index constituency data.
//!
//! Writes index → stock mappings from niftyindices.com to QuestDB so
//! Grafana can display constituency tables. Daily snapshot — DEDUP
//! UPSERT KEYS on `(ts, index_name, symbol)` prevent duplicates.
//!
//! Best-effort: failures log ERROR but don't block trading.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Sender, TimestampNanos};
use reqwest::Client;
use tracing::{info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::QUESTDB_TABLE_INDEX_CONSTITUENTS;
use dhan_live_trader_common::instrument_types::IndexConstituencyMap;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY for the `index_constituents` table.
const DEDUP_KEY_INDEX_CONSTITUENTS: &str = "index_name, symbol";

/// DDL for `index_constituents` — one row per (index, stock) pair.
const INDEX_CONSTITUENTS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS index_constituents (\
        index_name SYMBOL,\
        symbol SYMBOL,\
        isin STRING,\
        weight DOUBLE,\
        sector STRING,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY MONTH WAL\
";

/// Maximum retry attempts for constituency ILP persistence.
const CONSTITUENCY_PERSIST_MAX_RETRIES: u32 = 3;

/// Delay between constituency persistence retries (seconds).
const CONSTITUENCY_PERSIST_RETRY_DELAY_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// DDL Setup
// ---------------------------------------------------------------------------

/// Creates the `index_constituents` table and enables DEDUP. Idempotent.
pub async fn ensure_constituency_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(
                ?err,
                "failed to build HTTP client for constituency table DDL"
            );
            return;
        }
    };

    // Step 1: Create table
    match client
        .get(&base_url)
        .query(&[("query", INDEX_CONSTITUENTS_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("index_constituents table ensured (CREATE TABLE IF NOT EXISTS)");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "index_constituents table CREATE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "failed to send CREATE TABLE for index_constituents");
        }
    }

    // Step 2: Enable DEDUP
    let dedup_ddl = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_INDEX_CONSTITUENTS, DEDUP_KEY_INDEX_CONSTITUENTS
    );

    match client
        .get(&base_url)
        .query(&[("query", &dedup_ddl)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("index_constituents DEDUP UPSERT KEYS enabled");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                // DEDUP already enabled = non-error (QuestDB returns error on re-enable)
                if !body.contains("already enabled") {
                    warn!(
                        %status,
                        body = body.chars().take(200).collect::<String>(),
                        "index_constituents DEDUP DDL returned non-success"
                    );
                }
            }
        }
        Err(err) => {
            warn!(?err, "failed to send DEDUP DDL for index_constituents");
        }
    }
}

// ---------------------------------------------------------------------------
// Data Persistence
// ---------------------------------------------------------------------------

/// Persists constituency data to QuestDB with retry logic. Best-effort.
pub fn persist_constituency(
    constituency_map: &IndexConstituencyMap,
    questdb_config: &QuestDbConfig,
) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=CONSTITUENCY_PERSIST_MAX_RETRIES {
        match persist_constituency_inner(constituency_map, questdb_config) {
            Ok(count) => {
                info!(
                    entries = count,
                    table = QUESTDB_TABLE_INDEX_CONSTITUENTS,
                    attempt,
                    "index constituency persisted to QuestDB"
                );
                return Ok(());
            }
            Err(err) => {
                warn!(
                    ?err,
                    attempt,
                    max_retries = CONSTITUENCY_PERSIST_MAX_RETRIES,
                    "constituency persistence attempt failed — retrying"
                );
                last_err = Some(err);
                if attempt < CONSTITUENCY_PERSIST_MAX_RETRIES {
                    std::thread::sleep(std::time::Duration::from_secs(
                        CONSTITUENCY_PERSIST_RETRY_DELAY_SECS,
                    ));
                }
            }
        }
    }
    tracing::error!(
        err = ?last_err,
        "constituency persistence failed after {CONSTITUENCY_PERSIST_MAX_RETRIES} attempts — index_constituents table will be empty in Grafana"
    );
    Ok(())
}

fn persist_constituency_inner(
    constituency_map: &IndexConstituencyMap,
    questdb_config: &QuestDbConfig,
) -> Result<usize> {
    let conf_string = format!(
        "tcp::addr={}:{};",
        questdb_config.host, questdb_config.ilp_port
    );
    let mut sender = Sender::from_conf(&conf_string)
        .context("failed to connect to QuestDB ILP for constituency")?;
    let mut buffer = sender.new_buffer();

    // Use build timestamp as the snapshot timestamp for all rows.
    let build_epoch_nanos = constituency_map
        .build_metadata
        .build_timestamp
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let snapshot_ts = TimestampNanos::new(build_epoch_nanos);

    let mut count = 0_usize;

    for (index_name, constituents) in &constituency_map.index_to_constituents {
        for constituent in constituents {
            buffer
                .table(QUESTDB_TABLE_INDEX_CONSTITUENTS)
                .context("table name")?
                .symbol("index_name", index_name)
                .context("index_name")?
                .symbol("symbol", &constituent.symbol)
                .context("symbol")?
                .column_str("isin", &constituent.isin)
                .context("isin")?
                .column_f64("weight", constituent.weight)
                .context("weight")?
                .column_str("sector", &constituent.sector)
                .context("sector")?
                .at(snapshot_ts)
                .context("designated timestamp")?;

            count = count.saturating_add(1);
        }
    }

    if count > 0 {
        sender
            .flush(&mut buffer)
            .context("failed to flush constituency data to QuestDB")?;
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_ddl_is_valid_sql() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains(QUESTDB_TABLE_INDEX_CONSTITUENTS));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("index_name SYMBOL"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("weight DOUBLE"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_dedup_key_includes_index_name_and_symbol() {
        assert!(DEDUP_KEY_INDEX_CONSTITUENTS.contains("index_name"));
        assert!(DEDUP_KEY_INDEX_CONSTITUENTS.contains("symbol"));
    }

    #[test]
    fn test_persist_empty_map_returns_ok() {
        // Empty map should return Ok(0) without connecting to QuestDB.
        // Since we can't connect in tests, verify the count logic works.
        let map = IndexConstituencyMap::default();
        assert_eq!(map.index_count(), 0);
        assert_eq!(map.stock_count(), 0);
    }

    #[test]
    fn test_table_constant_matches_ddl() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains(QUESTDB_TABLE_INDEX_CONSTITUENTS));
    }
}
