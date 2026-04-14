//! QuestDB partition lifecycle manager.
//!
//! Detaches partitions older than the configured retention window.
//! Runs daily at 16:30 IST (after market close) to keep hot data bounded.
//!
//! # Partition Strategy
//! - HOUR-partitioned tables (ticks, depth, OBI, greeks): detach hourly partitions > N days old
//! - DAY-partitioned tables (candles, instruments, calendar): detach daily partitions > N days old
//!
//! # SEBI Compliance
//! Detached partitions remain on disk until physically removed.
//! Phase B.7 (S3 archival) exports to S3 before removal for 5-year retention.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{debug, info, warn};

use tickvault_common::config::QuestDbConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// HTTP timeout for partition management queries.
const PARTITION_DDL_TIMEOUT_SECS: u64 = 30;

/// Tables with HOUR partitioning (high-frequency data).
const HOUR_PARTITIONED_TABLES: &[&str] = &[
    "ticks",
    "market_depth",
    "deep_market_depth",
    "obi_snapshots",
    "option_greeks",
    "dhan_option_chain_raw",
    "greeks_verification",
    "pcr_snapshots",
    "indicator_snapshots",
    "stock_movers",
    "option_movers",
];

/// Tables with DAY partitioning (lower-frequency data).
const DAY_PARTITIONED_TABLES: &[&str] = &["historical_candles", "candles_1s"];

// ---------------------------------------------------------------------------
// Partition Manager
// ---------------------------------------------------------------------------

/// Manages QuestDB partition lifecycle (detach old partitions).
pub struct PartitionManager {
    base_url: String,
    client: Client,
    retention_days: u32,
}

impl PartitionManager {
    /// Creates a new partition manager.
    ///
    /// # Arguments
    /// * `config` — QuestDB connection config.
    /// * `retention_days` — Hot partition retention in days (0 = disabled).
    // TEST-EXEMPT: HTTP client creation — requires live QuestDB
    pub fn new(config: &QuestDbConfig, retention_days: u32) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(PARTITION_DDL_TIMEOUT_SECS))
            .build()
            .context("failed to create HTTP client for partition manager")?;

        Ok(Self {
            base_url: format!("http://{}:{}/exec", config.host, config.http_port),
            client,
            retention_days,
        })
    }

    /// Detaches all partitions older than the retention window.
    ///
    /// Iterates over all known tables and detaches partitions where
    /// the partition timestamp is older than `now - retention_days`.
    ///
    /// Returns the total count of partitions detached.
    // TEST-EXEMPT: requires live QuestDB with populated tables
    pub async fn detach_old_partitions(&self) -> Result<u32> {
        if self.retention_days == 0 {
            info!("partition detach disabled (retention_days=0)");
            return Ok(0);
        }

        let cutoff_days = self.retention_days;
        let mut total_detached: u32 = 0;

        info!(
            retention_days = cutoff_days,
            hour_tables = HOUR_PARTITIONED_TABLES.len(),
            day_tables = DAY_PARTITIONED_TABLES.len(),
            "starting partition detach cycle"
        );

        // HOUR-partitioned tables
        for table in HOUR_PARTITIONED_TABLES {
            match self
                .detach_partitions_for_table(table, cutoff_days, "HOUR")
                .await
            {
                Ok(count) => {
                    total_detached = total_detached.saturating_add(count);
                    if count > 0 {
                        info!(table, detached = count, "partitions detached");
                    }
                }
                Err(err) => {
                    warn!(?err, table, "failed to detach partitions");
                }
            }
        }

        // DAY-partitioned tables
        for table in DAY_PARTITIONED_TABLES {
            match self
                .detach_partitions_for_table(table, cutoff_days, "DAY")
                .await
            {
                Ok(count) => {
                    total_detached = total_detached.saturating_add(count);
                    if count > 0 {
                        info!(table, detached = count, "partitions detached");
                    }
                }
                Err(err) => {
                    warn!(?err, table, "failed to detach partitions");
                }
            }
        }

        info!(total_detached, "partition detach cycle complete");

        Ok(total_detached)
    }

    /// Detaches old partitions for a single table.
    ///
    /// Uses QuestDB's `SHOW PARTITIONS FROM` to list partitions,
    /// then `ALTER TABLE DETACH PARTITION` for those beyond retention.
    async fn detach_partitions_for_table(
        &self,
        table: &str,
        cutoff_days: u32,
        _partition_by: &str,
    ) -> Result<u32> {
        // Query partitions older than cutoff
        let list_sql = format!(
            "SELECT name FROM table_partitions('{}') \
             WHERE minTimestamp < dateadd('d', -{}, now()) \
             AND active = true",
            table, cutoff_days
        );

        let response = self
            .client
            .get(&self.base_url)
            .query(&[("query", &list_sql)])
            .send()
            .await
            .context("partition list query failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>(); // O(1) EXEMPT: error logging
            debug!(%status, body, table, "partition list query returned non-success (table may not exist yet)");
            return Ok(0);
        }

        let body = response
            .text()
            .await
            .context("failed to read partition list response")?;

        let partition_names = parse_partition_names(&body);

        if partition_names.is_empty() {
            return Ok(0);
        }

        let mut detached: u32 = 0;
        for name in &partition_names {
            let detach_sql = format!("ALTER TABLE {} DETACH PARTITION LIST '{}'", table, name);

            match self
                .client
                .get(&self.base_url)
                .query(&[("query", &detach_sql)])
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    detached = detached.saturating_add(1);
                    debug!(table, partition = name, "partition detached");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp
                        .text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect::<String>(); // O(1) EXEMPT: error logging
                    warn!(%status, body, table, partition = name, "partition detach failed");
                }
                Err(err) => {
                    warn!(
                        ?err,
                        table,
                        partition = name,
                        "partition detach request failed"
                    );
                }
            }
        }

        Ok(detached)
    }
}

/// Parses partition names from QuestDB JSON response.
///
/// Expected format: `{"columns":[{"name":"name","type":"VARCHAR"}],"dataset":[["2026-04-01"],["2026-04-02"]],...}`
/// Returns a Vec of partition name strings.
fn parse_partition_names(json: &str) -> Vec<String> {
    // O(1) EXEMPT: begin — JSON parsing for DDL response (cold path, not hot)
    let parsed: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let dataset = match parsed.get("dataset").and_then(|d| d.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };

    dataset
        .iter()
        .filter_map(|row| {
            row.as_array()
                .and_then(|cols| cols.first())
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect()
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hour_partitioned_tables_not_empty() {
        assert!(!HOUR_PARTITIONED_TABLES.is_empty());
    }

    #[test]
    fn test_day_partitioned_tables_not_empty() {
        assert!(!DAY_PARTITIONED_TABLES.is_empty());
    }

    #[test]
    fn test_all_table_names_lowercase() {
        for table in HOUR_PARTITIONED_TABLES {
            assert_eq!(
                *table,
                table.to_ascii_lowercase(),
                "table name must be lowercase: {}",
                table
            );
        }
        for table in DAY_PARTITIONED_TABLES {
            assert_eq!(
                *table,
                table.to_ascii_lowercase(),
                "table name must be lowercase: {}",
                table
            );
        }
    }

    #[test]
    fn test_no_duplicate_tables() {
        let mut all_tables: Vec<&str> =
            Vec::with_capacity(HOUR_PARTITIONED_TABLES.len() + DAY_PARTITIONED_TABLES.len());
        all_tables.extend_from_slice(HOUR_PARTITIONED_TABLES);
        all_tables.extend_from_slice(DAY_PARTITIONED_TABLES);
        all_tables.sort();
        let before = all_tables.len();
        all_tables.dedup();
        assert_eq!(
            before,
            all_tables.len(),
            "duplicate table in partition lists"
        );
    }

    #[test]
    fn test_parse_partition_names_valid() {
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"}],"dataset":[["2026-04-01"],["2026-04-02"]],"count":2}"#;
        let names = parse_partition_names(json);
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "2026-04-01");
        assert_eq!(names[1], "2026-04-02");
    }

    #[test]
    fn test_parse_partition_names_empty_dataset() {
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"}],"dataset":[],"count":0}"#;
        let names = parse_partition_names(json);
        assert!(names.is_empty());
    }

    #[test]
    fn test_parse_partition_names_invalid_json() {
        let names = parse_partition_names("not json");
        assert!(names.is_empty());
    }

    #[test]
    fn test_parse_partition_names_missing_dataset() {
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"}]}"#;
        let names = parse_partition_names(json);
        assert!(names.is_empty());
    }

    #[test]
    fn test_parse_partition_names_hour_format() {
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"}],"dataset":[["2026-04-01T09"],["2026-04-01T10"]],"count":2}"#;
        let names = parse_partition_names(json);
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "2026-04-01T09");
    }

    #[test]
    fn test_obi_in_hour_partitioned_tables() {
        assert!(
            HOUR_PARTITIONED_TABLES.contains(&"obi_snapshots"),
            "obi_snapshots must be in HOUR-partitioned list"
        );
    }

    #[test]
    fn test_ticks_in_hour_partitioned_tables() {
        assert!(
            HOUR_PARTITIONED_TABLES.contains(&"ticks"),
            "ticks must be in HOUR-partitioned list"
        );
    }

    #[test]
    fn test_default_retention_config() {
        use tickvault_common::config::PartitionRetentionConfig;
        let config = PartitionRetentionConfig::default();
        assert_eq!(config.retention_days, 90);
    }
}
