//! QuestDB persistence for OHLCV candles from Dhan historical API.
//!
//! Stores candles in `historical_candles` table with DEDUP UPSERT KEYS
//! on `(ts, security_id, timeframe)` to ensure idempotent re-ingestion.
//! Supports multiple timeframes: 1m, 5m, 15m, 60m, and daily.
//!
//! # Table Schema
//! - `ts` TIMESTAMP (designated) — candle open time (IST-as-UTC: UTC epoch + 19800s)
//! - `security_id` LONG — Dhan security identifier
//! - `segment` SYMBOL — exchange segment (NSE_FNO, NSE_EQ, etc.)
//! - `timeframe` SYMBOL — candle interval: 1m, 5m, 15m, 60m, 1d
//! - OHLCV + OI as DOUBLE/LONG columns
//!
//! # Deduplication
//! Server-side: QuestDB DEDUP UPSERT KEYS(ts, security_id, timeframe) prevents
//! duplicate candles from re-fetches.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{
    CANDLE_FLUSH_BATCH_SIZE, EXCHANGE_SEGMENT_BSE_CURRENCY, EXCHANGE_SEGMENT_BSE_EQ,
    EXCHANGE_SEGMENT_BSE_FNO, EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_MCX_COMM,
    EXCHANGE_SEGMENT_NSE_CURRENCY, EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO,
    IST_UTC_OFFSET_SECONDS_I64, QUESTDB_TABLE_CANDLES_1M, QUESTDB_TABLE_CANDLES_1S,
    QUESTDB_TABLE_HISTORICAL_CANDLES,
};
use dhan_live_trader_common::tick_types::HistoricalCandle;

use crate::tick_persistence::f32_to_f64_clean;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY columns for the historical candles table.
/// Compound key: (ts, security_id, timeframe, segment) ensures uniqueness per candle.
/// `segment` is required because security IDs 13 (NIFTY) and 25 (BANKNIFTY) exist
/// in both `IDX_I` and `NSE_EQ` segments with different data.
const DEDUP_KEY_CANDLES: &str = "security_id, timeframe, segment";

/// Maps the binary exchange_segment_code to a human-readable symbol name.
///
/// Uses the same mapping as the Dhan Python SDK.
/// Note: code 6 is unused/skipped in Dhan's protocol.
fn segment_code_to_str(code: u8) -> &'static str {
    match code {
        EXCHANGE_SEGMENT_IDX_I => "IDX_I",
        EXCHANGE_SEGMENT_NSE_EQ => "NSE_EQ",
        EXCHANGE_SEGMENT_NSE_FNO => "NSE_FNO",
        EXCHANGE_SEGMENT_NSE_CURRENCY => "NSE_CURRENCY",
        EXCHANGE_SEGMENT_BSE_EQ => "BSE_EQ",
        EXCHANGE_SEGMENT_MCX_COMM => "MCX_COMM",
        EXCHANGE_SEGMENT_BSE_CURRENCY => "BSE_CURRENCY",
        EXCHANGE_SEGMENT_BSE_FNO => "BSE_FNO",
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
    /// Converts `candle.timestamp_utc_secs` (UTC epoch from Dhan V2 API) to
    /// IST-as-UTC by adding `IST_UTC_OFFSET_SECONDS_I64`, so QuestDB displays
    /// IST wall-clock time directly (e.g., 09:15 instead of 03:45).
    ///
    /// Writes to the unified `historical_candles` table with a `timeframe` column.
    ///
    /// Auto-flushes if the buffer reaches `CANDLE_FLUSH_BATCH_SIZE`.
    ///
    /// # Performance
    /// O(1) — single ILP row append + conditional flush.
    pub fn append_candle(&mut self, candle: &HistoricalCandle) -> Result<()> {
        // UTC epoch → IST-as-UTC: add 19800s so QuestDB shows IST wall-clock time.
        let ist_epoch_secs = candle
            .timestamp_utc_secs
            .saturating_add(IST_UTC_OFFSET_SECONDS_I64);
        let ts_nanos = TimestampNanos::new(ist_epoch_secs.saturating_mul(1_000_000_000));

        self.buffer
            .table(QUESTDB_TABLE_HISTORICAL_CANDLES)
            .context("table name")?
            .symbol("segment", segment_code_to_str(candle.exchange_segment_code))
            .context("segment")?
            .symbol("timeframe", candle.timeframe)
            .context("timeframe")?
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
// Live Candle Writer (candles_1s — feeds materialized views)
// ---------------------------------------------------------------------------

/// Batch size for live candle ILP flushes. Smaller than historical since
/// live candles arrive in real-time and need lower latency.
const LIVE_CANDLE_FLUSH_BATCH_SIZE: usize = 128;

/// Batched writer for live 1-second candles to `candles_1s` via ILP.
///
/// Completed candles from `CandleAggregator` are written here, feeding
/// QuestDB materialized views (candles_1m, 5m, 15m, etc.).
pub struct LiveCandleWriter {
    sender: Sender,
    buffer: Buffer,
    pending_count: usize,
}

impl LiveCandleWriter {
    /// Creates a new live candle writer connected to QuestDB via ILP TCP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for live candles")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
        })
    }

    /// Appends a completed 1-second candle to the ILP buffer.
    ///
    /// Auto-flushes when buffer reaches `LIVE_CANDLE_FLUSH_BATCH_SIZE`.
    ///
    /// # Performance
    /// O(1) — single ILP row append + conditional flush.
    #[allow(clippy::too_many_arguments)] // APPROVED: ILP row requires all OHLCV+meta columns
    pub fn append_candle(
        &mut self,
        security_id: u32,
        exchange_segment_code: u8,
        timestamp_secs: u32,
        open: f32,
        high: f32,
        low: f32,
        close: f32,
        volume: u32,
        tick_count: u32,
    ) -> Result<()> {
        // UTC epoch → IST-as-UTC: add 19800s so QuestDB shows IST wall-clock time.
        let ist_epoch_secs = i64::from(timestamp_secs).saturating_add(IST_UTC_OFFSET_SECONDS_I64);
        let ts_nanos = TimestampNanos::new(ist_epoch_secs.saturating_mul(1_000_000_000));

        self.buffer
            .table(QUESTDB_TABLE_CANDLES_1S)
            .context("table name")?
            .symbol("segment", segment_code_to_str(exchange_segment_code))
            .context("segment")?
            .column_i64("security_id", i64::from(security_id))
            .context("security_id")?
            .column_f64("open", f32_to_f64_clean(open))
            .context("open")?
            .column_f64("high", f32_to_f64_clean(high))
            .context("high")?
            .column_f64("low", f32_to_f64_clean(low))
            .context("low")?
            .column_f64("close", f32_to_f64_clean(close))
            .context("close")?
            .column_i64("volume", i64::from(volume))
            .context("volume")?
            .column_i64("oi", 0) // OI not available per-second from ticks
            .context("oi")?
            .column_i64("tick_count", i64::from(tick_count))
            .context("tick_count")?
            .at(ts_nanos)
            .context("designated timestamp")?;

        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= LIVE_CANDLE_FLUSH_BATCH_SIZE {
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
            .context("flush live candles to QuestDB")?;
        self.pending_count = 0;

        debug!(flushed_rows = count, "live candle batch flushed to QuestDB");
        Ok(())
    }

    /// Flushes if any candles are buffered.
    pub fn flush_if_needed(&mut self) -> Result<()> {
        if self.pending_count > 0 {
            self.force_flush()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Table DDL + DEDUP Setup
// ---------------------------------------------------------------------------

/// SQL to create the `historical_candles` table with explicit schema.
/// Includes `timeframe` SYMBOL column for multi-timeframe candle storage.
///
/// Idempotent — safe to call every startup.
const HISTORICAL_CANDLES_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS historical_candles (\
        segment SYMBOL,\
        timeframe SYMBOL,\
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

/// SQL to create the legacy `historical_candles_1m` table.
/// Kept for backward compatibility with existing data.
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

/// Creates candle tables (if not exist) and enables DEDUP UPSERT KEYS.
///
/// Creates both the new `historical_candles` table (multi-timeframe) and the
/// legacy `historical_candles_1m` table. Best-effort: if QuestDB is unreachable,
/// logs a warning and continues.
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
                "failed to build HTTP client for candle table DDL — tables not pre-created"
            );
            return;
        }
    };

    // Step 1: Create the new multi-timeframe table.
    match client
        .get(&base_url)
        .query(&[("query", HISTORICAL_CANDLES_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!(
                    table = QUESTDB_TABLE_HISTORICAL_CANDLES,
                    "historical_candles table ensured (CREATE TABLE IF NOT EXISTS)"
                );
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "historical_candles table CREATE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(
                ?err,
                "historical_candles table CREATE DDL request failed — table not pre-created"
            );
            return;
        }
    }

    // Step 2: Enable DEDUP UPSERT KEYS on new table (ts, security_id, timeframe).
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_HISTORICAL_CANDLES, DEDUP_KEY_CANDLES
    );
    match client
        .get(&base_url)
        .query(&[("query", &dedup_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!(
                    table = QUESTDB_TABLE_HISTORICAL_CANDLES,
                    "DEDUP UPSERT KEYS enabled"
                );
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "historical_candles table DEDUP DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "historical_candles table DEDUP DDL request failed");
        }
    }

    // Step 3: Create legacy table (backward compatibility).
    match client
        .get(&base_url)
        .query(&[("query", CANDLES_1M_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!(
                    table = QUESTDB_TABLE_CANDLES_1M,
                    "legacy historical_candles_1m table ensured"
                );
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "legacy historical_candles_1m CREATE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(
                ?err,
                "legacy historical_candles_1m CREATE DDL request failed"
            );
        }
    }

    info!("historical candle tables setup complete (DDL + DEDUP UPSERT KEYS)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_code_to_str_all_valid_codes() {
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_IDX_I), "IDX_I");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_NSE_EQ), "NSE_EQ");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_NSE_FNO), "NSE_FNO");
        assert_eq!(
            segment_code_to_str(EXCHANGE_SEGMENT_NSE_CURRENCY),
            "NSE_CURRENCY"
        );
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_BSE_EQ), "BSE_EQ");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_MCX_COMM), "MCX_COMM");
        assert_eq!(
            segment_code_to_str(EXCHANGE_SEGMENT_BSE_CURRENCY),
            "BSE_CURRENCY"
        );
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_BSE_FNO), "BSE_FNO");
    }

    #[test]
    fn test_segment_code_to_str_unknown() {
        assert_eq!(segment_code_to_str(255), "UNKNOWN");
        // Code 6 is unused in Dhan protocol — must map to UNKNOWN
        assert_eq!(segment_code_to_str(6), "UNKNOWN");
    }

    #[test]
    fn test_segment_code_bse_fno_is_8_not_7() {
        // Regression: BSE_FNO is code 8, not 7. Code 6 is skipped in Dhan protocol.
        assert_eq!(EXCHANGE_SEGMENT_BSE_FNO, 8);
        assert_eq!(EXCHANGE_SEGMENT_BSE_CURRENCY, 7);
        assert_eq!(segment_code_to_str(8), "BSE_FNO");
        assert_eq!(segment_code_to_str(7), "BSE_CURRENCY");
    }

    #[test]
    fn test_historical_candles_ddl_contains_table_and_timeframe() {
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("historical_candles"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("timeframe SYMBOL"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("TIMESTAMP(ts)"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("PARTITION BY DAY"));
    }

    #[test]
    fn test_legacy_candles_1m_ddl_contains_table_name() {
        assert!(CANDLES_1M_CREATE_DDL.contains("historical_candles_1m"));
        assert!(CANDLES_1M_CREATE_DDL.contains("TIMESTAMP(ts)"));
        assert!(CANDLES_1M_CREATE_DDL.contains("PARTITION BY DAY"));
    }

    #[test]
    fn test_dedup_key_includes_timeframe_and_segment() {
        assert!(DEDUP_KEY_CANDLES.contains("security_id"));
        assert!(DEDUP_KEY_CANDLES.contains("timeframe"));
        assert!(
            DEDUP_KEY_CANDLES.contains("segment"),
            "DEDUP key must include segment — security IDs 13/25 exist in both IDX_I and NSE_EQ"
        );
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

    // -----------------------------------------------------------------------
    // DEDUP key and DDL constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_candles_is_security_id() {
        assert_eq!(DEDUP_KEY_CANDLES, "security_id");
    }

    #[test]
    fn test_candles_1m_ddl_has_segment_column() {
        assert!(CANDLES_1M_CREATE_DDL.contains("segment"));
    }

    #[test]
    fn test_candles_1m_ddl_has_security_id() {
        assert!(CANDLES_1M_CREATE_DDL.contains("security_id"));
    }

    #[test]
    fn test_candles_1m_ddl_has_ohlcv_columns() {
        assert!(CANDLES_1M_CREATE_DDL.contains("open"));
        assert!(CANDLES_1M_CREATE_DDL.contains("high"));
        assert!(CANDLES_1M_CREATE_DDL.contains("low"));
        assert!(CANDLES_1M_CREATE_DDL.contains("close"));
        assert!(CANDLES_1M_CREATE_DDL.contains("volume"));
    }

    #[test]
    fn test_live_candle_flush_batch_size_positive() {
        let size = LIVE_CANDLE_FLUSH_BATCH_SIZE;
        assert!(size > 0);
    }

    #[test]
    fn test_questdb_ddl_timeout_is_reasonable_candle() {
        assert!((5..=60).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }
}
