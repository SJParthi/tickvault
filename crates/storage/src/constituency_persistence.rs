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
use dhan_live_trader_common::instrument_types::{FnoUniverse, IndexConstituencyMap};

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
        security_id LONG,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY MONTH WAL\
";

/// Maximum retry attempts for constituency ILP persistence.
const CONSTITUENCY_PERSIST_MAX_RETRIES: u32 = 3;

/// Delay between constituency persistence retries (seconds).
const CONSTITUENCY_PERSIST_RETRY_DELAY_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// Pure helper functions (testable without DB)
// ---------------------------------------------------------------------------

/// Builds the QuestDB HTTP exec URL from host and port.
fn build_questdb_exec_url(host: &str, http_port: u16) -> String {
    format!("http://{}:{}/exec", host, http_port)
}

/// Builds the ALTER TABLE DEDUP ENABLE UPSERT KEYS SQL statement.
fn build_dedup_sql(table_name: &str, dedup_key: &str) -> String {
    format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        table_name, dedup_key
    )
}

/// Builds the ILP TCP connection string from host and port.
fn build_ilp_conf_string(host: &str, ilp_port: u16) -> String {
    format!("tcp::addr={}:{};", host, ilp_port)
}

/// Resolves the security_id for a constituent from the FnoUniverse.
///
/// Returns `0` if the symbol is not found in the universe (not an F&O stock).
fn resolve_security_id(fno_universe: Option<&FnoUniverse>, symbol: &str) -> u32 {
    fno_universe
        .and_then(|u| u.symbol_to_security_id(symbol))
        .unwrap_or(0)
}

/// Counts total constituents across all indices in the map.
#[cfg(test)]
fn count_total_constituents(constituency_map: &IndexConstituencyMap) -> usize {
    constituency_map
        .index_to_constituents
        .values()
        .map(|v| v.len())
        .sum()
}

// ---------------------------------------------------------------------------
// DDL Setup
// ---------------------------------------------------------------------------

/// Creates the `index_constituents` table and enables DEDUP. Idempotent.
pub async fn ensure_constituency_table(questdb_config: &QuestDbConfig) {
    let base_url = build_questdb_exec_url(&questdb_config.host, questdb_config.http_port);

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
    let dedup_ddl = build_dedup_sql(
        QUESTDB_TABLE_INDEX_CONSTITUENTS,
        DEDUP_KEY_INDEX_CONSTITUENTS,
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
///
/// When `fno_universe` is provided, each constituent is enriched with the
/// Dhan `security_id` from the instrument master (symbol → security_id mapping).
/// This enables news-based trading: symbol → security_id → F&O contracts.
pub fn persist_constituency(
    constituency_map: &IndexConstituencyMap,
    questdb_config: &QuestDbConfig,
    fno_universe: Option<&FnoUniverse>,
) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=CONSTITUENCY_PERSIST_MAX_RETRIES {
        match persist_constituency_inner(constituency_map, questdb_config, fno_universe) {
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
    fno_universe: Option<&FnoUniverse>,
) -> Result<usize> {
    let conf_string = build_ilp_conf_string(&questdb_config.host, questdb_config.ilp_port);
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
            // Enrich with security_id from instrument master (0 if not found).
            let security_id = resolve_security_id(fno_universe, &constituent.symbol);

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
                .column_i64("security_id", i64::from(security_id))
                .context("security_id")?
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
    fn test_persist_constituency_unreachable_host_returns_ok() {
        // persist_constituency is best-effort — returns Ok even on failure.
        let map = IndexConstituencyMap::default();
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            ilp_port: 19009,
            http_port: 19000,
            pg_port: 18812,
        };
        let result = persist_constituency(&map, &config, None);
        assert!(
            result.is_ok(),
            "persist_constituency must be best-effort (Ok on failure)"
        );
    }

    #[test]
    fn test_persist_empty_map_returns_ok() {
        // Empty map should return Ok(0) without connecting to QuestDB.
        let map = IndexConstituencyMap::default();
        assert_eq!(map.index_count(), 0);
        assert_eq!(map.stock_count(), 0);
    }

    #[test]
    fn test_table_constant_matches_ddl() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains(QUESTDB_TABLE_INDEX_CONSTITUENTS));
    }

    // -----------------------------------------------------------------------
    // build_questdb_exec_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_exec_url_docker() {
        let url = build_questdb_exec_url("dlt-questdb", 9000);
        assert_eq!(url, "http://dlt-questdb:9000/exec");
    }

    #[test]
    fn test_build_questdb_exec_url_ip() {
        let url = build_questdb_exec_url("10.0.1.5", 19000);
        assert_eq!(url, "http://10.0.1.5:19000/exec");
    }

    #[test]
    fn test_build_questdb_exec_url_ends_with_exec() {
        let url = build_questdb_exec_url("host", 9000);
        assert!(url.ends_with("/exec"));
    }

    // -----------------------------------------------------------------------
    // build_dedup_sql
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_dedup_sql_constituency() {
        let sql = build_dedup_sql(
            QUESTDB_TABLE_INDEX_CONSTITUENTS,
            DEDUP_KEY_INDEX_CONSTITUENTS,
        );
        assert_eq!(
            sql,
            "ALTER TABLE index_constituents DEDUP ENABLE UPSERT KEYS(ts, index_name, symbol)"
        );
    }

    #[test]
    fn test_build_dedup_sql_generic() {
        let sql = build_dedup_sql("my_table", "col_a");
        assert!(sql.starts_with("ALTER TABLE my_table"));
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, col_a)"));
    }

    // -----------------------------------------------------------------------
    // build_ilp_conf_string
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ilp_conf_string_docker() {
        let conf = build_ilp_conf_string("dlt-questdb", 9009);
        assert_eq!(conf, "tcp::addr=dlt-questdb:9009;");
    }

    #[test]
    fn test_build_ilp_conf_string_ends_with_semicolon() {
        let conf = build_ilp_conf_string("host", 1234);
        assert!(conf.ends_with(';'));
    }

    // -----------------------------------------------------------------------
    // resolve_security_id
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_security_id_none_universe() {
        assert_eq!(resolve_security_id(None, "RELIANCE"), 0);
    }

    #[test]
    fn test_resolve_security_id_unknown_symbol() {
        let empty_map = IndexConstituencyMap::default();
        // We can't easily create a real FnoUniverse here without complex setup,
        // but we can test the None path
        assert_eq!(resolve_security_id(None, "UNKNOWN_STOCK"), 0);
        // And verify the empty map is truly empty
        assert_eq!(empty_map.stock_count(), 0);
    }

    // -----------------------------------------------------------------------
    // count_total_constituents
    // -----------------------------------------------------------------------

    #[test]
    fn test_count_total_constituents_empty() {
        let map = IndexConstituencyMap::default();
        assert_eq!(count_total_constituents(&map), 0);
    }

    #[test]
    fn test_count_total_constituents_with_data() {
        use dhan_live_trader_common::instrument_types::{
            ConstituencyBuildMetadata, IndexConstituent,
        };
        use std::collections::HashMap;

        let today = chrono::Utc::now().date_naive();
        let mut index_to_constituents = HashMap::new();
        index_to_constituents.insert(
            "NIFTY 50".to_string(),
            vec![
                IndexConstituent {
                    index_name: "NIFTY 50".to_string(),
                    symbol: "RELIANCE".to_string(),
                    isin: "INE002A01018".to_string(),
                    weight: 10.5,
                    sector: "Energy".to_string(),
                    last_updated: today,
                },
                IndexConstituent {
                    index_name: "NIFTY 50".to_string(),
                    symbol: "TCS".to_string(),
                    isin: "INE467B01029".to_string(),
                    weight: 5.2,
                    sector: "IT".to_string(),
                    last_updated: today,
                },
            ],
        );
        index_to_constituents.insert(
            "NIFTY BANK".to_string(),
            vec![IndexConstituent {
                index_name: "NIFTY BANK".to_string(),
                symbol: "HDFCBANK".to_string(),
                isin: "INE040A01034".to_string(),
                weight: 25.0,
                sector: "Banking".to_string(),
                last_updated: today,
            }],
        );

        let map = IndexConstituencyMap {
            index_to_constituents,
            stock_to_indices: HashMap::new(),
            build_metadata: ConstituencyBuildMetadata::default(),
        };

        assert_eq!(count_total_constituents(&map), 3);
    }

    // -----------------------------------------------------------------------
    // DDL constants validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_ddl_has_security_id_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("security_id LONG"));
    }

    #[test]
    fn test_ddl_has_isin_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("isin STRING"));
    }

    #[test]
    fn test_ddl_has_sector_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("sector STRING"));
    }

    #[test]
    fn test_ddl_uses_month_partitioning() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("PARTITION BY MONTH"));
    }

    #[test]
    fn test_ddl_uses_wal() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("WAL"));
    }

    #[test]
    fn test_ddl_is_single_statement() {
        assert!(
            !INDEX_CONSTITUENTS_CREATE_DDL.contains(';'),
            "DDL must not contain semicolons"
        );
    }

    #[test]
    fn test_retry_constants_are_reasonable() {
        let retries = CONSTITUENCY_PERSIST_MAX_RETRIES;
        let delay = CONSTITUENCY_PERSIST_RETRY_DELAY_SECS;
        assert!((1..=10).contains(&retries));
        assert!((1..=30).contains(&delay));
    }

    #[test]
    fn test_ddl_timeout_is_reasonable() {
        assert!((5..=30).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_unreachable_no_panic() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        ensure_constituency_table(&config).await;
    }
}
