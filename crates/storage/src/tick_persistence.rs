//! QuestDB ILP persistence for live tick data.
//!
//! Receives `ParsedTick` from the pipeline and writes to QuestDB via ILP.
//! Uses batched writes (flush every N rows or every T milliseconds) to
//! balance latency vs throughput.
//!
//! # Tables Written
//! - `ticks` — every tick stored with security_id, segment, OHLCV, OI, etc.
//!
//! # Idempotency
//! DEDUP UPSERT KEYS on `(ts, security_id)` prevent duplicate ticks on reconnect.
//!
//! # Error Handling
//! Tick persistence is observability data, NOT critical path. On QuestDB failure,
//! the system logs WARN and continues — no data is buffered in memory beyond
//! the current batch to prevent OOM.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{
    QUESTDB_TABLE_TICKS, TICK_FLUSH_BATCH_SIZE, TICK_FLUSH_INTERVAL_MS,
};
use dhan_live_trader_common::tick_types::ParsedTick;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY for the `ticks` table.
const DEDUP_KEY_TICKS: &str = "security_id";

// ---------------------------------------------------------------------------
// Exchange segment code → string mapping
// ---------------------------------------------------------------------------

/// Maps the binary exchange_segment_code to a human-readable symbol name.
///
/// Uses the same mapping as the Dhan Python SDK.
fn segment_code_to_str(code: u8) -> &'static str {
    match code {
        0 => "IDX_I",
        1 => "NSE_EQ",
        2 => "NSE_FNO",
        3 => "BSE_EQ",
        4 => "BSE_FNO",
        5 => "MCX_COMM",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Batched tick writer for QuestDB via ILP.
///
/// Accumulates ticks in an ILP buffer and flushes when either:
/// - The buffer reaches `TICK_FLUSH_BATCH_SIZE` rows, or
/// - `flush_if_needed()` detects that `TICK_FLUSH_INTERVAL_MS` has elapsed.
pub struct TickPersistenceWriter {
    sender: Sender,
    buffer: Buffer,
    pending_count: usize,
    last_flush_ms: u64,
}

impl TickPersistenceWriter {
    /// Creates a new tick writer connected to QuestDB via ILP TCP.
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
            last_flush_ms: current_time_ms(),
        })
    }

    /// Appends a parsed tick to the ILP buffer.
    ///
    /// Auto-flushes if the buffer reaches `TICK_FLUSH_BATCH_SIZE`.
    ///
    /// # Performance
    /// O(1) — single ILP row append + conditional flush.
    pub fn append_tick(&mut self, tick: &ParsedTick) -> Result<()> {
        let ts_nanos = TimestampNanos::new(i64::from(tick.exchange_timestamp) * 1_000_000_000);
        let received_nanos = TimestampNanos::new(tick.received_at_nanos);

        self.buffer
            .table(QUESTDB_TABLE_TICKS)
            .context("table name")?
            .symbol("segment", segment_code_to_str(tick.exchange_segment_code))
            .context("segment")?
            .column_i64("security_id", i64::from(tick.security_id))
            .context("security_id")?
            .column_f64("ltp", f64::from(tick.last_traded_price))
            .context("ltp")?
            .column_f64("open", f64::from(tick.day_open))
            .context("open")?
            .column_f64("high", f64::from(tick.day_high))
            .context("high")?
            .column_f64("low", f64::from(tick.day_low))
            .context("low")?
            .column_f64("close", f64::from(tick.day_close))
            .context("close")?
            .column_i64("volume", i64::from(tick.volume))
            .context("volume")?
            .column_i64("oi", i64::from(tick.open_interest))
            .context("oi")?
            .column_f64("avg_price", f64::from(tick.average_traded_price))
            .context("avg_price")?
            .column_i64("last_trade_qty", i64::from(tick.last_trade_quantity))
            .context("last_trade_qty")?
            .column_i64("total_buy_qty", i64::from(tick.total_buy_quantity))
            .context("total_buy_qty")?
            .column_i64("total_sell_qty", i64::from(tick.total_sell_quantity))
            .context("total_sell_qty")?
            .column_ts("received_at", received_nanos)
            .context("received_at")?
            .at(ts_nanos)
            .context("designated timestamp")?;

        self.pending_count += 1;

        if self.pending_count >= TICK_FLUSH_BATCH_SIZE {
            self.force_flush()?;
        }

        Ok(())
    }

    /// Flushes the buffer if enough time has elapsed since the last flush.
    ///
    /// Call this periodically (e.g., after each batch of frames) to ensure
    /// ticks don't sit in the buffer too long.
    pub fn flush_if_needed(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        let now_ms = current_time_ms();
        if now_ms.saturating_sub(self.last_flush_ms) >= TICK_FLUSH_INTERVAL_MS {
            self.force_flush()?;
        }

        Ok(())
    }

    /// Forces an immediate flush of all buffered ticks to QuestDB.
    pub fn force_flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        let count = self.pending_count;
        self.sender
            .flush(&mut self.buffer)
            .context("flush ticks to QuestDB")?;
        self.pending_count = 0;
        self.last_flush_ms = current_time_ms();

        debug!(flushed_rows = count, "tick batch flushed to QuestDB");
        Ok(())
    }

    /// Returns the number of ticks currently buffered (not yet flushed).
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }
}

// ---------------------------------------------------------------------------
// Table DDL + DEDUP Setup
// ---------------------------------------------------------------------------

/// SQL to create the `ticks` table with explicit schema.
///
/// Using CREATE TABLE IF NOT EXISTS so it's idempotent — safe to call every startup.
/// Schema matches exactly what `append_tick()` writes via ILP:
/// - `segment` as SYMBOL (indexed for WHERE clauses)
/// - `security_id` as LONG (4-byte in Dhan, but LONG for QuestDB compat)
/// - Price fields as DOUBLE (f64 from f32 parsed ticks)
/// - Volume/quantity fields as LONG (cumulative, can exceed u32 for indices)
/// - `received_at` as TIMESTAMP (local clock for latency measurement)
/// - `ts` as designated TIMESTAMP (exchange time, partition key)
const TICKS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS ticks (\
        segment SYMBOL,\
        security_id LONG,\
        ltp DOUBLE,\
        open DOUBLE,\
        high DOUBLE,\
        low DOUBLE,\
        close DOUBLE,\
        volume LONG,\
        oi LONG,\
        avg_price DOUBLE,\
        last_trade_qty LONG,\
        total_buy_qty LONG,\
        total_sell_qty LONG,\
        received_at TIMESTAMP,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// Creates the `ticks` table (if not exists) and enables DEDUP UPSERT KEYS.
///
/// Best-effort: if QuestDB is unreachable, logs a warning and continues.
/// The table must exist BEFORE ILP writes for predictable schema, and BEFORE
/// candle queries to avoid "table does not exist" errors from SAMPLE BY.
pub async fn ensure_tick_table_dedup_keys(questdb_config: &QuestDbConfig) {
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
                "failed to build HTTP client for tick table DDL — table not pre-created"
            );
            return;
        }
    };

    // Step 1: Create the table with explicit schema (idempotent).
    match client
        .get(&base_url)
        .query(&[("query", TICKS_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("ticks table ensured (CREATE TABLE IF NOT EXISTS)");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "ticks table CREATE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(
                ?err,
                "ticks table CREATE DDL request failed — table not pre-created"
            );
            return;
        }
    }

    // Step 2: Enable DEDUP UPSERT KEYS (idempotent — re-enabling is a no-op).
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_TICKS, DEDUP_KEY_TICKS
    );
    match client
        .get(&base_url)
        .query(&[("query", &dedup_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!("DEDUP UPSERT KEYS enabled for ticks table");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "ticks table DEDUP DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "ticks table DEDUP DDL request failed");
        }
    }

    info!("ticks table setup complete (DDL + DEDUP UPSERT KEYS)");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns current wall-clock time in milliseconds since Unix epoch.
fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use questdb::ingress::ProtocolVersion;

    fn make_test_tick(security_id: u32, ltp: f32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: 2, // NSE_FNO
            last_traded_price: ltp,
            last_trade_quantity: 75,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_123_456_789,
            average_traded_price: ltp - 10.0,
            volume: 50000,
            total_sell_quantity: 25000,
            total_buy_quantity: 25000,
            day_open: ltp - 50.0,
            day_close: ltp - 100.0,
            day_high: ltp + 20.0,
            day_low: ltp - 80.0,
            open_interest: 120000,
            oi_day_high: 130000,
            oi_day_low: 110000,
        }
    }

    #[test]
    fn test_segment_code_to_str_all_known() {
        assert_eq!(segment_code_to_str(0), "IDX_I");
        assert_eq!(segment_code_to_str(1), "NSE_EQ");
        assert_eq!(segment_code_to_str(2), "NSE_FNO");
        assert_eq!(segment_code_to_str(3), "BSE_EQ");
        assert_eq!(segment_code_to_str(4), "BSE_FNO");
        assert_eq!(segment_code_to_str(5), "MCX_COMM");
    }

    #[test]
    fn test_segment_code_to_str_unknown() {
        assert_eq!(segment_code_to_str(6), "UNKNOWN");
        assert_eq!(segment_code_to_str(255), "UNKNOWN");
    }

    #[test]
    fn test_tick_buffer_single_row() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = make_test_tick(13, 24500.50);

        let ts_nanos = TimestampNanos::new(i64::from(tick.exchange_timestamp) * 1_000_000_000);
        let received_nanos = TimestampNanos::new(tick.received_at_nanos);

        buffer
            .table(QUESTDB_TABLE_TICKS)
            .unwrap()
            .symbol("segment", segment_code_to_str(tick.exchange_segment_code))
            .unwrap()
            .column_i64("security_id", i64::from(tick.security_id))
            .unwrap()
            .column_f64("ltp", f64::from(tick.last_traded_price))
            .unwrap()
            .column_f64("open", f64::from(tick.day_open))
            .unwrap()
            .column_f64("high", f64::from(tick.day_high))
            .unwrap()
            .column_f64("low", f64::from(tick.day_low))
            .unwrap()
            .column_f64("close", f64::from(tick.day_close))
            .unwrap()
            .column_i64("volume", i64::from(tick.volume))
            .unwrap()
            .column_i64("oi", i64::from(tick.open_interest))
            .unwrap()
            .column_f64("avg_price", f64::from(tick.average_traded_price))
            .unwrap()
            .column_i64("last_trade_qty", i64::from(tick.last_trade_quantity))
            .unwrap()
            .column_i64("total_buy_qty", i64::from(tick.total_buy_quantity))
            .unwrap()
            .column_i64("total_sell_qty", i64::from(tick.total_sell_quantity))
            .unwrap()
            .column_ts("received_at", received_nanos)
            .unwrap()
            .at(ts_nanos)
            .unwrap();

        assert_eq!(buffer.row_count(), 1);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_tick_buffer_contains_expected_fields() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = make_test_tick(13, 24500.50);

        let ts_nanos = TimestampNanos::new(i64::from(tick.exchange_timestamp) * 1_000_000_000);
        let received_nanos = TimestampNanos::new(tick.received_at_nanos);

        buffer
            .table(QUESTDB_TABLE_TICKS)
            .unwrap()
            .symbol("segment", "NSE_FNO")
            .unwrap()
            .column_i64("security_id", 13)
            .unwrap()
            .column_f64("ltp", 24500.5)
            .unwrap()
            .column_f64("open", 24450.5)
            .unwrap()
            .column_f64("high", 24520.5)
            .unwrap()
            .column_f64("low", 24420.5)
            .unwrap()
            .column_f64("close", 24400.5)
            .unwrap()
            .column_i64("volume", 50000)
            .unwrap()
            .column_i64("oi", 120000)
            .unwrap()
            .column_f64("avg_price", 24490.5)
            .unwrap()
            .column_i64("last_trade_qty", 75)
            .unwrap()
            .column_i64("total_buy_qty", 25000)
            .unwrap()
            .column_i64("total_sell_qty", 25000)
            .unwrap()
            .column_ts("received_at", received_nanos)
            .unwrap()
            .at(ts_nanos)
            .unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_TICKS));
        assert!(content.contains("NSE_FNO"));
        assert!(content.contains("security_id"));
        assert!(content.contains("ltp"));
        assert!(content.contains("volume"));
        assert!(content.contains("oi"));
        assert!(content.contains("avg_price"));
        assert!(content.contains("received_at"));
    }

    #[test]
    fn test_tick_buffer_multiple_rows() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);

        for i in 0..10_u32 {
            let tick = make_test_tick(1000 + i, 24500.0 + (i as f32));
            let ts_nanos = TimestampNanos::new(i64::from(tick.exchange_timestamp) * 1_000_000_000);

            buffer
                .table(QUESTDB_TABLE_TICKS)
                .unwrap()
                .symbol("segment", segment_code_to_str(tick.exchange_segment_code))
                .unwrap()
                .column_i64("security_id", i64::from(tick.security_id))
                .unwrap()
                .column_f64("ltp", f64::from(tick.last_traded_price))
                .unwrap()
                .at(ts_nanos)
                .unwrap();
        }

        assert_eq!(buffer.row_count(), 10);
    }

    #[test]
    fn test_tick_buffer_zero_oi_ticker_packet() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 0, // IDX_I
            last_traded_price: 24500.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default() // all other fields zero
        };

        let ts_nanos = TimestampNanos::new(i64::from(tick.exchange_timestamp) * 1_000_000_000);

        buffer
            .table(QUESTDB_TABLE_TICKS)
            .unwrap()
            .symbol("segment", segment_code_to_str(tick.exchange_segment_code))
            .unwrap()
            .column_i64("security_id", i64::from(tick.security_id))
            .unwrap()
            .column_f64("ltp", f64::from(tick.last_traded_price))
            .unwrap()
            .column_i64("oi", 0)
            .unwrap()
            .column_i64("volume", 0)
            .unwrap()
            .at(ts_nanos)
            .unwrap();

        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("IDX_I"));
    }

    #[test]
    fn test_current_time_ms_is_reasonable() {
        let now = current_time_ms();
        // Must be after 2020-01-01 (1577836800000) and before 2100 (4102444800000).
        assert!(now > 1_577_836_800_000, "current_time_ms too small");
        assert!(now < 4_102_444_800_000, "current_time_ms too large");
    }

    #[tokio::test]
    async fn test_ensure_tick_table_dedup_keys_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(), // RFC 5737 TEST-NET
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        // Must complete without panic.
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_tick_table_dedup_keys_does_not_panic_localhost() {
        let config = QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 19998, // unlikely port
            pg_port: 8812,
            ilp_port: 9009,
        };
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[test]
    fn test_tick_buffer_idx_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = make_test_tick(13, 24500.0);
        // Override to IDX_I segment
        let ts_nanos = TimestampNanos::new(i64::from(tick.exchange_timestamp) * 1_000_000_000);

        buffer
            .table(QUESTDB_TABLE_TICKS)
            .unwrap()
            .symbol("segment", segment_code_to_str(0)) // IDX_I
            .unwrap()
            .column_i64("security_id", 13)
            .unwrap()
            .column_f64("ltp", 24500.0)
            .unwrap()
            .at(ts_nanos)
            .unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("IDX_I"));
    }

    #[test]
    fn test_tick_buffer_mcx_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let ts_nanos = TimestampNanos::new(1_740_556_500_000_000_000);

        buffer
            .table(QUESTDB_TABLE_TICKS)
            .unwrap()
            .symbol("segment", segment_code_to_str(5)) // MCX_COMM
            .unwrap()
            .column_i64("security_id", 99999)
            .unwrap()
            .column_f64("ltp", 75000.0)
            .unwrap()
            .at(ts_nanos)
            .unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("MCX_COMM"));
    }
}
