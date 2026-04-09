//! QuestDB ILP persistence for OBI (Order Book Imbalance) snapshots.
//!
//! Stores computed OBI values from 20-level depth data for each instrument.
//! OBI measures the imbalance between bid and ask quantities across the order book.
//!
//! # Table Written
//! - `obi_snapshots` — one row per security per depth snapshot
//!
//! # Idempotency
//! DEDUP UPSERT KEYS on `(ts, security_id, segment)` prevent duplicates on reconnect.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::segment::segment_code_to_str;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// QuestDB table name for OBI snapshots.
pub const QUESTDB_TABLE_OBI_SNAPSHOTS: &str = "obi_snapshots";

/// DEDUP key for OBI snapshots.
const DEDUP_KEY_OBI: &str = "security_id, segment";

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Flush batch size for OBI snapshots.
const OBI_FLUSH_BATCH_SIZE: usize = 100;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL for the `obi_snapshots` table.
///
/// Stores per-instrument OBI computed from 20-level depth snapshots.
/// Partitioned by HOUR (depth updates arrive ~1/sec per instrument).
const OBI_SNAPSHOTS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS obi_snapshots (\
        segment SYMBOL,\
        security_id LONG,\
        underlying SYMBOL,\
        obi DOUBLE,\
        weighted_obi DOUBLE,\
        total_bid_qty LONG,\
        total_ask_qty LONG,\
        bid_levels LONG,\
        ask_levels LONG,\
        max_bid_wall_price DOUBLE,\
        max_bid_wall_qty LONG,\
        max_ask_wall_price DOUBLE,\
        max_ask_wall_qty LONG,\
        spread DOUBLE,\
        received_at TIMESTAMP,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

// ---------------------------------------------------------------------------
// DDL Execution
// ---------------------------------------------------------------------------

/// Ensures the `obi_snapshots` table exists in QuestDB.
// TEST-EXEMPT: DDL creation — requires live QuestDB
pub async fn ensure_obi_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    execute_ddl(&client, &base_url, OBI_SNAPSHOTS_DDL, "obi_snapshots").await;

    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_OBI_SNAPSHOTS, DEDUP_KEY_OBI
    );
    execute_ddl(&client, &base_url, &dedup_sql, "obi_snapshots DEDUP").await;
}

// ---------------------------------------------------------------------------
// OBI Snapshot Record (for ILP write)
// ---------------------------------------------------------------------------

/// All values needed to write one OBI snapshot row.
#[derive(Debug, Clone, Copy)]
pub struct ObiRecord {
    /// Dhan security ID.
    pub security_id: u32,
    /// Exchange segment byte code.
    pub segment_code: u8,
    /// OBI value: (total_bid - total_ask) / (total_bid + total_ask), range [-1, +1].
    pub obi: f64,
    /// Weighted OBI: levels closer to LTP weighted more.
    pub weighted_obi: f64,
    /// Sum of all bid quantities across 20 levels.
    pub total_bid_qty: u64,
    /// Sum of all ask quantities across 20 levels.
    pub total_ask_qty: u64,
    /// Number of non-empty bid levels.
    pub bid_levels: u32,
    /// Number of non-empty ask levels.
    pub ask_levels: u32,
    /// Price of the largest bid wall (qty > 10x average).
    pub max_bid_wall_price: f64,
    /// Quantity of the largest bid wall.
    pub max_bid_wall_qty: u64,
    /// Price of the largest ask wall (qty > 10x average).
    pub max_ask_wall_price: f64,
    /// Quantity of the largest ask wall.
    pub max_ask_wall_qty: u64,
    /// Spread: best_ask - best_bid.
    pub spread: f64,
    /// Timestamp in nanoseconds (IST).
    pub ts_nanos: i64,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Batched writer for OBI snapshots to QuestDB via ILP.
pub struct ObiWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
    /// Underlying symbol for this writer (used as SYMBOL column).
    underlying: String,
}

impl ObiWriter {
    /// Creates a new OBI writer connected to QuestDB via ILP TCP.
    // TEST-EXEMPT: ILP connection — requires live QuestDB
    pub fn new(config: &QuestDbConfig, underlying: &str) -> Result<Self> {
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for OBI snapshots")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
            underlying: underlying.to_owned(),
        })
    }

    /// Appends a single OBI snapshot row to the ILP buffer.
    // TEST-EXEMPT: ILP buffer append — requires live QuestDB
    pub fn append_obi(&mut self, record: &ObiRecord) -> Result<()> {
        // Reconnect if sender is dead.
        if self.sender.is_none() {
            self.try_reconnect()?;
        }

        let segment_str = segment_code_to_str(record.segment_code);
        let ts = TimestampNanos::new(record.ts_nanos);

        // ILP: table → symbols → columns → at
        self.buffer
            .table(QUESTDB_TABLE_OBI_SNAPSHOTS)
            .context("obi table")?
            // Symbols first
            .symbol("segment", segment_str)
            .context("segment")?
            .symbol("underlying", &self.underlying)
            .context("underlying")?
            // Value columns
            .column_i64("security_id", i64::from(record.security_id))
            .context("security_id")?
            .column_f64("obi", record.obi)
            .context("obi")?
            .column_f64("weighted_obi", record.weighted_obi)
            .context("weighted_obi")?
            .column_i64("total_bid_qty", record.total_bid_qty as i64)
            .context("total_bid_qty")?
            .column_i64("total_ask_qty", record.total_ask_qty as i64)
            .context("total_ask_qty")?
            .column_i64("bid_levels", i64::from(record.bid_levels))
            .context("bid_levels")?
            .column_i64("ask_levels", i64::from(record.ask_levels))
            .context("ask_levels")?
            .column_f64("max_bid_wall_price", record.max_bid_wall_price)
            .context("max_bid_wall_price")?
            .column_i64("max_bid_wall_qty", record.max_bid_wall_qty as i64)
            .context("max_bid_wall_qty")?
            .column_f64("max_ask_wall_price", record.max_ask_wall_price)
            .context("max_ask_wall_price")?
            .column_i64("max_ask_wall_qty", record.max_ask_wall_qty as i64)
            .context("max_ask_wall_qty")?
            .column_f64("spread", record.spread)
            .context("spread")?
            .column_ts("received_at", ts)
            .context("received_at")?
            .at(ts)
            .context("ts")?;

        self.pending_count = self.pending_count.saturating_add(1);

        // Auto-flush when batch is large enough.
        if self.pending_count >= OBI_FLUSH_BATCH_SIZE
            && let Err(err) = self.force_flush()
        {
            warn!(?err, underlying = self.underlying, "OBI auto-flush failed");
        }

        Ok(())
    }

    /// Forces an immediate flush of all buffered rows to QuestDB.
    // TEST-EXEMPT: ILP flush — requires live QuestDB
    pub fn flush(&mut self) -> Result<()> {
        self.force_flush()
    }

    /// Internal flush implementation.
    fn force_flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        if self.sender.is_none() {
            self.try_reconnect()?;
        }

        let count = self.pending_count;
        let sender = self
            .sender
            .as_mut()
            .context("sender unavailable in force_flush")?;

        if let Err(err) = sender.flush(&mut self.buffer) {
            self.sender = None;
            // Create fresh buffer (questdb-rs state machine may be corrupted after failed flush).
            self.buffer = Buffer::new(questdb::ingress::ProtocolVersion::V1);
            self.pending_count = 0;
            return Err(err).context("flush OBI snapshots to QuestDB");
        }

        self.pending_count = 0;
        debug!(
            flushed_rows = count,
            underlying = self.underlying,
            "OBI batch flushed to QuestDB"
        );
        Ok(())
    }

    /// Attempts to reconnect to QuestDB.
    fn try_reconnect(&mut self) -> Result<()> {
        let sender = Sender::from_conf(&self.ilp_conf_string)
            .context("failed to reconnect to QuestDB for OBI")?;
        self.buffer = sender.new_buffer();
        self.sender = Some(sender);
        self.pending_count = 0;
        info!(
            underlying = self.underlying,
            "OBI writer reconnected to QuestDB"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DDL Helper
// ---------------------------------------------------------------------------

/// Executes a DDL statement (best-effort).
async fn execute_ddl(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                info!("{label} DDL executed successfully");
            } else {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>(); // O(1) EXEMPT: DDL error logging
                warn!(%status, body, "{label} DDL returned non-success");
            }
        }
        Err(err) => {
            warn!(?err, "{label} DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_obi_table_name() {
        assert_eq!(QUESTDB_TABLE_OBI_SNAPSHOTS, "obi_snapshots");
    }

    #[test]
    fn test_obi_flush_batch_size_valid() {
        assert!(OBI_FLUSH_BATCH_SIZE > 0);
        assert!(OBI_FLUSH_BATCH_SIZE <= 1000);
    }

    #[test]
    fn test_obi_ddl_contains_required_columns() {
        assert!(OBI_SNAPSHOTS_DDL.contains("obi DOUBLE"));
        assert!(OBI_SNAPSHOTS_DDL.contains("weighted_obi DOUBLE"));
        assert!(OBI_SNAPSHOTS_DDL.contains("total_bid_qty LONG"));
        assert!(OBI_SNAPSHOTS_DDL.contains("total_ask_qty LONG"));
        assert!(OBI_SNAPSHOTS_DDL.contains("bid_levels LONG"));
        assert!(OBI_SNAPSHOTS_DDL.contains("ask_levels LONG"));
        assert!(OBI_SNAPSHOTS_DDL.contains("max_bid_wall_price DOUBLE"));
        assert!(OBI_SNAPSHOTS_DDL.contains("max_bid_wall_qty LONG"));
        assert!(OBI_SNAPSHOTS_DDL.contains("max_ask_wall_price DOUBLE"));
        assert!(OBI_SNAPSHOTS_DDL.contains("max_ask_wall_qty LONG"));
        assert!(OBI_SNAPSHOTS_DDL.contains("spread DOUBLE"));
        assert!(OBI_SNAPSHOTS_DDL.contains("underlying SYMBOL"));
        assert!(OBI_SNAPSHOTS_DDL.contains("segment SYMBOL"));
        assert!(OBI_SNAPSHOTS_DDL.contains("security_id LONG"));
        assert!(OBI_SNAPSHOTS_DDL.contains("received_at TIMESTAMP"));
        assert!(OBI_SNAPSHOTS_DDL.contains("ts TIMESTAMP"));
    }

    #[test]
    fn test_obi_ddl_partitioned_by_hour() {
        assert!(OBI_SNAPSHOTS_DDL.contains("PARTITION BY HOUR"));
    }

    #[test]
    fn test_obi_ddl_uses_wal() {
        assert!(OBI_SNAPSHOTS_DDL.contains("WAL"));
    }

    #[test]
    fn test_obi_dedup_key_includes_security_id_and_segment() {
        assert!(DEDUP_KEY_OBI.contains("security_id"));
        assert!(DEDUP_KEY_OBI.contains("segment"));
    }

    #[test]
    fn test_obi_record_default_values() {
        let record = ObiRecord {
            security_id: 11536,
            segment_code: 2,
            obi: 0.25,
            weighted_obi: 0.30,
            total_bid_qty: 50000,
            total_ask_qty: 30000,
            bid_levels: 20,
            ask_levels: 18,
            max_bid_wall_price: 24500.0,
            max_bid_wall_qty: 100000,
            max_ask_wall_price: 24550.0,
            max_ask_wall_qty: 80000,
            spread: 0.50,
            ts_nanos: 1_000_000_000,
        };
        assert_eq!(record.security_id, 11536);
        assert!((record.obi - 0.25).abs() < f64::EPSILON);
        assert_eq!(record.total_bid_qty, 50000);
        assert_eq!(record.bid_levels, 20);
    }

    #[test]
    fn test_obi_record_extreme_values() {
        // OBI = +1.0 when all bids, no asks
        let record = ObiRecord {
            security_id: 1,
            segment_code: 1,
            obi: 1.0,
            weighted_obi: 1.0,
            total_bid_qty: 100000,
            total_ask_qty: 0,
            bid_levels: 20,
            ask_levels: 0,
            max_bid_wall_price: 100.0,
            max_bid_wall_qty: 50000,
            max_ask_wall_price: 0.0,
            max_ask_wall_qty: 0,
            spread: 0.0,
            ts_nanos: 1_000_000_000,
        };
        assert!((record.obi - 1.0).abs() < f64::EPSILON);

        // OBI = -1.0 when all asks, no bids
        let record_neg = ObiRecord {
            security_id: 1,
            segment_code: 1,
            obi: -1.0,
            weighted_obi: -1.0,
            total_bid_qty: 0,
            total_ask_qty: 100000,
            bid_levels: 0,
            ask_levels: 20,
            max_bid_wall_price: 0.0,
            max_bid_wall_qty: 0,
            max_ask_wall_price: 100.0,
            max_ask_wall_qty: 50000,
            spread: 0.0,
            ts_nanos: 1_000_000_000,
        };
        assert!((record_neg.obi - (-1.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_obi_record_zero_quantities() {
        // OBI = 0 when both sides empty
        let record = ObiRecord {
            security_id: 1,
            segment_code: 1,
            obi: 0.0,
            weighted_obi: 0.0,
            total_bid_qty: 0,
            total_ask_qty: 0,
            bid_levels: 0,
            ask_levels: 0,
            max_bid_wall_price: 0.0,
            max_bid_wall_qty: 0,
            max_ask_wall_price: 0.0,
            max_ask_wall_qty: 0,
            spread: 0.0,
            ts_nanos: 1_000_000_000,
        };
        assert!((record.obi).abs() < f64::EPSILON);
    }

    // --- Dedup key mechanical verification ---

    #[test]
    fn test_obi_dedup_sql_includes_ts_security_id_segment() {
        // Mechanically verify the exact SQL generated for DEDUP.
        // This prevents silent regressions if someone changes DEDUP_KEY_OBI.
        let dedup_sql = format!(
            "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
            QUESTDB_TABLE_OBI_SNAPSHOTS, DEDUP_KEY_OBI
        );
        assert_eq!(
            dedup_sql,
            "ALTER TABLE obi_snapshots DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"
        );
    }

    #[test]
    fn test_obi_dedup_key_has_security_id() {
        assert!(
            DEDUP_KEY_OBI.contains("security_id"),
            "DEDUP key must include security_id for per-instrument uniqueness"
        );
    }

    #[test]
    fn test_obi_dedup_key_has_segment() {
        assert!(
            DEDUP_KEY_OBI.contains("segment"),
            "DEDUP key must include segment for cross-segment isolation"
        );
    }

    #[test]
    fn test_obi_ddl_has_designated_timestamp() {
        assert!(
            OBI_SNAPSHOTS_DDL.contains("TIMESTAMP(ts)"),
            "DDL must have TIMESTAMP(ts) as designated timestamp"
        );
    }
}
