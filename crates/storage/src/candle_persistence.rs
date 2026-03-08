//! QuestDB persistence for 1-minute OHLCV candles from Dhan historical API.
//!
//! Stores candles in a dedicated `historical_candles_1m` table with DEDUP UPSERT KEYS
//! on `(ts, security_id)` to ensure idempotent re-ingestion.
//!
//! # Table Schema
//! - `ts` TIMESTAMP (designated) — candle open time in UTC
//! - `security_id` LONG — Dhan security identifier
//! - `segment` SYMBOL — exchange segment (NSE_FNO, NSE_EQ, etc.)
//! - OHLCV + OI as DOUBLE/LONG columns
//!
//! # Deduplication
//! Server-side: QuestDB DEDUP UPSERT KEYS(ts, security_id) prevents
//! duplicate candles from re-fetches. Client-side: timestamp uniqueness
//! per security_id is guaranteed by Dhan API (one candle per minute).

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{CANDLE_FLUSH_BATCH_SIZE, QUESTDB_TABLE_CANDLES_1M};
use dhan_live_trader_common::tick_types::HistoricalCandle;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY column for the candles table.
const DEDUP_KEY_CANDLES: &str = "security_id";

/// Maps binary exchange_segment_code to string for ILP SYMBOL column.
fn segment_code_to_str(code: u8) -> &'static str {
    match code {
        0 => "IDX_I",
        1 => "NSE_EQ",
        2 => "NSE_FNO",
        3 => "NSE_CURRENCY",
        4 => "BSE_EQ",
        5 => "MCX_COMM",
        6 => "BSE_CURRENCY",
        7 => "BSE_FNO",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Candle Persistence Writer
// ---------------------------------------------------------------------------

/// Batched candle writer for QuestDB via ILP.
///
/// Accumulates candle rows and flushes when the buffer reaches
/// `CANDLE_FLUSH_BATCH_SIZE` rows.
pub struct CandlePersistenceWriter {
    sender: Sender,
    buffer: Buffer,
    pending_count: usize,
}

impl CandlePersistenceWriter {
    /// Creates a new candle writer connected to QuestDB via ILP TCP.
    ///
    /// # Errors
    /// Returns error if the ILP connection cannot be established.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        let sender =
            Sender::from_conf(&conf_string).context("failed to connect to QuestDB via ILP")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
        })
    }

    /// Appends a historical candle to the ILP buffer.
    ///
    /// Auto-flushes if the buffer reaches `CANDLE_FLUSH_BATCH_SIZE`.
    ///
    /// # Performance
    /// O(1) — single ILP row append + conditional flush.
    pub fn append_candle(&mut self, candle: &HistoricalCandle) -> Result<()> {
        let ts_nanos = TimestampNanos::new(candle.timestamp_utc_secs.saturating_mul(1_000_000_000));

        self.buffer
            .table(QUESTDB_TABLE_CANDLES_1M)
            .context("table name")?
            .symbol("segment", segment_code_to_str(candle.exchange_segment_code))
            .context("segment")?
            .column_i64("security_id", i64::from(candle.security_id))
            .context("security_id")?
            .column_f64("open", candle.open)
            .context("open")?
            .column_f64("high", candle.high)
            .context("high")?
            .column_f64("low", candle.low)
            .context("low")?
            .column_f64("close", candle.close)
            .context("close")?
            .column_i64("volume", candle.volume)
            .context("volume")?
            .column_i64("oi", candle.open_interest)
            .context("oi")?
            .at(ts_nanos)
            .context("designated timestamp")?;

        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= CANDLE_FLUSH_BATCH_SIZE {
            self.force_flush()?;
        }

        Ok(())
    }

    /// Forces an immediate flush of all buffered candles to QuestDB.
    pub fn force_flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        let count = self.pending_count;
        self.sender
            .flush(&mut self.buffer)
            .context("flush candles to QuestDB")?;
        self.pending_count = 0;

        debug!(flushed_rows = count, "candle batch flushed to QuestDB");
        Ok(())
    }

    /// Returns the number of candles currently buffered (not yet flushed).
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }
}

// ---------------------------------------------------------------------------
// Table DDL + DEDUP Setup
// ---------------------------------------------------------------------------

/// SQL to create the `historical_candles_1m` table with explicit schema.
///
/// Idempotent — safe to call every startup.
const CANDLES_1M_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS historical_candles_1m (\
        segment SYMBOL,\
        security_id LONG,\
        open DOUBLE,\
        high DOUBLE,\
        low DOUBLE,\
        close DOUBLE,\
        volume LONG,\
        oi LONG,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

/// Creates the `candles_1m` table (if not exists) and enables DEDUP UPSERT KEYS.
///
/// Best-effort: if QuestDB is unreachable, logs a warning and continues.
pub async fn ensure_candle_table_dedup_keys(questdb_config: &QuestDbConfig) {
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
                "failed to build HTTP client for candle table DDL — table not pre-created"
            );
            return;
        }
    };

    // Step 1: Create the table with explicit schema (idempotent).
    match client
        .get(&base_url)
        .query(&[("query", CANDLES_1M_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("historical_candles_1m table ensured (CREATE TABLE IF NOT EXISTS)");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "historical_candles_1m table CREATE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(
                ?err,
                "historical_candles_1m table CREATE DDL request failed — table not pre-created"
            );
            return;
        }
    }

    // Step 2: Enable DEDUP UPSERT KEYS (idempotent — re-enabling is a no-op).
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_CANDLES_1M, DEDUP_KEY_CANDLES
    );
    match client
        .get(&base_url)
        .query(&[("query", &dedup_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!("DEDUP UPSERT KEYS enabled for historical_candles_1m table");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "historical_candles_1m table DEDUP DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "historical_candles_1m table DEDUP DDL request failed");
        }
    }

    info!("historical_candles_1m table setup complete (DDL + DEDUP UPSERT KEYS)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_code_to_str_nse_fno() {
        assert_eq!(segment_code_to_str(2), "NSE_FNO");
    }

    #[test]
    fn test_segment_code_to_str_unknown() {
        assert_eq!(segment_code_to_str(255), "UNKNOWN");
    }

    #[test]
    fn test_candles_1m_ddl_contains_table_name() {
        assert!(CANDLES_1M_CREATE_DDL.contains("historical_candles_1m"));
        assert!(CANDLES_1M_CREATE_DDL.contains("TIMESTAMP(ts)"));
        assert!(CANDLES_1M_CREATE_DDL.contains("PARTITION BY DAY"));
    }

    #[tokio::test]
    async fn test_ensure_candle_table_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Should not panic — just logs warnings and returns.
        ensure_candle_table_dedup_keys(&config).await;
    }
}
