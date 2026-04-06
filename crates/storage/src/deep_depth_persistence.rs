//! QuestDB ILP persistence for 20-level and 200-level market depth.
//!
//! Persists deep depth snapshots from the depth WebSocket connections.
//! Each row = one level on one side (bid or ask) for one instrument.
//!
//! # Table: `deep_market_depth`
//! Partitioned by HOUR (high volume: up to 50 instruments × 20 levels × 2 sides × ~1/sec).
//! DEDUP on `(security_id, segment, level, side)` prevents duplicates on reconnect.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::IST_UTC_OFFSET_NANOS;
use dhan_live_trader_common::tick_types::DeepDepthLevel;

/// QuestDB table name for deep market depth (20/200 level).
pub const QUESTDB_TABLE_DEEP_MARKET_DEPTH: &str = "deep_market_depth";

/// DEDUP UPSERT KEY for deep market depth.
const DEDUP_KEY_DEEP_DEPTH: &str = "security_id, segment, level, side";

/// DDL timeout.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Flush batch size for deep depth.
const DEEP_DEPTH_FLUSH_BATCH_SIZE: usize = 500;

/// DDL for the `deep_market_depth` table.
const DEEP_MARKET_DEPTH_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS deep_market_depth (\
        segment SYMBOL,\
        security_id LONG,\
        side SYMBOL,\
        level LONG,\
        price DOUBLE,\
        quantity LONG,\
        orders LONG,\
        depth_type SYMBOL,\
        received_at TIMESTAMP,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// Ensures the `deep_market_depth` table exists in QuestDB.
// TEST-EXEMPT: DDL creation — requires live QuestDB
pub async fn ensure_deep_depth_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    execute_ddl(
        &client,
        &base_url,
        DEEP_MARKET_DEPTH_CREATE_DDL,
        "deep_market_depth",
    )
    .await;

    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_DEEP_MARKET_DEPTH, DEDUP_KEY_DEEP_DEPTH
    );
    execute_ddl(&client, &base_url, &dedup_sql, "deep_market_depth DEDUP").await;
}

/// Writer for deep depth data to QuestDB via ILP.
pub struct DeepDepthWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
}

impl DeepDepthWriter {
    /// Creates a new deep depth writer connected to QuestDB.
    // TEST-EXEMPT: ILP connection — requires live QuestDB, tested via DDL integration
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for deep depth")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Appends one side (bid or ask) of a deep depth snapshot.
    ///
    /// # Arguments
    /// * `security_id` — Dhan security ID.
    /// * `segment_code` — Exchange segment byte code.
    /// * `side` — "BID" or "ASK".
    /// * `levels` — Depth levels (20 or up to 200).
    /// * `depth_type` — "20" or "200".
    /// * `received_at_nanos` — UTC receive timestamp in nanoseconds.
    // TEST-EXEMPT: ILP buffer append — requires live QuestDB, tested via DDL integration
    pub fn append_deep_depth(
        &mut self,
        security_id: u32,
        segment_code: u8,
        side: &str,
        levels: &[DeepDepthLevel],
        depth_type: &str,
        received_at_nanos: i64,
    ) -> Result<()> {
        let segment_str = segment_code_to_str(segment_code);
        let received_nanos =
            TimestampNanos::new(received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS));

        for (i, level) in levels.iter().enumerate() {
            // Skip empty levels (price = 0)
            if level.price <= 0.0 || !level.price.is_finite() {
                continue;
            }

            self.buffer
                .table(QUESTDB_TABLE_DEEP_MARKET_DEPTH)
                .context("deep depth table")?
                .symbol("segment", segment_str)
                .context("segment")?
                .column_i64("security_id", i64::from(security_id))
                .context("security_id")?
                .symbol("side", side)
                .context("side")?
                .column_i64("level", (i as i64).saturating_add(1))
                .context("level")?
                .column_f64("price", level.price)
                .context("price")?
                .column_i64("quantity", i64::from(level.quantity))
                .context("quantity")?
                .column_i64("orders", i64::from(level.orders))
                .context("orders")?
                .symbol("depth_type", depth_type)
                .context("depth_type")?
                .column_ts("received_at", received_nanos)
                .context("received_at")?
                .at(received_nanos)
                .context("ts")?;

            self.pending_count = self.pending_count.saturating_add(1);
        }

        // Auto-flush when batch is large enough
        if self.pending_count >= DEEP_DEPTH_FLUSH_BATCH_SIZE {
            self.flush()?;
        }

        Ok(())
    }

    /// Flushes pending rows to QuestDB.
    // TEST-EXEMPT: ILP flush — requires live QuestDB TCP connection
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        if let Some(ref mut sender) = self.sender {
            match sender.flush(&mut self.buffer) {
                Ok(()) => {
                    debug!(rows = self.pending_count, "deep depth batch flushed");
                    self.pending_count = 0;
                }
                Err(err) => {
                    warn!(?err, "deep depth flush failed — reconnecting");
                    self.sender = None;
                    self.pending_count = 0;
                    // Try to reconnect
                    match Sender::from_conf(&self.ilp_conf_string) {
                        Ok(new_sender) => {
                            self.buffer = new_sender.new_buffer();
                            self.sender = Some(new_sender);
                            info!("deep depth writer reconnected to QuestDB");
                        }
                        Err(reconn_err) => {
                            warn!(?reconn_err, "deep depth writer reconnection failed");
                        }
                    }
                }
            }
        } else {
            // Try to reconnect
            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(new_sender) => {
                    self.buffer = new_sender.new_buffer();
                    self.sender = Some(new_sender);
                    info!("deep depth writer reconnected to QuestDB");
                }
                Err(err) => {
                    warn!(?err, "deep depth writer still disconnected");
                }
            }
        }

        Ok(())
    }
}

/// Converts segment code to string.
fn segment_code_to_str(code: u8) -> &'static str {
    match code {
        0 => "IDX_I",
        1 => "NSE_EQ",
        2 => "NSE_FNO",
        3 => "NSE_CURRENCY",
        4 => "BSE_EQ",
        5 => "MCX_COMM",
        7 => "BSE_CURRENCY",
        8 => "BSE_FNO",
        _ => "UNKNOWN",
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deep_depth_constants() {
        assert_eq!(QUESTDB_TABLE_DEEP_MARKET_DEPTH, "deep_market_depth");
        assert!(DEEP_DEPTH_FLUSH_BATCH_SIZE > 0);
        assert!(DEEP_DEPTH_FLUSH_BATCH_SIZE <= 1000);
    }

    #[test]
    fn test_segment_code_to_str() {
        assert_eq!(segment_code_to_str(0), "IDX_I");
        assert_eq!(segment_code_to_str(1), "NSE_EQ");
        assert_eq!(segment_code_to_str(2), "NSE_FNO");
        assert_eq!(segment_code_to_str(5), "MCX_COMM");
        assert_eq!(segment_code_to_str(6), "UNKNOWN");
        assert_eq!(segment_code_to_str(255), "UNKNOWN");
    }

    #[test]
    fn test_ddl_contains_all_columns() {
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("segment SYMBOL"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("security_id LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("side SYMBOL"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("level LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("price DOUBLE"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("quantity LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("orders LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("depth_type SYMBOL"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("PARTITION BY HOUR WAL"));
    }

    #[test]
    fn test_dedup_key_includes_security_id_and_side() {
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("security_id"));
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("segment"));
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("level"));
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("side"));
    }
}
