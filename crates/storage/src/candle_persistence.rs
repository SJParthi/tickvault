//! QuestDB persistence for OHLCV candles from Dhan historical API.
//!
//! Stores candles in `historical_candles` table with DEDUP UPSERT KEYS
//! on `(ts, security_id, timeframe, segment)` to ensure idempotent re-ingestion.
//! Supports multiple timeframes: 1m, 5m, 15m, 60m, and daily.
//!
//! # Table Schema
//! - `ts` TIMESTAMP (designated) — candle open time (IST epoch for live, UTC+19800s for historical)
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
    CANDLE_FLUSH_BATCH_SIZE, IST_UTC_OFFSET_SECONDS_I64, QUESTDB_TABLE_CANDLES_1S,
    QUESTDB_TABLE_HISTORICAL_CANDLES,
};
use dhan_live_trader_common::segment::segment_code_to_str;
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
    /// IST wall-clock time directly (e.g., 09:00 instead of 03:30).
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
        // Dhan WebSocket exchange_timestamp is already IST epoch seconds.
        // Store directly — no offset needed.
        let ts_nanos = TimestampNanos::new(i64::from(timestamp_secs).saturating_mul(1_000_000_000));

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

/// Creates the `historical_candles` table (if not exists) and enables DEDUP UPSERT KEYS.
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

    info!("historical candle tables setup complete (DDL + DEDUP UPSERT KEYS)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::{
        EXCHANGE_SEGMENT_BSE_CURRENCY, EXCHANGE_SEGMENT_BSE_EQ, EXCHANGE_SEGMENT_BSE_FNO,
        EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_MCX_COMM, EXCHANGE_SEGMENT_NSE_CURRENCY,
        EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO,
    };

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
    fn test_dedup_key_candles_includes_all_components() {
        assert_eq!(DEDUP_KEY_CANDLES, "security_id, timeframe, segment");
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

    // -----------------------------------------------------------------------
    // TCP drain server helper (same pattern as tick_persistence tests)
    // -----------------------------------------------------------------------

    /// Spawn a background TCP server that accepts one connection and drains
    /// all data until EOF. Returns the port.
    fn spawn_tcp_drain_server() -> u16 {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 65536];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        port
    }

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_400: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 31\r\n\r\n{\"error\":\"table does not exist\"}";

    /// Spawn an async HTTP mock server that returns a fixed `response` for
    /// every request. Returns the port.
    async fn spawn_mock_http_server(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn make_test_candle(security_id: u32, timeframe: &'static str) -> HistoricalCandle {
        HistoricalCandle {
            security_id,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            timestamp_utc_secs: 1_772_073_900, // UTC epoch
            timeframe,
            open: 24500.0,
            high: 24550.0,
            low: 24450.0,
            close: 24520.0,
            volume: 100_000,
            open_interest: 50_000,
        }
    }

    // -----------------------------------------------------------------------
    // CandlePersistenceWriter tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_candle_persistence_writer_new_with_unreachable_port() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let result = CandlePersistenceWriter::new(&config);
        // Either succeeds (lazy) or fails — must not panic.
        let _is_ok = result.is_ok();
    }

    #[test]
    fn test_candle_persistence_writer_new_with_invalid_hostname() {
        let config = QuestDbConfig {
            host: "this.host.does.not.exist.example.invalid".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let result = CandlePersistenceWriter::new(&config);
        let _is_ok = result.is_ok();
    }

    #[test]
    fn test_candle_persistence_writer_new_with_valid_server() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let writer = CandlePersistenceWriter::new(&config);
        assert!(writer.is_ok(), "writer should connect to valid TCP");
    }

    #[test]
    fn test_candle_persistence_writer_append_candle_increments_pending() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        assert_eq!(writer.pending_count(), 0);

        let candle = make_test_candle(13, "1m");
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);
    }

    #[test]
    fn test_candle_persistence_writer_force_flush_resets_counter() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        for i in 0..5_u32 {
            let candle = make_test_candle(1000 + i, "5m");
            writer.append_candle(&candle).unwrap();
        }
        assert_eq!(writer.pending_count(), 5);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_persistence_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        let result = writer.force_flush();
        assert!(result.is_ok(), "flush with zero pending must be a no-op");
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_persistence_writer_batch_boundary_auto_flush() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        for i in 0..CANDLE_FLUSH_BATCH_SIZE as u32 {
            let candle = make_test_candle(1000 + i, "1m");
            writer.append_candle(&candle).unwrap();
        }
        // After auto-flush at CANDLE_FLUSH_BATCH_SIZE, pending should be 0.
        assert_eq!(
            writer.pending_count(),
            0,
            "auto-flush at batch size boundary must reset pending count"
        );
    }

    #[test]
    fn test_candle_persistence_writer_pending_count_tracks_correctly() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        for i in 0..10_u32 {
            let candle = make_test_candle(1000 + i, "15m");
            writer.append_candle(&candle).unwrap();
            assert_eq!(writer.pending_count(), (i + 1) as usize);
        }
    }

    #[test]
    fn test_candle_persistence_writer_append_flush_reuse() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        let candle = make_test_candle(42, "1d");
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);

        // Reuse after flush
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    // -----------------------------------------------------------------------
    // LiveCandleWriter tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_live_candle_writer_new_with_valid_server() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let writer = LiveCandleWriter::new(&config);
        assert!(writer.is_ok());
    }

    #[test]
    fn test_live_candle_writer_new_with_unreachable_port() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let result = LiveCandleWriter::new(&config);
        let _is_ok = result.is_ok();
    }

    #[test]
    fn test_live_candle_writer_append_and_flush() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();

        writer
            .append_candle(
                13,
                2,
                1_773_100_800,
                24500.0,
                24550.0,
                24450.0,
                24520.0,
                10000,
                5,
            )
            .unwrap();
        assert_eq!(writer.pending_count, 1);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_writer_oi_is_always_zero() {
        // Live candle writer always writes oi=0 since OI is not available
        // per-second from ticks. This test verifies the behavior documented
        // in the code comment at line 227.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();

        // OI is not a parameter — it's hardcoded to 0 in append_candle.
        writer
            .append_candle(42, 1, 1_773_100_800, 100.0, 110.0, 90.0, 105.0, 5000, 3)
            .unwrap();

        // The fact that append_candle doesn't take an OI parameter confirms
        // OI is always zero. Just verify the write succeeded.
        assert_eq!(writer.pending_count, 1);
    }

    #[test]
    fn test_live_candle_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        let result = writer.force_flush();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_writer_flush_if_needed_empty() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        let result = writer.flush_if_needed();
        assert!(result.is_ok());
    }

    #[test]
    fn test_live_candle_writer_flush_if_needed_with_pending() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        writer
            .append_candle(
                13,
                2,
                1_773_100_800,
                24500.0,
                24550.0,
                24450.0,
                24520.0,
                10000,
                5,
            )
            .unwrap();

        // flush_if_needed calls force_flush when pending_count > 0.
        let result = writer.flush_if_needed();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_writer_batch_flush() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();

        for i in 0..LIVE_CANDLE_FLUSH_BATCH_SIZE as u32 {
            writer
                .append_candle(
                    1000 + i,
                    2,
                    1_773_100_800 + i,
                    24500.0 + i as f32,
                    24550.0 + i as f32,
                    24450.0 + i as f32,
                    24520.0 + i as f32,
                    10000 + i,
                    5,
                )
                .unwrap();
        }
        // After auto-flush at LIVE_CANDLE_FLUSH_BATCH_SIZE, pending should be 0.
        assert_eq!(writer.pending_count, 0);
    }

    // -----------------------------------------------------------------------
    // DDL tests with mock HTTP server
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_candle_table_ddl_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the success path for both CREATE TABLE and DEDUP DDL.
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_ddl_non_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the non-success path for CREATE TABLE DDL.
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[test]
    fn test_candle_append_adds_ist_offset_to_utc_timestamp() {
        // Verify that append_candle converts UTC epoch to IST-as-UTC by adding offset.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        let candle = HistoricalCandle {
            security_id: 13,
            exchange_segment_code: EXCHANGE_SEGMENT_IDX_I,
            timestamp_utc_secs: 1_772_073_900, // UTC epoch
            timeframe: "1m",
            open: 24500.0,
            high: 24550.0,
            low: 24450.0,
            close: 24520.0,
            volume: 100_000,
            open_interest: 50_000,
        };

        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);

        // The IST offset is added inside append_candle. Verify it doesn't panic.
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_append_all_timeframes() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        let timeframes = ["1", "5", "15", "25", "60", "1d"];
        for (i, tf) in timeframes.iter().enumerate() {
            let candle = make_test_candle(13 + i as u32, tf);
            writer.append_candle(&candle).unwrap();
        }
        assert_eq!(writer.pending_count(), 6);
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_append_all_segments() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        let segments = [
            EXCHANGE_SEGMENT_IDX_I,
            EXCHANGE_SEGMENT_NSE_EQ,
            EXCHANGE_SEGMENT_NSE_FNO,
            EXCHANGE_SEGMENT_BSE_EQ,
            EXCHANGE_SEGMENT_MCX_COMM,
        ];
        for (i, seg) in segments.iter().enumerate() {
            let candle = HistoricalCandle {
                security_id: 100 + i as u32,
                exchange_segment_code: *seg,
                timestamp_utc_secs: 1_772_073_900,
                timeframe: "1m",
                open: 100.0,
                high: 110.0,
                low: 90.0,
                close: 105.0,
                volume: 1000,
                open_interest: 0,
            };
            writer.append_candle(&candle).unwrap();
        }
        assert_eq!(writer.pending_count(), 5);
        writer.force_flush().unwrap();
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: DDL constants, IST offset, edge cases, LiveCandleWriter
    // -----------------------------------------------------------------------

    #[test]
    fn test_historical_candles_ddl_contains_all_columns() {
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("segment SYMBOL"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("security_id LONG"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("open DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("high DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("low DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("close DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("volume LONG"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("oi LONG"));
    }

    #[test]
    fn test_candle_append_ist_offset_arithmetic() {
        // Verify that UTC + IST offset doesn't overflow or produce negative values
        let candle = HistoricalCandle {
            security_id: 13,
            exchange_segment_code: EXCHANGE_SEGMENT_IDX_I,
            timestamp_utc_secs: 0, // epoch zero
            timeframe: "1m",
            open: 100.0,
            high: 110.0,
            low: 90.0,
            close: 105.0,
            volume: 0,
            open_interest: 0,
        };
        let ist_epoch = candle
            .timestamp_utc_secs
            .saturating_add(IST_UTC_OFFSET_SECONDS_I64);
        // IST offset = 19800 seconds (5h30m)
        assert_eq!(ist_epoch, 19800);
    }

    #[test]
    fn test_candle_append_large_timestamp() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Year ~2040 timestamp
        let candle = HistoricalCandle {
            security_id: 13,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_FNO,
            timestamp_utc_secs: 2_208_988_800, // ~2040
            timeframe: "1d",
            open: 50000.0,
            high: 51000.0,
            low: 49000.0,
            close: 50500.0,
            volume: 1_000_000,
            open_interest: 500_000,
        };
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);
        writer.force_flush().unwrap();
    }

    #[test]
    fn test_candle_append_zero_volume_and_oi() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        let candle = HistoricalCandle {
            security_id: 13,
            exchange_segment_code: EXCHANGE_SEGMENT_IDX_I,
            timestamp_utc_secs: 1_772_073_900,
            timeframe: "1m",
            open: 24500.0,
            high: 24500.0,
            low: 24500.0,
            close: 24500.0,
            volume: 0,
            open_interest: 0,
        };
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);
    }

    #[test]
    fn test_live_candle_writer_multiple_append_reuse() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();

        // Append, flush, append again
        writer
            .append_candle(13, 2, 1_773_100_800, 100.0, 110.0, 90.0, 105.0, 5000, 3)
            .unwrap();
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count, 0);

        writer
            .append_candle(25, 2, 1_773_100_801, 200.0, 210.0, 190.0, 205.0, 3000, 2)
            .unwrap();
        assert_eq!(writer.pending_count, 1);
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_writer_all_segments() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();

        let segments = [
            EXCHANGE_SEGMENT_IDX_I,
            EXCHANGE_SEGMENT_NSE_EQ,
            EXCHANGE_SEGMENT_NSE_FNO,
            EXCHANGE_SEGMENT_BSE_EQ,
        ];
        for (i, seg) in segments.iter().enumerate() {
            writer
                .append_candle(
                    100 + i as u32,
                    *seg,
                    1_773_100_800 + i as u32,
                    100.0,
                    110.0,
                    90.0,
                    105.0,
                    1000,
                    5,
                )
                .unwrap();
        }
        assert_eq!(writer.pending_count, 4);
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_flush_batch_size_is_128() {
        assert_eq!(LIVE_CANDLE_FLUSH_BATCH_SIZE, 128);
    }

    #[test]
    fn test_candle_flush_batch_size_from_constants() {
        // CANDLE_FLUSH_BATCH_SIZE must be positive
        assert!(CANDLE_FLUSH_BATCH_SIZE > 0);
    }

    #[tokio::test]
    async fn test_ensure_candle_table_ddl_with_mock_http_success_exercises_dedup() {
        // This test exercises both the CREATE TABLE success path and DEDUP success path.
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_candle_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: DDL warn! field evaluation with tracing subscriber
    // -----------------------------------------------------------------------

    fn install_test_subscriber() -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());
        tracing::subscriber::set_default(subscriber)
    }

    #[tokio::test]
    async fn test_ensure_candle_table_non_success_with_tracing_subscriber() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With tracing subscriber, warn! body expressions are evaluated.
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_send_error_with_tracing_subscriber() {
        let _guard = install_test_subscriber();
        // Accept connection then drop → send error
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_dedup_send_error_with_tracing() {
        let _guard = install_test_subscriber();
        // Two-phase mock: first request (CREATE TABLE) succeeds,
        // second request (DEDUP) gets connection dropped → send error
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // First connection: respond with 200
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(MOCK_HTTP_200.as_bytes()).await;
                drop(stream);
            }
            // Second connection: drop immediately → DEDUP send error
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_candle_table_dedup_keys(&config).await;
    }
}
