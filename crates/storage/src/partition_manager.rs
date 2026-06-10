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
///
/// 2026-04-28 (Phase 8): added 22 `movers_{T}` tables for the movers
/// 22-timeframe redesign. Each table is `PARTITION BY DAY WAL` but is
/// listed here because it's high-frequency snapshot data subject to the
/// same retention rotation. The constant name predates the rotation-vs-
/// partition-period distinction.
// 2026-06-05: corrected to the live post-cleanup reality. The previous lists
// named DELETED tables (greeks / depth / movers / option_chain — modules gone)
// and OMITTED every live growing table, so the retention sweep visited nothing
// real → unbounded active-table growth (a storage/cost runaway). `ticks` is the
// only live HOUR-partitioned table.
pub(crate) const HOUR_PARTITIONED_TABLES: &[&str] = &["ticks"];

/// DAY-partitioned **audit + daily-data** tables the retention sweep DETACHes
/// past the hot window. The 21 live **candle** tables (`candles_1m` …
/// `candles_1d`) are NOT listed here — they are swept by iterating
/// [`crate::shadow_persistence::candle_table_names`] (the single source of
/// truth, `TfIndex::table_name()`) so the candle names can never drift.
///
/// **Honest boundary:** `detach_old_partitions` uses `ALTER TABLE … DETACH
/// PARTITION` — it moves old partitions to a local `detached/` dir (SEBI-safe,
/// never DROP). Actually freeing EBS requires S3-upload + local cleanup of the
/// detached partitions — a SEPARATE, AWS-dependent piece that is NOT yet
/// implemented. This list bounds the *active* table and is the prerequisite for
/// archival; it does not by itself reclaim disk.
pub(crate) const DAY_PARTITIONED_TABLES: &[&str] = &[
    // Audit + daily-data tables (SEBI 5y → detach to S3 cold when archival ships).
    "instrument_fetch_audit",
    "instrument_lifecycle_audit",
    "option_chain_minute_snapshot",
    "prev_day_ohlcv",
    "cross_verify_1m_audit",
    "index_constituency",
    // TICK-CONSERVE-01 (2026-06-10): one row per daily conservation-audit
    // run — same SEBI-audit class + DAY partitioning as cross_verify_1m_audit.
    "tick_conservation_audit",
];

/// Tables EXEMPT from retention sweeping — NEVER detached or dropped.
///
/// `instrument_lifecycle` is the SEBI point-in-time master: every instrument
/// EVER observed, never deleted (daily-universe §5/§25). It must stay whole for
/// point-in-time reconstruction, so it is never swept even though it is
/// `PARTITION BY DAY`. It is small (~219K rows) so it does not need sweeping.
pub(crate) const RETENTION_EXEMPT_TABLES: &[&str] = &["instrument_lifecycle"];

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

        // HOUR-partitioned tables. Defense-in-depth: never sweep an exempt
        // (SEBI point-in-time, never-delete) table even if one is mistakenly
        // added to a sweep list — the exempt set is the final guard.
        for table in HOUR_PARTITIONED_TABLES {
            if RETENTION_EXEMPT_TABLES.contains(table) {
                continue;
            }
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

        // DAY-partitioned audit/data tables (same exempt guard as the HOUR loop).
        for table in DAY_PARTITIONED_TABLES {
            if RETENTION_EXEMPT_TABLES.contains(table) {
                continue;
            }
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

        // The 21 live candle tables (`candles_1m` … `candles_1d`), DAY-partitioned.
        // Derived from `candle_table_names()` (the single source of truth,
        // `TfIndex::table_name()`) so the swept names can NEVER drift from what is
        // actually created/written. This is the dominant disk-growth source —
        // #1022 named phantom `candles_*_shadow` tables and missed it.
        for table in crate::shadow_persistence::candle_table_names() {
            if RETENTION_EXEMPT_TABLES.contains(&table) {
                continue;
            }
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
    fn test_ticks_is_the_only_hour_table() {
        assert_eq!(HOUR_PARTITIONED_TABLES, &["ticks"]);
    }

    #[test]
    fn test_day_list_holds_the_audit_data_tables() {
        // The DAY const holds ONLY the audit/data tables; candle tables are
        // swept via candle_table_names() in detach_old_partitions (see
        // test_candle_tables_are_real_plain_names).
        for live in [
            "instrument_fetch_audit",
            "instrument_lifecycle_audit",
            "option_chain_minute_snapshot",
            "prev_day_ohlcv",
            "cross_verify_1m_audit",
            "index_constituency",
        ] {
            assert!(
                DAY_PARTITIONED_TABLES.contains(&live),
                "audit/data table not covered by retention sweep: {live}"
            );
        }
    }

    #[test]
    fn test_candle_tables_are_real_plain_names_not_shadow() {
        // The candle tables swept by detach_old_partitions come from the single
        // source of truth. They MUST be plain `candles_<TF>` (no `_shadow`) and
        // number 21 — this is the exact bug #1022 had (phantom `_shadow` names).
        let names = crate::shadow_persistence::candle_table_names();
        assert_eq!(names.len(), 21, "expected 21 live candle tables");
        for name in names {
            assert!(
                name.starts_with("candles_"),
                "candle table name not canonical: {name}"
            );
            assert!(
                !name.contains("_shadow"),
                "candle table must be plain (no _shadow): {name}"
            );
        }
    }

    #[test]
    fn test_instrument_lifecycle_is_exempt_never_swept() {
        // SEBI point-in-time master — never detached/dropped (daily-universe §5/§25).
        assert!(RETENTION_EXEMPT_TABLES.contains(&"instrument_lifecycle"));
        assert!(
            !HOUR_PARTITIONED_TABLES.contains(&"instrument_lifecycle"),
            "instrument_lifecycle must NEVER be swept (SEBI)"
        );
        assert!(
            !DAY_PARTITIONED_TABLES.contains(&"instrument_lifecycle"),
            "instrument_lifecycle must NEVER be swept (SEBI)"
        );
    }

    #[test]
    fn test_exempt_tables_never_appear_in_a_sweep_list() {
        for ex in RETENTION_EXEMPT_TABLES {
            assert!(!HOUR_PARTITIONED_TABLES.contains(ex));
            assert!(!DAY_PARTITIONED_TABLES.contains(ex));
        }
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

    // (removed test_obi_in_hour_partitioned_tables — `obi_snapshots` is a
    // deleted table; the 2026-06-05 retention fix dropped it from the list.)

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
