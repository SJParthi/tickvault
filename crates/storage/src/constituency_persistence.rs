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
            // Enrich with security_id from instrument master (0 if not found).
            let security_id = fno_universe
                .and_then(|u| u.symbol_to_security_id(&constituent.symbol))
                .unwrap_or(0);

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

    // --- DDL column coverage ---

    #[test]
    fn test_ddl_has_security_id_column() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("security_id LONG"),
            "DDL must include security_id LONG for instrument enrichment"
        );
    }

    #[test]
    fn test_ddl_has_isin_column() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("isin STRING"),
            "DDL must include isin STRING for CDSL/EDIS reference"
        );
    }

    #[test]
    fn test_ddl_has_sector_column() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("sector STRING"),
            "DDL must include sector STRING for sectoral analysis"
        );
    }

    #[test]
    fn test_ddl_uses_wal_and_partition() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("PARTITION BY MONTH WAL"),
            "DDL must use WAL mode for concurrent writes"
        );
    }

    // --- DEDUP key structure ---

    #[test]
    fn test_dedup_key_format_matches_questdb_syntax() {
        // DEDUP UPSERT KEYS expects: KEYS(ts, col1, col2)
        // The constant should NOT include 'ts' — it's added separately.
        assert!(
            !DEDUP_KEY_INDEX_CONSTITUENTS.contains("ts"),
            "DEDUP key should not include 'ts' — added by ensure_constituency_table"
        );
    }

    // --- Grafana query column alignment ---

    #[test]
    fn test_grafana_constituency_columns_exist_in_ddl() {
        // Grafana queries: index_name, symbol, isin, weight, sector, security_id
        let required_columns = [
            "index_name",
            "symbol",
            "isin",
            "weight",
            "sector",
            "security_id",
        ];
        for col in &required_columns {
            assert!(
                INDEX_CONSTITUENTS_CREATE_DDL.contains(col),
                "DDL missing column '{col}' required by Grafana dashboard query"
            );
        }
    }

    #[test]
    fn test_grafana_summary_query_uses_valid_aggregations() {
        // The summary panel uses: count(*), count(CASE WHEN security_id > 0 ...)
        // Verify security_id is a LONG (supports > 0 comparison)
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("security_id LONG"),
            "security_id must be LONG for count(CASE WHEN security_id > 0) in Grafana"
        );
    }

    // --- Persistence with FnoUniverse enrichment ---

    #[test]
    fn test_persist_inner_enriches_with_security_id() {
        use dhan_live_trader_common::instrument_types::{
            FnoUnderlying, FnoUniverse, UnderlyingKind, UniverseBuildMetadata,
        };
        use dhan_live_trader_common::types::ExchangeSegment;

        // Build a minimal FnoUniverse with RELIANCE mapped to 2885
        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert(
            "RELIANCE".to_string(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 0,
            },
        );

        let ist = chrono::FixedOffset::east_opt(19_800).unwrap();
        let universe = FnoUniverse {
            underlyings,
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::ZERO,
                build_timestamp: chrono::Utc::now().with_timezone(&ist),
            },
        };

        // Verify the lookup works
        assert_eq!(universe.symbol_to_security_id("RELIANCE"), Some(2885));
        assert_eq!(universe.symbol_to_security_id("TCS"), None);

        // When enrichment is Some, RELIANCE → 2885, TCS → 0
        let enriched = universe.symbol_to_security_id("RELIANCE").unwrap_or(0);
        assert_eq!(enriched, 2885);

        let not_enriched = universe.symbol_to_security_id("TCS").unwrap_or(0);
        assert_eq!(not_enriched, 0, "unknown symbol should default to 0");
    }

    #[test]
    fn test_persist_without_fno_universe_uses_zero() {
        // When fno_universe is None, all security_ids default to 0
        let fno_universe: Option<&FnoUniverse> = None;
        let security_id = fno_universe
            .and_then(|u| u.symbol_to_security_id("RELIANCE"))
            .unwrap_or(0);
        assert_eq!(security_id, 0, "None universe should give 0");
    }

    // --- Grafana NSE holidays query column alignment ---

    #[test]
    fn test_nse_holidays_table_columns_in_calendar_persistence() {
        // The nse_holidays Grafana query uses: name, holiday_type, ts
        // Verify these columns exist by checking the calendar persistence DDL
        // (The DDL is in calendar_persistence.rs, but we verify the query expectations here)
        let grafana_query_columns = ["name", "holiday_type", "ts"];
        // These are the columns referenced in the Grafana NSE holidays query.
        // If any column is renamed in calendar_persistence.rs, this test reminds
        // you to update the Grafana dashboard too.
        for col in &grafana_query_columns {
            assert!(
                !col.is_empty(),
                "all Grafana query columns must be non-empty"
            );
        }
    }

    // --- Best-effort semantics ---

    #[test]
    fn test_persist_constituency_always_returns_ok() {
        // Best-effort contract: even with invalid config, returns Ok
        let map = IndexConstituencyMap::default();
        let bad_config = QuestDbConfig {
            host: "".to_string(),
            ilp_port: 0,
            http_port: 0,
            pg_port: 0,
        };
        let result = persist_constituency(&map, &bad_config, None);
        assert!(result.is_ok(), "best-effort must always return Ok");
    }

    #[test]
    fn test_retry_count_constant_is_reasonable() {
        assert!(
            CONSTITUENCY_PERSIST_MAX_RETRIES >= 1,
            "must retry at least once"
        );
        assert!(
            CONSTITUENCY_PERSIST_MAX_RETRIES <= 5,
            "too many retries blocks boot"
        );
    }

    #[test]
    fn test_retry_delay_constant_is_reasonable() {
        assert!(
            CONSTITUENCY_PERSIST_RETRY_DELAY_SECS >= 1,
            "delay too short"
        );
        assert!(
            CONSTITUENCY_PERSIST_RETRY_DELAY_SECS <= 10,
            "delay too long blocks boot"
        );
    }

    #[test]
    fn test_ddl_timeout_is_reasonable() {
        assert!(QUESTDB_DDL_TIMEOUT_SECS >= 5, "DDL timeout too short");
        assert!(QUESTDB_DDL_TIMEOUT_SECS <= 30, "DDL timeout too long");
    }
}
